//! 媒体资产在磁盘上的**布局规则**（上游 `src/common/media_paths.py`）。
//!
//! # 为什么这一层单独存在
//!
//! `image.origin` 里存的是**相对路径**，它同时被四处消费：
//! 落盘（`movie_image`）、读取（`image_store`）、打包（`assets.zip`）、
//! 清理（`image_cleanup`）。四处各拼一次路径就会各自漂移 ——
//! 同一部影片的图片散到两个目录，而清理只删其中一个。
//!
//! # 规则（与上游逐字对齐）
//!
//! | 资产 | 相对路径 |
//! |---|---|
//! | 影片封面 | `movies/<shard>/<番号>/cover<ext>` |
//! | 影片薄封面 | `movies/<shard>/<番号>/thin-cover<ext>` |
//! | 剧情图 | `movies/<shard>/<番号>/plot-<i><ext>` |
//! | 影片资产包 | `movies/<shard>/<番号>/assets.zip` |
//! | 演员头像 | `actors/<safe><ext>` |
//! | 缩略图 | `.../media/<media_id>/thumbnails/<name>`，包为同级 `thumbnails.zip` |
//!
//! `<shard>` = `sha1(归一化后的番号)[:2]`，**固定 256 片**：顶层 `movies/`
//! 的条目数从「番号数（30 万）」降到常数 256。
//!
//! # ★ 分片必须拿**归一化后的目录名**去算
//!
//! 上游 `movie_asset_shard` 的文档原话：「入参必须是最终落盘的目录名本身
//! （已归一化）」。先分片再归一会让同一部片落到两个 shard —— 而路径一旦
//! 写进 `image.origin` 就不会再改。
//!
//! # 剧情图**平铺**，不建 `plots/` 子目录
//!
//! 上游 `:425` 的理由：30 万规模下会多出 30 万个空目录。

use std::path::{Path, PathBuf};

/// 影片资产在图片根下的一级目录名，与 `videos/` / `actors/` 平级。
pub const MOVIE_ASSETS_SUBDIR: &str = "movies";
/// 影片图片（封面 / 薄封面 / 剧情图）的打包形态：同目录、不含压缩。
pub const MOVIE_ASSETS_PACK_NAME: &str = "assets.zip";
/// 时间轴缩略图的子目录名。
pub const MEDIA_THUMBNAILS_SUBDIR: &str = "thumbnails";
/// 缩略图包的后缀。包与目录同级同名。
pub const THUMBNAILS_PACK_SUFFIX: &str = ".zip";
/// 分片目录名取 sha1 十六进制前 2 位。
pub const MOVIE_ASSET_SHARD_HEX_LENGTH: usize = 2;

/// 把番号 / `javdb_id` 等 owner key 归一成安全目录名。
///
/// 上游 `normalize_asset_dir_name`（`media_paths.py:39-45`）：
/// `[^0-9A-Za-z._-]` → `_`，再 `strip("._-")`，全空则 `unknown`。
///
/// ★ 封面 / 剧照 / 缩略图 / 字幕四类资产**必须走同一个归一化** —— 否则同一
/// 部影片会散到两个目录。
pub fn normalize_asset_dir_name(owner_key: &str) -> String {
    let replaced: String = owner_key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = replaced.trim_matches(|ch| matches!(ch, '.' | '_' | '-'));
    if trimmed.is_empty() {
        return "unknown".to_owned();
    }
    trimmed.to_owned()
}

/// 番号资产目录的分片名：`sha1(dir_name)[:2]`。
///
/// **入参必须是已归一化的目录名**（见模块文档）。
pub fn movie_asset_shard(dir_name: &str) -> String {
    use sha1::{Digest, Sha1};
    // `sha1(dir_name)` 十六进制串的前 2 位 = 第一个字节的两位十六进制。
    let digest = Sha1::digest(dir_name.as_bytes());
    format!("{:02x}", digest[0])
}

/// 番号资产目录的库内相对路径 `movies/<shard>/<番号>`。
///
/// 可直接拼进 `image.origin`。**入参应当是已归一化的目录名。**
pub fn movie_asset_relative_dir(dir_name: &str) -> String {
    format!(
        "{}/{}/{}",
        MOVIE_ASSETS_SUBDIR,
        movie_asset_shard(dir_name),
        dir_name
    )
}

/// 影片封面的相对路径。
pub fn movie_cover_relative_path(movie_number: &str, extension: &str) -> String {
    format!(
        "{}/cover{}",
        movie_asset_relative_dir(&normalize_asset_dir_name(movie_number)),
        normalize_image_extension(Some(extension))
    )
}

/// 影片薄封面的相对路径。
pub fn thin_cover_relative_path(movie_number: &str, extension: &str) -> String {
    format!(
        "{}/thin-cover{}",
        movie_asset_relative_dir(&normalize_asset_dir_name(movie_number)),
        normalize_image_extension(Some(extension))
    )
}

/// 第 `index` 张剧情图的相对路径。**平铺在资产目录下**（见模块文档）。
pub fn movie_plot_relative_path(movie_number: &str, index: i32, extension: &str) -> String {
    format!(
        "{}/plot-{}{}",
        movie_asset_relative_dir(&normalize_asset_dir_name(movie_number)),
        index,
        normalize_image_extension(Some(extension))
    )
}

/// 演员头像的相对路径 `actors/<safe><ext>`。
pub fn actor_relative_path(owner_key: &str, extension: &str) -> String {
    format!(
        "actors/{}{}",
        normalize_asset_dir_name(owner_key),
        normalize_image_extension(Some(extension))
    )
}

/// 归一化图片扩展名。上游 `_normalize_image_extension`（`:88-97`）：
/// 去空白转小写 → 空则 `.jpg` → 补前导 `.` → **超过 8 字符也回退 `.jpg`**
/// （URL 里带查询串时会截出一长串，那个不能当扩展名）。
pub fn normalize_image_extension(raw_extension: Option<&str>) -> String {
    let normalized = raw_extension
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if normalized.is_empty() {
        return ".jpg".to_owned();
    }
    let with_dot = if normalized.starts_with('.') {
        normalized
    } else {
        format!(".{normalized}")
    };
    if with_dot.len() > 8 {
        return ".jpg".to_owned();
    }
    with_dot
}

/// 图片对应的**包**相对路径；非可打包路径返回 `None`。
///
/// 上游 `image_pack_relative_path`（`media_paths.py:82-102`）两条约定：
///
/// 1. `.../thumbnails/<name>` → 同级 `thumbnails.zip`；
/// 2. `movies/<shard>/<番号>/<name>`（**深度固定 4**）→ 同目录 `assets.zip`；
///    更深的子目录（`subtitles/`、`media/.../thumbnails/`）不匹配。
///
/// 只做路径推导，**不检查包是否存在**。
pub fn image_pack_relative_path(relative_path: &str) -> Option<String> {
    let normalized = relative_path.trim().replace('\\', "/");
    let normalized = normalized.trim_matches('/');
    if normalized.is_empty() {
        return None;
    }
    let parts: Vec<&str> = normalized.split('/').collect();
    let name = parts.last().copied()?;
    if name.is_empty() {
        return None;
    }
    // 约定一：缩略图。
    if parts.len() >= 2 && parts[parts.len() - 2] == MEDIA_THUMBNAILS_SUBDIR {
        let parent: String = parts[..parts.len() - 1].join("/");
        return Some(format!("{parent}{THUMBNAILS_PACK_SUFFIX}"));
    }
    // 约定二：影片资产（深度固定 4）。
    if parts.len() == 4 && parts[0] == MOVIE_ASSETS_SUBDIR {
        return Some(format!(
            "{}/{}",
            parts[..3].join("/"),
            MOVIE_ASSETS_PACK_NAME
        ));
    }
    None
}

/// 相对路径 → 绝对路径。**唯一**的拼接入口。
pub fn absolute(root: &Path, relative: &str) -> PathBuf {
    root.join(relative.trim().replace('\\', "/").trim_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 危险字符全换成 `_`，首尾的 `.` / `_` / `-` 被剥掉。
    #[test]
    fn the_directory_name_is_normalized_like_upstream() {
        assert_eq!(normalize_asset_dir_name("ABC-123"), "ABC-123");
        assert_eq!(normalize_asset_dir_name("a b/c"), "a_b_c");
        assert_eq!(normalize_asset_dir_name(".."), "unknown", "全被剥掉时兜底");
        assert_eq!(normalize_asset_dir_name("___"), "unknown");
        assert_eq!(normalize_asset_dir_name(".-abc-."), "abc");
    }

    /// 分片**稳定**：同一个目录名永远落在同一个 shard，且只有 2 位十六进制。
    #[test]
    fn the_shard_is_two_hex_chars_and_stable() {
        let shard = movie_asset_shard("ABC-123");
        assert_eq!(shard.len(), 2);
        assert!(shard.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(shard, movie_asset_shard("ABC-123"));
        assert_ne!(shard, movie_asset_shard("ABC-124"));
    }

    /// 四类图片都落在**同一个**资产目录下（否则清理会漏删另一半）。
    #[test]
    fn all_movie_images_share_one_asset_directory() {
        let dir = movie_asset_relative_dir(&normalize_asset_dir_name("ABC-123"));
        assert!(movie_cover_relative_path("ABC-123", ".jpg").starts_with(&dir));
        assert!(thin_cover_relative_path("ABC-123", ".jpg").starts_with(&dir));
        assert!(movie_plot_relative_path("ABC-123", 0, ".jpg").starts_with(&dir));
        assert_eq!(
            movie_plot_relative_path("ABC-123", 2, ".jpg"),
            format!("{dir}/plot-2.jpg"),
            "剧情图平铺，不建 plots/ 子目录"
        );
        assert_eq!(actor_relative_path("abc", ""), "actors/abc.jpg");
    }

    /// 扩展名规则：空 → `.jpg`；补前导点；**超过 8 字符回退 `.jpg`**
    /// （URL 带查询串时会截出一长串）。
    #[test]
    fn the_extension_is_normalized_like_upstream() {
        assert_eq!(normalize_image_extension(None), ".jpg");
        assert_eq!(normalize_image_extension(Some("")), ".jpg");
        assert_eq!(normalize_image_extension(Some(" .PNG ")), ".png");
        assert_eq!(normalize_image_extension(Some("png")), ".png");
        assert_eq!(
            normalize_image_extension(Some(".jpeg?token=abcdef")),
            ".jpg",
            "过长的扩展名回退 .jpg"
        );
    }

    /// 包路径的两条约定（上游 `:82-102`）。
    #[test]
    fn the_pack_path_follows_both_conventions() {
        let movie = movie_cover_relative_path("ABC-123", ".jpg");
        assert_eq!(
            image_pack_relative_path(&movie),
            Some(format!(
                "{}/{}",
                movie_asset_relative_dir("ABC-123"),
                MOVIE_ASSETS_PACK_NAME
            ))
        );
        assert_eq!(
            image_pack_relative_path("movies/ab/ABC-123/subtitles/x.srt"),
            None,
            "更深的子目录不匹配"
        );
        assert_eq!(
            image_pack_relative_path("media/1/thumbnails/0001.jpg"),
            Some("media/1/thumbnails.zip".to_owned())
        );
        assert_eq!(image_pack_relative_path(""), None);
    }

    /// 拼接入口统一处理 Windows 分隔符与多余斜杠 —— `image.origin` 里存的是
    /// posix 风格，而部署可能在 Windows 上。
    #[test]
    fn absolute_joins_a_posix_relative_path() {
        assert_eq!(
            absolute(Path::new("/root"), "movies/ab/ABC-123/cover.jpg"),
            PathBuf::from("/root/movies/ab/ABC-123/cover.jpg")
        );
        assert_eq!(
            absolute(Path::new("/root"), r"movies\ab\cover.jpg"),
            PathBuf::from("/root/movies/ab/cover.jpg")
        );
    }
}
