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
//! | [`movie_subtitle_dir`] | 字幕写/读两侧（`subtitle_asset` / `movie_subtitle`）|
//! | [`is_movie_subtitle_target_name`] | 同上（分配与识别规范名）|
//! | [`ensure_movie_subtitle_path`] | 同上（读/写任何字幕文件前必过）|
//!
//! 字幕那一族的原语（扩展名白名单、`<番号>-<N>` 分配、路径逃逸校验）与画像/包
//! 的原语**同源**（上游都是 `common/media_paths.py` + `common/subtitle_paths.py`），
//! 所以放在这里而不是任一侧的服务里 —— 放服务里就会有两份「番号目录怎么算」，
//! 而那两份一旦分叉，症状是「图片在、字幕找不到」。

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

// ---------------------------------------------------------------- 字幕路径原语

/// 字幕目录在影片资产目录里的子目录名。上游 `MOVIE_SUBTITLES_SUBDIR`（`:11`）。
pub const MOVIE_SUBTITLES_SUBDIR: &str = "subtitles";

/// 默认字幕扩展名。上游 `MOVIE_SUBTITLE_EXTENSION`（`:96`）。
pub const MOVIE_SUBTITLE_EXTENSION: &str = ".srt";

/// 受支持的字幕扩展名。上游 `MOVIE_SUBTITLE_EXTENSIONS`（`media_paths.py:97`）。
///
/// ⚠️ **四项，没有 `.sub`**。骨架期 `subtitle_asset::is_subtitle_extension` 自己
/// 抄了一份五项的表（多了 `.sub`），于是 `.sub` 能过写侧的扩展名检查、却卡在
/// [`allocate_next_movie_subtitle_path`] 的白名单上 —— 一个「有时收有时拒」的
/// 分支。列表只留这一份，两边都从这里取。
pub const MOVIE_SUBTITLE_EXTENSIONS: [&str; 4] = [".srt", ".ass", ".ssa", ".vtt"];

/// 该扩展名（**含点、小写**）是否受支持。
pub fn is_movie_subtitle_extension(extension: &str) -> bool {
    MOVIE_SUBTITLE_EXTENSIONS.contains(&extension)
}

/// 文件名的小写扩展名（**含点**）；没有扩展名时 `None`。
///
/// 复刻 Python 的 `Path(name).suffix.lower()`，两处**容易漏**的行为：
///
/// | 输入 | `suffix` | 这里 |
/// |---|---|---|
/// | `"a.SRT"` | `.SRT` → 小写 | `.srt` |
/// | `"a"` | `""` | `None` |
/// | `".srt"`（隐藏文件） | `""` | `None` |
///
/// 最后一行不是抠字眼：上游 `import_subtitle_content` 拿 `Path(filename).suffix`
/// 做白名单校验，而 `Path(".srt").suffix` 是**空串**，于是 `filename=".srt"` 会
/// 落进「不支持的扩展名」分支。若这里返回 `.srt`，同一个请求就变成「收下并落盘」。
pub fn subtitle_extension_of(file_name: &str) -> Option<String> {
    let name = file_name.trim();
    let dot = name.rfind('.')?;
    // 点在**开头**且后面没有别的点（`.srt`）时，Python 认为没有扩展名。
    (dot > 0).then(|| name[dot..].to_ascii_lowercase())
}

/// 该文件名是不是本仓分配的规范字幕名 `<番号>-<N><扩展名>`。
///
/// 上游 `is_movie_subtitle_target_name`（`media_paths.py:99-110`）。两条容易写错：
///
/// * 扩展名比对**大小写敏感**（上游是 `file_name.endswith(extension)`，而常量表
///   是小写）—— 别顺手 `to_lowercase()`，那会把 `.SRT` 认成规范名；
/// * `N` 必须与 `str(int(N))` 相同，也就是**拒绝前导零**（`01`），但 `0` 本身合法。
pub fn is_movie_subtitle_target_name(movie_number: &str, file_name: &str) -> bool {
    let prefix = format!("{movie_number}-");
    for extension in MOVIE_SUBTITLE_EXTENSIONS {
        let Some(stem) = file_name.strip_suffix(extension) else {
            continue;
        };
        let Some(tail) = stem.strip_prefix(&prefix) else {
            return false;
        };
        return !tail.is_empty()
            && tail.bytes().all(|byte| byte.is_ascii_digit())
            && (tail == "0" || !tail.starts_with('0'));
    }
    false
}

/// 影片字幕目录的**绝对**路径：`<图片根>/movies/<分片>/<番号>/subtitles`。
///
/// 上游 `movie_subtitle_dir`（`media_paths.py:65`）。番号先过
/// [`normalize_asset_dir_name`] —— 分片名与目录名都按归一化后的名字算，
/// 与图片资产走同一套规则（否则同一部影片的字幕和图会落到两个目录）。
pub fn movie_subtitle_dir(
    config: &ConfigService,
    movie_number: &str,
) -> Result<PathBuf, ServiceError> {
    let dir_name = normalize_asset_dir_name(movie_number);
    Ok(media_image_root_path(config)?
        .join(movie_asset_relative_dir(&dir_name))
        .join(MOVIE_SUBTITLES_SUBDIR))
}

/// 上游 `_current_max_subtitle_sequence`（`media_paths.py:115-127`）：
/// 扫字幕目录里已落盘的规范名，取最大的 `N`；目录不存在返回 0。
fn current_max_subtitle_sequence(subtitle_dir: &Path, movie_number: &str) -> i64 {
    if !subtitle_dir.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(subtitle_dir) else {
        return 0;
    };
    let prefix = format!("{movie_number}-");
    let mut max_seq = 0_i64;
    for entry in entries.flatten() {
        // 上游 `entry.is_file()` —— 目录（与不可读项）跳过。软链指向文件时算文件，
        // 与 `Path::is_file()` 一样跟随软链。
        if !entry.path().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_movie_subtitle_target_name(movie_number, &name) {
            continue;
        }
        if let Some(seq) = sequence_of_subtitle_name(movie_number, &name, &prefix) {
            max_seq = max_seq.max(seq);
        }
    }
    max_seq
}

/// 从规范名 `<番号>-<N><扩展名>` 里取出 `N`。**调用方必须先过白名单校验。**
fn sequence_of_subtitle_name(movie_number: &str, file_name: &str, prefix: &str) -> Option<i64> {
    let _ = movie_number;
    // 去掉扩展名（`Path.stem` 的语义：最后一个点之后是后缀）。
    let dot = file_name.rfind('.')?;
    file_name[..dot].strip_prefix(prefix)?.parse::<i64>().ok()
}

/// 分配下一个字幕目标路径 `<字幕目录>/<番号>-<N><扩展名>`。
///
/// 上游 `allocate_next_movie_subtitle_path`（`media_paths.py:130-160`）。
/// `N` 从 `max(已落盘的最大 N, 本批已预留名里的最大 N) + 1` 起 ——
/// `reserved_names` 是**同一批处理里已分配但还没落盘**的文件名，不传就会在一批
/// 多文件时重复分配同一个名字。
///
/// `extension` 必须是**带点的小写**扩展名：上游拿它直接与
/// [`MOVIE_SUBTITLE_EXTENSIONS`]（元素带点）比对，传 `"srt"` 会报
/// 「不支持的扩展名」。照抄，别在这里替调用方补点。
pub fn allocate_next_movie_subtitle_path(
    config: &ConfigService,
    movie_number: &str,
    reserved_names: &std::collections::BTreeSet<String>,
    extension: &str,
) -> Result<PathBuf, ServiceError> {
    let normalized = extension.to_ascii_lowercase();
    if !is_movie_subtitle_extension(&normalized) {
        return Err(ServiceError::validation(
            "invalid_subtitle_extension",
            format!("不支持的字幕扩展名: {extension}"),
        ));
    }
    let subtitle_dir = movie_subtitle_dir(config, movie_number)?;
    let prefix = format!("{movie_number}-");
    let mut max_seq = current_max_subtitle_sequence(&subtitle_dir, movie_number);
    for name in reserved_names {
        if !is_movie_subtitle_target_name(movie_number, name) {
            continue;
        }
        if let Some(seq) = sequence_of_subtitle_name(movie_number, name, &prefix) {
            max_seq = max_seq.max(seq);
        }
    }
    Ok(subtitle_dir.join(format!("{movie_number}-{}{normalized}", max_seq + 1)))
}

/// 「字幕路径非法」的统一出口：**403 `file_path_invalid`**。
///
/// 上游 `ApiError(403, "file_path_invalid", "文件路径非法")`
/// （`common/subtitle_paths.py:14` / `:33`）。
pub fn invalid_subtitle_path(file_path: &Path) -> ServiceError {
    ServiceError::from_status(
        403,
        "file_path_invalid",
        format!("文件路径非法：{}", file_path.display()),
    )
}

/// 规范化字幕路径：`~` 展开 → 相对路径按 **进程 cwd** 绝对化 → 解析 → 扩展名白名单。
///
/// 上游 `normalize_subtitle_path`（`common/subtitle_paths.py:9-18`）。
///
/// # 两条与骨架期不同、都是上游行为
///
/// 1. **不要求文件存在**：上游 `Path.resolve()` 在 Python 3.6+ 是非严格的
///    （不存在也能解析出绝对路径）。骨架期用 `canonicalize()`，于是「文件没了」
///    被判成「路径非法」→ 403，而上游那条路是 409（`subtitle_unavailable`）。
/// 2. **要过扩展名白名单**：`suffix.lower()` 不在表里就是 403。骨架期只查了
///    是否在根目录之内。
pub fn normalize_subtitle_path(file_path: &Path) -> Result<PathBuf, ServiceError> {
    let raw = file_path.to_string_lossy();
    let expanded = expand_tilde(raw.trim())?;
    let absolute = if expanded.is_absolute() {
        resolve_lenient(&expanded)
    } else {
        let cwd = std::env::current_dir().map_err(|error| {
            ServiceError::from(sm_db::DbError::business(
                "MediaPaths",
                format!("读不到当前工作目录，字幕相对路径无法解析：{error}"),
            ))
        })?;
        resolve_lenient(&cwd.join(expanded))
    };
    let extension = absolute
        .file_name()
        .and_then(|name| subtitle_extension_of(&name.to_string_lossy()));
    match extension {
        Some(extension) if is_movie_subtitle_extension(&extension) => Ok(absolute),
        _ => Err(invalid_subtitle_path(file_path)),
    }
}

/// 校验字幕绝对路径落在**该影片的标准字幕目录**之内，返回规范化后的路径。
///
/// 上游 `ensure_movie_subtitle_path`（`common/subtitle_paths.py:28-34`）。
///
/// # 错误语义：只报「非法」，不报「不存在」
///
/// 上游这个函数**只**在两种情况下抛 403：扩展名不在白名单、解析后不在字幕目录
/// 之下。文件不存在**不是**它的判据 —— 那由调用方的 `is_file()` 决定，走的是
/// 409 `subtitle_unavailable`。两件事混起来会让「字幕文件被别的进程删了」
/// 显示成「你的请求路径非法」，把排查方向带偏。
///
/// # 为什么根目录也要解析
///
/// 只做字符串前缀比对会被**软链**绕过（`<字幕目录>/link -> /etc`，
/// 于是 `<字幕目录>/link/passwd` 通过了字符串检查）。两边都先用
/// `resolve_lenient` 解析到真实路径再比。
pub fn ensure_movie_subtitle_path(
    config: &ConfigService,
    movie_number: &str,
    file_path: &Path,
) -> Result<PathBuf, ServiceError> {
    let absolute = normalize_subtitle_path(file_path)?;
    let root = resolve_lenient(&movie_subtitle_dir(config, movie_number)?);
    if absolute.starts_with(&root) {
        Ok(absolute)
    } else {
        Err(invalid_subtitle_path(file_path))
    }
}

/// **非严格**解析：能解析多少算多少（上游 `Path.resolve()`）。
///
/// 从路径自身向上找到第一个**存在**的祖先做 `canonicalize`（顺带解掉软链与
/// `.` / `..`），再把剩下的段原样接回去。整条路径都不存在时退化成原样返回 ——
/// 与 Python 的 `resolve(strict=False)` 行为一致。
fn resolve_lenient(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            // 走到根（或相对路径的起点）还是解析不出来 —— 原样返回。
            _ => return path.to_path_buf(),
        }
    }
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

    /// ★ 扩展名白名单**没有 `.sub`**（上游 `media_paths.py:97` 只有四项）。
    ///
    /// 骨架期 `subtitle_asset` 自己抄了一份五项的表，于是 `.sub` 能过写侧的
    /// 扩展名检查、却卡在分配目标路径的白名单上 —— 「有时收有时拒」。
    #[test]
    fn the_subtitle_extension_table_matches_upstream() {
        assert_eq!(MOVIE_SUBTITLE_EXTENSIONS.len(), 4);
        for extension in [".srt", ".ass", ".ssa", ".vtt"] {
            assert!(is_movie_subtitle_extension(extension), "{extension} 该收");
        }
        assert!(!is_movie_subtitle_extension(".sub"), ".sub 不在上游表里");
        assert!(!is_movie_subtitle_extension("srt"), "必须是带点的小写形式");
        assert!(
            !is_movie_subtitle_extension(".SRT"),
            "表里是小写，比对不做归一"
        );
    }

    /// ★ `Path(name).suffix.lower()` 的三个易错点。
    #[test]
    fn the_extension_of_a_name_follows_pythons_suffix_rules() {
        assert_eq!(subtitle_extension_of("a.SRT").as_deref(), Some(".srt"));
        assert_eq!(subtitle_extension_of("a.tar.srt").as_deref(), Some(".srt"));
        assert_eq!(subtitle_extension_of("a").as_deref(), None);
        // 隐藏文件：`Path(".srt").suffix` 是空串 —— 于是 `.srt` 这个名字会落进
        // 「不支持的扩展名」分支，而不是被当成字幕收下。
        assert_eq!(subtitle_extension_of(".srt").as_deref(), None);
    }

    /// ★ 规范名 `<番号>-<N><扩展名>`：拒绝前导零与别的番号，扩展名比对大小写敏感。
    #[test]
    fn subtitle_target_names_are_recognized_like_upstream() {
        assert!(is_movie_subtitle_target_name("ABC-001", "ABC-001-1.srt"));
        assert!(is_movie_subtitle_target_name("ABC-001", "ABC-001-12.ass"));
        // `str(int(N)) == N`：`0` 合法，`01` 不合法。
        assert!(is_movie_subtitle_target_name("ABC-001", "ABC-001-0.srt"));
        assert!(!is_movie_subtitle_target_name("ABC-001", "ABC-001-01.srt"));
        // 大小写敏感（上游 `file_name.endswith(extension)`，表里是小写）。
        assert!(!is_movie_subtitle_target_name("ABC-001", "ABC-001-1.SRT"));
        // 别的番号、别的扩展名、别的形状。
        assert!(!is_movie_subtitle_target_name("ABC-001", "ABC-002-1.srt"));
        assert!(!is_movie_subtitle_target_name("ABC-001", "ABC-001-1.sub"));
        assert!(!is_movie_subtitle_target_name("ABC-001", "ABC-001-.srt"));
        assert!(!is_movie_subtitle_target_name("ABC-001", "ABC-001-x.srt"));
    }

    /// ★ 已落盘的最大序号从目录里扫出来；目录不存在算 0。
    #[test]
    fn the_current_max_sequence_scans_only_canonical_names() {
        let dir =
            std::env::temp_dir().join(format!("sm-sub-seq-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        assert_eq!(current_max_subtitle_sequence(&dir, "ABC-001"), 0);

        for name in [
            "ABC-001-1.srt",
            "ABC-001-3.srt",
            "ABC-001-01.srt", // 前导零：不算
            "ABC-002-9.srt",  // 别的番号
            "notes.txt",      // 不是规范名
        ] {
            std::fs::write(dir.join(name), b"x").expect("写文件");
        }
        // 子目录不算（上游 `entry.is_file()`）。
        std::fs::create_dir_all(dir.join("ABC-001-7.srt")).expect("建子目录");

        assert_eq!(current_max_subtitle_sequence(&dir, "ABC-001"), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ★ `resolve_lenient` 不要求路径存在（上游 `Path.resolve()` 非严格）。
    ///
    /// 这是「文件没了 → 409 还是 403」的分界：判据用的是**解析后仍在根目录之下**
    /// 而不是「文件在不在」。
    #[test]
    fn lenient_resolution_does_not_require_existence() {
        let dir =
            std::env::temp_dir().join(format!("sm-sub-path-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let root = resolve_lenient(&dir);

        let missing = dir.join("ABC-001-1.srt");
        assert!(!missing.exists());
        let resolved = resolve_lenient(&missing);
        assert!(
            resolved.starts_with(&root),
            "不存在的子路径仍应解析到根目录之下：{}",
            resolved.display()
        );

        // `..` 要走出去 —— 逃逸判定不能因为文件不存在就放行。
        let escaped = resolve_lenient(&dir.join("..").join("elsewhere.srt"));
        assert!(!escaped.starts_with(&root), "{}", escaped.display());
        std::fs::remove_dir_all(&dir).ok();
    }
}
