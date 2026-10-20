//! 真实 Maven 构建的端到端测试。
//!
//! 需要本机存在 Maven 与 Java；缺失时打印原因并跳过（不算失败）。
//! 可用环境变量指定 Maven 家目录：`MAVEN_UTIL_TEST_MAVEN_HOME=/opt/maven`。
//! 设置 `MAVEN_UTIL_SKIP_E2E=1` 可强制跳过。

mod common;

use common::temp_dir;
use std::path::{Path, PathBuf};
use std::process::Command;

const POM: &str = r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0</version>
  <properties>
    <maven.compiler.release>17</maven.compiler.release>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
  </properties>
</project>
"#;

fn maven_executable_name() -> &'static str {
    if cfg!(windows) { "mvn.cmd" } else { "mvn" }
}

fn find_maven() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("MAVEN_UTIL_TEST_MAVEN_HOME") {
        let candidate = PathBuf::from(home)
            .join("bin")
            .join(maven_executable_name());
        return candidate.is_file().then_some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(maven_executable_name()))
        .find(|candidate| candidate.is_file())
}

fn java_available() -> bool {
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let candidate =
            Path::new(&home)
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
        if candidate.is_file() {
            return true;
        }
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "java.exe" } else { "java" }))
        .any(|candidate| candidate.is_file())
}

#[test]
fn download_builds_real_maven_project() {
    if std::env::var_os("MAVEN_UTIL_SKIP_E2E").is_some() {
        eprintln!("跳过 Maven 端到端测试：MAVEN_UTIL_SKIP_E2E 已设置");
        return;
    }
    let Some(maven) = find_maven() else {
        eprintln!("跳过 Maven 端到端测试：未找到 mvn（可用 MAVEN_UTIL_TEST_MAVEN_HOME 指定）");
        return;
    };
    if !java_available() {
        eprintln!("跳过 Maven 端到端测试：未找到 Java");
        return;
    }

    let home = temp_dir("home-e2e");
    let project = temp_dir("project-e2e");
    // mirror 仓库复用 target/ 下的持久目录：首次运行下载依赖，之后的运行
    // 直接命中本地缓存，避免每次端到端测试都重新拉取全部插件。
    let server_repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("maven-e2e")
        .join("mirror-repo");
    std::fs::create_dir_all(&server_repo).expect("创建 mirror 仓库目录失败");
    let download_repo = project.join("maven-repo");
    std::fs::write(project.join("pom.xml"), POM).expect("写入 pom.xml 失败");

    let output = Command::new(env!("CARGO_BIN_EXE_maven-util"))
        .args([
            "download",
            project.to_str().unwrap(),
            "--download-repo",
            download_repo.to_str().unwrap(),
            "--server-repo",
            server_repo.to_str().unwrap(),
            "--maven",
            maven.to_str().unwrap(),
            "--skip-tests",
        ])
        .env("HOME", &home)
        .output()
        .expect("运行 maven-util download 失败");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Maven 构建必须成功。stdout={stdout}\nstderr={stderr}"
    );

    let plugins = download_repo.join("org/apache/maven/plugins");
    assert!(
        plugins.exists(),
        "插件必须被下载到依赖仓库: {}",
        plugins.display()
    );
    assert!(
        contains_state_files(&download_repo).is_none(),
        "依赖仓库中不能残留 Maven 客户端状态文件"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&project).ok();
    // server_repo 是可复用的缓存目录，保留供下次运行
}

/// 返回第一个残留的 `_remote.repositories` / `*.lastUpdated` 文件路径。
fn contains_state_files(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name == "_remote.repositories" || name.ends_with(".lastUpdated") {
                return Some(path);
            }
        }
    }
    None
}
