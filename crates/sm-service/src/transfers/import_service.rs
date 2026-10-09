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
//! | 值 | 含义 |
//! |---|---|
//! | `keep` | 暂存后**保留**源（默认） |
//! | `move` | 移动（转存语义） |
//!
//! 与 `in_place_import` 冲突：某些 provider **不支持原地导入**
//! （`422 in_place_import_unsupported`）—— 那时源与目标在同一存储，
//! 「移动」等于删掉自己。
//!
//! # 「不安全文件名」是一道真实的安全检查
//!
//! 上游 `502 provider_invalid_response` 里有一条是文件名不安全
//! （含 `..`、绝对路径、控制字符）。**不要**因为「provider 是可信插件」就
//! 跳过 —— 文件名最终会进宿主的路径拼接，一个 `../../etc/passwd` 就够。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 源文件处置方式。
pub mod source_disposition {
    /// 保留源文件（默认）。
    pub const KEEP: &str = "keep";
    /// 移动源文件（转存语义）。
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
}

/// 失败原因码。**这些值会进 `params` 并被 retry 端点读**，所以不能改名。
pub mod failure_reason {
    /// 番号识别不出来。
    pub const MOVIE_NUMBER_NOT_FOUND: &str = "movie_number_not_found";
    /// 元数据抓取失败。
    pub const METADATA_FETCH_FAILED: &str = "metadata_fetch_failed";
    /// 已是合集条目。
    pub const IS_COLLECTION: &str = "is_collection";
    /// 文件名不安全。
    pub const UNSAFE_FILENAME: &str = "unsafe_filename";
    /// provider 暂存失败。
    pub const STAGE_FAILED: &str = "stage_failed";
    /// 定稿失败。
    pub const FINALIZE_FAILED: &str = "finalize_failed";
}

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

/// 导入服务。
// 两个依赖尚未被方法体引用（导入编排还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct MediaImportService {
    provider: Option<Box<dyn StorageProvider>>,
    catalog_import: Option<Box<dyn CatalogImport>>,
}

/// 插件的存储能力。**形状待插件 ABI 定型**，这里只声明宿主用到的五个动作。
pub trait StorageProvider {
    /// 扫描可导入的条目。
    fn scan_import_source(
        &self,
        parent_ref: &serde_json::Value,
    ) -> Result<Vec<ScannedEntry>, ServiceError>;
    /// 暂存一个文件。
    fn stage_import_file(&self, source_ref: &serde_json::Value)
        -> Result<StagedFile, ServiceError>;
    /// 定稿（把暂存变成最终记录）。
    fn finalize(&self, staged: &StagedFile) -> Result<FinalizedFile, ServiceError>;
    /// 清理暂存。**失败路径必调。**
    fn abort(&self, staged: &StagedFile) -> Result<(), ServiceError>;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedFile {
    pub stage_token: String,
    pub source_ref: serde_json::Value,
    pub file_name: String,
    pub size_bytes: Option<i64>,
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
    pub fn new(provider: Box<dyn StorageProvider>, catalog_import: Box<dyn CatalogImport>) -> Self {
        Self {
            provider: Some(provider),
            catalog_import: Some(catalog_import),
        }
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
        let _ = (
            source_ref,
            library_id,
            media_kind,
            source_disposition,
            collection_id,
            &mut progress,
        );
        todo!("骨架：scan -> 逐条 stage -> 宿主写入 -> finalize；stage 之后必 abort 兜底")
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
        let _ = movie_numbers;
        todo!("骨架：并发按番号取元数据；逐条失败不中断（结果里带 failure_reason）")
    }

    /// 重试一条失败项。上游 `retry_failed_file`。
    ///
    /// 错误码：`422 invalid_retry_media_kind` / `422 invalid_retry_file` /
    /// `409 failed_item_source_unavailable`（源已被清理）。
    pub async fn retry_failed_file(
        &self,
        failure_item: &ImportFailure,
        candidate_id: &str,
        operation_key: &str,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = (failure_item, candidate_id, operation_key);
        todo!("骨架：复用已暂存的文件（staged=true 时跳过 scan）-> 换元数据候选 -> 重新写入")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「已是合集」是**跳过**而不是**失败**。
    ///
    /// 合集影片本来就不该单独导入，把它算作失败会让失败列表被合集条目淹没，
    /// 真正的失败项反而看不见。
    #[test]
    fn collection_entries_are_skipped_not_failed() {
        let result = ImportResult::default();
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 0);
        assert!(result.failed.is_empty());
        // 失败原因码是**独立**于跳过原因的 —— 别把它塞进 failed。
        assert_ne!(failure_reason::IS_COLLECTION, failure_reason::STAGE_FAILED);
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
        };
        assert!(failure.staged, "暂存成功过 -> 可跳过 scan");
        failure.staged = false;
        assert!(!failure.staged, "没暂存成功 -> 必须重新 scan");
    }

    /// 失败原因码是**稳定契约** —— 它们进 TaskRun `params` 并被 retry 读。
    #[test]
    fn the_failure_reason_codes_are_part_of_the_contract() {
        for code in [
            failure_reason::MOVIE_NUMBER_NOT_FOUND,
            failure_reason::METADATA_FETCH_FAILED,
            failure_reason::IS_COLLECTION,
            failure_reason::UNSAFE_FILENAME,
            failure_reason::STAGE_FAILED,
            failure_reason::FINALIZE_FAILED,
        ] {
            assert!(!code.is_empty());
        }
    }
}
