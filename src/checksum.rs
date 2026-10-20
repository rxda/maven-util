//! Maven checksum 计算。
//!
//! 直接使用成熟的 `sha1` crate，避免手写摘要算法的边界错误生成错误的
//! Maven checksum。

use sha1::{Digest, Sha1};
use std::io::{self, Read};

const BUFFER_SIZE: usize = 64 * 1024;

/// 计算任意可读数据的 SHA-1（小写十六进制）。
pub fn sha1_hex_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha1::new();
    digest.update(bytes);
    to_hex(&digest.finalize())
}

/// 流式计算 SHA-1（小写十六进制），不会把整个文件读进内存。
pub fn sha1_hex_reader<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut digest = Sha1::new();
    let mut buffer = [0_u8; BUFFER_SIZE];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(to_hex(&digest.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_sha1_of_abc() {
        assert_eq!(
            sha1_hex_bytes(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn calculates_sha1_of_empty_input() {
        assert_eq!(
            sha1_hex_bytes(b""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    #[test]
    fn streaming_matches_in_memory_digest() {
        let data = vec![b'a'; 200 * 1024];
        let mut input = io::Cursor::new(&data);
        assert_eq!(sha1_hex_reader(&mut input).unwrap(), sha1_hex_bytes(&data));
    }
}
