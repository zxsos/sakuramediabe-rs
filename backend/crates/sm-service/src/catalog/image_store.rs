//! 图片包的字节原语（上游 `common/image_store.py` 的子集）。
//!
//! 上游这个模块还负责「读取图片字节」（包条目优先、单文件兜底，给签名 URL 的
//! 那一路用）。本仓只落地**写包**与**读单个条目** —— 读取入口要等签名 URL 那
//! 条链路（`files` 域）落地时按那时的调用方一起补，现在抄一半会写出一堆没人
//! 用的重载。
//!
//! # 压缩方式必须是 **STORED**（不压缩）
//!
//! 图片（webp/jpg）本身已是压缩格式，deflate 几乎不省空间却要花 CPU。而这里的
//! 场景是**局域网内传输**，CPU 比空间贵。
//!
//! ⚠️ 写成 DEFLATE 会让打包慢一个数量级，且产物体积几乎不变。见根 `Cargo.toml`
//! 里 `zip` 的 `default-features = false`（默认特性会把整套压缩实现拖进来）。
//!
//! # 写完之后要 `fsync`
//!
//! 包会被 `os.replace` 原子替换成正式包。如果临时包的内容还在页缓存里就替换，
//! 一次断电会留下**一个长度对但内容为空的包** —— 而 zip 从尾部读取，损坏的包
//! 表现为「完全打不开」，不是「少几张图」。

use std::io::Write;
use std::path::Path;

use crate::catalog::media_paths;
use crate::error::ServiceError;

/// 写一个 ZIP_STORED 包并 `fsync`。
///
/// **调用方负责临时路径与原子替换**（上游同款分工）—— 这个函数只管把字节
/// 按给定顺序落成一个完整的包文件。
///
/// 条目顺序即入参顺序：包的内容要能按字节稳定复现（重建后与旧包比对、
/// 缓存校验都依赖这一点），所以调用方必须先排序再进来。
pub fn write_pack(pack_path: &Path, entries: &[(String, Vec<u8>)]) -> Result<(), ServiceError> {
    if let Some(parent) = pack_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| pack_error(pack_path, error))?;
    }
    let file = std::fs::File::create(pack_path).map_err(|error| pack_error(pack_path, error))?;
    let mut archive = zip::ZipWriter::new(file);
    // 显式点名 `Stored`：`FileOptions::default()` 的方式会随 crate 缺省特性
    // 变化（开了 deflate 特性就变成 Deflated），而这条约定是**硬要求**。
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (entry_name, bytes) in entries {
        archive
            .start_file(entry_name.as_str(), options)
            .map_err(|error| pack_error(pack_path, error))?;
        archive
            .write_all(bytes)
            .map_err(|error| pack_error(pack_path, error))?;
    }
    // `finish` 才写出中央目录 —— 少了它的包是**打不开的**，不是「缺几条」。
    // 它同时把内部的 `File` 交还出来，而我们**正要**一个写句柄来 fsync。
    let handle = archive
        .finish()
        .map_err(|error| pack_error(pack_path, error))?;
    // 落盘之后再 fsync：见模块文档。
    //
    // ⚠️ **别用 `File::open` 重新开一个句柄**：那是只读的，而 `sync_all()` 在
    // Windows 上走 `FlushFileBuffers`，**要求句柄有写权限** —— 只读句柄直接
    // ERROR_ACCESS_DENIED（os error 5），表现为「写包在 Windows 上必然失败、
    // 在 Linux 上一切正常」。上游 `image_store.py:55` 是
    // `os.open(path, os.O_RDWR)`，特意用**读写**打开，同一个道理；
    // 我们手上已经有一个带写权限的句柄，不必再开一次。
    handle
        .sync_all()
        .map_err(|error| pack_error(pack_path, error))?;
    Ok(())
}

/// 读包里的一个条目。包不存在、打不开、或没有该条目都返回 `None`。
///
/// 三种情况合并成一个 `None` 是有意的：调用方（重建包）对它们的处置**相同**
/// —— 回退到别处取字节，都取不到就放弃本轮。而包损坏**不**是致命错误：它还
/// 原样留在磁盘上，下一次重建会覆盖它。
pub fn read_pack_entry(pack_path: &Path, entry_name: &str) -> Option<Vec<u8>> {
    let file = std::fs::File::open(pack_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut entry = archive.by_name(entry_name).ok()?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut bytes).ok()?;
    Some(bytes)
}

/// 读取图片字节：**包条目优先，缺失回退单文件**。
///
/// 上游 `image_store.read_image_bytes`。
///
/// # 顺序不能反
///
/// 包一旦存在即视为该图片的**主要存储** —— 打包后 loose 文件会被
/// `movie_asset_pack` 清掉。先试单文件会在「包是权威、loose 早已不存在」的常态
/// 下每次都多一次无效 `stat`。
///
/// 包**损坏或条目缺失**不算致命：回退单文件（上游同款 —— 磁盘坏或包被替换过时
/// 另一个副本还在）。
///
/// 两边都没有 → `Err`。上游抛 `FileNotFoundError`，调用方按「文件缺失」处理。
pub fn read_image_bytes(root: &Path, relative_path: &str) -> Result<Vec<u8>, ServiceError> {
    if let Some(pack_relative) = media_paths::image_pack_relative_path(relative_path) {
        let pack_relative = pack_relative.to_string_lossy().into_owned();
        if let Ok(pack_path) = media_paths::resolve_inside(root, &pack_relative) {
            if pack_path.is_file() {
                // 包内条目名就是**文件名**（不含目录）—— 见
                // `media_paths::image_pack_relative_path` 的两条约定。
                let normalized = relative_path.trim().replace('\\', "/");
                let entry_name = normalized.rsplit('/').next().unwrap_or_default().to_owned();
                if let Some(bytes) = read_pack_entry(&pack_path, &entry_name) {
                    return Ok(bytes);
                }
            }
        }
    }
    // 逃逸路径在这里**报错而不是回退**：`resolve_inside` 拒绝它是有意的，
    // 把它当「文件不存在」会让上层以为只是缺文件（见 `media_paths` 的说明）。
    let loose = media_paths::resolve_inside(root, relative_path)?;
    std::fs::read(&loose).map_err(|error| {
        ServiceError::from(sm_db::DbError::business(
            "ImageStore",
            format!("读取图片 {} 失败：{error}", loose.display()),
        ))
    })
}

/// 临时包路径：`<pack>.tmp-<随机>`。
///
/// # 为什么名字里要带随机后缀
///
/// 重建是「写临时包 -> 原子替换」。两个进程同时对同一部影片重建时，**共用一个
/// 临时名**会让它们互相写坏同一个文件 —— 而原子替换会把坏的那份扶正。
/// 上游两处（`movie_asset_pack` 与 `thumbnails/artifacts`）都用同款命名。
pub fn temp_pack_path(pack_path: &Path) -> std::path::PathBuf {
    let name = pack_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    pack_path.with_file_name(format!("{name}.tmp-{}", uuid::Uuid::new_v4().simple()))
}

/// 备份包路径：`<pack>.bak`。上游两处都用这个后缀。
///
/// 存在理由是**回滚**：替换成功后若 DB 登记失败，包必须能退回旧的那一份
/// （见 `artifacts::persist`）。
pub fn backup_pack_path(pack_path: &Path) -> std::path::PathBuf {
    let name = pack_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    pack_path.with_file_name(format!("{name}.bak"))
}

/// 清掉遗留的临时包（`<pack>.tmp-*`）。
///
/// 上一次重建中途崩溃会留下它。不清理的话：`remove_loose_files` 会把它们当成
/// 「包前缀文件」保留，于是一个几千兆的垃圾文件永远躺在影片目录里。
///
/// 失败**静默跳过**（上游捕 `OSError`）—— 清理失败不该让重建整体失败。
pub fn cleanup_stale_temp_files(pack_path: &Path) {
    let Some(parent) = pack_path.parent() else {
        return;
    };
    let Some(name) = pack_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let prefix = format!("{name}.tmp-");
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 包路径上的 IO 错误 → 信封化的服务错误。
///
/// 单独一个函数是为了让每个失败点都带上**包路径** —— 只报「Permission denied」
/// 而不知道是哪个包，在几十个影片目录里没法排查。
fn pack_error(pack_path: &Path, error: impl std::fmt::Display) -> ServiceError {
    ServiceError::from(sm_db::DbError::business(
        "ImagePack",
        format!("图片包 {} 操作失败：{error}", pack_path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pack(name: &str) -> std::path::PathBuf {
        // 每个用例一个独立文件名：并行跑时不许互相踩。
        let unique = format!(
            "sm-pack-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        std::env::temp_dir().join(unique)
    }

    /// ★ 写出来能读回来，且**压缩方式是 STORED**（不是 deflate）。
    ///
    /// 「能不能读回来」保证的是 `finish()` 没漏；压缩方式这一条保证的是没
    /// 悄悄退回默认的 Deflated —— 那会让打包慢一个数量级而产物体积几乎不变，
    /// **不会有任何报错**。
    #[test]
    fn a_written_pack_reads_back_and_entries_are_stored() {
        let path = temp_pack("stored.zip");
        let entries = vec![
            ("a.jpg".to_owned(), vec![1_u8, 2, 3]),
            ("b.jpg".to_owned(), vec![4_u8; 64]),
        ];
        write_pack(&path, &entries).expect("写包");

        assert_eq!(read_pack_entry(&path, "a.jpg"), Some(vec![1, 2, 3]));
        assert_eq!(read_pack_entry(&path, "b.jpg"), Some(vec![4_u8; 64]));
        assert_eq!(read_pack_entry(&path, "missing.jpg"), None);

        let file = std::fs::File::open(&path).expect("开包");
        let mut archive = zip::ZipArchive::new(file).expect("解析包");
        for name in ["a.jpg", "b.jpg"] {
            let entry = archive.by_name(name).expect("取条目");
            assert_eq!(
                entry.compression(),
                zip::CompressionMethod::Stored,
                "{name} 必须是 STORED —— 图片已压缩，deflate 只花 CPU"
            );
        }

        std::fs::remove_file(&path).ok();
    }

    /// 包不存在 / 不是包 —— 都返回 `None`，不 panic 也不报错。
    #[test]
    fn a_missing_or_corrupt_pack_yields_no_entry() {
        let missing = temp_pack("absent.zip");
        assert_eq!(read_pack_entry(&missing, "a.jpg"), None);

        let corrupt = temp_pack("corrupt.zip");
        std::fs::write(&corrupt, b"not a zip at all").expect("写坏文件");
        assert_eq!(read_pack_entry(&corrupt, "a.jpg"), None);

        std::fs::remove_file(&corrupt).ok();
    }
}
