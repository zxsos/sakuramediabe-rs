//! 最小 Bencode 解析器，作用域仅限 `.torrent` 的 `info` 字典定位。
//!
//! # 为什么不引入 `bendy`
//!
//! 需求只有两条：① 确认整个文件是合法 bencode；② 取出顶层 `info` 值
//! **原始字节**的偏移区间，因为 v1 info hash 是对原始编码字节做 SHA-1，
//! 任何重新编码（哪怕语义等价）都会得到不同的哈希。
//!
//! 完整反序列化（含写回、字典排序、类型转换）在这里都用不上，反而会引入
//! 攻击面：恶意种子可以构造极深嵌套或超大整数。下面的实现因此只做
//! 单遍扫描 + 深度上限，不分配任何值对象。

use core::fmt;
use core::ops::Range;

/// bencode 结构错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BencodeError {
    /// 到达末尾仍未遇到预期的结束符或数据。
    UnexpectedEnd,
    /// 出现无法识别的类型标记。
    InvalidTypeMarker(u8),
    /// 字符串长度前缀不是合法十进制数。
    InvalidStringLength,
    /// 声明长度与实际剩余字节不符。
    StringLengthOutOfRange { declared: u64, available: u64 },
    /// 整数不符合 `-?[0-9]+` 或超出长度上限。
    InvalidInteger,
    /// 字典键不是字符串。
    NonStringDictionaryKey,
    /// 嵌套层级超过上限。
    DepthLimitExceeded { limit: usize },
    /// 顶层不是字典。
    TopLevelNotDictionary,
    /// 缺少 `info` 键。
    MissingInfoKey,
}

impl fmt::Display for BencodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "bencode: unexpected end of input"),
            Self::InvalidTypeMarker(byte) => {
                write!(f, "bencode: invalid type marker {:?}", *byte as char)
            }
            Self::InvalidStringLength => write!(f, "bencode: invalid string length prefix"),
            Self::StringLengthOutOfRange {
                declared,
                available,
            } => write!(
                f,
                "bencode: string length {declared} exceeds available {available}"
            ),
            Self::InvalidInteger => write!(f, "bencode: invalid integer"),
            Self::NonStringDictionaryKey => write!(f, "bencode: dictionary key is not a string"),
            Self::DepthLimitExceeded { limit } => {
                write!(f, "bencode: nesting deeper than {limit}")
            }
            Self::TopLevelNotDictionary => write!(f, "bencode: top level is not a dictionary"),
            Self::MissingInfoKey => write!(f, "bencode: missing 'info' key"),
        }
    }
}

impl std::error::Error for BencodeError {}

/// 嵌套深度上限。合法 `.torrent` 的实际深度不超过 3。
const MAX_DEPTH: usize = 32;

/// 单个整数数字部分的最大字节数，防止超长数字做无意义运算。
const MAX_INTEGER_DIGITS: usize = 24;

/// 从 `.torrent` 提取的 `info` 元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentMeta {
    /// `info` 值在原始输入中的字节区间，可直接切片喂给 SHA-1。
    pub info_span: Range<usize>,
    /// `info` 字典的键集合，用于判断 v1 / v2。
    pub info_keys: Vec<Vec<u8>>,
}

impl TorrentMeta {
    /// `info` 是否含 `pieces` 键。
    ///
    /// 这是 v1 infohash 存在与否的判据：`pieces` 是 v1 必需字段，
    /// 纯 v2（`meta version = 2` + `file tree`）种子没有它，
    /// 对应 libtorrent 的 `info_hashes().has_v1() == false`。
    pub fn has_v1(&self) -> bool {
        self.info_keys.iter().any(|key| key.as_slice() == b"pieces")
    }

    /// `info` 是否含 `meta version` 键（v2 标记）。
    pub fn has_v2(&self) -> bool {
        self.info_keys
            .iter()
            .any(|key| key.as_slice() == b"meta version")
    }
}

/// 解析 `.torrent` 字节流，定位 `info` 字典。
pub fn parse_torrent(bytes: &[u8]) -> Result<TorrentMeta, BencodeError> {
    if bytes.first() != Some(&b'd') {
        return Err(BencodeError::TopLevelNotDictionary);
    }
    let mut cursor = 1usize;

    let mut info_span: Option<Range<usize>> = None;
    let mut info_keys: Vec<Vec<u8>> = Vec::new();

    loop {
        match bytes.get(cursor) {
            Some(b'e') => {
                cursor += 1;
                break;
            }
            Some(&marker @ b'0'..=b'9') => {
                let _ = marker;
                let key = read_string(bytes, &mut cursor)?;
                let value_start = cursor;
                skip_value(bytes, &mut cursor, 1)?;
                if key == b"info" {
                    info_span = Some(value_start..cursor);
                    info_keys = read_dictionary_keys(bytes, value_start, cursor)?;
                }
            }
            Some(other) => return Err(BencodeError::InvalidTypeMarker(*other)),
            None => return Err(BencodeError::UnexpectedEnd),
        }
    }

    if cursor != bytes.len() {
        // 顶层字典之后还有数据 => 不是合法的单值 bencode。
        return Err(BencodeError::UnexpectedEnd);
    }

    let info_span = info_span.ok_or(BencodeError::MissingInfoKey)?;
    Ok(TorrentMeta {
        info_span,
        info_keys,
    })
}

/// 校验整段输入恰好是一个合法的 bencode 值。
pub fn validate(bytes: &[u8]) -> Result<(), BencodeError> {
    let mut cursor = 0usize;
    skip_value(bytes, &mut cursor, 0)?;
    if cursor == bytes.len() {
        Ok(())
    } else {
        Err(BencodeError::UnexpectedEnd)
    }
}

/// 扫描并跳过当前游标处的值，游标停在值之后。
fn skip_value(bytes: &[u8], cursor: &mut usize, depth: usize) -> Result<(), BencodeError> {
    if depth > MAX_DEPTH {
        return Err(BencodeError::DepthLimitExceeded { limit: MAX_DEPTH });
    }
    match bytes.get(*cursor) {
        None => Err(BencodeError::UnexpectedEnd),
        Some(b'i') => {
            *cursor += 1;
            let start = *cursor;
            while let Some(&byte) = bytes.get(*cursor) {
                match byte {
                    b'e' => {
                        if !is_valid_integer(&bytes[start..*cursor]) {
                            return Err(BencodeError::InvalidInteger);
                        }
                        *cursor += 1;
                        return Ok(());
                    }
                    b'-' | b'0'..=b'9' => *cursor += 1,
                    _ => return Err(BencodeError::InvalidInteger),
                }
            }
            Err(BencodeError::UnexpectedEnd)
        }
        Some(b'l') => {
            *cursor += 1;
            loop {
                match bytes.get(*cursor) {
                    Some(b'e') => {
                        *cursor += 1;
                        return Ok(());
                    }
                    None => return Err(BencodeError::UnexpectedEnd),
                    _ => skip_value(bytes, cursor, depth + 1)?,
                }
            }
        }
        Some(b'd') => {
            *cursor += 1;
            loop {
                match bytes.get(*cursor) {
                    Some(b'e') => {
                        *cursor += 1;
                        return Ok(());
                    }
                    Some(&marker @ b'0'..=b'9') => {
                        let _ = marker;
                        read_string(bytes, cursor)?;
                        skip_value(bytes, cursor, depth + 1)?;
                    }
                    Some(_) => return Err(BencodeError::NonStringDictionaryKey),
                    None => return Err(BencodeError::UnexpectedEnd),
                }
            }
        }
        Some(&marker @ b'0'..=b'9') => {
            let _ = marker;
            read_string(bytes, cursor)?;
            Ok(())
        }
        Some(other) => Err(BencodeError::InvalidTypeMarker(*other)),
    }
}

/// 读取 `长度:内容` 形式的字节串。
fn read_string(bytes: &[u8], cursor: &mut usize) -> Result<Vec<u8>, BencodeError> {
    let start = *cursor;
    while let Some(&byte) = bytes.get(*cursor) {
        if byte == b':' {
            break;
        }
        if !byte.is_ascii_digit() {
            return Err(BencodeError::InvalidStringLength);
        }
        *cursor += 1;
    }
    if bytes.get(*cursor) != Some(&b':') {
        return Err(BencodeError::UnexpectedEnd);
    }

    let digits = &bytes[start..*cursor];
    if digits.is_empty() || digits.len() > 20 {
        return Err(BencodeError::InvalidStringLength);
    }
    let declared: u64 = std::str::from_utf8(digits)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(BencodeError::InvalidStringLength)?;

    *cursor += 1;
    let available = (bytes.len() - *cursor) as u64;
    if declared > available {
        return Err(BencodeError::StringLengthOutOfRange {
            declared,
            available,
        });
    }
    let end = *cursor + declared as usize;
    let out = bytes[*cursor..end].to_vec();
    *cursor = end;
    Ok(out)
}

/// 收集一个字典区间内的所有键。
fn read_dictionary_keys(
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<Vec<Vec<u8>>, BencodeError> {
    let mut keys = Vec::new();
    let mut cursor = start;
    if bytes.get(cursor) != Some(&b'd') {
        return Err(BencodeError::TopLevelNotDictionary);
    }
    cursor += 1;
    while cursor < end {
        match bytes.get(cursor) {
            Some(b'e') => break,
            Some(&marker @ b'0'..=b'9') => {
                let _ = marker;
                let key = read_string(bytes, &mut cursor)?;
                skip_value(bytes, &mut cursor, 1)?;
                keys.push(key);
            }
            Some(_) => return Err(BencodeError::NonStringDictionaryKey),
            None => return Err(BencodeError::UnexpectedEnd),
        }
    }
    Ok(keys)
}

fn is_valid_integer(digits: &[u8]) -> bool {
    let (negative, body) = match digits.first() {
        Some(b'-') => (true, &digits[1..]),
        _ => (false, digits),
    };
    if body.is_empty() || body.len() > MAX_INTEGER_DIGITS {
        return false;
    }
    if !body.iter().all(u8::is_ascii_digit) {
        return false;
    }
    // 拒绝前导零（`01`）与负零（`-0`）：两者都是 libtorrent 拒绝的畸形整数。
    // 注意 `-0` 的 body 就是单个 `0`，必须单独判定，否则会漏过。
    if body.len() > 1 && body[0] == b'0' {
        return false;
    }
    if negative && body == b"0" {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小合法单文件种子。
    const MINIMAL: &[u8] = b"d4:infod6:lengthi1024e4:name8:test.txt12:piece lengthi16384e6:pieces20:01234567890123456789ee";

    #[test]
    fn locates_info_span() {
        let meta = parse_torrent(MINIMAL).unwrap();
        let info = &MINIMAL[meta.info_span.clone()];
        assert!(info.starts_with(b"d"));
        assert!(info.ends_with(b"e"));
        assert!(meta.has_v1());
        assert!(!meta.has_v2());
        assert!(meta.info_keys.contains(&b"name".to_vec()));
    }

    #[test]
    fn info_span_is_exact_original_bytes() {
        let meta = parse_torrent(MINIMAL).unwrap();
        // 顶层 'd' 占 1 字节，"4:info" 占 6 字节。
        assert_eq!(meta.info_span.start, 7);
        assert_eq!(meta.info_span.end, MINIMAL.len() - 1);
    }

    #[test]
    fn detects_v2_only_torrent_without_pieces() {
        // 尾部三个 e 分别是：整数结束、info 字典结束、顶层字典结束。
        let bytes = b"d4:infod12:meta versioni2eee";
        let meta = parse_torrent(bytes).unwrap();
        assert!(!meta.has_v1());
        assert!(meta.has_v2());
        assert_eq!(meta.info_keys, vec![b"meta version".to_vec()]);
    }

    #[test]
    fn detects_v2_only_torrent_with_file_tree() {
        // 完整 v2 info（含 file tree），同样没有 pieces。
        let bytes = b"d4:infod9:file treed5:attrsl4:pathl1:aee6:lengthi1024ee12:meta versioni2e4:name5:t.txtee";
        let meta = parse_torrent(bytes).unwrap();
        assert!(!meta.has_v1());
        assert!(meta.has_v2());
        assert!(meta.info_keys.contains(&b"file tree".to_vec()));
    }

    #[test]
    fn rejects_missing_info_key() {
        let bytes = b"d8:announce15:http://tracker/e";
        assert_eq!(parse_torrent(bytes), Err(BencodeError::MissingInfoKey));
    }

    #[test]
    fn rejects_non_dictionary_top_level() {
        assert_eq!(
            parse_torrent(b"li1ee"),
            Err(BencodeError::TopLevelNotDictionary)
        );
    }

    #[test]
    fn rejects_truncated_input() {
        assert_eq!(validate(b"d4:infod"), Err(BencodeError::UnexpectedEnd));
        // "8:abcee" 声明 8 字节但只剩 5 字节，属于长度越界而非单纯截断。
        assert!(matches!(
            validate(b"d4:infod4:name8:abcee"),
            Err(BencodeError::StringLengthOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_overflowing_string_length() {
        let bytes = b"d4:infod999:abceee";
        assert!(matches!(
            parse_torrent(bytes),
            Err(BencodeError::StringLengthOutOfRange { .. })
        ));
    }

    #[test]
    fn rejects_deep_nesting() {
        let depth = MAX_DEPTH + 8;
        let mut bytes = vec![b'l'; depth];
        bytes.extend(std::iter::repeat(b'e').take(depth));
        assert!(matches!(
            validate(&bytes),
            Err(BencodeError::DepthLimitExceeded { .. })
        ));
    }

    #[test]
    fn rejects_malformed_integers() {
        assert_eq!(validate(b"i01e"), Err(BencodeError::InvalidInteger));
        assert_eq!(validate(b"i-0e"), Err(BencodeError::InvalidInteger));
        assert_eq!(validate(b"ie"), Err(BencodeError::InvalidInteger));
        assert_eq!(validate(b"i1x2e"), Err(BencodeError::InvalidInteger));
    }

    #[test]
    fn accepts_nested_lists_and_negative_integers() {
        // 由 parity/gen_bencode_fixtures.py 生成：[[["a", "b", -3]]]
        assert!(validate(b"lll1:a1:bi-3eeee").is_ok());
    }

    #[test]
    fn validates_minimal_torrent() {
        assert!(validate(MINIMAL).is_ok());
    }
}
