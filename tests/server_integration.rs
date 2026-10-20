//! 只读 Maven 仓库服务的 HTTP 集成测试。
//!
//! 覆盖真实 HTTP GET/HEAD/400/404/405/502、上游缓存、元数据生成、
//! 缓存容量限制与并发访问。

mod common;

use common::{FakeUpstream, JAR, get, has_partial_files, head, request, temp_dir};
use maven_util::server::{CacheLimits, ServerHandle, start_server};
use maven_util::tls::HttpSettings;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::thread;

fn client() -> Arc<reqwest::blocking::Client> {
    Arc::new(
        HttpSettings::default()
            .standard_client()
            .expect("构建 HTTP 客户端失败"),
    )
}

fn start(root: &Path, upstreams: Vec<String>, cache: CacheLimits) -> (ServerHandle, SocketAddr) {
    let handle = start_server(maven_util::server::ServerArgs {
        root: root.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        upstreams,
        client: client(),
        cache,
    })
    .expect("启动测试仓库失败");
    let address = handle.address();
    (handle, address)
}

fn unlimited() -> CacheLimits {
    CacheLimits {
        max_file_bytes: 64 * 1024 * 1024,
        max_cache_bytes: 1024 * 1024 * 1024,
    }
}

#[test]
fn serves_local_artifact_over_get_and_head() {
    let root = temp_dir("local-get");
    let artifact_dir = root.join("org/example/demo/1.0");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::write(artifact_dir.join("demo-1.0.jar"), JAR).unwrap();

    let (_handle, address) = start(&root, vec![], unlimited());

    let (status, body) = get(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 200);
    assert_eq!(body.as_bytes(), JAR);

    let (status, body) = head(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 200);
    assert!(body.is_empty(), "HEAD 响应不应包含响应体");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn generates_missing_checksum_and_metadata() {
    let root = temp_dir("checksum-metadata");
    let artifact_dir = root.join("org/example/demo/1.0");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::write(artifact_dir.join("demo-1.0.jar"), JAR).unwrap();

    let (_handle, address) = start(&root, vec![], unlimited());

    let (status, checksum) = get(address, "/org/example/demo/1.0/demo-1.0.jar.sha1");
    assert_eq!(status, 200);
    let expected = maven_util::checksum::sha1_hex_bytes(JAR);
    assert_eq!(checksum.trim(), expected);

    let (status, metadata) = get(address, "/org/example/demo/maven-metadata.xml");
    assert_eq!(status, 200);
    assert!(metadata.contains("<artifactId>demo</artifactId>"));
    assert!(metadata.contains("<version>1.0</version>"));

    // 生成的元数据同样需要能取到校验和
    let (status, checksum) = get(address, "/org/example/demo/maven-metadata.xml.sha1");
    assert_eq!(status, 200);
    assert_eq!(checksum.trim().len(), 40);

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn returns_404_for_missing_artifact() {
    let root = temp_dir("not-found");
    let upstream = FakeUpstream::start(HashMap::new());
    let (_handle, address) = start(&root, vec![upstream.base_url.clone()], unlimited());

    let (status, _) = get(address, "/org/example/absent/1.0/absent-1.0.jar");
    assert_eq!(status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn returns_502_when_upstream_is_unavailable() {
    let root = temp_dir("upstream-down");
    // 127.0.0.1:1 没有任何服务监听，连接会立刻失败
    let (_handle, address) = start(&root, vec!["http://127.0.0.1:1".into()], unlimited());

    let (status, _) = get(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 502);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn returns_502_when_upstream_errors() {
    let root = temp_dir("upstream-500");
    let mut routes = HashMap::new();
    routes.insert(
        "/org/example/demo/1.0/demo-1.0.jar".to_string(),
        common::Reply::Status(503),
    );
    let upstream = FakeUpstream::start(routes);
    let (_handle, address) = start(&root, vec![upstream.base_url.clone()], unlimited());

    let (status, _) = get(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 502);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rejects_bad_paths_and_methods() {
    let root = temp_dir("bad-request");
    let (_handle, address) = start(&root, vec![], unlimited());

    let (status, _) = request(address, "GET", "/../secret.jar");
    assert_eq!(status, 400);

    let (status, _) = request(address, "GET", "//double-slash.jar");
    assert_eq!(status, 400);

    let (status, _) = request(address, "POST", "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 405);

    let (status, _) = request(address, "DELETE", "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 405);

    // 真正畸形的请求行（缺少 HTTP 版本）
    {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(b"GARBAGE\r\n\r\n").unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400"), "实际响应: {text}");
    }

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn never_serves_maven_state_files() {
    let root = temp_dir("state-files");
    let artifact_dir = root.join("org/example/demo/1.0");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::write(artifact_dir.join("demo-1.0.jar"), JAR).unwrap();
    std::fs::write(artifact_dir.join("_remote.repositories"), b"remote").unwrap();
    std::fs::write(artifact_dir.join("demo-1.0.jar.lastUpdated"), b"x").unwrap();

    let (_handle, address) = start(&root, vec![], unlimited());
    let (status, _) = get(address, "/org/example/demo/1.0/_remote.repositories");
    assert_eq!(status, 404);
    let (status, _) = get(address, "/org/example/demo/1.0/demo-1.0.jar.lastUpdated");
    assert_eq!(status, 404);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn fetches_from_upstream_and_caches_once() {
    let root = temp_dir("upstream-cache");
    let mut routes = HashMap::new();
    routes.insert(
        "/org/example/demo/1.0/demo-1.0.jar".to_string(),
        common::Reply::Artifact(JAR.to_vec()),
    );
    let upstream = FakeUpstream::start(routes);
    let (_handle, address) = start(&root, vec![upstream.base_url.clone()], unlimited());

    let (status, body) = get(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 200);
    assert_eq!(body.as_bytes(), JAR);

    let cached = root.join("org/example/demo/1.0/demo-1.0.jar");
    assert!(cached.is_file(), "构件必须被缓存到本地仓库");
    assert_eq!(std::fs::read(&cached).unwrap(), JAR);
    assert_eq!(upstream.hit_count(), 1);

    // 第二次请求命中本地缓存，不再回源
    let (status, body) = get(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 200);
    assert_eq!(body.as_bytes(), JAR);
    assert_eq!(upstream.hit_count(), 1, "缓存命中后不应继续请求上游");

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn concurrent_fetches_keep_cache_consistent() {
    let root = temp_dir("concurrent");
    let mut routes = HashMap::new();
    routes.insert(
        "/org/example/demo/1.0/demo-1.0.jar".to_string(),
        common::Reply::Delayed(JAR.to_vec(), std::time::Duration::from_millis(50)),
    );
    let upstream = FakeUpstream::start(routes);
    let (_handle, address) = start(&root, vec![upstream.base_url.clone()], unlimited());

    let threads: Vec<_> = (0..16)
        .map(|_| thread::spawn(move || get(address, "/org/example/demo/1.0/demo-1.0.jar")))
        .collect();

    for handle in threads {
        let (status, body) = handle.join().expect("并发请求线程失败");
        assert_eq!(status, 200);
        assert_eq!(body.as_bytes(), JAR);
    }

    let cached = root.join("org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(
        std::fs::read(&cached).expect("并发结束后缓存必须存在"),
        JAR,
        "并发下载后缓存内容必须完整"
    );
    assert!(
        !has_partial_files(&root),
        "并发下载结束后不能残留 .part 临时文件"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn oversized_artifact_is_streamed_without_caching() {
    let root = temp_dir("oversized");
    let mut routes = HashMap::new();
    routes.insert(
        "/org/example/demo/1.0/demo-1.0.jar".to_string(),
        common::Reply::Artifact(JAR.to_vec()),
    );
    let upstream = FakeUpstream::start(routes);
    let cache = CacheLimits {
        max_file_bytes: 4, // 小于 JAR 长度
        max_cache_bytes: 1024 * 1024 * 1024,
    };
    let (_handle, address) = start(&root, vec![upstream.base_url.clone()], cache);

    let (status, body) = get(address, "/org/example/demo/1.0/demo-1.0.jar");
    assert_eq!(status, 200, "超过单文件上限时应当转发而不是失败");
    assert_eq!(body.as_bytes(), JAR);
    assert!(
        !root.join("org/example/demo/1.0/demo-1.0.jar").exists(),
        "超过单文件上限的构件不能落盘"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn full_cache_budget_is_streamed_without_caching() {
    let root = temp_dir("budget");
    let artifact_dir = root.join("org/example/demo/1.0");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    // 预先塞满缓存预算
    std::fs::write(artifact_dir.join("filler.jar"), vec![0_u8; 64]).unwrap();

    let mut routes = HashMap::new();
    routes.insert(
        "/org/example/demo/2.0/demo-2.0.jar".to_string(),
        common::Reply::Artifact(JAR.to_vec()),
    );
    let upstream = FakeUpstream::start(routes);
    let cache = CacheLimits {
        max_file_bytes: 1024 * 1024,
        max_cache_bytes: 64, // 已被 filler.jar 占满
    };
    let (_handle, address) = start(&root, vec![upstream.base_url.clone()], cache);

    let (status, body) = get(address, "/org/example/demo/2.0/demo-2.0.jar");
    assert_eq!(status, 200, "缓存已满时应当转发而不是失败");
    assert_eq!(body.as_bytes(), JAR);
    assert!(
        !root.join("org/example/demo/2.0/demo-2.0.jar").exists(),
        "缓存已满时不能继续落盘"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn keeps_working_across_keep_alive_requests() {
    let root = temp_dir("keep-alive");
    let artifact_dir = root.join("org/example/demo/1.0");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    std::fs::write(artifact_dir.join("demo-1.0.jar"), JAR).unwrap();

    let (_handle, address) = start(&root, vec![], unlimited());

    let mut stream = std::net::TcpStream::connect(address).unwrap();
    use std::io::{Read, Write};
    let first = b"GET /org/example/demo/1.0/demo-1.0.jar HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let second = b"GET /org/example/demo/1.0/missing.jar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(first).unwrap();
    stream.write_all(second).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 1);
    assert_eq!(text.matches("HTTP/1.1 404 Not Found").count(), 1);

    std::fs::remove_dir_all(&root).unwrap();
}
