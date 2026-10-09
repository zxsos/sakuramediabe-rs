//! 提交前识别 BT 资源的主机黑名单（上游 `downloads/resource_hash.py`，94 行）。
//!
//! # 目的是「提交之前」拦住黑名单，不是提交之后删
//!
//! 资源哈希必须在**下载器拿到种子之前**算出来 —— 因为一旦提交进去，
//! 下载器就开始下载了，删任务也追不回已落盘的数据。
//!
//! # 两条路径：magnet 与 torrent 文件
//!
//! | 输入 | 怎么取 hash | 代价 |
//! |---|---|---|
//! | `magnet:?xt=urn:btih:...` | 直接读 URI 里的哈希 | **零网络** |
//! | `.torrent` URL | 流式 GET + libtorrent 解析 infohash | 出网 + 最多 10 MiB |
//!
//! # 三个上限是安全边界，不是调优项
//!
//! | 常量 | 值 | 挡什么 |
//! |---|---|---|
//! | [`MAX_TORRENT_BYTES`] | 10 MiB | 恶意/畸形种子撑爆内存 |
//! | [`MAX_HTTP_REDIRECTS`] | 5 | 重定向环 |
//!
//! `MAX_TORRENT_BYTES` 必须在**流式读取过程中**计数，不能先下完再判大小 ——
//! 那就失去意义了。
//!
//! # 出网失败映射成 503 而不是 422
//!
//! 拉不到种子文件是**上游不可用**，不是用户请求写错了。所以是
//! `download_source_unavailable`（503）；只有「URI 根本不是 http(s)」
//! 才是 422（`invalid_download_source`）。

use crate::error::ServiceError;

/// `.torrent` 文件的大小上限（字节）。见模块文档。
pub const MAX_TORRENT_BYTES: usize = 10 * 1024 * 1024;

/// HTTP 重定向次数上限。见模块文档。
pub const MAX_HTTP_REDIRECTS: usize = 5;

/// 规范化 info-hash：统一小写十六进制。
///
/// 上游 `canonical_info_hash`。**必须小写** —— BT 的 info-hash 惯例是
/// 十六进制但大小写混用（magnet URI 里尤其常见），不归一就会让同一个
/// 资源在黑名单里存了两条。
pub fn canonical_info_hash(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// 从 magnet URI 里取 info-hash。**非 magnet 或缺 hash → `None`**。
///
/// 缺 hash 的 magnet 是合法的 BT 形态（tracker-only），但**无法做黑名单匹配**
/// —— 只能选择放行。返回 `None` 让调用方决定，不要报错：报错会让所有
/// tracker-only 的种子都用不了。
fn magnet_hash(source_uri: &str) -> Option<String> {
    let query = source_uri.strip_prefix("magnet:")?;
    query.split('&').find_map(|pair| {
        let value = pair.strip_prefix("urn:btih:")?;
        Some(canonical_info_hash(value))
    })
}

/// 从 `.torrent` URL 算 info-hash。**需要出网 + libtorrent**。
fn torrent_hash(source_uri: &str) -> Result<String, ServiceError> {
    let _ = source_uri;
    todo!("骨架：流式 GET（限 10MiB / 5 次重定向）+ libtorrent 解析 infohash")
}

/// 取资源哈希。magnet 走本地，torrent 走出网。
///
/// 错误码（照上游）：
///
/// | 情况 | 码 |
/// |---|---|
/// | 两者都取不到 | `422 invalid_download_resource_hash` |
/// | 非 http(s) scheme | `422 invalid_download_source` |
/// | 上游 5xx / 连接失败 | `503 download_source_unavailable` |
/// | 404 | `404 download_source_not_found` |
/// | 超过 10 MiB | `422 download_torrent_too_large` |
pub async fn resolve_resource_hash(source_uri: &str) -> Result<String, ServiceError> {
    let trimmed = source_uri.trim();
    if let Some(hash) = magnet_hash(trimmed) {
        return Ok(hash);
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return torrent_hash(trimmed).await;
    }
    Err(ServiceError::validation(
        "invalid_download_source",
        "source_uri 既不是 magnet 也不是 http(s) 地址",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hash 必须**归一为小写** —— 大小写混用会让同一个资源在黑名单里存两条。
    #[test]
    fn hashes_are_canonicalized_to_lowercase() {
        assert_eq!(canonical_info_hash("  A1B2C3  "), "a1b2c3");
    }

    /// magnet 取 hash 是**零网络**的：以下断言不碰任何 IO。
    #[test]
    fn magnet_uris_yield_their_hash_without_network() {
        let uri = "magnet:?xt=urn:btih:ABCDEF0123456789&dn=some+movie";
        assert_eq!(magnet_hash(uri).as_deref(), Some("abcdef0123456789"));
    }

    /// tracker-only magnet（无 btih）返回 `None` 而**不报错**。
    ///
    /// 报错会让所有 tracker-only 种子都用不了 —— 而它们是合法 BT 形态。
    #[test]
    fn a_magnet_without_an_info_hash_is_not_an_error() {
        assert_eq!(magnet_hash("magnet:?xt=urn:btmh:1220aaaa&dn=x"), None);
    }

    /// 非 magnet / 非 http 的输入 → 422 `invalid_download_source`。
    #[test]
    fn an_unsupported_scheme_is_rejected_with_the_dedicated_code() {
        let error = resolve_resource_hash("ftp://example.com/x.torrent")
            .expect_err("非 http(s) 应被拒");
        assert_eq!(error.code(), "invalid_download_source");
    }
}
