# maven-util

一个用 Rust 编写的 Maven 工具，支持：

- 批量上传本地 Maven 仓库文件到私有仓库（失败会以非零退出码结束）
- 启动本地只读 Maven mirror，并从一个或多个上游仓库回源、限容缓存
- 执行 Maven 构建并缓存 parent、BOM、插件和传递依赖
- Maven Wrapper（`mvnw` / `mvnw.cmd`）

## 首次运行与初始化

第一次执行任意命令时会自动完成初始化：

1. 创建 `~/.maven-util/` 并生成配置文件 `~/.maven-util/config.toml`
2. 把当前可执行文件复制到 `~/.maven-util/bin/maven-util`
3. 在 `~/.bashrc` / `~/.zshrc` / `~/.profile` 中追加
   `export PATH="$HOME/.maven-util/bin:$PATH"`（已存在则跳过）

之后打开新终端即可直接使用 `maven-util`。也可以手动初始化或重跑（例如升级二进制后）：

```bash
maven-util init              # 重新复制可执行文件并确保 PATH 配置存在
maven-util init --no-path    # 不修改 shell 配置文件
maven-util init --no-binary  # 只写配置文件，不复制可执行文件
```

整个初始化过程幂等。CI / 只读环境可用环境变量 `MAVEN_UTIL_SKIP_INIT=1`
跳过全部初始化动作，`--dry-run` 也不会触发初始化。

Windows 下不会静默修改用户 PATH，请手动执行：

```bat
setx PATH "%USERPROFILE%\.maven-util\bin;%PATH%"
```

## 配置文件

配置优先级：**命令行参数 > 环境变量 > 配置文件 > 内置默认值**。

配置文件查找顺序：

1. `-c/--config` 或环境变量 `MAVEN_UTIL_CONFIG`（指定但不存在时报错）
2. 当前目录的 `maven-uploader.toml` / `maven-uploader.example.toml`（便于按项目覆盖）
3. `~/.maven-util/config.toml`（首次运行自动生成）

因此配好 `~/.maven-util/config.toml` 之后，最短命令是：

```bash
maven-util                       # 上传（等价于 maven-util upload）
maven-util download
maven-util serve
```

配置文件模板（首次运行自动生成，含全部注释）：

```toml
[upload]
# url = "https://nexus.example.com/repository/maven-releases"
# snapshot_url = "https://nexus.example.com/repository/maven-snapshots"
# username = "admin"
# password = "your-password"
dir = "."
force = false
exclude = []
max_size_mb = 100
db_path = "~/.maven-util/uploads.db"

[download]
workdir = "."
download_repo = "./maven-repo"
server_repo = "~/.m2/repository"
upstreams = ["https://repo.maven.apache.org/maven2"]
# maven = "mvn"
maven_args = []
dry_run = false
skip_tests = false

[serve]
dir = "~/.m2/repository"
host = "127.0.0.1"
port = 8080
upstreams = ["https://repo.maven.apache.org/maven2"]

[clean]
repo = "~/.m2/repository"

[http]
connect_timeout_secs = 10     # 连接超时
request_timeout_secs = 60     # HEAD / 上游回源总超时
upload_timeout_secs = 600     # 上传总超时
max_file_bytes = 268435456    # 单个构件缓存上限（超出只转发不落盘）
max_cache_bytes = 10737418240 # 缓存总容量上限（超出只转发不落盘）
```

## 构建

```bash
cargo build --release
```

生成的程序位于 `target/release/maven-util`。

## 上传本地 Maven 仓库

配置文件里填好 `url` / `username` / `password` 之后：

```bash
maven-util
# 或显式调用
maven-util upload --dir ./maven-repo
```

命令行参数仍然可用，并优先于配置文件：

```bash
maven-util \
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
-f, --force                 强制重新上传（跳过本地状态库和远程 HEAD 检查）
-E, --exclude <TEXT,...>    排除 groupId 或 artifactId 关键词
    --max-size <MB>         跳过超过大小的 jar/war，默认 100MB
    --db-path <FILE>        上传状态数据库，默认 ~/.maven-util/uploads.db
```

上传语义：

- 任何一个文件上传失败（HTTP 非 2xx 或网络错误），都会记录并计入汇总，
  进程以**退出码 1** 结束，便于 CI / 脚本判断；
- 目录遍历本身出错（权限、线程池异常等）同样以非零退出码结束，
  因为此时扫描结果不完整；结构不合法的 POM 只作为警告逐条打印；
- 状态库（redb）保存的是 `文件大小:纳秒级 mtime` 指纹，同一秒内的修改
  也会被识别为需要重新上传；`--force` 可完全绕过状态库和远程 HEAD 检查。

## 下载依赖

`download` 会在后台启动内置 Maven mirror，生成临时 `settings.xml`，然后在项目目录执行：

```text
mvn clean package -Dmaven.repo.local=<下载仓库>
```

Maven 自己负责解析 parent、parent 的 parent、BOM、插件和传递依赖，因此不会遗漏
parent POM。`download` 会把 `server_repo`（默认 `~/.m2/repository`）启动为内置
mirror 的仓库根目录，Maven 通过 `-Dmaven.repo.local` 将新下载的依赖保存到目标
`download_repo`。

`clean package` 只会下载本次构建生命周期实际执行的插件。为补齐 POM 中声明但未参与
默认构建的插件，构建成功后程序还会：

1. 执行 `maven-help-plugin:effective-pom`，收集继承、`pluginManagement` 和未激活
   profile 中的全部插件声明；
2. 对尚未缓存的插件逐个执行 `maven-help-plugin:describe`，下载插件 JAR、POM 及其
   传递依赖。

该流程不使用 `dependency:go-offline`。全部完成后，程序会递归删除 `download_repo`
中由 Maven 生成的 `_remote.repositories` 和 `*.lastUpdated` 文件。

```bash
maven-util download . --download-repo ./maven-repo --upstream https://repo.maven.apache.org/maven2
maven-util download . --download-repo ./maven-repo --skip-tests
maven-util download . --download-repo ./maven-repo -- -DskipTests   # 追加 Maven 参数
```

主要参数：

```text
<DIR>                       Maven 项目运行目录，默认当前目录
-o, --download-repo <DIR>   依赖保存目录，默认 maven-repo
    --server-repo <DIR>     mirror 服务仓库，默认 ~/.m2/repository
    --upstream <URL,...>    上游仓库，可重复指定，按顺序尝试
    --maven <FILE>          Maven 可执行文件；未指定时优先使用项目中的 mvnw/mvnw.cmd
-s, --skip-tests            跳过测试执行，向 Maven 追加 -DskipTests
    --dry-run               仅打印计划，不产生任何副作用
```

`--dry-run` 严格无副作用：不创建依赖目录、不创建 mirror 仓库、不监听端口、
不写临时 `settings.xml`，因此在只读目录或无网络权限的环境里也能预览。

内置 mirror 固定使用 `mirrorOf=*`，因此 Maven 的普通依赖仓库和插件仓库都会走内置
mirror。

## 启动只读 Maven Server

把本地 Maven 仓库按 Maven 仓库协议提供给 Maven、Gradle 或其他构建工具：

```bash
maven-util serve --dir ~/.m2/repository --host 127.0.0.1 --port 8080
maven-util serve --upstream https://nexus.example.com/repository/maven-public \
                 --upstream https://repo.maven.apache.org/maven2
```

`serve` 支持 Maven 常用的 `GET` 和 `HEAD` 请求，使用固定工作线程池和 HTTP
Keep-Alive，拒绝上传和删除请求。已有的 `maven-metadata.xml` 会原样返回；缺失时先
从上游获取，若所有上游都没有，再从本地目录生成基础元数据；`.sha1` 缺失时实时计算。

HTTP 状态码由明确的错误类型决定，而不是错误文案：

| 情况 | 状态码 |
| --- | --- |
| 构件不存在（本地和上游都没有） | 404 |
| 路径非法（越界、空组件、控制字符） | 400 |
| 请求方法不是 GET/HEAD | 405 |
| 所有上游都失败（网络错误或非 404 响应） | 502 |
| 磁盘等内部错误 | 500 |

上游回源时的缓存策略（见 `[http]` 配置）：

- 构件大小超过 `max_file_bytes`：直接转发给客户端，不落盘；
- 缓存总量达到 `max_cache_bytes`：直接转发给客户端，不落盘；
- 其余情况先写临时文件再原子重命名，中途失败会清理临时文件。

## 清理 Maven 仓库状态文件

递归删除 Maven 仓库中的 `_remote.repositories` 和 `*.lastUpdated` 文件：

```bash
maven-util clean
maven-util clean --repo /path/to/.m2/repository
maven-util clean --dry-run      # 只显示将要删除的文件
```

该命令不会删除 JAR、POM、元数据或目录。

## 代码结构

```
src/
  main.rs              入口（仅退出码处理）
  lib.rs               子命令分发
  cli.rs               命令行定义 + 配置合并
  config.rs            ~/.maven-util/config.toml 生成与读取
  init.rs              首次运行自装与 PATH 配置
  upload.rs            批量上传
  downloader.rs        内置 mirror 执行 Maven 构建
  tls.rs               HTTP 客户端（纯 Rust TLS + 超时）
  checksum.rs          Maven checksum（sha1 crate）
  cleanup.rs           状态文件清理
  server/
    mod.rs             服务启动、连接与工作线程
    http.rs            请求解析与响应写出
    repository.rs      本地/上游解析、缓存与 ResolveError
    metadata.rs        maven-metadata.xml 生成
```

## 测试

```bash
cargo test              # 单元测试 + 集成测试（HTTP、退出码、配置优先级、dry-run 等）
```

集成测试包含：

- 真实 HTTP `GET`/`HEAD`/`400`/`404`/`405`/`502` 与 Keep-Alive；
- 上游回源只回源一次、并发下载后缓存完整且不残留 `.part` 文件；
- 单文件 / 总容量超限时只转发不落盘；
- 上传失败退出码、`--force`、状态库复用；
- 配置文件自动生成、命令行优先于配置文件；
- `--dry-run` 零副作用（不建目录、不写临时文件、不触发初始化）；
- 首次运行安装可执行文件并写入 PATH，且重复 `init` 幂等；
- 真实 Maven 构建端到端（本机没有 Maven 或设置 `MAVEN_UTIL_SKIP_E2E=1` 时自动跳过；
  可用 `MAVEN_UTIL_TEST_MAVEN_HOME=/opt/maven` 指定 Maven）。

## 依赖说明

- `rustls-rustcrypto` 仍是 alpha 依赖，选它是为了保持**纯 Rust、无 C 代码**的 TLS。
  如果需要更成熟的提供者，可以换成 `rustls` 的 `ring` 或 `aws-lc-rs` provider
  （`tls.rs` 中的 `build_client` 是唯一的注入点）。
- SHA-1 使用成熟的 `sha1` crate，不再手写摘要算法。
