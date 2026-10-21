//! Nexus 3 Components API 上传路径（`mode = "components"`）。
//!
//! PUT 模式对每个文件各发一次请求（HEAD + PUT），而 Components API 允许
//! **一个请求提交整个构件**：jar + pom + sources + javadoc 一起发，
//! 由服务端计算校验和。一个构件从 `文件数 × 2` 次往返降到 1 次。
//!
//! 端点：`POST {base}/service/rest/v1/components?repository=<仓库名>`，
//! 请求体为 `multipart/form-data`，字段见 <https://help.sonatype.com/en/components-api.html>。
//!
//! ## 与 PUT 模式的取舍
//!
//! - 只对 Nexus 3 有意义；Artifactory / 通用 raw 仓库没有这套 API，
//!   所以 PUT 模式保留为默认。
//! - 不使用 `maven2.generate-pom`：生成的是只有坐标的空壳 POM，
//!   会丢掉依赖信息，下游消费时引发 `NoClassDefFoundError`。
//!   真实 POM 作为资产上传；即使构件确实没有 POM，也只影响它自己。
//! - 校验和文件（`.sha1`/`.md5`/…）不再上传——服务端自己算，
//!   本地那份是多余的。

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::multipart::{Form, Part};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use std::path::PathBuf;

use crate::checksum;
use crate::upload::{MavenArtifact, fingerprint};

/// Components API 的目标：服务根地址 + 仓库名。
///
/// 该 API 挂在服务根（`/service/rest/v1/...`）而不是仓库路径上，
/// 因此必须把 PUT 模式那个仓库 URL 拆成两段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentsTarget {
    /// 服务根地址，例如 `https://nexus.example.com`。
    pub base_url: String,
    /// 仓库名，例如 `maven-releases`。
    pub repository: String,
}

impl ComponentsTarget {
    /// 从形如 `https://host[:port]/repository/<name>` 的仓库 URL 解析。
    ///
    /// 兼容结尾斜杠；URL 中出现多次 `/repository/` 时以最后一段为准
    /// （支持反向代理把服务挂在路径前缀下的情况）。
    pub fn from_repository_url(url: &str) -> Result<Self> {
        let trimmed = url.trim_end_matches('/');
        let (base, repository) = trimmed
            .rsplit_once("/repository/")
            .ok_or_else(|| anyhow!("URL 中没有 `/repository/<仓库名>` 段，无法推断仓库名"))?;
        if repository.is_empty() || repository.contains('/') {
            return Err(anyhow!("无法从 URL 解析出仓库名: {url}"));
        }
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(anyhow!("URL 缺少 http(s) 协议前缀: {url}"));
        }
        Ok(Self {
            base_url: base.to_string(),
            repository: repository.to_string(),
        })
    }

    /// 显式给定 base 与仓库名（配置了 `repository` 时使用）。
    pub fn from_base_and_repository(base: &str, repository: &str) -> Result<Self> {
        let base = base.trim_end_matches('/');
        if repository.is_empty() || repository.contains('/') {
            return Err(anyhow!("仓库名无效: {repository:?}"));
        }
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(anyhow!("服务根地址缺少 http(s) 协议前缀: {base}"));
        }
        Ok(Self {
            base_url: base.to_string(),
            repository: repository.to_string(),
        })
    }

    /// 组件提交端点。
    pub fn components_url(&self) -> String {
        format!("{}/service/rest/v1/components", self.base_url)
    }

    /// 存在性查询端点（带 repository/group/name/version 过滤）。
    pub fn query_url(&self, group_id: &str, artifact_id: &str, version: &str) -> String {
        format!(
            "{}?repository={}&group={}&name={}&version={}",
            self.components_url(),
            self.repository,
            group_id,
            artifact_id,
            version
        )
    }
}

/// 一个待上传的资产（对应一个 `maven2.assetN` 字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentAsset {
    pub path: PathBuf,
    /// Maven 扩展名，例如 `jar` / `pom` / `module`。
    pub extension: String,
    /// classifier，例如 `sources` / `javadoc`；没有则为 `None`。
    pub classifier: Option<String>,
}

fn is_checksum_extension(extension: &str) -> bool {
    extension.ends_with("sha1")
        || extension.ends_with("md5")
        || extension.ends_with("sha256")
        || extension.ends_with("sha512")
}

/// 从构件目录里的文件收集待上传资产，并按路径排序（保证指纹稳定）。
///
/// 文件名形如 `{artifactId}-{version}.{ext}` 或 `{artifactId}-{version}-{classifier}.{ext}`；
/// 校验和文件一律跳过——Components API 由服务端计算。
pub fn collect_assets(artifact: &MavenArtifact) -> Vec<ComponentAsset> {
    let prefix = format!("{}-{}", artifact.artifact_id, artifact.version);
    let mut assets = Vec::new();
    for (path, _) in &artifact.files {
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(remainder) = name.strip_prefix(&prefix) else {
            continue;
        };
        let (classifier, extension) = if let Some(extension) = remainder.strip_prefix('.') {
            (None, extension.to_string())
        } else if let Some(rest) = remainder.strip_prefix('-') {
            // `{prefix}-sources.jar` → classifier=sources, extension=jar
            match rest.split_once('.') {
                Some((classifier, extension)) if !classifier.is_empty() => {
                    (Some(classifier.to_string()), extension.to_string())
                }
                _ => continue,
            }
        } else {
            continue;
        };
        if extension.is_empty() || is_checksum_extension(&extension) {
            continue;
        }
        assets.push(ComponentAsset {
            path: path.clone(),
            extension,
            classifier,
        });
    }
    assets.sort_by(|left, right| left.path.cmp(&right.path));
    assets
}

/// 组件指纹：各资产指纹按路径排序后拼接、取 SHA-1。
///
/// Components API 是整组件一次提交，任一资产变化都需要重传整个组件，
/// 所以状态库按组件记录一个聚合指纹，而不是逐文件记录。
pub fn component_fingerprint(assets: &[ComponentAsset]) -> Result<String> {
    let mut parts = Vec::with_capacity(assets.len());
    for asset in assets {
        let name = asset
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let single = fingerprint(&asset.path)
            .with_context(|| format!("无法计算指纹: {}", asset.path.display()))?;
        parts.push(format!("{name}:{single}"));
    }
    parts.sort();
    Ok(checksum::sha1_hex_bytes(parts.join("\n").as_bytes()))
}

/// 状态库 key：组件的逻辑 URL，与 PUT 模式的逐文件 key 不会冲突。
pub fn component_key(target: &ComponentsTarget, artifact: &MavenArtifact) -> String {
    format!(
        "{}/service/rest/v1/components?repository={}&group={}&name={}&version={}",
        target.base_url,
        target.repository,
        artifact.group_id,
        artifact.artifact_id,
        artifact.version
    )
}

/// 构造组件的 multipart 表单。
///
/// 资产编号从 1 开始，与官方文档示例一致（`maven2.asset1`、`maven2.asset2`…）。
pub fn build_form(artifact: &MavenArtifact, assets: &[ComponentAsset]) -> Result<Form> {
    let mut form = Form::new()
        .text("maven2.groupId", artifact.group_id.clone())
        .text("maven2.artifactId", artifact.artifact_id.clone())
        .text("maven2.version", artifact.version.clone())
        // 明确关闭占位 POM 生成：真实 POM 作为资产上传，绝不替换成空壳。
        .text("maven2.generate-pom", "false");
    for (index, asset) in assets.iter().enumerate() {
        let field = index + 1;
        let part = Part::file(&asset.path)
            .with_context(|| format!("无法读取文件: {}", asset.path.display()))?;
        form = form.part(format!("maven2.asset{field}"), part).text(
            format!("maven2.asset{field}.extension"),
            asset.extension.clone(),
        );
        if let Some(classifier) = &asset.classifier {
            form = form.text(
                format!("maven2.asset{field}.classifier"),
                classifier.clone(),
            );
        }
    }
    Ok(form)
}

/// Components 查询返回的分页结构（只关心 `items` 是否为空）。
#[derive(Debug, Deserialize)]
struct ComponentsPage {
    #[serde(default)]
    items: Vec<serde_json::Value>,
}

/// 查询远端是否已存在同坐标的组件。
pub fn component_exists(
    client: &Client,
    target: &ComponentsTarget,
    artifact: &MavenArtifact,
    username: &str,
    password: &str,
) -> Result<bool> {
    let url = target.query_url(&artifact.group_id, &artifact.artifact_id, &artifact.version);
    let response: Response = client
        .get(&url)
        .basic_auth(username, Some(password))
        .send()
        .with_context(|| format!("查询组件失败: {url}"))?;
    if !response.status().is_success() {
        return Err(anyhow!("查询组件返回 HTTP {}", response.status()));
    }
    let page: ComponentsPage = response.json().context("无法解析 Components 查询响应")?;
    Ok(!page.items.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn artifact(files: &[(&str, &str)]) -> MavenArtifact {
        MavenArtifact {
            group_id: "org.example".into(),
            artifact_id: "demo".into(),
            version: "1.0".into(),
            files: files
                .iter()
                .map(|(name, ext)| {
                    (
                        PathBuf::from(format!("/repo/org/example/demo/1.0/{name}")),
                        (*ext).to_string(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn parses_repository_url() {
        let target = ComponentsTarget::from_repository_url(
            "https://nexus.example.com/repository/maven-releases",
        )
        .unwrap();
        assert_eq!(target.base_url, "https://nexus.example.com");
        assert_eq!(target.repository, "maven-releases");

        // 结尾斜杠
        let target = ComponentsTarget::from_repository_url(
            "https://nexus.example.com:8081/repository/snapshots/",
        )
        .unwrap();
        assert_eq!(target.base_url, "https://nexus.example.com:8081");
        assert_eq!(target.repository, "snapshots");

        // 反向代理路径前缀 + 多个 /repository/ 段：取最后一段
        let target = ComponentsTarget::from_repository_url(
            "https://example.com/nexus/repository/maven-releases",
        )
        .unwrap();
        assert_eq!(target.base_url, "https://example.com/nexus");
        assert_eq!(target.repository, "maven-releases");
    }

    #[test]
    fn rejects_url_without_repository_segment() {
        assert!(ComponentsTarget::from_repository_url("https://nexus.example.com").is_err());
        assert!(
            ComponentsTarget::from_repository_url("https://nexus.example.com/repository/").is_err()
        );
        // 缺协议
        assert!(
            ComponentsTarget::from_repository_url("nexus.example.com/repository/releases").is_err()
        );
    }

    #[test]
    fn builds_explicit_target() {
        let target = ComponentsTarget::from_base_and_repository(
            "https://nexus.example.com/",
            "maven-releases",
        )
        .unwrap();
        assert_eq!(
            target.components_url(),
            "https://nexus.example.com/service/rest/v1/components"
        );
        assert_eq!(
            target.query_url("org.example", "demo", "1.0"),
            "https://nexus.example.com/service/rest/v1/components?repository=maven-releases&group=org.example&name=demo&version=1.0"
        );
        assert!(ComponentsTarget::from_base_and_repository("https://x/", "").is_err());
        assert!(ComponentsTarget::from_base_and_repository("ftp://x/", "r").is_err());
    }

    #[test]
    fn collects_assets_with_classifier_and_skips_checksums() {
        let artifact = artifact(&[
            ("demo-1.0.pom", "pom"),
            ("demo-1.0.jar", "jar"),
            ("demo-1.0-sources.jar", "sources.jar"),
            ("demo-1.0-javadoc.jar", "javadoc.jar"),
            ("demo-1.0.module", "module"),
            ("demo-1.0.jar.sha1", "jar.sha1"),
            ("demo-1.0.pom.md5", "pom.md5"),
            ("demo-1.0.jar.sha256", "jar.sha256"),
        ]);
        let assets = collect_assets(&artifact);
        let extensions: Vec<_> = assets
            .iter()
            .map(|asset| (asset.extension.as_str(), asset.classifier.as_deref()))
            .collect();
        assert_eq!(
            extensions,
            vec![
                ("jar", Some("javadoc")),
                ("jar", Some("sources")),
                ("jar", None),
                ("module", None),
                ("pom", None),
            ],
            "应跳过全部校验和文件，并解析出 classifier；结果按路径排序（`-` < `.`）"
        );
    }

    #[test]
    fn fingerprint_changes_with_any_asset() {
        let dir = std::env::temp_dir().join(format!(
            "maven-util-cfpr-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo-1.0.pom"), b"<project/>").unwrap();
        std::fs::write(dir.join("demo-1.0.jar"), b"jar").unwrap();

        let artifact = artifact(&[("demo-1.0.pom", "pom"), ("demo-1.0.jar", "jar")]);
        let base: Vec<ComponentAsset> = collect_assets(&artifact)
            .into_iter()
            .map(|mut asset| {
                asset.path = dir.join(asset.path.file_name().unwrap());
                asset
            })
            .collect();

        let first = component_fingerprint(&base).unwrap();
        assert_eq!(
            first,
            component_fingerprint(&base).unwrap(),
            "相同输入指纹应稳定"
        );

        // 新增一个资产 → 指纹必须变化（整个组件需要重传）
        let mut extended = base.clone();
        std::fs::write(dir.join("demo-1.0-sources.jar"), b"src").unwrap();
        extended.push(ComponentAsset {
            path: dir.join("demo-1.0-sources.jar"),
            extension: "jar".into(),
            classifier: Some("sources".into()),
        });
        assert_ne!(first, component_fingerprint(&extended).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }
}
