//! `/download-tasks*` 与 `POST /download-requests` —— 任务台账四个端点。
//!
//! # 与上游 `src/api/routers/transfers/downloads.py` 的对应
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `POST /download-requests` | 200 |
//! | `GET /download-tasks` | 200（`PageResponse`） |
//! | `DELETE /download-tasks/{task_id}` | **204** |
//! | `POST /download-tasks/{task_id}/import` | **202 Accepted** |
//!
//! # 最重要的一处：`DELETE` 的**两步确认**
//!
//! 上游：
//!
//! ```python
//! if delete_files and not confirm_delete_files:
//!     raise ApiError(
//!         422,
//!         "download_task_delete_confirmation_required",
//!         "Deleting downloaded files requires explicit confirmation",
//!         {"task_id": task_id},
//!     )
//! ```
//!
//! 三个要素都要照抄：
//!
//! | 要素 | 值 | 照抄的理由 |
//! |---|---|---|
//! | 状态码 | **422**（不是 400、不是 409） | 上游用 422 |
//! | code | `download_task_delete_confirmation_required` | 客户端要靠它区分「要二次确认」与其他失败 |
//! | details | `{"task_id": task_id}` | 客户端要能告诉用户**是哪个任务**要确认 |
//!
//! # 为什么这个设计值得单独写一段
//!
//! `delete_files=true` 会**真删磁盘文件**。一个手滑或一个被构造的请求就能让
//! 下载好的影片消失，而任务台账里可能还留着记录。两个布尔参数把「我确认」
//! 与「我要删」分开，客户端必须发两次请求 —— 第一次 `delete_files=true`
//! 拿到 422，弹确认框；用户确认后再带 `confirm_delete_files=true` 重发。
//!
//! **不要「优化」成一步。** 合并成单参数等于取消确认，而确认的价值恰恰在于
//! 「客户端必须先收到拒绝才知道要弹框」。
//!
//! # `state` 是**重复 query 参数**，不是 CSV
//!
//! `GET /download-tasks` 的 `state: list[str] | None = Query(default=None)` ——
//! FastAPI 对 `list[str]` 会展开成重复参数：`?state=downloading&state=done`。
//!
//! **这与本仓库别处的 CSV 约定不同**（`actor_ids`、`movie_ids` 都是 CSV）。
//! 两处形态都照上游，不要「统一」成一种 —— 统一了就有一边对不上。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
// 删除参数与「两步确认」的校验都在 service 层，**不在路由层复制一份**
// （`handoff.md` 纪律第 7 条）。
use sm_service::transfers::download_task::DeleteTaskQuery;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/download-requests",
            post(create_download_request).fallback(method_not_allowed),
        )
        .route(
            "/download-tasks",
            get(list_download_tasks).fallback(method_not_allowed),
        )
        .route(
            "/download-tasks/{task_id}",
            delete(delete_download_task).fallback(method_not_allowed),
        )
        .route(
            "/download-tasks/{task_id}/import",
            post(trigger_download_task_import).fallback(method_not_allowed),
        )
}

/// `DELETE /download-tasks/{task_id}`
///
/// 上游用 `Query` 参数而非请求体 —— 因为它要能被浏览器与 curl 直接调。
/// 所以这里注册成 `delete(handler)` 而非 `delete(handler, body)`。
async fn delete_download_task(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(task_id): Path<i64>,
    EnvelopeQuery(query): EnvelopeQuery<DeleteTaskQuery>,
) -> Result<StatusCode, ErrorResponse> {
    // 顺序不能反：先查确认，再动手。确认过了才允许碰磁盘。
    //
    // 契约本体在 service 层（`sm_service::transfers::download_task::
    // ensure_delete_confirmed`）—— 这里是**调用**它而不是重写一遍，
    // 因为错误码、状态码与 `details.task_id` 三者是一个整体，两处各写一份
    // 迟早会漂移。
    sm_service::transfers::download_task::ensure_delete_confirmed(task_id, &query)?;
    todo!("骨架：接任务台账删除（成功 204 不带 body）")
}

/// `GET /download-tasks` 的查询参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListTasksQuery {
    /// 轮次 / 任务键等过滤。
    pub task_key: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    /// **重复 query 参数**：`?state=downloading&state=done`。
    /// 不是 CSV —— 见模块文档。
    pub state: Option<Vec<String>>,
    pub sort: Option<String>,
}

/// 下载任务条目。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTaskResource {
    pub id: i64,
    pub movie_number: String,
    /// 任务状态。取值集合由下载器 provider 决定，**不要建模成 enum**。
    pub state: String,
    pub progress: Option<f64>,
    pub client_id: Option<i32>,
    pub created_at: Option<String>,
}

/// `GET /download-tasks`
async fn list_download_tasks(
    _user: CurrentUser,
    State(_state): State<AppState>,
    EnvelopeQuery(_query): EnvelopeQuery<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接任务台账分页列表（state 是重复 query 参数）")
}

/// `POST /download-requests` —— 创建下载请求。
///
/// **200**（不是 201、不是 202）—— 上游如此。任务本身是异步的，但这个响应
/// 返回的是「已登记的候选 + client id」，不是任务句柄。
async fn create_download_request(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接下载请求创建（注意 200 而非 201/202）")
}

/// `POST /download-tasks/{task_id}/import` —— **202 Accepted**。
///
/// 202 是对的：导入是**长任务**，立即返回结果不可能。响应体给出任务句柄供轮询。
async fn trigger_download_task_import(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_task_id): Path<i64>,
) -> Result<(StatusCode, Json<serde_json::Value>), ErrorResponse> {
    todo!("骨架：接导入流水线触发（202 + 任务句柄）")
}
