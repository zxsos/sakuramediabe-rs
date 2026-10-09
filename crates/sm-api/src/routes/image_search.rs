//! `/image-search*` —— 图搜与剧情图搜的六个端点。
//!
//! # 与上游 `src/api/routers/discovery/image_search.py` 的对应
//!
//! | 上游端点 | 编码 | 依赖 |
//! |---|---|---|
//! | `POST /image-search/sessions`（`:34`） | **multipart** | `sm_service::discovery::image_search` |
//! | `GET /image-search/sessions/{session_id}/results`（`:60`） | query | 同上 |
//! | `POST /image-search/text-sessions`（`:74`） | **form** | 同上 |
//! | `POST /image-search/plot-sessions`（`:94`） | **multipart** | `sm_service::discovery::plot_image_search` |
//! | `GET /image-search/plot-sessions/{session_id}/results`（`:128`） | query | 同上 |
//! | `POST /image-search/plot-text-sessions`（`:147`） | **form** | 同上 |
//!
//! # ⚠️ 骨架期说「axum 的 `form` feature 未启用」—— **是错的**
//!
//! 本仓的 `axum` 依赖**没关** default features（见 `crates/sm-api/Cargo.toml`），
//! 而 axum 0.8 的 default 里**就含 `form`**。所以 `axum::extract::Form` /
//! `Multipart` 一直都在 —— `extract.rs` 里那个 `Form` 信封包装能编译即为证。
//! 这里**没有**任何 feature 前置要解。
//!
//! 骨架还把那段写成了 `POST /auth/docs-token`「不实现」的论据（「form 编码
//! 在本仓库不可用」）—— **不成立**：那条理由是关于**消费方**的（本仓无
//! Swagger UI），与 form 编码无关；而且该端点**现已照上游实现**（见
//! `routes/auth.rs`）。
//!
//! 这四个 form 端点真正要做的一件事：用 [`crate::extract::Form`]（保留错误
//! 信封）而不是 axum 原生 `Form`。
//!
//! # 错误码是 **400**，不是仓库惯例的 422
//!
//! 四个建会话端点都是 `except ValueError -> HTTPException(400)`，而本仓库
//! `ServiceError::validation` 是 422。**照抄 400。**
//!
//! 但注意 `GET .../results` 是 `LookupError -> 404` 与 `ValueError -> 400`
//! —— 同一个端点里 404 与 400 都可能出现，**不是所有错误都该转 422**。
//!
//! # `movie_ids` / `exclude_movie_ids` 是 **CSV 字符串**，不是数组
//!
//! 上游用 `parse_csv_positive_ints(value, name, error_code="invalid_image_search_filter")`
//! 解析，传输形态是 `movie_ids=1,2,3`。两个要点：
//!
//! 1. **只收正整数**（函数名里的 `positive`）—— 0 或负数报
//!    `invalid_image_search_filter`，**不是**当作「没有过滤」。
//! 2. **CSV 是有意的，不是将就** —— multipart 的 form field 没有「数组」类型，
//!    重复字段名在客户端实现上不统一。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::discovery::image_search::ImageSearchPage;
use sm_service::discovery::image_search_reset::ImageSearchResetResult;
use sm_service::discovery::plot_image_search::PlotImageSearchPage;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/image-search/sessions", post(create_image_search_session))
        .route(
            "/image-search/sessions/{session_id}/results",
            get(get_image_search_results),
        )
        .route(
            "/image-search/text-sessions",
            post(create_text_image_search_session),
        )
        .route(
            "/image-search/plot-sessions",
            post(create_plot_image_search_session),
        )
        .route(
            "/image-search/reset",
            post(reset_image_search).fallback(method_not_allowed),
        )
        .route(
            "/image-search/plot-sessions/{session_id}/results",
            get(get_plot_image_search_results),
        )
        .route(
            "/image-search/plot-text-sessions",
            post(create_plot_text_search_session),
        )
}
/// 四个建会话端点**共用**的过滤/分页字段。
///
/// 形状照上游：`movie_ids` / `exclude_movie_ids` 是 CSV 字符串，`score_threshold`
/// 是浮点门限，三者都可空。
#[derive(Debug, Default, Deserialize)]
pub struct SearchFilters {
    #[serde(default)]
    pub page_size: Option<i64>,
    /// CSV 正整数。`None` 与 `Some("")` 语义不同：后者是「包含/排除零个」。
    #[serde(default)]
    pub movie_ids: Option<String>,
    #[serde(default)]
    pub exclude_movie_ids: Option<String>,
    #[serde(default)]
    pub score_threshold: Option<f64>,
}

/// 翻页查询。
#[derive(Debug, Default, Deserialize)]
pub struct ResultsQuery {
    /// 上游 `Query(min_length=1)` —— 空串要 422。
    #[serde(default)]
    pub cursor: Option<String>,
}

/// 图搜会话分页响应（对应上游 `ImageSearchSessionPageResource`）。
#[derive(Debug, Serialize)]
pub struct ImageSearchSessionResponse {
    pub session_id: String,
    #[serde(flatten)]
    pub page: ImageSearchPage,
}

/// 剧情图搜会话分页响应（对应上游 `MoviePlotImageSearchSessionPageResource`）。
#[derive(Debug, Serialize)]
pub struct PlotImageSearchSessionResponse {
    pub session_id: String,
    #[serde(flatten)]
    pub page: PlotImageSearchPage,
}

/// `POST /image-search/sessions` —— multipart：文件 + 过滤条件。
async fn create_image_search_session(
    State(_state): State<AppState>,
    _user: CurrentUser,
    _payload: axum::extract::Multipart,
) -> Result<Json<ImageSearchSessionResponse>, ErrorResponse> {
    todo!("骨架：照上游 `:34-58` 实现（multipart；ValueError -> 400）")
}

/// `GET /image-search/sessions/{session_id}/results`
///
/// 上游：`LookupError -> 404`、`ValueError -> 400`（`:60-72`）。
async fn get_image_search_results(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_session_id): Path<String>,
    axum::extract::Query(_query): axum::extract::Query<ResultsQuery>,
) -> Result<Json<ImageSearchSessionResponse>, ErrorResponse> {
    todo!("骨架：接 ImageSearchService::list_results（404 / 400 两种错误）")
}

/// `POST /image-search/text-sessions` —— **form 编码**。
///
/// ⚠️ 骨架期签名收的是 `Json<SearchFilters>` —— **传输形态就错了**（上游是
/// `Form`，字段还是 CSV 字符串，见模块文档）。实现时换成
/// [`crate::extract::Form`] + 表单专用 DTO，别沿用 `SearchFilters`。
async fn create_text_image_search_session(
    State(_state): State<AppState>,
    _user: CurrentUser,
    _payload: axum::extract::Json<SearchFilters>,
) -> Result<Json<ImageSearchSessionResponse>, ErrorResponse> {
    todo!("骨架：用 EnvelopeForm + 表单 DTO；照上游 `:74-92` 实现")
}

/// `POST /image-search/plot-sessions` —— multipart。
async fn create_plot_image_search_session(
    State(_state): State<AppState>,
    _user: CurrentUser,
    _payload: axum::extract::Multipart,
) -> Result<Json<PlotImageSearchSessionResponse>, ErrorResponse> {
    todo!("骨架：照上游 `:94-119` 实现")
}

/// `GET /image-search/plot-sessions/{session_id}/results`
async fn get_plot_image_search_results(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_session_id): Path<String>,
    axum::extract::Query(_query): axum::extract::Query<ResultsQuery>,
) -> Result<Json<PlotImageSearchSessionResponse>, ErrorResponse> {
    todo!("骨架：接 MoviePlotImageSearchService::list_results")
}

/// `POST /image-search/plot-text-sessions` —— **form 编码**。
///
/// ⚠️ 同 [`create_text_image_search_session`]：骨架的 `Json<SearchFilters>`
/// 传输形态错了，实现时换 `EnvelopeForm` + 表单 DTO。
async fn create_plot_text_search_session(
    State(_state): State<AppState>,
    _user: CurrentUser,
    _payload: axum::extract::Json<SearchFilters>,
) -> Result<Json<PlotImageSearchSessionResponse>, ErrorResponse> {
    todo!("骨架：用 EnvelopeForm + 表单 DTO；照上游 `:147-165` 实现")
}

/// CSV 正整数解析。
///
/// **空串返回 `Some(vec![])` 而不是 `None`** —— 调用方据此区分「显式给了空
/// 列表」与「没给这个过滤条件」。合并两者会让「排除全部」变成「不过滤」。
pub fn parse_csv_positive_ints(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<Vec<i64>>, ErrorResponse> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mut out = Vec::new();
    for piece in raw.split(',') {
        match piece.trim().parse::<i64>() {
            Ok(value) if value > 0 => out.push(value),
            _ => {
                return Err(ErrorResponse::new(
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid_image_search_filter",
                    format!("{field} 只接受逗号分隔的正整数"),
                ));
            }
        }
    }
    Ok(Some(out))
}
/// `POST /image-search/reset` —— **202 入队，不是「已重置」**。
///
/// # 这个端点**不删任何东西**
///
/// 它只是把 `image_search_index` 任务排上队（`params = {"reset": true}`）。
/// 真正的重置在后台 handler 里，而那个 handler **还没写**（21 个只落地 1 个）。
///
/// 所以响应是「排上了」而不是「重置完了」—— 客户端要拿 `task_run_id` 去轮询。
/// **别把它当成同步端点**，那会让用户以为索引已经清空。
///
/// # 409 的两个来源
///
/// | 情况 | code |
/// |---|---|
/// | 图搜未启用（`qdrant.enabled && image_search.enabled` 不满足）| `optional_services` 里的 code |
/// | 已有同 key 任务在跑/在队 | `image_search_reset_conflict` + `blocking_task_run_id` |
///
/// 后者用 `ConflictPolicy::Raise` 而非 `Skip` —— 手动触发必须让用户知道
/// 「已经有一次在跑」，否则点了没反应会以为功能坏了。
async fn reset_image_search(
    State(_state): State<AppState>,
    _user: CurrentUser,
) -> Result<(StatusCode, Json<ImageSearchResetResult>), ErrorResponse> {
    // TODO: 接 `TaskQueueService::new(state.db().clone())` 与
    // `optional_services::capabilities_of(state.config())`。
    // 前者要 `BackgroundTaskRunRepository`（`sm-server/lib.rs` 里已有构造
    // 范例），后者返回 `Capabilities`。两者都确认过存在，只差接起来。
    todo!("骨架：入队语义已定；待接 TaskQueueService 与配置快照")
}
