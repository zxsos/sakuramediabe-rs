//! 图片文件的**落盘与读取**（上游 `src/common/image_store.py` +
//! `service/catalog/movie_image_service.py` 里那些文件操作）。
//!
//! # ★ 原子落盘：先写临时文件，再 `rename`
//!
//! 直接写最终路径会在中途失败时留下**半张图**（0 字节或截断），而 `image`
//! 记录一旦建立就指向它 —— 播放器显示裂图，且**没有重试机会**（记录已存在，
//! 下次导入认为「已有」）。
//!
//! 顺序是：写同目录临时文件 → `fsync` → `rename`。**必须同目录**，否则
//! `rename` 跨设备会退化成拷贝，原子性就没了。
//!
//! # ⚠️ 未闭环：包（zip）读写
//!
//! 上游读图是「**包优先**、单文件兜底」（`image_store.read_image_bytes`），
//! 写包是 `ZIP_STORED`。本仓还没有 zip 依赖，所以这里**只实现单文件那一路**。
//!
//! 这不是偷懒：包是「省一次 inode / 省一次目录项」的**优化**，而单文件读取
//! 在任何情况下都正确。先落正确的一路，包留给真正需要它的时候（30 万部影片
//! 的目录项压力）。本模块的 `pack` 相关判据在 [`crate::paths::image_pack_relative_path`]
//! 里已经就位 —— 那时只需在这里补读包的那一步。
//!
//! # 薄封面：从封面里**切**出右半
//!
//! [`crop_thin_cover`] 复刻上游 `_split_image`（`:149-181`）：解码 → 检测书脊
//! 分割点 → 裁掉左半。切不出来返回 `None` —— 那是**可降级**的（少一张竖封面
//! 不影响影片可用性），与「下载失败」不同。

use std::io;
use std::path::{Path, PathBuf};

use crate::cover_split::detect_split_points;
use crate::paths::absolute;

/// 解码后的 RGB 图。
#[derive(Debug, Clone, PartialEq)]
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    /// 逐像素 RGB，长度 `width * height * 3`。
    pub data: Vec<u8>,
}

/// 把字节**原子**写到 `root/relative`，返回最终绝对路径。
///
/// 父目录自动创建。`rename` 之前先 `fsync`：只 rename 不 fsync 的话，崩溃后
/// 可能留下 0 字节的最终文件 —— 那正是本模块要防的那件事。
pub fn atomic_write(root: &Path, relative: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let final_path = absolute(root, relative);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 临时文件**必须与最终文件同目录** —— 跨设备 rename 不是原子的。
    let temp_path = sibling_temp_path(&final_path);
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&temp_path, &final_path) {
        Ok(()) => Ok(final_path),
        Err(error) => {
            // 留在那里的临时文件会堆满磁盘；删不掉也只能记下来继续。
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

/// 读回字节。**单文件那一路**（包的判据就位但还没实现，见模块文档）。
pub fn read_image_bytes(root: &Path, relative: &str) -> io::Result<Vec<u8>> {
    std::fs::read(absolute(root, relative))
}

/// 同目录下的临时文件路径。用 pid + 原子计数避免同进程内并发撞名。
fn sibling_temp_path(final_path: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned());
    final_path.with_file_name(format!(".{}.{}.{}.tmp", name, std::process::id(), seq))
}

/// 解码成 RGB。解码失败返回 `None`（不是有效图片）—— 调用方按「可降级」处理。
pub fn decode_rgb(bytes: &[u8]) -> Option<Rgb> {
    let image = image::load_from_memory(bytes).ok()?;
    let rgb = image.to_rgb8();
    Some(Rgb {
        width: rgb.width(),
        height: rgb.height(),
        data: rgb.into_raw(),
    })
}

/// 是否**竖图**（高 > 宽）。上游 `_is_portrait_image`（`:183-191`）：
/// 解码失败时记 warn 并返回 `false`，不是错误。
pub fn is_portrait(bytes: &[u8]) -> bool {
    // `into_dimensions` 只读头部，不解码整张 —— 竖图判定只需要尺寸。
    // （`image::image_dimensions` 这个便捷函数只收路径，不收 reader。）
    let Ok(reader) = image::ImageReader::new(io::Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    match reader.into_dimensions() {
        Ok((width, height)) => width > 0 && height > width,
        Err(_) => false,
    }
}

/// 从封面里切出竖封面。上游 `_split_image`（`:149-181`）。
///
/// 返回裁好的图；**切不出来返回 `None`**（这是可降级路径，不是错误）：
/// 解码失败、找不到分割点、或裁完宽度是 0。
pub fn crop_thin_cover(bytes: &[u8], center_range: usize) -> Option<Rgb> {
    let image = decode_rgb(bytes)?;
    let gray = crate::cover_split::to_gray(&image.data);
    let (_, right) = detect_split_points(image.width, image.height, &gray, center_range);
    if right < 0 {
        return None;
    }
    let right = right as u32;
    if right >= image.width {
        return None;
    }
    let width = image.width - right;
    let mut data = Vec::with_capacity((width * image.height * 3) as usize);
    for y in 0..image.height {
        let start = ((y * image.width + right) * 3) as usize;
        data.extend_from_slice(&image.data[start..start + (width as usize) * 3]);
    }
    Some(Rgb {
        width,
        height: image.height,
        data,
    })
}

/// 按扩展名编码落盘。格式由**扩展名**推断（上游 `cv2.imwrite` 同理）。
pub fn save_rgb(path: &Path, image: &Rgb) -> io::Result<()> {
    let buffer = image::RgbImage::from_raw(image.width, image.height, image.data.clone())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "图片尺寸与字节数不符"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    image::DynamicImage::ImageRgb8(buffer)
        .save(path)
        .map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成一张 40×20 的图：左半纯灰、右半纯白 —— 中间有一条强竖边。
    fn synthetic_rgb() -> Rgb {
        let (width, height) = (40u32, 20u32);
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for _y in 0..height {
            for x in 0..width {
                // 左半纯灰、右半纯白：第 20 列有一条强竖边（书脊）。
                let value = if x < 20 { 40 } else { 200 };
                data.extend_from_slice(&[value, value, value]);
            }
        }
        Rgb {
            width,
            height,
            data,
        }
    }

    fn encode_png(image: &Rgb) -> Vec<u8> {
        let buffer = image::RgbImage::from_raw(image.width, image.height, image.data.clone())
            .expect("可构造");
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(&mut io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("可编码");
        bytes
    }

    /// 原子写：最终文件存在、内容正确，且**不留临时文件**。
    #[test]
    fn atomic_write_leaves_only_the_final_file() {
        let root = std::env::temp_dir().join(format!("svc-image-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = atomic_write(&root, "movies/ab/ABC/cover.png", b"bytes").expect("可写");
        assert_eq!(path, absolute(&root, "movies/ab/ABC/cover.png"));
        assert_eq!(std::fs::read(&path).expect("可读"), b"bytes".to_vec());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("有父目录"))
            .expect("可列目录")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 竖图判定：解码失败也返回 `false`（可降级，不是错误）。
    #[test]
    fn portrait_detection_degrades_to_false() {
        assert!(!is_portrait(b"not an image at all"));
        let wide = Rgb {
            width: 40,
            height: 20,
            data: vec![0; 40 * 20 * 3],
        };
        assert!(!is_portrait(&encode_png(&wide)));
        let tall = Rgb {
            width: 20,
            height: 40,
            data: vec![0; 20 * 40 * 3],
        };
        assert!(is_portrait(&encode_png(&tall)));
    }

    /// 切出来的图**变窄**且高度不变 —— 这才是竖封面。
    #[test]
    fn the_cropped_cover_is_narrower_but_just_as_tall() {
        let source = synthetic_rgb();
        let bytes = encode_png(&source);
        let cropped = crop_thin_cover(&bytes, 100).expect("这张图有明确书脊");
        assert_eq!(cropped.height, source.height);
        assert!(cropped.width < source.width, "必须切掉左半");
    }

    /// 纯色图没有可测的竖边 → 切不出来，**返回 `None` 而不是报错**。
    #[test]
    fn a_flat_image_yields_no_thin_cover() {
        let flat = Rgb {
            width: 40,
            height: 20,
            data: vec![120; 40 * 20 * 3],
        };
        assert_eq!(crop_thin_cover(&encode_png(&flat), 100), None);
        assert_eq!(crop_thin_cover(b"garbage", 100), None, "解不开也不报错");
    }
}
