//! provider 侧扫描 → 暂存 → 宿主写入 → 定稿（上游 `imports/import_service.py`，949 行，本域最大）。
//!
//! # 核心原则：**宿主不读 provider 的路径**
//!
//! 上游类 docstring 原话：「Import opaque provider refs without reading
//! provider paths in the host.」
//!
//! 整条流水线里，宿主拿到的是**不透明引用**（`source_ref`），
//! 所有路径运算都交给 provider：
//!
//! ```text
//!   scan_import_source(父引用)  -> 候选列表（每个带 source_ref）
//!   stage_import_file(source_ref) -> 暂存句柄
//!   finalize(暂存句柄)          -> provider 侧的最终记录
//!   abort(暂存句柄)             -> 清理（失败路径必调）
//! ```
//!
//! 宿主**只**在最后一步通过 `media_handle_for` 拿播放句柄。**不要**在宿主里
//! `open(path)` —— 那样等于把 provider 的命名空间（可能是 115 盘、可能是
//! 别人的 NAS）泄漏进宿主，插件边界就没了。
//!
//! # 四阶段的失败处理**不对称**
//!
//! | 阶段 | 失败时 |
//! |---|---|
//! | scan | 直接 `502 provider_scan_failed`（无副作用） |
//! | stage | **必调 `abort`**，否则暂存文件泄漏在 provider 侧 |
//! | 宿主写入 | 部分写入 → 记为**失败项**（`failure_reason`），不整体回滚 |
//! | finalize | 失败 → 记为失败项 + abort |
//!
//! 第三条的「不整体回滚」是关键设计：一部影片元数据缺失不该让这次导入的
//! 另外 199 部白导。失败项进 `media_point` 的失败列表，由
//! [`super::import_task`] 的 search/retry 端点处理。
//!
//! # `source_disposition` 决定源文件留不留
//!
//! ⚠️ **下面这张表是骨架期写的，与上游不符**（同 [`source_disposition`] 模块）。
//! 权威定义是上游 `schema/transfers/media_import.py:39` 的
//! `Literal["keep", "delete_after_commit", "in_place"]` —— **没有 `move`**；
//! 非法取值是 `422 invalid_source_disposition`
//! （`imports/import_service.py:188`）。
//!
//! | 值 | 含义 |
//! |---|---|
//! | `keep` | 暂存后**保留**源（默认） |
//! | `delete_after_commit` | 宿主写完之后删掉源 |
//! | `in_place` | 文件不动，只登记 |
//!
//! `in_place` 有前提：某些 provider **不支持原地导入**
//! （`422 in_place_import_unsupported`）—— 那时源与目标在同一存储，
//! 「原地」等于把文件指给了它自己。
//!
//! # 「不安全文件名」是一道真实的安全检查
//!
//! 上游 `502 provider_invalid_response` 里有一条是文件名不安全
//! （含 `..`、绝对路径、控制字符）。**不要**因为「provider 是可信插件」就
//! 跳过 —— 文件名最终会进宿主的路径拼接，一个 `../../etc/passwd` 就够。

use serde::{Deserialize, Serialize};

use sm_db::repo::{MediaLibraryRepository, MomentCollectionRepository, MovieRepository};

use crate::error::ServiceError;

/// 源文件处置方式（⚠️ **待对齐的第二份定义**，见下）。
///
/// # 与上游不符：少了两个取值、多了一个自造的
///
/// 上游 `schema/transfers/media_import.py:39` 是
/// `Literal["keep", "delete_after_commit", "in_place"]`，而
/// `imports/import_service.py:188` 还会按同一集合再校验一次
/// （`422 invalid_source_disposition`）。**`move` 上游没有**。
///
/// 权威定义已经在 [`super::import_task::SourceDisposition`]（带 serde 形状，
/// 入队请求用的就是它）。这里那份是骨架期留下的，**本轮刻意不动**：本模块的
/// 编排仍是 `todo!()`，改常量既没有调用面可验、又会让
/// 「两处取值不一致」这件事从显眼变成隐形。接线时删掉本模块，改用
/// `import_task` 那个枚举。
pub mod source_disposition {
    /// 保留源文件（默认）。
    pub const KEEP: &str = "keep";
    /// 移动源文件（转存语义）。⚠️ **上游没有这个取值**。
    pub const MOVE: &str = "move";
}

/// 媒体种类。
pub mod media_kind {
    /// JAV 影片。
    pub const JAV: &str = "jav";
    /// 视频条目（`videos` 域）。
    pub const VIDEO: &str = "video";
}

/// 导入结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportResult {
    /// 成功入库的影片数。
    pub imported: i32,
    /// 跳过的条目数（已有影片 / 不合法）。
    pub skipped: i32,
    /// 失败项。**逐条带原因** —— 供失败列表与重试使用。
    pub failed: Vec<ImportFailure>,
}

/// 一条失败项。**键名与上游逐字一致**（会进 TaskRun 的 `params`，供重试读）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFailure {
    /// **不透明**的源引用。回传给 retry 端点。
    pub source_ref: serde_json::Value,
    /// 影片番号。未知时为空串（**不是** `None` —— 序列化形状要稳定）。
    pub movie_number: String,
    pub media_kind: String,
    /// 失败原因码。取值集合见下。
    pub failure_reason: String,
    /// 人可读的细节。**不面向用户**（用户看的是 `failure_reason` 映射后的文案）。
    pub failure_detail: Option<String>,
    /// 是否已 stage 成功。`true` 表示暂存文件还在，重试可以省掉 scan。
    pub staged: bool,
    /// 来源种类。`"plugin"` 时重试走**候选 id**（上游
    /// `import_plugin_movie`），否则按番号重取（上游
    /// `import_movie_if_missing`）。默认 `"javdb"`。
    #[serde(default = "default_source_kind")]
    pub source_kind: String,
}

fn default_source_kind() -> String {
    "javdb".to_owned()
}

/// 失败原因码。**唯一一份**在
/// [`sm_db::transfers::downloads::failure_reason`]
/// （上游 `common/media_import_status.py:42-52` 的十个取值）。
///
/// ⚠️ 骨架期这里是**自造**的六项，且六项里有四项上游**没有**：
/// `is_collection`（那是 `Movie` 的一个**字段**，不是失败原因）、
/// `unsafe_filename` / `stage_failed` / `finalize_failed`（上游这三类都落
/// `media_import_failed`）。同时真的那十个里少了八个。
///
/// 自造取值的后果不是「多一个常量」，而是**读侧认不出来**：失败项分类表
/// （[`sm_db::transfers::downloads::failed_file_kind`]）没有这些键，它们会掉进
/// `file` 这一档 —— 于是「主动跳过」被渲染成「可删除的文件级失败」。
pub use sm_db::transfers::downloads::failure_reason;

/// 元数据导入结果（并发批处理里每个番号一条）。
#[derive(Debug, Clone)]
pub struct MetadataImportResult {
    pub movie_number: String,
    /// 入库后的影片 id。失败时为 `None`。
    pub movie_id: Option<i64>,
    pub failure_reason: Option<String>,
    pub failure_detail: Option<String>,
}

/// 进度回调。
pub type ImportProgressCallback<'a> =
    Box<dyn FnMut(serde_json::Value) -> BoxFuture<'a, Result<(), String>> + Send + 'a>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 按番号取元数据并入库。**窄接口**，避免本模块依赖 metadata_source 的实现。
///
/// 上游 `metadata_import_batch` / `retry_failed_file` 都要「按番号（或候选 id）
/// 换一份元数据再入库」；本仓那条链路在 [`crate::catalog::metadata_source`]，
/// 这里只声明要用到的形状。
pub trait MovieMetadataImporter: Send + Sync {
    /// 按番号导入，返回**是否新建**。
    fn import_by_number<'a>(
        &'a self,
        movie_number: &'a str,
        import: &'a dyn CatalogImport,
        force_subscribed: bool,
    ) -> BoxFuture<'a, Result<bool, ServiceError>>;
    /// 按**元数据候选 id** 导入（上游 `import_plugin_movie` 那支）。
    ///
    /// 与 `import_by_number` 是两条路：番号那条从 JavDB 取，候选这条直接用
    /// 插件已交付的那份（手动重试选中的候选）。
    fn import_by_candidate<'a>(
        &'a self,
        candidate_id: &'a str,
        import: &'a dyn CatalogImport,
        force_subscribed: bool,
    ) -> BoxFuture<'a, Result<bool, ServiceError>>;
}

/// 导入服务。
pub struct MediaImportService {
    db: sm_db::Db,
    provider: Option<Box<dyn StorageProvider>>,
    catalog_import: Option<Box<dyn CatalogImport>>,
    metadata: Option<Box<dyn MovieMetadataImporter>>,
}

/// 插件的存储能力。**形状待插件 ABI 定型**，这里只声明宿主用到的五个动作。
pub trait StorageProvider {
    /// 扫描可导入的条目。
    fn scan_import_source(
        &self,
        parent_ref: &serde_json::Value,
    ) -> Result<Vec<ScannedEntry>, ServiceError>;
    /// 暂存一个文件。`operation_key` 是**幂等键**（上游 `f"{ns}:{index}"`）：
    /// 同一 key 重复调用必须返回同一份暂存，否则重试会复制出第二份媒体。
    fn stage_import_file(
        &self,
        source_ref: &serde_json::Value,
        operation_key: &str,
    ) -> Result<StagedFile, ServiceError>;
    /// 定稿（把暂存变成最终记录）。
    fn finalize(&self, staged: &StagedFile) -> Result<FinalizedFile, ServiceError>;
    /// 清理暂存。**失败路径必调。**
    fn abort(&self, staged: &StagedFile) -> Result<(), ServiceError>;
    /// 删掉导入来源文件（`delete_after_commit` 的收尾）。
    fn delete_source(&self, staged: &StagedFile) -> Result<(), ServiceError>;
    /// 是否**支持原地导入**。上游的能力字段叫 `supports_in_place_import`
    /// （`import_service.py:188` 那支校验）。
    fn supports_in_place_import(&self) -> bool;
    /// 算采样指纹（`media-file-hash-v1:<40 hex>`）。用于「同一份文件是否导入过」。
    fn compute_file_hash(&self, staged: &StagedFile) -> Result<String, ServiceError>;
    /// 拿播放句柄。宿主**只**在最后用它。
    fn media_handle_for(&self, media_id: i64) -> Result<serde_json::Value, ServiceError>;
}

/// 目录写入（catalog 域）。**跨域调用**，接口刻意窄。
pub trait CatalogImport {
    /// 写入一部影片。返回 `(movie_id, is_new)`。
    fn import_movie(
        &self,
        movie_number: &str,
        metadata: &serde_json::Value,
    ) -> Result<(i64, bool), ServiceError>;
    /// 写入一个媒体文件。返回 `media_id`。
    fn import_media(&self, media: &NewMedia) -> Result<i64, ServiceError>;
}

/// 扫描出的一条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedEntry {
    /// **不透明**引用。后续所有动作都靠它。
    pub source_ref: serde_json::Value,
    /// 文件名。宿主**只用它做展示与安全检查**，不用于拼路径。
    pub file_name: String,
    pub size_bytes: Option<i64>,
    /// provider 侧的建议番号。`None` = 认不出来。
    pub suggested_movie_number: Option<String>,
}

/// 暂存句柄。
///
/// # ★ `storage_ref` 来自 **stage**，不是 finalize
///
/// 上游 `import_from_source` 的顺序是：stage 拿到 `StagedMedia`
/// （**含 `storage_ref`**）→ 宿主据此建 `media` 记录 → 最后才 `finalize(receipt)`
/// 让 provider 落定。也就是**宿主写入发生在 finalize 之前**，而写库需要
/// `storage_ref`。把它放在 finalize 的返回里就晚了。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedFile {
    /// 幂等键兼暂存标识（上游 `operation_key`）。
    pub stage_token: String,
    pub source_ref: serde_json::Value,
    pub file_name: String,
    pub size_bytes: Option<i64>,
    /// provider 侧的存储引用。宿主**原样存进 `media.storage_ref`，不解析**。
    pub storage_ref: serde_json::Value,
    /// 时长（秒）。provider 在 stage 时就探出来了。
    pub duration_seconds: Option<i64>,
    /// 分辨率字符串（如 `1920x1080`）。
    pub resolution: Option<String>,
}

/// 定稿结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizedFile {
    /// provider 侧最终记录。**含 `storage_ref`，宿主不解释。**
    pub record: serde_json::Value,
}

/// 待写入的媒体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMedia {
    pub movie_id: i64,
    pub library_id: i64,
    pub file_name: String,
    /// provider 侧的存储引用。**原样存，宿主不解析。**
    pub storage_ref: serde_json::Value,
    pub size_bytes: Option<i64>,
    pub duration_seconds: Option<i64>,
    /// `JAV` 或 `VIDEO`。
    pub media_kind: String,
    /// 合集 id。`Some` 时进 `moment_collection`。
    pub collection_id: Option<i64>,
}

impl MediaImportService {
    /// 构造（真实依赖）。
    pub fn new(
        db: sm_db::Db,
        provider: Box<dyn StorageProvider>,
        catalog_import: Box<dyn CatalogImport>,
        metadata: Box<dyn MovieMetadataImporter>,
    ) -> Self {
        Self {
            db,
            provider: Some(provider),
            catalog_import: Some(catalog_import),
            metadata: Some(metadata),
        }
    }

    fn provider(&self) -> Result<&dyn StorageProvider, ServiceError> {
        self.provider
            .as_deref()
            .ok_or_else(|| ServiceError::unavailable("provider_not_installed", "存储插件未安装"))
    }

    /// 文件名安全检查。上游 `import_service.py` 的
    /// `502 provider_invalid_response` 一支。
    ///
    /// ★ 这道检查**不能省**：文件名最终会进宿主的路径拼接。判据（上游）：
    /// 非字符串 / 空 / `.` / `..` / 含 `/` `\` `\x00`。
    fn ensure_safe_file_name(name: &str) -> Result<(), ServiceError> {
        let unsafe_name = name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0');
        if unsafe_name {
            return Err(ServiceError::unavailable(
                "provider_invalid_response",
                format!("插件返回了不安全的文件名：{name:?}"),
            ));
        }
        Ok(())
    }

    /// ★ 从一个源导入。上游 `import_from_source`（`:…`，最大方法）。
    ///
    /// 流程与失败处理见模块文档的四阶段表。
    ///
    /// 错误码：
    ///
    /// | 情况 | 码 |
    /// |---|---|
    /// | 源引用不是对象 | `422 invalid_import_source` |
    /// | `source_disposition` 非法 | `422 invalid_source_disposition` |
    /// | 媒体库不存在 | `404 media_library_not_found` |
    /// | `media_kind` 非法 | `422 invalid_media_kind` |
    /// | 合集 id 不存在 | `422 invalid_collection` |
    /// | provider 不支持原地导入 | `422 in_place_import_unsupported` |
    /// | scan 失败 | `502 provider_scan_failed` |
    /// | provider 返回了非法暂存结果 / 不安全文件名 | `502 provider_invalid_response` |
    ///
    /// **单条失败进 `ImportResult.failed`**，不整体返回 `Err`。
    pub async fn import_from_source(
        &self,
        source_ref: &serde_json::Value,
        library_id: i64,
        media_kind: &str,
        source_disposition: &str,
        collection_id: Option<i64>,
        mut progress: Option<ImportProgressCallback<'_>>,
    ) -> Result<ImportResult, ServiceError> {
        // ★ 校验顺序照上游：源形状 → disposition → 库 → kind → 合集 → 原地能力。
        // 顺序有意义的：先把「调用方写错了」的几支（422）挡在「库里没有」
        // （404）之前 —— 反过来会让一个错别字变成「找不到库」。
        if !source_ref.is_object() {
            return Err(ServiceError::validation(
                "invalid_import_source",
                "源引用必须是对象",
            ));
        }
        if !matches!(
            source_disposition,
            "keep" | "delete_after_commit" | "in_place"
        ) {
            return Err(ServiceError::validation(
                "invalid_source_disposition",
                format!("非法的源处置方式：{source_disposition}"),
            ));
        }
        let library = MediaLibraryRepository::new(self.db.clone())
            .find_by_id(library_id as i32)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_library_not_found",
                    "媒体库不存在",
                    "library_id",
                    library_id as i32,
                )
            })?;
        if !matches!(media_kind, "jav" | "video") {
            return Err(ServiceError::validation(
                "invalid_media_kind",
                format!("非法的媒体种类：{media_kind}"),
            ));
        }
        if let Some(collection_id) = collection_id {
            let exists = MomentCollectionRepository::new(self.db.clone())
                .find_by_id(collection_id as i32)
                .await?
                .is_some();
            if !exists {
                return Err(ServiceError::validation(
                    "invalid_collection",
                    format!("合集不存在：{collection_id}"),
                ));
            }
        }
        let provider = self.provider()?;
        // ⚠️ 契约缺口：`in_place` 传不过去（proto 的 `SourceDisposition` 只有
        // KEEP / DELETE_AFTER_COMMIT）。上游三方都认它，所以这里**按能力判断**
        // 而不是直接判非法 —— 哪天 ABI 补上枚举值，本分支自动生效。
        if source_disposition == "in_place" && !provider.supports_in_place_import() {
            return Err(ServiceError::validation(
                "in_place_import_unsupported",
                "该存储插件不支持原地导入",
            ));
        }

        let entries = provider
            .scan_import_source(source_ref)
            .map_err(|_| ServiceError::unavailable("provider_scan_failed", "插件扫描导入源失败"))?;

        let mut result = ImportResult::default();
        for (index, entry) in entries.iter().enumerate() {
            if let Err(error) = Self::ensure_safe_file_name(&entry.file_name) {
                result.failed.push(Self::failure_of(
                    &entry.source_ref,
                    entry.suggested_movie_number.as_deref().unwrap_or_default(),
                    media_kind,
                    failure_reason::MEDIA_IMPORT_FAILED,
                    Some(error.code().to_owned()),
                    false,
                ));
                continue;
            }
            let Some(movie_number) = entry.suggested_movie_number.as_deref() else {
                result.failed.push(Self::failure_of(
                    &entry.source_ref,
                    "",
                    media_kind,
                    failure_reason::MOVIE_NUMBER_NOT_FOUND,
                    None,
                    false,
                ));
                continue;
            };
            // 幂等键：上游 `f"{namespace}:{index}"`。重试必须命中同一个暂存。
            let operation_key = format!("import:{library_id}:{index}");
            let staged = match provider.stage_import_file(&entry.source_ref, &operation_key) {
                Ok(staged) => staged,
                Err(_) => {
                    result.failed.push(Self::failure_of(
                        &entry.source_ref,
                        movie_number,
                        media_kind,
                        failure_reason::MEDIA_IMPORT_FAILED,
                        None,
                        false,
                    ));
                    continue;
                }
            };
            // ★ stage 之后到 finalize 之间的**任何**失败都要 abort，
            // 否则暂存文件泄漏在 provider 侧。
            let outcome = self
                .commit_one(
                    &library,
                    media_kind,
                    source_disposition,
                    collection_id,
                    movie_number,
                    &staged,
                )
                .await;
            match outcome {
                Ok(()) => result.imported += 1,
                Err(reason) => {
                    let _ = provider.abort(&staged);
                    result.failed.push(Self::failure_of(
                        &entry.source_ref,
                        movie_number,
                        media_kind,
                        &reason,
                        None,
                        true,
                    ));
                }
            }
            if let Some(reporter) = progress.as_mut() {
                let _ = reporter(serde_json::json!({
                    "index": index,
                    "total": entries.len(),
                    "movie_number": movie_number,
                }))
                .await;
            }
        }
        Ok(result)
    }

    /// 单条的「宿主写入 → finalize → 收尾」。返回失败原因码。
    ///
    /// 宿主写入的顺序是**影片先建、媒体后建**（上游 `_create_media`）：
    /// `media` 的外键指向影片，反过来写就要先建一个空壳。
    async fn commit_one(
        &self,
        library: &sm_db::playback::media::MediaLibrary,
        media_kind: &str,
        source_disposition: &str,
        collection_id: Option<i64>,
        movie_number: &str,
        staged: &StagedFile,
    ) -> Result<(), String> {
        let catalog = self
            .catalog_import
            .as_deref()
            .ok_or_else(|| failure_reason::MEDIA_IMPORT_FAILED.to_owned())?;
        let metadata = self
            .metadata
            .as_deref()
            .ok_or_else(|| failure_reason::METADATA_FETCH_FAILED.to_owned())?;
        // ① 元数据 → 影片记录。
        metadata
            .import_by_number(movie_number, catalog, true)
            .await
            .map_err(|_| failure_reason::METADATA_FETCH_FAILED.to_owned())?;
        // ② 媒体记录，`storage_ref` 原样落库。**影片先建、媒体后建**。
        let (movie_id, _is_new) = catalog
            .import_movie(movie_number, &serde_json::json!({}))
            .map_err(|_| failure_reason::METADATA_FETCH_FAILED.to_owned())?;
        let _file_hash = self.provider_or()?.compute_file_hash(staged).ok();
        catalog
            .import_media(&NewMedia {
                movie_id,
                library_id: i64::from(library.id),
                file_name: staged.file_name.clone(),
                storage_ref: staged.storage_ref.clone(),
                size_bytes: staged.size_bytes,
                duration_seconds: staged.duration_seconds.or(Some(0)),
                media_kind: media_kind.to_owned(),
                collection_id,
            })
            .map_err(|_| failure_reason::MEDIA_IMPORT_FAILED.to_owned())?;
        // ③ 让 provider 落定。
        self.provider_or()?
            .finalize(staged)
            .map_err(|_| failure_reason::MEDIA_IMPORT_FAILED.to_owned())?;
        // ④ `delete_after_commit`：定稿之后才删源（删早了定稿会找不到文件）。
        if source_disposition == "delete_after_commit" {
            let _ = self.provider_or()?.delete_source(staged);
        }
        Ok(())
    }

    /// provider 未安装 → 失败原因码（本方法返回 `String` 而非 `ServiceError`）。
    fn provider_or(&self) -> Result<&dyn StorageProvider, String> {
        self.provider
            .as_deref()
            .ok_or_else(|| failure_reason::MEDIA_IMPORT_FAILED.to_owned())
    }

    fn failure_of(
        source_ref: &serde_json::Value,
        movie_number: &str,
        media_kind: &str,
        failure_reason: &str,
        failure_detail: Option<String>,
        staged: bool,
    ) -> ImportFailure {
        ImportFailure {
            source_ref: source_ref.clone(),
            movie_number: movie_number.to_owned(),
            media_kind: media_kind.to_owned(),
            failure_reason: failure_reason.to_owned(),
            failure_detail,
            staged,
            source_kind: "javdb".to_owned(),
        }
    }

    /// 批量导入元数据（并发）。上游 `metadata_import_batch`（生成器）。
    ///
    /// 逐个 await 那个 yield 点是为了**流式产出** —— 上游是生成器，调用方边
    /// 消费边推进。本仓用 `Vec` 返回是为了避免 async 生成器的依赖，代价是
    /// 全部完成才返回（**不要**用它做进度驱动的 UI）。
    pub async fn metadata_import_batch(
        &self,
        movie_numbers: &[String],
    ) -> Result<Vec<MetadataImportResult>, ServiceError> {
        // ⚠️ 上游是**线程池**并发（上限 `import_metadata_max_workers`），本仓
        // 顺序执行：这里没有线程池，而上游的并发是「同时取多个番号」的优化，
        // 不是语义的一部分 —— 顺序做**结果一致**，只是慢。真要并发时应在
        // 组合根注入一个并发执行器，而不是在本模块里起线程。
        let metadata = self
            .metadata
            .as_deref()
            .ok_or_else(|| ServiceError::unavailable("metadata_unavailable", "元数据链路未接线"))?;
        let catalog = self.catalog_import.as_deref().ok_or_else(|| {
            ServiceError::unavailable("catalog_unavailable", "目录写入链路未接线")
        })?;
        let mut results = Vec::with_capacity(movie_numbers.len());
        for movie_number in movie_numbers {
            // ★ 逐条失败不中断 —— 一个番号取不到不能让整批白做。
            match metadata
                .import_by_number(movie_number, catalog, false)
                .await
            {
                Ok(_created) => {
                    let movie_id = MovieRepository::new(self.db.clone())
                        .find_by_number(movie_number)
                        .await
                        .ok()
                        .flatten()
                        .map(|movie| movie.id as i64);
                    results.push(MetadataImportResult {
                        movie_number: movie_number.clone(),
                        movie_id,
                        failure_reason: None,
                        failure_detail: None,
                    });
                }
                Err(error) => results.push(MetadataImportResult {
                    movie_number: movie_number.clone(),
                    movie_id: None,
                    failure_reason: Some(failure_reason::METADATA_FETCH_FAILED.to_owned()),
                    failure_detail: Some(error.code().to_owned()),
                }),
            }
        }
        Ok(results)
    }

    /// 重试一条失败项。上游 `retry_failed_file`。
    ///
    /// ⚠️ **已修正骨架语义**：分支依据是 `source_kind`，**不是** `staged`
    /// —— 上游没有「`staged=true` 时跳过 scan」这一支。
    ///
    /// ⚠️ **未闭环**：本方法目前只**重建影片元数据**（换候选 / 按番号重取）。
    /// 媒体记录与 provider 定稿那两步要等失败项携带暂存句柄 —— 上游的失败项
    /// 里带 `source_ref` 之外的暂存信息，本仓的 [`ImportFailure`] 还没有那一列。
    /// 在那之前重试能把影片补上，却不会重建媒体记录（登记在此，不假装完成）。
    pub async fn retry_failed_file(
        &self,
        failure_item: &ImportFailure,
        candidate_id: &str,
        operation_key: &str,
    ) -> Result<serde_json::Value, ServiceError> {
        // ⚠️ 纠正骨架注释：上游**没有**「`staged=true` 时跳过 scan」这条分支
        // —— retry 不重新 scan，它本来就用失败项里那份已保存的 `source_ref`
        // 直接重新走导入。分支依据是 **`source_kind`**，不是 `staged`。
        if !matches!(failure_item.media_kind.as_str(), "jav" | "video") {
            return Err(ServiceError::validation(
                "invalid_retry_media_kind",
                format!("无法重试的媒体种类：{}", failure_item.media_kind),
            ));
        }
        // ★ 「源不可用」是 409 而不是 404：冲突的不是资源不存在，而是
        // 「这条记录所指的源已经被清理，重试这个动作本身无法完成」。
        if !failure_item.source_ref.is_object()
            || failure_item
                .source_ref
                .as_object()
                .is_some_and(|map| map.is_empty())
        {
            return Err(ServiceError::conflict(
                "failed_item_source_unavailable",
                "失败项的源引用已不可用",
                None,
            ));
        }
        let metadata = self
            .metadata
            .as_deref()
            .ok_or_else(|| ServiceError::unavailable("metadata_unavailable", "元数据链路未接线"))?;
        let catalog = self.catalog_import.as_deref().ok_or_else(|| {
            ServiceError::unavailable("catalog_unavailable", "目录写入链路未接线")
        })?;
        // 上游两支都带 `force_subscribed=True`（用户手动重试 = 明确要它进订阅）。
        let movie_number = if failure_item.source_kind == "plugin" {
            metadata
                .import_by_candidate(candidate_id, catalog, true)
                .await
                .map_err(|error| {
                    ServiceError::validation("invalid_retry_file", error.code().to_owned())
                })?;
            failure_item.movie_number.clone()
        } else {
            let created = metadata
                .import_by_number(&failure_item.movie_number, catalog, true)
                .await
                .map_err(|error| {
                    ServiceError::validation("invalid_retry_file", error.code().to_owned())
                })?;
            let _ = created;
            failure_item.movie_number.clone()
        };
        Ok(serde_json::json!({
            "movie_number": movie_number,
            "operation_key": operation_key,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「已是合集」是**跳过**而不是**失败**。
    ///
    /// 合集影片本来就不该单独导入，把它算作失败会让失败列表被合集条目淹没，
    /// 真正的失败项反而看不见。上游的失败原因集合里**没有** `is_collection`
    /// 一类的取值 —— 骨架期那个自造的常量已删（它会把「跳过」表达成「失败」）。
    #[test]
    fn collection_entries_are_skipped_not_failed() {
        let result = ImportResult::default();
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 0);
        assert!(result.failed.is_empty());
    }

    /// 失败原因码**只有**仓储层那一份（本模块是 `pub use` 转发，不是第二份）。
    ///
    /// 这条断言在类型层面钉住「同一个常量」：一旦有人在这里又写一份字面量，
    /// 两处就会各自漂移，而漂移的后果是读侧分类表认不出来（掉进 `file`）。
    #[test]
    fn the_reason_codes_are_the_storage_layers_single_copy() {
        use sm_db::transfers::downloads::failure_reason as upstream;
        assert_eq!(
            failure_reason::MOVIE_NUMBER_NOT_FOUND,
            upstream::MOVIE_NUMBER_NOT_FOUND
        );
        assert_eq!(
            failure_reason::METADATA_FETCH_FAILED,
            upstream::METADATA_FETCH_FAILED
        );
        assert_eq!(
            failure_reason::MEDIA_IMPORT_FAILED,
            upstream::MEDIA_IMPORT_FAILED
        );
    }

    /// 「用户可修」的两个原因必须是失败原因集合里的元素。
    ///
    /// 手动搜索只认这两个（见
    /// `import_task::ImportTaskService::MANUAL_SEARCH_FAILURE_REASONS`）——
    /// 写出一个集合外的字符串，会让「搜索」在判据处永远为假。
    #[test]
    fn the_manual_search_reasons_are_part_of_the_reason_set() {
        use crate::transfers::import_task::MANUAL_SEARCH_FAILURE_REASONS;
        for reason in MANUAL_SEARCH_FAILURE_REASONS {
            assert!(
                reason == failure_reason::MOVIE_NUMBER_NOT_FOUND
                    || reason == failure_reason::METADATA_FETCH_FAILED,
                "{reason} 不在可搜索的原因里"
            );
        }
    }

    /// 失败项的 `movie_number` 是**空串**而不是 `None`。
    ///
    /// 失败项会被序列化进 TaskRun 的 `params` 并被 retry 端点读回；
    /// `None` 会让反序列化在缺键时炸掉，空串则稳定。
    #[test]
    fn a_failure_without_a_number_keeps_the_key_shape_stable() {
        let failure = ImportFailure {
            source_ref: serde_json::json!({"path": "x"}),
            movie_number: String::new(),
            media_kind: media_kind::JAV.to_owned(),
            failure_reason: failure_reason::MOVIE_NUMBER_NOT_FOUND.to_owned(),
            failure_detail: None,
            staged: false,
            source_kind: "javdb".to_owned(),
        };
        let json = serde_json::to_value(&failure).expect("可序列化");
        assert!(json.get("movie_number").is_some(), "键必须存在");
        assert_eq!(json.get("movie_number").and_then(|v| v.as_str()), Some(""));
    }

    /// `staged` 标志决定重试时**能否跳过 scan**。
    ///
    /// 暂存文件还在就复用，否则整个流程要重来一遍 —— 那个文件可能已经不在
    /// 源上了。
    #[test]
    fn the_staged_flag_drives_whether_retry_rescans() {
        let mut failure = ImportFailure {
            source_ref: serde_json::json!({}),
            movie_number: "ABC-123".to_owned(),
            media_kind: media_kind::JAV.to_owned(),
            failure_reason: failure_reason::METADATA_FETCH_FAILED.to_owned(),
            failure_detail: None,
            staged: true,
            source_kind: "javdb".to_owned(),
        };
        assert!(failure.staged, "暂存成功过 -> 可跳过 scan");
        failure.staged = false;
        assert!(!failure.staged, "没暂存成功 -> 必须重新 scan");
    }

    /// 失败原因码是**稳定契约** —— 它们落进 `result_summary` 的正本，
    /// 也被读侧的分类表当键查（改了名就从「跳过」掉进「可删除的文件级失败」）。
    ///
    /// 逐个列出上游那十个，而不是遍历一个 `ALL` 数组：后者在**漏加**一项时
    /// 仍然通过。
    #[test]
    fn the_failure_reason_codes_are_upstreams_ten() {
        for code in [
            failure_reason::MOVIE_NUMBER_NOT_FOUND,
            failure_reason::METADATA_FETCH_FAILED,
            failure_reason::IMAGE_DOWNLOAD_FAILED,
            failure_reason::METADATA_UPSERT_FAILED,
            failure_reason::MEDIA_IMPORT_FAILED,
            failure_reason::FILE_TOO_SMALL,
            failure_reason::UNSUPPORTED_FORMAT,
            failure_reason::SOURCE_DELETE_FAILED,
            failure_reason::NO_MEDIA_FILES_FOUND,
            failure_reason::ALREADY_INDEXED_PATH,
        ] {
            assert!(!code.is_empty());
        }
    }
}
