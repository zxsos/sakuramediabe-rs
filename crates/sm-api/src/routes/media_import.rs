//! `/import-sources/*` 与 `/imports/*` —— 导入流水线五个端点。
//!
//! # 与上游 `src/api/routers/transfers/media_import.py` 的对应
//!
//! | 上游端点 | 状态码 | 作用 |
//! |---|---|---|
//! | `POST /import-sources/browse` | 200 | 浏览可导入的源 |
//! | `POST /imports` | **202** | 发起一次导入 |
//! | `GET /imports/{task_run_id}/failed-items` | 200 | 列出失败项 |
//! | `POST /imports/{task_run_id}/failed-items/{item_id}/search` | 200 | 给失败项搜元数据 |
//! | `POST /imports/{task_run_id}/failed-items/{item_id}/retry` | **202** | 重试失败项 |
//!
//! # `{item_id}` 是 **`String` 而非整数**
//!
//! 三处都带 `{item_id}`，而 `task_run_id` 是 `int`。**item_id 不是自增主键**
//! —— 它是导入器给的**业务标识**（可能带前缀、可能含 URL 编码后的路径）。
//!
//! 所以：**不要**把 `item_id` 声明成 `i64`（那会让含前缀的 id 直接 422），
//! 也**不要**声明成 `Path<String>` 后再做数字解析 —— 上游就是字符串。
//!
//! # 两个 202 的语义不同
//!
//! - `POST /imports` —— 导入是**长任务**，202 + 任务句柄
//! - `POST .../retry` —— 重试也是长任务，202
//!
//! 而 `POST /import-sources/browse` 与 `.../search` 是 **200**：它们是**查询**
//! 不是任务。所以这个文件里 202/200 的分界是「有没有后台工作在跑」，
//! **不是**「参数是不是 JSON 体」。
//!
//! # 失败项的 search 与 retry 是一对
//!
//! `search` 给失败项找候选元数据（供人工挑选），`retry` 带上挑选结果重新入队。
//! `search` 返回的是 `ImportMetadataSearchResponse`，`retry` 收的是
//! `ImportFailedItemRetryRequest` —— **两者不是同一个结构**，别复用。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/import-sources/browse",
            post(browse_import_sources).fallback(method_not_allowed),
        )
        .route(
            "/imports",
            post(create_import).fallback(method_not_allowed),
        )
        .route(
            "/imports/{task_run_id}/failed-items",
            get(list_import_failed_items).fallback(method_not_allowed),
        )
        .route(
            "/imports/{task_run_id}/failed-items/{item_id}/search",
            post(search_import_failed_item).fallback(method_not_allowed),
        )
        .route(
            "/imports/{task_run_id}/failed-items/{item_id}/retry",
            post(retry_import_failed_item).fallback(method_not_allowed),
        )
}

/// 浏览请求。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImportBrowseRequest {
    /// 来源类型（本地目录、远程路径等）。取值集合由导入器决定。
    pub source: String,
    /// 递归深度；`None` = 导入器默认。
    pub depth: Option<i64>,
}

/// 浏览结果。
#[derive(Debug, Clone, Serialize)]
pub struct ImportBrowseResponse {
    /// 候选条目。
    pub items: Vec<ImportBrowseItem>,
    /// 是否还有更多（分页游标）。
    pub next_cursor: Option<String>,
}

/// 一条可导入项。
#[derive(Debug, Clone, Serialize)]
pub struct ImportBrowseItem {
    /// 导入器给的不透明标识。**回传给 `create_import` 用**。
    pub source_ref: String,
    pub name: String,
    pub is_directory: bool,
    pub size_bytes: Option<i64>,
}

/// 导入失败项。
#[derive(Debug, Clone, Serialize)]
pub struct ImportFailedItemResource {
    /// **字符串业务标识**，不是自增主键（见模块文档）。
    pub item_id: String,
    pub name: String,
    /// 失败原因码。
    pub reason_code: String,
    pub reason_detail: Option<String>,
    /// 已尝试次数。
    pub attempts: i32,
}

/// 元数据搜索结果。
#[derive(Debug, Clone, Serialize)]
pub struct ImportMetadataSearchResponse {
    pub item_id: String,
    /// 候选元数据，按置信度降序。
    pub candidates: Vec<ImportMetadataCandidate>,
}

/// 一条元数据候选。
#[derive(Debug, Clone, Serialize)]
pub struct ImportMetadataCandidate {
    pub title: String,
    pub date: Option<String>,
    pub provider: String,
    /// 置信度 [0, 1]。
    pub confidence: f32,
}

/// 重试请求。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImportFailedItemRetryRequest {
    /// 采用哪条候选；`None` = 用导入器自己的默认重试。
    pub chosen_candidate: Option<ImportMetadataCandidateRef>,
}

/// 重试时引用的候选（**与 `ImportMetadataCandidate` 不是同一结构**）。
#[derive(Debug, Clone, Deserialize)]
pub struct ImportMetadataCandidateRef {
    pub provider: String,
    /// 该 provider 侧的外部 id。
    pub external_id: String,
}

/// 已受理的导入任务。
#[derive(Debug, Clone, Serialize)]
pub struct ImportAcceptedResponse {
    /// 任务运行 id，后续查失败项要用。
    pub task_run_id: i64,
    /// 已入队的条目数。
    pub accepted: i32,
}

/// `POST /import-sources/browse` —— **200**（查询，不是任务）。
async fn browse_import_sources(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<ImportBrowseRequest>,
) -> Result<Json<ImportBrowseResponse>, ErrorResponse> {
    todo!("骨架：接导入器（依赖 provider 插件）")
}

/// `POST /imports` —— **202 Accepted**（长任务）。
async fn create_import(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<(StatusCode, Json<ImportAcceptedResponse>), ErrorResponse> {
    todo!("骨架：接导入流水线；成功返回 202 + task_run_id")
}

/// `GET /imports/{task_run_id}/failed-items`
///
/// `task_run_id` 不存在 → **404**（区别于「存在但没有失败项」= 200 空列表）。
async fn list_import_failed_items(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_task_run_id): Path<i64>,
) -> Result<Json<Vec<ImportFailedItemResource>>, ErrorResponse> {
    todo!("骨架：接失败项列表（运行不存在 -> 404）")
}

/// `POST /imports/{task_run_id}/failed-items/{item_id}/search` —— **200**。
async fn search_import_failed_item(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path((_task_run_id, _item_id)): Path<(i64, String)>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<Json<ImportMetadataSearchResponse>, ErrorResponse> {
    todo!("骨架：接失败项的元数据搜索")
}

/// `POST /imports/{task_run_id}/failed-items/{item_id}/retry` —— **202**。
async fn retry_import_failed_item(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path((_task_run_id, _item_id)): Path<(i64, String)>,
    axum::extract::Json(_payload): axum::extract::Json<ImportFailedItemRetryRequest>,
) -> Result<(StatusCode, Json<ImportAcceptedResponse>), ErrorResponse> {
    todo!("骨架：接失败项重试（202）")
}