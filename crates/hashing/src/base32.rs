//! Base32（RFC 4648）编解码，用于 BitTorrent v2 info hash 的 32 字符表示。

use core::fmt;

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Base32 解码失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base32Error {
    /// 出现了字母表以外的字符（`=` 补位除外）。
    InvalidCharacter { character: char, position: usize },
    /// 补位 `=` 之后仍出现数据字符。
    DataAfterPadding { position: usize },
    /// 有效字符数不合法：`len % 8` 属于 {1, 3, 6} 时无法由 5-bit 分组还原。
    InvalidLength { length: usize },
    /// 尾部残留比特非零，属于非规范编码。
    NonZeroTrailingBits,
}

impl fmt::Display for Base32Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCharacter {
                character,
                position,
            } => write!(f, "invalid base32 character {character:?} at position {position}"),
            Self::DataAfterPadding { position } => {
                write!(f, "base32 data character after padding at position {position}")
            }
            Self::InvalidLength { length } => {
                write!(f, "invalid base32 payload length {length}")
            }
            Self::NonZeroTrailingBits => write!(f, "base32 trailing bits are non-zero"),
        }
    }
}

impl std::error::Error for Base32Error {}

/// 大写 Base32 编码，输出带 `=` 补位（与 Python `base64.b32encode` 一致）。
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    for chunk in data.chunks(5) {
        let mut buffer = [0u8; 5];
        buffer[..chunk.len()].copy_from_slice(chunk);
        // 5 字节 = 40 bit，正好 8 个 5-bit 分组。
        let bits = u64::from(buffer[0]) << 32
            | u64::from(buffer[1]) << 24
            | u64::from(buffer[2]) << 16
            | u64::from(buffer[3]) << 8
            | u64::from(buffer[4]);
        // 符号数必须向上取整：1 字节 -> 2 符号，3 字节 -> 5 符号。
        let produced = (chunk.len() * 8).div_ceil(5);
        for index in 0..8 {
            if index < produced {
                let shift = 35 - index * 5;
                out.push(ALPHABET[((bits >> shift) & 0x1f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base32 解码。
///
/// 与 Python `base64.b32decode` 的差异：**接受省略 `=` 补位的输入**。
/// 磁力链接里的 `urn:btih:` 常见省略补位，Python 侧靠调用方补齐后再解码，
/// 这里直接容忍，避免把补位逻辑泄漏到调用方。
pub fn decode(text: &str) -> Result<Vec<u8>, Base32Error> {
    let bytes = text.as_bytes();
    let mut symbols: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut padding_start: Option<usize> = None;

    for (position, byte) in bytes.iter().enumerate() {
        match byte {
            b'=' => {
                if padding_start.is_none() {
                    padding_start = Some(position);
                }
            }
            _ => {
                if padding_start.is_some() {
                    return Err(Base32Error::DataAfterPadding { position });
                }
                symbols.push(*byte);
            }
        }
    }

    let remainder = symbols.len() % 8;
    if matches!(remainder, 1 | 3 | 6) {
        return Err(Base32Error::InvalidLength {
            length: symbols.len(),
        });
    }
    // 显式补位长度必须与实际长度自洽。
    if let Some(start) = padding_start {
        let expected_padding = (8 - remainder) % 8;
        if bytes.len() - start != expected_padding {
            return Err(Base32Error::InvalidLength {
                length: symbols.len(),
            });
        }
    }

    let mut out = Vec::with_capacity(symbols.len() * 5 / 8);
    let mut accumulator: u16 = 0;
    let mut bits_in_accumulator = 0u32;

    for (position, symbol) in symbols.iter().enumerate() {
        let value = decode_symbol(*symbol, position)?;
        accumulator = (accumulator << 5) | u16::from(value);
        bits_in_accumulator += 5;
        if bits_in_accumulator >= 8 {
            bits_in_accumulator -= 8;
            out.push((accumulator >> bits_in_accumulator) as u8);
        }
    }

    if bits_in_accumulator > 0 {
        let trailing = accumulator & ((1 << bits_in_accumulator) - 1);
        if trailing != 0 {
            return Err(Base32Error::NonZeroTrailingBits);
        }
    }

    Ok(out)
}

fn decode_symbol(symbol: u8, position: usize) -> Result<u8, Base32Error> {
    match symbol {
        b'A'..=b'Z' => Ok(symbol - b'A'),
        b'2'..=b'7' => Ok(symbol - b'2' + 26),
        // 磁力链接偶尔出现小写，Python 的 b32decode 不接受，这里同样拒绝以保持一致。
        _ => Err(Base32Error::InvalidCharacter {
            character: symbol as char,
            position,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_all_partial_lengths() {
        // 覆盖 len % 5 == 0..4 的全部分支。
        for length in 0..12usize {
            let data: Vec<u8> = (0u8..=255).cycle().take(length).collect();
            let encoded = encode(&data);
            assert_eq!(decode(&encoded).unwrap(), data, "length={length}");
            assert_eq!(encoded.len(), length.div_ceil(5) * 8);
        }
    }

    #[test]
    fn accepts_unpadded_input() {
        for length in 2..12usize {
            let data: Vec<u8> = (0u8..=255).cycle().take(length).collect();
            let padded = encode(&data);
            let unpadded = padded.trim_end_matches('=');
            assert_eq!(decode(unpadded).unwrap(), data, "length={length}");
        }
    }

    #[test]
    fn rejects_malformed_padding() {
        assert_eq!(
            decode("MZXW6YTB="),
            Err(Base32Error::InvalidLength { length: 8 })
        );
        assert!(matches!(
            decode("MZXW6YTB=A"),
            Err(Base32Error::DataAfterPadding { .. })
        ));
    }

    #[test]
    fn rejects_impossible_lengths() {
        assert!(matches!(
            decode("A"),
            Err(Base32Error::InvalidLength { length: 1 })
        ));
        assert!(matches!(
            decode("ABC"),
            Err(Base32Error::InvalidLength { length: 3 })
        ));
    }
}
