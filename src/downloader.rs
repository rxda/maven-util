//! 通过内置 Maven mirror 执行完整构建，并补齐 POM 中声明的插件。

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::server::{self, CacheLimits, ServerArgs, ServerHandle};

#[derive(Debug, Clone)]
pub struct DownloadArgs {
    pub workdir: PathBuf,
    pub download_repo: PathBuf,
    pub server_repo: PathBuf,
    pub upstreams: Vec<String>,
    pub maven: Option<PathBuf>,
    pub maven_args: Vec<String>,
    pub dry_run: bool,
    pub cache: CacheLimits,
    pub client: Arc<Client>,
}

pub fn run_download(args: DownloadArgs) -> Result<()> {
    let workdir = fs::canonicalize(&args.workdir)
        .with_context(|| format!("Maven 运行目录不存在: {}", args.workdir.display()))?;
    if !workdir.is_dir() {
        return Err(anyhow!("Maven 运行路径不是目录: {}", workdir.display()));
    }
    if args.upstreams.is_empty() {
        return Err(anyhow!("至少需要一个 --upstream 上游仓库"));
    }

    // 这些路径在真正执行前只做解析，不创建任何目录。
    let download_repo = absolute_path(&args.download_repo, &workdir);
    let server_repo = absolute_path(&args.server_repo, &workdir);
    let maven = resolve_maven(args.maven.clone(), &workdir);

    // --dry-run 只打印将要执行的内容，不创建目录、不启动服务、不写临时文件。
    if args.dry_run {
        print_dry_run(&workdir, &download_repo, &server_repo, &maven, &args);
        return Ok(());
    }

    fs::create_dir_all(&download_repo)
        .with_context(|| format!("无法创建依赖下载目录: {}", download_repo.display()))?;
    fs::create_dir_all(&server_repo)
        .with_context(|| format!("无法创建 Maven mirror 仓库: {}", server_repo.display()))?;

    let server = server::start_server(ServerArgs {
        root: server_repo,
        host: "127.0.0.1".into(),
        port: 0,
        upstreams: args.upstreams.clone(),
        client: args.client,
        cache: args.cache,
    })?;
    let settings = TemporarySettings::create(&server)?;

    let mut command = maven_command(&maven, &workdir, settings.path(), &download_repo);
    command.arg("clean").arg("package").args(&args.maven_args);

    println!("Maven mirror 已启动: {}", server.base_url());
    println!("运行 Maven: {}", workdir.display());
    let status = command
        .status()
        .with_context(|| format!("无法执行 Maven 命令: {}", maven.display()))?;
    if !status.success() {
        return Err(anyhow!("Maven 构建失败: {status}"));
    }

    // `clean package` 只会加载本次构建生命周期实际用到的插件。POM 的
    // pluginManagement 或未激活 profile 中声明的插件不会被 Maven 下载。
    // 先生成完整 effective POM，再用 `help:describe` 逐个解析尚未缓存的
    // 插件及其传递依赖；不使用 `dependency:go-offline`。
    let effective_pom = EffectivePom::create(&maven, &workdir, settings.path(), &download_repo)?;
    let declared_plugins = parse_declared_plugins(&effective_pom.read()?)
        .context("无法解析 Maven effective POM 中的插件声明")?;
    println!(
        "POM 声明插件: {} 个（含 pluginManagement 和未激活 profile）",
        declared_plugins.len()
    );

    for (index, plugin) in declared_plugins.iter().enumerate() {
        if plugin.is_cached(&download_repo) {
            continue;
        }

        println!(
            "补齐插件 {}/{}: {}",
            index + 1,
            declared_plugins.len(),
            plugin.reference()
        );
        let mut describe = maven_command(&maven, &workdir, settings.path(), &download_repo);
        describe
            .arg("-q")
            .arg("org.apache.maven.plugins:maven-help-plugin:3.5.1:describe")
            .arg(format!("-Dplugin={}", plugin.reference()))
            .arg("-Ddetail=false");
        run_maven(&mut describe, &maven, "插件解析")?;
    }

    // Maven 构建成功后，清理本次下载仓库中的客户端状态文件，
    // 使依赖目录可以直接作为可移植的 Maven 仓库使用。
    crate::cleanup::run_cleanup(crate::cleanup::CleanupArgs {
        repo: download_repo.clone(),
        dry_run: false,
    })?;

    println!("Maven 构建完成，依赖下载目录: {}", download_repo.display());
    Ok(())
}

/// `--dry-run`：只打印计划，不产生任何副作用（不建目录、不监听端口、
/// 不写临时 settings.xml）。
fn print_dry_run(
    workdir: &Path,
    download_repo: &Path,
    server_repo: &Path,
    maven: &Path,
    args: &DownloadArgs,
) {
    println!("工作目录: {}", workdir.display());
    println!("依赖下载目录: {} (不会创建)", download_repo.display());
    println!("Mirror 仓库: {} (不会创建)", server_repo.display());
    println!("Mirror URL: <启动后分配的随机端口>");
    println!("上游仓库: {}", args.upstreams.join(", "));
    let mut command = maven_command(
        maven,
        workdir,
        Path::new("<临时 settings.xml>"),
        download_repo,
    );
    command.arg("clean").arg("package").args(&args.maven_args);
    println!("命令: {command:?}");
}

fn maven_command(maven: &Path, workdir: &Path, settings: &Path, download_repo: &Path) -> Command {
    let mut command = Command::new(maven);
    command
        .current_dir(workdir)
        .arg("--settings")
        .arg(maven_compatible_path(settings))
        .arg(format!(
            "-Dmaven.repo.local={}",
            maven_compatible_path(download_repo).display()
        ));
    command
}

fn run_maven(command: &mut Command, maven: &Path, action: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("无法执行 Maven 命令: {}", maven.display()))?;
    if !status.success() {
        return Err(anyhow!("Maven {action}失败: {status}"));
    }
    Ok(())
}

fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// 将 Rust 在 Windows 上 canonicalize 后可能产生的扩展路径前缀转换为
/// Java/Maven 能识别的普通 Windows 路径。
///
/// `std::fs::canonicalize` 在 Windows 上可能返回 `\\?\C:\...`，但 Java
/// 的 WindowsPathParser 会把其中的 `?` 当作非法字符。UNC 路径也需要从
/// `\\?\UNC\server\share` 转回 `\\server\share`。
fn maven_compatible_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn resolve_maven(configured: Option<PathBuf>, workdir: &Path) -> PathBuf {
    if let Some(maven) = configured {
        if maven.is_absolute()
            || maven
                .parent()
                .is_none_or(|parent| parent.as_os_str().is_empty())
        {
            return maven;
        }
        let relative = workdir.join(&maven);
        return if relative.is_file() { relative } else { maven };
    }

    #[cfg(windows)]
    let wrapper = workdir.join("mvnw.cmd");
    #[cfg(not(windows))]
    let wrapper = workdir.join("mvnw");
    if wrapper.is_file() {
        return wrapper;
    }

    #[cfg(windows)]
    {
        PathBuf::from("mvn.cmd")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("mvn")
    }
}

struct EffectivePom {
    path: PathBuf,
}

impl EffectivePom {
    fn create(maven: &Path, workdir: &Path, settings: &Path, download_repo: &Path) -> Result<Self> {
        let path = env::temp_dir().join(format!(
            "maven-util-effective-pom-{}-{}.xml",
            std::process::id(),
            unique_suffix()
        ));
        let mut command = maven_command(maven, workdir, settings, download_repo);
        command
            .arg("-q")
            .arg("org.apache.maven.plugins:maven-help-plugin:3.5.1:effective-pom")
            .arg(format!(
                "-Doutput={}",
                maven_compatible_path(&path).display()
            ));

        println!("扫描 POM 中声明的插件...");
        run_maven(&mut command, maven, "effective POM 生成")?;
        Ok(Self { path })
    }

    fn read(&self) -> Result<String> {
        fs::read_to_string(&self.path)
            .with_context(|| format!("无法读取 effective POM: {}", self.path.display()))
    }
}

impl Drop for EffectivePom {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MavenPlugin {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
}

impl MavenPlugin {
    fn reference(&self) -> String {
        match &self.version {
            Some(version) => format!("{}:{}:{}", self.group_id, self.artifact_id, version),
            None => format!("{}:{}", self.group_id, self.artifact_id),
        }
    }

    fn is_cached(&self, repository: &Path) -> bool {
        let Some(version) = &self.version else {
            return false;
        };
        let directory = repository
            .join(self.group_id.replace('.', "/"))
            .join(&self.artifact_id)
            .join(version);
        let basename = format!("{}-{}", self.artifact_id, version);
        directory.join(format!("{basename}.pom")).is_file()
            && directory.join(format!("{basename}.jar")).is_file()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XmlTagKind {
    Start,
    End,
    Empty,
}

#[derive(Debug, Clone)]
struct XmlTag<'a> {
    kind: XmlTagKind,
    name: &'a str,
    start: usize,
    end: usize,
}

/// 解析 Maven Help 生成的 effective POM。
///
/// 不引入 XML 依赖，只处理 Maven 生成的、格式稳定的 XML；解析时会跳过
/// 注释和声明，并正确忽略带引号属性中的 `>`。
fn parse_xml_tags(xml: &str) -> Result<Vec<XmlTag<'_>>> {
    let mut tags = Vec::new();
    let mut cursor = 0;

    while let Some(relative_start) = xml[cursor..].find('<') {
        let start = cursor + relative_start;
        if xml[start..].starts_with("<!--") {
            let comment_end = xml[start + 4..]
                .find("-->")
                .map(|offset| start + 4 + offset + 3)
                .context("effective POM 中存在未闭合的 XML 注释")?;
            cursor = comment_end;
            continue;
        }
        if xml[start..].starts_with("<?") {
            let declaration_end = xml[start + 2..]
                .find("?>")
                .map(|offset| start + 2 + offset + 2)
                .context("effective POM 中存在未闭合的 XML 声明")?;
            cursor = declaration_end;
            continue;
        }
        if xml[start..].starts_with("<!") {
            let declaration_end = xml[start + 2..]
                .find('>')
                .map(|offset| start + 2 + offset + 1)
                .context("effective POM 中存在未闭合的 XML 声明")?;
            cursor = declaration_end;
            continue;
        }

        let end = find_tag_end(xml, start).context("effective POM 中存在未闭合的 XML 标签")?;
        let mut inner = xml[start + 1..end.saturating_sub(1)].trim();
        let kind = if let Some(rest) = inner.strip_prefix('/') {
            inner = rest.trim_start();
            XmlTagKind::End
        } else {
            XmlTagKind::Start
        };
        let name_end = inner
            .find(|character: char| character.is_whitespace() || character == '/')
            .unwrap_or(inner.len());
        let name = &inner[..name_end];
        if name.is_empty() {
            return Err(anyhow!("effective POM 中存在没有名称的 XML 标签"));
        }
        let kind = if kind == XmlTagKind::Start && inner[name_end..].trim_end().ends_with('/') {
            XmlTagKind::Empty
        } else {
            kind
        };
        tags.push(XmlTag {
            kind,
            name,
            start,
            end,
        });
        cursor = end;
    }

    Ok(tags)
}

fn find_tag_end(xml: &str, start: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut quote = None;
    for (offset, byte) in bytes.iter().enumerate().skip(start + 1) {
        match quote {
            Some(expected) if *byte == expected => quote = None,
            Some(_) => {}
            None if *byte == b'"' || *byte == b'\'' => quote = Some(*byte),
            None if *byte == b'>' => return Some(offset + 1),
            None => {}
        }
    }
    None
}

fn is_plugin_container_parent(path: &[&str]) -> bool {
    matches!(
        path,
        [.., "build", "plugins"]
            | [.., "build", "pluginManagement", "plugins"]
            | [.., "reporting", "plugins"]
    )
}

fn parse_declared_plugins(xml: &str) -> Result<Vec<MavenPlugin>> {
    let tags = parse_xml_tags(xml)?;
    let mut path: Vec<&str> = Vec::new();
    let mut active_plugin: Option<(usize, usize)> = None;
    let mut blocks = Vec::new();

    for tag in &tags {
        match tag.kind {
            XmlTagKind::Start => {
                if active_plugin.is_none()
                    && tag.name == "plugin"
                    && is_plugin_container_parent(&path)
                {
                    active_plugin = Some((tag.start, path.len() + 1));
                }
                path.push(tag.name);
            }
            XmlTagKind::Empty => {
                if tag.name == "plugin"
                    && is_plugin_container_parent(&path)
                    && let Some(plugin) = parse_plugin_block(&xml[tag.start..tag.end])?
                {
                    blocks.push(plugin);
                }
            }
            XmlTagKind::End => {
                if let Some((start, depth)) = active_plugin
                    && tag.name == "plugin"
                    && path.last() == Some(&"plugin")
                    && path.len() == depth
                {
                    if let Some(plugin) = parse_plugin_block(&xml[start..tag.start])? {
                        blocks.push(plugin);
                    }
                    active_plugin = None;
                }
                path.pop();
            }
        }
    }

    let mut concrete_versions: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for plugin in &blocks {
        if let Some(version) = &plugin.version
            && !version.contains("${")
        {
            concrete_versions
                .entry((plugin.group_id.clone(), plugin.artifact_id.clone()))
                .or_default()
                .insert(version.clone());
        }
    }

    let mut result = BTreeSet::new();
    for plugin in &blocks {
        if plugin
            .version
            .as_ref()
            .is_some_and(|version| !version.contains("${"))
        {
            result.insert(plugin.clone());
            continue;
        }

        let key = (plugin.group_id.clone(), plugin.artifact_id.clone());
        if let Some(versions) = concrete_versions.get(&key) {
            result.extend(versions.iter().map(|version| MavenPlugin {
                group_id: plugin.group_id.clone(),
                artifact_id: plugin.artifact_id.clone(),
                version: Some(version.clone()),
            }));
        } else {
            result.insert(MavenPlugin {
                group_id: plugin.group_id.clone(),
                artifact_id: plugin.artifact_id.clone(),
                version: None,
            });
        }
    }

    Ok(result.into_iter().collect())
}

fn parse_plugin_block(block: &str) -> Result<Option<MavenPlugin>> {
    let tags = parse_xml_tags(block)?;
    let mut path: Vec<&str> = Vec::new();
    let mut values: BTreeMap<&str, String> = BTreeMap::new();
    let mut text_start = None;
    let mut text_depth = None;

    for tag in &tags {
        match tag.kind {
            XmlTagKind::Start => {
                if path == ["plugin"]
                    && matches!(tag.name, "groupId" | "artifactId" | "version")
                    && text_start.is_none()
                {
                    text_start = Some(tag.end);
                    text_depth = Some(path.len() + 1);
                }
                path.push(tag.name);
            }
            XmlTagKind::End => {
                if text_depth == Some(path.len())
                    && path.last() == Some(&tag.name)
                    && matches!(tag.name, "groupId" | "artifactId" | "version")
                    && let (Some(start), Some(name)) = (text_start, path.last())
                {
                    values.insert(name, decode_xml_text(&block[start..tag.start]));
                    text_start = None;
                    text_depth = None;
                }
                path.pop();
            }
            XmlTagKind::Empty => {}
        }
    }

    let Some(artifact_id) = values.get("artifactId").filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let group_id = values
        .get("groupId")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| "org.apache.maven.plugins".into());
    let version = values
        .get("version")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_owned());

    Ok(Some(MavenPlugin {
        group_id,
        artifact_id: artifact_id.clone(),
        version,
    }))
}

fn decode_xml_text(value: &str) -> String {
    value
        .trim()
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

struct TemporarySettings {
    path: PathBuf,
}

impl TemporarySettings {
    fn create(server: &ServerHandle) -> Result<Self> {
        let path = env::temp_dir().join(format!(
            "maven-uploader-settings-{}-{}.xml",
            std::process::id(),
            unique_suffix()
        ));
        let mirror_url = server.base_url();
        let settings = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<settings xmlns=\"http://maven.apache.org/SETTINGS/1.0.0\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://maven.apache.org/SETTINGS/1.0.0 https://maven.apache.org/xsd/settings-1.0.0.xsd\">\n  <mirrors>\n    <mirror>\n      <id>maven-uploader</id>\n      <name>maven-uploader local mirror</name>\n      <url>{}</url>\n      <mirrorOf>*</mirrorOf>\n    </mirror>\n  </mirrors>\n  <profiles>\n    <profile>\n      <id>maven-uploader-repository</id>\n      <repositories>\n        <repository>\n          <id>maven-uploader-source</id>\n          <url>{}</url>\n          <releases><enabled>true</enabled></releases>\n          <snapshots><enabled>true</enabled></snapshots>\n        </repository>\n      </repositories>\n      <pluginRepositories>\n        <pluginRepository>\n          <id>maven-uploader-plugin-source</id>\n          <url>{}</url>\n          <releases><enabled>true</enabled></releases>\n          <snapshots><enabled>true</enabled></snapshots>\n        </pluginRepository>\n      </pluginRepositories>\n    </profile>\n  </profiles>\n  <activeProfiles>\n    <activeProfile>maven-uploader-repository</activeProfile>\n  </activeProfiles>\n</settings>\n",
            xml_escape(&mirror_url),
            xml_escape(&mirror_url),
            xml_escape(&mirror_url)
        );
        fs::write(&path, settings).context("无法创建临时 Maven settings.xml")?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporarySettings {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
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
    fn resolves_relative_download_repo_against_workdir() {
        assert_eq!(
            absolute_path(Path::new("repo"), Path::new("/tmp/project")),
            PathBuf::from("/tmp/project/repo")
        );
        assert_eq!(
            absolute_path(Path::new("/tmp/repo"), Path::new("/tmp/project")),
            PathBuf::from("/tmp/repo")
        );
    }

    #[test]
    fn uses_explicit_maven_path_when_given() {
        assert_eq!(
            resolve_maven(Some(PathBuf::from("maven")), Path::new("/tmp/project")),
            PathBuf::from("maven")
        );
    }

    #[test]
    fn escapes_settings_values() {
        assert_eq!(xml_escape("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
    }

    #[test]
    fn parses_effective_plugins_from_build_management_and_profiles() {
        let xml = r#"
            <?xml version="1.0"?>
            <projects>
              <project>
                <build>
                  <pluginManagement>
                    <plugins>
                      <plugin>
                        <groupId>com.google.cloud.tools</groupId>
                        <artifactId>jib-maven-plugin</artifactId>
                        <version>3.5.2</version>
                      </plugin>
                    </plugins>
                  </pluginManagement>
                  <plugins>
                    <plugin>
                      <artifactId>maven-antrun-plugin</artifactId>
                      <version>1.8</version>
                      <configuration>
                        <plugins>
                          <plugin>
                            <artifactId>not-a-build-plugin</artifactId>
                            <version>9.9.9</version>
                          </plugin>
                        </plugins>
                      </configuration>
                    </plugin>
                    <plugin>
                      <groupId>org.codehaus.mojo</groupId>
                      <artifactId>build-helper-maven-plugin</artifactId>
                    </plugin>
                  </plugins>
                </build>
                <profiles>
                  <profile>
                    <id>image</id>
                    <build>
                      <plugins>
                        <plugin>
                          <groupId>com.google.cloud.tools</groupId>
                          <artifactId>jib-maven-plugin</artifactId>
                        </plugin>
                      </plugins>
                    </build>
                  </profile>
                </profiles>
              </project>
            </projects>
        "#;

        let plugins = parse_declared_plugins(xml).unwrap();
        assert_eq!(
            plugins,
            vec![
                MavenPlugin {
                    group_id: "com.google.cloud.tools".into(),
                    artifact_id: "jib-maven-plugin".into(),
                    version: Some("3.5.2".into()),
                },
                MavenPlugin {
                    group_id: "org.apache.maven.plugins".into(),
                    artifact_id: "maven-antrun-plugin".into(),
                    version: Some("1.8".into()),
                },
                MavenPlugin {
                    group_id: "org.codehaus.mojo".into(),
                    artifact_id: "build-helper-maven-plugin".into(),
                    version: None,
                },
            ]
        );
    }

    #[test]
    fn reports_cache_path_for_complete_plugin_artifact() {
        let root = Path::new("/tmp/plugin-cache");
        let plugin = MavenPlugin {
            group_id: "com.google.cloud.tools".into(),
            artifact_id: "jib-maven-plugin".into(),
            version: Some("3.5.2".into()),
        };
        assert!(!plugin.is_cached(root));
        assert_eq!(
            plugin.reference(),
            "com.google.cloud.tools:jib-maven-plugin:3.5.2"
        );
    }

    #[cfg(windows)]
    #[test]
    fn removes_windows_extended_path_prefix_for_maven() {
        assert_eq!(
            maven_compatible_path(Path::new(r"\\?\C:\code\maven-repo")),
            PathBuf::from(r"C:\code\maven-repo")
        );
        assert_eq!(
            maven_compatible_path(Path::new(r"\\?\UNC\server\share\maven-repo")),
            PathBuf::from(r"\\server\share\maven-repo")
        );
    }
}
