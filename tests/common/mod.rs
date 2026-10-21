//! 集成测试公用工具：可控的假上游仓库、临时目录、Maven 仓库夹具。

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 假上游对某个路径的响应。
pub enum Reply {
    /// 200 + 内容
    Artifact(Vec<u8>),
    /// 200 + 内容，但先延迟指定时间（用于制造并发）
    Delayed(Vec<u8>, Duration),
    /// 指定状态码（无响应体）
    Status(u16),
    /// HEAD 与其他方法返回不同状态码（用于上传测试：远端不存在但 PUT 成功）
    MethodStatus { head: u16, other: u16 },
    /// 指定状态码 + JSON 响应体（用于 Components API 的存在性查询）
    Json(u16, String),
    /// PUT 前 N 次返回失败状态码、之后成功（用于重试测试）；HEAD 恒为 404。
    FlakyPut {
        counter: Arc<AtomicUsize>,
        fail_times: usize,
        fail_code: u16,
        then_code: u16,
    },
}

/// 一个最小可用的 HTTP 服务器，用于充当 Maven 上游或被上传的目标仓库。
pub struct FakeUpstream {
    pub base_url: String,
    pub address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FakeUpstream {
    pub fn start(routes: HashMap<String, Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口失败");
        let address = listener.local_addr().expect("读取监听地址失败");
        listener.set_nonblocking(true).expect("设置非阻塞失败");

        let routes = Arc::new(routes);
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_stop = Arc::clone(&stop);
        let thread_requests = Arc::clone(&requests);
        let thread_hits = Arc::clone(&hits);
        let join = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let routes = Arc::clone(&routes);
                        let requests = Arc::clone(&thread_requests);
                        let hits = Arc::clone(&thread_hits);
                        thread::spawn(move || handle(stream, &routes, &requests, &hits));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{address}"),
            address,
            requests,
            hits,
            stop,
            join: Some(join),
        }
    }

    /// 收到的原始请求数。
    pub fn hit_count(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    /// 收到的原始请求文本。
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// 是否有请求包含指定子串（例如某个用户名或路径）。
    pub fn request_contains(&self, needle: &str) -> bool {
        self.requests()
            .iter()
            .any(|request| request.contains(needle))
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle(
    stream: TcpStream,
    routes: &HashMap<String, Reply>,
    requests: &Mutex<Vec<String>>,
    hits: &AtomicUsize,
) {
    let mut stream = stream;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let mut raw = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                raw.push(byte[0]);
                if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let mut text = String::from_utf8_lossy(&raw).into_owned();

    // 读取请求体：multipart 提交需要完整读完，否则客户端可能写到一半被重置
    let content_length = text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        let mut read = 0;
        while read < content_length {
            match stream.read(&mut body[read..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n,
            }
        }
        body.truncate(read);
    }
    text.push_str(&String::from_utf8_lossy(&body));
    requests.lock().unwrap().push(text.clone());
    hits.fetch_add(1, Ordering::Relaxed);

    let mut parts = text.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();

    // 路由支持两种键：`METHOD /path`（按方法区分）与 `/path`（所有方法共用）
    let method_key = format!("{method} {path}");
    let lookup = routes.get(&method_key).or_else(|| routes.get(&path));
    let response: Vec<u8> = match lookup {
        Some(Reply::Artifact(body)) => reply(200, "OK", body, &method),
        Some(Reply::Delayed(body, delay)) => {
            thread::sleep(*delay);
            reply(200, "OK", body, &method)
        }
        Some(Reply::MethodStatus { head, other }) => {
            let code = if method == "HEAD" { *head } else { *other };
            let reason = match code {
                200 | 201 | 204 => "Created",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Status",
            };
            reply(code, reason, b"", &method)
        }
        Some(Reply::FlakyPut {
            counter,
            fail_times,
            fail_code,
            then_code,
        }) => {
            if method == "HEAD" {
                reply(404, "Not Found", b"", &method)
            } else {
                let nth = counter.fetch_add(1, Ordering::Relaxed);
                let code = if nth < *fail_times {
                    *fail_code
                } else {
                    *then_code
                };
                let reason = match code {
                    200 | 201 => "Created",
                    503 => "Service Unavailable",
                    _ => "Status",
                };
                reply(code, reason, b"", &method)
            }
        }
        Some(Reply::Json(code, body)) => {
            let reason = if *code == 200 { "OK" } else { "Status" };
            reply(*code, reason, body.as_bytes(), &method)
        }
        Some(Reply::Status(code)) => {
            let reason = match code {
                400 => "Bad Request",
                404 => "Not Found",
                401 => "Unauthorized",
                500 => "Internal Server Error",
                503 => "Service Unavailable",
                _ => "Status",
            };
            format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes()
        }
        None => reply(404, "Not Found", b"", &method),
    };
    let _ = stream.write_all(&response);
}

fn reply(code: u16, reason: &str, body: &[u8], method: &str) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nContent-Type: application/java-archive\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut response = head.into_bytes();
    if method != "HEAD" {
        response.extend_from_slice(body);
    }
    response
}

/// 发起一次原始 HTTP 请求，返回 `(状态码, 响应体)`。
pub fn request(address: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).expect("连接测试服务器失败");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时失败");
    let raw = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(raw.as_bytes()).expect("发送请求失败");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("无法解析状态码");
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, body)
}

pub fn get(address: SocketAddr, path: &str) -> (u16, String) {
    request(address, "GET", path)
}

pub fn head(address: SocketAddr, path: &str) -> (u16, String) {
    request(address, "HEAD", path)
}

/// 唯一临时目录。
pub fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "maven-util-it-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("创建临时目录失败");
    path
}

/// 构造一个标准 Maven 仓库夹具：`org/example/demo/1.0/` 下的 pom + jar。
pub fn maven_repo_fixture(root: &Path) -> PathBuf {
    let artifact_dir = root.join("org/example/demo/1.0");
    std::fs::create_dir_all(&artifact_dir).expect("创建夹具目录失败");
    std::fs::write(artifact_dir.join("demo-1.0.pom"), POM).expect("写入 pom 失败");
    std::fs::write(artifact_dir.join("demo-1.0.jar"), JAR).expect("写入 jar 失败");
    artifact_dir
}

pub const POM: &[u8] = br#"<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>demo</artifactId><version>1.0</version></project>"#;
pub const JAR: &[u8] = b"jar-bytes-not-a-real-jar";
pub const SOURCES_JAR: &[u8] = b"sources-jar-bytes";
pub const JAR_SHA1: &[u8] = b"0123456789abcdef0123456789abcdef01234567";

/// 更丰富的夹具：在 pom + jar 基础上补 sources jar 与本地校验和文件，
/// 用于验证 Components 模式的 classifier 解析与「校验和不上传」。
pub fn maven_repo_fixture_with_extras(root: &Path) -> PathBuf {
    let artifact_dir = maven_repo_fixture(root);
    std::fs::write(artifact_dir.join("demo-1.0-sources.jar"), SOURCES_JAR)
        .expect("写入 sources jar 失败");
    std::fs::write(artifact_dir.join("demo-1.0.jar.sha1"), JAR_SHA1).expect("写入 sha1 失败");
    std::fs::write(
        artifact_dir.join("demo-1.0.pom.md5"),
        b"0123456789abcdef0123456789abcdef",
    )
    .expect("写入 md5 失败");
    artifact_dir
}

/// 检查目录下是否存在指定后缀的 `.part` 临时缓存文件。
pub fn has_partial_files(root: &Path) -> bool {
    jwalk_partial(root)
}

fn jwalk_partial(root: &Path) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(name) = path.file_name().and_then(|name| name.to_str())
                && name.contains(".part-")
            {
                return true;
            }
        }
    }
    false
}
