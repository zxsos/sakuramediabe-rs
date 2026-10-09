//! 媒体图片根的路径原语（上游 `common/media_paths.py` 的**子集**）。
//!
//! # 为什么单独一个模块
//!
//! 本仓没有上游那个 `common/` 层。这三件事被 `catalog` 与 `playback` 两侧都要用
//! （`image_cleanup` 删文件、`movie_asset_pack` 写包、未来的
//! `media_thumbnail_service` 写 `thumbnails.zip`），所以不能挂在任何一个服务上：
//!
//! | 原语 | 谁在用 |
//! |---|---|
//! | [`media_image_root_path`] | 两个包/清理服务 |
//! | [`image_pack_relative_path`] | `image_cleanup`（判断路径在不在包里）|
//! | [`resolve_inside`] | 所有要**按库里的路径删/写文件**的地方 |
//!
//! 上游 `media_paths.py` 里还有字幕命名分配（`allocate_next_movie_subtitle_path`
//! 等）与番号目录分片（`movie_asset_shard`），本仓尚未落地 —— 到 `movie_subtitle`
//! 那一批再补，不要现在抄一半。

use std::path::{Component, Path, PathBuf};

use crate::error::ServiceError;
use crate::system::config::ConfigService;

/// 默认图片根，与 `sm-core` 的 `media.import_image_root_path` 缺省值一致。
pub const DEFAULT_IMAGE_ROOT: &str = "/data/cache/assets";

/// 影片资产在图片根下的一级目录名（与 `videos/`、`actors/` 平级）。
pub const MOVIE_ASSETS_SUBDIR: &str = "movies";

/// 影片图片包的文件名，与影片目录同级。
pub const MOVIE_ASSETS_PACK_NAME: &str = "assets.zip";

/// 番号目录内部的保留子目录名：缩略图按 `media/<media_id>/thumbnails` 归档。
pub const MOVIE_MEDIA_SUBDIR: &str = "media";

/// 分片目录名取 sha1 十六进制**前 2 位**，固定 256 片。
///
/// 目的：`movies/` 下的条目数从「番号个数」降到常数 256 —— 否则一个几万部影片
/// 的库会让 `readdir` 与人工浏览都变慢。
pub const MOVIE_ASSET_SHARD_HEX_LENGTH: usize = 2;

/// 把番号 / `javdb_id` 等 owner key 归一成安全目录名。
///
/// **封面、剧照、缩略图、字幕四类资产必须走同一个归一化**，否则同一部影片会散
/// 到两个目录 —— 而症状是「封面在，缩略图没了」，很难联想到是目录名不同。
///
/// 归一化后为空（全是非法字符）时退化成 `unknown`：宁可用一个共用目录，也不要
/// 让它落到 `movies/<shard>/` 本身（那会把子目录当文件）。
pub fn normalize_asset_dir_name(owner_key: &str) -> String {
    let sanitized: String = owner_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(|c| c == '.' || c == '_' || c == '-');
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// 番号资产目录的分片名：`sha1` 十六进制前 [`MOVIE_ASSET_SHARD_HEX_LENGTH`] 位。
///
/// # 入参必须是**最终落盘的目录名本身**（已归一化）
///
/// 上游的注释写明了这条。若一处传番号原文、另一处传归一化后的名字，同一部影片
/// 会算出两个分片。
///
/// # 必须是 SHA-1
///
/// 上游用 `hashlib.sha1`。换成 SHA-256 会算出**完全不同的分片**，
/// 于是迁移过来的数据目录里既有图片一张都找不到 —— 表现为「图片全丢」，
/// 且不会有任何报错。见根 `Cargo.toml` 里 `sha1` 的注释。
pub fn movie_asset_shard(dir_name: &str) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(dir_name.as_bytes());
    let hex = format!("{digest:x}");
    hex[..MOVIE_ASSET_SHARD_HEX_LENGTH].to_owned()
}

/// 番号资产目录的库内相对路径 `movies/<shard>/<番号>`，可直接拼进 `image.origin`。
pub fn movie_asset_relative_dir(dir_name: &str) -> PathBuf {
    PathBuf::from(MOVIE_ASSETS_SUBDIR)
        .join(movie_asset_shard(dir_name))
        .join(dir_name)
}

/// 时间轴缩略图在影片目录里的子目录名。
pub const MEDIA_THUMBNAILS_SUBDIR: &str = "thumbnails";

/// 时间轴缩略图包的后缀（`thumbnails.zip`）。
pub const MEDIA_THUMBNAILS_PACK_SUFFIX: &str = ".zip";

/// 媒体图片根目录。**从配置读**（`media.import_image_root_path`）。
///
/// # 三条与上游一致、但容易漏的规则
///
/// 1. 先做 `~` 展开（上游 `Path(...).expanduser()`）；
/// 2. 相对路径按**进程当前工作目录**解析成绝对路径（上游
///    `(Path.cwd() / p).resolve()`）—— **不是**相对图片根，这一条很容易想当然；
/// 3. 配置缺项/类型不对时用缺省值 [`DEFAULT_IMAGE_ROOT`]，与 `sm-core` 的
///    schema 缺省一致。
pub fn media_image_root_path(config: &ConfigService) -> Result<PathBuf, ServiceError> {
    let snapshot = config.snapshot()?;
    let raw = snapshot
        .get("media")
        .and_then(|section| section.get("import_image_root_path"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(DEFAULT_IMAGE_ROOT)
        .trim()
        .to_owned();

    let expanded = expand_tilde(&raw)?;
    if expanded.is_absolute() {
        return Ok(expanded);
    }
    let cwd = std::env::current_dir().map_err(|error| {
        ServiceError::from(sm_db::DbError::business(
            "MediaPaths",
            format!("无法读取当前工作目录以解析相对图片根：{error}"),
        ))
    })?;
    Ok(cwd.join(expanded))
}

/// 图片对应的**包**相对路径；不属于任何包时 `None`。
///
/// 上游 `common/media_paths.image_pack_relative_path`。两条约定：
///
/// 1. `<...>/media/<media_id>/thumbnails/<name>` 的包与目录**同级同名**，
///    即 `<...>/media/<media_id>/thumbnails.zip`，条目名就是 `<name>`；
/// 2. `movies/<shard>/<番号>/<name>` 的包是同目录 `assets.zip`。
///
/// # 约定 2 的**深度必须固定为 4**
///
/// 判断是「段数恰好 4 且首段是 `movies`」，不是「看到 `movies/` 就算」。
/// 否则 `movies/<shard>/<番号>/subtitles/x.srt` 会被算成影片图片、进
/// `assets.zip` 的重建集合 —— 而字幕**不在 `image` 表里**，混进去就是往包里
/// 塞一个找不到源文件的条目。
///
/// ⚠️ 只做路径推导，**不检查包是否存在**。
pub fn image_pack_relative_path(relative_path: &str) -> Option<PathBuf> {
    let normalized = relative_path.trim().replace('\\', "/");
    let path = Path::new(&normalized);
    // 没有文件名（空路径、`a/`、`/`）就没有可打包的东西。
    path.file_name()?;
    let parent = path.parent()?;

    // 约定 1：`.../thumbnails/<name>` -> `.../thumbnails.zip`
    if parent
        .file_name()
        .is_some_and(|name| name == MEDIA_THUMBNAILS_SUBDIR)
    {
        return Some(parent.with_file_name(format!(
            "{MEDIA_THUMBNAILS_SUBDIR}{MEDIA_THUMBNAILS_PACK_SUFFIX}"
        )));
    }

    // 约定 2：`movies/<shard>/<番号>/<name>`（段数恰好 4）
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() == 4 && parts[0] == MOVIE_ASSETS_SUBDIR {
        return Some(parent.join(MOVIE_ASSETS_PACK_NAME));
    }

    None
}

/// 把库里存的相对路径解析成 `root` **之内**的绝对路径。
///
/// # ⚠️ 上游没有这层检查 —— 这是本仓的加固
///
/// `image.origin` 理应是相对路径，但它只是 `varchar(255)`、由导入流程写入。
/// 一个 bug 或一次手工改库就能让它变成 `../../etc/passwd`。而下游是
/// `unlink`／写包 —— **动到图片根之外的文件不可撤销**。
///
/// 上游 `_unlink_image_file(image_root / relative_path)` 直接就用，
/// 且 Python 的 `Path.__truediv__` 遇到绝对路径会**替换**掉整个前缀
/// （Rust 的 `Path::join` 行为相同），也就是 `/etc/passwd` 这种输入会原样
/// 落到系统文件上。
///
/// 所以逐段检查：绝对路径、`..`、以及根/盘符前缀一律拒绝；`.` 与重复分隔符
/// 无害，跳过。
pub fn resolve_inside(root: &Path, relative: &str) -> Result<PathBuf, ServiceError> {
    let candidate = Path::new(relative.trim());
    let mut resolved = root.to_path_buf();
    let mut depth = 0_usize;
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                resolved.push(part);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ServiceError::validation(
                    "invalid_image_path",
                    format!("图片路径逃逸出图片根目录，已拒绝：{relative}"),
                ));
            }
        }
    }
    if depth == 0 {
        // 空路径解析出来就是图片根本身 —— 删它等于删掉整个图片库。
        return Err(ServiceError::validation(
            "invalid_image_path",
            format!("图片路径为空，已拒绝：{relative}"),
        ));
    }
    Ok(resolved)
}

/// 删掉一个文件；文件不存在返回 `Ok(false)`。
///
/// # 错误策略由**调用方**决定，所以这里向上传播
///
/// 上游两处的处置**不同**，这不是笔误：
///
/// | 调用方 | 上游做法 |
/// |---|---|
/// | `image_cleanup._unlink_image_file` | 只捕 `FileNotFoundError`，**其余冒到路由**（权限问题变成 500）|
/// | `movie_asset_pack` 的三处清理 | 捕 `OSError`，**静默跳过**（清理失败不该让重建整体失败）|
///
/// 所以这个函数只把「不存在」当成功（目标状态已达成），其余错误原样返回，
/// 由调用方按各自的上游策略处理（包那边 `.ok()` 掉即可）。
pub fn remove_file_if_exists(path: &Path) -> Result<bool, ServiceError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ServiceError::from(sm_db::DbError::business(
            "MediaPaths",
            format!("删除文件 {} 失败：{error}", path.display()),
        ))),
    }
}

/// `~` 展开。上游 `Path.expanduser()` 的**子集**。
///
/// 只处理 `~` 与 `~/...`。`~user/...` 需要读 `/etc/passwd`（或 Windows 的
/// 用户目录表），本仓没有可用依赖 —— 与其把它当普通相对路径默默解成
/// `<cwd>/~user/...`，不如**报错**：那两行差异会让运维找很久。
fn expand_tilde(raw: &str) -> Result<PathBuf, ServiceError> {
    let Some(rest) = raw.strip_prefix('~') else {
        return Ok(PathBuf::from(raw));
    };
    if !rest.is_empty() && !rest.starts_with('/') && !rest.starts_with('\\') {
        return Err(ServiceError::validation(
            "invalid_image_root_path",
            format!("不支持 `~user` 形式的图片根路径：{raw}"),
        ));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            ServiceError::validation(
                "invalid_image_root_path",
                "图片根路径用了 `~`，但读不到 HOME / USERPROFILE",
            )
        })?;
    let trimmed = rest.trim_start_matches(['/', '\\']);
    Ok(if trimmed.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(trimmed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逃逸出图片根的路径一律拒绝。
    #[test]
    fn paths_escaping_the_image_root_are_rejected() {
        let root = Path::new("/data/cache/assets");
        for bad in [
            "../../etc/passwd",
            "/etc/passwd",
            "movies/../../etc/passwd",
            "",
            "   ",
            "./",
        ] {
            assert!(
                resolve_inside(root, bad).is_err(),
                "{bad:?} 应当被拒绝，却解析成了 {:?}",
                resolve_inside(root, bad)
            );
        }
    }

    /// 正常路径解析到图片根之下，`.` 与重复分隔符无害。
    #[test]
    fn ordinary_paths_resolve_below_the_root() {
        let root = Path::new("/data/cache/assets");
        assert_eq!(
            resolve_inside(root, "movies/ab/ABC-001/1.jpg").unwrap(),
            PathBuf::from("/data/cache/assets/movies/ab/ABC-001/1.jpg")
        );
        assert_eq!(
            resolve_inside(root, "./movies//ab/ABC-001/1.jpg").unwrap(),
            PathBuf::from("/data/cache/assets/movies/ab/ABC-001/1.jpg")
        );
    }

    /// ★ 分片名必须与上游 `hashlib.sha1(...).hexdigest()[:2]` **逐字相同**。
    ///
    /// 这三个期望值就是 Python 算出来的。改成 SHA-256 会得到完全不同的分片，
    /// 于是迁移过来的数据目录里既有图片**一张都找不到** —— 表现为「图片全丢」，
    /// 而且不会有任何报错。所以这不是「选个哈希都行」的场合。
    #[test]
    fn the_shard_matches_upstreams_sha1() {
        assert_eq!(movie_asset_shard("ABC-001"), "a0");
        assert_eq!(movie_asset_shard("abc/def"), "0d");
        assert_eq!(movie_asset_shard(""), "da");
        assert_eq!(
            movie_asset_relative_dir("ABC-001"),
            PathBuf::from("movies/a0/ABC-001")
        );
    }

    /// 归一化：非法字符换 `_`，首尾的 `.`/`_`/`-` 去掉，全空退化 `unknown`。
    ///
    /// **四类资产（封面/剧照/缩略图/字幕）必须走同一个归一化** —— 否则同一部
    /// 影片会散到两个目录，症状是「封面在，缩略图没了」。
    #[test]
    fn asset_dir_names_are_normalized_the_same_way_everywhere() {
        assert_eq!(normalize_asset_dir_name("ABC-001"), "ABC-001");
        assert_eq!(normalize_asset_dir_name("abc/def"), "abc_def");
        assert_eq!(normalize_asset_dir_name("a b"), "a_b");
        assert_eq!(normalize_asset_dir_name("  ..A B..  "), "A_B");
        // 全非法字符时退化成一个共用目录，而不是落到 `movies/<shard>/` 本身
        // （那会把子目录当成文件）。
        assert_eq!(normalize_asset_dir_name(""), "unknown");
        assert_eq!(normalize_asset_dir_name("///"), "unknown");
        assert_eq!(normalize_asset_dir_name("..."), "unknown");
    }

    /// `~user` 必须报错而不是被当成相对路径。
    #[test]
    fn tilde_user_is_rejected_instead_of_silently_relative() {
        assert!(expand_tilde("~someone/assets").is_err());
        assert!(expand_tilde("/data/cache/assets").is_ok());
    }

    /// ★ 两条包约定各自成立，且**深度不匹配也不算影片图片**。
    #[test]
    fn pack_relative_paths_follow_the_two_conventions() {
        assert_eq!(
            image_pack_relative_path("movies/ab/ABC-001/1.jpg").unwrap(),
            PathBuf::from("movies/ab/ABC-001/assets.zip")
        );
        assert_eq!(
            image_pack_relative_path("movies/ab/ABC-001/media/12/thumbnails/1.jpg").unwrap(),
            PathBuf::from("movies/ab/ABC-001/media/12/thumbnails.zip")
        );
        // 深度 5 的 `movies/...` 不是影片资产（比如字幕目录）—— 判据是段数恰好 4。
        assert_eq!(
            image_pack_relative_path("movies/ab/ABC-001/subtitles/1.srt"),
            None
        );
        // 没有文件名。
        assert_eq!(image_pack_relative_path("movies/ab/ABC-001/"), None);
        assert_eq!(image_pack_relative_path(""), None);
    }
}
