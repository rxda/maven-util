//! HTTP 层：请求解析与响应写出。

use reqwest::blocking::Response as UpstreamResponse;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;

use crate::checksum;

/// 单个请求头的最大字节数。
pub const MAX_REQUEST_SIZE: usize = 16 * 1024;
/// 单个连接上处理的最大请求数（Keep-Alive 上限）。
pub const MAX_REQUESTS_PER_CONNECTION: usize = 100;

pub enum Body {
    File(File, u64),
    Bytes(Vec<u8>),
    /// 直接转发上游响应体（缓存空间不足或构件过大时不落盘）。
    Upstream(UpstreamResponse, u64),
}

impl Body {
    pub fn len(&self) -> u64 {
        match self {
            Self::File(_, length) | Self::Upstream(_, length) => *length,
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: Body,
}

impl Response {
    pub fn file(path: &Path, file: File, length: u64) -> Self {
        Self {
            status: "200 OK",
            content_type: content_type(path),
            body: Body::File(file, length),
        }
    }

    pub fn bytes(content_type: &'static str, bytes: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body: Body::Bytes(bytes),
        }
    }

    /// 由本地路径推断 content-type，直接流式返回上游响应。
    pub fn upstream(response: UpstreamResponse, length: u64, path: &Path) -> Self {
        Self {
            status: "200 OK",
            content_type: content_type(path),
            body: Body::Upstream(response, length),
        }
    }

    pub fn checksum(checksum: String) -> Self {
        Self::bytes(
            "text/plain; charset=utf-8",
            format!("{checksum}\n").into_bytes(),
        )
    }

    pub fn xml(xml: String) -> Self {
        Self::bytes("application/xml; charset=utf-8", xml.into_bytes())
    }
}

/// 计算响应体 SHA-1（用于生成校验和响应）。
pub fn sha1_body(body: Body) -> io::Result<String> {
    match body {
        Body::File(mut file, _) => checksum::sha1_hex_reader(&mut file),
        Body::Bytes(bytes) => Ok(checksum::sha1_hex_bytes(&bytes)),
        Body::Upstream(mut response, _) => checksum::sha1_hex_reader(&mut response),
    }
}

pub fn read_request<R: Read>(reader: &mut R) -> anyhow::Result<Option<String>> {
    use anyhow::{Context, anyhow};

    let mut request = Vec::with_capacity(1024);
    let mut byte = [0_u8; 1];
    loop {
        let read = reader.read(&mut byte).context("读取 HTTP 请求失败")?;
        if read == 0 {
            if request.is_empty() {
                return Ok(None);
            }
            return Err(anyhow!("客户端提前关闭连接"));
        }
        request.push(byte[0]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > MAX_REQUEST_SIZE {
            return Err(anyhow!("HTTP 请求头过大"));
        }
    }
    String::from_utf8(request)
        .map(Some)
        .context("HTTP 请求不是有效 UTF-8")
}

/// 解析请求行与 Keep-Alive 语义，返回 `(method, target, keep_alive)`。
pub fn parse_request(request: &str) -> anyhow::Result<(&str, &str, bool)> {
    use anyhow::anyhow;

    let mut lines = request.split("\r\n");
    let line = lines.next().ok_or_else(|| anyhow!("bad request"))?;
    let mut fields = line.split_ascii_whitespace();
    let method = fields.next().ok_or_else(|| anyhow!("bad request"))?;
    let target = fields.next().ok_or_else(|| anyhow!("bad request"))?;
    let version = fields.next().ok_or_else(|| anyhow!("bad request"))?;
    if fields.next().is_some() || (version != "HTTP/1.0" && version != "HTTP/1.1") {
        return Err(anyhow!("bad request"));
    }
    let mut close = false;
    let mut explicit_keep_alive = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(anyhow!("bad request"));
        };
        if name.eq_ignore_ascii_case("connection") {
            for token in value.split(',').map(str::trim) {
                if token.eq_ignore_ascii_case("close") {
                    close = true;
                } else if token.eq_ignore_ascii_case("keep-alive") {
                    explicit_keep_alive = true;
                }
            }
        }
    }
    let keep_alive = if version == "HTTP/1.1" {
        !close
    } else {
        explicit_keep_alive && !close
    };
    Ok((method, target, keep_alive))
}

pub fn write_response<W: Write>(
    stream: &mut W,
    response: Response,
    head_only: bool,
    keep_alive: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let length = response.body.len();
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nCache-Control: no-cache\r\nConnection: {connection}\r\nKeep-Alive: timeout=10, max={}\r\n\r\n",
        response.status, length, response.content_type, MAX_REQUESTS_PER_CONNECTION
    )?;
    if head_only {
        return Ok(());
    }
    match response.body {
        Body::File(mut file, _) => {
            io::copy(&mut file, stream).context("发送仓库文件失败")?;
        }
        Body::Bytes(bytes) => {
            stream.write_all(&bytes).context("发送响应失败")?;
        }
        Body::Upstream(mut upstream, _) => {
            io::copy(&mut upstream, stream).context("发送上游响应失败")?;
        }
    }
    Ok(())
}

pub fn write_error<W: Write>(
    stream: &mut W,
    status: &'static str,
    message: &str,
    keep_alive: bool,
    allow: Option<&str>,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let allow_header = allow.map_or(String::new(), |value| format!("Allow: {value}\r\n"));
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 {status}\r\n{allow_header}Content-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: {connection}\r\n\r\n{message}",
        message.len(),
    )?;
    stream.flush().context("发送错误响应失败")?;
    Ok(())
}

/// Maven 客户端状态文件：不作为构件对外提供。
pub fn ignored_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name == "_remote.repositories" || name.ends_with(".lastUpdated") || name.ends_with(".part")
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("pom") | Some("xml") => "application/xml",
        Some("json") => "application/json",
        Some("jar") | Some("war") | Some("aar") | Some("zip") => "application/java-archive",
        Some("sha1") | Some("md5") | Some("sha256") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub fn buffered(stream: TcpStream) -> BufReader<TcpStream> {
    BufReader::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http11_is_keep_alive_by_default() {
        let request = "GET /a.jar HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert!(parse_request(request).unwrap().2);
    }

    #[test]
    fn connection_close_disables_keep_alive() {
        let request = "GET /a.jar HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        assert!(!parse_request(request).unwrap().2);
    }

    #[test]
    fn http10_needs_explicit_keep_alive() {
        let request = "GET /a.jar HTTP/1.0\r\nHost: localhost\r\n\r\n";
        assert!(!parse_request(request).unwrap().2);
        let request = "GET /a.jar HTTP/1.0\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        assert!(parse_request(request).unwrap().2);
    }

    #[test]
    fn rejects_malformed_request_line() {
        assert!(parse_request("GET\r\n\r\n").is_err());
        assert!(parse_request("GET /a.jar HTTP/2.0\r\n\r\n").is_err());
        assert!(parse_request("GET /a.jar HTTP/1.1 extra\r\n\r\n").is_err());
    }

    #[test]
    fn reads_request_with_headers() {
        let mut input = b"GET /a.jar HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice();
        let request = read_request(&mut input).unwrap().unwrap();
        assert!(request.starts_with("GET /a.jar"));
    }

    #[test]
    fn returns_none_for_empty_connection() {
        let mut input: &[u8] = b"";
        assert!(read_request(&mut input).unwrap().is_none());
    }

    #[test]
    fn writes_content_length_response() {
        let mut output = Vec::new();
        let response = Response::bytes("text/plain; charset=utf-8", b"hi".to_vec());
        write_response(&mut output, response, false, false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn head_response_has_no_body() {
        let mut output = Vec::new();
        let response = Response::bytes("text/plain; charset=utf-8", b"hi".to_vec());
        write_response(&mut output, response, true, true).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn ignores_maven_state_files() {
        assert!(ignored_file(Path::new("/repo/_remote.repositories")));
        assert!(ignored_file(Path::new("/repo/a.jar.lastUpdated")));
        assert!(!ignored_file(Path::new("/repo/a.jar")));
    }
}
