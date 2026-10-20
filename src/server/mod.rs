//! 只读 Maven 仓库服务。
//!
//! Maven 仓库本质上是一个约定了目录结构和元数据文件格式的 HTTP 仓库。
//! 这里不提供目录上传等通用文件服务功能，只允许读取仓库中的公开文件。

pub mod http;
pub mod metadata;
pub mod repository;

pub use repository::{CacheBudget, Repository, ResolveError};

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::{fs, thread as std_thread, time::Duration};

use crate::tls::HttpSettings;
use http::{MAX_REQUESTS_PER_CONNECTION, read_request};

const WORKER_COUNT: usize = 16;

/// 缓存限制（字节）。
#[derive(Debug, Clone, Copy)]
pub struct CacheLimits {
    /// 单个构件的缓存上限，超出时只转发不落盘。
    pub max_file_bytes: u64,
    /// 缓存总容量上限，超出时只转发不落盘。
    pub max_cache_bytes: u64,
}

impl Default for CacheLimits {
    fn default() -> Self {
        let settings = HttpSettings::default();
        Self {
            max_file_bytes: settings.max_cache_file_bytes,
            max_cache_bytes: settings.max_cache_bytes,
        }
    }
}

impl From<&HttpSettings> for CacheLimits {
    fn from(settings: &HttpSettings) -> Self {
        Self {
            max_file_bytes: settings.max_cache_file_bytes,
            max_cache_bytes: settings.max_cache_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerArgs {
    pub root: PathBuf,
    pub host: String,
    pub port: u16,
    pub upstreams: Vec<String>,
    pub client: Arc<Client>,
    pub cache: CacheLimits,
}

pub struct ServerHandle {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<std_thread::JoinHandle<()>>,
    sender: Option<Sender<TcpStream>>,
    workers: Vec<std_thread::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn base_url(&self) -> String {
        format!("http://{}/", self.address)
    }

    pub fn address(&self) -> std::net::SocketAddr {
        self.address
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

pub fn run_server(args: ServerArgs) -> Result<()> {
    let handle = start_server(args)?;
    println!(
        "Maven 只读仓库已启动: {} (按 Ctrl+C 停止)",
        handle.base_url()
    );
    loop {
        std_thread::park();
    }
}

pub fn start_server(args: ServerArgs) -> Result<ServerHandle> {
    let root = fs::canonicalize(&args.root)
        .with_context(|| format!("无法定位 Maven 仓库目录: {}", args.root.display()))?;
    if !root.is_dir() {
        return Err(anyhow!("Maven 仓库路径不是目录: {}", root.display()));
    }

    let bind_address = format!("{}:{}", args.host, args.port);
    let listener = std::net::TcpListener::bind(&bind_address)
        .with_context(|| format!("无法监听地址: {bind_address}"))?;
    let actual_address = listener.local_addr().context("无法读取监听地址")?;
    listener
        .set_nonblocking(true)
        .context("设置监听非阻塞模式失败")?;

    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel::<TcpStream>();
    let receiver = Arc::new(Mutex::new(receiver));
    let repository = Repository::new(root, args.upstreams, args.client, args.cache);

    let mut workers = Vec::with_capacity(WORKER_COUNT);
    for _ in 0..WORKER_COUNT {
        let receiver = Arc::clone(&receiver);
        let repository = repository.clone();
        workers.push(std_thread::spawn(move || worker_loop(receiver, repository)));
    }

    let thread_stop = Arc::clone(&stop);
    let thread_sender = sender.clone();
    let join = std_thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Windows 可能把监听套接字的非阻塞模式传播给已连接套接字，
                    // 请求解析依赖读超时，因此这里显式恢复为阻塞模式。
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("设置连接阻塞模式失败: {error}");
                        continue;
                    }
                    if thread_sender.send(stream).is_err() {
                        break;
                    }
                }
                Err(error) if is_would_block(&error) => {
                    std_thread::sleep(Duration::from_millis(10));
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
        // WSAEWOULDBLOCK。Windows 工具链不总是把该错误映射为 WouldBlock。
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
    let mut reader = http::buffered(stream);
    for request_number in 0..MAX_REQUESTS_PER_CONNECTION {
        let Some(request) = read_request(&mut reader)? else {
            return Ok(());
        };
        let (method, target, keep_alive) = match http::parse_request(&request) {
            Ok(request) => request,
            Err(_) => {
                http::write_error(
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
            http::write_error(
                reader.get_mut(),
                "405 Method Not Allowed",
                "仅支持 GET 和 HEAD 请求\n",
                false,
                Some("GET, HEAD"),
            )?;
            return Ok(());
        }

        // HTTP 状态码由错误类型决定，而不是错误字符串。
        match repository.resolve(target) {
            Ok(response) => {
                http::write_response(reader.get_mut(), response, method == "HEAD", keep_alive)?
            }
            Err(ResolveError::NotFound) => {
                http::write_error(
                    reader.get_mut(),
                    "404 Not Found",
                    "文件不存在\n",
                    keep_alive,
                    None,
                )?;
            }
            Err(ResolveError::BadPath) => {
                http::write_error(
                    reader.get_mut(),
                    "400 Bad Request",
                    "非法仓库路径\n",
                    false,
                    None,
                )?;
                return Ok(());
            }
            Err(ResolveError::UpstreamUnavailable(detail)) => {
                http::write_error(
                    reader.get_mut(),
                    "502 Bad Gateway",
                    "Maven 上游暂时不可用\n",
                    false,
                    None,
                )?;
                eprintln!("上游回源失败: {detail}");
                return Ok(());
            }
            Err(ResolveError::Internal(error)) => {
                let _ = http::write_error(
                    reader.get_mut(),
                    "500 Internal Server Error",
                    "内部错误\n",
                    false,
                    None,
                );
                return Err(error);
            }
        }

        if !keep_alive {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::SystemTime;

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "maven-util-server-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rejects_non_directory_root() {
        let root = temp_dir("not-a-dir");
        let file = root.join("not-a-dir.txt");
        fs::write(&file, b"x").unwrap();
        let error = start_server(ServerArgs {
            root: file.clone(),
            host: "127.0.0.1".into(),
            port: 0,
            upstreams: vec![],
            client: Arc::new(HttpSettings::default().standard_client().unwrap()),
            cache: CacheLimits::default(),
        })
        .err()
        .expect("非目录根目录必须启动失败");
        assert!(error.to_string().contains("不是目录"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_limits_default_is_sane() {
        let limits = CacheLimits::default();
        assert!(limits.max_file_bytes > 0);
        assert!(limits.max_cache_bytes >= limits.max_file_bytes);
    }

    #[test]
    fn bind_ephemeral_port_is_available() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }
}
