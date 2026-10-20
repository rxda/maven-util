//! 仓库解析：本地文件、生成校验和、上游回源与缓存。
//!
//! 解析过程中的错误使用 [`ResolveError`] 明确区分，HTTP 层据此决定返回
//! 404 / 400 / 502 / 500，而不是比较错误字符串。

use anyhow::anyhow;
use reqwest::blocking::Client;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::CacheLimits;
use super::http::{self, Response};
use crate::checksum;

pub type ResolveResult<T> = Result<T, ResolveError>;

/// 仓库解析错误。
#[derive(Debug)]
pub enum ResolveError {
    /// 目标不存在。
    NotFound,
    /// 路径非法（越界、空组件、包含控制字符等）。
    BadPath,
    /// 所有上游都不可用（网络错误或非 404 的失败响应）。
    UpstreamUnavailable(String),
    /// 其他内部错误（磁盘、权限等）。
    Internal(anyhow::Error),
}

impl ResolveError {
    pub(super) fn internal(
        context: &str,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Internal(anyhow!(error).context(context.to_string()))
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(formatter, "文件不存在"),
            Self::BadPath => write!(formatter, "非法仓库路径"),
            Self::UpstreamUnavailable(detail) => write!(formatter, "上游仓库不可用: {detail}"),
            Self::Internal(error) => write!(formatter, "内部错误: {error:#}"),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => error.source(),
            _ => None,
        }
    }
}

/// 缓存总容量预算：统计仓库当前占用，超出上限的构件只转发不落盘。
pub struct CacheBudget {
    max_total: u64,
    used: AtomicU64,
    measured: AtomicBool,
    gate: Mutex<()>,
}

impl CacheBudget {
    pub fn new(max_total: u64) -> Self {
        Self {
            max_total,
            used: AtomicU64::new(0),
            measured: AtomicBool::new(false),
            gate: Mutex::new(()),
        }
    }

    /// 首次使用时统计仓库现有占用（只统计一次）。
    fn ensure_measured(&self, root: &Path) {
        if self.measured.load(Ordering::Acquire) {
            return;
        }
        let _guard = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        if self.measured.load(Ordering::Acquire) {
            return;
        }
        self.used.store(directory_size(root), Ordering::Relaxed);
        self.measured.store(true, Ordering::Release);
    }

    /// 尝试占用 `bytes` 字节缓存预算。
    pub fn try_reserve(&self, root: &Path, bytes: u64) -> bool {
        self.ensure_measured(root);
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > self.max_total {
                return false;
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// 释放预算（写入失败时归还）。
    pub fn release(&self, bytes: u64) {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(bytes))
            })
            .ok();
    }
}

fn directory_size(path: &Path) -> u64 {
    jwalk::WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

/// 本地仓库 + 上游回源的只读 Maven 仓库实现。
#[derive(Clone)]
pub struct Repository {
    root: Arc<PathBuf>,
    upstreams: Arc<Vec<String>>,
    client: Arc<Client>,
    cache: CacheLimits,
    budget: Arc<CacheBudget>,
}

impl Repository {
    pub fn new(
        root: PathBuf,
        upstreams: Vec<String>,
        client: Arc<Client>,
        cache: CacheLimits,
    ) -> Self {
        let upstreams = upstreams
            .into_iter()
            .map(|url| url.trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .collect();
        Self {
            root: Arc::new(root),
            upstreams: Arc::new(upstreams),
            client,
            budget: Arc::new(CacheBudget::new(cache.max_cache_bytes)),
            cache,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// 解析一个请求路径，返回 200 响应或具体错误。
    pub fn resolve(&self, target: &str) -> ResolveResult<Response> {
        let relative = target
            .split_once('?')
            .map_or(target, |(path, _)| path)
            .strip_prefix('/')
            .ok_or(ResolveError::BadPath)?;
        let components = decode_path(relative).ok_or(ResolveError::BadPath)?;

        match self.resolve_file(&components) {
            Ok(response) => return Ok(response),
            Err(ResolveError::NotFound) => {}
            Err(error) => return Err(error),
        }

        if let Some(response) = self.resolve_generated_checksum(&components)? {
            return Ok(response);
        }

        match self.fetch_from_upstreams(&components) {
            Ok(response) => Ok(response),
            // maven-metadata.xml 可以由本地目录结构生成
            Err(ResolveError::NotFound)
                if components.last().map(String::as_str) == Some("maven-metadata.xml") =>
            {
                self.generate_metadata(&components)
            }
            Err(error) => Err(error),
        }
    }

    /// 为缺失的 `.sha1` 请求实时计算校验和。
    fn resolve_generated_checksum(&self, components: &[String]) -> ResolveResult<Option<Response>> {
        let Some(checksum_name) = components
            .last()
            .and_then(|name| name.strip_suffix(".sha1"))
        else {
            return Ok(None);
        };
        if checksum_name.is_empty() {
            return Ok(None);
        }
        let mut artifact_components = components.to_vec();
        let Some(last) = artifact_components.last_mut() else {
            return Err(ResolveError::BadPath);
        };
        *last = checksum_name.to_string();

        let artifact_path = match self.safe_path(&artifact_components) {
            Ok(path) => path,
            Err(ResolveError::NotFound) => {
                // maven-metadata.xml 可能由本地目录生成，没有实体文件；它同样
                // 会有 .sha1 请求，对生成结果做哈希即可。
                if artifact_components.last().map(String::as_str) == Some("maven-metadata.xml") {
                    let response = match self.generate_metadata(&artifact_components) {
                        Ok(response) => response,
                        Err(ResolveError::NotFound) => return Ok(None),
                        Err(error) => return Err(error),
                    };
                    let checksum = http::sha1_body(response.body)
                        .map_err(|error| ResolveError::internal("计算 SHA-1 校验值失败", error))?;
                    return Ok(Some(Response::checksum(checksum)));
                }
                return Ok(None);
            }
            Err(error) => return Err(error),
        };

        let mut file = match File::open(&artifact_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ResolveError::internal("打开待校验文件失败", error)),
        };
        let checksum = checksum::sha1_hex_reader(&mut file)
            .map_err(|error| ResolveError::internal("计算 SHA-1 校验值失败", error))?;
        Ok(Some(Response::checksum(checksum)))
    }

    /// 本地没有时按顺序尝试上游，并把构件缓存到本地仓库。
    fn fetch_from_upstreams(&self, components: &[String]) -> ResolveResult<Response> {
        if components.is_empty() {
            return Err(ResolveError::BadPath);
        }
        let relative_url = components
            .iter()
            .map(|component| url_encode(component))
            .collect::<Vec<_>>()
            .join("/");

        let mut last_error = None;
        let mut upstream_response = None;
        for upstream in self.upstreams.iter() {
            let url = format!("{upstream}/{relative_url}");
            match self.client.get(&url).send() {
                Ok(candidate) if candidate.status().as_u16() == 404 => continue,
                Ok(candidate) if candidate.status().is_success() => {
                    upstream_response = Some(candidate);
                    break;
                }
                Ok(candidate) => last_error = Some(format!("上游返回 {}", candidate.status())),
                Err(error) => last_error = Some(error.to_string()),
            }
        }

        let mut response = match upstream_response {
            Some(response) => response,
            None if last_error.is_none() => return Err(ResolveError::NotFound),
            None => {
                return Err(ResolveError::UpstreamUnavailable(
                    last_error.unwrap_or_else(|| "未配置上游仓库".to_string()),
                ));
            }
        };

        let file_name = components.last().ok_or(ResolveError::BadPath)?;
        let parent = self.cache_parent(&components[..components.len() - 1])?;
        let cache_path = parent.join(file_name);
        let declared = response.content_length();

        // 单文件超限：只转发不落盘
        if let Some(length) = declared
            && length > self.cache.max_file_bytes
        {
            return Ok(Response::upstream(response, length, &cache_path));
        }

        match declared {
            Some(length) => {
                if !self.budget.try_reserve(self.root.as_path(), length) {
                    // 总容量不足：只转发不落盘
                    return Ok(Response::upstream(response, length, &cache_path));
                }
                match self.store(&mut response, &cache_path, self.cache.max_file_bytes) {
                    Ok(written) if written > length => {
                        if !self
                            .budget
                            .try_reserve(self.root.as_path(), written - length)
                        {
                            self.budget.release(length);
                            let _ = fs::remove_file(&cache_path);
                            return Err(ResolveError::UpstreamUnavailable(
                                "缓存空间不足".to_string(),
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.budget.release(length);
                        return Err(error.into_resolve(self.cache.max_file_bytes));
                    }
                }
            }
            None => match self.store(&mut response, &cache_path, self.cache.max_file_bytes) {
                Ok(written) => {
                    if !self.budget.try_reserve(self.root.as_path(), written) {
                        let _ = fs::remove_file(&cache_path);
                        return Err(ResolveError::UpstreamUnavailable(
                            "缓存空间不足".to_string(),
                        ));
                    }
                }
                Err(error) => return Err(error.into_resolve(self.cache.max_file_bytes)),
            },
        }

        self.resolve_file(components)
    }

    /// 把上游响应写入缓存目录（临时文件 + 原子重命名），限制单文件大小。
    fn store(
        &self,
        response: &mut reqwest::blocking::Response,
        cache_path: &Path,
        limit: u64,
    ) -> Result<u64, CacheError> {
        let parent = cache_path
            .parent()
            .ok_or_else(|| CacheError::Io(io::Error::other("缓存路径缺少父目录")))?;
        fs::create_dir_all(parent).map_err(CacheError::Io)?;
        let temporary = cache_path.with_file_name(format!(
            ".{}.part-{}",
            cache_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("artifact"),
            unique_suffix()
        ));

        let written = (|| -> Result<u64, CacheError> {
            let mut file = File::create(&temporary).map_err(CacheError::Io)?;
            let written = copy_limited(response, &mut file, limit)?;
            file.flush().map_err(CacheError::Io)?;
            Ok(written)
        })();
        let written = match written {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        fs::rename(&temporary, cache_path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            CacheError::Io(error)
        })?;
        Ok(written)
    }

    /// 计算缓存目录并保证不越出仓库根目录。
    fn cache_parent(&self, components: &[String]) -> ResolveResult<PathBuf> {
        let parent = components
            .iter()
            .fold(self.root.as_ref().clone(), |path, component| {
                path.join(component)
            });
        fs::create_dir_all(&parent)
            .map_err(|error| ResolveError::internal("无法创建 Maven 缓存目录", error))?;
        let canonical = fs::canonicalize(&parent)
            .map_err(|error| ResolveError::internal("解析 Maven 缓存目录失败", error))?;
        if !canonical.starts_with(self.root.as_path()) || !canonical.is_dir() {
            return Err(ResolveError::BadPath);
        }
        Ok(canonical)
    }

    pub(super) fn resolve_file(&self, components: &[String]) -> ResolveResult<Response> {
        let path = self.safe_path(components)?;
        let file = File::open(&path).map_err(|error| map_open_error(error, "打开仓库文件失败"))?;
        let metadata = file
            .metadata()
            .map_err(|error| ResolveError::internal("读取仓库文件信息失败", error))?;
        if !metadata.is_file() || http::ignored_file(&path) {
            return Err(ResolveError::NotFound);
        }
        Ok(Response::file(&path, file, metadata.len()))
    }

    /// 把请求组件解析为仓库内的绝对路径，并做越界检查。
    pub(super) fn safe_path(&self, components: &[String]) -> ResolveResult<PathBuf> {
        if components.is_empty() || components.iter().any(|component| component.is_empty()) {
            return Err(ResolveError::BadPath);
        }
        let path = components
            .iter()
            .fold(self.root.as_ref().clone(), |path, component| {
                path.join(component)
            });
        let canonical =
            fs::canonicalize(&path).map_err(|error| map_open_error(error, "解析仓库路径失败"))?;
        if !canonical.starts_with(self.root.as_path()) {
            return Err(ResolveError::BadPath);
        }
        Ok(canonical)
    }
}

fn map_open_error(error: io::Error, context: &str) -> ResolveError {
    if error.kind() == io::ErrorKind::NotFound {
        ResolveError::NotFound
    } else {
        ResolveError::internal(context, error)
    }
}

#[derive(Debug)]
enum CacheError {
    TooLarge,
    Io(io::Error),
}

impl CacheError {
    fn into_resolve(self, max_file_bytes: u64) -> ResolveError {
        match self {
            Self::TooLarge => ResolveError::UpstreamUnavailable(format!(
                "上游构件超过单文件缓存上限 {max_file_bytes} 字节"
            )),
            Self::Io(error) => ResolveError::internal("缓存 Maven 构件失败", error),
        }
    }
}

/// 复制数据并限制最大字节数，超限返回 [`CacheError::TooLarge`]。
fn copy_limited<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    limit: u64,
) -> Result<u64, CacheError> {
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut written = 0_u64;
    loop {
        let read = reader.read(&mut buffer).map_err(CacheError::Io)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > limit {
            return Err(CacheError::TooLarge);
        }
        writer.write_all(&buffer[..read]).map_err(CacheError::Io)?;
    }
    Ok(written)
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
    fn encodes_url_segments() {
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(url_encode("plain-1.0.jar"), "plain-1.0.jar");
    }

    #[test]
    fn copy_limited_enforces_limit() {
        let mut input = io::Cursor::new(vec![0_u8; 100]);
        let mut output = Vec::new();
        assert_eq!(copy_limited(&mut input, &mut output, 100).unwrap(), 100);

        let mut input = io::Cursor::new(vec![0_u8; 100]);
        let mut output = Vec::new();
        assert!(matches!(
            copy_limited(&mut input, &mut output, 99),
            Err(CacheError::TooLarge)
        ));
    }

    #[test]
    fn cache_budget_respects_total_limit() {
        let root = std::env::temp_dir().join(format!(
            "maven-util-budget-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&root).unwrap();
        let budget = CacheBudget::new(100);
        // 预算首次使用时会统计仓库现有占用（空目录为 0）
        assert!(budget.try_reserve(&root, 60));
        assert!(!budget.try_reserve(&root, 50));
        budget.release(60);
        assert!(budget.try_reserve(&root, 50));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cache_budget_accounts_for_existing_files() {
        let root = std::env::temp_dir().join(format!(
            "maven-util-budget-used-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.jar"), vec![0_u8; 40]).unwrap();
        let budget = CacheBudget::new(100);
        assert!(!budget.try_reserve(&root, 70));
        assert!(budget.try_reserve(&root, 50));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_error_display_is_stable() {
        assert_eq!(ResolveError::NotFound.to_string(), "文件不存在");
        assert_eq!(ResolveError::BadPath.to_string(), "非法仓库路径");
        assert!(
            ResolveError::UpstreamUnavailable("boom".into())
                .to_string()
                .contains("boom")
        );
    }
}
