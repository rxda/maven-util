//! 快速清理 Maven 仓库中的客户端状态文件。

use anyhow::{Context, Result, anyhow};
use jwalk::WalkDir;
use rayon::prelude::*;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct CleanupArgs {
    pub repo: PathBuf,
    pub dry_run: bool,
}

pub fn run_cleanup(args: CleanupArgs) -> Result<()> {
    let repo = fs::canonicalize(&args.repo)
        .with_context(|| format!("Maven 仓库目录不存在: {}", args.repo.display()))?;
    if !repo.is_dir() {
        return Err(anyhow!("Maven 仓库路径不是目录: {}", repo.display()));
    }

    let mut files = Vec::new();
    collect_files(&repo, &mut files)?;
    files.par_iter().try_for_each(|path| -> Result<()> {
        if args.dry_run {
            println!("将删除: {}", path.display());
        } else {
            fs::remove_file(path).with_context(|| format!("无法删除文件: {}", path.display()))?;
            println!("已删除: {}", path.display());
        }
        Ok(())
    })?;
    println!(
        "{} {} 个 Maven 状态文件",
        if args.dry_run { "找到" } else { "清理" },
        files.len()
    );
    Ok(())
}

fn collect_files(directory: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    *result = WalkDir::new(directory)
        .parallelism(jwalk::Parallelism::RayonDefaultPool {
            busy_timeout: Duration::from_secs(1),
        })
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path())
        .filter(|path| is_cleanup_file(path))
        .collect();
    Ok(())
}

fn is_cleanup_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name == "_remote.repositories" || name.ends_with(".lastUpdated")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_maven_state_files() {
        assert!(is_cleanup_file(Path::new("_remote.repositories")));
        assert!(is_cleanup_file(Path::new("artifact.pom.lastUpdated")));
        assert!(!is_cleanup_file(Path::new("artifact.pom")));
        assert!(!is_cleanup_file(Path::new("artifact.jar")));
    }

    #[test]
    fn run_cleanup_removes_only_state_files() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let root = std::env::temp_dir().join(format!(
            "maven-util-cleanup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let artifact_dir = root.join("org/example/demo/1.0");
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::write(artifact_dir.join("demo-1.0.jar"), b"jar").unwrap();
        fs::write(artifact_dir.join("_remote.repositories"), b"remote").unwrap();
        fs::write(artifact_dir.join("demo-1.0.jar.lastUpdated"), b"update").unwrap();

        run_cleanup(CleanupArgs {
            repo: root.clone(),
            dry_run: false,
        })
        .unwrap();

        assert!(artifact_dir.join("demo-1.0.jar").is_file());
        assert!(!artifact_dir.join("_remote.repositories").exists());
        assert!(!artifact_dir.join("demo-1.0.jar.lastUpdated").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
