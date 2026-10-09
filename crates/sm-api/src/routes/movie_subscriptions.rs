//! `/movie-subscriptions` —— 影片订阅台账。
//!
//! # 与上游 `src/api/routers/catalog/subscriptions.py` 的对应
//!
//! 上游这个 router 有 3 条路由，本文件落了 **1** 条：
//!
//! | 上游端点 | 本文件 | 状态 |
//! |---|---|---|
//! | `POST /search-resets` | `reset_searches` | **已落** |
//! | `GET ""` | —— | 未落（要派生状态表达式 + 富 DTO + 三条辅助查询） |
//! | `GET /status-counts` | —— | 未落（同上的状态表达式） |
//!
//! **它是独立顶层资源而不是挂在 `/movies` 下**：上游注释写明了理由 ——
//! 避免与 `/movies/{movie_number}` 抢路由，也不用依赖注册顺序保证匹配优先级。
//! 这里跟着上游单独一个文件。
//!
//! # 「批量取消订阅」刻意不在这个域
//!
//! 上游注释：不删文件走 `POST /movies/unsubscriptions`，要删媒体文件走
//! `DELETE /media/{media_id}`，两者都已存在，这里不平行造一套。照抄。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_core::pagination::Paginated;
use sm_service::catalog::movie::MovieService;
use sm_service::catalog::movie_subscription::{
    MovieSubscriptionService, SubscriptionListItem, SubscriptionListParams, SubscriptionSort,
    SubscriptionStatusCounts,
};
use sm_service::error::details_of;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::routes::movies::{default_page, default_page_size};
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

/// 订阅状态筛选（上游 `MovieSubscriptionStatus`）。
///
/// `all` 是缺省；其余七项与数据库里那个 `CASE` 的取值**逐字一致** —— 筛选用它
/// 直接和 `CASE` 比较，所以这里不能自己造拼写。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MovieSubscriptionStatusQuery {
    #[default]
    All,
    Imported,
    Downloading,
    ImportFailed,
    Pending,
    Missing,
    Exhausted,
    Failed,
}

impl MovieSubscriptionStatusQuery {
    /// `None` = 全部。
    fn as_filter(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Imported => Some("imported"),
            Self::Downloading => Some("downloading"),
            Self::ImportFailed => Some("import_failed"),
            Self::Pending => Some("pending"),
            Self::Missing => Some("missing"),
            Self::Exhausted => Some("exhausted"),
            Self::Failed => Some("failed"),
        }
    }
}

/// 排序（上游 `MovieSubscriptionSort` 的七个字面量，含冒号与方向）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
enum MovieSubscriptionSortQuery {
    #[default]
    #[serde(rename = "subscribed_at:desc")]
    SubscribedAtDesc,
    #[serde(rename = "subscribed_at:asc")]
    SubscribedAtAsc,
    #[serde(rename = "release_date:desc")]
    ReleaseDateDesc,
    #[serde(rename = "release_date:asc")]
    ReleaseDateAsc,
    #[serde(rename = "last_searched_at:desc")]
    LastSearchedAtDesc,
    #[serde(rename = "last_searched_at:asc")]
    LastSearchedAtAsc,
    #[serde(rename = "attempt_count:desc")]
    AttemptCountDesc,
}

impl From<MovieSubscriptionSortQuery> for SubscriptionSort {
    fn from(value: MovieSubscriptionSortQuery) -> Self {
        match value {
            MovieSubscriptionSortQuery::SubscribedAtDesc => Self::SubscribedAtDesc,
            MovieSubscriptionSortQuery::SubscribedAtAsc => Self::SubscribedAtAsc,
            MovieSubscriptionSortQuery::ReleaseDateDesc => Self::ReleaseDateDesc,
            MovieSubscriptionSortQuery::ReleaseDateAsc => Self::ReleaseDateAsc,
            MovieSubscriptionSortQuery::LastSearchedAtDesc => Self::LastSearchedAtDesc,
            MovieSubscriptionSortQuery::LastSearchedAtAsc => Self::LastSearchedAtAsc,
            MovieSubscriptionSortQuery::AttemptCountDesc => Self::AttemptCountDesc,
        }
    }
}

/// `GET /movie-subscriptions` 的查询参数。
#[derive(Debug, Deserialize)]
struct ListSubscriptionsQuery {
    #[serde(default)]
    status: MovieSubscriptionStatusQuery,
    #[serde(default)]
    sort: MovieSubscriptionSortQuery,
    /// 番号或片名的子串（`LIKE %q%`，不转义 `%` —— 与影片检索同一口径）。
    #[serde(default)]
    search: Option<String>,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

/// 订阅列表项（上游 `MovieSubscriptionListItemResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionListItemResource {
    pub movie_id: i32,
    pub movie_number: String,
    pub title: String,
    pub cover_image: Option<crate::dto::ImageResource>,
    pub thin_cover_image: Option<crate::dto::ImageResource>,
    /// `YYYY-MM-DD`；没有上映日期时 `null`。
    pub release_date: Option<String>,
    pub subscribed_at: Option<String>,
    pub status: String,
    pub is_fresh: bool,
    pub attempt_count: i32,
    pub attempt_limit: i32,
    pub last_searched_at: Option<String>,
    pub last_error: Option<String>,
    pub import_status: Option<String>,
    /// computed：导入状态的中文说明。
    pub import_status_label: Option<String>,
    pub dead_download_task_count: i64,
    pub media_count: i64,
}

impl SubscriptionListItemResource {
    /// 由 service 的投影构造。签名的 `secret` / `now` 用于给封面签名 ——
    /// 与 [`crate::dto::MovieListItemResource`] 同一个理由。
    fn from_item(item: SubscriptionListItem, secret: &str, now: i64) -> Self {
        let signed = |image: Option<&sm_db::catalog::asset::Image>| {
            image.map(|image| crate::dto::ImageResource {
                id: image.id,
                origin: crate::dto::sign_image_origin(secret, &image.origin, now),
            })
        };
        Self {
            movie_id: item.movie_id,
            movie_number: item.movie_number,
            title: item.title,
            cover_image: signed(item.cover_image.as_ref()),
            thin_cover_image: signed(item.thin_cover_image.as_ref()),
            release_date: item
                .release_date
                .map(|value| value.format("%Y-%m-%d").to_string()),
            subscribed_at: timestamp_of(item.subscribed_at),
            status: item.status,
            is_fresh: item.is_fresh,
            attempt_count: item.attempt_count,
            attempt_limit: item.attempt_limit,
            last_searched_at: timestamp_of(item.last_searched_at),
            last_error: item.last_error,
            import_status: item.import_status.clone(),
            // 上游 `@computed_field`：未知取值回退原值，而不是 null。
            // 映射本尊在 `crate::dto`（`GET /download-tasks` 用的是同一份）。
            import_status_label: item
                .import_status
                .map(|status| crate::dto::describe_import_status(&status)),
            dead_download_task_count: item.dead_download_task_count,
            media_count: item.media_count,
        }
    }
}

/// 可空时间戳 → 上游 Pydantic 的 `datetime` 字面量形状。
fn timestamp_of(value: Option<chrono::NaiveDateTime>) -> Option<String> {
    value.map(|value| value.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// 从运行期配置读两个订阅检索参数。
///
/// # 「键不存在」用默认值，「配置读不了」报错
///
/// 原来返回 `(i32, i64)` 且内部 `unwrap_or_default()`。缺键时用 3 / 90 是对的
/// —— 那是**配置声明的默认值**，属于业务判断。但配置文件整个读不了时也返回
/// 3 / 90 就成了静默降级：用户改的检索窗口不生效，且没有任何提示。
///
/// 现在把两种情况分开：缺键 -> 默认值（3 / 90）；配置坏 -> 500 `config_invalid`。
fn subscription_search_settings(state: &AppState) -> Result<(i32, i64), ErrorResponse> {
    let snapshot = crate::config::snapshot_or_500(state)?;
    let downloads = snapshot.get("downloads");
    let pick = |key: &str| {
        downloads
            .and_then(|section| section.get(key))
            .and_then(serde_json::Value::as_i64)
    };
    let attempt_limit = pick("subscription_search_stale_attempt_limit").unwrap_or(3);
    let fresh_days = pick("subscription_search_fresh_days").unwrap_or(90);
    Ok((attempt_limit as i32, fresh_days))
}

async fn list_subscriptions(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListSubscriptionsQuery>,
) -> Result<Json<Paginated<SubscriptionListItemResource>>, ErrorResponse> {
    // **这个端点的分页是校验过的**（上游 `validate_page`），与 `GET /movies` 的
    // 裸 `int` 不同 —— 错误码也是它专属的。
    if let Err(error) = sm_core::pagination::validate_page(query.page, query.page_size) {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_movie_subscription_filter",
            error.message(),
        )
        .with_details(details_of("detail", error.message())));
    }

    let (attempt_limit, fresh_days) = subscription_search_settings(&state)?;
    let params = SubscriptionListParams {
        status: query.status.as_filter().map(str::to_owned),
        sort: query.sort.into(),
        search: query.search.clone(),
        attempt_limit,
        fresh_days,
    };

    let page = MovieSubscriptionService::new(state.db())
        .list_subscriptions(&params, query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items = page
        .items
        .into_iter()
        .map(|item| SubscriptionListItemResource::from_item(item, &secret, now))
        .collect();

    Ok(Json(Paginated::new(
        items,
        query.page,
        query.page_size,
        page.total,
    )))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/movie-subscriptions",
            get(list_subscriptions).fallback(method_not_allowed),
        )
        .route(
            "/movie-subscriptions/status-counts",
            get(count_statuses).fallback(method_not_allowed),
        )
        .route(
            "/movie-subscriptions/search-resets",
            post(reset_searches).fallback(method_not_allowed),
        )
}

/// 订阅状态计数（上游 `MovieSubscriptionStatusCountsResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionStatusCountsResource {
    pub total: i64,
    pub imported: i64,
    pub downloading: i64,
    pub import_failed: i64,
    pub pending: i64,
    pub missing: i64,
    pub exhausted: i64,
    pub failed: i64,
}

impl From<SubscriptionStatusCounts> for SubscriptionStatusCountsResource {
    fn from(value: SubscriptionStatusCounts) -> Self {
        Self {
            total: value.total,
            imported: value.imported,
            downloading: value.downloading,
            import_failed: value.import_failed,
            pending: value.pending,
            missing: value.missing,
            exhausted: value.exhausted,
            failed: value.failed,
        }
    }
}

async fn count_statuses(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<SubscriptionStatusCountsResource>, ErrorResponse> {
    let counts = MovieSubscriptionService::new(state.db())
        .count_by_status()
        .await?;
    Ok(Json(counts.into()))
}

/// `POST /search-resets` 的请求体（上游 `MovieSubscriptionSearchResetRequest`）。
#[derive(Debug, Clone, Deserialize)]
struct MovieSubscriptionSearchResetRequest {
    /// 省略 = 重开全部**已放弃**的订阅；传入则只重开这些影片。
    #[serde(default)]
    movie_ids: Option<Vec<i32>>,
}

/// 重开结果（上游 `MovieSubscriptionSearchResetResponse`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieSubscriptionSearchResetResource {
    pub reset_count: i64,
}

/// # 为什么用 `Bytes` 手解而不是 `EnvelopeJson`
///
/// 上游这个端点的请求体是**可选**的（`payload: ... | None = None`），完全不发
/// body 也算合法。axum 的 `Json` 提取器对空体会走拒绝路径（JSON 解析失败），
/// 那会把「没带 body」变成 422 —— 而上游是 200 + 重开全部已放弃的订阅。
///
/// 所以这里手接原始字节：空体 → 等价于省略；非空 → 按 JSON 解析，解析失败仍是
/// 422 `validation_error`（与 [`crate::extract::Json`] 的拒绝形状一致）。
async fn reset_searches(
    _user: CurrentUser,
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<MovieSubscriptionSearchResetResource>, ErrorResponse> {
    let movie_ids = if body.is_empty() {
        None
    } else {
        let payload: MovieSubscriptionSearchResetRequest =
            serde_json::from_slice(&body).map_err(|err| {
                ErrorResponse::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation_error",
                    "Request validation failed",
                )
                .with_details(details_of("detail", err.to_string()))
            })?;
        payload.movie_ids.filter(|ids| !ids.is_empty())
    };

    let reset_count = MovieService::new(state.db())
        .reset_subscription_searches(movie_ids.as_deref())
        .await?;
    Ok(Json(MovieSubscriptionSearchResetResource { reset_count }))
}
