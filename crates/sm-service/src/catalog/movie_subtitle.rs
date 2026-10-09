//! 影片字幕的读取与列出（上游 `catalog/movie_subtitle_service.py`，183 行）。
//!
//! # 读字幕**不需要** provider
//!
//! `docs/handoff.md` 里 `GET /movies/{n}/subtitles` 长期挂在「卡死的（不用试）」，
//! 理由写的是「要读媒体文件系统（provider 族）」。
//!
//! ⚠️ **那个判断是错的**：字幕文件由 [`super::subtitle_asset`] 落在宿主自己的
//! 字幕目录里（`<图片根>/movies/<分片>/<番号>/subtitles`）。provider 参与的是
//! 「把字幕从媒体库**搬过来**」那一步（写侧），读侧只读宿主目录。
//!
//! # ★ 10 MiB 上限判两次
//!
//! [`MAX_SUBTITLE_CONTENT_BYTES`]。上游 `read_subtitle_content`（`:69-83`）先
//! `os.fstat` 判一次，再用 `file.read(MAX + 1)` 限读、读完按实际长度再判一次。
//! 两次的理由正是 `fstat`：文件在 `stat` 与 `read` 之间可能被写大，所以**读的
//! 时候也要限**。只判一次遇到正在写入的文件会把内存打满。
//!
//! ⚠️ 骨架期这里写着「上游用 `os.stat`（不是 `fstat`）」—— 是错的：`:73` 是
//! `os.fstat(file.fileno())`。
//!
//! # 路径校验用 [`crate::catalog::media_paths::ensure_movie_subtitle_path`]
//!
//! 上游 `common/subtitle_paths.ensure_movie_subtitle_path`。它**只**管两件事：
//! 扩展名不在白名单、解析后不在该影片的字幕目录之下。**文件不存在不是它的
//! 判据** —— 那是调用方的 `is_file()`，报的是 409 `subtitle_unavailable`。
//! 骨架期那个 `ensure_subtitle_path` 把两者混成 403，已删除。
//!
//! # 与上游的一处**刻意**分层差异：签名 URL 由路由层拼
//!
//! 上游 `get_movie_subtitles` 返回 `MovieSubtitleListResource`，每项带
//! `url=build_signed_subtitle_url(subtitle_id)`（`:150-167`）—— 它自己能拿全局
//! 配置。本仓的签名密钥由路由层持有（`sm-api/src/signing.rs` 的
//! `signing_secret`，与 `clip_collections` / `videos` 拼 `stream_url` 同款分层），
//! 所以这里返回 [`MovieSubtitleList`]（**不带 URL**），路由拿到 items 后补。
//! 字段与上游逐字对齐（`subtitle_id` / `file_name` / `format` / `size_bytes` /
//! `created_at`），拼出来的 JSON 与上游一致。

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use sm_db::catalog::movie::Movie;
use sm_db::repo::{MovieRepository, SubtitleRepository};
use sm_db::Db;

use crate::catalog::media_paths::{ensure_movie_subtitle_path, movie_subtitle_dir};
use crate::error::{details_of, ServiceError};
use crate::system::config::ConfigService;

/// 字幕内容大小上限（10 MiB）。见模块文档。
pub const MAX_SUBTITLE_CONTENT_BYTES: u64 = 10 * 1024 * 1024;

/// 读侧错误。**抛错**（与写侧相反，见 [`super::subtitle_asset`] 的模块文档）。
///
/// 每个取值的 `code()` 与上游 `SubtitleReadError(code, message)` 的 `code`
/// **逐字相同**（`schema/catalog/subtitles.py:57-63`）：插件/前端就是靠它分流的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleReadError {
    MovieNotFound,
    SubtitleNotFound,
    /// ★ 路径逃逸（`../`、绝对路径、软链指向目录外）或扩展名不在白名单。
    PathInvalid,
    /// 文件在库里有记录但磁盘上读不到（或不是普通文件）。
    Unavailable,
    /// 超过 [`MAX_SUBTITLE_CONTENT_BYTES`]。
    TooLarge,
}

impl SubtitleReadError {
    /// 对外错误码。上游那五个 `code` 字符串。
    pub fn code(&self) -> &'static str {
        match self {
            Self::MovieNotFound => "movie_not_found",
            Self::SubtitleNotFound => "subtitle_not_found",
            Self::PathInvalid => "subtitle_path_invalid",
            Self::Unavailable => "subtitle_unavailable",
            Self::TooLarge => "subtitle_too_large",
        }
    }

    /// 面向用户的说明。上游文案照抄。
    pub fn message(&self) -> &'static str {
        match self {
            Self::MovieNotFound => "影片不存在",
            Self::SubtitleNotFound => "该影片下不存在此字幕",
            Self::PathInvalid => "字幕路径非法",
            Self::Unavailable => "字幕文件不可访问",
            Self::TooLarge => "字幕文件超过 10 MiB",
        }
    }

    /// 对应的 HTTP 状态码。
    ///
    /// `PathInvalid` 是 **403** 而不是 404 —— 路径不合法是「请求不被允许」，
    /// 而「字幕不存在」才是 404。
    pub fn status(&self) -> u16 {
        match self {
            Self::MovieNotFound | Self::SubtitleNotFound => 404,
            Self::PathInvalid => 403,
            Self::Unavailable => 409,
            Self::TooLarge => 413,
        }
    }
}

impl std::fmt::Display for SubtitleReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for SubtitleReadError {}

impl From<SubtitleReadError> for ServiceError {
    /// 读侧错误 → 服务层错误：**状态码与错误码都取上面那张表**。
    ///
    /// 不重新映射一遍：重映射就是第二份契约，改一处漏一处。
    fn from(error: SubtitleReadError) -> Self {
        ServiceError::from_status(error.status(), error.code(), error.message())
    }
}

/// 一条字幕的只读元信息。上游 `SubtitleAsset`（`:38-46`）。
///
/// ⚠️ 按上游 reshape：`format` / `size_bytes` 来自**磁盘 stat**，库里没有指纹列
/// 也没有大小列。骨架期这个结构体带着 `content_hash` 与 `language` —— 那两个
/// 字段在 DDL 里都不存在（`subtitle` 只有 `(movie_id, file_path)` + 时间戳）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubtitleAsset {
    pub subtitle_id: i32,
    /// 文件名（**仅展示**，不可用于拼路径）。
    pub file_name: String,
    /// 扩展名，**不带点**、小写（上游 `path.suffix.lower().lstrip(".")`）。
    pub format: String,
    /// 文件字节数。
    pub size_bytes: i64,
    /// 登记时刻。DDL 里可空，所以是 `Option`（上游模型非空，差异仅在于迁移数据）。
    pub created_at: Option<NaiveDateTime>,
}

/// 某部影片的字幕列表。上游 `MovieSubtitleListResource`（`:14-16`）。
///
/// 上游那两项是 `movie_number` 与 `items`；items 里每项在路由层补 `url`（见模块
/// 文档的分层说明）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MovieSubtitleList {
    pub movie_number: String,
    pub items: Vec<SubtitleAsset>,
}

/// 字幕内容。上游 `SubtitleContent`（`:49-54`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleContent {
    pub subtitle_id: i32,
    /// 文件字节。**按原始字节返回**，编码判断交给客户端。
    pub content: Vec<u8>,
    /// 本次读取内容的 SHA-256（上游 `:87`）。客户端据此判断内容有没有变。
    pub sha256: String,
}

/// 字幕服务（读取侧）。
///
/// # 为什么它有状态
///
/// 四个入口都要发 SQL（查影片、查/列/删/建字幕记录），而路径校验与目录扫描都要
/// 从**配置**读图片根。骨架期是单元结构体 + 关联函数，拿不到 `Db` 与
/// `ConfigService`，于是五个方法都落不了地。
pub struct MovieSubtitleService {
    db: Db,
    config: ConfigService,
}

impl MovieSubtitleService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
        }
    }

    /// 列出某部影片的全部字幕。
    ///
    /// 上游 `list_subtitle_assets`（`:33-53`）。**纯读**：跳过失效记录，不扫描、
    /// 不登记、不清理文件 —— 那些是 [`Self::sync_movie_subtitles`] 的活。
    ///
    /// 「影片不存在」与「影片没有字幕」是两件事：前者 404，后者空列表。
    ///
    /// 跳过（而不是报错）的三类：路径非法 / `stat` 失败 / 不是普通文件
    /// （上游 `:44-45` 捕 `ApiError, OSError, RuntimeError` 后 `continue`）——
    /// 一条坏记录不该让播放页的字幕列表整个 500。
    pub async fn list_subtitle_assets(
        &self,
        movie_id: i32,
    ) -> Result<Vec<SubtitleAsset>, ServiceError> {
        let movie = self.require_movie(movie_id).await?;
        let rows = SubtitleRepository::new(self.db.clone())
            .list_by_movie(movie_id)
            .await?;

        let mut items = Vec::new();
        for row in rows {
            let Ok(path) = ensure_movie_subtitle_path(
                &self.config,
                &movie.movie_number,
                Path::new(&row.file_path),
            ) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            items.push(SubtitleAsset {
                subtitle_id: row.id,
                file_name: file_name_of(&path),
                format: format_of(&path),
                size_bytes: metadata.len() as i64,
                created_at: row.created_at,
            });
        }
        Ok(items)
    }

    /// ★ 读一条字幕的内容。
    ///
    /// 上游 `read_subtitle_content`（`:55-88`）。顺序照抄，**别重排**：
    ///
    /// 1. 查影片（不存在 → [`SubtitleReadError::MovieNotFound`]）
    /// 2. 按 `(影片, 字幕 id)` 查记录（不存在 → `SubtitleNotFound`）
    /// 3. 路径校验（不过 → `PathInvalid`）
    /// 4. 打开 + `fstat` 判上限（超限 → `TooLarge`；**先判后读**）
    /// 5. 限读 `MAX + 1`，再按实际长度判一次
    /// 6. 读不到（记录在、文件没了/不是普通文件）→ `Unavailable`
    ///
    /// # 返回 `ServiceError`，而不是 `SubtitleReadError`
    ///
    /// 骨架期的签名是 `Result<SubtitleContent, SubtitleReadError>`，而那个形状
    /// **表达不了「数据库连不上」** —— 只能硬塞进五个客户端错误之一，把 500 说成
    /// 「字幕不存在」。现在客户端错误经 [`From<SubtitleReadError>`] 转成相同的
    /// 状态码与错误码，基础设施错误原样往上走。
    pub async fn read_subtitle_content(
        &self,
        movie_id: i32,
        subtitle_id: i32,
    ) -> Result<SubtitleContent, ServiceError> {
        let movie = self.require_movie(movie_id).await?;
        let Some(row) = SubtitleRepository::new(self.db.clone())
            .find_in_movie(movie_id, subtitle_id)
            .await?
        else {
            return Err(SubtitleReadError::SubtitleNotFound.into());
        };

        let path = ensure_movie_subtitle_path(
            &self.config,
            &movie.movie_number,
            Path::new(&row.file_path),
        )
        // 路径非法（含扩展名不在白名单）→ 403。上游 `:64-66` 捕 `ApiError`
        // 与 `RuntimeError` 后报 `subtitle_path_invalid`。
        .map_err(|_| ServiceError::from(SubtitleReadError::PathInvalid))?;

        let content = read_capped(&path, &row.file_path)?;
        Ok(SubtitleContent {
            subtitle_id: row.id,
            sha256: hashing::sha256_hex(&content),
            content,
        })
    }

    /// 下载路由用：由字幕 id 解析出**磁盘上的绝对路径**。
    ///
    /// 上游 `resolve_subtitle_file_path`（`common/file_signatures.py:264-271`）：
    /// 按主键取记录（没有 → `404 subtitle_not_found`），再过
    /// [`ensure_movie_subtitle_path`]（路径非法 → `403 file_path_invalid`）。
    ///
    /// # 与 [`Self::read_subtitle_content`] 的区别（**别合并**）
    ///
    /// 那个是「读内容」入口：文件不在报 **409** `subtitle_unavailable`、超过
    /// 10 MiB 报 **413**。下载路由两者都不是 —— 文件不在是 **404
    /// `file_not_found`**（上游 `require_existing_file`），而且**不设大小上限**
    /// （`FileResponse` 是流式的；10 MiB 是读接口的保护）。合并会把两种语义糊成
    /// 一个，客户端拿到的码就不对了。
    pub async fn resolve_file_path(&self, subtitle_id: i32) -> Result<PathBuf, ServiceError> {
        let Some(row) = SubtitleRepository::new(self.db.clone())
            .find(subtitle_id)
            .await?
        else {
            return Err(ServiceError::not_found_with(
                "subtitle_not_found",
                "字幕不存在",
                details_of("subtitle_id", serde_json::Value::from(subtitle_id)),
            ));
        };
        let movie = self.require_movie(row.movie_id).await?;
        ensure_movie_subtitle_path(&self.config, &movie.movie_number, Path::new(&row.file_path))
    }

    /// 按番号取该影片的字幕列表（端点入口）。
    ///
    /// 上游 `get_movie_subtitles`（`:90-104`）：`require_record`（找不到 →
    /// `404 movie_not_found`，details 带 `movie_number`）后走纯读那条路。
    ///
    /// 端点是 `/movies/{n}/subtitles`，`n` 就是番号（不是主键）。
    pub async fn get_movie_subtitles(
        &self,
        movie_number: &str,
    ) -> Result<MovieSubtitleList, ServiceError> {
        let movie = MovieRepository::new(self.db.clone())
            .find_by_number(movie_number)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "movie_not_found",
                    "影片不存在",
                    details_of("movie_number", serde_json::Value::from(movie_number)),
                )
            })?;
        let items = self.list_subtitle_assets(movie.id).await?;
        Ok(MovieSubtitleList {
            movie_number: movie.movie_number,
            items,
        })
    }

    /// 扫描字幕目录，与记录**双向**同步。
    ///
    /// 上游 `sync_movie_subtitles`（`:106-139`）。用于「字幕已由其它途径放进
    /// 目录」的情况。
    ///
    /// ⚠️ 骨架期这里写着「算指纹查重 -> 登记缺失的」—— **两半都不对**：
    ///
    /// 1. 它**不算指纹**。去重键是**规范化后的路径字符串**（`:110` / `:129-132`）；
    /// 2. 它是**双向**的：先删掉**失效记录**（路径非法 `:117-120`；路径合法但
    ///    文件不存在 `:121-124`），再为扫到但没登记的文件建记录（`:127-133`）。
    ///    只「补登记」会把坏记录一直留在列表里，播放页于是持续暴露死链。
    ///
    /// 返回的三个键照上游：`created_subtitles` / `deleted_subtitles` /
    /// `total_subtitles`（最后一个是**同步后**的路径数，不是数据库行数）。
    pub async fn sync_movie_subtitles(
        &self,
        movie_id: i32,
    ) -> Result<serde_json::Value, ServiceError> {
        let movie = self.require_movie(movie_id).await?;
        let repo = SubtitleRepository::new(self.db.clone());

        let discovered = self.discover_subtitle_paths(&movie)?;
        let existing_rows = repo.list_by_movie(movie_id).await?;

        // 键是**规范化后**的绝对路径字符串 —— 与 `discover_subtitle_paths` 里
        // `ensure_movie_subtitle_path` 的返回值同型，才能直接比。
        let mut existing_by_path: std::collections::BTreeMap<String, sm_db::Subtitle> =
            std::collections::BTreeMap::new();
        let mut deleted_count = 0_u64;
        for row in existing_rows {
            let resolved = match ensure_movie_subtitle_path(
                &self.config,
                &movie.movie_number,
                Path::new(&row.file_path),
            ) {
                Ok(path) => path,
                // 路径非法：记录本身不可信，删掉（上游 `:117-120`）。
                Err(_) => {
                    repo.delete(row.id).await?;
                    deleted_count += 1;
                    continue;
                }
            };
            // 路径合法但文件不在了：删掉（上游 `:121-124`）。
            if !resolved.exists() {
                repo.delete(row.id).await?;
                deleted_count += 1;
                continue;
            }
            existing_by_path.insert(resolved.to_string_lossy().into_owned(), row);
        }

        let mut created_count = 0_u64;
        for path in discovered {
            let key = path.to_string_lossy().into_owned();
            if existing_by_path.contains_key(&key) {
                continue;
            }
            let row = repo
                .create(&sm_db::repo::NewSubtitle {
                    movie_id,
                    file_path: key.clone(),
                })
                .await?;
            existing_by_path.insert(key, row);
            created_count += 1;
        }

        Ok(serde_json::json!({
            "created_subtitles": created_count,
            "deleted_subtitles": deleted_count,
            "total_subtitles": existing_by_path.len(),
        }))
    }

    /// 影片不存在 → `404 movie_not_found`（details 带影片 id）。
    ///
    /// 上游读侧有两条不同的「影片不存在」出口：`list/read` 抛
    /// `SubtitleReadError("movie_not_found")`、`get_movie_subtitles` 走
    /// `require_record(error_code="movie_not_found")`。两者的**码相同**，所以
    /// 本仓统一成同一个 [`ServiceError`]。
    async fn require_movie(&self, movie_id: i32) -> Result<Movie, ServiceError> {
        MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "movie_not_found",
                    "影片不存在",
                    details_of("movie_id", serde_json::Value::from(movie_id)),
                )
            })
    }

    /// 扫该影片的标准字幕目录，返回**已校验**的路径。
    ///
    /// 上游 `_discover_subtitle_paths`（`:169-183`）：
    ///
    /// * 目录不存在 → 空列表（**不是错误**：还没导入过字幕的影片就是这样）；
    /// * 只收 `.srt`（上游这里**硬编码** `.srt`，不走白名单 —— 白名单是写侧的事）；
    /// * 排序按文件名小写（保证同一目录两次扫描结果一致）；
    /// * 每条再过一次路径校验，不通过的**跳过**。
    fn discover_subtitle_paths(&self, movie: &Movie) -> Result<Vec<PathBuf>, ServiceError> {
        let scan_root = movie_subtitle_dir(&self.config, &movie.movie_number)?;
        if !scan_root.is_dir() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&scan_root).map_err(|error| {
            ServiceError::from(sm_db::DbError::business(
                "MovieSubtitle",
                format!("读字幕目录失败：{}（{error}）", scan_root.display()),
            ))
        })?;

        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_srt = path
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("srt"));
            if !is_srt {
                continue;
            }
            candidates.push(path);
        }
        candidates.sort_by_key(|path| file_name_of(path).to_lowercase());

        let mut discovered = Vec::new();
        for path in candidates {
            // 用**规范化后**的路径：与记录那边（`ensure_movie_subtitle_path` 的
            // 返回值）必须同型才能比出「这个文件已经登记过了」。
            if let Ok(resolved) =
                ensure_movie_subtitle_path(&self.config, &movie.movie_number, &path)
            {
                discovered.push(resolved);
            }
        }
        Ok(discovered)
    }
}

/// 文件名（含扩展名）。取不到时是空串 —— 路径校验已保证有文件名。
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 扩展名，**不带点、小写**（上游 `path.suffix.lower().lstrip(".")`）。
fn format_of(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 读文件内容，**先 `fstat` 判上限、再限读 `MAX + 1`、读完再判一次**。
///
/// 上游 `:69-83`。每一步都不能省：
///
/// * 只判 `fstat`：文件在判完之后被写大，`read` 会把内存打满；
/// * 只限读：读到 `MAX + 1` 时得知道「是超了」而不是「正好这么大」，
///   所以用 `== MAX + 1` 判超限。
///
/// `record_path` 只用于错误信息（记录里那份原文，便于对比磁盘上解析后的路径）。
fn read_capped(resolved: &Path, record_path: &str) -> Result<Vec<u8>, ServiceError> {
    use std::io::Read;

    let mut file = std::fs::File::open(resolved)
        // 记录在、文件打不开 → 409（不是 404：记录确实存在）。
        .map_err(|error| {
            tracing::warn!(path = %resolved.display(), record_path, %error, "字幕文件打不开");
            ServiceError::from(SubtitleReadError::Unavailable)
        })?;
    let metadata = file.metadata().map_err(|error| {
        tracing::warn!(path = %resolved.display(), %error, "字幕文件 stat 失败");
        ServiceError::from(SubtitleReadError::Unavailable)
    })?;
    if !metadata.is_file() {
        return Err(SubtitleReadError::Unavailable.into());
    }
    if metadata.len() > MAX_SUBTITLE_CONTENT_BYTES {
        return Err(SubtitleReadError::TooLarge.into());
    }

    // `MAX + 1`：多读一个字节就能区分「正好等于上限」与「超过上限」。
    let mut content = Vec::new();
    file.by_ref()
        .take(MAX_SUBTITLE_CONTENT_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|error| {
            tracing::warn!(path = %resolved.display(), %error, "字幕文件读取失败");
            ServiceError::from(SubtitleReadError::Unavailable)
        })?;
    if content.len() as u64 > MAX_SUBTITLE_CONTENT_BYTES {
        return Err(SubtitleReadError::TooLarge.into());
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 五个错误码与上游 `SubtitleReadError(code, ...)` **逐字相同**。
    ///
    /// 插件/前端靠 code 分流；改一个字母等于改协议。
    #[test]
    fn the_read_codes_match_upstream() {
        for (error, code) in [
            (SubtitleReadError::MovieNotFound, "movie_not_found"),
            (SubtitleReadError::SubtitleNotFound, "subtitle_not_found"),
            (SubtitleReadError::PathInvalid, "subtitle_path_invalid"),
            (SubtitleReadError::Unavailable, "subtitle_unavailable"),
            (SubtitleReadError::TooLarge, "subtitle_too_large"),
        ] {
            assert_eq!(error.code(), code);
        }
    }

    /// ★ `PathInvalid` 是 **403**，`SubtitleNotFound` 是 **404**。
    ///
    /// 混成 404 会把「有人在探测路径逃逸」伪装成「字幕不存在」。
    #[test]
    fn invalid_path_is_403_but_missing_subtitle_is_404() {
        assert_eq!(SubtitleReadError::PathInvalid.status(), 403);
        assert_eq!(SubtitleReadError::SubtitleNotFound.status(), 404);
        assert_eq!(SubtitleReadError::MovieNotFound.status(), 404);
    }

    /// 「记录在、文件没了」是 **409** 而不是 404 —— 记录确实存在。
    ///
    /// 报 404 会让客户端以为字幕被删了，从而不再重试同步。
    #[test]
    fn a_missing_file_is_conflict_not_not_found() {
        assert_eq!(SubtitleReadError::Unavailable.status(), 409);
        assert_ne!(
            SubtitleReadError::Unavailable.status(),
            SubtitleReadError::SubtitleNotFound.status()
        );
    }

    /// 超过上限是 **413**，不是 422/500。
    #[test]
    fn too_large_is_413() {
        assert_eq!(SubtitleReadError::TooLarge.status(), 413);
        assert_eq!(MAX_SUBTITLE_CONTENT_BYTES, 10 * 1024 * 1024);
    }

    /// ★ 转成 `ServiceError` 时**不重新映射**：码与状态都取那张表。
    #[test]
    fn the_conversion_reuses_the_same_table() {
        let error: ServiceError = SubtitleReadError::PathInvalid.into();
        assert_eq!(error.status, 403);
        assert_eq!(error.code(), "subtitle_path_invalid");
    }

    /// 扩展名与文件名从**路径**取：`format` 不带点、小写。
    #[test]
    fn format_and_name_come_from_the_path() {
        let path = Path::new("/tmp/movies/ab/ABC-001/subtitles/ABC-001-1.SRT");
        assert_eq!(file_name_of(path), "ABC-001-1.SRT");
        assert_eq!(format_of(path), "srt");
    }
}
