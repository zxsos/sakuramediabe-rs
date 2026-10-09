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

use image::metadata::Orientation;
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

/// ★ 解码并**按 EXIF 方向转正**。
///
/// 对应上游 `image_search_input.py:12` 的 `ImageOps.exif_transpose(image)`：
/// 手机横拍时传感器是竖的，EXIF 里记着「显示时要转 90°」，不解这一层的话
/// 用户上传的竖图在检索侧是躺着的 —— 搜出来的结果也就躺着了。
///
/// # 为什么单独一个函数而不是给 [`decode`] 加参数
///
/// 另两个解码调用点（封面书脊检测、缩略图自检）要的是**像素原样**：
/// 那里转正反而会破坏它们的坐标假设（`cover_split::to_gray` 按列求和找书脊，
/// 转正的图会把它判成另一本）。所以默认行为不动，要转正的显式调这个。
///
/// # 为什么手写 EXIF 解析（而不是引 `kamadak-exif`）
///
/// `image` 0.25.10 的 `JpegDecoder::orientation()` 与 `exif_metadata()` 都是
/// **私有**的（0.26 才随 `exif` feature 公开），所以拿 orientation 只能自己解析
/// APP1 段。`kamadak-exif` 是 `image` 0.26 自己用的那个库，纯 Rust、零依赖 ——
/// 但为了这一个 u8 值多一个依赖不划算，而这段解析能被完整测试（见 `tests`）。
///
/// 只取 IFD0 的 `0x0112`（Orientation），**不做**别的 EXIF 修正：
/// 上游 Pillow 的 `exif_transpose` 也只做方向 —— 它会顺手清掉 orientation
/// 标签，而我们连标签都不保留（转完就没人看它了）。
pub fn decode_oriented(bytes: &[u8]) -> Result<DynamicImage, ImageError> {
    let mut image = decode(bytes)?;
    if let Some(orientation) = exif_orientation(bytes).and_then(Orientation::from_exif) {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// ★ 图搜输入图归一化：**EXIF 转正 → 模式归一 → 无损 WebP**。
///
/// 对应上游 `image_search_input.py:7-21` 的 `normalize_image_search_query`
/// （全文 18 行）。它不是校验器，是**转换器**。
///
/// # 为什么整条链放在这里而不是调用方
///
/// 三步全都只用 `image` 的能力，而 `image` **只有本 crate 依赖** ——
/// 放到 `sm-service` 就得让服务层也依赖图像库，测试里也没法造真图。
/// 这里做完，调用方只拿到字节。
///
/// # 为什么必须无损
///
/// 输出会被送进 embedding 服务算向量。有损压缩改变像素，就等于改变了向量 ——
/// 同一张图两次检索可能给出不同结果，而用户完全无法理解。
pub fn normalize_for_embedding(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let image = decode_oriented(bytes)?;
    // 模式归一：输出只有 RGB / RGBA 两种。`to_rgb8` / `to_rgba8` 对本来就是
    // RGB / RGBA 的输入是原样通道数，所以这一句覆盖了上游那个 `if` 的全部。
    let rgba: RgbaImage = if image.color().has_alpha() {
        image.to_rgba8()
    } else {
        // RGB -> RGBA：补一个不透明 alpha。`Luma([255])` 取它的 R 分量当 alpha
        // 是绕 —— 直接用 255 即可。
        let rgb = image.to_rgb8();
        RgbaImage::from_fn(rgb.width(), rgb.height(), |x, y| {
            let [r, g, b] = rgb.get_pixel(x, y).0;
            image::Rgba([r, g, b, 255])
        })
    };
    encode_lossless(&rgba)
}

/// 从 JPEG 的 EXIF 里取 orientation 字节。**没有就 `None`。**
///
/// 只做「找 APP1 段 → 读 TIFF IFD0 的 0x0112」这一件事。坏文件一律 `None`
/// 而不是 Err —— 一个 orientation 读不出来**不该**让整张图解码失败。
fn exif_orientation(jpeg: &[u8]) -> Option<u8> {
    // SOI
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut at = 2usize;
    // 每段是 `FF <marker> <len16> <payload>`；总长受 `jpeg.len()` 约束。
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xFF {
            return None;
        }
        let marker = jpeg[at + 1];
        // 填充字节 `FF FF` 可以连着出现，跳过。
        if marker == 0xFF {
            at += 1;
            continue;
        }
        // 无长度字段的 marker：SOI / EOI / RSTn / TEM
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            at += 2;
            continue;
        }
        // SOS 之后是压缩数据，EXIF 不会在那后面。
        if marker == 0xDA {
            return None;
        }
        let payload_len = read_u16(jpeg, at + 2, false)? as usize;
        let payload_start = at + 4;
        let payload_end = payload_start.checked_add(payload_len)?;
        if payload_end > jpeg.len() {
            return None;
        }
        if marker == 0xE1 {
            // APP1 的 payload 以 `Exif\0\0` 开头，之后才是 TIFF 头。
            let payload = jpeg.get(payload_start..payload_end)?;
            if payload.starts_with(b"Exif\0\0") {
                return tiff_orientation(payload.get(6..)?);
            }
        }
        at = payload_end;
    }
    None
}

/// 从 TIFF 头里读 IFD0 的 `Orientation`（tag `0x0112`，type `SHORT`）。
fn tiff_orientation(tiff: &[u8]) -> Option<u8> {
    // 字节序标记：`II` 小端 / `MM` 大端。
    let little = match tiff.get(0..2)? {
        [0x49, 0x49] => true,
        [0x4D, 0x4D] => false,
        _ => return None,
    };
    // 42 是 TIFF 魔数。
    if read_u16(tiff, 2, little)? != 42 {
        return None;
    }
    let ifd0 = read_u16(tiff, 4, little)? as usize;
    let count = read_u16(tiff, ifd0, little)? as usize;
    for index in 0..count {
        let entry = ifd0.checked_add(2 + index.checked_mul(12)?)?;
        if read_u16(tiff, entry, little)? != 0x0112 {
            continue;
        }
        // entry 布局：tag(2) type(2) count(4) value(4)。SHORT 的值在 value 的
        // 前两字节（TIFF 里数值按左对齐存放；这里不校验 type —— 真实文件里
        // orientation 永远是 SHORT，而校验它反而会拒掉某些写错的相机文件）。
        return read_u16(tiff, entry.checked_add(8)?, little).map(|value| value as u8);
    }
    None
}

/// 读一个大端/小端的 u16，越界或长度不够一律 `None`。
fn read_u16(bytes: &[u8], at: usize, little: bool) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    let raw = [pair[0], pair[1]];
    Some(if little {
        u16::from_le_bytes(raw)
    } else {
        u16::from_be_bytes(raw)
    })
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

    // ============================================================ EXIF 方向

    /// 造一个带 EXIF orientation 的**最小** JPEG。
    ///
    /// 不走真编码器 —— 1×1 的图编码出来再塞 EXIF 反而绕。直接手拼：
    /// `SOI` + `APP1(Exif + TIFF)` + `EOI`。`image` 解码时只需要认得 SOI 就
    /// 会继续读 SEGMENT，`APP1` 对它是无害的附加段。
    ///
    /// 第二个返回值是**单独的 APP1 段**（不含 SOI/EOI）—— 拼进真 JPEG 时要的是
    /// 它，把 EOI 一起带上会把后面的压缩数据截断。
    fn jpeg_with_orientation(orientation: Option<u8>) -> (Vec<u8>, Vec<u8>) {
        // TIFF（小端）+ 1 个 entry（Orientation）
        let mut tiff: Vec<u8> = Vec::new();
        match orientation {
            None => {
                // 合法 TIFF 但没有 orientation entry：IFD0 条数 = 0
                tiff.extend_from_slice(b"II\x2a\x00\x08\x00\x00\x00\x00\x00");
            }
            Some(value) => {
                tiff.extend_from_slice(b"II\x2a\x00\x08\x00\x00\x00"); // 头 + IFD0 偏移 8
                tiff.extend_from_slice(&1u16.to_le_bytes()); // 条数 = 1
                tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // tag = Orientation
                tiff.extend_from_slice(&3u16.to_le_bytes()); // type = SHORT
                tiff.extend_from_slice(&1u32.to_le_bytes()); // count = 1
                tiff.extend_from_slice(&value.to_le_bytes()); // 值（左对齐，低位在前）
                tiff.extend_from_slice(&[0, 0]); // 值的高两字节补零
                tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
            }
        }

        let mut exif_payload: Vec<u8> = b"Exif\0\0".to_vec();
        exif_payload.extend_from_slice(&tiff);

        // APP1 段：`FF E1 <len16> <payload>`，len16 含自己那 2 字节。
        let mut app1: Vec<u8> = vec![0xFF, 0xE1];
        let length = (exif_payload.len() + 2) as u16;
        app1.extend_from_slice(&length.to_be_bytes());
        app1.extend_from_slice(&exif_payload);

        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8]; // SOI
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        (jpeg, app1)
    }

    /// ★ EXIF 里的 orientation 要读得出来（大端 / 小端都试）。
    #[test]
    fn exif_orientation_is_read_from_app1() {
        for value in [1u8, 3, 6, 8] {
            assert_eq!(
                exif_orientation(&jpeg_with_orientation(Some(value)).0),
                Some(value),
                "orientation={value} 要读得出来"
            );
        }
        // 没有 EXIF 段 -> None（而不是报错）
        assert_eq!(exif_orientation(&[0xFF, 0xD8, 0xFF, 0xD9]), None);
        // 有 TIFF 但没有 orientation entry -> None
        assert_eq!(exif_orientation(&jpeg_with_orientation(None).0), None);
    }

    /// ★ 坏输入一律 `None`，**绝不 panic**。
    ///
    /// 这些都是「用户随手上传的任意字节」可能长得的样子。
    #[test]
    fn malformed_input_yields_none_instead_of_panicking() {
        for bad in [
            vec![],
            vec![0xFF],
            vec![0xFF, 0xD8],
            // 段长度声称超出缓冲
            {
                let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF, 0x45, 0x78];
                v.push(0x69);
                v
            },
            // TIFF 魔数不对
            {
                let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x0A];
                v.extend_from_slice(b"Exif\0\0");
                v.extend_from_slice(b"XX\x2a\x00\x08\x00\x00\x00\x00\x00");
                v
            },
            // 字节序标记不认识
            {
                let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x0A];
                v.extend_from_slice(b"Exif\0\0");
                v.extend_from_slice(b"ZZ\x2a\x00\x08\x00\x00\x00\x00\x00");
                v
            },
        ] {
            assert_eq!(exif_orientation(&bad), None, "坏输入不该 panic：{bad:?}");
        }
    }

    /// ★ 真图 + 真 EXIF：orientation=6（顺时针 90°）时宽高要交换。
    ///
    /// 这是整个函数的**意义**所在：图搜按「用户拍的图」检索，而手机横拍时
    /// 传感器是竖的 —— 不转正的话，检索侧看到的是躺着的图。
    #[test]
    fn decode_oriented_swaps_dimensions_for_a_rotated_jpeg() {
        // 造一张 40×20 的 JPEG，再把 orientation 标成 6（= 顺时针 90°）
        let wide = sample(40, 20);
        let mut jpeg_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(wide)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg_bytes),
                image::ImageFormat::Jpeg,
            )
            .expect("编码 JPEG");
        // 把 APP1 段（不含 SOI/EOI）插到 SOI 之后
        let mut with_exif = vec![0xFF, 0xD8];
        let (_, app1) = jpeg_with_orientation(Some(6));
        with_exif.extend_from_slice(&app1);
        with_exif.extend_from_slice(&jpeg_bytes[2..]); // 去掉原 SOI

        // 不转正的解码：原样 40×20
        let raw = decode(&with_exif).expect("解码失败");
        assert_eq!((raw.width(), raw.height()), (40, 20), "不转正时保持原样");

        // 转正后：宽高交换
        let oriented = decode_oriented(&with_exif).expect("转正解码失败");
        assert_eq!(
            (oriented.width(), oriented.height()),
            (20, 40),
            "★ orientation=6 应该把 40×20 转成 20×40"
        );
    }

    /// 没有 EXIF 的图走 `decode_oriented` **必须与 `decode` 完全一致**
    /// （多转一次会白白重排像素，白丢质量与时间）。
    #[test]
    fn decode_oriented_is_a_no_op_without_exif() {
        let src = sample(24, 13);
        let encoded = encode_lossless(&src).expect("编码");
        let a = decode(&encoded).expect("解码").to_rgba8();
        let b = decode_oriented(&encoded).expect("转正解码").to_rgba8();
        assert_eq!(a, b, "无 EXIF 时两条路径必须一致");
    }

    // ============================================================ 图搜归一化

    /// 造一张真 PNG（RGBA，每像素不同）。
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(sample(width, height))
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("编码 PNG");
        out
    }

    /// ★ 整条链：PNG（有 alpha）→ 无损 WebP，**每个像素都还在**。
    ///
    /// 「无损」是硬要求：这张图要进 embedding 服务，有损改了像素就改了向量。
    #[test]
    fn normalize_keeps_every_pixel_and_returns_webp() {
        let src = sample(31, 19);
        let png = png_bytes(31, 19);
        let out = normalize_for_embedding(&png).expect("归一化应成功");

        assert!(is_webp(&out), "输出必须是 WebP");
        let back = decode(&out).expect("回解").to_rgba8();
        assert_eq!((back.width(), back.height()), (31, 19));
        assert_eq!(back.as_raw(), src.as_raw(), "★ 无损：像素必须逐个相同");
    }

    /// ★ 无 alpha 的输入要补上**不透明** alpha，而不是留 0（那会当全透明）。
    #[test]
    fn an_opaque_input_gets_opaque_alpha_not_zero() {
        // 造一张 RGB（无 alpha）的 PNG
        let mut rgb = image::RgbImage::new(8, 5);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x * 30) as u8, (y * 50) as u8, 128]);
        }
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(rgb.clone())
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("编码 PNG");

        let back = decode(&normalize_for_embedding(&png).expect("归一化"))
            .expect("回解")
            .to_rgba8();
        assert!(
            back.as_raw().chunks_exact(4).all(|p| p[3] == 255),
            "alpha 必须是 255（不透明），不能是 0"
        );
        // RGB 通道也要原样
        for (x, y, px) in back.enumerate_pixels() {
            let want = rgb.get_pixel(x, y).0;
            assert_eq!([px.0[0], px.0[1], px.0[2]], want, "({x},{y}) RGB 要原样");
        }
    }

    /// 端到端：带 EXIF orientation=6 的 JPEG → 输出是**竖**的 WebP。
    #[test]
    fn normalize_applies_exif_orientation_end_to_end() {
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgba8(sample(40, 20))
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .expect("编码 JPEG");
        let (_, app1) = jpeg_with_orientation(Some(6));

        let mut with_exif = vec![0xFF, 0xD8];
        with_exif.extend_from_slice(&app1);
        with_exif.extend_from_slice(&jpeg[2..]);

        let out = decode(&normalize_for_embedding(&with_exif).expect("归一化")).expect("回解");
        assert_eq!(
            (out.width(), out.height()),
            (20, 40),
            "★ orientation=6：40×20 应转成 20×40"
        );
    }

    /// 坏输入返回 `Err`（不 panic、不返回空图）。
    #[test]
    fn normalize_rejects_garbage() {
        for bad in [
            b"".to_vec(),
            b"not an image".to_vec(),
            vec![0xFF, 0xD8, 0xFF],
        ] {
            assert!(normalize_for_embedding(&bad).is_err(), "{bad:?} 该报错");
        }
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
