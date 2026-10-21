//! 配置文件：`~/.maven-util/config.toml`。
//!
//! 首次运行时自动创建配置目录和配置文件；之后每次运行都会读取该文件，
//! 命令行参数和环境变量优先于配置文件。

use crate::tls::HttpSettings;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// 用户配置目录名（位于用户主目录下）。
pub const CONFIG_DIR_NAME: &str = ".maven-util";
/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.toml";
/// 旧版项目级配置文件名，仍被支持（优先于全局配置）。
pub const LEGACY_CONFIG_NAMES: &[&str] = &["maven-uploader.toml", "maven-uploader.example.toml"];

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Config {
    pub upload: Option<UploadConfig>,
    pub serve: Option<ServeConfig>,
    pub download: Option<DownloadConfig>,
    pub clean: Option<CleanConfig>,
    pub http: Option<HttpConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct UploadConfig {
    /// Release 仓库地址。
    pub url: Option<String>,
    /// Snapshot 仓库地址（可选，默认复用 `url`）。
    pub snapshot_url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// 扫描根目录（包含 org/ com/ 的那一层）。
    pub dir: Option<String>,
    /// 强制重新上传。
    pub force: Option<bool>,
    /// 排除关键字（匹配 groupId 或 artifactId）。
    pub exclude: Option<Vec<String>>,
    /// 跳过大于此大小的 jar/war（MB）。
    pub max_size_mb: Option<u64>,
    /// 上传状态数据库路径。
    pub db_path: Option<String>,
    /// 上传并发数（同时进行的 HTTP 请求数），默认等于 CPU 核数。
    pub concurrency: Option<usize>,
    /// 瞬时错误（网络错误 / 5xx / 429）的重试次数。
    pub retries: Option<u32>,
    /// 跳过 HEAD 远端存在性检查（状态库检查仍然生效）。
    pub skip_head_check: Option<bool>,
    /// 上传路径："put"（默认，通用）或 "components"（Nexus 3，整构件一次提交）。
    pub mode: Option<String>,
    /// Components 模式下的仓库名；不填则从 url 的 `/repository/<名>` 推断。
    pub repository: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ServeConfig {
    pub dir: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub upstreams: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct DownloadConfig {
    pub workdir: Option<String>,
    pub download_repo: Option<String>,
    pub server_repo: Option<String>,
    pub upstreams: Option<Vec<String>>,
    pub maven: Option<String>,
    pub maven_args: Option<Vec<String>>,
    pub dry_run: Option<bool>,
    pub skip_tests: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct CleanConfig {
    pub repo: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct HttpConfig {
    pub connect_timeout_secs: Option<u64>,
    pub request_timeout_secs: Option<u64>,
    pub upload_timeout_secs: Option<u64>,
    /// 单个上游构件缓存上限（字节）。
    pub max_file_bytes: Option<u64>,
    /// 缓存总容量上限（字节）。
    pub max_cache_bytes: Option<u64>,
}

impl HttpConfig {
    /// 解析为运行时设置，未配置的项使用内置默认值。
    pub fn resolve(&self) -> HttpSettings {
        let defaults = HttpSettings::default();
        HttpSettings {
            connect_timeout: self
                .connect_timeout_secs
                .map_or(defaults.connect_timeout, Duration::from_secs),
            request_timeout: self
                .request_timeout_secs
                .map_or(defaults.request_timeout, Duration::from_secs),
            upload_timeout: self
                .upload_timeout_secs
                .map_or(defaults.upload_timeout, Duration::from_secs),
            max_cache_file_bytes: self.max_file_bytes.unwrap_or(defaults.max_cache_file_bytes),
            max_cache_bytes: self.max_cache_bytes.unwrap_or(defaults.max_cache_bytes),
        }
    }
}

impl Config {
    /// 用项目级配置逐字段覆盖基础配置；未设置的字段保留基础配置的值。
    pub fn overlay(self, over: Config) -> Config {
        Config {
            upload: merge_section(self.upload, over.upload, |base, over| UploadConfig {
                url: over.url.or(base.url),
                snapshot_url: over.snapshot_url.or(base.snapshot_url),
                username: over.username.or(base.username),
                password: over.password.or(base.password),
                dir: over.dir.or(base.dir),
                force: over.force.or(base.force),
                exclude: over.exclude.or(base.exclude),
                max_size_mb: over.max_size_mb.or(base.max_size_mb),
                db_path: over.db_path.or(base.db_path),
                concurrency: over.concurrency.or(base.concurrency),
                retries: over.retries.or(base.retries),
                skip_head_check: over.skip_head_check.or(base.skip_head_check),
                mode: over.mode.or(base.mode),
                repository: over.repository.or(base.repository),
            }),
            serve: merge_section(self.serve, over.serve, |base, over| ServeConfig {
                dir: over.dir.or(base.dir),
                host: over.host.or(base.host),
                port: over.port.or(base.port),
                upstreams: over.upstreams.or(base.upstreams),
            }),
            download: merge_section(self.download, over.download, |base, over| DownloadConfig {
                workdir: over.workdir.or(base.workdir),
                download_repo: over.download_repo.or(base.download_repo),
                server_repo: over.server_repo.or(base.server_repo),
                upstreams: over.upstreams.or(base.upstreams),
                maven: over.maven.or(base.maven),
                maven_args: over.maven_args.or(base.maven_args),
                dry_run: over.dry_run.or(base.dry_run),
                skip_tests: over.skip_tests.or(base.skip_tests),
            }),
            clean: merge_section(self.clean, over.clean, |base, over| CleanConfig {
                repo: over.repo.or(base.repo),
            }),
            http: merge_section(self.http, over.http, |base, over| HttpConfig {
                connect_timeout_secs: over.connect_timeout_secs.or(base.connect_timeout_secs),
                request_timeout_secs: over.request_timeout_secs.or(base.request_timeout_secs),
                upload_timeout_secs: over.upload_timeout_secs.or(base.upload_timeout_secs),
                max_file_bytes: over.max_file_bytes.or(base.max_file_bytes),
                max_cache_bytes: over.max_cache_bytes.or(base.max_cache_bytes),
            }),
        }
    }
}

fn merge_section<T>(base: Option<T>, over: Option<T>, merge: impl FnOnce(T, T) -> T) -> Option<T> {
    match (base, over) {
        (Some(base), Some(over)) => Some(merge(base, over)),
        (base, over) => over.or(base),
    }
}

/// 把配置片段合并为运行时 HTTP 设置。
pub fn http_settings(config: &Config) -> HttpSettings {
    config
        .http
        .as_ref()
        .map_or_else(HttpSettings::default, HttpConfig::resolve)
}

/// 用户主目录。
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let variable = "USERPROFILE";
    #[cfg(not(windows))]
    let variable = "HOME";
    env::var_os(variable).map(PathBuf::from)
}

/// `~/.maven-util` 目录。
pub fn config_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(CONFIG_DIR_NAME))
}

/// `~/.maven-util/config.toml` 路径。
pub fn default_config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

/// 默认上传状态数据库路径：`~/.maven-util/uploads.db`。
pub fn default_db_path() -> PathBuf {
    config_dir().map_or_else(|| PathBuf::from("uploads.db"), |dir| dir.join("uploads.db"))
}

/// 默认本地 Maven 仓库：`~/.m2/repository`。
pub fn default_server_repo() -> PathBuf {
    home_dir().map_or_else(
        || PathBuf::from(".m2/repository"),
        |home| home.join(".m2").join("repository"),
    )
}

/// 首次运行时创建 `~/.maven-util/config.toml`。
///
/// 返回新创建的配置文件路径；目录或文件已存在、以及创建失败时返回 `None`
/// （创建失败只打印警告，不阻断运行，例如 HOME 只读的场景）。
pub fn ensure_default_config() -> Option<PathBuf> {
    let path = default_config_path()?;
    if path.exists() {
        return None;
    }
    let directory = path.parent()?;
    if let Err(error) = fs::create_dir_all(directory) {
        eprintln!("无法创建配置目录 {}: {error}", directory.display());
        return None;
    }
    if let Err(error) = fs::write(&path, DEFAULT_CONFIG_TEMPLATE) {
        eprintln!("无法写入默认配置 {}: {error}", path.display());
        return None;
    }
    restrict_permissions(&path);
    println!("已生成默认配置文件: {}（可按需修改后重跑）", path.display());
    Some(path)
}

/// 在 Unix 上把配置文件权限收敛为 0600，因为里面可能包含仓库密码。
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        eprintln!("无法设置配置文件权限: {error}");
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// 读取配置。
///
/// 查找顺序：
/// 1. `-c/--config` 或环境变量 `MAVEN_UTIL_CONFIG`（必须存在，否则报错）
/// 2. `~/.maven-util/config.toml` 作为基础配置
/// 3. 当前目录的 `maven-uploader.toml` 逐字段覆盖基础配置
/// 4. 全部缺失时使用内置默认值
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let Some(path) = explicit else {
        let global = default_config_path()
            .filter(|path| path.is_file())
            .and_then(|path| read(&path).ok());
        let local = discover_local()
            .and_then(|path| read(&path).ok())
            .unwrap_or_default();
        return Ok(match global {
            Some(global) => global.overlay(local),
            None => local,
        });
    };
    read(path)
}

/// 当前目录下的项目级配置文件（覆盖全局配置）。
pub fn discover_local() -> Option<PathBuf> {
    LEGACY_CONFIG_NAMES
        .iter()
        .map(|name| PathBuf::from(".").join(name))
        .find(|candidate| candidate.is_file())
}

fn read(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("无法读取配置文件: {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("配置文件格式错误: {}", path.display()))
}

/// 展开路径中的 `~`。
pub fn path(value: String) -> PathBuf {
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(relative) = value.strip_prefix("~/") {
        return home_dir()
            .map(|home| home.join(relative))
            .unwrap_or_else(|| PathBuf::from(value));
    }
    PathBuf::from(value)
}

/// 自动写入用户目录的默认配置模板。
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# maven-util 全局配置
# 位置: ~/.maven-util/config.toml
#
# 优先级：命令行参数 > 环境变量 > 本文件 > 内置默认值
# 当前目录下若存在 maven-uploader.toml，会优先于本文件（便于按项目覆盖）。
#
# 最少只需填写 [upload] 中的 url / username / password，
# 之后直接执行 `maven-util` 即可上传。

[upload]
# Release 仓库地址，例如 https://nexus.example.com/repository/maven-releases
# url = "https://nexus.example.com/repository/maven-releases"
# Snapshot 仓库地址（不填则复用 url）
# snapshot_url = "https://nexus.example.com/repository/maven-snapshots"
# username = "admin"
# password = "your-password"

# 扫描根目录（包含 org/ com/ 的那一层），相对路径相对于执行命令时的当前目录
dir = "."
# 强制重新上传（忽略本地状态库和远程 HEAD 检查）
force = false
# 排除 groupId 或 artifactId 关键字
exclude = []
# 跳过大于此大小（MB）的 jar/war
max_size_mb = 100
# 上传状态数据库
db_path = "~/.maven-util/uploads.db"
# 上传并发数（同时进行的 HTTP 请求数），默认等于 CPU 核数。
# 上传是纯 I/O 操作：远端延迟越高，并发收益越大（实测 80ms 延迟下
# 4 并发约 16s、32 并发约 2s 的线性差距），内网可保持默认。
concurrency = 8
# 网络错误 / 5xx / 429 的重试次数（PUT 幂等，重试安全）
retries = 2
# 跳过 HEAD 存在性检查（状态库仍生效）。仅当远端尚无这些文件时才更快；
# 远端已有内容时会全量重传，并会覆盖上传本地已改动的构件
skip_head_check = false

[download]
workdir = "."
download_repo = "./maven-repo"
server_repo = "~/.m2/repository"
upstreams = ["https://repo.maven.apache.org/maven2"]
# Maven 可执行文件；留空时优先使用项目里的 mvnw/mvnw.cmd
# maven = "mvn"
maven_args = []
# 仅打印命令，不产生任何副作用
dry_run = false
skip_tests = false

[serve]
dir = "~/.m2/repository"
host = "127.0.0.1"
port = 8080
upstreams = ["https://repo.maven.apache.org/maven2"]

[clean]
repo = "~/.m2/repository"

[http]
# 连接超时（秒）
connect_timeout_secs = 10
# 普通请求（HEAD/上游回源）总超时（秒）
request_timeout_secs = 60
# 上传总超时（秒）
upload_timeout_secs = 600
# 单个上游构件缓存上限（字节），超出时只转发不落盘
max_file_bytes = 268435456
# 缓存总容量上限（字节），超出时只转发不落盘
max_cache_bytes = 10737418240
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upload_and_http_config() {
        let config: Config = toml::from_str(
            r#"
                [upload]
                url = "https://nexus.example.com/repository/releases"
                username = "admin"
                max_size_mb = 30
                exclude = ["internal"]
                [http]
                connect_timeout_secs = 3
                max_file_bytes = 1024
            "#,
        )
        .unwrap();
        let upload = config.upload.unwrap();
        assert_eq!(
            upload.url.as_deref(),
            Some("https://nexus.example.com/repository/releases")
        );
        assert_eq!(upload.username.as_deref(), Some("admin"));
        assert_eq!(upload.max_size_mb, Some(30));
        assert_eq!(upload.exclude.unwrap(), vec!["internal"]);

        let http = config.http.unwrap().resolve();
        assert_eq!(http.connect_timeout, Duration::from_secs(3));
        assert_eq!(http.request_timeout, Duration::from_secs(60));
        assert_eq!(http.max_cache_file_bytes, 1024);
        assert_eq!(http.max_cache_bytes, 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn default_template_is_valid_toml() {
        let parsed: Config =
            toml::from_str(DEFAULT_CONFIG_TEMPLATE).expect("内置配置模板必须是合法 TOML");
        assert!(parsed.upload.is_some());
        assert!(parsed.download.is_some());
        assert!(parsed.serve.is_some());
        assert!(parsed.http.is_some());
    }

    #[test]
    fn project_config_overrides_global_field_by_field() {
        let global: Config = toml::from_str(
            r#"
                [upload]
                url = "http://global"
                username = "admin"
                [serve]
                port = 8080
                host = "127.0.0.1"
            "#,
        )
        .unwrap();
        let local: Config = toml::from_str(
            r#"
                [serve]
                port = 9090
            "#,
        )
        .unwrap();

        let merged = global.overlay(local);
        let upload = merged.upload.unwrap();
        assert_eq!(upload.url.as_deref(), Some("http://global"));
        assert_eq!(upload.username.as_deref(), Some("admin"));
        let serve = merged.serve.unwrap();
        assert_eq!(serve.port, Some(9090), "项目级端口应覆盖全局");
        assert_eq!(
            serve.host.as_deref(),
            Some("127.0.0.1"),
            "未覆盖项保留全局值"
        );
    }

    #[test]
    fn empty_overlay_keeps_base_config() {
        let global: Config = toml::from_str("[upload]\nurl = \"http://global\"\n").unwrap();
        let merged = global.clone().overlay(Config::default());
        assert_eq!(merged.upload.unwrap().url.as_deref(), Some("http://global"));
    }

    #[test]
    fn expands_home_path() {
        let home = home_dir().unwrap();
        assert_eq!(path("~/repo".into()), home.join("repo"));
        assert_eq!(path("~".into()), home);
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("this-config-does-not-exist.toml");
        let error = load(Some(&missing)).unwrap_err();
        assert!(error.to_string().contains("无法读取配置文件"));
    }
}
