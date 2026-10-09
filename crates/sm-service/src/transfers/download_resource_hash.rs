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
//! | `.torrent` URL | 流式 GET + bencode 解析 infohash | 出网 + 最多 10 MiB |
//!
//! # 解析**不在本文件里**，在 [`svc_hash`]
//!
//! 哈希的规范化（40 位 hex / 32 位 base32 / magnet 里搜 `urn:btih:` /
//! `.torrent` 的 bencode + v1 infohash）已经由 `crates/svc-hash` 零依赖复刻过一遍
//! —— 它当初就是为了把 `libtorrent` 从这个项目里换掉而写的。
//!
//! 本文件只做三件事：**分流**（magnet / http(s)）、**接管网络**（抓 `.torrent`
//! 字节）、**把失败翻成服务层错误**（`from_resolve_error`）。别在这里再写一份
//! 解析 —— 那就是第二份公式，改一处漏一处。
//!
//! # hash **要校验长度与字符集**，不是拿来就用
//!
//! 合法的只有三种：40 位十六进制、32 位 base32（解出来就是 40 位 hex）、
//! magnet 里的 `urn:btih:` 段。其余一律 `422 invalid_download_resource_hash`
//! （上游 `canonical_info_hash`，`:15-21`）。
//!
//! 骨架期这里只做「trim + 小写」，于是**任何**垃圾串都被当成合法 hash 存下来：
//! 拿去比对永远不命中，表现为「黑名单静默失效」，而不是报错。
//!
//! ⚠️ **对外可见的行为变更**：拿不到 btih 的 magnet（tracker-only、
//! `btmh:` 的 v2 形态）现在报 `422 invalid_download_resource_hash`，
//! **不再放行**。上游 `:26-29` 就是 422 —— 「无法检查」在黑名单这件事上等于
//! 放行，被拉黑的资源换个 tracker-only 的 magnet 就能提交。
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

use svc_hash::ResolveError;

use crate::error::ServiceError;

/// `.torrent` 文件的大小上限（字节）。见模块文档。
///
/// **指向 [`svc_hash::MAX_TORRENT_BYTES`]**，不写第二份数字：那边复刻的是上游
/// 同一个常量，而且 `torrent_hash` 落地时会直接调那边的解析函数 —— 两处各写
/// 一个 `10 * 1024 * 1024` 正是那种「改一处漏一处」的起点。
pub const MAX_TORRENT_BYTES: usize = svc_hash::MAX_TORRENT_BYTES;

/// HTTP 重定向次数上限。见模块文档。同上，指向 [`svc_hash::MAX_HTTP_REDIRECTS`]。
pub const MAX_HTTP_REDIRECTS: usize = svc_hash::MAX_HTTP_REDIRECTS;

/// `svc-hash` 的失败 → 服务层错误。
///
/// # 状态码与错误码**直接取那张表**
///
/// `svc_hash::ResolveError::status_and_code` 是 Rust 与 Python 之间的硬契约
/// （见那个 crate 的模块文档），这里**不许重新映射一遍** —— 重映射就是第二份
/// 契约，哪天那边改了码，这边的分支会静默错位。
///
/// 中文提示也取自那边（`message()`）：它复刻的是上游 `ApiError` 的文案。
fn from_resolve_error(error: ResolveError) -> ServiceError {
    let (status, code) = error.status_and_code();
    ServiceError::from_status(status, code, error.message())
}

/// 规范化 info-hash：统一小写十六进制。**不合法 → 422。**
///
/// 上游 `canonical_info_hash`（`:15-21`）。三种输入，别的都拒：
///
/// | 输入 | 处理 |
/// |---|---|
/// | 40 位十六进制（`[0-9a-fA-F]{40}`） | 小写返回 |
/// | 32 位 base32（`[A-Za-z2-7]{32}`） | 折成大写 → base32 解码 → 40 位 hex |
/// | 其余 | `422 invalid_download_resource_hash` |
///
/// **必须小写** —— magnet URI 里大小写混用很常见，不归一就会让同一个资源在
/// 黑名单里存两条。
///
/// # 为什么必须校验，不能只 trim + 小写
///
/// 骨架期这里就是 `value.trim().to_ascii_lowercase()`，于是**任何**垃圾串都被
/// 当成合法 hash 存进黑名单比对 —— 而拿它去匹配永远不会命中，表现为「黑名单
/// 不生效」而不是报错。上游把「多少位、什么字符集」当判据，照抄。
pub fn canonical_info_hash(value: &str) -> Result<String, ServiceError> {
    svc_hash::canonical_info_hash(value).map_err(from_resolve_error)
}

/// 从 magnet URI 里取 info-hash。**取不到 → 422。**
///
/// 上游 `_magnet_hash`（`:24-30`）两步：
///
/// 1. 先 `unquote`（百分号解码）—— `%75rn%3Abtih%3A...` 这种编码过的 btih 也认；
/// 2. 再不区分大小写地搜 `urn:btih:`，**在整串里搜**，不是解析 `xt=` 参数。
///    搜不到就是 `422 invalid_download_resource_hash`。
///
/// # 这里曾经「放行」过 tracker-only 的 magnet —— 那是错的
///
/// 骨架期这个函数返回 `Option`，理由是「tracker-only 是合法 BT 形态，报错会
/// 让它们都用不了」。但上游 `:26-29` **明确报 422**：拿不到 hash 就无法做黑名单
/// 匹配，而「无法检查」在黑名单这件事上等于放行 —— 于是被拉黑的资源换个
/// tracker-only 的 magnet 就能提交。返回 `Option` 还把「非 magnet 输入」和
/// 「magnet 但没有 btih」混成一种结果，前者该报 `invalid_download_source`、
/// 后者该报 `invalid_download_resource_hash`。
fn magnet_hash(source_uri: &str) -> Result<String, ServiceError> {
    svc_hash::magnet_info_hash(source_uri).map_err(from_resolve_error)
}

/// 从 `.torrent` URL 算 info-hash。**需要出网**。
///
/// 是 `async` 的：**实现时要出网**（流式 GET + 限 10 MiB + 5 次重定向）。
/// 骨架期 `todo!()` 不返回，签名先按终态写，免得实现时改调用点。
///
/// # 落地时用 `svc_hash::torrent_v1_info_hash`，**不要**引 libtorrent
///
/// 这个函数的文档原来写着「需要 libtorrent」—— 那是骨架期的错误前提。
/// `crates/svc-hash` 已经用纯 Rust 复刻了 bencode 解析与 v1 info hash
/// （`svc_hash::torrent_v1_info_hash`），`libtorrent` 正是它替代掉的东西。
/// 所以落地时是「reqwest 抓字节 → 交给 svc-hash」，与上游
/// 「httpx 抓字节 → 交给 `_torrent_hash`」一一对应（上游那边用的是 libtorrent，
/// 本仓换成 svc-hash，语义等价）。
async fn torrent_hash(source_uri: &str) -> Result<String, ServiceError> {
    let _ = source_uri;
    todo!("骨架：流式 GET（限 10MiB / 5 次重定向）+ libtorrent 解析 infohash")
}

/// 取资源哈希。magnet 走本地，torrent 走出网。
///
/// 上游 `resolve_resource_hash`（`:47-50`）：**以 `magnet:` 开头就走 magnet
/// 那一路，由它决定报什么错**。所以「magnet 但没有 btih」拿到的是
/// `invalid_download_resource_hash`，不会被 fallthrough 成
/// `invalid_download_source` —— 后者会把「你的磁力链接少了 hash」说成
/// 「协议不支持」。
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
    // 上游是 `source_uri.lower().startswith("magnet:")` —— **大小写不敏感**，
    // `MAGNET:?xt=...` 也走这一路。这个判据用 `svc_hash::is_magnet`：它复刻的
    // 正是上游那句（含 trim 与折小写），别在这里手写第二份。
    if svc_hash::is_magnet(trimmed) {
        return magnet_hash(trimmed);
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
        let uri = "magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567";
        assert_eq!(
            magnet_hash(uri).expect("合法 40 位 hex"),
            "0123456789abcdef0123456789abcdef01234567"
        );
    }

    /// magnet 取 hash 是**零网络**的：以下断言不碰任何 IO。
    #[test]
    fn magnet_uris_yield_their_hash_without_network() {
        let uri = "magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567&dn=some+movie";
        assert_eq!(
            magnet_hash(uri).expect("普通 magnet"),
            "0123456789abcdef0123456789abcdef01234567"
        );
    }

    /// ★ 百分号编码过的 btih 也要认 —— 上游先 `unquote` 再正则（`:25`）。
    #[test]
    fn magnet_hash_survives_percent_encoding() {
        let encoded = "magnet:?xt=%75rn%3Abtih%3A0123456789ABCDEF0123456789ABCDEF01234567";
        assert_eq!(
            magnet_hash(encoded).expect("编码过的 btih"),
            "0123456789abcdef0123456789abcdef01234567"
        );
    }

    /// ★ 32 位 base32 解出来必须等于对应的十六进制（大小写输入都认）。
    ///
    /// 把手写解码器的结果与「同一串字节的 hex 写法」对拍 —— 两条路径互相独立，
    /// 比单看一个常数更能说明问题。
    ///
    /// 期望串是**按 RFC 4648 手推**的：字节串
    /// `0123456789abcdef0123456789abcdef01234567` 每 5 个字节（40 bit）折成
    /// 8 个字符，得 `AERUKZ4J` `VPG66AJD` `IVTYTK6N` `54ASGRLH`。
    #[test]
    fn base32_form_decodes_to_the_same_hex() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let base32 = "AERUKZ4JVPG66AJDIVTYTK6N54ASGRLH";
        assert_eq!(base32.len(), 32);
        assert_eq!(canonical_info_hash(base32).expect("base32"), hex);
        // 上游先 `.upper()`（Python 的 `b32decode` 默认不 casefold），小写输入要认。
        assert_eq!(
            canonical_info_hash(&base32.to_ascii_lowercase()).expect("小写 base32"),
            hex
        );
    }

    /// ★ 长度或字符集不对 → `422 invalid_download_resource_hash`。
    ///
    /// 这条用例锁的是「别退回 trim + 小写」：那种实现会把下面每一条都当成合法
    /// hash 放过去，然后黑名单比对永不命中 —— 不报错，只是静默失效。
    #[test]
    fn malformed_hashes_are_rejected_not_silently_lowered() {
        for bad in [
            "",                                          // 空
            "a1b2c3",                                    // 太短（骨架期用例用的就是这个）
            "ABCDEF0123456789",                          // 16 位：既不是 40 hex 也不是 32 base32
            "0123456789abcdef0123456789abcdef0123456",   // 39 位
            "0123456789abcdef0123456789abcdef012345678", // 41 位
            "0123456789abcdef0123456789abcdef0123456g",  // 非十六进制
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ23456",           // 31 位 base32
            "AERUKZ4JVPG66AJDIVTYTK6N54ASGR0H",          // 32 位但含 '0'（不在 2-7 里）
        ] {
            let error = canonical_info_hash(bad).expect_err("应被拒");
            assert_eq!(
                error.code(),
                "invalid_download_resource_hash",
                "{bad:?} 不该被当成合法 hash"
            );
        }
    }

    /// ★ tracker-only magnet（无 btih）→ **422**，不是放行。
    ///
    /// 上游 `:26-29`。这里曾经返回 `None`，注释写着「tracker-only 是合法 BT
    /// 形态，报错会让它们都用不了」—— 但「无法做黑名单匹配」在这件事上等于
    /// 放行：被拉黑的资源换个 tracker-only 的 magnet 就能提交。
    #[test]
    fn a_magnet_without_an_info_hash_is_rejected() {
        let error = magnet_hash("magnet:?xt=urn:btmh:1220aaaa&dn=x").expect_err("没有 btih 就该拒");
        assert_eq!(error.code(), "invalid_download_resource_hash");
    }

    /// `btih:` 后面跟的不是合法 hash → 同一个码（上游 `:30` 转 `canonical_info_hash` 再抛）。
    #[test]
    fn a_magnet_whose_btih_is_not_a_hash_is_rejected() {
        let error = magnet_hash("magnet:?xt=urn:btih:a1b2c3").expect_err("长度不对");
        assert_eq!(error.code(), "invalid_download_resource_hash");
    }

    /// 非 magnet / 非 http 的输入 → 422 `invalid_download_source`。
    ///
    /// `#[tokio::test]`：`resolve_resource_hash` 是 async（`torrent_hash`
    /// 那一路要出网）。这条用例走不到那一路，但 future 仍必须 await 才有
    /// `Result`。
    #[tokio::test]
    async fn an_unsupported_scheme_is_rejected_with_the_dedicated_code() {
        let error = resolve_resource_hash("ftp://example.com/x.torrent")
            .await
            .expect_err("非 http(s) 应被拒");
        assert_eq!(error.code(), "invalid_download_source");
    }

    /// ★ magnet 分流**大小写不敏感**：`MAGNET:` 也走 magnet 那一路，而不是
    /// 掉进「不是 http(s)」的 `invalid_download_source`（上游 `:49` 用的是
    /// `source_uri.lower().startswith("magnet:")`）。
    #[tokio::test]
    async fn the_magnet_branch_is_case_insensitive() {
        let uri = "MAGNET:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567";
        assert_eq!(
            resolve_resource_hash(uri).await.expect("大写也是 magnet"),
            "0123456789abcdef0123456789abcdef01234567"
        );
    }

    /// ★ magnet 但没 hash → `invalid_download_resource_hash`，
    /// **不是** `invalid_download_source`。
    ///
    /// 上游 `:49-50` 是「以 `magnet:` 开头就直接交给 `_magnet_hash`」，没有
    /// fallthrough。混起来会把「你的磁力链接少了 hash」说成「协议不支持」。
    #[tokio::test]
    async fn a_hashless_magnet_does_not_fall_through_to_the_scheme_error() {
        let error = resolve_resource_hash("magnet:?dn=x")
            .await
            .expect_err("没有 hash");
        assert_eq!(error.code(), "invalid_download_resource_hash");
    }
}
