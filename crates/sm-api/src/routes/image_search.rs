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
use sm_service::discovery::image_search::{ImageSearchPage, ImageSearchSessionPage};
use sm_service::discovery::image_search_reset::{ImageSearchResetResult, ImageSearchResetService};
use sm_service::discovery::image_search_space::normalize_image_search_query;
use sm_service::discovery::plot_image_search::{PlotImageSearchPage, PlotImageSearchSessionPage};
use sm_service::system::task_queue::TaskQueueService;

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

impl From<ImageSearchSessionPage> for ImageSearchSessionResponse {
    /// 服务层的「会话 + 第一页」与本 DTO **同构**，所以是拆开而不是转换。
    fn from(value: ImageSearchSessionPage) -> Self {
        Self {
            session_id: value.session_id,
            page: value.page,
        }
    }
}

/// 剧情图搜会话分页响应（对应上游 `MoviePlotImageSearchSessionPageResource`）。
#[derive(Debug, Serialize)]
pub struct PlotImageSearchSessionResponse {
    pub session_id: String,
    #[serde(flatten)]
    pub page: PlotImageSearchPage,
}

impl From<PlotImageSearchSessionPage> for PlotImageSearchSessionResponse {
    /// 服务层那个是**平铺**的（没有嵌套的 `page`），所以要拆开重组。
    ///
    /// ⚠️ 这里丢掉了 `status` / `page_size` —— 见 `get_plot_image_search_results`
    /// 那条注释记的契约偏差（上游 resource 还有 `expires_at`）。
    fn from(value: PlotImageSearchSessionPage) -> Self {
        Self {
            session_id: value.session_id,
            page: PlotImageSearchPage {
                items: value.items,
                next_cursor: value.next_cursor,
            },
        }
    }
}

/// `POST /image-search/sessions` —— multipart：文件 + 过滤条件。
async fn create_image_search_session(
    _user: CurrentUser,
    RequireImageSearch(service): RequireImageSearch,
    multipart: crate::extract::Multipart,
) -> Result<Json<ImageSearchSessionResponse>, ErrorResponse> {
    // 能力检查在提取器里，**先于** `Multipart` 解析 body —— 所以未启用时
    // 连「缺 file 字段 / 错误 content-type」都不会被看到，得到 409 而不是
    // 422。顺序理由见 `get_image_search_results`。
    let (image_bytes, filters) = read_query_image(multipart).await?;
    let image_bytes = normalize_image_search_query(&image_bytes).map_err(as_bad_request)?;
    let page = service
        .create_session_and_first_page(
            &image_bytes,
            filters.page_size,
            filters.movie_ids.as_deref(),
            filters.exclude_movie_ids.as_deref(),
            filters.score_threshold,
        )
        .await?;
    Ok(Json(ImageSearchSessionResponse::from(page)))
}

/// `GET /image-search/sessions/{session_id}/results`
///
/// 上游：`LookupError -> 404`、`ValueError -> 400`（`:60-72`）。
async fn get_image_search_results(
    _user: CurrentUser,
    RequireImageSearch(service): RequireImageSearch,
    Path(session_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ResultsQuery>,
) -> Result<Json<ImageSearchSessionResponse>, ErrorResponse> {
    // ★ `None` = 未启用 → **409**，不是空列表。
    //
    // 上游把 `require_image_search()` 挂成 router 级依赖（`image_search.py:30`），
    // 未启用时六个端点统一 `ApiError(409, "feature_disabled", ...)`。这里没有
    // router 级依赖机制，所以在每个 handler 里显式取一次 —— 判据是「组合根
    // 有没有挂上服务」，与「配置开关」等价（组合根就是照开关挂的）。
    //
    // 返回空列表会让用户以为「搜过了、没有」，而真相是「这台机器没开」。
    // 能力检查由 [`RequireImageSearch`] 提取器完成 —— 它在参数列表里排在
    // `Query` **之前**，所以未启用时 409 先于「空 cursor」的 422。
    //
    // 顺序照 FastAPI 的 `solve_dependencies`（`dependencies/utils.py`）：
    // 它**先**跑 `dependant.dependencies` 的循环（`require_image_search` 就在
    // 这一层），`request_params_to_args` 在循环**之后**。而
    // `require_image_search` 是直接 `raise ApiError(409, ...)`（不是累积进
    // `errors` 列表），所以一旦未启用就立刻返回，参数校验根本没机会跑。
    //
    // 我先前把这里写成「422 先于 409」，理由是「pydantic 校验在依赖之前」
    // —— 那个理由是错的。`Query(min_length=1)` 属于 endpoint 自己的
    // `dependant.query_params`，在依赖之后解析。
    //
    // 上游 `cursor: Query(min_length=1)` —— 空串是 **422 校验失败**，
    // 不是「没有 cursor」。而 `Option<String>` 分不出「没传」与「传了空串」
    // （`?cursor=` 会解成 `Some("")`），所以在这里显式判一次。
    if let Some(cursor) = query.cursor.as_deref() {
        if cursor.is_empty() {
            return Err(crate::error::validation_error(
                "cursor: String should have at least 1 character",
            ));
        }
    }
    // 404（会话不存在/已过期）与 400（cursor 坏了）都由服务层映射好了 ——
    // `list_results` 内部用 `not_found_with` 与 `ServiceError::validation`，
    // 这里**不能再**把 400 归一成 422（模块文档第 29-35 行）。
    let page = service
        .list_results(&session_id, query.cursor.as_deref())
        .await?;
    // `session_id` 在响应体里也要有（上游 `ImageSearchSessionPageResource`
    // 就带着它），而 `list_results` 只返回页 —— 用路径参数那一份。
    Ok(Json(ImageSearchSessionResponse { session_id, page }))
}

/// `POST /image-search/text-sessions` —— **form 编码**。
///
/// ⚠️ 骨架期签名收的是 `Json<SearchFilters>` —— **传输形态就错了**（上游是
/// `Form`，字段还是 CSV 字符串，见模块文档）。实现时换成
/// [`crate::extract::Form`] + 表单专用 DTO，别沿用 `SearchFilters`。
async fn create_text_image_search_session(
    _user: CurrentUser,
    RequireImageSearch(service): RequireImageSearch,
    form: crate::extract::Form<TextSessionForm>,
) -> Result<Json<ImageSearchSessionResponse>, ErrorResponse> {
    // 能力检查在提取器里 —— 理由见 `create_image_search_session`。
    let text = require_non_empty_text(&form.0.text)?;
    let filters = form.0.filters.parse()?;
    let page = service
        .create_text_session_and_first_page(
            text,
            filters.page_size,
            filters.movie_ids.as_deref(),
            filters.exclude_movie_ids.as_deref(),
            filters.score_threshold,
        )
        .await?;
    Ok(Json(ImageSearchSessionResponse::from(page)))
}

/// `POST /image-search/plot-sessions` —— multipart。
async fn create_plot_image_search_session(
    _user: CurrentUser,
    RequirePlotImageSearch(service): RequirePlotImageSearch,
    multipart: crate::extract::Multipart,
) -> Result<Json<PlotImageSearchSessionResponse>, ErrorResponse> {
    // 能力检查在提取器里 —— 理由见 `create_image_search_session`。
    let (image_bytes, filters) = read_query_image(multipart).await?;
    let image_bytes = normalize_image_search_query(&image_bytes).map_err(as_bad_request)?;
    let page = service
        .create_session_and_first_page(
            &image_bytes,
            filters.page_size,
            filters.movie_ids.as_deref(),
            filters.exclude_movie_ids.as_deref(),
            filters.score_threshold,
        )
        .await?;
    Ok(Json(PlotImageSearchSessionResponse::from(page)))
}

/// `GET /image-search/plot-sessions/{session_id}/results`
async fn get_plot_image_search_results(
    _user: CurrentUser,
    RequirePlotImageSearch(service): RequirePlotImageSearch,
    Path(session_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ResultsQuery>,
) -> Result<Json<PlotImageSearchSessionResponse>, ErrorResponse> {
    // 能力检查在提取器里 —— 理由与顺序见 `get_image_search_results`。
    // 空 cursor → 422（上游 `Query(min_length=1)`）。
    if let Some(cursor) = query.cursor.as_deref() {
        if cursor.is_empty() {
            return Err(crate::error::validation_error(
                "cursor: String should have at least 1 character",
            ));
        }
    }
    // `list_results` 返回的是 `PlotImageSearchSessionPage`（平铺的会话页），
    // 而本端点的响应体是 `{session_id, page}` —— 拆出 page 那一半。
    //
    // ⚠️ **已知契约偏差**（记在 handoff）：上游
    // `MoviePlotImageSearchSessionPageResource` 还有 `status` / `page_size` /
    // `expires_at` 三个字段，本仓两个 DTO 都只有 `items` + `next_cursor`。
    // 图搜那条同样少（服务层 `list_results` 就只返回 `ImageSearchPage`）——
    // 两边一致，但都与上游不齐。留到接四个建会话端点时一并处理：那时
    // `ImageSearchSessionPage` 也要凑齐同一组字段，改一处能一起验。
    let session_page = service
        .list_results(&session_id, query.cursor.as_deref())
        .await?;
    Ok(Json(PlotImageSearchSessionResponse {
        page: PlotImageSearchPage {
            items: session_page.items,
            next_cursor: session_page.next_cursor,
        },
        session_id,
    }))
}

/// `POST /image-search/plot-text-sessions` —— **form 编码**。
///
/// ⚠️ 同 [`create_text_image_search_session`]：骨架的 `Json<SearchFilters>`
/// 传输形态错了，实现时换 `EnvelopeForm` + 表单 DTO。
async fn create_plot_text_search_session(
    _user: CurrentUser,
    RequirePlotImageSearch(service): RequirePlotImageSearch,
    form: crate::extract::Form<TextSessionForm>,
) -> Result<Json<PlotImageSearchSessionResponse>, ErrorResponse> {
    // 能力检查在提取器里，先于 `Form` 解析 —— 理由见 `create_image_search_session`。
    //
    // 上游 `text: Form(min_length=1)` —— 空串是 **422**。服务层那条
    // `image_search_empty_text` 挡的是「去空白后为空」，而 pydantic 的
    // `min_length=1` 只挡空串；两个都要，顺序是先这里。
    let text = require_non_empty_text(&form.0.text)?;
    let filters = form.0.filters.parse()?;
    let page = service
        .create_text_session_and_first_page(
            text,
            filters.page_size,
            filters.movie_ids.as_deref(),
            filters.exclude_movie_ids.as_deref(),
            filters.score_threshold,
        )
        .await?;
    Ok(Json(PlotImageSearchSessionResponse::from(page)))
}

// ============================================================ 四个建会话端点共用

/// 已解析的过滤参数。
struct SessionFilters {
    page_size: Option<i64>,
    movie_ids: Option<Vec<i64>>,
    exclude_movie_ids: Option<Vec<i64>>,
    score_threshold: Option<f64>,
}

/// 过滤参数的**传输形态**（`movie_ids` 是 CSV 字符串）。
///
/// multipart 与 form 两条路径共用：前者逐键从字段表里取，后者由 serde 直接
/// 反序列化。上游四个端点传的都是这几个键，形状一致。
#[derive(Debug, Default, Deserialize)]
struct RawFilters {
    #[serde(default)]
    page_size: Option<i64>,
    #[serde(default)]
    movie_ids: Option<String>,
    #[serde(default)]
    exclude_movie_ids: Option<String>,
    #[serde(default)]
    score_threshold: Option<f64>,
}

impl RawFilters {
    /// 解析成 [`SessionFilters`]。CSV 不合法 → **422**。
    fn parse(self) -> Result<SessionFilters, ErrorResponse> {
        Ok(SessionFilters {
            page_size: self.page_size,
            movie_ids: parse_csv_positive_ints(self.movie_ids.as_deref(), "movie_ids")?,
            exclude_movie_ids: parse_csv_positive_ints(
                self.exclude_movie_ids.as_deref(),
                "exclude_movie_ids",
            )?,
            score_threshold: self.score_threshold,
        })
    }

    /// 从 multipart 的文本字段表构造。
    ///
    /// `page_size` / `score_threshold` 解析失败 → **422**：上游这两个是
    /// `int` / `float` 类型的 `Form(...)`，pydantic 解析不了就是
    /// `RequestValidationError`（422），不是业务错误。
    fn from_fields(
        fields: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ErrorResponse> {
        let parse_i64 = |key: &str| -> Result<Option<i64>, ErrorResponse> {
            let Some(raw) = fields.get(key) else {
                return Ok(None);
            };
            raw.trim().parse::<i64>().map(Some).map_err(|_| {
                crate::error::validation_error(&format!("{key}: value is not a valid integer"))
            })
        };
        let parse_f64 = |key: &str| -> Result<Option<f64>, ErrorResponse> {
            let Some(raw) = fields.get(key) else {
                return Ok(None);
            };
            raw.trim().parse::<f64>().map(Some).map_err(|_| {
                crate::error::validation_error(&format!("{key}: value is not a valid number"))
            })
        };
        Ok(Self {
            page_size: parse_i64("page_size")?,
            score_threshold: parse_f64("score_threshold")?,
            movie_ids: fields.get("movie_ids").cloned(),
            exclude_movie_ids: fields.get("exclude_movie_ids").cloned(),
        })
    }
}

/// `POST /image-search/text-sessions` 与 `/plot-text-sessions` 的表单体。
#[derive(Debug, Deserialize)]
struct TextSessionForm {
    text: String,
    #[serde(flatten)]
    filters: RawFilters,
}

/// 上游 `text: Form(min_length=1)` —— 空串是 **422**。
///
/// 服务层那条 `image_search_empty_text` 挡的是**去空白后为空**（`"   "`），
/// 而 pydantic 的 `min_length=1` 只挡**空串**。两者都要，且这个先判：
/// 顺序反了的话空串会落到服务层那条上，得到 400 而不是 422。
fn require_non_empty_text(text: &str) -> Result<&str, ErrorResponse> {
    if text.is_empty() {
        return Err(crate::error::validation_error(
            "text: String should have at least 1 character",
        ));
    }
    Ok(text)
}

/// 未启用图搜。**409** `feature_disabled`，不是空列表。
///
/// 上游的 `require_image_search` 是 **router 依赖**（`image_search.py:30`），
/// 而 FastAPI 的 `solve_dependencies` **先**跑依赖、`request_params_to_args`
/// 与 body 解析都在之后。axum 没有 router 依赖这层，但**提取器按参数顺序
/// 解析** —— 把它做成 [`axum::extract::FromRequestParts`] 并放在读 body 的
/// 提取器（`Multipart` / `Form`）**之前**，就等价于上游那个位置。
///
/// 写成 handler 体内的一行则不行：那时 `Multipart` 已经解析完了，
/// 「未启用 + 错误 content-type」会得到 422 而不是 409。
pub struct RequireImageSearch(
    pub std::sync::Arc<sm_service::discovery::image_search::ImageSearchService>,
);

impl axum::extract::FromRequestParts<AppState> for RequireImageSearch {
    type Rejection = ErrorResponse;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        state
            .image_search()
            .cloned()
            .map(Self)
            .ok_or_else(feature_disabled)
    }
}

/// 剧情图搜版。位置与理由同 [`RequireImageSearch`]。
pub struct RequirePlotImageSearch(
    pub std::sync::Arc<sm_service::discovery::plot_image_search::MoviePlotImageSearchService>,
);

impl axum::extract::FromRequestParts<AppState> for RequirePlotImageSearch {
    type Rejection = ErrorResponse;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        state
            .plot_image_search()
            .cloned()
            .map(Self)
            .ok_or_else(feature_disabled)
    }
}

/// 未启用图搜。**409** `feature_disabled`，不是空列表。
///
/// 上游把 `require_image_search()` 挂成 **router 级依赖**
/// （`image_search.py:30`），六个端点统一拒绝。本仓没有 router 级依赖，
/// 所以每个 handler 显式取一次。
fn feature_disabled() -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::CONFLICT,
        "feature_disabled",
        "当前服务器未启用图片与文字搜图",
    )
}

/// 读 multipart 里的查询图与过滤参数。
///
/// 上游 `_read_image_search_query`（`image_search.py:121-125`）：
///
/// ```python
/// image_bytes = await file.read()
/// if not image_bytes:
///     raise ValueError("Uploaded file is empty")
/// return normalize_image_search_query(image_bytes)
/// ```
///
/// 两点：**没有 `file` 字段**是 422（上游 `File(...)` 是必填），**空文件**
/// 是 400（上游 `ValueError -> 400`）。两者码不同 —— 前者是「请求没拼对」，
/// 后者是「文件本身不对」。
async fn read_query_image(
    mut multipart: crate::extract::Multipart,
) -> Result<(Vec<u8>, SessionFilters), ErrorResponse> {
    let form = multipart.receive_file_with_fields().await?;
    let Some(_file) = form.file else {
        return Err(crate::error::validation_error("file: field required"));
    };
    if form.file_bytes.is_empty() {
        // 400，且错误码用服务层那一条的（`create_*_session` 内部也是这个判据），
        // 免得同一个「空文件」在两条路径上给出两个码。
        return Err(ErrorResponse::new(
            StatusCode::BAD_REQUEST,
            "image_search_empty_image",
            "Uploaded file is empty",
        ));
    }
    Ok((
        form.file_bytes,
        RawFilters::from_fields(&form.fields)?.parse()?,
    ))
}

/// 归一化失败 → **400**。
///
/// 上游 `normalize_image_search_query` 的 `ValueError` 落在
/// `except ValueError -> HTTPException(400)` 那条上（`image_search.py:56-57`）。
/// 服务层给的是 `ServiceError`，这里按上游的 400 落，不沿用它的码。
fn as_bad_request(error: sm_service::error::ServiceError) -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::BAD_REQUEST,
        "image_search_invalid_image",
        "uploaded image is invalid or unsupported",
    )
    // 原始码留在日志里，不进响应体（上游只回 `str(exc)`）。
    .with_details(details_of("reason", error.code()))
}

fn details_of(key: &str, value: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert(key.to_owned(), serde_json::Value::from(value));
    map
}

/// CSV 正整数解析。对应上游 `parse_csv_positive_ints`
/// （`api/routers/_utils.py:21-36`）。
///
/// # ★ 空串是 **422 报错**，不是「空列表」
///
/// 上游：
///
/// ```python
/// parts = [part.strip() for part in raw.split(",")]
/// if not parts or any(not part for part in parts):
///     raise ApiError(422, error_code, "Invalid filter value", {field_name: raw})
/// ```
///
/// `""` 会拆出 `[""]`，`any(not part)` 为真 → **报错**。骨架期这里返回
/// `Some(vec![])`,理由是「别把『排除全部』变成『不过滤』」—— 但那条理由
/// 是**自己造的语义**：上游根本不接受空串，客户端传空串是**客户端的错**
/// （多半是模板拼了个空变量）。静默当成「排除全部」会让用户看到「一个
/// 结果都没有」而不知道为什么。
///
/// # ★ 错误码是 **422**，不是 400
///
/// 上游抛的是 `ApiError(422, ...)` —— 它是**校验**错误，不是业务错误，所以
/// 与「图片无效」那条 400 **不同码**。客户端据此区分「改改参数」（422）与
/// 「换个文件」（400）。骨架期这里写的是 400。
pub fn parse_csv_positive_ints(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<Vec<i64>>, ErrorResponse> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for piece in raw.split(',') {
        match piece.trim().parse::<i64>() {
            // 空项在这里落进 `_`：`"".parse()` 失败，`"1,,2"` 的中间项也是。
            Ok(value) if value > 0 => out.push(value),
            _ => return Err(invalid_image_search_filter(field, raw)),
        }
    }
    Ok(Some(out))
}

/// `invalid_image_search_filter` 错误。**422**，且 `details` 回显原始串。
fn invalid_image_search_filter(field: &str, raw: &str) -> ErrorResponse {
    let mut details = serde_json::Map::new();
    details.insert(field.to_owned(), serde_json::Value::from(raw));
    ErrorResponse::new(
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_image_search_filter",
        "Invalid filter value",
    )
    .with_details(details)
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
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<(StatusCode, Json<ImageSearchResetResult>), ErrorResponse> {
    // 快照**先取**：它在下面要同时喂给能力检查与入队，而 `snapshot()` 会读盘
    // ——取两次的话，一次「启用」一次「停用」之间配置被改掉，入队就会与检查
    // 的结论不一致（表现为「刚说能用，下一秒排上了队」）。
    let values = state.config().snapshot()?;
    let queue = TaskQueueService::new(state.db());
    let result = ImageSearchResetService::new(queue).reset(&values).await?;
    // **202** 而不是 200：索引是后台重建的，此刻还没清。
    Ok((StatusCode::ACCEPTED, Json(result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 `ErrorResponse` 转成状态码。
    ///
    /// 走 `IntoResponse` 而不是给 `ErrorResponse` 加访问器 —— 后者要为了测试
    /// 给生产类型加方法，而 `IntoResponse` 本来就是它对外表达自己的方式。
    /// 信封的 `code` / `details` 形状由 `error.rs` 那边的测试覆盖。
    fn status_of(error: ErrorResponse) -> StatusCode {
        use axum::response::IntoResponse as _;
        error.into_response().status()
    }

    /// 期望**报错**，否则 panic。返回那个错误。
    fn expect_error(raw: &str) -> ErrorResponse {
        match parse_csv_positive_ints(Some(raw), "movie_ids") {
            Err(error) => error,
            Ok(other) => panic!("{raw} 该报错，实际解析成了 {other:?}"),
        }
    }

    /// ★ 空串是 **422 报错**，不是「空列表」。
    ///
    /// 上游 `""` 会拆出 `[""]`，`any(not part)` 为真 → `ApiError(422, ...)`。
    /// 骨架期这里返回 `Some(vec![])`，把「客户端拼了个空变量」静默变成
    /// 「排除全部」—— 用户看到零结果而不知道为什么。
    #[test]
    fn an_empty_csv_is_422_not_an_empty_list() {
        assert_eq!(
            status_of(expect_error("")),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// 有空项（`1,,2`）同样是 422 —— 上游 `any(not part)` 覆盖这一条。
    #[test]
    fn a_csv_with_a_blank_item_is_422() {
        for raw in ["1,,2", ",", "1,", ",1"] {
            assert_eq!(
                status_of(expect_error(raw)),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{raw}"
            );
        }
    }

    /// 非正整数（0 / 负数 / 非数字）都是 422。
    #[test]
    fn a_csv_with_a_non_positive_int_is_422() {
        for raw in ["0", "-1", "abc", "1.5", "  "] {
            assert_eq!(
                status_of(expect_error(raw)),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{raw}"
            );
        }
    }

    /// 正常解析：`None` 与合法串（**允许空格**，上游 `strip()` 过每一项）。
    #[test]
    fn a_valid_csv_parses() {
        assert_eq!(parse_csv_positive_ints(None, "movie_ids").unwrap(), None);
        assert_eq!(
            parse_csv_positive_ints(Some("1, 2,3"), "movie_ids").unwrap(),
            Some(vec![1, 2, 3])
        );
    }
}
