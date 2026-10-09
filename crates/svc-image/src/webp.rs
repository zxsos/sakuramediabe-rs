//! WebP 编解码 —— 对应上游 Pillow 的 `save(format="WEBP", ...)`。
//!
//! # 上游的三种用法
//!
//! | 位置 | 参数 | 本模块 |
//! |---|---|---|
//! | `discovery/image_search_input.py:18` | `lossless=True` | [`encode_lossless`] ✅ |
//! | `catalog/actor_service.py:701` | `quality=90, method=6` | ❌ **无纯 Rust 实现** |
//! | `videos/video_cover_service.py:66` | `quality=80` | ❌ **无纯 Rust 实现** |
//!
//! # 有损编码是缺口，不是遗漏
//!
//! `image-webp` 0.2.4 只实现了 VP8L（无损）。其 `WebPEncoder::new` 的文档原文：
//! "Only supports \"VP8L\" lossless encoding." —— 没有 `quality` 参数，也没有
//! 有损入口。Rust 生态当前的替代只有：
//!
//! | 方案 | 纯 Rust | 代价 |
//! |---|:---:|---|
//! | `webp` 0.3.1 | ❌ | libwebp 的 C 绑定，破坏「纯 Rust / NAS 离线可编」 |
//! | `cwebp` CLI | ❌（外部二进制） | 构建零链接，但运行时依赖二进制；与 `ffprobe` 同一套路 |
//! | 降级为无损 | ✅ | 头像/封面体积涨数倍，不可接受 |
//!
//! 结论记在 ADR 里：无损走本模块，**有损走进程外 `cwebp`**（与媒体探测的
//! `ffprobe` 同一抽象层次）。本模块因此**不提供** `encode_lossy` —— 与其
//! 放一个会 panic 或静默降级的占位函数，不如让调用点在编译期就看见缺口。
//!
//! # 魔数检测
//!
//! 对应 `discovery/image_search_index_service.py:421`：
//! `payload[:4] == b"RIFF" and payload[8:12] == b"WEBP"`。

use std::io::Cursor;

use image::{DynamicImage, ImageError, ImageReader, RgbaImage};

/// WebP 魔数：RIFF 容器 + `WEBP` 四字符码。
const RIFF: &[u8; 4] = b"RIFF";
const WEBP: &[u8; 4] = b"WEBP";

/// 判断字节流是否为 WebP。
///
/// 对应 `image_search_index_service.py:421` 的魔数判断 —— 它比 `image.format`
/// 更早执行，用于在下载图片时提前拒绝非 WebP 响应。
///
/// 短于 12 字节的输入直接判否：Python 侧切片不会越界，这里也不能 panic。
pub fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == RIFF && &bytes[8..12] == WEBP
}

/// 编码为**无损** WebP（VP8L）。
///
/// 对应 `image_search_input.py:18` 的 `save(output, format="WEBP", lossless=True)`：
/// 以图搜图的输入图会被送进 embedding 服务，有损压缩会改变向量结果，
/// 所以这里**必须**是无损。
pub fn encode_lossless(image: &RgbaImage) -> Result<Vec<u8>, ImageError> {
    use image::codecs::webp::WebPEncoder;
    use image::ImageEncoder;

    let mut out = Vec::new();
    // image 0.25 的 WebP 编码器**只有** new_lossless —— 这本身就是
    // 「Rust 生态暂无纯 Rust 有损 WebP 编码器」的编译期证据。
    let encoder = WebPEncoder::new_lossless(&mut out);
    encoder.write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// 解码任意受支持的图片（jpeg / png / webp）。
///
/// 只开了这三种格式，`image` 默认特性里的 avif（→ `dav1d` C 绑定）已被关掉，
/// 传入 AVIF 会返回 [`ImageError::Unsupported`] 而不是 panic。
pub fn decode(bytes: &[u8]) -> Result<DynamicImage, ImageError> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(width: u32, height: u32) -> RgbaImage {
        use image::Rgba;
        let mut img = RgbaImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // 每个像素都不同，避免"恰好全同色"让无损编码测不出问题
            *px = Rgba([
                (x % 251) as u8,
                (y % 253) as u8,
                ((x * 7 + y * 13) % 249) as u8,
                ((x + y) % 255) as u8,
            ]);
        }
        img
    }

    #[test]
    fn lossless_round_trip_preserves_every_pixel() {
        let src = sample(37, 23);
        let encoded = encode_lossless(&src).expect("无损编码失败");
        let decoded = decode(&encoded).expect("解码失败").to_rgba8();

        assert_eq!(decoded.width(), src.width());
        assert_eq!(decoded.height(), src.height());
        assert_eq!(
            decoded.as_raw(),
            src.as_raw(),
            "无损 WebP 往返后像素必须逐字节相同"
        );
    }

    #[test]
    fn encoded_output_is_detected_as_webp() {
        let encoded = encode_lossless(&sample(8, 8)).expect("编码失败");
        assert!(is_webp(&encoded), "自己编出的 WebP 必须能通过魔数检测");
    }

    #[test]
    fn magic_check_matches_the_python_predicate() {
        // image_search_index_service.py 的判断：RIFF + 偏移 8 处的 WEBP
        assert!(is_webp(b"RIFF\x00\x00\x00\x00WEBP...."));
        assert!(
            !is_webp(b"RIFF\x00\x00\x00\x00AVIF...."),
            "WEBP 四字符码不匹配"
        );
        assert!(!is_webp(b"\x89PNG\r\n\x1a\n...."), "PNG 不是 WebP");
        assert!(!is_webp(b""), "空输入不能 panic");
        assert!(!is_webp(b"RIFF\x00\x00"), "短于 12 字节不能 panic");
    }
}
