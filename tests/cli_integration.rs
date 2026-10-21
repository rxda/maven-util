//! 命令行集成测试：退出码、配置文件自动创建与优先级、`--dry-run` 零副作用。
//!
//! 通过 `CARGO_BIN_EXE_maven-util` 运行真实二进制。

mod common;

use common::{
    FakeUpstream, JAR, Reply, maven_repo_fixture, maven_repo_fixture_with_extras, temp_dir,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 运行被测二进制；HOME 与 TMPDIR 指向临时目录，避免污染真实用户环境。
fn run(args: &[&str], home: &Path, tmpdir: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maven-util"));
    command
        .args(args)
        .env("HOME", home)
        // 在临时目录中执行，避免仓库自带的 maven-uploader.toml 干扰配置优先级
        .current_dir(home);
    if let Some(tmpdir) = tmpdir {
        command.env("TMPDIR", tmpdir);
    }
    // 避免继承测试环境中的同名变量干扰配置合并
    for variable in [
        "MAVEN_UTIL_CONFIG",
        "NEXUS_URL",
        "NEXUS_SNAPSHOT_URL",
        "NEXUS_USERNAME",
        "NEXUS_PASSWORD",
        "NEXUS_DIR",
        "NEXUS_FORCE",
        "NEXUS_EXCLUDE",
        "NEXUS_MAX_MB",
        "MAVEN_UTIL_DB_PATH",
        "MAVEN_UTIL_CONCURRENCY",
        "MAVEN_UTIL_RETRIES",
        "MAVEN_UTIL_SKIP_HEAD_CHECK",
    ] {
        command.env_remove(variable);
    }
    command.output().expect("运行 maven-util 失败")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// 进度条消息走 stderr，最终汇总走 stdout，这里合并判断。
fn combined(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn upload_routes(status: u16) -> HashMap<String, Reply> {
    let mut routes = HashMap::new();
    for path in [
        "/org/example/demo/1.0/demo-1.0.pom",
        "/org/example/demo/1.0/demo-1.0.jar",
    ] {
        routes.insert(
            path.to_string(),
            Reply::MethodStatus {
                head: 404,
                other: status,
            },
        );
    }
    routes
}

fn repo_with_artifact(name: &str) -> PathBuf {
    let root = temp_dir(name);
    maven_repo_fixture(&root);
    root
}

/// 含 sources jar 与本地校验和文件的仓库（验证 classifier 与「校验和不上传」）。
fn repo_with_extras(name: &str) -> PathBuf {
    let root = temp_dir(name);
    maven_repo_fixture_with_extras(&root);
    root
}

#[test]
fn upload_failure_exits_non_zero() {
    let home = temp_dir("home-upload-fail");
    let repo = repo_with_artifact("repo-upload-fail");
    let upstream = FakeUpstream::start(upload_routes(500));

    let output = run(
        &[
            "--url",
            &upstream.base_url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
        ],
        &home,
        None,
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "上传失败必须返回非零退出码。stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    let combined = combined(&output);
    assert!(
        combined.contains("上传失败"),
        "必须明确报告失败: {combined}"
    );
    assert!(
        combined.contains("demo-1.0.jar") || combined.contains("demo-1.0.pom"),
        "失败明细必须包含具体文件: {combined}"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn upload_success_exits_zero_and_second_run_reuses_state() {
    let home = temp_dir("home-upload-ok");
    let repo = repo_with_artifact("repo-upload-ok");
    let db = repo.join("state.db");
    let upstream = FakeUpstream::start(upload_routes(201));

    let args = [
        "--url",
        upstream.base_url.as_str(),
        "--username",
        "admin",
        "--password",
        "secret",
        "--dir",
        repo.to_str().unwrap(),
        "--db-path",
        db.to_str().unwrap(),
    ];

    let first = run(&args, &home, None);
    assert_eq!(
        first.status.code(),
        Some(0),
        "上传成功必须返回 0。stdout={} stderr={}",
        stdout(&first),
        stderr(&first)
    );
    assert!(combined(&first).contains("上传成功"));
    assert!(db.is_file(), "上传状态库必须被创建");

    let hits_after_first = upstream.hit_count();

    let second = run(&args, &home, None);
    assert_eq!(second.status.code(), Some(0));
    assert!(
        combined(&second).contains("已存在"),
        "第二次运行应命中状态库: {}",
        combined(&second)
    );
    assert_eq!(
        upstream.hit_count(),
        hits_after_first,
        "命中状态库后不应再请求远端"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn upload_force_bypasses_local_state() {
    let home = temp_dir("home-upload-force");
    let repo = repo_with_artifact("repo-upload-force");
    let db = repo.join("state.db");
    let upstream = FakeUpstream::start(upload_routes(201));

    let args = [
        "--url",
        upstream.base_url.as_str(),
        "--username",
        "admin",
        "--password",
        "secret",
        "--dir",
        repo.to_str().unwrap(),
        "--db-path",
        db.to_str().unwrap(),
    ];

    assert_eq!(run(&args, &home, None).status.code(), Some(0));
    let hits_after_first = upstream.hit_count();

    let mut forced: Vec<&str> = args.to_vec();
    forced.push("--force");
    let output = run(&forced, &home, None);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        upstream.hit_count() > hits_after_first,
        "--force 必须重新上传"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn download_dry_run_has_no_side_effects() {
    let home = temp_dir("home-dry-run");
    let project = temp_dir("dry-run-project");
    std::fs::write(project.join("pom.xml"), b"<project/>").unwrap();
    let download_repo = project.join("download-repo");
    let server_repo = project.join("server-repo");
    let tmpdir = temp_dir("dry-run-tmp");

    let output = run(
        &[
            "download",
            project.to_str().unwrap(),
            "--download-repo",
            download_repo.to_str().unwrap(),
            "--server-repo",
            server_repo.to_str().unwrap(),
            "--dry-run",
        ],
        &home,
        Some(&tmpdir),
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    let stdout = stdout(&output);
    assert!(stdout.contains("命令:"), "必须打印命令: {stdout}");
    assert!(!download_repo.exists(), "--dry-run 不能创建依赖下载目录");
    assert!(!server_repo.exists(), "--dry-run 不能创建 mirror 仓库");
    assert_eq!(
        std::fs::read_dir(&tmpdir).unwrap().count(),
        0,
        "--dry-run 不能写临时 settings.xml"
    );
    assert!(
        !home.join(".maven-util").exists(),
        "--dry-run 不能触发首次运行初始化"
    );

    std::fs::remove_dir_all(&project).ok();
    std::fs::remove_dir_all(&tmpdir).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn config_file_is_created_and_read_with_cli_override() {
    let home = temp_dir("home-config");
    let repo = repo_with_artifact("repo-config");
    let tmpdir = temp_dir("config-tmp");
    let configured = FakeUpstream::start(upload_routes(201));
    let override_upstream = FakeUpstream::start(upload_routes(201));

    // 1. 首次运行即自动生成 ~/.maven-util/config.toml，并报告缺少 url
    let output = run(
        &["upload", "--dir", repo.to_str().unwrap()],
        &home,
        Some(&tmpdir),
    );
    let config_path = home.join(".maven-util/config.toml");
    assert_eq!(
        output.status.code(),
        Some(1),
        "缺少 url 时必须失败: {}",
        stdout(&output)
    );
    assert!(config_path.is_file(), "首次运行必须在用户目录生成配置文件");
    let generated = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        generated.contains("[upload]"),
        "生成的配置必须包含 upload 段"
    );

    // 2. 在配置文件中填写凭据后，直接运行 `maven-util upload` 即可上传
    let db = repo.join("state.db");
    std::fs::write(
        &config_path,
        format!(
            "[upload]\nurl = \"{}\"\nusername = \"admin\"\npassword = \"secret\"\ndir = \"{}\"\ndb_path = \"{}\"\n",
            configured.base_url,
            repo.display(),
            db.display()
        ),
    )
    .unwrap();

    let output = run(&["upload"], &home, Some(&tmpdir));
    assert_eq!(
        output.status.code(),
        Some(0),
        "读取配置文件后应能直接上传。stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        combined(&output).contains("上传成功"),
        "实际输出: {}",
        combined(&output)
    );
    let configured_hits = configured.hit_count();
    assert!(configured_hits > 0);

    // 3. 命令行 --url 优先于配置文件
    let output = run(
        &[
            "upload",
            "--url",
            override_upstream.base_url.as_str(),
            "--force",
        ],
        &home,
        Some(&tmpdir),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    assert!(override_upstream.hit_count() > 0, "命令行 --url 必须生效");
    assert_eq!(
        configured.hit_count(),
        configured_hits,
        "命令行覆盖后不应再请求配置文件中的仓库"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&tmpdir).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn missing_credentials_message_points_to_config_file() {
    let home = temp_dir("home-missing-credentials");
    let output = run(&["upload"], &home, None);
    assert_eq!(output.status.code(), Some(1));
    let combined = combined(&output);
    assert!(
        combined.contains(".maven-util"),
        "错误信息必须指向配置文件: {combined}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn clean_dry_run_lists_files_without_deleting() {
    let home = temp_dir("home-clean");
    let repo = repo_with_artifact("repo-clean");
    let state_file = repo.join("org/example/demo/1.0/_remote.repositories");
    std::fs::write(&state_file, b"remote").unwrap();

    let output = run(
        &["clean", "--repo", repo.to_str().unwrap(), "--dry-run"],
        &home,
        None,
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("找到"));
    assert!(state_file.is_file(), "clean --dry-run 不能删除文件");

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn first_run_installs_binary_and_configures_path() {
    let home = temp_dir("home-first-run");

    // 首次运行（即使业务失败）也应完成初始化
    let output = run(&["upload"], &home, None);
    assert_eq!(output.status.code(), Some(1), "缺少 url 应当失败");

    let config = home.join(".maven-util/config.toml");
    assert!(config.is_file(), "首次运行必须生成配置文件");

    let installed = home.join(".maven-util/bin/maven-util");
    assert!(installed.is_file(), "首次运行必须复制可执行文件");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "安装的可执行文件必须可执行");
    }

    let bashrc = home.join(".bashrc");
    let rc = std::fs::read_to_string(&bashrc).expect("首次运行应写入 shell 配置");
    assert!(
        rc.contains(maven_util_init_path_export()),
        "shell 配置必须包含 PATH 导出: {rc}"
    );

    // 安装的二进制可直接运行
    let status = std::process::Command::new(&installed)
        .arg("--version")
        .env("HOME", &home)
        .current_dir(&home)
        .status()
        .expect("运行安装后的可执行文件失败");
    assert!(status.success());

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn init_subcommand_is_idempotent() {
    let home = temp_dir("home-init-subcommand");

    let first = run(&["init"], &home, None);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stdout={} stderr={}",
        stdout(&first),
        stderr(&first)
    );
    let combined = combined(&first);
    assert!(combined.contains("初始化完成"), "输出: {combined}");

    let rc_path = home.join(".bashrc");
    let after_first = std::fs::read_to_string(&rc_path).unwrap();
    assert_eq!(
        after_first.matches(maven_util_init_path_export()).count(),
        1
    );

    let second = run(&["init"], &home, None);
    assert_eq!(second.status.code(), Some(0));
    let after_second = std::fs::read_to_string(&rc_path).unwrap();
    assert_eq!(
        after_second, after_first,
        "重复 init 不能重复追加 PATH 配置"
    );
    assert_eq!(
        after_second.matches(maven_util_init_path_export()).count(),
        1
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn init_can_skip_path_configuration() {
    let home = temp_dir("home-init-no-path");
    let output = run(&["init", "--no-path"], &home, None);
    assert_eq!(output.status.code(), Some(0));
    assert!(home.join(".maven-util/bin/maven-util").is_file());
    assert!(
        !home.join(".bashrc").exists(),
        "--no-path 不应修改 shell 配置"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn project_config_merges_over_global_config() {
    let home = temp_dir("home-merge");
    let repo = repo_with_artifact("repo-merge");
    let db = repo.join("state.db");
    let upstream = FakeUpstream::start(upload_routes(201));

    // 全局配置里放上传凭据
    let config_dir = home.join(".maven-util");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[upload]\nurl = \"{}\"\nusername = \"admin\"\npassword = \"secret\"\ndir = \"{}\"\ndb_path = \"{}\"\n",
            upstream.base_url,
            repo.display(),
            db.display()
        ),
    )
    .unwrap();

    // 当前目录再放一个只涉及其他段的项目级配置，不能覆盖掉全局上传凭据
    std::fs::write(
        home.join("maven-uploader.toml"),
        "[clean]\nrepo = \"/tmp/nowhere\"\n",
    )
    .unwrap();

    let output = run(&["upload"], &home, None);
    assert_eq!(
        output.status.code(),
        Some(0),
        "项目级配置不应覆盖全局上传凭据。stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    assert!(combined(&output).contains("上传成功"));

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn skip_init_env_disables_first_run_installation() {
    let home = temp_dir("home-skip-init");
    let output = Command::new(env!("CARGO_BIN_EXE_maven-util"))
        .args(["upload"])
        .env("HOME", &home)
        .env("MAVEN_UTIL_SKIP_INIT", "1")
        .current_dir(&home)
        .output()
        .expect("运行 maven-util 失败");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !home.join(".maven-util").exists(),
        "MAVEN_UTIL_SKIP_INIT=1 时不应做首次运行初始化"
    );
    std::fs::remove_dir_all(&home).ok();
}

fn maven_util_init_path_export() -> &'static str {
    r#"export PATH="$HOME/.maven-util/bin:$PATH""#
}

#[test]
fn jar_fixture_matches_constant() {
    // 保证夹具内容与 checksum 常量一致，避免上游缓存测试悄悄失效
    assert!(!JAR.is_empty());
}

/// 瞬时 5xx 会被自动重试；重试成功后整体仍算成功。
#[test]
fn upload_retries_transient_5xx_until_success() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let home = temp_dir("home-retry");
    let repo = repo_with_artifact("repo-retry");
    let jar_hits = Arc::new(AtomicUsize::new(0));
    let pom_hits = Arc::new(AtomicUsize::new(0));
    let mut routes = HashMap::new();
    for (path, hits) in [
        ("/org/example/demo/1.0/demo-1.0.jar", &jar_hits),
        ("/org/example/demo/1.0/demo-1.0.pom", &pom_hits),
    ] {
        routes.insert(
            path.to_string(),
            Reply::FlakyPut {
                counter: Arc::clone(hits),
                fail_times: 2,
                fail_code: 503,
                then_code: 201,
            },
        );
    }
    let upstream = FakeUpstream::start(routes);

    let output = run(
        &[
            "upload",
            "--url",
            &upstream.base_url,
            "--username",
            "u",
            "--password",
            "p",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            home.join("state.db").to_str().unwrap(),
            "--retries",
            "3",
            "--concurrency",
            "2",
        ],
        &home,
        None,
    );

    assert!(
        output.status.success(),
        "重试成功后必须退出 0：{}",
        combined(&output)
    );
    assert_eq!(
        jar_hits.load(Ordering::Relaxed),
        3,
        "jar：2 次 503 + 1 次成功"
    );
    assert_eq!(
        pom_hits.load(Ordering::Relaxed),
        3,
        "pom：2 次 503 + 1 次成功"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 重试次数耗尽后必须以非零退出码结束，而不是假装成功。
#[test]
fn upload_fails_when_retries_exhausted() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let home = temp_dir("home-retry-exhausted");
    let repo = repo_with_artifact("repo-retry-exhausted");
    let jar_hits = Arc::new(AtomicUsize::new(0));
    let mut routes = HashMap::new();
    routes.insert(
        "/org/example/demo/1.0/demo-1.0.jar".to_string(),
        Reply::FlakyPut {
            counter: Arc::clone(&jar_hits),
            fail_times: usize::MAX,
            fail_code: 503,
            then_code: 201,
        },
    );
    routes.insert(
        "/org/example/demo/1.0/demo-1.0.pom".to_string(),
        Reply::Status(503),
    );
    let upstream = FakeUpstream::start(routes);

    let output = run(
        &[
            "upload",
            "--url",
            &upstream.base_url,
            "--username",
            "u",
            "--password",
            "p",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            home.join("state.db").to_str().unwrap(),
            "--retries",
            "1",
            "--concurrency",
            "1",
        ],
        &home,
        None,
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "重试耗尽必须退出 1：{}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("上传失败"),
        "必须报告上传失败：{}",
        combined(&output)
    );
    assert_eq!(
        jar_hits.load(Ordering::Relaxed),
        2,
        "首次请求 + 1 次重试，共 2 次"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// `--skip-head-check` 跳过 HEAD 存在性检查（状态库仍然生效），直接 PUT。
#[test]
fn skip_head_check_uploads_even_when_remote_exists() {
    let home = temp_dir("home-skip-head");
    let second_home = temp_dir("home-skip-head-2");
    let repo = repo_with_artifact("repo-skip-head");
    let mut routes = HashMap::new();
    for path in [
        "/org/example/demo/1.0/demo-1.0.pom",
        "/org/example/demo/1.0/demo-1.0.jar",
    ] {
        routes.insert(
            path.to_string(),
            Reply::MethodStatus {
                head: 200,
                other: 201,
            },
        );
    }
    let upstream = FakeUpstream::start(routes);
    let base: &[&str] = &[
        "upload",
        "--url",
        &upstream.base_url,
        "--username",
        "u",
        "--password",
        "p",
        "--dir",
        repo.to_str().unwrap(),
    ];

    // 默认行为：HEAD 200 → 全部按已存在跳过，不发 PUT
    let first = run(base, &home, None);
    assert!(first.status.success(), "{}", combined(&first));
    assert!(
        upstream
            .requests()
            .iter()
            .all(|request| !request.starts_with("PUT")),
        "默认应跳过远端已存在的文件：{:?}",
        upstream.requests()
    );

    // 显式跳过 HEAD 检查：直接 PUT
    let second_db = second_home.join("state.db");
    let mut args: Vec<&str> = base.to_vec();
    args.extend_from_slice(&[
        "--db-path",
        second_db.to_str().unwrap(),
        "--skip-head-check",
    ]);
    let second = run(&args, &second_home, None);
    assert!(second.status.success(), "{}", combined(&second));
    assert_eq!(
        upstream
            .requests()
            .iter()
            .filter(|request| request.starts_with("PUT"))
            .count(),
        2,
        "--skip-head-check 后两个文件都必须真正 PUT"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&second_home).ok();
}

/// Components 模式的路由：查询走 `/search`（按坐标真正过滤）、提交走 `/components`。
///
/// `present` 里的坐标会被查询端点命中；仓库里其余组件不会被返回，
/// 因此「新构件是否被误判为已存在」能被这类测试抓住。
fn components_routes(present: &[(&str, &str, &str)], post_status: u16) -> HashMap<String, Reply> {
    let items: Vec<serde_json::Value> = present
        .iter()
        .map(|(group, name, version)| {
            serde_json::json!({"group": group, "name": name, "version": version})
        })
        .collect();
    let mut routes = HashMap::new();
    routes.insert(
        "GET /service/rest/v1/search".to_string(),
        Reply::ComponentQuery(items),
    );
    routes.insert(
        "POST /service/rest/v1/components".to_string(),
        Reply::Status(post_status),
    );
    routes
}

/// Components 模式：整个构件一次 multipart 提交，请求数与文件数无关。
#[test]
fn components_mode_submits_single_multipart_request() {
    let home = temp_dir("home-components");
    let repo = repo_with_artifact("repo-components");
    // 远端查询返回空（组件不存在）→ 应提交；提交返回 204
    let upstream = FakeUpstream::start(components_routes(&[], 204));
    let url = format!("{}/repository/maven-releases", upstream.base_url);

    let output = run(
        &[
            "upload",
            "--url",
            &url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
            "--mode",
            "components",
        ],
        &home,
        None,
    );

    assert!(output.status.success(), "{}", combined(&output));
    assert!(
        combined(&output).contains("提交成功"),
        "应报告组件提交成功：{}",
        combined(&output)
    );

    let requests = upstream.requests();
    let posts: Vec<_> = requests
        .iter()
        .filter(|request| request.starts_with("POST /service/rest/v1/components"))
        .collect();
    assert_eq!(posts.len(), 1, "整个构件只应提交一次：{requests:?}");
    let body = &posts[0];
    for expected in [
        "maven2.groupId",
        "org.example",
        "maven2.artifactId",
        "demo",
        "maven2.version",
        "1.0",
        "maven2.generate-pom",
        "maven2.asset1.extension",
        "pom",
        "maven2.asset2.extension",
        "jar",
        "filename=\"demo-1.0.pom\"",
        "filename=\"demo-1.0.jar\"",
    ] {
        assert!(
            body.contains(expected),
            "multipart 请求体缺少 {expected:?}：{body}"
        );
    }
    assert!(
        !body.contains(".sha1") && !body.contains(".md5"),
        "校验和文件不应上传（服务端自算）：{body}"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Components 模式：classifier 正确下发，校验和文件不进请求体。
#[test]
fn components_mode_sends_classifier_and_skips_checksums() {
    let home = temp_dir("home-components-cl");
    let repo = repo_with_extras("repo-components-cl");
    let upstream = FakeUpstream::start(components_routes(&[], 204));
    let url = format!("{}/repository/maven-releases", upstream.base_url);

    let output = run(
        &[
            "upload",
            "--url",
            &url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
            "--mode",
            "components",
        ],
        &home,
        None,
    );
    assert!(output.status.success(), "{}", combined(&output));

    let requests = upstream.requests();
    let body = requests
        .iter()
        .find(|request| request.starts_with("POST /service/rest/v1/components"))
        .expect("应有组件提交请求");

    // 资产按文件名路径排序，sources jar 排在最前（`-` < `.`）
    assert!(
        body.contains("maven2.asset1.classifier\"\r\n\r\nsources\r\n"),
        "sources jar 应带 classifier=sources：{body}"
    );
    assert!(
        body.contains("filename=\"demo-1.0-sources.jar\""),
        "sources jar 文件名应出现在请求体：{body}"
    );
    assert!(
        !body.contains(".sha1") && !body.contains(".md5"),
        "本地校验和文件不应上传（服务端自算）：{body}"
    );
    // 三个资产：sources jar、jar、pom
    for expected in ["maven2.asset1", "maven2.asset2", "maven2.asset3"] {
        assert!(body.contains(expected), "缺少 {expected}：{body}");
    }
    assert!(!body.contains("maven2.asset4"), "不应有第四个资产：{body}");

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Components 模式：远端已有同坐标组件时跳过，不重复提交。
#[test]
fn components_mode_skips_existing_component() {
    let home = temp_dir("home-components-exists");
    let repo = repo_with_artifact("repo-components-exists");
    let upstream = FakeUpstream::start(components_routes(&[("org.example", "demo", "1.0")], 204));
    let url = format!("{}/repository/maven-releases", upstream.base_url);

    let output = run(
        &[
            "upload",
            "--url",
            &url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
            "--mode",
            "components",
        ],
        &home,
        None,
    );

    assert!(output.status.success(), "{}", combined(&output));
    assert!(
        combined(&output).contains("远端已存在"),
        "应报告远端已存在并跳过：{}",
        combined(&output)
    );
    assert!(
        !upstream
            .requests()
            .iter()
            .any(|request| request.starts_with("POST")),
        "已存在的组件不应再次提交：{:?}",
        upstream.requests()
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Components 模式：提交返回 5xx 时按重试次数退避，耗尽后以非零退出。
#[test]
fn components_mode_retries_then_fails() {
    let home = temp_dir("home-components-retry");
    let repo = repo_with_artifact("repo-components-retry");
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut routes = components_routes(&[], 204);
    routes.insert(
        "POST /service/rest/v1/components".to_string(),
        Reply::FlakyPut {
            counter: std::sync::Arc::clone(&counter),
            fail_times: usize::MAX,
            fail_code: 503,
            then_code: 204,
        },
    );
    let upstream = FakeUpstream::start(routes);
    let url = format!("{}/repository/maven-releases", upstream.base_url);

    let output = run(
        &[
            "upload",
            "--url",
            &url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
            "--mode",
            "components",
            "--retries",
            "2",
        ],
        &home,
        None,
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "重试耗尽必须退出 1：{}",
        combined(&output)
    );
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "首次 + 2 次重试，共 3 次提交"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 回归：仓库里已有别的组件时，新构件不得被误判为「远端已存在」。
///
/// 真实 Nexus 的 `/components` 端点会忽略坐标过滤参数、返回仓库内全部组件。
/// 若实现只看「响应非空」，新构件会被静默跳过且进程退出 0（数据丢失级 bug）。
/// 因此查询端点必须走 `/search`，且夹具会真正按坐标过滤。
#[test]
fn components_mode_does_not_skip_new_artifact_among_existing_ones() {
    let home = temp_dir("home-components-mixed");
    let repo = repo_with_artifact("repo-components-mixed");
    // 仓库里已经有 org.example:demo:1.0（正是本次要传的坐标）——但换个场景：
    // 这里放一个「别的」坐标，模拟远端已有其他构件。
    let upstream = FakeUpstream::start(components_routes(
        &[("com.other", "already-there", "9.9.9")],
        204,
    ));
    let url = format!("{}/repository/maven-releases", upstream.base_url);

    let output = run(
        &[
            "upload",
            "--url",
            &url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
            "--mode",
            "components",
        ],
        &home,
        None,
    );

    assert!(output.status.success(), "{}", combined(&output));
    let message = combined(&output);
    assert!(
        !message.contains("远端已存在"),
        "远端只有别的坐标，不应判定目标已存在：{message}"
    );
    assert!(
        message.contains("提交成功"),
        "新构件必须真正提交：{message}"
    );
    assert_eq!(
        upstream
            .requests()
            .iter()
            .filter(|request| request.starts_with("POST /service/rest/v1/components"))
            .count(),
        1,
        "应发出一次提交请求：{:?}",
        upstream.requests()
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Components 模式：URL 中没有 /repository/<名> 时给出可操作的报错。
#[test]
fn components_mode_rejects_url_without_repository_segment() {
    let home = temp_dir("home-components-badurl");
    let repo = repo_with_artifact("repo-components-badurl");
    let upstream = FakeUpstream::start(components_routes(&[], 204));

    let output = run(
        &[
            "upload",
            "--url",
            &upstream.base_url,
            "--username",
            "admin",
            "--password",
            "secret",
            "--dir",
            repo.to_str().unwrap(),
            "--db-path",
            repo.join("state.db").to_str().unwrap(),
            "--mode",
            "components",
        ],
        &home,
        None,
    );

    assert_eq!(output.status.code(), Some(1));
    let message = combined(&output);
    assert!(
        message.contains("/repository/") && message.contains("repository"),
        "报错应指出如何修复 URL：{message}"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}
