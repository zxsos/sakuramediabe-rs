//! 测试与压测用的本地目录数据集。
//!
//! 布局：
//!
//! ```text
//! <root>/
//!   a-clips/
//!     clip-01.mp4          4096 B
//!     note.txt               11 B
//!   b-movies/
//!     movie-01.mkv         8192 B
//!     c-nested/
//!       deep-01.mov        2048 B
//!   z-readme.txt             32 B
//! ```
//!
//! 根层的 `z-readme.txt` 排在子目录之后，是为了检验
//! `ScanImportSource` 的遍历顺序确实可预测（本层文件先出，之后才下沉）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use prost_types::Struct;
use sm_plugin_api::v1::{LibraryHandle, MediaHandle};

use crate::opaque::string_ref;
use crate::provider::PROVIDER_KEY;

/// 根层条目名，按 provider 的字典序排列。Browse 的分页断言用它。
pub const ROOT_ENTRY_NAMES: [&str; 3] = ["a-clips", "b-movies", "z-readme.txt"];

/// `ScanImportSource` 必须严格按此顺序吐出条目。
pub const SCAN_RELATIVE_ORDER: [&str; 5] = [
    "z-readme.txt",
    "a-clips/clip-01.mp4",
    "a-clips/note.txt",
    "b-movies/movie-01.mkv",
    "b-movies/c-nested/deep-01.mov",
];

/// 临时根目录的前缀成分，避免不同测试互相踩到同一棵数据集。
const ROOT_FILE: &str = "z-readme.txt";
const ROOT_FILE_SIZE: usize = 32;

/// 子目录里的文件（相对路径, 字节数）。
const FILE_LAYOUT: [(&str, usize); 4] = [
    ("a-clips/clip-01.mp4", 4096),
    ("a-clips/note.txt", 11),
    ("b-movies/movie-01.mkv", 8192),
    ("b-movies/c-nested/deep-01.mov", 2048),
];

/// 数据集里某个相对路径应有的字节数；不在数据集里返回 0。
pub fn expected_size(relative_path: &str) -> i64 {
    if relative_path == ROOT_FILE {
        return ROOT_FILE_SIZE as i64;
    }
    FILE_LAYOUT
        .iter()
        .find(|(path, _)| *path == relative_path)
        .map(|(_, size)| *size as i64)
        .unwrap_or(0)
}

/// 生成一个唯一的临时根目录。
///
/// 不用 `tempfile`：那会为「跑一次距离级测量」引入一个新依赖。
pub fn scratch_root(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "plugin-ref-local-{tag}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

/// 在 `root` 下铺好上面那棵树。
pub async fn populate_tree(root: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(root.join("a-clips")).await?;
    tokio::fs::create_dir_all(root.join("b-movies/c-nested")).await?;
    tokio::fs::write(root.join(ROOT_FILE), filler(ROOT_FILE_SIZE)).await?;
    for (relative_path, size) in FILE_LAYOUT {
        tokio::fs::write(root.join(relative_path), filler(size)).await?;
    }
    Ok(())
}

/// 构造一个指向本地参考插件的 library 句柄。
pub fn library_handle(library_id: i64) -> LibraryHandle {
    LibraryHandle {
        library_id,
        provider_key: PROVIDER_KEY.to_owned(),
        provider_config: Some(Struct::default()),
        account_key: None,
    }
}

/// 构造一个 media 句柄；`relative_path` 同时进 `storage_ref.path` 与 `file_name`。
pub fn media_handle(
    media_id: i64,
    library: LibraryHandle,
    relative_path: &str,
    duration_seconds: i64,
) -> MediaHandle {
    let file_name = relative_path
        .rsplit('/')
        .next()
        .unwrap_or(relative_path)
        .to_owned();
    MediaHandle {
        media_id,
        library: Some(library),
        storage_ref: Some(string_ref(relative_path)),
        file_name,
        file_size_bytes: expected_size(relative_path),
        duration_seconds,
    }
}

fn filler(size: usize) -> Vec<u8> {
    vec![b'x'; size]
}
