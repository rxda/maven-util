use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub serve: Option<ServeConfig>,
    pub download: Option<DownloadConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServeConfig {
    pub dir: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub upstreams: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadConfig {
    pub workdir: Option<String>,
    pub download_repo: Option<String>,
    pub server_repo: Option<String>,
    pub upstreams: Option<Vec<String>>,
    pub maven: Option<String>,
    pub maven_args: Option<Vec<String>>,
    pub dry_run: Option<bool>,
}

const DEFAULT_CONFIG_NAMES: &[&str] = &["maven-uploader.toml", "maven-uploader.example.toml"];

/// 读取显式指定的配置；未指定时按顺序自动发现默认配置。
///
/// 查找顺序：
/// 1. 当前目录
/// 2. 可执行文件所在目录
/// 3. 编译本程序的 maven-util 源码目录
///
/// 每个目录依次查找 `maven-uploader.toml` 和 `maven-uploader.example.toml`。
/// 一个配置都没有时返回默认值，保证不传 `-c` 也可以运行。
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let selected = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => discover_default(),
    };
    let Some(path) = selected else {
        return Ok(Config::default());
    };
    read(&path)
}

/// 自动发现默认配置，返回找到的第一个文件。
pub fn discover_default() -> Option<PathBuf> {
    let mut directories = vec![PathBuf::from(".")];

    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        directories.push(parent.to_path_buf());
    }

    directories.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    let mut checked = Vec::new();
    for directory in directories {
        for name in DEFAULT_CONFIG_NAMES {
            let candidate = directory.join(name);
            if checked.contains(&candidate) {
                continue;
            }
            checked.push(candidate.clone());
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn read(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("无法读取配置文件: {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("配置文件格式错误: {}", path.display()))
}

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

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let variable = "USERPROFILE";
    #[cfg(not(windows))]
    let variable = "HOME";
    env::var_os(variable).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_download_config() {
        let config: Config = toml::from_str(
            r#"
                [download]
                download_repo = "./repo"
                upstreams = ["https://repo.example.com/maven2"]
                maven_args = ["-DskipTests"]
            "#,
        )
        .unwrap();
        let download = config.download.unwrap();
        assert_eq!(download.download_repo.as_deref(), Some("./repo"));
        assert_eq!(download.maven_args.unwrap(), vec!["-DskipTests"]);
    }

    #[test]
    fn expands_home_path() {
        let home = home_dir().unwrap();
        assert_eq!(path("~/repo".into()), home.join("repo"));
    }

    #[test]
    fn discovers_default_config() {
        let source_default = Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_NAMES[0]);
        assert!(source_default.is_file());

        let discovered = discover_default().unwrap();
        assert!(discovered.is_file());
        assert_eq!(
            discovered.file_name(),
            Some(DEFAULT_CONFIG_NAMES[0].as_ref())
        );
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("this-config-does-not-exist.toml");
        let error = load(Some(&missing)).unwrap_err();
        assert!(error.to_string().contains("无法读取配置文件"));
    }
}
