//! 本地 Maven 仓库批量上传。
//!
//! 扫描本地仓库中的 `*.pom` / `pom.xml`，把同目录下 `artifactId-version.*`
//! 文件 PUT 到远端仓库。任何文件上传失败都会被计入，最终以非零退出码结束。

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Sender, unbounded};
use dashmap::DashMap;
use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use redb::{Database, ReadableDatabase, TableDefinition};
use reqwest::blocking::Client;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::components_api::{self, ComponentsTarget};
use crate::tls::HttpSettings;

/// 上传路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UploadMode {
    /// 逐文件 HEAD + PUT：通用，任何支持 Maven 布局的仓库都能用。
    #[default]
    Put,
    /// Nexus 3 Components API：整个构件一次 multipart 提交，
    /// 请求数从「文件数 × 2」降到「构件数」，校验和由服务端计算。
    Components,
}

impl UploadMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "put" | "" => Ok(Self::Put),
            "components" | "component" | "nexus" => Ok(Self::Components),
            other => Err(anyhow!("未知的上传模式: {other}（可选 put / components）")),
        }
    }
}

/// Components 模式下解析好的目标仓库。
#[derive(Debug, Clone)]
struct ComponentsMode {
    release: ComponentsTarget,
    snapshot: Option<ComponentsTarget>,
}

/// 上传状态表：Key 是目标 URL，Value 是文件指纹（`大小:纳秒mtime`）。
///
/// 早期版本只保存秒级 mtime，同一秒内的修改会被误判为已上传，因此改为
/// 保存文件大小 + 纳秒 mtime。
const TABLE: TableDefinition<&str, &str> = TableDefinition::new("uploads_v2");

/// 汇总输出里最多展示的失败明细条数。
const MAX_RECORDED_ERRORS: usize = 20;

/// 批量扫描的进度提示间隔。
const PROGRESS_STYLE: &str = "{spinner:.blue} [{bar:40.cyan/blue}] {pos}/{len} {msg} ({percent}%)";

#[derive(Debug, Clone)]
pub struct UploadOptions {
    /// Release 仓库地址（保证以 `/` 结尾）。
    pub url: String,
    pub snapshot_url: Option<String>,
    pub username: String,
    pub password: String,
    pub dir: PathBuf,
    pub force: bool,
    pub exclude: Vec<String>,
    /// 跳过超过此大小（MB）的 jar/war。
    pub max_size_mb: u64,
    pub db_path: PathBuf,
    /// 上传并发数：同时进行的 HTTP 请求数（每个上传线程一个请求）。
    pub concurrency: usize,
    /// 瞬时错误（网络错误 / 5xx / 429）的最大重试次数。
    pub retries: u32,
    /// 跳过 HEAD 远端存在性检查（状态库检查仍然生效）。
    pub skip_head_check: bool,
    /// 上传路径：逐文件 PUT 或 Nexus Components API。
    pub mode: UploadMode,
    /// Components 模式下显式指定仓库名（不填则从 URL 的 `/repository/<名>` 推断）。
    pub repository: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MavenArtifact {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub files: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOutcome {
    Uploaded,
    AlreadyExists,
}

/// 失败记录：总计数 + 前若干条明细。
#[derive(Default)]
struct FailureLog {
    count: AtomicUsize,
    samples: std::sync::Mutex<Vec<String>>,
}

impl FailureLog {
    fn record(&self, message: String) {
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if samples.len() < MAX_RECORDED_ERRORS {
            samples.push(message);
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    fn samples(&self) -> Vec<String> {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 默认上传并发数：等于 CPU 核数。
///
/// 上传是纯 I/O 密集操作，实测在 80ms 模拟延迟下吞吐与并发数严格成正比
/// （4 并发 800 请求约 16s，32 并发约 2s），因此远距离仓库建议显式调高。
pub fn default_upload_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get().max(1))
        .unwrap_or(4)
}

/// 扫描专用线程池大小：最多 4 个线程，避免和上传侧抢 CPU。
fn scan_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get().clamp(1, 4))
        .unwrap_or(2)
}

/// 输出一行日志：终端下跟随进度条，非终端（CI/管道）时直接写 stderr，
/// 避免消息被进度条隐藏。
fn emit(pb: &ProgressBar, message: String) {
    if pb.is_hidden() {
        eprintln!("{message}");
    } else {
        pb.println(message);
    }
}

/// 计算文件指纹：大小 + 纳秒级 mtime。
pub fn fingerprint(path: &Path) -> Result<String> {
    let metadata =
        fs::metadata(path).with_context(|| format!("无法读取文件信息: {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("无法读取修改时间: {}", path.display()))?;
    let nanos = modified
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    Ok(format!("{}:{nanos}", metadata.len()))
}

pub fn run_upload(options: UploadOptions, http: &HttpSettings) -> Result<()> {
    // Components 模式需要先解析出「服务根地址 + 仓库名」，解析失败要在
    // 任何副作用（建目录、开数据库）之前报错。
    let components = match options.mode {
        UploadMode::Put => None,
        UploadMode::Components => Some(ComponentsMode {
            release: resolve_target(&options.url, options.repository.as_deref(), "url")?,
            snapshot: match &options.snapshot_url {
                Some(url) => Some(resolve_target(
                    url,
                    options.repository.as_deref(),
                    "snapshot-url",
                )?),
                None => None,
            },
        }),
    };

    let root_path = fs::canonicalize(&options.dir)
        .with_context(|| format!("路径无效: {}", options.dir.display()))?;

    // 状态库父目录可能尚未存在（例如 ~/.maven-util/）
    if let Some(parent) = options.db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建状态库目录: {}", parent.display()))?;
    }
    let db = Arc::new(
        Database::builder()
            .create(&options.db_path)
            .with_context(|| format!("无法打开/创建 redb 数据库: {}", options.db_path.display()))?,
    );
    // 首次运行时先建表，否则读事务打开状态表会失败
    ensure_table(&db)?;

    let (db_tx, db_rx) = unbounded::<(String, String)>();
    let db_writer_handle = spawn_db_writer(Arc::clone(&db), db_rx);

    let upload_pb = ProgressBar::new(0);
    upload_pb.set_style(
        ProgressStyle::default_bar()
            .template(PROGRESS_STYLE)
            .context("无法配置进度条样式")?
            .progress_chars("#>-"),
    );

    let (tx, rx) = unbounded::<MavenArtifact>();
    let processed_poms = Arc::new(DashMap::new());
    // 扫描阶段两类问题分开统计：
    // - warnings：结构不合法的 POM 等，可跳过但要报告
    // - errors：目录遍历本身失败（如线程池/IO 错误），扫描不完整必须以失败退出
    let scan_warnings = Arc::new(AtomicUsize::new(0));
    let scan_errors = Arc::new(AtomicUsize::new(0));

    // 扫描线程（生产者）
    let scan_dir = options.dir.clone();
    let scan_root = root_path.clone();
    let scan_exclude = options.exclude.clone();
    let scan_max_size = options.max_size_mb;
    let scan_pb = upload_pb.clone();
    let scan_processed = Arc::clone(&processed_poms);
    let scan_warnings_thread = Arc::clone(&scan_warnings);
    let scan_errors_thread = Arc::clone(&scan_errors);
    let scan_handle = thread::spawn(move || {
        WalkDir::new(&scan_dir)
            // 扫描使用独立线程池：上传侧的 par_bridge 会占满默认 rayon 池，
            // 共用默认池时 jwalk 会因拿不到线程而中止扫描。
            .parallelism(jwalk::Parallelism::RayonNewPool(scan_thread_count()))
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(error) => {
                    scan_errors_thread.fetch_add(1, Ordering::Relaxed);
                    emit(&scan_pb, format!("  [!] 扫描目录失败: {error}"));
                    None
                }
            })
            .for_each(|entry| {
                let path = entry.path().to_path_buf();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if !(name.ends_with(".pom") || name == "pom.xml") {
                    return;
                }
                let artifact = match extract_full_artifact(&path, &scan_root) {
                    Ok(artifact) => artifact,
                    Err(error) => {
                        scan_warnings_thread.fetch_add(1, Ordering::Relaxed);
                        emit(
                            &scan_pb,
                            format!("  [!] 跳过 {}: {error:#}", path.display()),
                        );
                        return;
                    }
                };
                if is_excluded(&artifact, &scan_exclude, scan_max_size, &scan_pb) {
                    return;
                }
                if scan_processed.insert(path, ()).is_none() {
                    scan_pb.inc_length(1);
                    let _ = tx.send(artifact);
                }
            });
    });

    // 上传线程（消费者）
    let worker_count = options.concurrency.max(1);
    let uploader = Arc::new(Uploader {
        client: Arc::new(http.upload_client(worker_count)?),
        options: Arc::new(options),
        components,
        pb: upload_pb.clone(),
        db,
        db_tx,
    });
    let failures = Arc::new(FailureLog::default());
    let uploaded = Arc::new(AtomicUsize::new(0));
    let already_exists = Arc::new(AtomicUsize::new(0));

    // 专用上传线程池：每个线程阻塞在一个 HTTP 请求上，线程数与 CPU 无关。
    // 阻塞线程的内存开销可以忽略，而高延迟远端的吞吐与并发数成正比。
    let workers: Vec<_> = (0..worker_count)
        .map(|_| {
            let uploader = Arc::clone(&uploader);
            let rx = rx.clone();
            let failures = Arc::clone(&failures);
            let uploaded = Arc::clone(&uploaded);
            let already_exists = Arc::clone(&already_exists);
            thread::spawn(move || {
                while let Ok(artifact) = rx.recv() {
                    uploader.upload_artifact(&artifact, &failures, &uploaded, &already_exists);
                }
            })
        })
        .collect();
    drop(rx); // 扫描线程结束后 channel 断开，所有 worker 自然退出
    for worker in workers {
        let _ = worker.join();
    }

    drop(uploader); // 关闭 Sender，使数据库写入线程结束
    let _ = db_writer_handle.join();
    let _ = scan_handle.join();

    let failed = failures.count();
    let scanned_warnings = scan_warnings.load(Ordering::Relaxed);
    let scanned_errors = scan_errors.load(Ordering::Relaxed);
    if scanned_warnings > 0 {
        println!("扫描阶段跳过 {scanned_warnings} 个无法处理的文件（详见上方 [!] 日志）");
    }

    // 目录遍历失败意味着扫描不完整，即使已上传的文件都成功也必须非零退出
    if scanned_errors > 0 {
        upload_pb.finish_with_message("❌ 扫描失败");
        return Err(anyhow!(
            "扫描阶段发生 {scanned_errors} 个错误，上传结果不完整（成功 {}，失败 {failed}）",
            uploaded.load(Ordering::Relaxed),
        ));
    }

    if failed > 0 {
        upload_pb.finish_with_message("❌ 上传结束，存在失败");
        eprintln!("上传失败 {failed} 个文件：");
        for sample in failures.samples() {
            eprintln!("  - {sample}");
        }
        return Err(anyhow!(
            "上传失败 {failed} 个文件（成功 {}，已存在 {}）",
            uploaded.load(Ordering::Relaxed),
            already_exists.load(Ordering::Relaxed),
        ));
    }

    upload_pb.finish_with_message("✅ 任务完成");
    println!(
        "上传完成：成功 {} 个文件，跳过 {} 个已存在文件",
        uploaded.load(Ordering::Relaxed),
        already_exists.load(Ordering::Relaxed),
    );
    Ok(())
}

struct Uploader {
    client: Arc<Client>,
    options: Arc<UploadOptions>,
    /// Components 模式下已解析的目标仓库；PUT 模式为 None。
    components: Option<ComponentsMode>,
    pb: ProgressBar,
    db: Arc<Database>,
    db_tx: Sender<(String, String)>,
}

/// 把仓库 URL 解析成 Components API 需要的「服务根地址 + 仓库名」。
///
/// 配置了 `repository` 时直接用它作为仓库名、URL 作为服务根；
/// 否则从 URL 的 `/repository/<名>` 段推断。
fn resolve_target(url: &str, repository: Option<&str>, field: &str) -> Result<ComponentsTarget> {
    match repository {
        Some(name) => ComponentsTarget::from_base_and_repository(url, name)
            .with_context(|| format!("无法解析 {field} 作为服务根地址: {url}")),
        None => ComponentsTarget::from_repository_url(url).with_context(|| {
            format!(
                "无法从 {field} 推断仓库名（需要形如 https://host/repository/<仓库名>）: {url}。                 可改用 `repository` 配置显式指定仓库名，或改回 put 模式"
            )
        }),
    }
}

impl Uploader {
    fn upload_artifact(
        &self,
        artifact: &MavenArtifact,
        failures: &FailureLog,
        uploaded: &AtomicUsize,
        already_exists: &AtomicUsize,
    ) {
        let is_snapshot = artifact.version.ends_with("-SNAPSHOT");
        let raw_url = if is_snapshot {
            self.options
                .snapshot_url
                .as_deref()
                .unwrap_or(&self.options.url)
        } else {
            &self.options.url
        };
        let base_url = if raw_url.ends_with('/') {
            raw_url.to_string()
        } else {
            format!("{raw_url}/")
        };

        self.pb
            .set_message(format!("{}:{}", artifact.artifact_id, artifact.version));

        if let Some(mode) = &self.components {
            let target = if artifact.version.ends_with("-SNAPSHOT") {
                mode.snapshot.as_ref().unwrap_or(&mode.release)
            } else {
                &mode.release
            };
            self.upload_component(artifact, target, failures, uploaded, already_exists);
            self.pb.inc(1);
            return;
        }

        for (file_path, remote_ext) in &artifact.files {
            match self.upload_file(&base_url, artifact, file_path, remote_ext) {
                Ok(FileOutcome::Uploaded) => {
                    uploaded.fetch_add(1, Ordering::Relaxed);
                }
                Ok(FileOutcome::AlreadyExists) => {
                    already_exists.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    emit(
                        &self.pb,
                        format!("  [❌] 失败: {} - {error:#}", file_path.display()),
                    );
                    failures.record(format!("{}: {error:#}", file_path.display()));
                }
            }
        }
        self.pb.inc(1);
    }

    /// Components 模式：整个构件一次 multipart 提交。
    ///
    /// 与 PUT 模式的差异：
    /// - 状态库按**组件**记录指纹（任一资产变化都要整组件重传）；
    /// - 存在性检查用一次查询代替逐文件 HEAD；
    /// - 校验和文件不上传，由服务端计算。
    fn upload_component(
        &self,
        artifact: &MavenArtifact,
        target: &ComponentsTarget,
        failures: &FailureLog,
        uploaded: &AtomicUsize,
        already_exists: &AtomicUsize,
    ) {
        let label = format!(
            "{}:{}:{}",
            artifact.group_id, artifact.artifact_id, artifact.version
        );
        let assets = components_api::collect_assets(artifact);
        if assets.is_empty() {
            emit(&self.pb, format!("  [!] 跳过 {label}：没有可提交的资产"));
            return;
        }
        let key = components_api::component_key(target, artifact);
        let fingerprint = match components_api::component_fingerprint(&assets) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                emit(&self.pb, format!("  [❌] 失败: {label} - {error:#}"));
                failures.record(format!("{label}: {error:#}"));
                return;
            }
        };

        if !self.options.force {
            if let Some(stored) = self.stored_fingerprint(&key).unwrap_or(None)
                && stored == fingerprint
            {
                emit(&self.pb, format!("  [-] 状态库已存在: {label}"));
                already_exists.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // 查询远端是否已有同坐标组件；查询失败不判定成败，继续提交。
            if !self.options.skip_head_check {
                match components_api::component_exists(
                    &self.client,
                    target,
                    artifact,
                    &self.options.username,
                    &self.options.password,
                ) {
                    Ok(true) => {
                        let _ = self.record(&key, &fingerprint);
                        emit(&self.pb, format!("  [-] 远端已存在: {label}"));
                        already_exists.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        emit(&self.pb, format!("  [!] 查询组件失败，继续提交: {error:#}"));
                    }
                }
            }
        }

        // multipart 表单每次重试都要重建（内部持有文件句柄）
        let response = self.send_with_retry("Components 提交", || {
            let form = components_api::build_form(artifact, &assets)?;
            Ok(self
                .client
                .post(target.components_url())
                .query(&[("repository", target.repository.as_str())])
                .basic_auth(&self.options.username, Some(&self.options.password))
                .multipart(form))
        });

        match response {
            Ok(response) if response.status().is_success() => {
                let _ = self.record(&key, &fingerprint);
                uploaded.fetch_add(assets.len(), Ordering::Relaxed);
                emit(
                    &self.pb,
                    format!(
                        "  [+] 提交成功: {label}（{} 个资产，1 次请求）",
                        assets.len()
                    ),
                );
            }
            Ok(response) => {
                let status = response.status();
                let detail = response.text().unwrap_or_default();
                let message = format!("HTTP {status}: {}", truncate(&detail));
                emit(&self.pb, format!("  [❌] 失败: {label} - {message}"));
                failures.record(format!("{label}: {message}"));
            }
            Err(error) => {
                emit(&self.pb, format!("  [❌] 失败: {label} - {error:#}"));
                failures.record(format!("{label}: {error:#}"));
            }
        }
    }

    fn upload_file(
        &self,
        base_url: &str,
        artifact: &MavenArtifact,
        file_path: &Path,
        remote_ext: &str,
    ) -> Result<FileOutcome> {
        let group_path = artifact.group_id.replace('.', "/");
        let file_name = file_path
            .file_name()
            .context("无法获取文件名")?
            .to_string_lossy();
        let target_url = format!(
            "{base_url}{}/{}/{}/{}",
            group_path, artifact.artifact_id, artifact.version, file_name
        );
        let fingerprint = fingerprint(file_path)?;

        if !self.options.force && !self.options.skip_head_check {
            if let Some(stored) = self.stored_fingerprint(&target_url)?
                && stored == fingerprint
            {
                emit(&self.pb, format!("  [-] 远程(DB)已存在: {file_name}"));
                return Ok(FileOutcome::AlreadyExists);
            }
            // HEAD 检查：远端已存在则记录并跳过。网络错误不在此处判定成败，
            // 继续走 PUT，由 PUT 的结果决定是否计为失败。
            match self
                .client
                .head(&target_url)
                .basic_auth(&self.options.username, Some(&self.options.password))
                .send()
            {
                Ok(response) if response.status().is_success() => {
                    self.record(&target_url, &fingerprint)?;
                    emit(&self.pb, format!("  [-] 远程已存在: {file_name}"));
                    return Ok(FileOutcome::AlreadyExists);
                }
                Ok(_) => {}
                Err(error) => {
                    emit(&self.pb, format!("  [!] HEAD 检查失败，继续上传: {error}"));
                }
            }
        }

        // PUT 是幂等操作，网络错误 / 5xx / 429 都可以安全重试
        let response = self.send_with_retry("PUT 上传", || {
            let file = File::open(file_path)
                .with_context(|| format!("无法读取文件: {}", file_path.display()))?;
            Ok(self
                .client
                .put(&target_url)
                .basic_auth(&self.options.username, Some(&self.options.password))
                .body(file))
        })?;

        if response.status().is_success() {
            if !remote_ext.contains("sha1") && !remote_ext.contains("md5") {
                emit(&self.pb, format!("  [+] 上传成功: {file_name}"));
            }
            self.record(&target_url, &fingerprint)?;
            return Ok(FileOutcome::Uploaded);
        }
        let status = response.status();
        let detail = response.text().unwrap_or_default();
        Err(anyhow!("HTTP {status}: {}", truncate(&detail)))
    }

    /// 发送请求，对网络错误、5xx 与 429 做指数退避重试。
    ///
    /// 只有 2xx / 3xx / 4xx 会被返回给调用方（4xx 是调用方的问题，重试无意义）；
    /// 重试次数耗尽后返回最后一次的错误。
    fn send_with_retry(
        &self,
        what: &str,
        build: impl Fn() -> Result<reqwest::blocking::RequestBuilder>,
    ) -> Result<reqwest::blocking::Response> {
        let retries = self.options.retries;
        let mut last_error = None;
        for attempt in 0..=retries {
            if attempt > 0 {
                // 250ms、500ms、1s、2s…… 上限 4s
                let backoff = (250_u64 << attempt.min(4)).min(4_000);
                thread::sleep(Duration::from_millis(backoff));
            }
            let builder = build()?;
            match builder.send() {
                Ok(response)
                    if response.status().is_server_error()
                        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS =>
                {
                    let status = response.status();
                    let _ = response.text();
                    last_error = Some(anyhow!("HTTP {status}"));
                    if attempt < retries {
                        emit(
                            &self.pb,
                            format!(
                                "  [!] {what} 返回 {status}，重试 ({}/{retries})",
                                attempt + 1
                            ),
                        );
                    }
                }
                Ok(response) => return Ok(response),
                Err(error) => {
                    last_error = Some(anyhow!("网络错误: {error}"));
                    if attempt < retries {
                        emit(
                            &self.pb,
                            format!(
                                "  [!] {what} 失败: {error}，重试 ({}/{retries})",
                                attempt + 1
                            ),
                        );
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("{what} 失败")))
    }

    /// 读取状态库中已记录的指纹。
    fn stored_fingerprint(&self, target_url: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read().context("无法开始读取上传状态库")?;
        let table = read_txn.open_table(TABLE).context("无法打开上传状态表")?;
        let value = table
            .get(target_url)
            .context("无法读取上传状态")?
            .map(|access| access.value().to_string());
        Ok(value)
    }

    /// 写入状态库（由后台线程批量落盘）。
    fn record(&self, target_url: &str, fingerprint: &str) -> Result<()> {
        self.db_tx
            .send((target_url.to_string(), fingerprint.to_string()))
            .map_err(|_| anyhow!("上传状态库写入线程已退出"))
    }
}

fn truncate(value: &str) -> String {
    const MAX: usize = 200;
    let value = value.trim();
    if value.len() <= MAX {
        return value.to_string();
    }
    let mut end = MAX;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// 状态库后台写入线程：攒批提交，减少事务次数。
/// 确保状态表存在（幂等）。
fn ensure_table(db: &Database) -> Result<()> {
    let write_txn = db.begin_write().context("无法初始化上传状态库")?;
    {
        let _table = write_txn.open_table(TABLE).context("无法创建上传状态表")?;
    }
    write_txn.commit().context("无法提交上传状态库初始化")?;
    Ok(())
}

fn spawn_db_writer(
    db: Arc<Database>,
    rx: crossbeam_channel::Receiver<(String, String)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok((key, fingerprint)) = rx.recv() {
            let mut batch = vec![(key, fingerprint)];
            while batch.len() < 100 {
                match rx.try_recv() {
                    Ok(message) => batch.push(message),
                    Err(_) => break,
                }
            }
            let write_txn = match db.begin_write() {
                Ok(write_txn) => write_txn,
                Err(error) => {
                    eprintln!("无法开始写入上传状态库: {error}");
                    continue;
                }
            };
            // 先把插入结果收集起来，保证 table 的借用在 commit 前结束
            let insert_errors = {
                let mut table = match write_txn.open_table(TABLE) {
                    Ok(table) => table,
                    Err(error) => {
                        eprintln!("无法打开上传状态表: {error}");
                        continue;
                    }
                };
                let mut errors = Vec::new();
                for (key, fingerprint) in batch {
                    if let Err(error) = table.insert(key.as_str(), fingerprint.as_str()) {
                        errors.push(format!("{key}: {error}"));
                    }
                }
                errors
            };
            for error in insert_errors {
                eprintln!("无法记录上传状态 {error}");
            }
            if let Err(error) = write_txn.commit() {
                eprintln!("无法提交上传状态: {error}");
            }
        }
    })
}

pub(crate) fn extract_full_artifact(pom_path: &Path, root_path: &Path) -> Result<MavenArtifact> {
    let abs_pom_path = fs::canonicalize(pom_path)?;
    let relative_path = abs_pom_path
        .strip_prefix(root_path)
        .map_err(|_| anyhow!("路径不在指定的根目录下: {abs_pom_path:?}"))?;

    let components: Vec<_> = relative_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect();

    let n = components.len();
    if n < 4 {
        return Err(anyhow!("无效的 Maven 目录结构: {relative_path:?}"));
    }

    // components 例子: ["org", "example", "my-app", "1.0", "my-app-1.0.pom"]
    let version = components[n - 2].to_string();
    let artifact_id = components[n - 3].to_string();
    let group_id = components[..n - 3].join(".");

    let prefix = format!("{artifact_id}-{version}");
    let parent_dir = pom_path.parent().context("无法获取父目录")?;
    let mut files = Vec::new();

    for entry in fs::read_dir(parent_dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        if !file_name.starts_with(&prefix)
            || file_name.contains("_remote.repositories")
            || file_name.ends_with(".lastUpdated")
        {
            continue;
        }
        let extension = file_name
            .get(prefix.len() + 1..)
            .filter(|extension| !extension.is_empty())
            .unwrap_or_default()
            .to_string();
        if !extension.is_empty() {
            files.push((path, extension));
        }
    }

    Ok(MavenArtifact {
        group_id,
        artifact_id,
        version,
        files,
    })
}

fn is_excluded(
    artifact: &MavenArtifact,
    exclude: &[String],
    max_size_mb: u64,
    pb: &ProgressBar,
) -> bool {
    for pattern in exclude {
        if artifact.artifact_id.contains(pattern) || artifact.group_id.contains(pattern) {
            emit(
                pb,
                format!(
                    "  [🚫] 匹配排除规则 '{}': {}",
                    pattern, artifact.artifact_id
                ),
            );
            return true;
        }
    }
    for (path, extension) in &artifact.files {
        if extension != "jar" && extension != "war" {
            continue;
        }
        match fs::metadata(path) {
            Ok(metadata) => {
                if metadata.len() / 1024 / 1024 > max_size_mb {
                    emit(pb, format!("  [🐘] 跳过大文件: {}", artifact.artifact_id));
                    return true;
                }
            }
            // 读不到文件信息时明确告知，不再静默忽略
            Err(error) => emit(
                pb,
                format!("  [!] 读取文件信息失败 {}: {error}", path.display()),
            ),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "maven-util-upload-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn fingerprint_changes_when_content_changes_in_same_second() {
        let root = temp_dir("fingerprint");
        let file = root.join("demo.jar");
        fs::write(&file, b"aaaa").unwrap();
        let first = fingerprint(&file).unwrap();
        thread::sleep(Duration::from_millis(20));
        fs::write(&file, b"bbbb").unwrap();
        let second = fingerprint(&file).unwrap();
        assert_ne!(first, second, "同一秒内的修改必须产生不同的指纹");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fingerprint_changes_with_size() {
        let root = temp_dir("fingerprint-size");
        let file = root.join("demo.jar");
        fs::write(&file, b"aa").unwrap();
        let first = fingerprint(&file).unwrap();
        fs::write(&file, b"aaaa").unwrap();
        assert_ne!(first, fingerprint(&file).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn extracts_artifact_from_maven_layout() {
        let root = temp_dir("extract");
        let artifact_dir = root.join("org/example/demo/1.0");
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::write(artifact_dir.join("demo-1.0.pom"), b"<project/>").unwrap();
        fs::write(artifact_dir.join("demo-1.0.jar"), b"jar").unwrap();
        fs::write(artifact_dir.join("demo-1.0.jar.sha1"), b"sha").unwrap();
        fs::write(artifact_dir.join("demo-1.0.jar.lastUpdated"), b"x").unwrap();
        fs::write(artifact_dir.join("_remote.repositories"), b"x").unwrap();
        fs::write(artifact_dir.join("other-1.0.jar"), b"x").unwrap();

        let artifact = extract_full_artifact(&artifact_dir.join("demo-1.0.pom"), &root).unwrap();
        assert_eq!(artifact.group_id, "org.example");
        assert_eq!(artifact.artifact_id, "demo");
        assert_eq!(artifact.version, "1.0");

        let mut names: Vec<_> = artifact
            .files
            .iter()
            .map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["demo-1.0.jar", "demo-1.0.jar.sha1", "demo-1.0.pom"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_pom_outside_maven_layout() {
        let root = temp_dir("extract-invalid");
        let pom = root.join("pom.xml");
        fs::write(&pom, b"<project/>").unwrap();
        assert!(extract_full_artifact(&pom, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclusion_matches_group_or_artifact_and_size() {
        let artifact = MavenArtifact {
            group_id: "com.example.internal".into(),
            artifact_id: "secret".into(),
            version: "1.0".into(),
            files: vec![],
        };
        let pb = ProgressBar::hidden();
        assert!(is_excluded(&artifact, &["internal".into()], 100, &pb));
        assert!(!is_excluded(&artifact, &["other".into()], 100, &pb));

        let root = temp_dir("exclude-size");
        let jar = root.join("big-1.0.jar");
        fs::write(&jar, vec![0_u8; 2 * 1024 * 1024]).unwrap();
        let big = MavenArtifact {
            group_id: "com.example".into(),
            artifact_id: "big".into(),
            version: "1.0".into(),
            files: vec![(jar, "jar".into())],
        };
        assert!(is_excluded(&big, &[], 1, &pb));
        assert!(!is_excluded(&big, &[], 10, &pb));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn truncates_long_error_details() {
        assert_eq!(truncate("short"), "short");
        let long = "错".repeat(200);
        let truncated = truncate(&long);
        assert!(truncated.chars().count() < 201);
        assert!(truncated.ends_with('…'));
    }
}
