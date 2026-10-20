//! HTTP 客户端构建：纯 Rust TLS 提供者 + 显式超时。
//!
//! 所有客户端都必须带连接超时和总请求超时，避免上游仓库卡住时长时间
//! 占满工作线程。

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use rustls::{ClientConfig, RootCertStore};
use std::sync::Arc;
use std::time::Duration;

/// HTTP 超时与缓存容量限制，取值来自配置文件 `[http]` 段。
#[derive(Debug, Clone, Copy)]
pub struct HttpSettings {
    /// 建立 TCP/TLS 连接的超时。
    pub connect_timeout: Duration,
    /// 普通请求（HEAD / 上游回源）的总超时。
    pub request_timeout: Duration,
    /// 上传（PUT 大文件）的总超时。
    pub upload_timeout: Duration,
    /// 单个上游构件允许缓存的最大字节数。
    pub max_cache_file_bytes: u64,
    /// 仓库缓存允许占用的最大总字节数。
    pub max_cache_bytes: u64,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            upload_timeout: Duration::from_secs(600),
            max_cache_file_bytes: 256 * 1024 * 1024,
            max_cache_bytes: 10 * 1024 * 1024 * 1024,
        }
    }
}

impl HttpSettings {
    /// 普通请求客户端（带 `request_timeout`）。
    pub fn standard_client(&self) -> Result<Client> {
        build_client(self, self.request_timeout)
    }

    /// 上传客户端（带更宽松的 `upload_timeout`，允许上传较大的构件）。
    pub fn upload_client(&self) -> Result<Client> {
        build_client(self, self.upload_timeout)
    }
}

fn build_client(settings: &HttpSettings, request_timeout: Duration) -> Result<Client> {
    // 1. 初始化纯 Rust 的加密提供者 (RustCrypto)，替代默认的 ring 或 aws-lc-rs
    let provider = rustls_rustcrypto::provider();

    // 2. 加载 webpki-roots 提供的 Mozilla 根证书集
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // 3. 构建 rustls 配置，明确指定使用上面的提供者
    let tls_config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("无法配置协议版本")?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // 4. 注入 reqwest，并配置超时（reqwest 的 timeout 覆盖整个请求与响应读取）
    let client = Client::builder()
        .use_preconfigured_tls(tls_config)
        .connect_timeout(settings.connect_timeout)
        .timeout(request_timeout)
        .pool_max_idle_per_host(4)
        .user_agent(concat!("maven-util/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("无法构建 reqwest 客户端")?;

    Ok(client)
}
