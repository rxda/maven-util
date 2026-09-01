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

pub fn load(path: Option<&Path>) -> Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
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
}
