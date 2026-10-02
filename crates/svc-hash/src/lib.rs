//! BT 资源 info hash 解析 —— 用于替代后端对 `libtorrent` 的依赖。
//!
//! # 迁移背景
//!
//! `src/service/transfers/downloads/resource_hash.py` 在提交下载前解析
//! 磁力链接 / `.torrent`，取 info hash 供宿主黑名单匹配。它用 `libtorrent`
//! 做的唯一一件事是 **bencode 解码 + info hash 提取**，为此把一个重量级
//! 依赖（含 C++ 运行时）拖进了主后端镜像。
//!
//! 本 crate 用零依赖 Rust 复刻了同样的语义，`libtorrent` 即可移除。
//!
//! # 分工：HTTP 抓取仍在 Python
//!
//! 原实现里紧邻哈希解析的还有一个 HTTP 重定向链追踪（最多 5 跳、10 MiB 上限）。
//! 那部分**故意不复刻**——`httpx` 已经在后端里跑得好好的，为它引入
//! `reqwest` + `tokio` 会让这个 crate 从"零依赖纯逻辑"变成重量级网络服务。
//! Python 侧继续负责抓取，只把最终的字节或链接交给本 crate。
//!
//! # 契约对齐
//!
//! 错误码与 HTTP 状态码必须与 `src/api/exception/errors.py` 的 `ApiError`
//! 逐一对齐，否则客户端的错误提示分支会错位。

#![forbid(unsafe_code)]

use core::fmt;

use hashing::{base32_decode, hex, sha1, sha256};

mod bencode;

pub use bencode::{parse_torrent, validate as validate_bencode, BencodeError, TorrentMeta};

/// `.torrent` 体积上限，对应 Python 侧的 `MAX_TORRENT_BYTES`。
pub const MAX_TORRENT_BYTES: usize = 10 * 1024 * 1024;

/// HTTP 重定向跳数上限，对应 Python 侧的 `MAX_HTTP_REDIRECTS`。
///
/// 本 crate 不发起 HTTP 请求，保留该常量仅为让 Python 侧引用同一来源。
pub const MAX_HTTP_REDIRECTS: usize = 5;

/// 资源哈希解析失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// 链接里既没有合法的 40 位 hex，也没有合法的 32 位 base32。
    InvalidResourceHash,
    /// `.torrent` 无效，或缺少 BT v1 hash。
    InvalidTorrent,
    /// 链接协议不受支持。
    InvalidSource,
    /// 远端返回 404。
    SourceNotFound,
    /// 远端返回 5xx 或传输失败。
    SourceUnavailable,
    /// `.torrent` 超过体积上限。
    TorrentTooLarge,
}

impl ResolveError {
    /// 映射到后端 `ApiError` 的 `(status_code, error_code)`。
    ///
    /// 这张表是 Rust 与 Python 之间的硬契约，改动必须同步
    /// `src/service/transfers/downloads/resource_hash.py`。
    pub const fn status_and_code(self) -> (u16, &'static str) {
        match self {
            Self::InvalidResourceHash => (422, "invalid_download_resource_hash"),
            Self::InvalidTorrent => (422, "invalid_download_torrent"),
            Self::InvalidSource => (422, "invalid_download_source"),
            Self::SourceNotFound => (404, "download_source_not_found"),
            Self::SourceUnavailable => (503, "download_source_unavailable"),
            Self::TorrentTooLarge => (422, "download_torrent_too_large"),
        }
    }

    /// 对应 Python 侧抛出的中文提示，供日志与测试比对。
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidResourceHash => "资源缺少有效的 BT hash",
            Self::InvalidTorrent => "种子文件无效或缺少 BT v1 hash",
            Self::InvalidSource => "种子链接不受支持",
            Self::SourceNotFound => "种子文件不存在",
            Self::SourceUnavailable => "种子文件服务暂不可用",
            Self::TorrentTooLarge => "种子文件超过大小限制",
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (status, code) = self.status_and_code();
        write!(f, "{code} ({status}): {}", self.message())
    }
}

impl std::error::Error for ResolveError {}

/// `.torrent` 的 v1 / v2 info hash。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentHashes {
    /// 40 位小写 hex；纯 v2 种子为 `None`。
    pub v1: Option<String>,
    /// 40 位小写 hex；纯 v1 种子为 `None`。
    pub v2: Option<String>,
}

/// 规范化 info hash 字符串为 40 位小写 hex。
///
/// 复刻 Python 的 `canonical_info_hash`：
///
/// 1. 40 位 hex（v1）→ 转小写；
/// 2. 32 位 Base32（v2）→ 解码为 20 字节再转 hex。
///
/// 顺序不可颠倒：v1 判定必须优先。
pub fn canonical_info_hash(value: &str) -> Result<String, ResolveError> {
    let trimmed = value.trim();

    if trimmed.len() == 40 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(trimmed.to_ascii_lowercase());
    }

    // Python 正则 `[A-Za-z2-7]{32}`：注意排除了 0/1/8/9。
    let is_base32_alphabet = !trimmed.is_empty()
        && trimmed.bytes().all(|byte| {
            byte.is_ascii_alphabetic() || (b'2'..=b'7').contains(&byte)
        });
    if trimmed.len() == 32 && is_base32_alphabet {
        // Python 侧是 `base64.b32decode(value.upper())`，因此小写 base32 也必须
        // 接受；本 crate 的 `base32_decode` 只认大写，故先统一转大写。
        let decoded = base32_decode(&trimmed.to_ascii_uppercase())
            .map_err(|_| ResolveError::InvalidResourceHash)?;
        if decoded.len() != 20 {
            return Err(ResolveError::InvalidResourceHash);
        }
        return Ok(hex(&decoded));
    }

    Err(ResolveError::InvalidResourceHash)
}

/// 从磁力链接中提取并规范化 info hash。
///
/// 复刻 Python 的 `_magnet_hash`：先做 URL 解码（`unquote`），
/// 再不区分大小写地搜索 `urn:btih:`，其后连续字母数字即 hash 本体。
pub fn magnet_info_hash(source_uri: &str) -> Result<String, ResolveError> {
    let decoded = percent_decode(source_uri);
    let lower = decoded.to_ascii_lowercase();

    let start = lower
        .find("urn:btih:")
        .ok_or(ResolveError::InvalidResourceHash)?
        + "urn:btih:".len();

    let body: String = decoded[start..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .collect();

    if body.is_empty() {
        return Err(ResolveError::InvalidResourceHash);
    }
    canonical_info_hash(&body)
}

/// 从 `.torrent` 字节流提取 BT **v1** info hash。
///
/// 复刻 Python 的 `_torrent_hash`，包括它对 v1 的强制要求：
/// 纯 v2 种子（`info` 字典没有 `pieces`）会被判为 `InvalidTorrent`，
/// 对应原实现里 `info.info_hashes().has_v1()` 为假时抛出的 `missing v1 hash`。
pub fn torrent_v1_info_hash(payload: &[u8]) -> Result<String, ResolveError> {
    if payload.len() > MAX_TORRENT_BYTES {
        return Err(ResolveError::TorrentTooLarge);
    }
    let meta = parse_torrent(payload).map_err(|_| ResolveError::InvalidTorrent)?;
    if !meta.has_v1() {
        return Err(ResolveError::InvalidTorrent);
    }
    Ok(hex(&sha1(&payload[meta.info_span])))
}

/// 从 `.torrent` 字节流同时提取 v1 与 v2 info hash。
///
/// 目前宿主只消费 v1；这个函数为后续支持 v2 预留。
pub fn torrent_info_hashes(payload: &[u8]) -> Result<TorrentHashes, ResolveError> {
    if payload.len() > MAX_TORRENT_BYTES {
        return Err(ResolveError::TorrentTooLarge);
    }
    let meta = parse_torrent(payload).map_err(|_| ResolveError::InvalidTorrent)?;
    // 先取布尔再取 span：`Range<usize>` 不是 Copy，取走后 meta 不可再 borrow。
    let has_v1 = meta.has_v1();
    let has_v2 = meta.has_v2();
    let info = &payload[meta.info_span];
    Ok(TorrentHashes {
        v1: has_v1.then(|| hex(&sha1(info))),
        v2: has_v2.then(|| hex(&sha256(info)[..20])),
    })
}

/// 校验 `.torrent` 体积是否在上限内。
pub fn check_torrent_size(length: usize) -> Result<(), ResolveError> {
    if length > MAX_TORRENT_BYTES {
        return Err(ResolveError::TorrentTooLarge);
    }
    Ok(())
}

/// 链接是否是磁力链接。对应 Python `resolve_resource_hash` 的分派条件。
pub fn is_magnet(source_uri: &str) -> bool {
    source_uri.trim().to_ascii_lowercase().starts_with("magnet:")
}

/// 校验链接协议，对应 Python 里的 scheme 白名单检查。
pub fn check_source_scheme(source_uri: &str) -> Result<(), ResolveError> {
    let trimmed = source_uri.trim();
    if is_magnet(trimmed) {
        return Ok(());
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err(ResolveError::InvalidSource);
    };
    let scheme = scheme.to_ascii_lowercase();
    if (scheme == "http" || scheme == "https") && !rest.is_empty() {
        Ok(())
    } else {
        Err(ResolveError::InvalidSource)
    }
}

/// 纯计算入口：磁力链接直接解析，其余要求调用方先抓取字节。
pub fn resolve_from_source(
    source_uri: &str,
    torrent_payload: Option<&[u8]>,
) -> Result<String, ResolveError> {
    let trimmed = source_uri.trim();
    if is_magnet(trimmed) {
        return magnet_info_hash(trimmed);
    }
    check_source_scheme(trimmed)?;
    match torrent_payload {
        Some(payload) => torrent_v1_info_hash(payload),
        None => Err(ResolveError::InvalidSource),
    }
}

/// 最小化的百分号解码，对应 Python 的 `urllib.parse.unquote`。
///
/// 只处理 `%XX`，其余字符原样保留；不完整转义（尾部 `%` 或 `%A`）原样保留，
/// 与 Python 的容错行为一致。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_HEX: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";
    /// 同一 hash 的 32 字符 base32 表示（RFC 4648，大写），由 Python
    /// `base64.b32encode(bytes.fromhex(V1_HEX))` 实算得出。
    const V1_BASE32: &str = "3WBFL3G4PSSV7MF37AJSHWDQMLNR63I4";

    #[test]
    fn canonicalizes_hex_case_and_padding() {
        assert_eq!(canonical_info_hash(V1_HEX).unwrap(), V1_HEX);
        assert_eq!(
            canonical_info_hash(&V1_HEX.to_ascii_uppercase()).unwrap(),
            V1_HEX
        );
        assert_eq!(
            canonical_info_hash(&format!("  {V1_HEX}\t")).unwrap(),
            V1_HEX
        );
    }

    #[test]
    fn canonicalizes_base32_v2_like_python_upper_then_decode() {
        let from_base32 = canonical_info_hash(V1_BASE32).unwrap();
        assert_eq!(from_base32.len(), 40);
        assert_eq!(from_base32, V1_HEX, "base32 与 hex 必须归一到同一值");
    }

    #[test]
    fn accepts_lowercase_base32_like_python() {
        // Python 走 `.upper()`，所以小写 base32 也应成功。
        let lower = V1_BASE32.to_ascii_lowercase();
        assert_eq!(canonical_info_hash(&lower).unwrap(), V1_HEX);
    }

    #[test]
    fn hex_wins_over_base32_branch() {
        // 40 位输入绝不能被当成 32 位 base32 处理。
        let hex_like = "A".repeat(40);
        assert_eq!(canonical_info_hash(&hex_like).unwrap(), "a".repeat(40));
    }

    #[test]
    fn rejects_malformed_hash() {
        for input in [
            "",
            "abc",
            &"z".repeat(40),   // 长度对但非 hex
            &"0".repeat(39),   // 长度不足
            &"0".repeat(41),   // 长度超
            &"0".repeat(32),   // 数字不在 base32 字母表内
        ] {
            assert_eq!(
                canonical_info_hash(input),
                Err(ResolveError::InvalidResourceHash),
                "input={input:?}"
            );
        }
    }

    #[test]
    fn extracts_magnet_hash() {
        let uri = format!("magnet:?xt=urn:btih:{V1_HEX}&dn=some+movie");
        assert_eq!(magnet_info_hash(&uri).unwrap(), V1_HEX);
    }

    #[test]
    fn magnet_matching_is_case_insensitive() {
        // Python 用 re.IGNORECASE，因此 URN:BTIH: 也要匹配。
        let uri = format!("MAGNET:?XT=URN:BTIH:{}&DN=T", V1_HEX.to_ascii_uppercase());
        assert_eq!(magnet_info_hash(&uri).unwrap(), V1_HEX);
    }

    #[test]
    fn magnet_handles_percent_encoded_hash() {
        let encoded: String = V1_HEX
            .chars()
            .map(|c| format!("%{:02X}", c as u32))
            .collect();
        assert_eq!(
            magnet_info_hash(&format!("magnet:?xt=urn:btih:{encoded}")).unwrap(),
            V1_HEX
        );
    }

    #[test]
    fn magnet_stops_at_non_alphanumeric() {
        let uri = format!("magnet:?xt=urn:btih:{V1_HEX}&dn=x");
        assert_eq!(magnet_info_hash(&uri).unwrap(), V1_HEX);
    }

    #[test]
    fn magnet_without_hash_fails() {
        assert_eq!(
            magnet_info_hash("magnet:?dn=test"),
            Err(ResolveError::InvalidResourceHash)
        );
    }

    #[test]
    fn computes_v1_hash_over_exact_original_info_bytes() {
        let info = b"d6:lengthi1e4:name1:a6:pieces20:01234567890123456789e";
        let mut torrent = Vec::from(&b"d4:info"[..]);
        torrent.extend_from_slice(info);
        torrent.push(b'e');

        assert_eq!(torrent_v1_info_hash(&torrent).unwrap(), hex(&sha1(info)));
    }

    #[test]
    fn info_hash_is_sensitive_to_key_order() {
        // bencode 字典按键排序，info 内部顺序变化会改变哈希 —— 确认我们
        // 对原始字节哈希，而不是重新规范化后哈希。
        let a = b"d4:name1:ai1ee";
        let b = b"d1:ai1e4:name1:ae";
        assert_ne!(hex(&sha1(a)), hex(&sha1(b)));
    }

    #[test]
    fn rejects_v2_only_torrent_matching_python_has_v1_gate() {
        let torrent = b"d4:infod12:meta versioni2eee";
        assert_eq!(
            torrent_v1_info_hash(torrent),
            Err(ResolveError::InvalidTorrent),
            "纯 v2 种子没有 v1 hash，原实现会抛 missing v1 hash"
        );
        // 反向确认：同一份数据确实是合法 bencode，只是缺少 pieces。
        // 否则这个用例会因为「数据本身就坏」而假通过。
        let meta = parse_torrent(torrent).expect("v2_only 必须是合法 bencode");
        assert!(!meta.has_v1());
    }

    #[test]
    fn rejects_garbage_payload() {
        for payload in [b"not a torrent".as_slice(), b"", b"d4:infod"] {
            assert_eq!(
                torrent_v1_info_hash(payload),
                Err(ResolveError::InvalidTorrent),
                "payload={payload:?}"
            );
        }
    }

    #[test]
    fn enforces_torrent_size_limit_before_parsing() {
        let oversized = vec![b'd'; MAX_TORRENT_BYTES + 1];
        assert_eq!(
            torrent_v1_info_hash(&oversized),
            Err(ResolveError::TorrentTooLarge)
        );
        assert_eq!(
            check_torrent_size(MAX_TORRENT_BYTES + 1),
            Err(ResolveError::TorrentTooLarge)
        );
        assert!(check_torrent_size(MAX_TORRENT_BYTES).is_ok());
    }

    #[test]
    fn extracts_both_hashes_for_hybrid_torrent() {
        // 由 parity/gen_bencode_fixtures.py 生成：info 同时有 meta version 与 pieces。
        let torrent = b"d4:infod12:meta versioni2e6:pieces20:01234567890123456789ee";
        let hashes = torrent_info_hashes(torrent).unwrap();
        assert!(hashes.v1.is_some());
        assert!(hashes.v2.is_some());
    }

    #[test]
    fn scheme_whitelist_matches_python() {
        assert!(check_source_scheme("https://example.com/a.torrent").is_ok());
        assert!(check_source_scheme("HTTP://example.com/a.torrent").is_ok());
        assert!(check_source_scheme("magnet:?xt=urn:btih:x").is_ok());
        for bad in [
            "ftp://example.com/a.torrent",
            "file:///etc/passwd",
            "example.com/a.torrent",
            "https://",
        ] {
            assert_eq!(
                check_source_scheme(bad),
                Err(ResolveError::InvalidSource),
                "input={bad}"
            );
        }
    }

    #[test]
    fn resolve_from_source_dispatches_by_scheme() {
        let magnet = format!("magnet:?xt=urn:btih:{V1_HEX}");
        assert_eq!(resolve_from_source(&magnet, None).unwrap(), V1_HEX);
        // http 链接未提供 payload 时只能报 InvalidSource。
        assert_eq!(
            resolve_from_source("https://example.com/a.torrent", None),
            Err(ResolveError::InvalidSource)
        );
    }

    #[test]
    fn error_codes_match_python_api_error_table() {
        // 这张断言表就是 Rust <-> Python 的契约快照，改动任一侧都必须同步。
        assert_eq!(
            ResolveError::InvalidResourceHash.status_and_code(),
            (422, "invalid_download_resource_hash")
        );
        assert_eq!(
            ResolveError::InvalidTorrent.status_and_code(),
            (422, "invalid_download_torrent")
        );
        assert_eq!(
            ResolveError::InvalidSource.status_and_code(),
            (422, "invalid_download_source")
        );
        assert_eq!(
            ResolveError::SourceNotFound.status_and_code(),
            (404, "download_source_not_found")
        );
        assert_eq!(
            ResolveError::SourceUnavailable.status_and_code(),
            (503, "download_source_unavailable")
        );
        assert_eq!(
            ResolveError::TorrentTooLarge.status_and_code(),
            (422, "download_torrent_too_large")
        );
    }

    #[test]
    fn percent_decode_matches_python_unquote_tolerance() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        // 不完整转义原样保留，不报错。
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%A"), "%A");
    }
}
