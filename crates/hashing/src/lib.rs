//! SakuraMedia 媒体 IO 服务的自包含哈希原语。
//!
//! 这里刻意不引入 `sha1` / `sha2` / `data-encoding` 等 crate，原因有三：
//!
//! 1. 产出的静态二进制要塞进 `sakuramedia` 镜像，依赖越少，交叉编译与审计面越小。
//! 2. 三个算法（SHA-1、SHA-256、Base32）都是固定的标准实现，总计约 200 行，
//!    且每一处都有公开测试向量兜底，不构成维护负担。
//! 3. 本工作区需要能在无外网环境下完成 `cargo build`（CI / 离线 NAS 构建）。
//!
//! **用途限定**：SHA-1 在本工作区只用于内容指纹与 BitTorrent v1 info hash，
//! 不用于任何需要抗碰撞的场景。

#![forbid(unsafe_code)]

use std::fmt;

mod base32;
mod sha1;
mod sha256;

pub use base32::Base32Error;
pub use sha1::Sha1;
pub use sha256::Sha256;

/// 一次性求 SHA-1 摘要，等价于 Python 的 `hashlib.sha1(data).digest()`。
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize()
}

/// 一次性求 SHA-1 十六进制小写摘要。
pub fn sha1_hex(data: &[u8]) -> String {
    hex(&sha1(data))
}

/// 一次性求 SHA-256 摘要。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

/// 一次性求 SHA-256 十六进制小写摘要。
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

/// 小写十六进制编码，对应 Python 的 `bytes.hex()`。
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 大写 Base32 编码，对应 Python 的 `base64.b32encode`。
///
/// 编码过程不会失败，因此不返回 `Result`。
pub fn base32_encode(data: &[u8]) -> String {
    base32::encode(data)
}

/// Base32 解码，对应 Python 的 `base64.b32decode`。
///
/// 与 Python 的差异：接受省略 `=` 补位的输入（磁力链接常见）。
pub fn base32_decode(text: &str) -> Result<Vec<u8>, Base32Error> {
    base32::decode(text)
}

/// 用于 `#[derive]` 友好的错误包装。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexError(pub String);

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid hex input: {}", self.0)
    }
}

impl std::error::Error for HexError {}

/// 十六进制解码，接受大小写混合。
///
/// 对应本工作区的用途：把 `info_hash` 字符串还原成字节。
pub fn unhex(text: &str) -> Result<Vec<u8>, HexError> {
    if text.len() % 2 != 0 {
        return Err(HexError(format!("odd length {}", text.len())));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_value(bytes[index])?;
        let low = hex_value(bytes[index + 1])?;
        out.push((high << 4) | low);
        index += 2;
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Result<u8, HexError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(HexError(format!("bad digit {:?}", other as char))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(
            sha1_hex(b""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            sha1_hex(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // 跨 64 字节块边界的长度扩展用例。
        let long = vec![b'a'; 1_000_000];
        assert_eq!(
            sha1_hex(&long),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn base32_roundtrips_rfc4648_vectors() {
        for (plain, encoded) in [
            ("f", "MY======"),
            ("fo", "MZXQ===="),
            ("foo", "MZXW6==="),
            ("foob", "MZXW6YQ="),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI======"),
        ] {
            assert_eq!(base32_encode(plain.as_bytes()), encoded);
            assert_eq!(base32_decode(encoded).unwrap(), plain.as_bytes());
            // 磁力链接里的 base32 常省略补位 =，两种形式都要接受。
            let unpadded = encoded.trim_end_matches('=');
            if unpadded.len() != encoded.len() {
                assert_eq!(base32_decode(unpadded).unwrap(), plain.as_bytes());
            }
        }
    }

    #[test]
    fn base32_rejects_invalid_alphabet() {
        // 数字 0/1/8/9 不在 RFC 4648 字母表内。
        assert!(base32_decode("MZXW6YTB1").is_err());
        // 空串是合法的空字节序列，与 Python 的 b32decode("") == b"" 一致。
        assert_eq!(base32_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_roundtrip_and_errors() {
        let digest = sha1(b"sakuramedia");
        assert_eq!(unhex(&hex(&digest)).unwrap(), digest);
        assert!(unhex("abc").is_err());
        assert!(unhex("zz").is_err());
    }
}
