//! 命令行定义与「命令行 > 环境变量 > 配置文件 > 默认值」的合并逻辑。

use crate::cleanup::CleanupArgs;
use crate::config::{self, Config};
use crate::downloader::DownloadArgs;
use crate::server::{CacheLimits, ServerArgs};
use crate::upload::UploadOptions;
use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

/// 内置默认上游仓库。
pub const DEFAULT_UPSTREAM: &str = "https://repo.maven.apache.org/maven2";

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "高性能 Maven 仓库迁移工具 (Pure Rust 版)")]
pub struct Args {
    /// 上传参数（不带子命令时即为上传模式）
    #[command(flatten)]
    pub upload: UploadArgs,

    /// TOML 配置文件；未指定时自动使用 ~/.maven-util/config.toml
    #[arg(short = 'c', long, global = true, env = "MAVEN_UTIL_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 上传本地 Maven 仓库（与不带子命令时等价，参数来自配置文件）
    Upload(UploadCli),
    /// 将本地 Maven 仓库作为只读 Maven 仓库提供服务
    Serve(ServeCli),
    /// 启动临时 Maven mirror 并执行 clean package 下载依赖
    Download(DownloadCli),
    /// 清理 Maven 仓库中的远程标记和失败更新文件
    Clean(CleanCli),
    /// 初始化：复制程序到 ~/.maven-util/bin 并把该目录加入 PATH
    Init(InitCli),
}

#[derive(ClapArgs, Debug, Clone, Default)]
pub struct UploadArgs {
    /// Release 仓库 URL
    #[arg(short = 'U', long, env = "NEXUS_URL")]
    pub url: Option<String>,

    /// Snapshot 仓库 URL (可选)
    #[arg(short = 'S', long, env = "NEXUS_SNAPSHOT_URL")]
    pub snapshot_url: Option<String>,

    /// 用户名
    #[arg(short = 'u', long, env = "NEXUS_USERNAME")]
    pub username: Option<String>,

    /// 密码
    #[arg(short = 'p', long, env = "NEXUS_PASSWORD")]
    pub password: Option<String>,

    /// 扫描根目录 (包含 org/, com/ 的那一层)
    #[arg(short = 'd', long, env = "NEXUS_DIR")]
    pub dir: Option<String>,

    /// 是否强制重新上传
    #[arg(short = 'f', long, env = "NEXUS_FORCE")]
    pub force: bool,

    /// 排除关键字 (逗号分隔)
    #[arg(short = 'E', long, env = "NEXUS_EXCLUDE", value_delimiter = ',')]
    pub exclude: Vec<String>,

    /// 跳过大于此大小的二进制文件 (MB)
    #[arg(long, env = "NEXUS_MAX_MB")]
    pub max_size: Option<u64>,

    /// 状态数据库路径 (redb 格式)
    #[arg(long, env = "MAVEN_UTIL_DB_PATH")]
    pub db_path: Option<PathBuf>,

    /// 上传并发数（同时进行的 HTTP 请求数，默认 = CPU 核数）
    #[arg(long, env = "MAVEN_UTIL_CONCURRENCY")]
    pub concurrency: Option<usize>,

    /// 网络错误 / 5xx / 429 的重试次数
    #[arg(long, env = "MAVEN_UTIL_RETRIES")]
    pub retries: Option<u32>,

    /// 跳过 HEAD 远端存在性检查（状态库仍然生效，可减少请求数）
    #[arg(long, env = "MAVEN_UTIL_SKIP_HEAD_CHECK")]
    pub skip_head_check: bool,
}

/// `upload` 子命令：参数与不带子命令时的顶层上传参数一致。
#[derive(ClapArgs, Debug, Clone)]
pub struct UploadCli {
    #[command(flatten)]
    pub upload: UploadArgs,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct ServeCli {
    /// 本地 Maven 仓库目录，默认使用 ~/.m2/repository
    #[arg(short = 'd', long, env = "MAVEN_REPO_DIR")]
    pub dir: Option<PathBuf>,
    /// 监听地址
    #[arg(long)]
    pub host: Option<String>,
    /// 监听端口
    #[arg(short = 'P', long)]
    pub port: Option<u16>,
    /// 本地缺少构件时回源的仓库地址，可重复指定，按顺序尝试
    #[arg(
        long = "upstream",
        visible_alias = "central-url",
        value_delimiter = ','
    )]
    pub upstreams: Option<Vec<String>>,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct DownloadCli {
    /// Maven 项目运行目录，默认当前目录
    pub workdir: Option<PathBuf>,
    /// Maven 下载依赖保存目录，传给 -Dmaven.repo.local
    #[arg(long, short = 'o')]
    pub download_repo: Option<PathBuf>,
    /// 作为内置 Maven mirror 根目录的已有仓库，默认 ~/.m2/repository
    #[arg(long)]
    pub server_repo: Option<PathBuf>,
    /// 本地缺少构件时回源的仓库地址，可重复指定，按顺序尝试
    #[arg(
        long = "upstream",
        visible_alias = "central-url",
        value_delimiter = ','
    )]
    pub upstreams: Option<Vec<String>>,
    /// Maven 可执行文件；未指定时优先使用项目中的 mvnw/mvnw.cmd
    #[arg(long)]
    pub maven: Option<PathBuf>,
    /// 跳过测试执行（向 Maven 追加 -DskipTests）
    #[arg(short = 's', long)]
    pub skip_tests: bool,
    /// 仅打印将要执行的 Maven 命令，不产生任何副作用
    #[arg(long)]
    pub dry_run: bool,
    /// 传递给 Maven 的额外参数
    #[arg(last = true)]
    pub maven_args: Vec<String>,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct InitCli {
    /// 只生成配置文件，不复制可执行文件
    #[arg(long)]
    pub no_binary: bool,
    /// 不修改 shell 配置文件（不自动配置 PATH）
    #[arg(long)]
    pub no_path: bool,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct CleanCli {
    /// Maven 仓库目录，默认 ~/.m2/repository
    #[arg(long, short = 'd')]
    pub repo: Option<PathBuf>,
    /// 只显示将要删除的文件，不实际删除
    #[arg(long)]
    pub dry_run: bool,
}

impl UploadArgs {
    /// 子命令参数优先于顶层参数（未提供时回落到顶层）。
    pub fn merge(top: &UploadArgs, sub: Option<&UploadArgs>) -> UploadArgs {
        let Some(sub) = sub else {
            return top.clone();
        };
        UploadArgs {
            url: sub.url.clone().or_else(|| top.url.clone()),
            snapshot_url: sub
                .snapshot_url
                .clone()
                .or_else(|| top.snapshot_url.clone()),
            username: sub.username.clone().or_else(|| top.username.clone()),
            password: sub.password.clone().or_else(|| top.password.clone()),
            dir: sub.dir.clone().or_else(|| top.dir.clone()),
            force: sub.force || top.force,
            exclude: if sub.exclude.is_empty() {
                top.exclude.clone()
            } else {
                sub.exclude.clone()
            },
            max_size: sub.max_size.or(top.max_size),
            db_path: sub.db_path.clone().or_else(|| top.db_path.clone()),
            concurrency: sub.concurrency.or(top.concurrency),
            retries: sub.retries.or(top.retries),
            skip_head_check: sub.skip_head_check || top.skip_head_check,
        }
    }
}

impl Args {
    /// 是否为只读预览模式：`--dry-run` 下不做任何初始化和写操作。
    pub fn is_dry_run(&self) -> bool {
        match &self.command {
            Some(Command::Download(cli)) => cli.dry_run,
            Some(Command::Clean(cli)) => cli.dry_run,
            _ => false,
        }
    }

    /// 合并上传配置：命令行 > 环境变量 > `[upload]` 配置 > 默认值。
    pub fn resolve_upload(
        &self,
        sub: Option<&UploadArgs>,
        config: &Config,
    ) -> Result<UploadOptions> {
        let cli = UploadArgs::merge(&self.upload, sub);
        let file = config.upload.clone().unwrap_or_default();

        let required = |value: Option<String>, name: &str, hint: &str| -> Result<String> {
            value
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!(
                        "缺少{name}：请在 ~/.maven-util/config.toml 中配置 {hint}，或传命令行参数"
                    )
                })
        };

        let url = required(cli.url.or(file.url), "上传仓库地址", "[upload].url")?;
        let username = required(
            cli.username.or(file.username),
            "用户名",
            "[upload].username",
        )?;
        let password = required(cli.password.or(file.password), "密码", "[upload].password")?;

        let exclude = if cli.exclude.is_empty() {
            file.exclude.unwrap_or_default()
        } else {
            cli.exclude
        };

        Ok(UploadOptions {
            url,
            snapshot_url: non_empty(cli.snapshot_url.or(file.snapshot_url)),
            username,
            password,
            dir: config::path(cli.dir.or(file.dir).unwrap_or_else(|| ".".into())),
            force: cli.force || file.force.unwrap_or(false),
            exclude,
            max_size_mb: cli.max_size.or(file.max_size_mb).unwrap_or(100),
            db_path: cli
                .db_path
                .or_else(|| file.db_path.map(config::path))
                .unwrap_or_else(config::default_db_path),
            concurrency: cli
                .concurrency
                .or(file.concurrency)
                .unwrap_or_else(crate::upload::default_upload_concurrency),
            retries: cli.retries.or(file.retries).unwrap_or(2),
            skip_head_check: cli.skip_head_check || file.skip_head_check.unwrap_or(false),
        })
    }

    /// 合并 `serve` 配置。
    pub fn resolve_serve(
        &self,
        cli: &ServeCli,
        config: &Config,
        settings: crate::tls::HttpSettings,
    ) -> Result<ServerArgs> {
        let file = config.serve.clone().unwrap_or_default();
        Ok(ServerArgs {
            root: cli
                .dir
                .clone()
                .or_else(|| file.dir.map(config::path))
                .unwrap_or_else(config::default_server_repo),
            host: cli
                .host
                .clone()
                .or(file.host)
                .unwrap_or_else(|| "127.0.0.1".into()),
            port: cli.port.or(file.port).unwrap_or(8080),
            upstreams: upstreams(cli.upstreams.clone().or(file.upstreams)),
            cache: CacheLimits::from(&settings),
            client: Arc::new(settings.standard_client()?),
        })
    }

    /// 合并 `download` 配置。
    pub fn resolve_download(
        &self,
        cli: &DownloadCli,
        config: &Config,
        settings: crate::tls::HttpSettings,
    ) -> Result<DownloadArgs> {
        let file = config.download.clone().unwrap_or_default();
        let mut maven_args = file.maven_args.unwrap_or_default();
        maven_args.extend(cli.maven_args.clone());
        let skip_tests = cli.skip_tests || file.skip_tests.unwrap_or(false);
        if skip_tests && !maven_args.iter().any(|arg| arg == "-DskipTests") {
            maven_args.push("-DskipTests".into());
        }
        Ok(DownloadArgs {
            workdir: cli
                .workdir
                .clone()
                .or_else(|| file.workdir.map(config::path))
                .unwrap_or_else(|| PathBuf::from(".")),
            download_repo: cli
                .download_repo
                .clone()
                .or_else(|| file.download_repo.map(config::path))
                .unwrap_or_else(|| PathBuf::from("maven-repo")),
            server_repo: cli
                .server_repo
                .clone()
                .or_else(|| file.server_repo.map(config::path))
                .unwrap_or_else(config::default_server_repo),
            upstreams: upstreams(cli.upstreams.clone().or(file.upstreams)),
            maven: cli.maven.clone().or_else(|| file.maven.map(config::path)),
            maven_args,
            dry_run: cli.dry_run || file.dry_run.unwrap_or(false),
            cache: CacheLimits::from(&settings),
            client: Arc::new(settings.standard_client()?),
        })
    }

    /// 合并 `clean` 配置。
    pub fn resolve_clean(&self, cli: &CleanCli, config: &Config) -> CleanupArgs {
        let file = config.clean.clone().unwrap_or_default();
        CleanupArgs {
            repo: cli
                .repo
                .clone()
                .or_else(|| file.repo.map(config::path))
                .unwrap_or_else(config::default_server_repo),
            dry_run: cli.dry_run,
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn upstreams(configured: Option<Vec<String>>) -> Vec<String> {
    configured
        .map(|urls| {
            urls.into_iter()
                .map(|url| url.trim().to_string())
                .filter(|url| !url.is_empty())
                .collect()
        })
        .filter(|urls: &Vec<String>| !urls.is_empty())
        .unwrap_or_else(|| vec![DEFAULT_UPSTREAM.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subcommand_upload_arguments_win_over_top_level() {
        let args = Args::parse_from([
            "maven-util",
            "--url",
            "http://top",
            "--username",
            "top-user",
            "upload",
            "--url",
            "http://sub",
        ]);
        let Some(Command::Upload(cli)) = args.command else {
            panic!("expected upload subcommand");
        };
        let merged = UploadArgs::merge(&args.upload, Some(&cli.upload));
        assert_eq!(merged.url.as_deref(), Some("http://sub"));
        assert_eq!(merged.username.as_deref(), Some("top-user"));
    }

    #[test]
    fn empty_subcommand_values_fall_back_to_top_level() {
        let args = Args::parse_from(["maven-util", "--url", "http://top", "--force", "upload"]);
        let Some(Command::Upload(cli)) = args.command else {
            panic!("expected upload subcommand");
        };
        let merged = UploadArgs::merge(&args.upload, Some(&cli.upload));
        assert_eq!(merged.url.as_deref(), Some("http://top"));
        assert!(merged.force);
    }

    #[test]
    fn upload_without_credentials_points_to_config_file() {
        let args = Args::parse_from(["maven-util"]);
        let error = args.resolve_upload(None, &Config::default()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("url"), "unexpected error: {message}");
        assert!(
            message.contains(".maven-util"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn upload_resolves_from_config_file() {
        let config: Config = toml::from_str(
            r#"
                [upload]
                url = "https://nexus.example.com/repository/releases"
                username = "admin"
                password = "secret"
                dir = "/tmp/repo"
                max_size_mb = 30
            "#,
        )
        .unwrap();
        let args = Args::parse_from(["maven-util"]);
        let options = args.resolve_upload(None, &config).unwrap();
        assert_eq!(options.username, "admin");
        assert_eq!(options.password, "secret");
        assert_eq!(options.dir, PathBuf::from("/tmp/repo"));
        assert_eq!(options.max_size_mb, 30);
        assert!(!options.force);
    }

    #[test]
    fn cli_overrides_config_file() {
        let config: Config = toml::from_str(
            r#"
                [upload]
                url = "https://nexus.example.com/repository/releases"
                username = "admin"
                password = "secret"
                max_size_mb = 30
            "#,
        )
        .unwrap();
        let args = Args::parse_from(["maven-util", "--username", "cli-user", "--max-size", "5"]);
        let options = args.resolve_upload(None, &config).unwrap();
        assert_eq!(options.username, "cli-user");
        assert_eq!(options.max_size_mb, 5);
    }

    #[test]
    fn default_upstream_is_used_when_none_configured() {
        assert_eq!(
            upstreams(None),
            vec!["https://repo.maven.apache.org/maven2".to_string()]
        );
        assert_eq!(
            upstreams(Some(vec![" ".into(), "http://a".into()])),
            vec!["http://a".to_string()]
        );
    }
}
