//! 字幕资产写入（上游 `catalog/subtitle_asset_service.py`，161 行）。
//!
//! # 与 [`super::movie_subtitle`] 是**写/读**两侧，方向相反
//!
//! | | 本文件 | `movie_subtitle` |
//! |---|---|---|
//! | 方向 | 写入 | 读取 + 列出 |
//! | 入口 | 插件经 `PluginContext.import_subtitle` 调用 | HTTP 端点 |
//! | 失败方式 | **返回状态枚举**，不抛错 | 抛 `SubtitleReadError` |
//!
//! ⚠️ 这个不对称是刻意的：写侧被插件调用，抛异常会变成插件崩溃；
//! 读侧对着 HTTP，异常能变成 4xx。
//!
//! # ★ 对外状态名与上游**逐字对齐**
//!
//! 上游 `schema/catalog/subtitles.py:23-27`：
//!
//! | 情况 | `SubtitleImportStatus` | 这里 |
//! |---|---|---|
//! | 新写入 | `imported` | [`SubtitleImportStatus::Imported`] |
//! | 已存在同内容 | `duplicate` | [`SubtitleImportStatus::Duplicate`] |
//! | 影片不存在 | `movie_not_found` | [`SubtitleImportStatus::MovieNotFound`] |
//! | 扩展名不接受 | `invalid_format` | [`SubtitleImportStatus::InvalidFormat`] |
//!
//! 骨架期这四个名字是 `AlreadyExists` / `UnsupportedExtension`，还多了一个上游
//! **没有**的 `PersistFailed`；而结果结构体的第三个字段当时叫 `language`，
//! 上游那一列叫 `reason`。这些字符串要进 JSON（插件返回值、任务摘要），
//! 名字漂了客户端就分不清「已存在」与「落盘失败」。已按上游改名 ——
//! `language` 只留在**入参**上（上游 `del language`：`Subtitle` 模型前后端都没有
//! 语言列，参数是为后续版本预留的）。
//!
//! # 指纹是**现读文件算出来的**，不是从库里取的
//!
//! ⚠️ 骨架期这里写着「查该影片字幕记录的内容指纹（存在表里，不重算文件）」——
//! **两个前提都不成立**：DDL 的 `subtitle` 表只有 `(movie_id, file_path)` 两列
//! （`docker/schema.sql`，与上游模型 `model/catalog/movies.py` 一致），**没有
//! 指纹列**；上游 `movie_subtitle_hashes`（`:61-77`）本来就是逐条读文件算
//! SHA-256。所以照上游来：算，而且**逐条**算 —— 一条坏路径只记 warning 跳过，
//! 不让整批去重失效。
//!
//! # 写入用**硬链接优先**
//!
//! 上游 `_copy_subtitle_file`（`:39-54`）：先 `os.link`，失败才 `copy2`。
//! 硬链接不复制字节 —— 字幕从媒体库链接过来是 O(1)，复制则可能是几百 MB。
//! `transfer_mode = "cleanup-source"` 时**直接复制**（调用方随后会删源文件；
//! 硬链接会让那次删除把刚写好的文件一起带走）。
//!
//! # 落盘是**先写 `.tmp` 再原子替换**
//!
//! 上游 `_write_atomic`（`:145-149`）。半截文件比没有文件更糟：读侧会把它当
//! 合法字幕返回给客户端。

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use sm_db::catalog::movie::Movie;
use sm_db::repo::{MovieRepository, NewSubtitle, SubtitleRepository};
use sm_db::Db;

use crate::catalog::media_paths::{
    self, allocate_next_movie_subtitle_path, ensure_movie_subtitle_path,
    is_movie_subtitle_extension, movie_subtitle_dir, subtitle_extension_of,
    MOVIE_SUBTITLE_EXTENSIONS,
};
use crate::error::ServiceError;
use crate::system::config::ConfigService;

/// 指纹分片的读块大小：1 MiB。上游 `_sha256_file` 的 `read(1024 * 1024)`。
const HASH_CHUNK_BYTES: usize = 1024 * 1024;

/// 目标路径的扩展名长度上限。上游 `_prepare_movie_subtitle_target_path`（`:33`）。
const MAX_EXTENSION_LEN: usize = 16;

/// 跳过原因码。上游 `register_subtitle_file` 的第二项（`:132`）。
pub const DUPLICATE_FINGERPRINT: &str = "duplicate_fingerprint";

/// 字幕导入结果状态。**每一种都是正常结果**，包括「已存在」。
///
/// 派生 `serde` 是因为它嵌在 [`SubtitleImportResult`] 里，而那个结构体会进
/// 插件返回值与任务摘要（JSON）。`rename_all = "snake_case"` 出来的字符串
/// **必须**与上游枚举一致，见模块文档那张表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleImportStatus {
    /// 新写入。上游 `IMPORTED`。
    Imported,
    /// ★ 已存在**同内容**的字幕 —— 不是失败。上游 `DUPLICATE`。
    Duplicate,
    /// 影片不存在。上游 `MOVIE_NOT_FOUND`。
    MovieNotFound,
    /// 扩展名不接受。上游 `INVALID_FORMAT`。
    InvalidFormat,
}

/// 字幕导入结果。上游 `SubtitleImportResult`（`schema/catalog/subtitles.py:29-35`）。
///
/// `reason` 是**给人看的**说明（上游中文文案照抄），只有失败分支才有值。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleImportResult {
    pub status: SubtitleImportStatus,
    /// 落盘后的字幕 id。只有 `Imported` 有值。
    pub subtitle_id: Option<i32>,
    /// 拒绝原因。`Imported` / `Duplicate` 时为 `None`。
    pub reason: Option<String>,
}

/// 本地文件登记的结果码。上游 `register_subtitle_file` 三元组的第一项。
///
/// 只有两个取值：`imported` 与 `skipped`（后者意味着同内容已存在）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleRegistrationStatus {
    Imported,
    Skipped,
}

/// 上游 `register_subtitle_file` 那个 `(status, reason, detail)` 三元组的形状。
///
/// 上游之所以返回元组而不是 [`SubtitleImportResult`]：目录导入是**批量**的，
/// 每个文件的落点（目标路径）与原因都要进日志，而 `SubtitleImportResult` 只有
/// 一个 `subtitle_id`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleRegistration {
    pub status: SubtitleRegistrationStatus,
    /// 跳过原因，目前只有 [`DUPLICATE_FINGERPRINT`]；导入成功时 `None`。
    pub reason: Option<String>,
    /// 跳过时是**源文件名**，导入时是**目标绝对路径**（上游 `:132` / `:142`）。
    pub detail: String,
}

/// 字幕资产服务（写入侧）。
///
/// # 为什么它有状态
///
/// 三个入口都要发 SQL（查影片、查已有字幕、登记），还要从**配置**读图片根
/// （字幕目录挂在它下面）。骨架期是 `pub struct SubtitleAssetService;` +
/// 关联函数 —— 拿不到 `Db` 与 `ConfigService`，于是三个方法都落不了地。
/// 与 [`crate::playback::media::MediaService`] 同一个取向。
pub struct SubtitleAssetService {
    db: Db,
    config: ConfigService,
}

impl SubtitleAssetService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
        }
    }

    /// 该影片**已有字幕的内容指纹集合**（sha256 十六进制，非小写化由 `hex` 保证）。
    ///
    /// 上游 `movie_subtitle_hashes`（`:61-77`）：逐条记录 → 校验路径 → 读文件算
    /// 指纹。写入前查一遍即可完成去重，**不必**每次重算（这也是目录导入要把它
    /// 塞进 `existing_hashes` 批次缓存的原因）。
    ///
    /// 用 `BTreeSet` 而不是 `Vec`：上游返回的是 `set[str]`，而调用方要做的是
    /// 「在不在里面」。排序顺带让结果可对拍。
    pub async fn movie_subtitle_hashes(
        &self,
        movie: &Movie,
    ) -> Result<BTreeSet<String>, ServiceError> {
        let rows = SubtitleRepository::new(self.db.clone())
            .list_by_movie(movie.id)
            .await?;
        let mut hashes = BTreeSet::new();
        for row in rows {
            let path = match ensure_movie_subtitle_path(
                &self.config,
                &movie.movie_number,
                Path::new(&row.file_path),
            ) {
                Ok(path) => path,
                Err(error) => {
                    // 上游 `:67-74`：记 warning 后 **continue**。一条坏路径不该让
                    // 「这部影片有哪些字幕」这件事整体失败 —— 那会让写侧再也去重
                    // 不了，每次都重复落盘。
                    // 用 `?error`（Debug）而不是 `%error`（Display）：
                    // `ServiceError` 没有实现 `Display`（它进 HTTP 时走的是
                    // `code()` / `message()`，见 `error.rs`）。
                    tracing::warn!(
                        movie_id = movie.id,
                        subtitle_id = row.id,
                        ?error,
                        "字幕路径非法，跳过指纹计算"
                    );
                    continue;
                }
            };
            // 记录在、文件不在：不算指纹（上游 `if absolute_path.is_file()`）。
            if path.is_file() {
                hashes.insert(sha256_file(&path)?);
            }
        }
        Ok(hashes)
    }

    /// ★ 从**内容**导入字幕（插件走这条）。
    ///
    /// 上游 `import_subtitle_content`（`:79-113`）。顺序照抄：
    /// 查影片 → 校验扩展名 → 算内容指纹 → 查重 → 落盘 → 登记记录。
    ///
    /// # 「不抛错」指的是**业务分支**
    ///
    /// 影片不存在、扩展名不支持、内容重复都走 [`SubtitleImportStatus`]，
    /// 不抛异常（插件把它当崩溃）。但**基础设施错误**（数据库连不上、磁盘满）
    /// 仍然返回 `Err` —— 那是宿主的问题，不该伪装成插件的问题。
    ///
    /// `language` 入参**不用**：上游 `del language`。保留是为了与上游签名一致，
    /// 免得实现侧以为「少了个参数」而自作主张加列。
    pub async fn import_subtitle_content(
        &self,
        movie_number: &str,
        content: &[u8],
        file_name: &str,
        language: Option<&str>,
    ) -> Result<SubtitleImportResult, ServiceError> {
        let _ = language; // 上游 `del language`。

        let Some(movie) = MovieRepository::new(self.db.clone())
            .find_by_number(movie_number)
            .await?
        else {
            return Ok(SubtitleImportResult {
                status: SubtitleImportStatus::MovieNotFound,
                subtitle_id: None,
                reason: Some(format!("影片不存在: {movie_number}")),
            });
        };

        let suffix = match subtitle_extension_of(file_name) {
            Some(suffix) if is_movie_subtitle_extension(&suffix) => suffix,
            other => {
                return Ok(SubtitleImportResult {
                    status: SubtitleImportStatus::InvalidFormat,
                    subtitle_id: None,
                    // 上游 `:100`：`不支持的扩展名: {suffix or '无'}（支持 .srt, ...）`
                    reason: Some(format!(
                        "不支持的扩展名: {}（支持 {}）",
                        other.as_deref().unwrap_or("无"),
                        MOVIE_SUBTITLE_EXTENSIONS.join(", ")
                    )),
                });
            }
        };

        let content_hash = sha256_bytes(content);
        if self
            .movie_subtitle_hashes(&movie)
            .await?
            .contains(&content_hash)
        {
            return Ok(SubtitleImportResult {
                status: SubtitleImportStatus::Duplicate,
                subtitle_id: None,
                reason: None,
            });
        }

        let target = self.prepare_movie_subtitle_target_path(&movie.movie_number, &suffix)?;
        write_atomic(&target, content)?;
        let row = SubtitleRepository::new(self.db.clone())
            .create(&NewSubtitle {
                movie_id: movie.id,
                file_path: target.to_string_lossy().into_owned(),
            })
            .await?;
        Ok(SubtitleImportResult {
            status: SubtitleImportStatus::Imported,
            subtitle_id: Some(row.id),
            reason: None,
        })
    }

    /// 从**文件**登记字幕（目录导入场景）。
    ///
    /// 上游 `register_subtitle_file`（`:115-142`），返回 `(status, reason, detail)`
    /// —— Rust 侧见 [`SubtitleRegistration`]。
    ///
    /// # `existing_hashes` 是**调用方**持有的批次缓存
    ///
    /// 形状对上游是 `dict[movie_id, set[hash]]`。一次目录导入要登记几十个字幕，
    /// 每个都去查一遍库、把该影片所有字幕文件读一遍是浪费 —— 缓存让每个影片只
    /// 算一次。**导入成功后要把新指纹并回缓存**（上游 `hashes.add(content_hash)`），
    /// 否则同一批里两个内容相同的文件都会被写进去。
    ///
    /// # `transfer_mode`
    ///
    /// * `"auto"`（默认）：先硬链接，失败才复制；
    /// * `"cleanup-source"`：**直接复制** —— 调用方随后会删源文件，硬链接会让
    ///   那次删除连带删掉新文件（同一个 inode）。
    pub async fn register_subtitle_file(
        &self,
        movie: &Movie,
        source_path: &Path,
        // `mut` 是为了 `as_deref_mut()`（要 `&mut Option<..>`）—— 改的是**缓存
        // 内容**，调用方拿到的还是同一个 `HashMap`。
        mut existing_hashes: Option<&mut HashMap<i32, BTreeSet<String>>>,
        transfer_mode: &str,
    ) -> Result<SubtitleRegistration, ServiceError> {
        let content_hash = sha256_file(source_path)?;
        let source_name = source_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        // 批次缓存里没有该影片的集合就先算一遍并回填（上游 `:126-130`）。
        //
        // 先判后改、分两次借用：`as_ref()` 拿到的借用会活到块末，与随后的
        // `as_deref_mut()` 冲突（而且中间还有一次 `await`，更不能跨着借用）。
        if existing_hashes
            .as_ref()
            .is_some_and(|cache| !cache.contains_key(&movie.id))
        {
            let computed = self.movie_subtitle_hashes(movie).await?;
            if let Some(cache) = existing_hashes.as_deref_mut() {
                cache.insert(movie.id, computed);
            }
        }
        let existing = match existing_hashes.as_ref() {
            Some(cache) => cache
                .get(&movie.id)
                .cloned()
                .expect("上面已确保该影片的集合存在"),
            None => self.movie_subtitle_hashes(movie).await?,
        };
        if existing.contains(&content_hash) {
            return Ok(SubtitleRegistration {
                status: SubtitleRegistrationStatus::Skipped,
                reason: Some(DUPLICATE_FINGERPRINT.to_owned()),
                detail: source_name,
            });
        }

        // 目标扩展名取**源文件**的（上游 `source_path.suffix.lower()`）。
        // 源文件没有扩展名时用默认的 `.srt`：上游会把空串交给 allocate，
        // 而空串不在白名单里于是一路抛到路由层 —— 这里退化成默认值更合理，
        // 且不会把「没有扩展名」误解成「不支持的字幕」。
        let extension = source_path
            .extension()
            .map(|extension| format!(".{}", extension.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_else(|| media_paths::MOVIE_SUBTITLE_EXTENSION.to_owned());

        let target = self.prepare_movie_subtitle_target_path(&movie.movie_number, &extension)?;
        copy_subtitle_file(source_path, &target, transfer_mode)?;
        SubtitleRepository::new(self.db.clone())
            .create(&NewSubtitle {
                movie_id: movie.id,
                file_path: target.to_string_lossy().into_owned(),
            })
            .await?;

        // 并入批次缓存（上游 `:141`）。这里是 `existing_hashes` 的最后一处使用，
        // 直接把它移进 `if let` —— `as_deref_mut()` 在这里是多余的（clippy 的
        // `needless_option_as_deref`）。
        if let Some(cache) = existing_hashes {
            if let Some(set) = cache.get_mut(&movie.id) {
                set.insert(content_hash);
            }
        }

        Ok(SubtitleRegistration {
            status: SubtitleRegistrationStatus::Imported,
            reason: None,
            detail: target.to_string_lossy().into_owned(),
        })
    }

    /// 上游 `_prepare_movie_subtitle_target_path`（`:31-36`）：先建目录，再分配
    /// 下一个 `<番号>-<N><扩展名>`。
    ///
    /// 两步都要 —— 白名单校验在 `allocate_*` 里，而**目录不存在**时那个函数扫不出
    /// 已有序号（返回 0），于是 `mkdir` 必须在分配**之前**（否则一批导入里第二个
    /// 文件会拿到 `-1` 再撞唯一索引）。
    fn prepare_movie_subtitle_target_path(
        &self,
        movie_number: &str,
        extension: &str,
    ) -> Result<PathBuf, ServiceError> {
        let normalized = extension.to_ascii_lowercase();
        if !normalized.starts_with('.') || normalized.len() > MAX_EXTENSION_LEN {
            return Err(ServiceError::validation(
                "invalid_subtitle_extension",
                format!("非法的字幕扩展名: {extension}"),
            ));
        }
        let dir = movie_subtitle_dir(&self.config, movie_number)?;
        std::fs::create_dir_all(&dir)
            .map_err(|error| subtitle_io_error(&dir, "建字幕目录", error))?;
        // 本批已预留名是**空集**：单次调用只看磁盘。批量导入要防撞名得由调用方
        // 逐个登记（登记完磁盘上就有了），上游同款。
        allocate_next_movie_subtitle_path(&self.config, movie_number, &BTreeSet::new(), &normalized)
    }
}

/// SHA-256（**小写十六进制**）。上游 `_sha256_bytes`。
fn sha256_bytes(content: &[u8]) -> String {
    hashing::sha256_hex(content)
}

/// 按块读文件算 SHA-256。上游 `_sha256_file`（`:151-157`，1 MiB 一块）。
///
/// **流式**：字幕最大 10 MiB（读侧的上限），但**写侧**没这个上限 —— 一个误传的
/// 视频文件也可能走到这里，读进内存就等于把整台机器的内存交给调用方。
fn sha256_file(path: &Path) -> Result<String, ServiceError> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| subtitle_io_error(path, "打开文件算指纹", error))?;
    let mut hasher = hashing::Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| subtitle_io_error(path, "读文件算指纹", error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hashing::hex(&hasher.finalize()))
}

/// 原子落盘：先写 `<目标>.tmp`，再 `rename` 覆盖。上游 `_write_atomic`（`:144-149`）。
///
/// `rename` 在同一文件系统内是原子的 —— 读侧要么看到旧文件、要么看到新文件，
/// 不会看到半截。写临时文件失败时删掉它（上游没删，但留着一个 `.tmp` 会让
/// 下一次的 `allocate` 扫到它，虽然它不匹配规范名、扫不进序号）。
fn write_atomic(target: &Path, content: &[u8]) -> Result<(), ServiceError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| subtitle_io_error(parent, "建字幕目录", error))?;
    }
    let mut tmp_name = target.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    std::fs::write(&tmp_path, content)
        .map_err(|error| subtitle_io_error(&tmp_path, "写临时字幕文件", error))?;
    if let Err(error) = std::fs::rename(&tmp_path, target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(subtitle_io_error(target, "替换字幕文件", error));
    }
    Ok(())
}

/// 把源字幕搬到目标位置：`auto` 先硬链接（O(1)），失败才复制。
///
/// 上游 `_copy_subtitle_file`（`:39-54`）。返回值刻意与上游一样是
/// `hardlink` / `copy`，方便日志对拍。
///
/// ⚠️ 与 `shutil.copy2` 的**已知差异**：`std::fs::copy` 会带上权限位，但**不保留
/// 时间戳**（`copy2` 会）。字幕的时间戳没有语义（读侧用的是 DB 的 `created_at`），
/// 所以不值得为它引第三方 crate。
fn copy_subtitle_file(
    source_path: &Path,
    target_path: &Path,
    transfer_mode: &str,
) -> Result<&'static str, ServiceError> {
    if transfer_mode == "cleanup-source" {
        std::fs::copy(source_path, target_path)
            .map_err(|error| subtitle_io_error(target_path, "复制字幕文件", error))?;
        return Ok("copy");
    }
    match std::fs::hard_link(source_path, target_path) {
        Ok(()) => Ok("hardlink"),
        // 跨盘、目标已存在、文件系统不支持硬链接 —— 都退回复制。
        Err(_) => {
            std::fs::copy(source_path, target_path)
                .map_err(|error| subtitle_io_error(target_path, "复制字幕文件", error))?;
            Ok("copy")
        }
    }
}

/// 文件系统错误 → 服务层错误。带上正在做什么，否则日志里只有一句 `os error 2`。
fn subtitle_io_error(path: &Path, action: &str, error: std::io::Error) -> ServiceError {
    ServiceError::from(sm_db::DbError::business(
        "SubtitleAsset",
        format!("{action}失败：{}（{error}）", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 扩展名白名单**没有 `.sub`**（上游只有四项），且大小写不敏感。
    ///
    /// 骨架期这里自己抄了一份五项的表 —— 于是 `.sub` 能过这一关、却卡在
    /// 分配目标路径的白名单上，「有时收有时拒」。
    #[test]
    fn only_upstream_subtitle_extensions_are_accepted() {
        assert!(is_movie_subtitle_extension(".srt"));
        assert!(is_movie_subtitle_extension(".ass"));
        assert!(!is_movie_subtitle_extension(".sub"), ".sub 不在上游表里");
        assert!(!is_movie_subtitle_extension(".mkv"));
        assert_eq!(subtitle_extension_of("A.SRT").as_deref(), Some(".srt"));
        assert_eq!(subtitle_extension_of("a.mkv").as_deref(), Some(".mkv"));
        assert_eq!(subtitle_extension_of("srt").as_deref(), None);
        assert_eq!(subtitle_extension_of(".srt").as_deref(), None);
    }

    /// ★「已存在同内容」是**正常结果**，不是失败；四个状态互不相同。
    ///
    /// 归成失败会让插件以为没导入成功而反复重试。
    #[test]
    fn an_existing_identical_subtitle_is_a_normal_outcome() {
        let statuses = [
            SubtitleImportStatus::Imported,
            SubtitleImportStatus::Duplicate,
            SubtitleImportStatus::MovieNotFound,
            SubtitleImportStatus::InvalidFormat,
        ];
        assert!(statuses.contains(&SubtitleImportStatus::Duplicate));
        assert_eq!(statuses.len(), 4, "别把 Duplicate 与失败合并");
        for (index, left) in statuses.iter().enumerate() {
            for right in &statuses[index + 1..] {
                assert_ne!(left, right, "状态不能重复");
            }
        }
    }

    /// ★ 对外字符串与上游 `SubtitleImportStatus` **逐字相同**。
    ///
    /// 这些串会进插件返回值与任务摘要；漂一个字母，客户端就分不清
    /// 「已存在同内容」与「落盘失败」。
    #[test]
    fn the_wire_status_strings_match_upstream() {
        let cases = [
            (SubtitleImportStatus::Imported, "imported"),
            (SubtitleImportStatus::Duplicate, "duplicate"),
            (SubtitleImportStatus::MovieNotFound, "movie_not_found"),
            (SubtitleImportStatus::InvalidFormat, "invalid_format"),
        ];
        for (status, expected) in cases {
            let json = serde_json::to_string(&status).expect("枚举应能序列化");
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    /// ★ 指纹是 **SHA-256**（不是 SHA-1），且与标准向量一致。
    ///
    /// 换成 SHA-1 的症状是「去重看起来在工作，但和上游算出的指纹对不上」——
    /// 迁移过来的库会一整批重复落盘。
    #[test]
    fn hashes_are_sha256() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_bytes("字幕".as_bytes()).len(), 64, "十六进制 64 位");
    }

    /// ★ 文件指纹是**按块流式**算的，与一次性算的结果一致。
    #[test]
    fn file_hashes_match_the_in_memory_result() {
        let dir =
            std::env::temp_dir().join(format!("sm-sub-hash-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        // 跨过 1 MiB 的块边界，确保分块累加没写错。
        let content = vec![b'x'; HASH_CHUNK_BYTES + 7];
        let path = dir.join("big.srt");
        std::fs::write(&path, &content).expect("写文件");

        assert_eq!(sha256_file(&path).expect("算指纹"), sha256_bytes(&content));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ★ 原子落盘：目标存在也要能覆盖，且**不留 `.tmp`**。
    ///
    /// 留着 `.tmp` 的后果是读侧把它当普通文件扫进列表（它不是规范名，进不了
    /// 序号扫描，但会出现在目录列表里）。
    #[test]
    fn atomic_write_replaces_and_leaves_no_temporary_file() {
        let dir =
            std::env::temp_dir().join(format!("sm-sub-atomic-{}", uuid::Uuid::new_v4().simple()));
        let target = dir.join("ABC-001-1.srt");
        write_atomic(&target, b"first").expect("首次落盘");
        assert_eq!(std::fs::read(&target).expect("读回"), b"first");

        write_atomic(&target, b"second").expect("覆盖落盘");
        assert_eq!(std::fs::read(&target).expect("读回"), b"second");

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("列目录")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件：{leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `cleanup-source` 模式**直接复制**（不能硬链接）。
    ///
    /// 硬链接的话，调用方随后删源文件会把新文件一起带走 —— 而且不报错，
    /// 只是字幕忽然没了。
    #[test]
    fn cleanup_source_mode_copies_instead_of_linking() {
        let dir =
            std::env::temp_dir().join(format!("sm-sub-copy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let source = dir.join("source.srt");
        let target = dir.join("target.srt");
        std::fs::write(&source, b"subtitle").expect("写源文件");

        assert_eq!(
            copy_subtitle_file(&source, &target, "cleanup-source").expect("复制"),
            "copy"
        );
        // 删掉源文件，目标必须还在（硬链接则会一起消失）。
        std::fs::remove_file(&source).expect("删源文件");
        assert_eq!(std::fs::read(&target).expect("目标仍在"), b"subtitle");
        std::fs::remove_dir_all(&dir).ok();
    }
}
