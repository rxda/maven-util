//! 只读 Maven 仓库服务。
//!
//! Maven 仓库本质上是一个约定了目录结构和元数据文件格式的 HTTP 仓库。
//! 这里不提供目录上传等通用文件服务功能，只允许读取仓库中的公开文件。

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_REQUEST_SIZE: usize = 16 * 1024;
const WORKER_COUNT: usize = 16;
const MAX_REQUESTS_PER_CONNECTION: usize = 100;

#[derive(Debug, Clone)]
pub struct ServerArgs {
    pub root: PathBuf,
    pub host: String,
    pub port: u16,
    pub upstreams: Vec<String>,
    pub client: Arc<Client>,
}

#[derive(Clone)]
struct Repository {
    root: Arc<PathBuf>,
    upstreams: Arc<Vec<String>>,
    client: Arc<Client>,
}

pub struct ServerHandle {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    sender: Option<Sender<TcpStream>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn base_url(&self) -> String {
        format!("http://{}/", self.address)
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.sender.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

enum Body {
    File(File, u64),
    Bytes(Vec<u8>),
}

impl Body {
    fn len(&self) -> u64 {
        match self {
            Self::File(_, length) => *length,
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Body,
}

pub fn run_server(args: ServerArgs) -> Result<()> {
    let _handle = start_server(args)?;
    println!("Maven 只读仓库已启动 (按 Ctrl+C 停止)");
    loop {
        thread::park();
    }
}

pub fn start_server(args: ServerArgs) -> Result<ServerHandle> {
    let root = fs::canonicalize(&args.root)
        .with_context(|| format!("无法定位 Maven 仓库目录: {}", args.root.display()))?;
    if !root.is_dir() {
        return Err(anyhow!("Maven 仓库路径不是目录: {}", root.display()));
    }

    let bind_address = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&bind_address)
        .with_context(|| format!("无法监听地址: {bind_address}"))?;
    let actual_address = listener.local_addr().context("无法读取监听地址")?;
    listener
        .set_nonblocking(true)
        .context("设置监听非阻塞模式失败")?;
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<TcpStream>();
    let receiver = Arc::new(Mutex::new(receiver));
    let repository = Repository {
        root: Arc::new(root.clone()),
        upstreams: Arc::new(
            args.upstreams
                .into_iter()
                .map(|url| url.trim_end_matches('/').to_string())
                .filter(|url| !url.is_empty())
                .collect(),
        ),
        client: args.client,
    };

    let mut workers = Vec::with_capacity(WORKER_COUNT);
    for _ in 0..WORKER_COUNT {
        let receiver = Arc::clone(&receiver);
        let repository = repository.clone();
        workers.push(thread::spawn(move || worker_loop(receiver, repository)));
    }

    let thread_stop = Arc::clone(&stop);
    let thread_sender = sender.clone();
    let join = thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Windows may propagate the listener's non-blocking mode to
                    // accepted sockets. Request parsing uses read timeouts and
                    // therefore expects a blocking connection.
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("设置连接阻塞模式失败: {error}");
                        continue;
                    }
                    if thread_sender.send(stream).is_err() {
                        break;
                    }
                }
                Err(error) if is_would_block(&error) => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => eprintln!("接受连接失败: {error}"),
            }
        }
    });

    Ok(ServerHandle {
        address: actual_address,
        stop,
        join: Some(join),
        sender: Some(sender),
        workers,
    })
}

fn is_would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        // WSAEWOULDBLOCK. Keep this explicit for Windows toolchains where the
        // raw socket error is not always mapped to ErrorKind::WouldBlock.
        || (cfg!(windows) && error.raw_os_error() == Some(10035))
}

fn worker_loop(receiver: Arc<Mutex<Receiver<TcpStream>>>, repository: Repository) {
    loop {
        let stream = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(stream) = stream else {
            return;
        };
        if let Err(error) = handle_connection(stream, &repository) {
            eprintln!("处理 Maven 请求失败: {error:#}");
        }
    }
}

fn handle_connection(stream: TcpStream, repository: &Repository) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .context("设置读取超时失败")?;
    let mut reader = BufReader::new(stream);
    for request_number in 0..MAX_REQUESTS_PER_CONNECTION {
        let Some(request) = read_request(&mut reader)? else {
            return Ok(());
        };
        let (method, target, keep_alive) = match parse_request(&request) {
            Ok(request) => request,
            Err(_) => {
                write_error(
                    reader.get_mut(),
                    "400 Bad Request",
                    "非法 HTTP 请求\n",
                    false,
                    None,
                )?;
                return Ok(());
            }
        };
        let keep_alive = keep_alive && request_number + 1 < MAX_REQUESTS_PER_CONNECTION;

        if method != "GET" && method != "HEAD" {
            write_error(
                reader.get_mut(),
                "405 Method Not Allowed",
                "仅支持 GET 和 HEAD 请求\n",
                false,
                Some("GET, HEAD"),
            )?;
            return Ok(());
        }

        match repository.resolve(target) {
            Ok(response) => {
                write_response(reader.get_mut(), response, method == "HEAD", keep_alive)?
            }
            Err(error) if error.to_string() == "not found" => {
                write_error(
                    reader.get_mut(),
                    "404 Not Found",
                    "文件不存在\n",
                    keep_alive,
                    None,
                )?;
            }
            Err(error) if error.to_string() == "bad path" => {
                write_error(
                    reader.get_mut(),
                    "400 Bad Request",
                    "非法仓库路径\n",
                    false,
                    None,
                )?;
                return Ok(());
            }
            Err(error) if error.to_string().starts_with("upstream unavailable") => {
                write_error(
                    reader.get_mut(),
                    "502 Bad Gateway",
                    "Maven 上游暂时不可用\n",
                    false,
                    None,
                )?;
                eprintln!("{error:#}");
                return Ok(());
            }
            Err(error) => return Err(error),
        }

        if !keep_alive {
            return Ok(());
        }
    }
    Ok(())
}

fn read_request<R: Read>(reader: &mut R) -> Result<Option<String>> {
    let mut request = Vec::with_capacity(1024);
    let mut byte = [0_u8; 1];
    loop {
        let read = reader.read(&mut byte).context("读取 HTTP 请求失败")?;
        if read == 0 {
            if request.is_empty() {
                return Ok(None);
            }
            return Err(anyhow!("客户端提前关闭连接"));
        }
        request.push(byte[0]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > MAX_REQUEST_SIZE {
            return Err(anyhow!("HTTP 请求头过大"));
        }
    }
    String::from_utf8(request)
        .map(Some)
        .context("HTTP 请求不是有效 UTF-8")
}

fn parse_request(request: &str) -> Result<(&str, &str, bool)> {
    let mut lines = request.split("\r\n");
    let line = lines.next().ok_or_else(|| anyhow!("bad request"))?;
    let mut fields = line.split_ascii_whitespace();
    let method = fields.next().ok_or_else(|| anyhow!("bad request"))?;
    let target = fields.next().ok_or_else(|| anyhow!("bad request"))?;
    let version = fields.next().ok_or_else(|| anyhow!("bad request"))?;
    if fields.next().is_some() || (version != "HTTP/1.0" && version != "HTTP/1.1") {
        return Err(anyhow!("bad request"));
    }
    let mut close = false;
    let mut explicit_keep_alive = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(anyhow!("bad request"));
        };
        if name.eq_ignore_ascii_case("connection") {
            for token in value.split(',').map(str::trim) {
                if token.eq_ignore_ascii_case("close") {
                    close = true;
                } else if token.eq_ignore_ascii_case("keep-alive") {
                    explicit_keep_alive = true;
                }
            }
        }
    }
    let keep_alive = if version == "HTTP/1.1" {
        !close
    } else {
        explicit_keep_alive && !close
    };
    Ok((method, target, keep_alive))
}

impl Repository {
    fn resolve(&self, target: &str) -> Result<Response> {
        let relative = target
            .split_once('?')
            .map_or(target, |(path, _)| path)
            .strip_prefix('/')
            .ok_or_else(|| anyhow!("bad path"))?;
        let components = decode_path(relative).ok_or_else(|| anyhow!("bad path"))?;
        match self.resolve_file(&components) {
            Ok(response) => Ok(response),
            Err(error) if error.to_string() == "not found" => {
                match self.resolve_generated_checksum(&components) {
                    Ok(Some(response)) => Ok(response),
                    Ok(None) => match self.fetch_from_upstreams(&components) {
                        Ok(response) => Ok(response),
                        Err(error) if error.to_string() == "not found" => {
                            if components.last().map(String::as_str) == Some("maven-metadata.xml") {
                                self.generate_metadata(&components)
                            } else {
                                Err(error)
                            }
                        }
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn resolve_generated_checksum(&self, components: &[String]) -> Result<Option<Response>> {
        let Some(checksum) = components
            .last()
            .and_then(|name| name.strip_suffix(".sha1"))
        else {
            return Ok(None);
        };
        if checksum.is_empty() {
            return Ok(None);
        }
        let mut artifact_components = components.to_vec();
        let last = artifact_components
            .last_mut()
            .context("无法获取校验文件名")?;
        *last = checksum.to_string();
        let artifact_path = match self.safe_path(&artifact_components) {
            Ok(path) => path,
            Err(error) if error.to_string() == "not found" => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut file = match File::open(&artifact_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(anyhow!(error).context("打开待校验文件失败")),
        };
        let checksum = sha1_hex(&mut file).context("计算 SHA-1 校验值失败")?;
        Ok(Some(Response {
            status: "200 OK",
            content_type: "text/plain; charset=utf-8",
            body: Body::Bytes(format!("{checksum}\n").into_bytes()),
        }))
    }

    fn fetch_from_upstreams(&self, components: &[String]) -> Result<Response> {
        if components.is_empty() {
            return Err(anyhow!("bad path"));
        }
        let relative_url = components
            .iter()
            .map(|component| url_encode(component))
            .collect::<Vec<_>>()
            .join("/");
        let mut last_error = None;
        let mut response = None;
        for upstream in self.upstreams.iter() {
            let url = format!("{upstream}/{relative_url}");
            match self.client.get(&url).send() {
                Ok(candidate) if candidate.status().as_u16() == 404 => continue,
                Ok(candidate) if candidate.status().is_success() => {
                    response = Some(candidate);
                    break;
                }
                Ok(candidate) => {
                    last_error = Some(format!("Maven Central returned {}", candidate.status()));
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        let mut response = match response {
            Some(response) => response,
            None if last_error.is_none() => return Err(anyhow!("not found")),
            None => {
                return Err(anyhow!(
                    "upstream unavailable: {}",
                    last_error.unwrap_or_else(|| "no upstream configured".into())
                ));
            }
        };

        let file_name = components.last().context("无法获取 Maven 构件文件名")?;
        let parent = self.cache_parent(&components[..components.len() - 1])?;
        let target = parent.join(file_name);
        fs::create_dir_all(&parent).context("无法创建 Maven 缓存目录")?;
        let temporary = target.with_file_name(format!(
            ".{}.part-{}",
            target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("artifact"),
            unique_suffix()
        ));
        let mut file = File::create(&temporary).context("无法创建 Maven 缓存临时文件")?;
        io::copy(&mut response, &mut file).context("下载 Maven Central 构件失败")?;
        drop(file);
        fs::rename(&temporary, &target).context("无法保存 Maven Central 构件缓存")?;

        self.resolve_file(components)
    }

    fn cache_parent(&self, components: &[String]) -> Result<PathBuf> {
        let parent = components
            .iter()
            .fold(self.root.as_ref().clone(), |path, component| {
                path.join(component)
            });
        fs::create_dir_all(&parent).context("无法创建 Maven 缓存目录")?;
        let canonical = fs::canonicalize(&parent).context("解析 Maven 缓存目录失败")?;
        if !canonical.starts_with(self.root.as_path()) || !canonical.is_dir() {
            return Err(anyhow!("bad path"));
        }
        Ok(canonical)
    }

    fn resolve_file(&self, components: &[String]) -> Result<Response> {
        open_response(&self.safe_path(components)?)
    }

    fn safe_path(&self, components: &[String]) -> Result<PathBuf> {
        if components.is_empty() || components.iter().any(|component| component.is_empty()) {
            return Err(anyhow!("bad path"));
        }
        let path = components
            .iter()
            .fold(self.root.as_ref().clone(), |path, component| {
                path.join(component)
            });
        let canonical = fs::canonicalize(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                anyhow!("not found")
            } else {
                anyhow!(error).context("解析仓库路径失败")
            }
        })?;
        if !canonical.starts_with(self.root.as_path()) {
            return Err(anyhow!("bad path"));
        }
        Ok(canonical)
    }

    fn generate_metadata(&self, components: &[String]) -> Result<Response> {
        let directory_components = &components[..components.len() - 1];
        let directory = self.safe_path(directory_components)?;
        if !directory.is_dir() {
            return Err(anyhow!("not found"));
        }

        let child_directories = fs::read_dir(&directory)
            .context("读取 Maven 仓库目录失败")?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                let name = entry.file_name().into_string().ok()?;
                (metadata.is_dir() && !name.starts_with('.')).then_some(name)
            })
            .collect::<Vec<_>>();

        let xml = if !child_directories.is_empty() {
            self.artifact_metadata(directory_components, child_directories)
        } else if directory_components.len() >= 3 {
            self.version_metadata(directory_components, &directory)?
        } else {
            return Err(anyhow!("not found"));
        };

        Ok(Response {
            status: "200 OK",
            content_type: "application/xml; charset=utf-8",
            body: Body::Bytes(xml.into_bytes()),
        })
    }

    fn artifact_metadata(&self, directory: &[String], mut versions: Vec<String>) -> String {
        versions.sort();
        let artifact_id = directory.last().map(String::as_str).unwrap_or_default();
        let group_id = directory[..directory.len().saturating_sub(1)].join(".");
        let latest = versions.last().map(String::as_str).unwrap_or_default();
        let versions_xml = versions
            .iter()
            .map(|version| format!("<version>{}</version>", xml_escape(version)))
            .collect::<String>();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <versioning>\n    <latest>{}</latest>\n    <release>{}</release>\n    <versions>{}</versions>\n    <lastUpdated>19700101000000</lastUpdated>\n  </versioning>\n</metadata>\n",
            xml_escape(&group_id),
            xml_escape(artifact_id),
            xml_escape(latest),
            xml_escape(latest),
            versions_xml
        )
    }

    fn version_metadata(&self, directory: &[String], version_directory: &Path) -> Result<String> {
        let artifact_id = &directory[directory.len() - 2];
        let version = &directory[directory.len() - 1];
        let group_id = directory[..directory.len() - 2].join(".");
        let prefix = format!("{artifact_id}-{version}");
        let mut snapshot_versions = Vec::new();

        for entry in fs::read_dir(version_directory).context("读取 Maven 版本目录失败")? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || ignored_file(&entry.path()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(suffix) = name.strip_prefix(&prefix) else {
                continue;
            };
            let (classifier, extension) = if let Some(extension) = suffix.strip_prefix('.') {
                (None, extension)
            } else if let Some(suffix) = suffix.strip_prefix('-') {
                let Some((classifier, extension)) = suffix.rsplit_once('.') else {
                    continue;
                };
                (Some(classifier), extension)
            } else {
                continue;
            };
            if extension.is_empty() || classifier.is_some_and(str::is_empty) {
                continue;
            }
            let classifier_xml = classifier.map_or_else(String::new, |classifier| {
                format!("<classifier>{}</classifier>", xml_escape(classifier))
            });
            snapshot_versions.push(format!(
                "<snapshotVersion><extension>{}</extension>{}<value>{}</value><updated>19700101000000</updated></snapshotVersion>",
                xml_escape(extension),
                classifier_xml,
                xml_escape(version)
            ));
        }
        snapshot_versions.sort();
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <version>{}</version>\n  <versioning>\n    <snapshot><localCopy>true</localCopy></snapshot>\n    <lastUpdated>19700101000000</lastUpdated>\n    <snapshotVersions>{}</snapshotVersions>\n  </versioning>\n</metadata>\n",
            xml_escape(&group_id),
            xml_escape(artifact_id),
            xml_escape(version),
            snapshot_versions.join("")
        ))
    }
}

fn open_response(path: &Path) -> Result<Response> {
    let file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            anyhow!("not found")
        } else {
            anyhow!(error).context("打开仓库文件失败")
        }
    })?;
    let metadata = file.metadata().context("读取仓库文件信息失败")?;
    if !metadata.is_file() || ignored_file(path) {
        return Err(anyhow!("not found"));
    }
    Ok(Response {
        status: "200 OK",
        content_type: content_type(path),
        body: Body::File(file, metadata.len()),
    })
}

fn decode_path(path: &str) -> Option<Vec<String>> {
    path.split('/')
        .map(|component| {
            let bytes = component.as_bytes();
            let mut decoded = Vec::with_capacity(bytes.len());
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index] == b'%' {
                    if index + 2 >= bytes.len() {
                        return None;
                    }
                    let high = hex_value(bytes[index + 1])?;
                    let low = hex_value(bytes[index + 2])?;
                    decoded.push(high * 16 + low);
                    index += 3;
                } else {
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
            let value = String::from_utf8(decoded).ok()?;
            if value.is_empty()
                || value == "."
                || value == ".."
                || value.contains('/')
                || value.contains('\\')
                || value.contains(':')
                || value.chars().any(char::is_control)
            {
                return None;
            }
            Some(value)
        })
        .collect()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn sha1_hex<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut digest = Sha1::default();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize())
}

struct Sha1 {
    state: [u32; 5],
    buffer: [u8; 64],
    buffer_len: usize,
    length: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self {
            state: [0; 5],
            buffer: [0; 64],
            buffer_len: 0,
            length: 0,
        }
    }
}

impl Sha1 {
    fn update(&mut self, bytes: &[u8]) {
        if self.state == [0; 5] {
            self.state = [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ];
        }
        self.length += bytes.len() as u64;
        let mut offset = 0;
        while offset < bytes.len() {
            let available = 64 - self.buffer_len;
            let count = available.min(bytes.len() - offset);
            self.buffer[self.buffer_len..self.buffer_len + count]
                .copy_from_slice(&bytes[offset..offset + count]);
            self.buffer_len += count;
            offset += count;
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.process(&block);
                self.buffer_len = 0;
            }
        }
    }

    fn finalize(mut self) -> String {
        let bit_length = self.length * 8;
        self.update(&[0x80]);
        let padding = [0_u8; 64];
        while self.buffer_len != 56 {
            let count = if self.buffer_len < 56 {
                56 - self.buffer_len
            } else {
                64 - self.buffer_len
            };
            self.update(&padding[..count]);
        }
        self.update(&bit_length.to_be_bytes());
        let mut result = String::with_capacity(40);
        for word in self.state {
            result.push_str(&format!("{word:08x}"));
        }
        result
    }

    fn process(&mut self, block: &[u8; 64]) {
        let mut words = [0_u32; 80];
        for (index, word) in words[..16].iter_mut().enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                block[offset],
                block[offset + 1],
                block[offset + 2],
                block[offset + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temporary = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temporary;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn ignored_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name == "_remote.repositories" || name.ends_with(".lastUpdated") || name.ends_with(".part")
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("pom") | Some("xml") => "application/xml",
        Some("json") => "application/json",
        Some("jar") | Some("war") | Some("aar") | Some("zip") => "application/java-archive",
        Some("sha1") | Some("md5") | Some("sha256") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn write_response<W: Write>(
    stream: &mut W,
    response: Response,
    head_only: bool,
    keep_alive: bool,
) -> Result<()> {
    let length = response.body.len();
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nCache-Control: no-cache\r\nConnection: {connection}\r\nKeep-Alive: timeout=10, max={}\r\n\r\n",
        response.status, length, response.content_type, MAX_REQUESTS_PER_CONNECTION
    )?;
    if head_only {
        return Ok(());
    }
    match response.body {
        Body::File(mut file, _) => {
            io::copy(&mut file, stream).context("发送仓库文件失败")?;
        }
        Body::Bytes(bytes) => {
            stream.write_all(&bytes).context("发送响应失败")?;
        }
    }
    Ok(())
}

fn write_error<W: Write>(
    stream: &mut W,
    status: &'static str,
    message: &str,
    keep_alive: bool,
    allow: Option<&str>,
) -> Result<()> {
    let allow_header = allow.map_or(String::new(), |value| format!("Allow: {value}\r\n"));
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 {status}\r\n{allow_header}Content-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: {connection}\r\n\r\n{message}",
        message.len(),
    )?;
    stream.flush().context("发送错误响应失败")?;
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_decodes_safe_segments() {
        assert!(decode_path("../secret").is_none());
        assert!(decode_path("%2e%2e/secret").is_none());
        assert!(decode_path("g%72oup/a/artifact.jar").is_some());
        assert!(decode_path("g%2Fa/a.jar").is_none());
    }

    #[test]
    fn escapes_generated_xml() {
        assert_eq!(xml_escape("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
    }

    #[test]
    fn http11_is_keep_alive_by_default() {
        let request = "GET /a.jar HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(parse_request(request).unwrap().2, true);
    }

    #[test]
    fn connection_close_disables_keep_alive() {
        let request = "GET /a.jar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        assert!(!parse_request(request).unwrap().2);
    }

    #[test]
    fn http10_needs_explicit_keep_alive() {
        let request = "GET /a.jar HTTP/1.0\r\nHost: localhost\r\n\r\n";
        assert!(!parse_request(request).unwrap().2);
        let request = "GET /a.jar HTTP/1.0\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        assert!(parse_request(request).unwrap().2);
    }

    #[test]
    fn calculates_sha1() {
        let mut input = "abc".as_bytes();
        assert_eq!(
            sha1_hex(&mut input).unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
