//! `maven-metadata.xml` 生成。
//!
//! 本地仓库缺少元数据文件时，根据目录结构生成基础元数据：
//! - 版本目录（`.../artifact/version/`）：生成版本列表元数据
//! - 版本目录本身（`.../group/artifact/version/`）：生成 snapshot 元数据

use std::fs;
use std::path::Path;

use super::http::Response;
use super::repository::{Repository, ResolveError, ResolveResult};

impl Repository {
    /// 根据仓库目录结构生成 `maven-metadata.xml`。
    pub(super) fn generate_metadata(&self, components: &[String]) -> ResolveResult<Response> {
        if components.is_empty() {
            return Err(ResolveError::BadPath);
        }
        let directory_components = &components[..components.len() - 1];
        let directory = self.safe_path(directory_components)?;
        if !directory.is_dir() {
            return Err(ResolveError::NotFound);
        }

        let child_directories = fs::read_dir(&directory)
            .map_err(|error| ResolveError::internal("读取 Maven 仓库目录失败", error))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                let name = entry.file_name().into_string().ok()?;
                (metadata.is_dir() && !name.starts_with('.')).then_some(name)
            })
            .collect::<Vec<_>>();

        let xml = if !child_directories.is_empty() {
            artifact_metadata(directory_components, child_directories)
        } else if directory_components.len() >= 3 {
            version_metadata(directory_components, &directory)?
        } else {
            return Err(ResolveError::NotFound);
        };

        Ok(Response::xml(xml))
    }
}

/// artifact 级元数据：列出所有版本。
fn artifact_metadata(directory: &[String], mut versions: Vec<String>) -> String {
    versions.sort();
    let artifact_id = directory.last().map(String::as_str).unwrap_or_default();
    let group_id = directory[..directory.len().saturating_sub(1)].join(".");
    let latest = versions.last().map(String::as_str).unwrap_or_default();
    let versions_xml = versions
        .iter()
        .map(|version| format!("<version>{}</version>", xml_escape(version)))
        .collect::<String>();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <versioning>\n    <latest>{}</latest>\n    <release>{}</release>\n    <versions>{}</versions>\n    <lastUpdated>19700101000000</lastUpdated>\n  </versioning>\n</metadata>\n",
        xml_escape(&group_id),
        xml_escape(artifact_id),
        xml_escape(latest),
        xml_escape(latest),
        versions_xml
    )
}

/// snapshot 级元数据：列出版本目录中的所有构件文件。
fn version_metadata(directory: &[String], version_directory: &Path) -> ResolveResult<String> {
    let artifact_id = &directory[directory.len() - 2];
    let version = &directory[directory.len() - 1];
    let group_id = directory[..directory.len() - 2].join(".");
    let prefix = format!("{artifact_id}-{version}");
    let mut snapshot_versions = Vec::new();

    for entry in fs::read_dir(version_directory)
        .map_err(|error| ResolveError::internal("读取 Maven 版本目录失败", error))?
    {
        let entry =
            entry.map_err(|error| ResolveError::internal("读取 Maven 版本目录失败", error))?;
        if !entry
            .file_type()
            .map_err(|error| ResolveError::internal("读取 Maven 版本目录失败", error))?
            .is_file()
            || super::http::ignored_file(&entry.path())
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let (classifier, extension) = if let Some(extension) = suffix.strip_prefix('.') {
            (None, extension)
        } else if let Some(suffix) = suffix.strip_prefix('-') {
            let Some((classifier, extension)) = suffix.rsplit_once('.') else {
                continue;
            };
            (Some(classifier), extension)
        } else {
            continue;
        };
        if extension.is_empty() || classifier.is_some_and(str::is_empty) {
            continue;
        }
        let classifier_xml = classifier.map_or_else(String::new, |classifier| {
            format!("<classifier>{}</classifier>", xml_escape(classifier))
        });
        snapshot_versions.push(format!(
            "<snapshotVersion><extension>{}</extension>{}<value>{}</value><updated>19700101000000</updated></snapshotVersion>",
            xml_escape(extension),
            classifier_xml,
            xml_escape(version)
        ));
    }
    snapshot_versions.sort();
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <version>{}</version>\n  <versioning>\n    <snapshot><localCopy>true</localCopy></snapshot>\n    <lastUpdated>19700101000000</lastUpdated>\n    <snapshotVersions>{}</snapshotVersions>\n  </versioning>\n</metadata>\n",
        xml_escape(&group_id),
        xml_escape(artifact_id),
        xml_escape(version),
        snapshot_versions.join("")
    ))
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
    fn escapes_generated_xml() {
        assert_eq!(xml_escape("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
    }

    #[test]
    fn artifact_metadata_lists_versions() {
        let xml = artifact_metadata(
            &["org".into(), "example".into(), "demo".into()],
            vec!["1.0".into(), "1.1".into()],
        );
        assert!(xml.contains("<artifactId>demo</artifactId>"));
        assert!(xml.contains("<groupId>org.example</groupId>"));
        assert!(xml.contains("<latest>1.1</latest>"));
        assert!(xml.contains("<version>1.0</version>"));
    }
}
