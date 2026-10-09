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
//! # 形状**一律**从 `sm-service` 取，这里不再定义
//!
//! 骨架期本文件自带一套内联 DTO（`ImportAcceptedResponse` /
//! `ImportFailedItemResource` / `ImportMetadataSearchResponse` /
//! `ImportMetadataCandidate` / `ImportFailedItemRetryRequest` / 浏览三件套），
//! 其中**每一个**都与 service 层的同名类型不同，而 service 层那一套又与上游
//! 不同 —— 同一个契约在本仓有两三份互相矛盾的版本。
//!
//! 现在只留一处：导入的形状在
//! [`sm_service::transfers::import_task`]，浏览的形状在
//! [`sm_service::transfers::provider_browse`]。**加端点时先看那边有没有现成的**。
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
//! `search` 返回的是 [`ImportMetadataSearchResponse`]，`retry` 收的是
//! [`ImportFailedItemRetryRequest`] —— **两者不是同一个结构**，别复用。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use sm_service::transfers::import_task::{
    ImportAcceptedResponse, ImportFailedItemResource, ImportFailedItemRetryRequest,
    ImportMetadataSearchRequest, ImportMetadataSearchResponse, ImportRequest, ImportTaskService,
};
use sm_service::transfers::provider_browse::{ImportBrowseRequest, ImportBrowseResponse};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/import-sources/browse",
            post(browse_import_sources).fallback(method_not_allowed),
        )
        .route("/imports", post(create_import).fallback(method_not_allowed))
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

/// `POST /import-sources/browse` —— **200**（查询，不是任务）。
///
/// ⚠️ **未接线**：浏览要调 storage provider（插件 ABI），属
/// `provider_browse` 那一轮。请求/响应形状已在
/// [`sm_service::transfers::provider_browse`] 定义（那里也还带着几处与上游的
/// 偏差，一并留到那一轮）。
async fn browse_import_sources(
    _user: CurrentUser,
    State(_state): State<AppState>,
    EnvelopeJson(_payload): EnvelopeJson<ImportBrowseRequest>,
) -> Result<Json<ImportBrowseResponse>, ErrorResponse> {
    todo!("骨架：接导入器（依赖 provider 插件）")
}

/// `POST /imports` —— **202 Accepted**（长任务）。
///
/// 上游就这一行：`ImportTaskService.enqueue(payload)`，其余参数取默认
/// （`trigger_type="manual"`、没有 `download_task_id`、任务名按 `media_kind`
/// 推）。所以这里也是那句话 —— 校验、媒体库 404/422、按库 409、
/// 502 全在 service 里。
async fn create_import(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<ImportRequest>,
) -> Result<(StatusCode, Json<ImportAcceptedResponse>), ErrorResponse> {
    let accepted = ImportTaskService::new(state.db())
        .enqueue(payload, "manual", None, None)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(accepted)))
}

/// `GET /imports/{task_run_id}/failed-items`
///
/// `task_run_id` 不存在，**或者它不是导入任务**（别的任务类型的 id 也能填进
/// 这个路径）→ **404 `import_task_not_found`**；存在但没有失败项 → **200 +
/// 空数组**。两者必须能区分：前者说明客户端拿着一个错的 id，后者是正常结果。
async fn list_import_failed_items(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(task_run_id): Path<i32>,
) -> Result<Json<Vec<ImportFailedItemResource>>, ErrorResponse> {
    let items = ImportTaskService::new(state.db())
        .list_failed_items(task_run_id)
        .await?;
    Ok(Json(items))
}

/// `POST /imports/{task_run_id}/failed-items/{item_id}/search` —— **200**。
///
/// 请求体**只有** `movie_number`（上游 `ImportMetadataSearchRequest`）——
/// 番号来自用户在前端的输入，**不是**失败项里存的那个（失败项可能压根没识别出
/// 番号，那正是它失败的原因）。
async fn search_import_failed_item(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((_task_run_id, _item_id)): Path<(i32, String)>,
    EnvelopeJson(payload): EnvelopeJson<ImportMetadataSearchRequest>,
) -> Result<Json<ImportMetadataSearchResponse>, ErrorResponse> {
    // 番号取**请求体**，不是失败项里存的那个（见上面的方法文档）。
    // 空番号 422 / 来源失败进 source_errors 都在服务层。
    let response = state
        .metadata_search()?
        .search_by_number(&payload.movie_number)
        .await?;
    Ok(Json(response))
}

/// `POST /imports/{task_run_id}/failed-items/{item_id}/retry` —— **202**。
async fn retry_import_failed_item(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path((_task_run_id, _item_id)): Path<(i32, String)>,
    EnvelopeJson(_payload): EnvelopeJson<ImportFailedItemRetryRequest>,
) -> Result<(StatusCode, Json<ImportAcceptedResponse>), ErrorResponse> {
    todo!("骨架：接失败项重试（202；mode=retry_failed_file）")
}
