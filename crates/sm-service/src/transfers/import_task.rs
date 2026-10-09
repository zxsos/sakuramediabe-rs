//! 导入的 TaskRun 边界（上游 `shared/import_task_service.py`，713 行，本域第二大）。
//!
//! # 这个类是**唯一**的导入入队路径
//!
//! HTTP 端点**只**做两件事：入队、返回 202。真正的导入跑在 worker 里，
//! 由 `execute(reporter, params)` 执行。
//!
//! 这么切的理由是导入可能跑几十分钟（几千个文件）。若在 HTTP 处理器里同步做，
//! 连接会占满、客户端断开后前功尽弃、且无法重试。
//!
//! # `execute` 按 `params["mode"]` 分**三种**执行模式
//!
//! | `mode` | 触发来源 | 做什么 |
//! |---|---|---|
//! | 缺省 | `POST /imports` | 跑一次 `import_from_source` |
//! | `download_tasks` | `download_task_auto_import` | 批量导入一批下载任务 |
//! | `retry_failed_item` | 失败项重试端点 | 重试一条失败项 |
//!
//! **模式来自 `params`，不是来自注册的多个 handler** —— 一个 `task_key` 对应
//! 一个 TaskRun，模式在参数里。这样任务中心里它们是同一类任务，
//! 而分成三个 `task_key` 会让「这次导入」在界面上裂成三条。
//!
//! # 互斥键：按**媒体库**，且是 `409` 不是排队
//!
//! 同一媒体库已有导入在跑 → **409 `import_task_conflict`**。不排队，
//! 因为用户此时多半是想「再点一次看看」，排队会让第二次点击无声无息。
//!
//! 互斥键的形状见 [`super::import_write_mutex`]。
//!
//! # 手动搜索的两种失败**要区别对待**
//!
//! [`ImportTaskService::MANUAL_SEARCH_FAILURE_REASONS`] 只含两个码：
//! `movie_number_not_found` 与 `metadata_fetch_failed`。它们是**用户能自己
//! 解决**的（换个番号、重试），所以手动触发的搜索失败**不消耗**订阅的重试
//! 预算 —— 见 `catalog::movie_subscription_search_state`。

use serde::{Deserialize, Serialize};

// 只被 `#[cfg(test)]` 里的用例用到 —— 不打 `cfg` 会在 lib 构建时报
// unused import。
#[cfg(test)]
use super::import_service::ImportFailure;
use crate::error::ServiceError;

/// 任务键。与 `cron_spec` 与 `sm_scheduler::lane_of` 里的 `library_import`
/// **必须一致** —— 它同时决定了**专属道**（`import` 道，2 并发）。
pub const TASK_KEY: &str = "library_import";

/// 手动搜索时**不算用户失误**的失败原因。
///
/// 只有这两个。其它失败原因（provider 挂了、磁盘满了）都属于服务端问题，
/// 该走重试与告警。
pub const MANUAL_SEARCH_FAILURE_REASONS: [&str; 2] =
    ["movie_number_not_found", "metadata_fetch_failed"];

/// 入队请求。
#[derive(Debug, Clone, Deserialize)]
pub struct ImportRequest {
    pub library_id: i64,
    /// 不透明源引用。
    pub source_ref: serde_json::Value,
    /// `JAV` / `VIDEO`。
    pub media_kind: String,
    /// `keep` / `move`。
    pub source_disposition: String,
    pub collection_id: Option<i64>,
    /// 操作命名空间。**用于互斥与去重**，同一次批量操作共用一个。
    pub operation_namespace: Option<String>,
}

/// 已受理的导入（**202**）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportAcceptedResponse {
    /// TaskRun id。**轮询它看进度**。
    pub task_run_id: i64,
}

/// 失败项（响应体）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportFailedItemResource {
    /// **字符串**业务标识，**不是**自增主键（可能是带前缀/含编码的路径）。
    pub item_id: String,
    pub movie_number: String,
    pub failure_reason: String,
    pub failure_detail: Option<String>,
    /// 已尝试次数。`>= 3` 意味着反复失败，UI 应提示换番号。
    pub attempts: i32,
}

/// 元数据搜索结果。
#[derive(Debug, Clone, Serialize)]
pub struct ImportMetadataSearchResponse {
    pub item_id: String,
    pub movie_number: String,
    /// 候选，**按置信度降序**（上游保证）。
    pub candidates: Vec<MetadataCandidate>,
}

/// 一条候选。
#[derive(Debug, Clone, Serialize)]
pub struct MetadataCandidate {
    /// 候选 id。**回传给 retry 端点**（`candidate_id`）。
    pub candidate_id: String,
    pub title: String,
    pub date: Option<String>,
    /// 置信度 [0, 1]。
    pub confidence: f32,
}

/// 执行结果摘要。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ImportExecuteSummary {
    pub imported: i32,
    pub skipped: i32,
    pub failed: i32,
}

/// 导入服务。
pub struct ImportTaskService;

impl ImportTaskService {
    /// 手动搜索的可重试原因（见模块文档）。
    pub const MANUAL_SEARCH_FAILURE_REASONS: [&'static str; 2] = MANUAL_SEARCH_FAILURE_REASONS;

    /// 入队一次导入。**202**。
    ///
    /// 错误码：
    ///
    /// | 情况 | 码 |
    /// |---|---|
    /// | 该库已有导入在跑 | `409 import_task_conflict` |
    /// | 媒体库不存在 | `404 media_library_not_found` |
    /// | 媒体库没有 provider 配置 | `422 invalid_media_library_provider` |
    /// | 建 TaskRun 失败 | `502 import_task_create_failed` |
    ///
    /// **409 不排队**（见模块文档）。
    pub async fn enqueue(
        request: ImportRequest,
        trigger_type: &str,
        download_task_id: Option<i64>,
    ) -> Result<ImportAcceptedResponse, ServiceError> {
        let _ = (request, trigger_type, download_task_id);
        todo!("骨架：按库取写互斥键 -> 已有在跑则 409 -> 建 TaskRun(三种 mode 之一)")
    }

    /// 批量入队（一批下载任务的自动导入）。
    ///
    /// 上游有一条硬约束：**列表不能为空**，且**必须同属一个媒体库** ——
    /// 违反抛 `ValueError`（不是 `ApiError`），因为那是**调用方**的 bug
    /// （worker 代码写错了），不是用户请求的问题。
    pub async fn enqueue_batch(download_task_ids: &[i64]) -> Result<(), ServiceError> {
        let _ = download_task_ids;
        todo!(
            "骨架：空列表 -> 编程错误；跨库 -> 编程错误；否则按 mode=download_tasks 建一个 TaskRun"
        )
    }

    /// `GET /imports/{task_run_id}/failed-items`
    ///
    /// TaskRun 不存在 → **404**（区别于「存在但没有失败项」= 200 空列表）。
    pub async fn list_failed_items(
        task_run_id: i64,
    ) -> Result<Vec<ImportFailedItemResource>, ServiceError> {
        let _ = task_run_id;
        todo!("骨架：查该 TaskRun 的失败项；运行不存在 -> 404 import_task_not_found")
    }

    /// `POST /imports/{task_run_id}/failed-items/{item_id}/search` —— **200**。
    ///
    /// 错误码：`404 import_task_not_found` / `404 failed_item_not_found` /
    /// `409 failed_item_not_pending`（这条已在导入中，不能再搜）/
    /// `409 failed_item_search_unavailable`（元数据源不可用）。
    pub async fn search_failed_item(
        task_run_id: i64,
        item_id: &str,
        movie_number: &str,
    ) -> Result<ImportMetadataSearchResponse, ServiceError> {
        let _ = (task_run_id, item_id, movie_number);
        todo!("骨架：取失败项 -> 按番号搜元数据候选（按置信度降序）")
    }

    /// `POST /imports/{task_run_id}/failed-items/{item_id}/retry` —— **202**。
    ///
    /// 错误码：同 `search_failed_item`，另加 `409 failed_item_source_unavailable`
    /// （暂存文件已被清理 —— 只能让用户重新浏览导入）。
    pub async fn enqueue_failed_item_retry(
        task_run_id: i64,
        item_id: &str,
        candidate_id: &str,
    ) -> Result<ImportAcceptedResponse, ServiceError> {
        let _ = (task_run_id, item_id, candidate_id);
        todo!("骨架：以选定的候选入队一条重试（mode=retry_failed_item）")
    }

    /// ★ 执行体。worker 调用。
    ///
    /// 按 `params["mode"]` 分发（见模块文档）。**未结束的 TaskRun 不得再执行**
    /// （`409 import_task_not_finished`）—— 那会让同一批文件被导两次。
    pub async fn execute(params: &serde_json::Value) -> Result<ImportExecuteSummary, ServiceError> {
        let _ = params;
        todo!("骨架：按 params.mode 分发三种模式；逐条失败进 failed 列表而非整体 Err")
    }

    /// 这个失败原因是否**不该**让用户自己背锅。
    pub fn is_manual_search_failure(reason: &str) -> bool {
        MANUAL_SEARCH_FAILURE_REASONS.contains(&reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动可重试的原因**只有两个**。
    ///
    /// 多加一个（比如 `is_collection`）会让「用户点了但导入不进去」被当作
    /// 系统错误走告警，而那其实是正常跳过。
    #[test]
    fn only_two_reasons_count_as_user_fixable() {
        assert!(ImportTaskService::is_manual_search_failure(
            "movie_number_not_found"
        ));
        assert!(ImportTaskService::is_manual_search_failure(
            "metadata_fetch_failed"
        ));
        assert!(!ImportTaskService::is_manual_search_failure(
            "is_collection"
        ));
        assert!(!ImportTaskService::is_manual_search_failure("stage_failed"));
    }

    /// 失败项的 `item_id` 是**字符串**。
    ///
    /// 上游的 item_id 是导入器给的业务标识（可能带前缀、可能含 URL 编码的
    /// 路径），声明成整数会让含前缀的 id 直接无法路由。
    #[test]
    fn the_failed_item_id_is_a_string_not_a_number() {
        let item = ImportFailedItemResource {
            item_id: "seed:ABC-123:0".to_owned(),
            movie_number: "ABC-123".to_owned(),
            failure_reason: "metadata_fetch_failed".to_owned(),
            failure_detail: None,
            attempts: 1,
        };
        let json = serde_json::to_value(&item).expect("可序列化");
        assert!(json.get("item_id").and_then(|v| v.as_str()).is_some());
    }

    /// `attempts >= 3` 意味着反复失败 —— UI 该提示换番号而不是重试。
    #[test]
    fn repeated_failures_are_visible_in_the_attempt_count() {
        let item = ImportFailedItemResource {
            item_id: "x".to_owned(),
            movie_number: "ABC-123".to_owned(),
            failure_reason: "movie_number_not_found".to_owned(),
            failure_detail: None,
            attempts: 3,
        };
        assert!(item.attempts >= 3);
    }

    /// 任务键必须与 `lane_of` 的专属道一致 —— 改成别的会让导入退回 default 道。
    #[test]
    fn the_task_key_matches_the_dedicated_lane() {
        assert_eq!(TASK_KEY, "library_import");
    }

    /// 失败项与候选是两个不同结构，**别复用**。
    #[test]
    fn candidates_and_failed_items_are_distinct_shapes() {
        let failure = ImportFailure {
            source_ref: serde_json::json!({}),
            movie_number: "ABC-123".to_owned(),
            media_kind: "jav".to_owned(),
            failure_reason: "metadata_fetch_failed".to_owned(),
            failure_detail: None,
            staged: false,
        };
        let candidate = MetadataCandidate {
            candidate_id: "c1".to_owned(),
            title: "t".to_owned(),
            date: None,
            confidence: 0.9,
        };
        // 候选带 confidence，失败项没有；失败项带 reason，候选没有。
        let candidate_json = serde_json::to_value(&candidate).expect("可序列化");
        assert!(candidate_json.get("confidence").is_some());
        assert!(candidate_json.get("failure_reason").is_none());
        let _ = failure;
    }
}
