//! 资源签名 URL —— 复刻 `src/common/file_signatures.py`。
//!
//! # 为什么单列一个模块
//!
//! 图片 / 媒体播放 / 片段 / 字幕走的是**旁路鉴权**：这些 URL 由前端直接
//! 交给 `<img>` 或播放器，带不了 `Authorization` 头，所以改用 HMAC 签名的
//! 查询参数。JWT 提取器在这里**不适用**，把它当成"没鉴权"是个容易犯的错。
//!
//! # 关键语义（改任何一条都会让已签出的 URL 失效）
//!
//! | 项 | 值 |
//! |---|---|
//! | 有效期 | 12 小时 |
//! | 过期时间戳 | **向上对齐到 6 小时窗口边界** |
//! | 算法 | HMAC-SHA256，十六进制输出 |
//! | 过期 | 403 `file_signature_expired` |
//! | 签名不符 | 403 `file_signature_invalid` |
//! | 路径非法 | 403 `file_path_invalid` |
//!
//! **窗口对齐不是优化。** 上游注释写得很清楚：同一窗口内签出的 URL 完全
//! 一致，浏览器和 CDN 才能真正命中缓存；向上取整保证实际有效期**不低于**
//! 12 小时，所以不存在"签出即过期"的边界问题。改成向下取整或去掉对齐，
//! 缓存命中率会掉，或者出现刚签出就过期的 URL。
//!
//! # 签名载荷必须逐字符一致
//!
//! ```text
//! images:{path}:{expires}
//! media:{media_id}:{resource_path}:{expires}
//! merged-media:{id1,id2}:{resource_path}:{expires}
//! clip:{clip_id}:{expires}
//! subtitles:{subtitle_id}:{expires}
//! ```
//!
//! 差一个冒号就会让所有已签出的 URL 变成 `file_signature_invalid`。
//! 本模块的测试用 Python `hmac` 生成的向量钉住了这五种格式。

use crate::hashing_support::{constant_time_eq, hex, hmac_sha256};

pub const IMAGE_FILE_ROUTE_PREFIX: &str = "/files/images";
pub const MEDIA_PLAY_ROUTE_PREFIX: &str = "/media";
pub const MERGED_MEDIA_PLAY_ROUTE_PREFIX: &str = "/media/merged-play";
pub const MEDIA_CLIP_STREAM_ROUTE_PREFIX: &str = "/media-clips";
pub const SUBTITLE_FILE_ROUTE_PREFIX: &str = "/files/subtitles";

/// 签名有效期：12 小时。
pub const FILE_SIGNATURE_EXPIRE_SECONDS: i64 = 12 * 60 * 60;
/// 过期时间戳的对齐窗口：6 小时。
pub const FILE_SIGNATURE_ALIGN_WINDOW_SECONDS: i64 = 6 * 60 * 60;

/// 生成窗口对齐的过期时间戳。
///
/// 对应上游 `-(-target // window) * window`，即**向上**取整。
pub fn signature_expires(now_seconds: i64) -> i64 {
    let target = now_seconds + FILE_SIGNATURE_EXPIRE_SECONDS;
    let window = FILE_SIGNATURE_ALIGN_WINDOW_SECONDS;
    // `i64::div_ceil` 在当前工具链上仍属 unstable（`int_roundings`），
    // 用 rem_euclid 手写：它对负数也给出非负余数，比手工判符号可靠。
    let remainder = target.rem_euclid(window);
    let floor = (target - remainder) / window;
    if remainder == 0 {
        floor * window
    } else {
        (floor + 1) * window
    }
}

/// 签名校验失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// 路径非法。
    PathInvalid,
    /// 签名已过期。
    Expired,
    /// 签名不匹配。
    Invalid,
}

impl SignatureError {
    pub const STATUS: u16 = 403;

    pub const fn code(self) -> &'static str {
        match self {
            Self::PathInvalid => "file_path_invalid",
            Self::Expired => "file_signature_expired",
            Self::Invalid => "file_signature_invalid",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::PathInvalid => "文件路径非法",
            Self::Expired => "文件签名已过期",
            Self::Invalid => "文件签名无效",
        }
    }
}

// ---------------------------------------------------------------- 路径归一

/// 图片路径归一。对应 `_normalize_relative_path`：反斜杠转正斜杠，
/// 拒绝绝对路径与 `.` / `..` / 空段。
pub fn normalize_relative_path(relative_path: &str) -> Result<String, SignatureError> {
    let normalized = relative_path.trim().replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') {
        return Err(SignatureError::PathInvalid);
    }
    join_checked(&normalized)
}

/// 媒体资源路径归一。对应 `_normalize_resource_path`。
///
/// 与图片路径唯一的区别：**空串是合法的**（provider 的资源路径可以为空，
/// 表示资源根）。其余规则相同。
pub fn normalize_resource_path(resource_path: &str) -> Result<String, SignatureError> {
    if resource_path.is_empty() {
        return Ok(String::new());
    }
    if resource_path.contains('\\') || resource_path.contains('\0') {
        return Err(SignatureError::PathInvalid);
    }
    if resource_path.starts_with('/') {
        return Err(SignatureError::PathInvalid);
    }
    join_checked(resource_path)
}

fn join_checked(path: &str) -> Result<String, SignatureError> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return Err(SignatureError::PathInvalid);
    }
    Ok(parts.join("/"))
}

// ---------------------------------------------------------------- 签名

fn digest(secret: &str, payload: &str) -> String {
    hex(&hmac_sha256(secret.as_bytes(), payload.as_bytes()))
}

pub fn image_signature(secret: &str, path: &str, expires: i64) -> String {
    digest(secret, &format!("images:{path}:{expires}"))
}

pub fn media_signature(secret: &str, media_id: i32, resource_path: &str, expires: i64) -> String {
    digest(
        secret,
        &format!("media:{media_id}:{resource_path}:{expires}"),
    )
}

pub fn merged_signature(
    secret: &str,
    media_ids: &[i32],
    resource_path: &str,
    expires: i64,
) -> String {
    let ids = media_ids
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    digest(
        secret,
        &format!("merged-media:{ids}:{resource_path}:{expires}"),
    )
}

pub fn clip_signature(secret: &str, clip_id: i32, expires: i64) -> String {
    digest(secret, &format!("clip:{clip_id}:{expires}"))
}

pub fn subtitle_signature(secret: &str, subtitle_id: i32, expires: i64) -> String {
    digest(secret, &format!("subtitles:{subtitle_id}:{expires}"))
}

fn checked(
    expected: &str,
    signature: &str,
    expires: i64,
    now_seconds: i64,
) -> Result<(), SignatureError> {
    if expires <= now_seconds {
        return Err(SignatureError::Expired);
    }
    if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
        return Err(SignatureError::Invalid);
    }
    Ok(())
}

/// 校验图片签名，返回归一后的路径。
pub fn verify_image(
    secret: &str,
    file_path: &str,
    expires: i64,
    signature: &str,
    now_seconds: i64,
) -> Result<String, SignatureError> {
    let path = normalize_relative_path(file_path)?;
    let expected = image_signature(secret, &path, expires);
    checked(&expected, signature, expires, now_seconds)?;
    Ok(path)
}

/// 校验媒体播放签名，返回归一后的资源路径。
pub fn verify_media(
    secret: &str,
    media_id: i32,
    resource_path: &str,
    expires: i64,
    signature: &str,
    now_seconds: i64,
) -> Result<String, SignatureError> {
    let path = normalize_resource_path(resource_path)?;
    let expected = media_signature(secret, media_id, &path, expires);
    checked(&expected, signature, expires, now_seconds)?;
    Ok(path)
}

/// 校验片段串流签名。
pub fn verify_clip(
    secret: &str,
    clip_id: i32,
    expires: i64,
    signature: &str,
    now_seconds: i64,
) -> Result<(), SignatureError> {
    checked(
        &clip_signature(secret, clip_id, expires),
        signature,
        expires,
        now_seconds,
    )
}

/// 校验字幕签名。
pub fn verify_subtitle(
    secret: &str,
    subtitle_id: i32,
    expires: i64,
    signature: &str,
    now_seconds: i64,
) -> Result<(), SignatureError> {
    checked(
        &subtitle_signature(secret, subtitle_id, expires),
        signature,
        expires,
        now_seconds,
    )
}

// ---------------------------------------------------------------- URL 构造

/// `quote(path, safe='/')` 的等价集合：字母数字与 `-._~` 不编码，`/` 保留。
const PATH_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode_path(path: &str) -> String {
    percent_encoding::utf8_percent_encode(path, PATH_ENCODE).to_string()
}

/// 构造签名图片 URL。
pub fn build_signed_image_url(
    secret: &str,
    relative_path: &str,
    now_seconds: i64,
) -> Result<String, SignatureError> {
    let path = normalize_relative_path(relative_path)?;
    let expires = signature_expires(now_seconds);
    let signature = image_signature(secret, &path, expires);
    Ok(format!(
        "{}/{}?expires={expires}&signature={signature}",
        IMAGE_FILE_ROUTE_PREFIX,
        encode_path(&path)
    ))
}

/// 构造签名播放 URL。
pub fn build_signed_media_url(
    secret: &str,
    media_id: i32,
    resource_path: &str,
    delivery: &str,
    now_seconds: i64,
) -> Result<String, SignatureError> {
    if delivery != "proxy" && delivery != "redirect" {
        // 上游对非法 delivery 抛 ValueError（500）而不是 ApiError
        return Err(SignatureError::PathInvalid);
    }
    let path = normalize_resource_path(resource_path)?;
    let expires = signature_expires(now_seconds);
    let signature = media_signature(secret, media_id, &path, expires);
    let tail = if path.is_empty() {
        String::new()
    } else {
        encode_path(&path)
    };
    Ok(format!(
        "{}/{media_id}/play/{tail}?expires={expires}&signature={signature}&delivery={delivery}",
        MEDIA_PLAY_ROUTE_PREFIX
    ))
}

/// 构造签名片段串流 URL。
///
/// 对应上游 `build_signed_clip_url`（`src/common/file_signatures.py:241`）：
///
/// ```text
/// /media-clips/{clip_id}/stream?expires={expires}&signature={signature}
/// ```
///
/// 与其它资源共用固定有效期与窗口对齐策略（[`signature_expires`]）——
/// 刻意不引入「片段有独立的有效期」这种差异。
///
/// # 没有 `path` 参数
///
/// 片段的产物路径由服务端从 `clip_id` 反查（`MediaClipService.stream_file_path`），
/// 不从 URL 里取。所以这里**不**走 [`normalize_resource_path`] ——
/// 那套 `.` / `..` / 绝对路径的拒绝逻辑在这里没有对应输入。
///
/// 顺带说明为什么这样更安全：URL 里带路径就意味着「路径来自客户端」，
/// 而片段路径是服务端状态。
pub fn build_signed_clip_url(secret: &str, clip_id: i32, now_seconds: i64) -> String {
    let expires = signature_expires(now_seconds);
    let signature = clip_signature(secret, clip_id, expires);
    format!(
        "{MEDIA_CLIP_STREAM_ROUTE_PREFIX}/{clip_id}/stream?expires={expires}&signature={signature}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "file-signature-secret";

    // ------------------------------------------------ 对拍向量（Python hmac）

    #[test]
    fn matches_the_python_payloads_byte_for_byte() {
        // 由 `hmac.new(secret, payload, sha256).hexdigest()` 生成，
        // 改动任何一种载荷格式都会让已签出的 URL 全部失效。
        assert_eq!(
            image_signature(SECRET, "a/b.webp", 1_800_000_000),
            "652bb8995c2a141e88deb70cfb1138593f646b17888b0cf2b99ff09e514bc962"
        );
        assert_eq!(
            media_signature(SECRET, 7, "dir/x.mp4", 1_800_000_000),
            "d6dff21d8f4c0264543c503f3efb7143f2ed5b926be2b4d61ca6f1506402b618"
        );
        assert_eq!(
            clip_signature(SECRET, 3, 1_800_000_000),
            "b18dbb2fe24bc2379e401051814ee1d5e27d03ffb66576d698d5e83f2ae3f8a2"
        );
        assert_eq!(
            subtitle_signature(SECRET, 9, 1_800_000_000),
            "3ff3872eb6ccb91de8e581453dcdb64ce6d655fab133096c13095889fae8805e"
        );
        assert_eq!(
            merged_signature(SECRET, &[1, 2], "p.mp4", 1_800_000_000),
            "c774cde152e3e30c97077eb7f5393e54fb49ea4adcee965bb51d8ee75b291b5b"
        );
    }

    #[test]
    fn payload_is_order_sensitive() {
        // 冒号分隔的格式意味着不同资源的签名不能互相顶替
        assert_ne!(
            image_signature(SECRET, "a", 1),
            clip_signature(SECRET, 1, 1)
        );
        assert_ne!(
            media_signature(SECRET, 7, "", 1),
            media_signature(SECRET, 7, "x", 1)
        );
    }

    // ------------------------------------------------ 窗口对齐

    #[test]
    fn expires_is_aligned_up_to_the_window() {
        // 窗口 21600。now=0 -> target=43200 正好整除 -> 43200；
        // now=1 -> target=43201 -> 向上到 64800。
        assert_eq!(signature_expires(0), 43_200);
        assert_eq!(signature_expires(1), 64_800);
        assert_eq!(signature_expires(21_599), 64_800);
    }

    #[test]
    fn validity_is_never_shorter_than_the_nominal_expiry() {
        // 向上取整的意义：实际有效期落在 [12h, 12h+6h)，永不"签出即过期"
        for now in [0i64, 1, 10_000, 21_599, 1_800_000_000, 1_800_000_123] {
            let expires = signature_expires(now);
            let validity = expires - now;
            assert!(
                validity >= FILE_SIGNATURE_EXPIRE_SECONDS,
                "now={now} 的实际有效期 {validity} 短于标称值"
            );
            assert!(
                validity < FILE_SIGNATURE_EXPIRE_SECONDS + FILE_SIGNATURE_ALIGN_WINDOW_SECONDS,
                "now={now} 的实际有效期 {validity} 超出窗口上界"
            );
        }
    }

    #[test]
    fn the_same_window_yields_the_same_url() {
        // 这是窗口对齐的全部目的：让浏览器/CDN 命中缓存。
        // 1 与 21599 落在同一个窗口（都对齐到 64800）；0 落在前一个窗口。
        let a = build_signed_image_url(SECRET, "a/b.webp", 1).unwrap();
        let b = build_signed_image_url(SECRET, "a/b.webp", 21_599).unwrap();
        assert_eq!(a, b, "同一窗口内签出的 URL 必须完全一致");
        assert_ne!(
            a,
            build_signed_image_url(SECRET, "a/b.webp", 0).unwrap(),
            "跨窗口必须换 URL"
        );
    }

    // ------------------------------------------------ 路径归一

    #[test]
    fn backslashes_are_normalized() {
        assert_eq!(normalize_relative_path(r"a\b.webp").unwrap(), "a/b.webp");
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        for bad in ["", "   ", "/abs", "a/../b", "a//b", "./a", "a/./b", ".."] {
            assert_eq!(
                normalize_relative_path(bad),
                Err(SignatureError::PathInvalid),
                "path={bad:?}"
            );
        }
    }

    #[test]
    fn empty_resource_path_is_legal_for_media_only() {
        // provider 的资源路径可以为空（表示资源根），图片路径不行
        assert_eq!(normalize_resource_path("").unwrap(), "");
        assert_eq!(
            normalize_relative_path(""),
            Err(SignatureError::PathInvalid)
        );
    }

    #[test]
    fn resource_path_rejects_nul_and_backslash() {
        assert_eq!(
            normalize_resource_path("a\0b"),
            Err(SignatureError::PathInvalid)
        );
        assert_eq!(
            normalize_resource_path(r"a\b"),
            Err(SignatureError::PathInvalid)
        );
    }

    // ------------------------------------------------ 校验

    #[test]
    fn a_fresh_signature_verifies() {
        let expires = signature_expires(1_800_000_000);
        let signature = image_signature(SECRET, "a/b.webp", expires);
        assert_eq!(
            verify_image(SECRET, "a/b.webp", expires, &signature, 1_800_000_000).unwrap(),
            "a/b.webp"
        );
    }

    #[test]
    fn expired_signature_is_reported_before_the_comparison() {
        let signature = image_signature(SECRET, "a/b.webp", 100);
        assert_eq!(
            verify_image(SECRET, "a/b.webp", 100, &signature, 100),
            Err(SignatureError::Expired)
        );
        // 即使签名正确，过期也优先
        assert_eq!(
            verify_image(SECRET, "a/b.webp", 99, &signature, 100),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn tampered_signature_is_invalid() {
        let expires = signature_expires(1_800_000_000);
        let mut signature = image_signature(SECRET, "a/b.webp", expires);
        signature.pop();
        signature.push('0');
        assert_eq!(
            verify_image(SECRET, "a/b.webp", expires, &signature, 1_800_000_000),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn another_secret_does_not_verify() {
        let expires = signature_expires(1_800_000_000);
        let signature = image_signature("other", "a/b.webp", expires);
        assert_eq!(
            verify_image(SECRET, "a/b.webp", expires, &signature, 1_800_000_000),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn error_codes_match_the_upstream_literals() {
        assert_eq!(SignatureError::PathInvalid.code(), "file_path_invalid");
        assert_eq!(SignatureError::Expired.code(), "file_signature_expired");
        assert_eq!(SignatureError::Invalid.code(), "file_signature_invalid");
        assert_eq!(SignatureError::STATUS, 403);
    }

    // ------------------------------------------------ URL

    #[test]
    fn image_url_shape_matches_upstream() {
        let url = build_signed_image_url(SECRET, "a/b.webp", 0).unwrap();
        assert!(url.starts_with("/files/images/a/b.webp?expires="), "{url}");
        assert!(url.contains("&signature="), "{url}");
    }

    #[test]
    fn path_encoding_keeps_slashes_and_unreserved_chars() {
        // Python quote(safe='/')：/ 与 -._~ 不编码，空格要编码
        assert_eq!(encode_path("a/b-c_d.e~f.webp"), "a/b-c_d.e~f.webp");
        assert_eq!(encode_path("a b/c.webp"), "a%20b/c.webp");
    }

    #[test]
    fn media_url_carries_delivery_and_rejects_unknown_values() {
        let url = build_signed_media_url(SECRET, 7, "dir/x.mp4", "redirect", 0).unwrap();
        assert!(url.starts_with("/media/7/play/dir/x.mp4?expires="), "{url}");
        assert!(url.ends_with("&delivery=redirect"), "{url}");
        assert_eq!(
            build_signed_media_url(SECRET, 7, "", "bogus", 0),
            Err(SignatureError::PathInvalid)
        );
    }

    #[test]
    fn clip_and_subtitle_verification() {
        let expires = signature_expires(1_800_000_000);
        assert_eq!(
            verify_clip(
                SECRET,
                3,
                expires,
                &clip_signature(SECRET, 3, expires),
                1_800_000_000
            ),
            Ok(())
        );
        assert_eq!(
            verify_subtitle(SECRET, 9, 100, &subtitle_signature(SECRET, 9, 100), 100),
            Err(SignatureError::Expired)
        );
    }
}
