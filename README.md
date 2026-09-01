# maven-uploader

一个用 Rust 编写的 Maven 工具，支持：

- 批量上传本地 Maven 仓库文件到私有仓库
- 启动本地只读 Maven mirror，并从一个或多个上游仓库回源
- 执行 Maven 构建并缓存 parent、BOM、插件和传递依赖
- Maven Wrapper（`mvnw` / `mvnw.cmd`）

## 清理 Maven 仓库状态文件

递归删除默认 Maven 仓库中的 `_remote.repositories` 和 `*.lastUpdated` 文件：

```bash
maven-uploader clean
```

指定仓库目录，或先使用 `--dry-run` 预览：

```bash
maven-uploader clean --repo /path/to/.m2/repository
maven-uploader clean --dry-run
```

该命令不会删除 JAR、POM、元数据或目录。

## 构建

```bash
cargo build --release
```

生成的程序位于 `target/release/maven-uploader`。

## 配置文件

`download` 和 `serve` 都支持通过 `-c/--config` 指定 TOML 配置文件，命令行参数优先级高于配置文件：

```bash
maven-uploader download --config maven-uploader.toml
```

配置示例：

```toml
[download]
workdir = "."
download_repo = "./maven-repo"
server_repo = "~/.m2/repository"
upstreams = [
  "https://nexus.example.com/repository/maven-public",
  "https://repo.maven.apache.org/maven2",
]
maven = "mvn"
maven_args = ["-DskipTests"]

[serve]
dir = "~/.m2/repository"
host = "127.0.0.1"
port = 8080
upstreams = ["https://repo.maven.apache.org/maven2"]
```

`download` 配置项：`workdir`、`download_repo`、`server_repo`、`upstreams`、`maven`、`maven_args`、`dry_run`。
`serve` 配置项：`dir`、`host`、`port`、`upstreams`。

## 下载依赖

`download` 会在后台启动内置 Maven mirror，生成临时 `settings.xml`，然后在项目目录执行：

```text
mvn clean package -Dmaven.repo.local=<下载仓库>
```

Maven 自己负责解析 parent、parent 的 parent、BOM、插件和传递依赖，因此不会遗漏 parent POM。`download` 会把 `server_repo`（默认 `~/.m2/repository`）启动为内置 mirror 的仓库根目录，Maven 通过 `-Dmaven.repo.local` 将新下载的依赖保存到目标 `download_repo`。

```bash
maven-uploader download . \
  --download-repo ./maven-repo \
  --upstream https://repo.maven.apache.org/maven2
```

也可以指定多个上游仓库，按顺序尝试：

```bash
maven-uploader download . \
  --download-repo ./maven-repo \
  --upstream https://nexus.example.com/repository/maven-public \
  --upstream https://repo.maven.apache.org/maven2
```

主要参数：

```text
<DIR>                       Maven 项目运行目录，默认当前目录
-o, --download-repo <DIR>  依赖保存目录，默认 maven-repo
    --server-repo <DIR>     mirror 服务仓库，默认 ~/.m2/repository
    --upstream <URL,...>     上游仓库，可重复指定
    --maven <FILE>           Maven 可执行文件；未指定时优先使用项目中的 mvnw/mvnw.cmd
    --dry-run                仅显示命令，不执行构建
```

内置 mirror 固定使用 `mirrorOf=*`，因此 Maven 的普通依赖仓库和插件仓库都会走内置 mirror。

`download` 还支持在命令末尾追加 Maven 参数，例如：

```bash
maven-uploader download . --download-repo ./maven-repo -- -DskipTests
```

## 上传本地 Maven 仓库

上传模式不使用 `download` 子命令：

```bash
maven-uploader \
  --url https://nexus.example.com/repository/releases \
  --username admin \
  --password 'your-password' \
  --dir ./maven-repo
```

常用参数：

```text
-U, --url <URL>             Release 仓库地址
-S, --snapshot-url <URL>    Snapshot 仓库地址
-u, --username <NAME>       用户名
-p, --password <PASSWORD>   密码
-d, --dir <DIR>             本地仓库目录，默认当前目录
-f, --force                 强制重新上传
-E, --exclude <TEXT,...>    排除 groupId 或 artifactId 关键词
    --max-size <MB>         跳过超过大小的 jar/war，默认 100MB
    --db-path <FILE>        上传状态数据库，默认 uploader_state.db
```

## 启动只读 Maven Server

把本地 Maven 仓库按 Maven 仓库协议提供给 Maven、Gradle 或其他构建工具：

```bash
maven-uploader serve \
  --dir ~/.m2/repository \
  --host 127.0.0.1 \
  --port 8080
```

`serve` 支持 Maven 常用的 `GET` 和 `HEAD` 请求，使用固定连接线程池和 HTTP Keep-Alive，拒绝上传和删除请求。已有的 `maven-metadata.xml` 会原样返回；缺失时先从上游获取，若所有上游都没有，再从本地目录生成基础元数据。

上游仓库可以通过 `--upstream` 指定多个：

```bash
maven-uploader serve \
  --dir ~/.m2/repository \
  --upstream https://nexus.example.com/repository/maven-public \
  --upstream https://repo.maven.apache.org/maven2
```

不指定 `--dir` 时，默认服务 `~/.m2/repository`。服务当前为只读模式，按 `Ctrl+C` 停止。
