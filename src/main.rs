use anyhow::{Context, Result};
use clap::Parser;
use crossbeam_channel::unbounded;
use dashmap::DashMap;
use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use rayon::iter::{ParallelBridge, ParallelIterator};
use redb::{Database, ReadableDatabase, TableDefinition};
use reqwest::blocking::Client;
use rustls::{ClientConfig, RootCertStore};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

mod cleanup;
mod config;
mod downloader;
mod server;

// 定义本地数据库表：Key 是目标 URL，Value 是文件最后修改时间 (u64)
const TABLE: TableDefinition<&str, u64> = TableDefinition::new("uploads_v1");

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "高性能 Maven 仓库迁移工具 (Pure Rust 版)")]
struct Args {
    /// TOML 配置文件，可放在命令前或子命令后
    #[arg(short = 'c', long, global = true)]
    config: Option<PathBuf>,
    /// Release 仓库 URL
    #[arg(short = 'U', long, env = "NEXUS_URL")]
    url: Option<String>,

    /// Snapshot 仓库 URL (可选)
    #[arg(short = 'S', long, env = "NEXUS_SNAPSHOT_URL")]
    snapshot_url: Option<String>,

    /// 用户名
    #[arg(short = 'u', long, env = "NEXUS_USERNAME")]
    username: Option<String>,

    /// 密码
    #[arg(short = 'p', long, env = "NEXUS_PASSWORD")]
    password: Option<String>,

    /// 扫描根目录 (包含 org/, com/ 的那一层)
    #[arg(short = 'd', long, env = "NEXUS_DIR", default_value = ".")]
    dir: String,

    /// 是否强制重新上传
    #[arg(short = 'f', long, env = "NEXUS_FORCE")]
    force: bool,

    /// 排除关键字 (逗号分隔)
    #[arg(short = 'E', long, env = "NEXUS_EXCLUDE", value_delimiter = ',')]
    exclude: Vec<String>,

    /// 跳过大于此大小的二进制文件 (MB)
    #[arg(long, default_value_t = 100)]
    max_size: u64,

    /// 状态数据库路径 (redb 格式)
    #[arg(long, default_value = "uploader_state.db")]
    db_path: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum Command {
    /// 将本地 Maven 仓库作为只读 Maven 仓库提供服务
    Serve(ServeCli),
    /// 启动临时 Maven mirror 并执行 clean package 下载依赖
    Download(DownloadCli),
    /// 清理 Maven 仓库中的远程标记和失败更新文件
    Clean(CleanCli),
}

#[derive(clap::Args, Debug, Clone)]
struct CleanCli {
    /// Maven 仓库目录，默认 ~/.m2/repository
    #[arg(long, short = 'd')]
    repo: Option<PathBuf>,
    /// 只显示将要删除的文件，不实际删除
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct ServeCli {
    /// 本地 Maven 仓库目录，默认使用 ~/.m2/repository
    #[arg(short = 'd', long, env = "MAVEN_REPO_DIR")]
    dir: Option<PathBuf>,
    /// 监听地址
    #[arg(long)]
    host: Option<String>,
    /// 监听端口
    #[arg(short = 'P', long)]
    port: Option<u16>,
    /// 本地缺少构件时回源的仓库地址，可重复指定，按顺序尝试
    #[arg(
        long = "upstream",
        visible_alias = "central-url",
        value_delimiter = ','
    )]
    upstreams: Option<Vec<String>>,
}

#[derive(clap::Args, Debug, Clone)]
struct DownloadCli {
    /// Maven 项目运行目录，默认当前目录
    workdir: Option<PathBuf>,
    /// Maven 下载依赖保存目录，传给 -Dmaven.repo.local
    #[arg(long, short = 'o')]
    download_repo: Option<PathBuf>,
    /// 作为内置 Maven mirror 根目录的已有仓库，默认 ~/.m2/repository
    #[arg(long)]
    server_repo: Option<PathBuf>,
    /// 本地缺少构件时回源的仓库地址，可重复指定，按顺序尝试
    #[arg(
        long = "upstream",
        visible_alias = "central-url",
        value_delimiter = ','
    )]
    upstreams: Option<Vec<String>>,
    /// Maven 可执行文件；未指定时优先使用项目中的 mvnw/mvnw.cmd
    #[arg(long)]
    maven: Option<PathBuf>,
    /// 仅打印将要执行的 Maven 命令
    #[arg(long)]
    dry_run: bool,
    /// 传递给 Maven 的额外参数
    #[arg(last = true)]
    maven_args: Vec<String>,
}

#[derive(Debug, Clone)]
struct MavenArtifact {
    group_id: String,
    artifact_id: String,
    version: String,
    files: Vec<(PathBuf, String)>,
}

fn create_pure_rust_client() -> Result<Client> {
    // 1. 初始化纯 Rust 的加密提供者 (RustCrypto)
    // 这步替代了默认的 ring 或 aws-lc-rs
    let provider = rustls_rustcrypto::provider();

    // 2. 加载根证书库 (这就是之前提到的 build_root_store 逻辑)
    let mut root_store = RootCertStore::empty();
    // 使用 webpki-roots 提供的 Mozilla 根证书集
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // 3. 构建 rustls 配置
    // builder_with_provider 明确指定使用刚才定义的 provider
    let tls_config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("无法配置协议版本")?
        .with_root_certificates(root_store) // 这里应用了刚才加载的证书
        .with_no_client_auth();

    // 4. 将配置注入 reqwest
    // 注意：必须开启 reqwest 的 "rustls-tls-manual-roots-no-provider" feature
    let client = reqwest::blocking::Client::builder()
        .use_preconfigured_tls(tls_config)
        .build()
        .context("无法构建 reqwest 客户端")?;

    Ok(client)
}

fn main() -> Result<()> {
    let parsed = Args::parse();
    if let Some(Command::Serve(cli)) = parsed.command.clone() {
        let file_config = config::load(parsed.config.as_deref())?;
        let serve_config = file_config.serve.unwrap_or_default();
        return server::run_server(server::ServerArgs {
            root: cli
                .dir
                .or_else(|| serve_config.dir.map(config::path))
                .unwrap_or_else(downloader::default_server_repo),
            host: cli
                .host
                .or(serve_config.host)
                .unwrap_or_else(|| "127.0.0.1".into()),
            port: cli.port.or(serve_config.port).unwrap_or(8080),
            upstreams: cli
                .upstreams
                .or(serve_config.upstreams)
                .unwrap_or_else(|| vec!["https://repo.maven.apache.org/maven2".into()]),
            client: Arc::new(create_pure_rust_client()?),
        });
    }
    if let Some(Command::Download(cli)) = parsed.command.clone() {
        let file_config = config::load(parsed.config.as_deref())?;
        let download_config = file_config.download.unwrap_or_default();
        let mut maven_args = download_config.maven_args.unwrap_or_default();
        maven_args.extend(cli.maven_args);
        return downloader::run_download(downloader::DownloadArgs {
            workdir: cli
                .workdir
                .or_else(|| download_config.workdir.map(config::path))
                .unwrap_or_else(|| PathBuf::from(".")),
            download_repo: cli
                .download_repo
                .or_else(|| download_config.download_repo.map(config::path))
                .unwrap_or_else(|| PathBuf::from("maven-repo")),
            server_repo: cli
                .server_repo
                .or_else(|| download_config.server_repo.map(config::path))
                .unwrap_or_else(downloader::default_server_repo),
            upstreams: cli
                .upstreams
                .or(download_config.upstreams)
                .unwrap_or_else(|| vec!["https://repo.maven.apache.org/maven2".into()]),
            maven: cli
                .maven
                .or_else(|| download_config.maven.map(config::path)),
            maven_args,
            dry_run: cli.dry_run || download_config.dry_run.unwrap_or(false),
            client: Arc::new(create_pure_rust_client()?),
        });
    }
    if let Some(Command::Clean(cli)) = parsed.command {
        return cleanup::run_cleanup(cleanup::CleanupArgs {
            repo: cli.repo.unwrap_or_else(downloader::default_server_repo),
            dry_run: cli.dry_run,
        });
    }
    let args = Arc::new(parsed);
    let _ = args.url.as_ref().context("上传模式需要 --url/-U")?;
    let _ = args
        .username
        .as_ref()
        .context("上传模式需要 --username/-u")?;
    let _ = args
        .password
        .as_ref()
        .context("上传模式需要 --password/-p")?;

    // 1. 初始化纯 Rust 数据库 redb
    let db = Arc::new(
        Database::builder()
            .create(&args.db_path)
            .context("无法打开/创建 redb 数据库")?,
    );
    // 初始化表结构
    // 1.1 背景数据库写入线程
    let (db_tx, db_rx) = unbounded::<(String, u64)>();
    let db_writer_handle = {
        let db_writer = Arc::clone(&db);
        thread::spawn(move || {
            while let Ok((key, mtime)) = db_rx.recv() {
                let mut batch = vec![(key, mtime)];
                // 尽量多取一点，攒够批量提交 (最多 100 条或直到管道空)
                while batch.len() < 100 {
                    if let Ok(msg) = db_rx.try_recv() {
                        batch.push(msg);
                    } else {
                        break;
                    }
                }
                if let Ok(write_txn) = db_writer.begin_write() {
                    if let Ok(mut table) = write_txn.open_table(TABLE) {
                        for (k, v) in batch {
                            let _ = table.insert(k.as_str(), v);
                        }
                    }
                    let _ = write_txn.commit();
                }
            }
        })
    };

    // 2. 规范化路径
    let root_path =
        fs::canonicalize(&args.dir).with_context(|| format!("路径无效: {}", args.dir))?;

    // 3. 进度条
    let upload_pb = ProgressBar::new(0);
    upload_pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.blue} [{bar:40.cyan/blue}] {pos}/{len} {msg} ({percent}%)")?
            .progress_chars("#>-"),
    );

    let (tx, rx) = unbounded::<MavenArtifact>();
    let processed_poms = Arc::new(DashMap::new());

    // 4. 扫描线程 (生产者)
    let args_scan = Arc::clone(&args);
    let pb_scan = upload_pb.clone();
    let processed_ref = Arc::clone(&processed_poms);
    let root_ref = root_path.clone();
    thread::spawn(move || {
        WalkDir::new(&args_scan.dir)
            .parallelism(jwalk::Parallelism::RayonDefaultPool {
                busy_timeout: Duration::from_secs(1),
            })
            .into_iter()
            .filter_map(|e| e.ok())
            .for_each(|entry| {
                let path = entry.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();

                if name.ends_with(".pom") || name == "pom.xml" {
                    if let Ok(art) = extract_full_artifact(&path, &root_ref) {
                        if is_excluded(&art, &args_scan, &pb_scan) {
                            return;
                        }
                        if processed_ref.insert(path.to_path_buf(), ()).is_none() {
                            pb_scan.inc_length(1);
                            let _ = tx.send(art);
                        }
                    }
                }
            });
    });

    // 5. 上传逻辑 (消费者)
    let client = Arc::new(create_pure_rust_client()?);
    let parallel_iter = ParallelBridge::par_bridge(rx.into_iter());

    ParallelIterator::for_each(parallel_iter, |artifact| {
        let is_snapshot = artifact.version.ends_with("-SNAPSHOT");
        let upload_url = args.url.as_ref().expect("validated above");
        let raw_url = if is_snapshot {
            args.snapshot_url.as_ref().unwrap_or(upload_url)
        } else {
            upload_url
        };
        let base_url = if raw_url.ends_with('/') {
            raw_url.to_string()
        } else {
            format!("{}/", raw_url)
        };

        upload_pb.set_message(format!("{}:{}", artifact.artifact_id, artifact.version));

        for (f_path, remote_ext) in &artifact.files {
            let _ = upload_file(
                &client, &base_url, &args, &artifact, f_path, remote_ext, &upload_pb, &db, &db_tx,
            );
        }
        upload_pb.inc(1);
    });

    drop(db_tx); // 关闭 Sender，使数据库线程结束
    let _ = db_writer_handle.join();

    upload_pb.finish_with_message("✅ 任务完成");
    Ok(())
}

fn extract_full_artifact(pom_path: &Path, root_path: &Path) -> Result<MavenArtifact> {
    let abs_pom_path = fs::canonicalize(pom_path)?;
    let relative_path = abs_pom_path
        .strip_prefix(root_path)
        .map_err(|_| anyhow::anyhow!("路径不在指定的根目录下: {:?}", abs_pom_path))?;

    let components: Vec<_> = relative_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();

    let n = components.len();
    if n < 4 {
        return Err(anyhow::anyhow!(
            "无效的 Maven 目录结构: {:?}",
            relative_path
        ));
    }

    // components 例子: ["org", "example", "my-app", "1.0", "my-app-1.0.pom"]
    let version = components[n - 2].to_string();
    let artifact_id = components[n - 3].to_string();
    let group_id = components[..n - 3].join(".");

    let prefix = format!("{}-{}", artifact_id, version);
    let parent_dir = pom_path.parent().context("无法获取父目录")?;
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(parent_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() {
                let fname = p.file_name().unwrap_or_default().to_string_lossy();
                // 匹配以 artifact-version 开头的文件，排除临时文件
                if fname.starts_with(&prefix)
                    && !fname.contains("_remote.repositories")
                    && !fname.ends_with(".lastUpdated")
                {
                    let ext = fname.get(prefix.len() + 1..).unwrap_or("").to_string();
                    if !ext.is_empty() {
                        files.push((p, ext));
                    }
                }
            }
        }
    }
    Ok(MavenArtifact {
        group_id,
        artifact_id,
        version,
        files,
    })
}

fn is_excluded(art: &MavenArtifact, args: &Args, pb: &ProgressBar) -> bool {
    for pattern in &args.exclude {
        if art.artifact_id.contains(pattern) || art.group_id.contains(pattern) {
            pb.println(format!(
                "  [🚫] 匹配排除规则 '{}': {}",
                pattern, art.artifact_id
            ));
            return true;
        }
    }
    for (path, ext) in &art.files {
        if ext == "jar" || ext == "war" {
            if let Ok(m) = fs::metadata(path) {
                if m.len() / 1024 / 1024 > args.max_size {
                    pb.println(format!("  [🐘] 跳过大文件: {}", art.artifact_id));
                    return true;
                }
            }
        }
    }
    false
}

fn upload_file(
    client: &reqwest::blocking::Client,
    base_url: &str,
    args: &Args,
    artifact: &MavenArtifact,
    file_path: &Path,
    remote_ext: &str,
    pb: &ProgressBar,
    db: &Database,
    db_tx: &crossbeam_channel::Sender<(String, u64)>,
) -> Result<()> {
    let group_path = artifact.group_id.replace('.', "/");
    let file_name = file_path
        .file_name()
        .context("无法获取文件名")?
        .to_string_lossy();
    let target_url = format!(
        "{}{}/{}/{}/{}",
        base_url, group_path, artifact.artifact_id, artifact.version, file_name
    );

    let mtime = fs::metadata(file_path)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs();

    if !args.force {
        // redb 读操作
        let skip = {
            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(TABLE)?;
            table
                .get(target_url.as_str())?
                .map_or(false, |v| v.value() == mtime)
        };
        if skip {
            pb.println(format!("  [-] 远程(DB)已存在: {}", file_name));
            return Ok(());
        }

        let r_status = client
            .head(&target_url)
            .basic_auth(
                args.username.as_ref().expect("validated above"),
                Some(args.password.as_ref().expect("validated above")),
            )
            .send();
        if let Ok(r) = r_status {
            if r.status().is_success() {
                let _ = db_tx.send((target_url.clone(), mtime));
                pb.println(format!("  [-] 远程已存在: {}", file_name));
                return Ok(());
            }
        }
    }

    let file = File::open(file_path)?;
    let put_resp = client
        .put(&target_url)
        .basic_auth(
            args.username.as_ref().expect("validated above"),
            Some(args.password.as_ref().expect("validated above")),
        )
        .body(file)
        .send();

    match put_resp {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                if !remote_ext.contains("sha1") && !remote_ext.contains("md5") {
                    pb.println(format!("  [+] 上传成功: {}", file_name));
                }
                let _ = db_tx.send((target_url, mtime));
            } else {
                let msg = resp.text().unwrap_or_default();
                pb.println(format!("  [❌] 失败 ({}): {} - {}", status, file_name, msg));
            }
        }
        Err(e) => pb.println(format!("  [!] 网络错误: {}", e)),
    }
    Ok(())
}
