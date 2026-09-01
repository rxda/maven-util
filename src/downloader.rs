//! 通过内置 Maven mirror 执行一次完整构建，下载项目需要的依赖。

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::server::{self, ServerArgs, ServerHandle};

#[derive(Debug, Clone)]
pub struct DownloadArgs {
    pub workdir: PathBuf,
    pub download_repo: PathBuf,
    pub server_repo: PathBuf,
    pub upstreams: Vec<String>,
    pub maven: Option<PathBuf>,
    pub maven_args: Vec<String>,
    pub dry_run: bool,
    pub client: Arc<Client>,
}

pub fn default_server_repo() -> PathBuf {
    #[cfg(windows)]
    let home = env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = env::var_os("HOME");

    home.map(|path| PathBuf::from(path).join(".m2/repository"))
        .unwrap_or_else(|| PathBuf::from(".m2/repository"))
}

pub fn run_download(args: DownloadArgs) -> Result<()> {
    let workdir = fs::canonicalize(&args.workdir)
        .with_context(|| format!("Maven 运行目录不存在: {}", args.workdir.display()))?;
    if !workdir.is_dir() {
        return Err(anyhow!("Maven 运行路径不是目录: {}", workdir.display()));
    }

    let download_repo = absolute_path(&args.download_repo, &workdir);
    fs::create_dir_all(&download_repo)
        .with_context(|| format!("无法创建依赖下载目录: {}", download_repo.display()))?;
    if args.upstreams.is_empty() {
        return Err(anyhow!("至少需要一个 --upstream 上游仓库"));
    }

    let server_repo = absolute_path(&args.server_repo, &workdir);
    fs::create_dir_all(&server_repo)
        .with_context(|| format!("无法创建 Maven mirror 仓库: {}", server_repo.display()))?;
    let server = server::start_server(ServerArgs {
        root: server_repo,
        host: "127.0.0.1".into(),
        port: 0,
        upstreams: args.upstreams.clone(),
        client: args.client,
    })?;
    let settings = TemporarySettings::create(&server)?;

    let maven = resolve_maven(args.maven, &workdir);
    let mut command = Command::new(&maven);
    command
        .current_dir(&workdir)
        .arg("--settings")
        .arg(maven_compatible_path(settings.path()))
        .arg("clean")
        .arg("package")
        .arg(format!(
            "-Dmaven.repo.local={}",
            maven_compatible_path(&download_repo).display()
        ))
        .args(&args.maven_args);

    if args.dry_run {
        println!("工作目录: {}", workdir.display());
        println!("依赖下载目录: {}", download_repo.display());
        println!("Mirror 仓库: {}", args.server_repo.display());
        println!("Mirror URL: {}", server.base_url());
        println!("上游仓库: {}", args.upstreams.join(", "));
        println!("命令: {:?}", command);
        return Ok(());
    }

    println!("Maven mirror 已启动: {}", server.base_url());
    println!("运行 Maven: {}", workdir.display());
    let status = command
        .status()
        .with_context(|| format!("无法执行 Maven 命令: {}", maven.display()))?;
    if !status.success() {
        return Err(anyhow!("Maven 构建失败: {status}"));
    }
    println!("Maven 构建完成，依赖下载目录: {}", download_repo.display());
    Ok(())
}

fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// 将 Rust 在 Windows 上 canonicalize 后可能产生的扩展路径前缀转换为
/// Java/Maven 能识别的普通 Windows 路径。
///
/// `std::fs::canonicalize` 在 Windows 上可能返回 `\\?\C:\...`，但 Java
/// 的 WindowsPathParser 会把其中的 `?` 当作非法字符。UNC 路径也需要从
/// `\\?\UNC\server\share` 转回 `\\server\share`。
fn maven_compatible_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn resolve_maven(configured: Option<PathBuf>, workdir: &Path) -> PathBuf {
    if let Some(maven) = configured {
        if maven.is_absolute()
            || maven
                .parent()
                .is_none_or(|parent| parent.as_os_str().is_empty())
        {
            return maven;
        }
        let relative = workdir.join(&maven);
        return if relative.is_file() { relative } else { maven };
    }

    #[cfg(windows)]
    let wrapper = workdir.join("mvnw.cmd");
    #[cfg(not(windows))]
    let wrapper = workdir.join("mvnw");
    if wrapper.is_file() {
        return wrapper;
    }

    #[cfg(windows)]
    {
        PathBuf::from("mvn.cmd")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("mvn")
    }
}

struct TemporarySettings {
    path: PathBuf,
}

impl TemporarySettings {
    fn create(server: &ServerHandle) -> Result<Self> {
        let path = env::temp_dir().join(format!(
            "maven-uploader-settings-{}-{}.xml",
            std::process::id(),
            unique_suffix()
        ));
        let settings = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<settings xmlns=\"http://maven.apache.org/SETTINGS/1.0.0\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://maven.apache.org/SETTINGS/1.0.0 https://maven.apache.org/xsd/settings-1.0.0.xsd\">\n  <mirrors>\n    <mirror>\n      <id>maven-uploader</id>\n      <name>maven-uploader local mirror</name>\n      <url>{}</url>\n      <mirrorOf>*</mirrorOf>\n    </mirror>\n  </mirrors>\n</settings>\n",
            xml_escape(&server.base_url())
        );
        fs::write(&path, settings).context("无法创建临时 Maven settings.xml")?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporarySettings {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
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
    fn resolves_relative_download_repo_against_workdir() {
        assert_eq!(
            absolute_path(Path::new("repo"), Path::new("/tmp/project")),
            PathBuf::from("/tmp/project/repo")
        );
        assert_eq!(
            absolute_path(Path::new("/tmp/repo"), Path::new("/tmp/project")),
            PathBuf::from("/tmp/repo")
        );
    }

    #[test]
    fn uses_explicit_maven_path_when_given() {
        assert_eq!(
            resolve_maven(Some(PathBuf::from("maven")), Path::new("/tmp/project")),
            PathBuf::from("maven")
        );
    }

    #[test]
    fn escapes_settings_values() {
        assert_eq!(xml_escape("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
    }

    #[cfg(windows)]
    #[test]
    fn removes_windows_extended_path_prefix_for_maven() {
        assert_eq!(
            maven_compatible_path(Path::new(r"\\?\C:\code\maven-repo")),
            PathBuf::from(r"C:\code\maven-repo")
        );
        assert_eq!(
            maven_compatible_path(Path::new(r"\\?\UNC\server\share\maven-repo")),
            PathBuf::from(r"\\server\share\maven-repo")
        );
    }
}
