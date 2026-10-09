//! 媒体格式工具。对应上游 `src/common/media_formats.py`。
//!
//! ⚠️ **只落地了 [`normalize_media_resolution`]。** 上游同文件还有
//! `SUPPORTED_VIDEO_EXTENSIONS` 与 `is_supported_video_file_name`（导入链路
//! 判「是不是视频文件」用的），本仓还没有调用方 —— 提前搬进来只会变成
//! 「永不调用的东西」。用到时再移，别先放着。

/// 分辨率字符串的长度上限（上游 `_MAX_MEDIA_RESOLUTION_LENGTH`）。
const MAX_MEDIA_RESOLUTION_LENGTH: usize = 32;

/// 单个维度的上限（上游 `_MAX_MEDIA_RESOLUTION_DIMENSION = "2147483647"`，
/// 即 `i32::MAX`）。
///
/// 比较写成**等长十进制串的字典序** —— 位数相同则字典序与数值序等价，
/// 而上游就是用字符串比的（`dimension > _MAX_MEDIA_RESOLUTION_DIMENSION`）。
/// 换成数值比较看似更自然，但上游从没把维度解析成整数，行为会分叉在
/// 「前导零」这类输入上。
const MAX_MEDIA_RESOLUTION_DIMENSION: &str = "2147483647";

/// 把 provider 给的分辨率归一成**规范的正 `WxH` 字符串**；不合规 → `None`。
///
/// 逐条对齐上游 `normalize_media_resolution`：
///
/// | 输入 | 结果 |
/// |---|---|
/// | `"1920x1080"` | `Some("1920x1080")` |
/// | `"  1920X1080 "` | `Some("1920x1080")`（整体 trim + 转小写）|
/// | `"01920x01080"` | `Some("1920x1080")`（去前导零）|
/// | `"0x1080"` / `"0000x1"` | `None`（去前导零后为空）|
/// | `"1920"` / `"1920x1080x1"` / `"1920x"` / `""` | `None`（不是恰好两段）|
/// | `"1920x1080i"` / `"全高清"` | `None`（非 ASCII 数字）|
/// | `"99999999999x1"` | `None`（维度超 `i32::MAX`）|
/// | 长度 > 32 | `None` |
///
/// **去前导零后为空也算不合规** —— `"0x1080"` 的第一个维度是 0，而分辨率 0
/// 没有意义。这是上游 `lstrip("0")` 之后判空的语义。
///
/// 长度上限按**字符**算（上游 `len()` 数的是码点，不是字节）。
///
/// # 那个 32 上限是冗余的，但**照抄**
///
/// 单个维度最多 10 位（`i32::MAX` 是 10 位），所以合规输入最长
/// `"2147483647x2147483647"` = **21 字符**，永远够不到 32。
/// 也就是说长度检查对「结果」没有影响 —— 上游有它，这里就留着，
/// 而不是自作聪明删掉（删了以后若上游某天放宽维度上限，两边就分叉了）。
/// **测试里不要为它编一条断言** —— 任何超过 32 字符的输入都已经因为维度
/// 超限而返回 `None`，那种断言是「对答案不对理由」。
pub fn normalize_media_resolution(value: &str) -> Option<String> {
    if value.chars().count() > MAX_MEDIA_RESOLUTION_LENGTH {
        return None;
    }
    let lowered = value.trim().to_ascii_lowercase();
    let mut parts = lowered.split('x');
    let (Some(width), Some(height), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let width = strip_dimension(width)?;
    let height = strip_dimension(height)?;
    Some(format!("{width}x{height}"))
}

/// 校验一个维度并去掉前导零。不合规 → `None`。
fn strip_dimension(raw: &str) -> Option<&str> {
    // 空串也在这里被拒（`is_ascii_digit` 对空串的全称量化恒真，所以要显式判）。
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let trimmed = raw.trim_start_matches('0');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > MAX_MEDIA_RESOLUTION_DIMENSION.len()
        || (trimmed.len() == MAX_MEDIA_RESOLUTION_DIMENSION.len()
            && trimmed > MAX_MEDIA_RESOLUTION_DIMENSION)
    {
        return None;
    }
    Some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_case_and_padding() {
        for (input, expected) in [
            ("1920x1080", "1920x1080"),
            ("  1920X1080 ", "1920x1080"),
            ("01920x01080", "1920x1080"),
            ("8x8", "8x8"),
        ] {
            assert_eq!(
                normalize_media_resolution(input).as_deref(),
                Some(expected),
                "输入 {input:?}"
            );
        }
    }

    #[test]
    fn zero_dimensions_are_rejected_after_stripping_zeros() {
        // 上游 `lstrip("0")` 之后判空 —— 「0x1080」的第一个维度是 0。
        for input in ["0x1080", "0000x1", "0x0", "000x000"] {
            assert_eq!(normalize_media_resolution(input), None, "输入 {input:?}");
        }
    }

    #[test]
    fn must_be_exactly_two_numeric_segments() {
        for input in [
            "",
            " ",
            "1920",
            "x",
            "x1080",
            "1920x",
            "1920x1080x1",
            "1920x1080i",
            "全高清",
            "1920 x 1080",
            "-1920x1080",
            "+1x2",
        ] {
            assert_eq!(normalize_media_resolution(input), None, "输入 {input:?}");
        }
    }

    #[test]
    fn dimensions_above_i32_max_are_rejected() {
        // 10 位且字典序大于 "2147483647" → 拒；等于 → 放行；11 位 → 拒。
        assert_eq!(normalize_media_resolution("9999999999x1"), None);
        assert_eq!(normalize_media_resolution("12147483647x1"), None);
        assert_eq!(
            normalize_media_resolution("2147483647x1").as_deref(),
            Some("2147483647x1")
        );
        // 合法输入的最长形态是 21 字符 —— 见函数文档「32 上限是冗余的」。
        assert_eq!(
            normalize_media_resolution("2147483647x2147483647")
                .as_deref()
                .map(str::len),
            Some(21)
        );
    }
}
