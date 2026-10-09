//! 下载任务台账与宿主侧导入交接（上游 `downloads/task_service.py`，176 行）。
//!
//! # ★ `DELETE` 的**两步确认**是本文件最重要的契约
//!
//! `delete_files=true` 会**真删磁盘文件**。上游要求客户端发两次请求：
//!
//! 第一次 `?delete_files=true` → **422**
//! `download_task_delete_confirmation_required`，客户端据此弹确认框；
//! 用户确认后再带 `confirm_delete_files=true` 重发。
//!
//! **不要「优化」成一步。** 合并成单参数等于取消确认，而确认的全部价值就在
//! 「客户端必须先收到拒绝，才知道要弹框」。
//!
//! # 可导入状态是**白名单**，且含 `failed` 与 `skipped`
//!
//! [`DownloadTaskService::DEFAULT_IMPORTABLE_STATUSES`] 三态都可重试导入。
//! 尤其别漏掉 `failed`：下载失败了正是最需要重试导入的时候。
//!
//! # 409 有三个不同语义，别混用
//!
//! | 码 | 含义 | 客户端该做什么 |
//! |---|---|---|
//! | `download_task_import_running` | **已经有一个导入在跑** | 等待，不要重发 |
//! | `download_task_import_conflict` | 任务状态不允许导入 | 先修状态 |
//! | `422 invalid_download_task_import` | 状态在白名单外 | 改状态 |

use serde::{Deserialize, Serialize};

use super::download_common::DownloadTaskRow;
use crate::error::ServiceError;

/// 导入状态：待处理。
pub const IMPORT_STATUS_PENDING: &str = "pending";
/// 导入状态：失败。
pub const IMPORT_STATUS_FAILED: &str = "failed";
/// 导入状态：跳过。
pub const IMPORT_STATUS_SKIPPED: &str = "skipped";

/// 任务条目（响应体）。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTaskResource {
    pub id: i64,
    pub movie_number: String,
    /// 状态。**取值由 provider 决定**，不是 enum。
    pub state: String,
    pub progress: Option<f64>,
    pub client_id: Option<i32>,
    pub client_name: Option<String>,
    pub created_at: Option<String>,
    /// 是否可触发导入。前端据此决定按钮是否可点。
    pub importable: bool,
}

/// 泛型分页（与其它域共用同一形状）。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTaskPage {
    pub items: Vec<DownloadTaskResource>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
}

/// 触发导入的响应（**202**）。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTaskImportResponse {
    /// 导入任务的运行 id。轮询它看结果。
    pub task_run_id: i64,
    /// 本次受理的下载任务数。
    pub accepted: i32,
}

/// 删除参数 —— **两步确认**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeleteTaskQuery {
    /// 是否连带删除已下载文件。默认 `false`。
    #[serde(default)]
    pub delete_files: bool,
    /// 是否已确认。`delete_files = true` 时**必须**也为 `true`。
    #[serde(default)]
    pub confirm_delete_files: bool,
}

/// 列表查询参数。
///
/// ⚠️ `state` 是**重复 query 参数**（`?state=queued&state=failed`），
/// **不是 CSV**。这与本仓别处（`actor_ids` / `movie_ids` 用 CSV）不同 ——
/// 两处形态都照上游，不要「统一」成一种，统一了就有一边对不上。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListTasksQuery {
    pub client_id: Option<i32>,
    pub movie_number: Option<String>,
    /// 重复 query 参数。`None` = 不过滤；`Some(&[])` = 无结果。
    pub state: Option<Vec<String>>,
    /// 白名单排序字段，见 [`super::download_common::TASK_SORT_FIELDS`]。
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// ★ 两步确认的第一道门。**路由层必须先调它**，再调 [`DownloadTaskService::delete_task`]。
///
/// 三个要素一个都不能少（见模块文档）：状态码 **422**、错误码
/// `download_task_delete_confirmation_required`、`details.task_id`。
///
/// 放成自由函数而不是 `delete_task` 的第一行，是因为**顺序**要由调用方保证
/// —— 路由层若先查了任务再判确认，就等于在拒绝前告诉了客户端「这个任务
/// 存在」，那本身是一次可被用来探测任务存在性的侧信道。
pub fn ensure_delete_confirmed(task_id: i64, query: &DeleteTaskQuery) -> Result<(), ServiceError> {
    if !query.delete_files || query.confirm_delete_files {
        return Ok(());
    }
    let mut details = serde_json::Map::new();
    details.insert("task_id".to_owned(), serde_json::json!(task_id));
    Err(ServiceError::validation_with(
        "download_task_delete_confirmation_required",
        "Deleting downloaded files requires explicit confirmation",
        details,
    ))
}

/// 台账服务。
pub struct DownloadTaskService;

impl DownloadTaskService {
    /// **可触发导入的状态白名单**（三态，见模块文档）。
    pub const DEFAULT_IMPORTABLE_STATUSES: [&'static str; 3] =
        [IMPORT_STATUS_PENDING, IMPORT_STATUS_FAILED, IMPORT_STATUS_SKIPPED];

    /// `GET /download-tasks` —— **200**，泛型分页。
    ///
    /// `state` 的归一见 [`super::download_common::normalize_state_filters`]；
    /// `sort` 的白名单见 [`super::download_common::resolve_task_sort`]。
    pub async fn list_tasks(query: &ListTasksQuery) -> Result<DownloadTaskPage, ServiceError> {
        let _ = query;
        todo!("骨架：分页 + 重复 state 参数 + 白名单排序；importable 按白名单算")
    }

    /// ★ `DELETE /download-tasks/{task_id}` —— **204**，两步确认。
    ///
    /// 签名里**没有** `confirm_delete_files` —— 它已被提到
    /// [`DeleteTaskQuery`] 里由路由层先判。这里只管「确认过了之后」的删除。
    ///
    /// 错误码：任务不存在 → 404；provider 删任务失败 → `provider_{code}`。
    pub async fn delete_task(
        task_id: i64,
        query: &DeleteTaskQuery,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = (task_id, query.delete_files);
        todo!("骨架：两步确认已由路由层校验；这里调 provider 删任务 + 按需删文件")
    }

    /// `POST /download-tasks/{task_id}/import` —— **202**。
    ///
    /// 三个 409/422 的分工见模块文档。`allowed_statuses` 缺省用
    /// [`Self::DEFAULT_IMPORTABLE_STATUSES`]。
    pub async fn trigger_import(
        task_id: i64,
        allowed_statuses: Option<&[&str]>,
    ) -> Result<DownloadTaskImportResponse, ServiceError> {
        let _ = (task_id, allowed_statuses);
        todo!("骨架：查任务 -> 校验状态白名单 -> 经 import_task 入队 -> 202 + task_run_id")
    }

    /// 该任务当前是否可导入。
    pub fn importable(task: &DownloadTaskRow, import_status: Option<&str>) -> bool {
        match import_status {
            // 有导入在跑 -> 不可再导。
            Some(status) if !status.is_empty() && status != "succeeded" => false,
            // 没有导入记录 -> 看下载状态在不在白名单里。
            _ => Self::DEFAULT_IMPORTABLE_STATUSES
                .iter()
                .any(|allowed| *allowed == task.state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: &str) -> DownloadTaskRow {
        DownloadTaskRow {
            id: 1,
            movie_number: "ABC-123".to_owned(),
            state: state.to_owned(),
            client_id: Some(1),
        }
    }

    /// 两步确认的第一道门：`delete_files` 且未确认 → **422 + 专用码 + task_id**。
    ///
    /// 三个要素都要有：状态码 422、错误码、以及 `details.task_id`（客户端要靠
    /// 它告诉用户**哪个**任务要确认）。
    #[test]
    fn deleting_files_without_confirmation_is_refused_with_the_task_id() {
        let query = DeleteTaskQuery {
            delete_files: true,
            confirm_delete_files: false,
        };
        let error = ensure_delete_confirmed(1, &query).expect_err("未确认应被拒");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), "download_task_delete_confirmation_required");
        let details = error.details().expect("必须带 details");
        assert_eq!(details.get("task_id").and_then(|v| v.as_i64()), Some(1));
    }

    /// 带确认就放行 —— 门只挡「要删文件但没确认」这一种组合。
    #[test]
    fn confirmation_lets_the_delete_through() {
        let confirmed = DeleteTaskQuery {
            delete_files: true,
            confirm_delete_files: true,
        };
        assert!(ensure_delete_confirmed(1, &confirmed).is_ok());
        // 不删文件时不需要确认.
        let no_files = DeleteTaskQuery::default();
        assert!(ensure_delete_confirmed(1, &no_files).is_ok());
    }

    /// 可导入状态**含 `failed` 与 `skipped`** —— 漏掉 failed 会让下载失败的
    /// 影片再也导不进来。
    #[test]
    fn failed_and_skipped_are_importable() {
        assert!(DownloadTaskService::importable(&task("pending"), None));
        assert!(DownloadTaskService::importable(&task("failed"), None));
        assert!(DownloadTaskService::importable(&task("skipped"), None));
        // 下载中不允许导入：文件还没下完。
        assert!(!DownloadTaskService::importable(&task("downloading"), None));
    }

    /// 已有**未成功**的导入在跑时不可再导 —— 否则会起两个导入抢同一个文件。
    #[test]
    fn a_running_import_blocks_another_one() {
        assert!(!DownloadTaskService::importable(&task("pending"), Some("running")));
        assert!(!DownloadTaskService::importable(&task("pending"), Some("failed")));
        // 已成功的不阻塞（可重新导入）。
        assert!(DownloadTaskService::importable(&task("pending"), Some("succeeded")));
    }

    /// `None` 与空列表在 `state` 上**语义不同**，这里锁住归一结果。
    #[test]
    fn state_filters_keep_the_upstream_distinction() {
        use super::super::download_common::normalize_state_filters;
        assert_eq!(normalize_state_filters(None).expect("不过滤"), None);
        assert_eq!(
            normalize_state_filters(Some(&[])).expect("空列表"),
            Some(Vec::new())
        );
    }
}
