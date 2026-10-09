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

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_core::pagination::Paginated;
use sm_db::repo::recommendation::MovieFeatureRepository;
use sm_service::catalog::actor::ActorService;
use sm_service::catalog::movie::{
    parse_movie_number_query, MovieCard, MovieCollectionMarkResponse, MovieCollectionStatus,
    MovieListParams, MovieNumberParseResult, MovieService, SubscriptionBatchResponse,
    SubscriptionSkippedItem, COLLECTION_TYPE_COLLECTION, COLLECTION_TYPE_SINGLE,
};
use sm_service::catalog::movie_subtitle::MovieSubtitleService;
use sm_service::catalog::movie_task::MovieTaskService;
use sm_service::discovery::recommendation::MovieRecommendationService;
use sm_service::error::details_of;
use sm_service::system::jobs::ManualJobTriggerResponse;

use crate::auth::CurrentUser;
use crate::dto::{ActorResource, MovieListItemResource, TagResource};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::{deser_bool, one, twenty};
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
        // ★ 单片端点全部在 `/movies/{movie_number}` 这**一段**上。
        //
        // ⚠️ 注册顺序**不**决定匹配优先级（axum 的路径匹配器按段数与字面量
        // 优先），所以 `"/movies/{movie_number}"` 不会吃掉
        // `"/movies/{movie_number}/subtitles"` —— 段数不同。
        //
        // 但**别**把 `"/movies/{movie_number}"` 写成 `"/movies/{*rest}"`：
        // 那样它会匹配任意深度，`/movies/latest` 这类静态路径就再也匹配不到了。
        .route(
            "/movies/{movie_number}",
            get(get_movie_detail).fallback(method_not_allowed),
        )
        .route(
            "/movies/{movie_number}/reviews",
            get(get_movie_reviews).fallback(method_not_allowed),
        )
        .route(
            "/movies/{movie_number}/subtitles",
            get(get_movie_subtitles).fallback(method_not_allowed),
        )
        .route(
            "/movies/{movie_number}/similar",
            get(list_similar_movies).fallback(method_not_allowed),
        )
        .route(
            "/movies/{movie_number}/merged-playback",
            get(get_merged_playback).fallback(method_not_allowed),
        )
        .route(
            "/movies/{movie_number}/metadata-refresh",
            post(refresh_movie_metadata).fallback(method_not_allowed),
        )
        .route(
            "/movies/{movie_number}/heat-recompute",
            // ★ 202 而非 200 —— 重算要扫全表，是长任务。
            post(recompute_movie_heat).fallback(method_not_allowed),
        )
        // ★ PUT / DELETE，不是 POST。上游用 RESTful 动词表达订阅开关。
        .route(
            "/movies/{movie_number}/subscription",
            // 204 且**无 body**：见两个 handler 的文档。
            put(subscribe_movie)
                .delete(unsubscribe_movie)
                .fallback(method_not_allowed),
        )
        // 两条 SSE 流。`include_in_schema` 由 axum 侧不管，路径照铺。
        .route(
            "/movies/search/javdb/stream",
            post(search_javdb_movies_stream).fallback(method_not_allowed),
        )
        .route(
            "/movies/series/{series_id}/javdb/import/stream",
            post(import_series_movies_stream).fallback(method_not_allowed),
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

    let secret = signing_secret(&state)?;
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

    let secret = signing_secret(&state)?;
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

    let secret = signing_secret(&state)?;
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

    let secret = signing_secret(&state)?;
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
///
/// 现在用的是 `crate::error` 里**公开**的那一份 —— 图搜的 `cursor` 校验也要它，
/// 与其复制第二份不如共享。
fn validation_error(detail: &str) -> ErrorResponse {
    crate::error::validation_error(detail)
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

// ================================================================ 单片端点（11 个）

/// `GET /movies/{movie_number}` —— 影片详情。
///
/// # 它是**三个域的汇合点**，这也是它曾被列为「卡死」的原因
///
/// 响应含：影片基本信息（`catalog`）+ 媒体与进度（`playback`）+ 打点
/// （`playback`）+ 榜单名次（`discovery::ranking`）。三域都铺完后才铺它。
async fn get_movie_detail(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let detail = MovieService::new(state.db())
        .get_movie_detail(&movie_number)
        .await?;

    // 演员：`ActorResource` 要签名头像，所以装配只能在这一层做（服务层不能
    // 反向依赖 `sm-api`）。逐个取 `ActorView` —— 一部片的演员是十几位以内。
    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let actor_service = ActorService::new(state.db());
    let mut actors = Vec::with_capacity(detail.actor_ids.len());
    for actor_id in &detail.actor_ids {
        let view = actor_service.detail(*actor_id).await?;
        actors.push(ActorResource::from_view(&view, &secret, now));
    }

    let tags: Vec<TagResource> = detail
        .tags
        .iter()
        .map(|(tag_id, name)| TagResource {
            tag_id: *tag_id,
            name: name.clone(),
        })
        .collect();
    let playlists: Vec<serde_json::Value> = detail
        .playlists
        .iter()
        .map(|(playlist_id, name)| serde_json::json!({ "playlist_id": playlist_id, "name": name }))
        .collect();

    // 基座用**列表项资源**：上游 `MovieDetailResource` 就是列表项 + 子资源，
    // 而 `MovieListItemResource` 是本仓唯一已经过对拍验证的影片资源形状。
    // （服务层的 `Movie` 模型不是 `Serialize`，不能直接 `to_value`。）
    let card = MovieService::new(state.db())
        .load_cards(&[detail.movie.id])
        .await?
        .into_iter()
        .next();
    let mut object = match card {
        Some(card) => {
            serde_json::to_value(MovieListItemResource::from_movie_card(&card, &secret, now))
                .map_err(|error| {
                    ErrorResponse::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        format!("影片详情序列化失败：{error}"),
                    )
                })?
        }
        None => {
            return Err(ErrorResponse::new(
                StatusCode::NOT_FOUND,
                "movie_not_found",
                "影片不存在",
            ))
        }
    };

    // 媒体：`MediaSummary` 不是 `Serialize`（它是服务层的投影），手工投影出
    // 详情页要的那几个字段 —— 不含 `storage_ref`（可能含凭据）与
    // `video_info`（可能很大），理由与 `MediaSummary` 的文档一致。
    let media_items: Vec<serde_json::Value> = detail
        .media_items
        .iter()
        .map(|item| {
            serde_json::json!({
                "media_id": item.media_id,
                "library_id": item.library_id,
                "library_name": item.library_name,
                "provider_key": item.provider_key,
                "file_name": item.file_name,
                "resolution": item.resolution,
                "file_size_bytes": item.file_size_bytes,
                "duration_seconds": item.duration_seconds,
            })
        })
        .collect();

    object["actors"] = serde_json::to_value(&actors).unwrap_or_default();
    object["tags"] = serde_json::to_value(&tags).unwrap_or_default();
    object["media_items"] = serde_json::Value::Array(media_items);
    object["media_count"] = serde_json::Value::from(detail.media_count);
    object["can_play"] = serde_json::Value::from(detail.can_play);
    object["rankings"] = serde_json::to_value(&detail.rankings).unwrap_or_default();
    object["playlists"] = serde_json::Value::Array(playlists);
    // ★ `plot_images` / `merge_playback_candidates` **不带这两个键**：
    // 它们是「本仓还没接」而不是「这部片没有」，给空数组会让客户端误判。
    Ok(Json(object))
}

/// `GET /movies/{movie_number}/reviews` 的查询参数。
///
/// # `page_size` **只有下界没有上界**
///
/// 上游 `Query(default=20, ge=1)` —— **无 `le`**。所以 `page_size=10000` 合法。
/// 与 daily/hot-actress 的 `ge=1, le=100` 不同，**别**复用那个有上界的结构。
// 三个字段都还没接上 —— handler 体是 `todo!()`，但它们是**契约的一部分**
// （客户端会传），所以不能删。落地后删掉这行 allow。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct MovieReviewQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
    /// 排序。默认 `recently`。
    #[serde(default)]
    sort: Option<String>,
}

/// `GET /movies/{movie_number}/reviews`
///
/// ⚠️ 响应是 **list 而非分页对象**（`list[JavdbMovieReviewResource]`）——
/// 上游给了分页参数却返回裸列表。**照抄**，别「修正」成 `PageResponse`。
async fn get_movie_reviews(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
    EnvelopeQuery(query): EnvelopeQuery<MovieReviewQuery>,
) -> Result<Json<Vec<serde_json::Value>>, ErrorResponse> {
    let _ = (movie_number, query);
    todo!("骨架：接 MovieService::get_movie_reviews；响应是裸 list 不是分页对象")
}

/// `GET /movies/{movie_number}/subtitles`
///
/// 服务层见 [`sm_service::catalog::movie_subtitle`]。两处不变量在那边：
/// 10 MiB 上限先 stat 再读、路径逃逸校验在 canonicalize 之后。
async fn get_movie_subtitles(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let list = MovieSubtitleService::new(state.db(), state.config())
        .get_movie_subtitles(&movie_number)
        .await?;

    // 每项补**签名 URL**：上游在服务里拼（`MovieSubtitleItemResource.url`，
    // `movie_subtitle_service.py:160-165`），本仓的签名密钥由路由层持有
    // （见 `sm_service::catalog::movie_subtitle` 的模块文档）。
    //
    // 载荷与上游逐字对齐：`subtitle_id` / `url` / `created_at` / `file_name` ——
    // **不含** `format` 与 `size_bytes`（那两个只在服务层的 `SubtitleAsset` 上，
    // 上游的列表资源也不带）。多塞字段不是「顺便给点信息」，是改协议。
    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items: Vec<serde_json::Value> = list
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "subtitle_id": item.subtitle_id,
                "url": sm_core::signing::build_signed_subtitle_url(&secret, item.subtitle_id, now),
                "created_at": item.created_at,
                "file_name": item.file_name,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "movie_number": list.movie_number,
        "items": items,
    })))
}

/// `limit` 的边界是 `0..=100`（**下界 0**，见 handler 文档）。
// `limit` 已接上：传给 `MovieRecommendationService::list_similar`。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct SimilarMoviesQuery {
    #[serde(default = "twenty")]
    limit: i64,
}

/// `GET /movies/{movie_number}/similar` —— 相似影片。
///
/// # ★ `limit` 的下界是 **0**，不是 1
///
/// 上游 `Query(default=20, ge=0, le=100)`。`ge=0` 意味着 **`limit=0` 合法**
/// 且返回**空列表**（不是 422）。
///
/// ⚠️ 这一点很容易写反。若照别处惯例写成 `ge=1`，客户端传 `limit=0`
/// （「不要相似影片」）时会收到 422 —— 那是**改变契约**。
///
/// 相似影片依赖 Qdrant 稀疏索引；索引未就绪时返回**空列表**而不是报错
/// （见 `discovery::recommendation` 的降级语义）。
async fn list_similar_movies(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
    EnvelopeQuery(query): EnvelopeQuery<SimilarMoviesQuery>,
) -> Result<Json<Vec<serde_json::Value>>, ErrorResponse> {
    let movies = MovieService::new(state.db());
    // 番号校验**在**降级判断之前。上游 `list_similar_resources` 也是先解析番号
    // （`list_similar` → 取影片）再查相似度，所以「番号不存在」永远是 404，
    // 与 Qdrant 通不通无关。反过来会让不存在的番号回 200 + 空列表。
    let (movie, _canonical) = movies.require_by_normalized_number(&movie_number).await?;

    // ★ 没启用相似度 -> **空列表**，不是 503、不是 404。
    //
    // 上游的降级语义（`recommendation_service.py:341-348`）：Qdrant 不可用只降级
    // 相似度信号，不让详情页整体报错。没启用比「不可用」更弱 —— 一条相似影片都
    // 给不出，返回空列表就是它的完整表现。
    let Some(store) = state.movie_similarity() else {
        return Ok(Json(Vec::new()));
    };

    let service = MovieRecommendationService::new(
        std::sync::Arc::clone(store),
        MovieFeatureRepository::new(state.db().clone()),
    );
    // `NotReady` 在这里变成 503（「索引在建，重试有意义」）；
    // `Unavailable` 由服务层降级成空列表。
    let similar = service
        .list_similar(i64::from(movie.id), query.limit)
        .await?;

    // 卡片按 id 索引回去 —— `load_cards` 的返回**顺序不保证**与入参一致。
    let ids: Vec<i32> = similar
        .iter()
        .map(|item| i32::try_from(item.movie_id).unwrap_or_default())
        .collect();
    let mut by_id: HashMap<i32, MovieCard> = movies
        .load_cards(&ids)
        .await?
        .into_iter()
        .map(|card| (card.movie.id, card))
        .collect();

    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items: Vec<serde_json::Value> = similar
        .iter()
        .filter_map(|item| {
            let card = by_id.remove(&i32::try_from(item.movie_id).ok()?)?;
            // 上游 `SimilarMovieListItemResource` = `MovieListItemResource` +
            // 一个 `similarity_score`，所以这里是「卡片 + 追加一个键」。
            let mut object =
                serde_json::to_value(MovieListItemResource::from_movie_card(&card, &secret, now))
                    .ok()?;
            object["similarity_score"] = serde_json::Value::from(f64::from(item.score));
            Some(object)
        })
        .collect();
    Ok(Json(items))
}

/// `GET /movies/{movie_number}/merged-playback` 的查询参数。
///
/// # ★ `library_id` **必填**且 `ge=1`
///
/// 上游 `library_id: int = Query(..., ge=1)` —— **无默认值**，缺参即 **422**。
/// 所以下面**没有** `#[serde(default)]`：缺了它会把「缺参」变成「library_id=0」。
// `library_id` 还没接上（handler 体是 `todo!()`），但它是**必填**契约参数。
// 落地后删 allow。
#[allow(dead_code)]
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct MergedPlaybackQuery {
    library_id: i64,
}

/// `GET /movies/{movie_number}/merged-playback` —— 合并播放。
async fn get_merged_playback(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
    EnvelopeQuery(query): EnvelopeQuery<MergedPlaybackQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let _ = (movie_number, query);
    todo!("骨架：接 MovieService::get_merged_playback；library_id 缺参应在 extractor 层 422")
}

/// `POST /movies/{movie_number}/metadata-refresh` —— **200**（不是 202）。
///
/// 覆盖式刷新（见 `catalog::catalog_import::refresh_movie_metadata_strict`）。
/// 番号冲突是 **409**（`movie_metadata_number_conflict`），调 JavDB 失败是 **502**。
async fn refresh_movie_metadata(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let _ = movie_number;
    todo!(
        "骨架：接 MovieMetadataRefreshService::refresh_movie_metadata；409 番号冲突 / 502 调用失败"
    )
}

/// `POST /movies/{movie_number}/heat-recompute` —— ★ **202 Accepted**。
///
/// 走调度器（`catalog::movie_task::MovieTaskService`），因为全表重算很慢。
/// 错误码：影片不存在 → 404；**已有同名任务在跑 → 409**
/// `movie_heat_recompute_conflict`（不排队）。
async fn recompute_movie_heat(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
) -> Result<(StatusCode, Json<ManualJobTriggerResponse>), ErrorResponse> {
    let response = MovieTaskService::new(state.db())
        .recompute_movie_heat(&movie_number)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// `PUT /movies/{movie_number}/subscription` —— ★ **204，无 body**。
///
/// 成功时**不返回资源**。别返回 `Json<...>` —— 那会让 204 带 body，
/// 而多数 HTTP 客户端会忽略它，白白序列化一遍。
async fn subscribe_movie(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
) -> Result<StatusCode, ErrorResponse> {
    MovieService::new(state.db())
        .set_subscription(&movie_number)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /movies/{movie_number}/subscription` —— ★ **204，无 body**。
async fn unsubscribe_movie(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(movie_number): Path<String>,
) -> Result<StatusCode, ErrorResponse> {
    MovieService::new(state.db())
        .unsubscribe_movie(&movie_number)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /movies/search/javdb/stream` —— **SSE 流**。
///
/// ⚠️ 路径里 `search` 是**字面段**，不是 `{movie_number}`，所以与
/// `/movies/{movie_number}` 不冲突（段数不同）。
async fn search_javdb_movies_stream(
    State(_state): State<AppState>,
    _user: CurrentUser,
    EnvelopeJson(_payload): EnvelopeJson<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：SSE —— MovieMetadataRefreshService::stream_search_and_upsert_movie_from_javdb")
}

/// `POST /movies/series/{series_id}/javdb/import/stream` —— **SSE 流**。
///
/// 导入一个系列的全部影片。**单部失败不中断整批**（事件里带 error）。
async fn import_series_movies_stream(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(series_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let _ = series_id;
    todo!("骨架：SSE —— MovieMetadataRefreshService::stream_import_series_movies_from_javdb")
}
