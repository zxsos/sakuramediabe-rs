//! `POST /movies/subscriptions`、`POST /movies/unsubscriptions`。
//!
//! # 与上游 `src/api/routers/catalog/movies.py` 的对应
//!
//! 上游这个 router 有 21 条路由，本文件落了 **2** 条 —— 其余的分两类：
//! 要影片列表/详情聚合的（`GET ""` / `GET /latest` / `GET /{n}` /
//! `GET /{n}/similar` …），以及要 provider 或插件 ABI 的
//! （`/search/javdb/stream` / `/{n}/metadata-refresh` / `/{n}/reviews` …）。
//!
//! | 上游端点 | 本文件 | 状态 |
//! |---|---|---|
//! | `POST /subscriptions` | `batch_subscribe_movies` | **已落** |
//! | `POST /unsubscriptions` | `batch_unsubscribe_movies` | **已落** |
//! | `PUT` / `DELETE /blacklist` | —— | 未落（要 `MovieOwnershipGateway` 的编排） |
//! | 其余 17 条 | —— | 未落 |
//!
//! # 请求体的两条约束在 handler 里，不在 service 里
//!
//! 上游是 pydantic 的 `Field(min_length=1)` 加逐项 `strip` 非空校验 ——
//! **都发生在 service 之前**，且失败形状是 `RequestValidationError`
//! （422 `validation_error`，不是业务错误码）。所以这里也放在 handler，
//! 用与 [`crate::extract::Json`] 拒绝路径同一个信封形状。
//!
//! # 「番号不存在」不是 422 而是 `skipped`
//!
//! 批量端点里不存在的番号走**部分成功**：200 + `skipped[].reason =
//! "movie_not_found"`。改成 404 会让整批失败，而用户勾了 20 部、其中 1 部
//! 已被删除时，他要的是「另外 19 部订上」。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_core::pagination::Paginated;
use sm_service::catalog::movie::{
    parse_movie_number_query, MovieCollectionMarkResponse, MovieCollectionStatus, MovieListParams,
    MovieNumberParseResult, MovieService, SubscriptionBatchResponse, SubscriptionSkippedItem,
    COLLECTION_TYPE_COLLECTION, COLLECTION_TYPE_SINGLE,
};
use sm_service::error::details_of;

use crate::auth::CurrentUser;
use crate::dto::MovieListItemResource;
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::deser_bool;
use crate::routes::method_not_allowed;
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/movies", get(list_movies).fallback(method_not_allowed))
        .route(
            "/movies/by-series",
            post(list_movies_by_series).fallback(method_not_allowed),
        )
        .route(
            "/movies/subscribed-actors/latest",
            get(list_subscribed_actor_latest_movies).fallback(method_not_allowed),
        )
        .route(
            "/movies/search/parse-number",
            post(parse_movie_number).fallback(method_not_allowed),
        )
        .route(
            "/movies/latest",
            get(list_latest_movies).fallback(method_not_allowed),
        )
        .route(
            "/movies/collection-type",
            patch(mark_collection_type).fallback(method_not_allowed),
        )
        .route(
            "/movies/blacklist",
            put(blacklist_movies)
                .delete(unblacklist_movies)
                .fallback(method_not_allowed),
        )
        .route(
            "/movies/subscriptions",
            post(batch_subscribe_movies).fallback(method_not_allowed),
        )
        .route(
            "/movies/unsubscriptions",
            post(batch_unsubscribe_movies).fallback(method_not_allowed),
        )
        // 三段路径，与上面两条同级的静态路径不冲突（匹配器按段数区分）。
        .route(
            "/movies/{movie_number}/collection-status",
            get(get_collection_status).fallback(method_not_allowed),
        )
}

// ================================================================ 番号解析

/// `POST /movies/search/parse-number` 的请求体（上游 `MovieNumberParseRequest`）。
#[derive(Debug, Clone, Deserialize)]
struct MovieNumberParseRequest {
    query: String,
}

/// 番号解析结果（上游 `MovieNumberParseResponse`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieNumberParseResource {
    /// **strip 之后**的输入。
    pub query: String,
    pub parsed: bool,
    pub movie_number: Option<String>,
    /// 失败时是 `movie_number_not_found`。
    pub reason: Option<String>,
}

impl From<MovieNumberParseResult> for MovieNumberParseResource {
    fn from(value: MovieNumberParseResult) -> Self {
        Self {
            query: value.query,
            parsed: value.parsed,
            movie_number: value.movie_number,
            reason: value.reason,
        }
    }
}

/// 从自由文本里识别番号。**不查库、不会 404** —— 识别不出只是 `parsed: false`。
async fn parse_movie_number(
    _user: CurrentUser,
    EnvelopeJson(payload): EnvelopeJson<MovieNumberParseRequest>,
) -> Result<Json<MovieNumberParseResource>, ErrorResponse> {
    // 上游 `min_length=1` + 逐项 strip 非空 → 422 `validation_error`。
    let normalized = payload.query.trim();
    if normalized.is_empty() {
        return Err(validation_error("query cannot be blank"));
    }
    Ok(Json(parse_movie_number_query(normalized).into()))
}

// ================================================================ 已订阅演员最新影片

async fn list_subscribed_actor_latest_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListLatestMoviesQuery>,
) -> Result<Json<Paginated<MovieListItemResource>>, ErrorResponse> {
    let page = MovieService::new(state.db())
        .list_subscribed_actor_latest_movies(query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state);
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| MovieListItemResource::from_movie_card(card, &secret, now))
        .collect();

    Ok(Json(Paginated::new(
        items,
        query.page,
        query.page_size,
        page.total,
    )))
}

// ================================================================ by-series

/// `POST /movies/by-series` 的请求体（上游 `MovieSeriesListRequest`）。
///
/// # 这个端点的分页**是校验过的**
///
/// `series_id` 与分页都在请求体里（上游是 pydantic 模型，带 `ge=1` /
/// `le=100`），而 `GET /movies` 与 `/movies/latest` 的分页在查询串里、**没有**
/// 约束。三者的来源不同，所以这里要显式校验 —— 不校验会让 `page_size=10000`
/// 直接打到数据库。
#[derive(Debug, Clone, Deserialize)]
struct MovieSeriesListRequest {
    series_id: i32,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

async fn list_movies_by_series(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MovieSeriesListRequest>,
) -> Result<Json<Paginated<MovieListItemResource>>, ErrorResponse> {
    if payload.series_id < 1 {
        return Err(validation_error("series_id 必须 >= 1"));
    }
    if payload.page < 1 {
        return Err(validation_error("page 必须 >= 1"));
    }
    if payload.page_size < 1 || payload.page_size > 100 {
        return Err(validation_error("page_size 必须在 1 到 100 之间"));
    }

    let page = MovieService::new(state.db())
        .list_movies_by_series(
            payload.series_id,
            payload.sort.as_deref(),
            payload.page,
            payload.page_size,
        )
        .await?;

    let secret = signing_secret(&state);
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| MovieListItemResource::from_movie_card(card, &secret, now))
        .collect();

    Ok(Json(Paginated::new(
        items,
        payload.page,
        payload.page_size,
        page.total,
    )))
}

// ================================================================ GET /movies

/// 订阅状态筛选（上游 `MovieListStatus`）。非法值 → 422 `validation_error`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MovieStatusQuery {
    #[default]
    All,
    Subscribed,
    Unsubscribed,
    Playable,
}

impl MovieStatusQuery {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Subscribed => "subscribed",
            Self::Unsubscribed => "unsubscribed",
            Self::Playable => "playable",
        }
    }
}

/// 合集筛选（上游 `MovieCollectionType`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MovieCollectionTypeQuery {
    #[default]
    All,
    Single,
}

impl MovieCollectionTypeQuery {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Single => "single",
        }
    }
}

/// 番号来源筛选（上游 `MovieNumberSource`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MovieNumberSourceQuery {
    #[default]
    All,
    Regular,
    Fc2,
}

impl MovieNumberSourceQuery {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Regular => "regular",
            Self::Fc2 => "fc2",
        }
    }
}

/// 多标签的组合关系（上游 `TagMatchMode`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TagMatchQuery {
    #[default]
    Or,
    And,
}

/// `GET /movies` 的查询参数（上游 `list_movies` 的 15 个筛选位 + 分页）。
///
/// 分页与 `/movies/latest` 一样**不校验**（上游是裸 `int`）。
#[derive(Debug, Deserialize)]
struct ListMoviesQuery {
    #[serde(default)]
    actor_id: Option<i32>,
    /// **逗号分隔的正整数串**（上游 `parse_csv_positive_ints`）。
    #[serde(default)]
    tag_ids: Option<String>,
    #[serde(default)]
    tag_match: TagMatchQuery,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    status: MovieStatusQuery,
    #[serde(default)]
    collection_type: MovieCollectionTypeQuery,
    #[serde(default)]
    number_source: MovieNumberSourceQuery,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    director_name: Option<String>,
    #[serde(default)]
    maker_name: Option<String>,
    #[serde(default)]
    heat_min: Option<i32>,
    #[serde(default)]
    heat_max: Option<i32>,
    #[serde(default)]
    resolution: Option<String>,
    /// pydantic lax 布尔（`?blacklisted=1` 也能解析），见 [`crate::query`]。
    #[serde(default, deserialize_with = "deser_bool")]
    blacklisted: bool,
    /// 原始检索串（切词在 service 层）。
    #[serde(default)]
    query: Option<String>,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

/// 上游 `parse_csv_positive_ints`：空串 / 有空项 / 非正整数都是 422，
/// `details.tag_ids` 回显**原始串**。
fn parse_tag_ids(raw: Option<&str>) -> Result<Vec<i32>, ErrorResponse> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return Err(invalid_movie_filter("tag_ids", raw));
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in parts {
        let Ok(value) = part.parse::<i32>() else {
            return Err(invalid_movie_filter("tag_ids", raw));
        };
        if value <= 0 {
            return Err(invalid_movie_filter("tag_ids", raw));
        }
        values.push(value);
    }
    Ok(values)
}

/// 上游 `parse_optional_exact_text`：strip 后为空是 **422**，
/// 而不是「当作没传」—— 客户端以为空串会被忽略，实际会筛出空结果。
pub(crate) fn parse_exact_text(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<String>, ErrorResponse> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let normalized = raw.trim();
    if normalized.is_empty() {
        return Err(invalid_movie_filter(field, raw));
    }
    Ok(Some(normalized.to_owned()))
}

/// 422 `invalid_movie_filter`，`details.<field>` 回显原始输入。
fn invalid_movie_filter(field: &str, raw: &str) -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_movie_filter",
        "Invalid filter value",
    )
    .with_details(details_of(field, raw))
}

async fn list_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListMoviesQuery>,
) -> Result<Json<Paginated<MovieListItemResource>>, ErrorResponse> {
    // 枚举类参数由 serde 反序列化（非法值 → 422 `validation_error`）；
    // 下面这几个的 422 是 `invalid_movie_filter` —— 两套码上游也是分开的。
    let params = MovieListParams {
        actor_id: query.actor_id,
        tag_ids: parse_tag_ids(query.tag_ids.as_deref())?,
        tag_match_all: query.tag_match == TagMatchQuery::And,
        year: query.year,
        status: query.status.as_str().to_owned(),
        collection_type: query.collection_type.as_str().to_owned(),
        number_source: query.number_source.as_str().to_owned(),
        sort: query.sort.clone(),
        director_name: parse_exact_text(query.director_name.as_deref(), "director_name")?,
        maker_name: parse_exact_text(query.maker_name.as_deref(), "maker_name")?,
        heat_min: query.heat_min,
        heat_max: query.heat_max,
        resolution: query.resolution.clone(),
        blacklisted: query.blacklisted,
        query: query.query.clone(),
    };

    let page = MovieService::new(state.db())
        .list_movies(&params, query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state);
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| MovieListItemResource::from_movie_card(card, &secret, now))
        .collect();

    Ok(Json(Paginated::new(
        items,
        query.page,
        query.page_size,
        page.total,
    )))
}

/// `GET /movies/latest` 的查询参数。与上游一样是**裸 `int`**（无 `ge` / `le`）。
#[derive(Debug, Deserialize)]
struct ListLatestMoviesQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

pub(crate) fn default_page() -> i64 {
    1
}

pub(crate) fn default_page_size() -> i64 {
    20
}

/// 「最新到货」：**只列有本地媒体的影片**，按最近一次媒体入库时间倒序。
///
/// 不是「最新影片」—— 没有本地文件的不出现，这是它和 `GET /movies` 最大的
/// 区别（后者是全部影片）。
async fn list_latest_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListLatestMoviesQuery>,
) -> Result<Json<Paginated<MovieListItemResource>>, ErrorResponse> {
    let page = MovieService::new(state.db())
        .list_latest_movies(query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state);
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| MovieListItemResource::from_movie_card(card, &secret, now))
        .collect();

    Ok(Json(Paginated::new(
        items,
        query.page,
        query.page_size,
        page.total,
    )))
}

/// 批量请求体。`POST /subscriptions` 与 `POST /unsubscriptions` 共用。
#[derive(Debug, Clone, Deserialize)]
struct MovieNumbersRequest {
    movie_numbers: Vec<String>,
}

/// 被跳过的一条（上游 `MovieSubscriptionSkippedItem`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieSubscriptionSkippedItemResource {
    /// **用户输入的那个番号**，不是归一后的大写形态 —— 客户端用它把
    /// 结果标回勾选的那一行。
    pub movie_number: String,
    /// `movie_not_found` / `blacklisted` / `has_media`。
    pub reason: String,
}

/// 批量订阅/退订的结果（上游 `MovieSubscriptionBatchResponse`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieSubscriptionBatchResource {
    /// 请求里给了**几项**（含空串与重复项）。
    pub requested_count: i64,
    pub updated_count: i64,
    pub skipped_count: i64,
    pub skipped: Vec<MovieSubscriptionSkippedItemResource>,
}

impl From<SubscriptionSkippedItem> for MovieSubscriptionSkippedItemResource {
    fn from(value: SubscriptionSkippedItem) -> Self {
        Self {
            movie_number: value.movie_number,
            reason: value.reason,
        }
    }
}

impl From<SubscriptionBatchResponse> for MovieSubscriptionBatchResource {
    fn from(value: SubscriptionBatchResponse) -> Self {
        Self {
            requested_count: value.requested_count,
            updated_count: value.updated_count,
            skipped_count: value.skipped_count,
            skipped: value.skipped.into_iter().map(Into::into).collect(),
        }
    }
}

/// 上游的两条 pydantic 约束。
fn validate_movie_numbers(values: &[String]) -> Result<(), ErrorResponse> {
    if values.is_empty() {
        return Err(validation_error("movie_numbers 至少需要一项"));
    }
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(validation_error("movie_numbers item cannot be blank"));
    }
    Ok(())
}

/// 与 [`crate::extract::Json`] 的拒绝路径同一个信封形状。
fn validation_error(detail: &str) -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_error",
        "Request validation failed",
    )
    .with_details(details_of("detail", detail))
}

async fn batch_subscribe_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MovieNumbersRequest>,
) -> Result<Json<MovieSubscriptionBatchResource>, ErrorResponse> {
    validate_movie_numbers(&payload.movie_numbers)?;
    let response = MovieService::new(state.db())
        .batch_set_subscription(&payload.movie_numbers)
        .await?;
    Ok(Json(response.into()))
}

async fn batch_unsubscribe_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MovieNumbersRequest>,
) -> Result<Json<MovieSubscriptionBatchResource>, ErrorResponse> {
    validate_movie_numbers(&payload.movie_numbers)?;
    let response = MovieService::new(state.db())
        .batch_unsubscribe_movies(&payload.movie_numbers)
        .await?;
    Ok(Json(response.into()))
}

// ================================================================ 合集标记

/// 影片合集状态（上游 `MovieCollectionStatusResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieCollectionStatusResource {
    /// **库内规范番号**，不是请求里那个写法。
    pub movie_number: String,
    pub is_collection: bool,
}

/// 批量标记合集的结果（上游 `MovieCollectionMarkResponse`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieCollectionMarkResource {
    pub requested_count: i64,
    pub updated_count: i64,
}

impl From<MovieCollectionStatus> for MovieCollectionStatusResource {
    fn from(value: MovieCollectionStatus) -> Self {
        Self {
            movie_number: value.movie_number,
            is_collection: value.is_collection,
        }
    }
}

impl From<MovieCollectionMarkResponse> for MovieCollectionMarkResource {
    fn from(value: MovieCollectionMarkResponse) -> Self {
        Self {
            requested_count: value.requested_count,
            updated_count: value.updated_count,
        }
    }
}

/// `PATCH /movies/collection-type` 的请求体
/// （上游 `MovieCollectionMarkRequest`）。
#[derive(Debug, Clone, Deserialize)]
struct MovieCollectionMarkRequest {
    movie_numbers: Vec<String>,
    /// 只有 `collection` / `single` 两个取值（上游是枚举，非法值 422）。
    collection_type: String,
}

#[derive(Debug, Deserialize)]
struct MovieNumberPath {
    movie_number: String,
}

async fn get_collection_status(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<MovieNumberPath>,
) -> Result<Json<MovieCollectionStatusResource>, ErrorResponse> {
    let status = MovieService::new(state.db())
        .get_collection_status(&path.movie_number)
        .await?;
    Ok(Json(status.into()))
}

async fn mark_collection_type(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MovieCollectionMarkRequest>,
) -> Result<Json<MovieCollectionMarkResource>, ErrorResponse> {
    validate_movie_numbers(&payload.movie_numbers)?;
    if payload.collection_type != COLLECTION_TYPE_COLLECTION
        && payload.collection_type != COLLECTION_TYPE_SINGLE
    {
        return Err(validation_error(
            "collection_type 必须是 collection 或 single",
        ));
    }
    let response = MovieService::new(state.db())
        .mark_collection_type(&payload.movie_numbers, &payload.collection_type)
        .await?;
    Ok(Json(response.into()))
}

// ================================================================ 黑名单

/// `PUT` / `DELETE /movies/blacklist` 的请求体（上游 `MovieBlacklistBatchRequest`）。
#[derive(Debug, Clone, Deserialize)]
struct MovieBlacklistRequest {
    movie_numbers: Vec<String>,
}

/// 上游这一条有 `max_length=1000` —— 订阅/退订那两个端点**没有**。
const BLACKLIST_MAX_ITEMS: usize = 1000;

/// 上游的三条 pydantic 约束：至少一项、最多 1000 项、逐项 strip 后非空。
///
/// 注意上限**只在这个端点**上：把它加到订阅端点会是行为变更。
fn validate_blacklist(values: &[String]) -> Result<(), ErrorResponse> {
    if values.is_empty() {
        return Err(validation_error("movie_numbers 至少需要一项"));
    }
    if values.len() > BLACKLIST_MAX_ITEMS {
        return Err(validation_error("movie_numbers 最多 1000 项"));
    }
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(validation_error("movie_numbers item cannot be blank"));
    }
    Ok(())
}

async fn blacklist_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MovieBlacklistRequest>,
) -> Result<StatusCode, ErrorResponse> {
    validate_blacklist(&payload.movie_numbers)?;
    MovieService::new(state.db())
        .set_blacklisted(&payload.movie_numbers, true)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unblacklist_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MovieBlacklistRequest>,
) -> Result<StatusCode, ErrorResponse> {
    validate_blacklist(&payload.movie_numbers)?;
    MovieService::new(state.db())
        .set_blacklisted(&payload.movie_numbers, false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
