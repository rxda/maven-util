//! 初始化：首次使用时把可执行文件安装到 `~/.maven-util/bin`，并把该目录
//! 加入 shell 的 PATH。
//!
//! - 首次运行（`~/.maven-util/config.toml` 尚不存在时）自动执行一次
//! - `maven-util init` 可随时手动重跑，整个过程幂等
//! - `--dry-run` 与环境变量 `MAVEN_UTIL_SKIP_INIT` 会跳过初始化

use crate::config;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// 安装目录名。
pub const BIN_DIR_NAME: &str = "bin";
/// 写入 shell 配置文件的标记注释，用于幂等判断与排查。
pub const PATH_MARKER: &str = "# added by maven-util init";
/// 写入 shell 配置文件的 PATH 行。
pub const PATH_EXPORT: &str = r#"export PATH="$HOME/.maven-util/bin:$PATH""#;

/// 初始化选项。
#[derive(Debug, Clone, Copy)]
pub struct InitOptions {
    /// 是否把当前可执行文件复制到 `~/.maven-util/bin`。
    pub install_binary: bool,
    /// 是否修改 shell 配置文件把安装目录加入 PATH。
    pub configure_path: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            install_binary: true,
            configure_path: true,
        }
    }
}

/// 初始化结果，用于输出汇总。
#[derive(Debug, Default)]
pub struct InitReport {
    /// 本次是否新建了配置文件。
    pub config_created: bool,
    /// 新安装（或更新）的可执行文件路径。
    pub binary_installed: Option<PathBuf>,
    /// 新写入 PATH 的 shell 配置文件。
    pub path_files: Vec<PathBuf>,
}

/// 可执行文件安装路径：`~/.maven-util/bin/maven-util[.exe]`。
pub fn installed_binary() -> Option<PathBuf> {
    let mut path = config::config_dir()?.join(BIN_DIR_NAME);
    path.push(exe_name());
    Some(path)
}

fn exe_name() -> String {
    format!("maven-util{}", std::env::consts::EXE_SUFFIX)
}

/// 执行初始化。显式调用（`maven-util init`）时错误会向外传播；
/// 首次运行的自动初始化用 [`auto_first_run`]，失败只告警。
pub fn run_init(options: InitOptions) -> Result<InitReport> {
    let mut report = InitReport {
        config_created: config::ensure_default_config().is_some(),
        ..InitReport::default()
    };
    if options.install_binary {
        report.binary_installed = install_binary()?;
    }
    if options.configure_path {
        report.path_files = configure_path()?;
    }
    Ok(report)
}

/// 首次运行时的自动初始化：返回配置文件是否为本次新建。
pub fn auto_first_run() -> bool {
    if config::ensure_default_config().is_none() {
        return false;
    }
    // 上面已创建配置文件，这里直接安装可执行文件并配置 PATH
    let outcome = (|| -> Result<InitReport> {
        Ok(InitReport {
            config_created: true,
            binary_installed: install_binary()?,
            path_files: configure_path()?,
        })
    })();
    match outcome {
        Ok(report) => {
            print_report(&report, "首次运行初始化");
            true
        }
        Err(error) => {
            eprintln!("首次运行初始化失败（不影响本次执行）: {error:#}");
            false
        }
    }
}

/// 打印初始化汇总。
pub fn print_report(report: &InitReport, title: &str) {
    println!("{title}:");
    if report.config_created
        && let Some(path) = config::default_config_path()
    {
        println!("  [+] 配置文件: {}", path.display());
    }
    match &report.binary_installed {
        Some(path) => println!("  [+] 可执行文件: {}", path.display()),
        None => println!("  [-] 可执行文件已在安装位置，无需复制"),
    }
    if report.path_files.is_empty() {
        println!("  [-] PATH 已包含安装目录，无需修改");
        print_path_hint();
    } else {
        for file in &report.path_files {
            println!("  [+] PATH 配置: {}", file.display());
        }
        println!(
            "  请重新打开终端，或执行 `source {}` 使 PATH 生效",
            report.path_files[0].display()
        );
    }
}

fn print_path_hint() {
    #[cfg(windows)]
    println!(
        "  Windows 请手动把 {} 加入用户 PATH（setx PATH \"%USERPROFILE%\\.maven-util\\bin;%PATH%\"）",
        BIN_DIR_NAME
    );
}

/// 把当前可执行文件复制到安装位置，内容一致时跳过。
fn install_binary() -> Result<Option<PathBuf>> {
    let config_dir = config::config_dir().context("无法确定用户主目录")?;
    let bin_dir = config_dir.join(BIN_DIR_NAME);
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("无法创建安装目录: {}", bin_dir.display()))?;

    let target = bin_dir.join(exe_name());
    let source = std::env::current_exe().context("无法确定当前可执行文件路径")?;

    if is_same_file(&source, &target) {
        return Ok(None);
    }
    if target.is_file() && files_identical(&source, &target).unwrap_or(false) {
        return Ok(None);
    }

    fs::copy(&source, &target)
        .with_context(|| format!("无法复制 {} 到 {}", source.display(), target.display()))?;
    set_executable(&target);
    Ok(Some(target))
}

/// 把安装目录追加进 shell 配置文件的 PATH，已存在则跳过。
fn configure_path() -> Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        // Windows 用户 PATH 需要写注册表，这里只给出手工命令，不做静默修改。
        return Ok(Vec::new());
    }

    #[cfg(not(windows))]
    {
        let home = config::home_dir().context("无法确定用户主目录")?;
        let mut candidates: Vec<PathBuf> = [".bashrc", ".zshrc", ".profile"]
            .iter()
            .map(|name| home.join(name))
            .filter(|path| path.exists())
            .collect();
        if candidates.is_empty() {
            candidates.push(home.join(".bashrc"));
        }

        let mut updated = Vec::new();
        for file in candidates {
            let mut content = fs::read_to_string(&file).unwrap_or_default();
            if content.contains(PATH_EXPORT) {
                continue;
            }
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("\n{PATH_MARKER}\n{PATH_EXPORT}\n"));
            fs::write(&file, content).with_context(|| format!("无法写入 {}", file.display()))?;
            updated.push(file);
        }
        Ok(updated)
    }
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 逐块比较两个文件内容是否一致。
fn files_identical(a: &Path, b: &Path) -> Result<bool> {
    let mut left = fs::File::open(a)?;
    let mut right = fs::File::open(b)?;
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    let mut left_buffer = vec![0_u8; 64 * 1024];
    let mut right_buffer = vec![0_u8; 64 * 1024];
    loop {
        let left_read = read_full(&mut left, &mut left_buffer)?;
        let right_read = read_full(&mut right, &mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn read_full(reader: &mut fs::File, buffer: &mut [u8]) -> Result<usize> {
    use std::io::Read;

    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_lines_are_stable() {
        assert!(PATH_EXPORT.contains(".maven-util/bin"));
        assert!(PATH_MARKER.starts_with('#'));
    }

    #[test]
    fn detects_identical_and_different_files() {
        let dir = std::env::temp_dir().join(format!(
            "maven-util-init-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        let b = dir.join("b");
        fs::write(&a, b"same content").unwrap();
        fs::write(&b, b"same content").unwrap();
        assert!(files_identical(&a, &b).unwrap());
        fs::write(&b, b"other content!").unwrap();
        assert!(!files_identical(&a, &b).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
