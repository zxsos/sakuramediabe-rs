//! `GET/PATCH/POST/DELETE /actors*` —— 演员目录端点的 HTTP 层。
//!
//! # 与上游 `src/api/routers/catalog/actors.py` 的对应
//!
//! 上游 13 条路由里，本文件落了 **11** 条（`PUT`/`DELETE` 订阅是两条）。
//!
//! | 上游 | 本文件 | 状态 |
//! |---|---|---|
//! | `GET ""` | `list_actors` | 已落（含 `cups` 解析与 `ge` 校验） |
//! | `GET /filter-options` | `get_actor_filter_options` | 已落 |
//! | `GET /{actor_id}` | `get_actor` | 已落 |
//! | `PATCH /{actor_id}` | `update_actor` | 已落（原始 JSON，保留 `exclude_unset`） |
//! | `DELETE /{actor_id}/profile-image` | `clear_actor_profile_image` | 已落（只做 loose 分支） |
//! | `PUT /{actor_id}/subscription` | `subscribe_actor` | 已落（204） |
//! | `DELETE /{actor_id}/subscription` | `unsubscribe_actor` | 已落（204） |
//! | `GET /{actor_id}/movie-ids` | `get_actor_movie_ids` | 已落 |
//! | `GET /{actor_id}/tags` | `get_actor_tags` | 已落 |
//! | `GET /{actor_id}/years` | `get_actor_years` | 已落 |
//! | `POST /{actor_id}/merge` | `merge_actor` | 已落（服务层六步合并） |
//! | `PUT /{actor_id}/profile-image` | —— | **阻塞**：需 EXIF + LANCZOS + 有损 WebP（阶段 9） |
//! | `POST /search/javdb/stream` | —— | **阻塞**：JavDB provider + 插件 ABI |
//!
//! # `cups` 与 `ge` 校验必须在 handler 里
//!
//! 上游把 `cups` 交给 `_parse_cups`（逗号分隔、trim、大写、含非 ASCII 字母即
//! 422 `invalid_actor_filter`），把 age/height 的 `ge` 交给 pydantic —— 两者
//! 都在 service **之前** 发生。本层不复刻的话，一个 `age_min=-5` 会走到
//! `years_before(today, -5)`（未来日期）而**静默命中全部演员**。
//!
//! # `PATCH` 收原始 JSON 对象
//!
//! 上游 `ActorUpdateRequest` 是 `extra="ignore"` 的 pydantic 模型，而
//! `service.update_profile` 靠 `exclude_unset` 区分「键不存在」与「键存在但为
//! `null`」。serde 的 `Option<T>` 把两者都解成 `None`，所以本层把 body 当
//! [`serde_json::Map`] 原样透传给 service。
//!
//! # 签名密钥每次请求重读
//!
//! 头像 `origin` 要签名，而 `PATCH /config` 能在运行期改密钥 —— 具体理由与
//! 取法见 [`crate::signing`]（影片卡片也要签名，两处共用同一份实现）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value};

use sm_db::catalog::actor::{GENDER_FEMALE, GENDER_MALE};
use sm_service::catalog::actor::{ActorListParams, ActorService};
use sm_service::catalog::actor_merge::ActorMergeService;

use crate::auth::CurrentUser;
use crate::dto::{
    ActorDetailResource, ActorFilterOptionsResource, ActorMergeRequest, ActorResource, TagResource,
    YearResource,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::deser_bool;
use crate::routes::method_not_allowed;
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/actors", get(list_actors).fallback(method_not_allowed))
        // 静态段 `/filter-options` 与 `/{actor_id}` 由 matchit 按「静态优先」
        // 路由，二者不冲突。
        .route(
            "/actors/filter-options",
            get(get_actor_filter_options).fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}",
            get(get_actor)
                .patch(update_actor)
                .fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}/merge",
            post(merge_actor).fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}/profile-image",
            delete(clear_actor_profile_image).fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}/subscription",
            put(subscribe_actor)
                .delete(unsubscribe_actor)
                .fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}/movie-ids",
            get(get_actor_movie_ids).fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}/tags",
            get(get_actor_tags).fallback(method_not_allowed),
        )
        .route(
            "/actors/{actor_id}/years",
            get(get_actor_years).fallback(method_not_allowed),
        )
}

#[derive(Debug, Deserialize)]
struct ActorPath {
    actor_id: i32,
}

/// `GET /actors` 的查询参数。
///
/// 缺省值照抄上游 `Query(default=...)`。`has_playable_movies` 走
/// [`deser_bool`] —— 上游是 pydantic 的 lax 布尔，`?has_playable_movies=1`
/// 必须能解析（理由见 [`crate::query`]）。
#[derive(Debug, Deserialize)]
struct ListActorsQuery {
    #[serde(default)]
    gender: GenderFilter,
    #[serde(default)]
    subscription_status: SubscriptionFilter,
    age_min: Option<i32>,
    age_max: Option<i32>,
    height_min: Option<i32>,
    height_max: Option<i32>,
    cups: Option<String>,
    #[serde(default, deserialize_with = "deser_bool")]
    has_playable_movies: bool,
    sort: Option<String>,
    query: Option<String>,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

fn default_page() -> i64 {
    1
}

fn default_page_size() -> i64 {
    20
}

/// `GET /actors/filter-options` 的查询参数。
#[derive(Debug, Default, Deserialize)]
struct FilterOptionsQuery {
    #[serde(default)]
    gender: GenderFilter,
    #[serde(default)]
    subscription_status: SubscriptionFilter,
}

/// 上游 `ActorListGender`（`all` / `female` / `male`）。
///
/// 非本枚举值 → [`crate::extract::Query`] 的 422 `validation_error`，
/// 与 FastAPI 对 enum 的行为一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GenderFilter {
    #[default]
    All,
    Female,
    Male,
}

impl GenderFilter {
    /// 映射成 `ActorListParams.gender`。`All` → `None`（不限）。
    fn as_i32(self) -> Option<i32> {
        match self {
            Self::All => None,
            Self::Female => Some(GENDER_FEMALE),
            Self::Male => Some(GENDER_MALE),
        }
    }
}

/// 上游 `ActorListSubscriptionStatus`（`all` / `subscribed` / `unsubscribed`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SubscriptionFilter {
    #[default]
    All,
    Subscribed,
    Unsubscribed,
}

impl SubscriptionFilter {
    /// 映射成 `ActorListParams.subscribed`。`All` → `None`（不限）。
    fn as_bool(self) -> Option<bool> {
        match self {
            Self::All => None,
            Self::Subscribed => Some(true),
            Self::Unsubscribed => Some(false),
        }
    }
}

fn today_utc() -> chrono::NaiveDate {
    chrono::Utc::now().naive_utc().date()
}

/// `_parse_cups` 的等价物：逗号分隔 → trim → 大写 → 去重升序。
///
/// 空串、含空项、含非 ASCII 字母一律 422 `invalid_actor_filter`，
/// `details` 回显**原始输入**（`{"cups": raw}`）—— 与上游逐字一致。
fn parse_cups(raw: Option<&str>) -> Result<Vec<String>, ErrorResponse> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut cups: Vec<String> = Vec::new();
    for item in raw.split(',') {
        let cup = item.trim().to_uppercase();
        if cup.is_empty() || !cup.chars().all(|c| c.is_ascii_alphabetic()) {
            let mut details = Map::new();
            details.insert("cups".to_owned(), Value::from(raw));
            return Err(ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_actor_filter",
                "Invalid cup filter",
            )
            .with_details(details));
        }
        cups.push(cup);
    }
    cups.sort_unstable();
    cups.dedup();
    Ok(cups)
}

/// pydantic `Query(ge=...)` 的等价物。越界 → 422 `validation_error`。
///
/// 报的是**第一个**越界字段即可 —— 上游 pydantic 会一次报全部，但那属于
/// pydantic 的批量校验；契约里客户端关心的只有「状态码 422 + code」。
fn check_lower_bound(field: &str, value: Option<i32>, minimum: i32) -> Result<(), ErrorResponse> {
    let Some(value) = value else {
        return Ok(());
    };
    if value < minimum {
        let mut details = Map::new();
        details.insert(field.to_owned(), Value::from(value));
        details.insert("min".to_owned(), Value::from(minimum));
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "Request validation failed",
        )
        .with_details(details));
    }
    Ok(())
}

async fn list_actors(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListActorsQuery>,
) -> Result<Json<sm_core::pagination::Paginated<ActorResource>>, ErrorResponse> {
    let cups = parse_cups(query.cups.as_deref())?;
    check_lower_bound("age_min", query.age_min, 0)?;
    check_lower_bound("age_max", query.age_max, 0)?;
    check_lower_bound("height_min", query.height_min, 1)?;
    check_lower_bound("height_max", query.height_max, 1)?;

    let params = ActorListParams {
        gender: query.gender.as_i32(),
        subscribed: query.subscription_status.as_bool(),
        age_min: query.age_min,
        age_max: query.age_max,
        height_min: query.height_min,
        height_max: query.height_max,
        cups,
        has_playable_movies: query.has_playable_movies,
        sort: query.sort,
        query: query.query,
        page: query.page,
        page_size: query.page_size,
    };

    let page = ActorService::new(state.db()).list(&params).await?;
    let secret = signing_secret(&state);
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|view| ActorResource::from_view(view, &secret, now))
        .collect();
    Ok(Json(sm_core::pagination::Paginated::new(
        items,
        page.page,
        page.page_size,
        page.total,
    )))
}

async fn get_actor_filter_options(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<FilterOptionsQuery>,
) -> Result<Json<ActorFilterOptionsResource>, ErrorResponse> {
    let options = ActorService::new(state.db())
        .filter_options(
            query.gender.as_i32(),
            query.subscription_status.as_bool(),
            today_utc(),
        )
        .await?;
    Ok(Json(ActorFilterOptionsResource::from(options)))
}

async fn get_actor(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<Json<ActorDetailResource>, ErrorResponse> {
    let view = ActorService::new(state.db()).detail(path.actor_id).await?;
    Ok(Json(ActorDetailResource::from_view(
        &view,
        &signing_secret(&state),
        now_seconds(),
    )))
}

async fn update_actor(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
    EnvelopeJson(body): EnvelopeJson<Map<String, Value>>,
) -> Result<Json<ActorDetailResource>, ErrorResponse> {
    let view = ActorService::new(state.db())
        .update_profile(path.actor_id, &body, today_utc())
        .await?;
    Ok(Json(ActorDetailResource::from_view(
        &view,
        &signing_secret(&state),
        now_seconds(),
    )))
}

async fn merge_actor(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
    EnvelopeJson(body): EnvelopeJson<ActorMergeRequest>,
) -> Result<Json<ActorDetailResource>, ErrorResponse> {
    // 上游这两条是 pydantic 层校验（`Field(min_length=1)` + 正整数 validator），
    // 都会走到 `422 validation_error`。
    validate_merge_sources(&body.source_actor_ids)?;
    let view = ActorMergeService::new(state.db())
        .merge_actors(path.actor_id, &body.source_actor_ids)
        .await?;
    Ok(Json(ActorDetailResource::from_view(
        &view,
        &signing_secret(&state),
        now_seconds(),
    )))
}

/// `ActorMergeRequest.source_actor_ids` 的非空与正整数校验。
fn validate_merge_sources(source_actor_ids: &[i32]) -> Result<(), ErrorResponse> {
    let invalid = source_actor_ids.is_empty() || source_actor_ids.iter().any(|id| *id <= 0);
    if invalid {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "Request validation failed",
        ));
    }
    Ok(())
}

async fn clear_actor_profile_image(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<Json<ActorDetailResource>, ErrorResponse> {
    let view = ActorService::new(state.db())
        .clear_profile_image(path.actor_id)
        .await?;
    Ok(Json(ActorDetailResource::from_view(
        &view,
        &signing_secret(&state),
        now_seconds(),
    )))
}

async fn subscribe_actor(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<StatusCode, ErrorResponse> {
    ActorService::new(state.db())
        .set_subscription(path.actor_id, true)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unsubscribe_actor(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<StatusCode, ErrorResponse> {
    ActorService::new(state.db())
        .set_subscription(path.actor_id, false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_actor_movie_ids(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<Json<Vec<i32>>, ErrorResponse> {
    let ids = ActorService::new(state.db())
        .movie_ids(path.actor_id)
        .await?;
    Ok(Json(ids))
}

async fn get_actor_tags(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<Json<Vec<TagResource>>, ErrorResponse> {
    let tags = ActorService::new(state.db()).tags(path.actor_id).await?;
    Ok(Json(tags.into_iter().map(TagResource::from).collect()))
}

async fn get_actor_years(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ActorPath>,
) -> Result<Json<Vec<YearResource>>, ErrorResponse> {
    let years = ActorService::new(state.db()).years(path.actor_id).await?;
    Ok(Json(years.into_iter().map(YearResource::from).collect()))
}
