//! `GET /daily-recommendations` 与 `GET /moment-recommendations` 与
//! `GET /hot-actress-releases` —— 三个只读列表端点。
//!
//! 放一个文件而不是三个：三者结构相同（都是 `GET ""` + `page`/`page_size` +
//! 分页响应），差异只在响应模型与分页形态，且**都只读**。拆三个文件会让
//! 同一段分页处理写三遍。
//!
//! # 与上游的对应
//!
//! | 上游 router | 端点 | 响应模型 | 依赖 |
//! |---|---|---|---|
//! | `daily_recommendations.py:15` | `GET /daily-recommendations` | `PageResponse[DailyRecommendationMovieResource]` | [`sm_service::discovery::daily_recommendation`] |
//! | `moment_recommendations.py:14` | `GET /moment-recommendations` | `MomentRecommendationPageResource` | [`sm_service::discovery::moment_recommendation`] |
//! | `hot_actress_releases.py:15` | `GET /hot-actress-releases` | `PageResponse[HotActressReleaseMovieResource]` | [`sm_service::discovery::hot_actress_release`] |
//!
//! # 两处上游不一致，**照抄不统一**
//!
//! **1. `page_size` 的校验边界。** daily 与 hot-actress 是
//! `Query(default=20, ge=1, le=100)`，**moment 没有任何边界**
//! （`Query(default=20)`）—— 也就是说 `page_size=100000` 在 moment 上合法。
//! 加上上界是「看着更安全」的直觉改动，但那会**改变契约**：客户端传大值时
//! 上游返回 200，本仓库返回 422。照抄。
//!
//! **2. 分页响应形态。** daily/hot-actress 用泛型 `PageResponse[T]`（带 `total`），
//! moment 用专用 `MomentRecommendationPageResource`（**不带 `total`**）。
//! 所以 [`MomentRecommendationPage`] 的形状与另两个不同 —— 不是漏字段。

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::discovery::daily_recommendation::DailyRecommendationItem;
use sm_service::discovery::hot_actress_release::HotActressReleaseItem;
use sm_service::discovery::moment_recommendation::MomentRecommendationItem;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/daily-recommendations", get(list_daily_recommendations))
        .route(
            "/moment-recommendations",
            get(list_moment_recommendations),
        )
        .route(
            "/hot-actress-releases",
            get(list_hot_actress_releases),
        )
}

/// 泛型分页查询（daily / hot-actress 共用）。
///
/// `page >= 1`、`1 <= page_size <= 100` —— 与上游 `ge=1, le=100` 一致。
/// **越界要 422 而不是夹到边界**（FastAPI 的行为）。
#[derive(Debug, Deserialize)]
struct BoundedPageQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
}

fn one() -> i64 { 1 }
fn twenty() -> i64 { 20 }

/// 泛型分页响应（对应上游 `PageResponse[T]`）。
#[derive(Debug, Serialize)]
struct PageResponse<T> {
    items: Vec<T>,
    page: i64,
    page_size: i64,
    total: i64,
}

/// `GET /daily-recommendations`
async fn list_daily_recommendations(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(_query): axum::extract::Query<BoundedPageQuery>,
) -> Result<Json<PageResponse<DailyRecommendationItem>>, ErrorResponse> {
    todo!("骨架：接 DailyRecommendationService::list_items")
}

/// 瞬时推荐分页查询 —— **刻意无上下界**。
///
/// 上游 `moment_recommendations.py:16-17` 是 `Query(default=1)` 与
/// `Query(default=20)`，**没有 `ge` / `le`**。所以不能复用上面的
/// [`BoundedPageQuery`]：加了上界就改变了契约。
#[derive(Debug, Deserialize)]
struct UnboundedPageQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
}

/// 瞬时推荐响应 —— **不带 `total`**。
///
/// 上游用专用资源而非 `PageResponse[T]`，所以没有总数字段。
/// **不是漏字段**，照抄。
#[derive(Debug, Serialize)]
struct MomentRecommendationResponse {
    items: Vec<MomentRecommendationItem>,
    page: i64,
    page_size: i64,
}

/// `GET /moment-recommendations`
async fn list_moment_recommendations(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(_query): axum::extract::Query<UnboundedPageQuery>,
) -> Result<Json<MomentRecommendationResponse>, ErrorResponse> {
    todo!("骨架：接 MomentRecommendationService::list_items")
}

/// `GET /hot-actress-releases`
async fn list_hot_actress_releases(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(_query): axum::extract::Query<BoundedPageQuery>,
) -> Result<Json<PageResponse<HotActressReleaseItem>>, ErrorResponse> {
    todo!("骨架：接 HotActressReleaseService::list_items")
}