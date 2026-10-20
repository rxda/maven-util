//! maven-util：Maven 仓库上传 / 下载 / 只读服务工具箱。
//!
//! 模块划分：
//! - [`cli`]：命令行定义与「命令行 > 环境变量 > 配置文件 > 默认值」合并
//! - [`config`]：`~/.maven-util/config.toml` 的生成与读取
//! - [`init`]：首次运行自装到 `~/.maven-util/bin` 并配置 PATH
//! - [`upload`]：本地仓库批量上传
//! - [`downloader`]：内置 mirror 执行 Maven 构建并缓存依赖
//! - [`server`]：只读 Maven 仓库 HTTP 服务
//! - [`tls`]：HTTP 客户端与超时
//! - [`checksum`]：Maven checksum 计算

pub mod checksum;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod downloader;
pub mod init;
pub mod server;
pub mod tls;
pub mod upload;

use anyhow::Result;
use clap::Parser;

/// 环境变量：设置后跳过首次运行初始化（CI / 只读环境）。
pub const SKIP_INIT_ENV: &str = "MAVEN_UTIL_SKIP_INIT";

/// 程序入口：初始化配置后分发子命令。
pub fn run() -> Result<()> {
    let args = cli::Args::parse();

    // 首次运行：生成 ~/.maven-util/config.toml、安装自身并配置 PATH。
    // --dry-run、MAVEN_UTIL_SKIP_INIT 或显式 `init` 子命令时跳过
    // （显式 init 由其自身的选项完全接管）。
    let explicit_init = matches!(args.command, Some(cli::Command::Init(_)));
    if !explicit_init && !args.is_dry_run() && std::env::var_os(SKIP_INIT_ENV).is_none() {
        init::auto_first_run();
    }

    let config = config::load(args.config.as_deref())?;
    let http = config::http_settings(&config);

    match args.command.as_ref() {
        Some(cli::Command::Upload(cli)) => {
            let options = args.resolve_upload(Some(&cli.upload), &config)?;
            upload::run_upload(options, &http)
        }
        Some(cli::Command::Serve(cli)) => {
            let server_args = args.resolve_serve(cli, &config, http)?;
            server::run_server(server_args)
        }
        Some(cli::Command::Download(cli)) => {
            let download_args = args.resolve_download(cli, &config, http)?;
            downloader::run_download(download_args)
        }
        Some(cli::Command::Clean(cli)) => {
            let cleanup_args = args.resolve_clean(cli, &config);
            cleanup::run_cleanup(cleanup_args)
        }
        Some(cli::Command::Init(cli)) => {
            let report = init::run_init(init::InitOptions {
                install_binary: !cli.no_binary,
                configure_path: !cli.no_path,
            })?;
            init::print_report(&report, "初始化完成");
            Ok(())
        }
        // 不带子命令即为上传模式，参数来自配置文件
        None => {
            let options = args.resolve_upload(None, &config)?;
            upload::run_upload(options, &http)
        }
    }
}
