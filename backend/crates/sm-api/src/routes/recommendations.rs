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
//! **1. `page_size` 的校验边界。** daily 与 hot-actress 的**查询参数**是
//! `Query(default=20, ge=1, le=100)`，moment 的查询参数**没有边界**
//! （`Query(default=20)`）。
//!
//! ⚠️ 一度据此写「`page_size=100000` 在 moment 上合法」—— **那是错的**。
//! 边界有两道：pydantic 的 Query 边界，与服务层的 `validate_page`
//! （`1 <= page_size <= 100`）。**三个端点都调 validate_page**，所以
//! `page_size=100000` 三条都 422；差别只在**错误码**：
//!
//! | 端点 | 上游拦在哪 | `page_size=100000` 的码 |
//! |---|---|---|
//! | daily / hot-actress | pydantic `le=100` | `validation_error` |
//! | moment | 服务层 | `invalid_moment_recommendation_filter` |
//!
//! 本仓三条路由都**不**在查询参数上设边界（都交给服务层），所以 daily /
//! hot-actress 这里报的是各自的专用码而不是 `validation_error` ——
//! 由 `daily_recommendations_http.rs` 的断言钉住。
//!
//! 「给 moment 也加上界」因此是个**看着有意义其实没有**的改动：上界本来就
//! 在，真正会变的只有错误码。照抄。
//!
//! **2. 分页响应形态。** daily/hot-actress 用泛型 `PageResponse[T]`（带 `total`），
//! moment 用专用 `MomentRecommendationPageResource` —— 形状不同，**但不是
//! 「少一个 total」**：专用资源里 `total` 与 `generated_at` 都在（见
//! `upstream/sakuramediabe/src/schema/discovery/moment_recommendations.py:21-26`）。
//! 真实差别是 moment **多一个 `generated_at`**，且 `items` 的元素是嵌套的
//! `image` + `movie` 对象而非扁平字段。

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::discovery::daily_recommendation::DailyRecommendationService;
use sm_service::discovery::hot_actress_release::HotActressReleaseQuery;
use sm_service::discovery::moment_recommendation::MomentRecommendationQuery;

use crate::auth::CurrentUser;
use crate::dto::{
    sign_image_origin, DailyRecommendationMovieResource, HotActressReleaseMovieResource,
    ImageResource, MovieListItemResource,
};
use crate::error::ErrorResponse;
// 查询参数一律走信封提取器：坏值（`?page=abc`）要 422 + 错误信封，而不是
// axum 默认的 400 + 纯文本。见 `crate::extract` 的模块文档。
use crate::extract::Query as EnvelopeQuery;
use crate::query::{one, twenty};
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/daily-recommendations", get(list_daily_recommendations))
        .route("/moment-recommendations", get(list_moment_recommendations))
        .route("/hot-actress-releases", get(list_hot_actress_releases))
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

/// 泛型分页响应（对应上游 `PageResponse[T]`）。
#[derive(Debug, Serialize)]
struct PageResponse<T> {
    items: Vec<T>,
    page: i64,
    page_size: i64,
    total: i64,
}

/// `GET /daily-recommendations`
///
/// 读**当前**快照的一页（上游 `list_items`，`daily_recommendation_service.py:414-476`）。
/// 响应元素是**完整影片卡片** + 8 个推荐字段
/// （[`DailyRecommendationMovieResource`]），**页级没有 `snapshot_date`**。
///
/// `page < 1` 或 `page_size` 不在 `1..=100` → 422
/// `invalid_daily_recommendation_filter`（服务层给，**不夹到边界** —— FastAPI
/// 的 `ge` / `le` 行为）。
///
/// ⚠️ **生成侧仍未接**：`generate_latest_snapshot` 与它的 IO 装载器还不存在，
/// 所以库里没有快照时返回**空页**（`items: []`, `total: 0`）—— 这不是 bug。
/// 见 [`sm_service::discovery::daily_recommendation`] 顶部说明。
async fn list_daily_recommendations(
    State(state): State<AppState>,
    _user: CurrentUser,
    EnvelopeQuery(query): EnvelopeQuery<BoundedPageQuery>,
) -> Result<Json<PageResponse<DailyRecommendationMovieResource>>, ErrorResponse> {
    let page = DailyRecommendationService::new(state.db())
        .list_items(query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| DailyRecommendationMovieResource::from_daily_card(card, &secret, now))
        .collect();

    Ok(Json(PageResponse {
        items,
        page: query.page,
        page_size: query.page_size,
        total: page.total,
    }))
}

/// 瞬时推荐分页查询 —— **刻意无上下界**。
///
/// 上游 `moment_recommendations.py:16-17` 是 `Query(default=1)` 与
/// `Query(default=20)`，**没有 `ge` / `le`**。所以不能复用上面的
/// [`BoundedPageQuery`]：它的名字会让人以为边界在这里。
///
/// 边界仍然存在 —— 在服务层（`MomentRecommendationQuery::validate_page`），
/// 见模块文档第 1 条：越界的错误**码**才是两者真正的差别。
#[derive(Debug, Deserialize)]
struct UnboundedPageQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
}

/// 瞬时推荐单条（上游 `MomentRecommendationItemResource`）。
///
/// # 与另两个列表的 item **形状不同**
///
/// daily / hot-actress 的元素是**扁平**影片字段，而这里是
/// `image` + `movie` 两个**嵌套对象**（上游 `:8-18`）。所以不能用同一个
/// 泛型 `PageResponse<T>` —— 那会把 `movie` 拍平，客户端反序列化直接失败。
#[derive(Debug, Serialize)]
struct MomentRecommendationItemResource {
    recommendation_id: i64,
    rank: i64,
    score: f64,
    strategy: String,
    reason: String,
    media_id: i64,
    thumbnail_id: i64,
    offset_seconds: i64,
    /// 该时刻的缩略图。**`origin` 要签名**（见 [`ImageResource`]）。
    image: ImageResource,
    movie: MovieListItemResource,
}

/// 瞬时推荐分页（上游 `MomentRecommendationPageResource`）。
///
/// # `total` **不是**表里的总行数
///
/// 上游 `:513-520` 的 `total` 与分页都带 `Media.valid == True` 且
/// `Movie.is_blacklisted == False` 两个条件 —— 失效的媒体与黑名单影片**不占
/// 分页槽位**。用 `COUNT(*)` 会让最后一页的条目数少于 `page_size` 却仍然
/// 报一个偏大的总数。
///
/// # `generated_at` 与 `items` 可能**不一致**
///
/// `generated_at` 取的是**全表**按时间倒序的第一行（`:521-528`），没有
/// 「有效」过滤。所以池子里最新的那批若全是失效媒体，`generated_at` 仍会
/// 显示那个时间，而 `items` 是空的。照抄，别「修正」成一致。
#[derive(Debug, Serialize)]
struct MomentRecommendationResponse {
    items: Vec<MomentRecommendationItemResource>,
    page: i64,
    page_size: i64,
    total: i64,
    generated_at: Option<String>,
}

/// `GET /moment-recommendations`
///
/// 上游用 `validate_page(..., error_code="invalid_moment_recommendation_filter")`
/// （`moment_recommendation_service.py:511`）—— **专用错误码**，与
/// hot-actress 的那个区分开。校验在服务层做（这里只把参数递进去）。
///
/// # 装配由服务层完成，这里只做两件本层才有的事
///
/// 1. **签名**：`image.origin` 与卡片里的封面都要密钥；
/// 2. **换形状**：`MomentRecommendationCard` → `MomentRecommendationItemResource`
///    （上游 `:548-575` 的 `MovieListResource.from_attributes_model(...)` 与
///    `ImageResource.from_attributes_model(thumbnail.image)`）。
///
/// 服务层返回的 `items` 可能比 `page_size` 短（取不到图片/卡片的行被跳过且
/// 不补位）—— **照原样返回，不补齐**，`total` 也不因此调整。
async fn list_moment_recommendations(
    State(state): State<AppState>,
    _user: CurrentUser,
    EnvelopeQuery(query): EnvelopeQuery<UnboundedPageQuery>,
) -> Result<Json<MomentRecommendationResponse>, ErrorResponse> {
    let page = MomentRecommendationQuery::new(state.db())
        .list_items(query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|item| MomentRecommendationItemResource {
            recommendation_id: item.row.recommendation_id,
            rank: item.row.rank,
            score: item.row.score,
            strategy: item.row.strategy.clone(),
            reason: item.row.reason.clone(),
            media_id: item.row.media_id,
            thumbnail_id: item.row.thumbnail_id,
            offset_seconds: item.row.offset_seconds,
            image: ImageResource {
                id: item.image.id,
                origin: sign_image_origin(&secret, &item.image.origin, now),
            },
            movie: MovieListItemResource::from_movie_card(&item.card, &secret, now),
        })
        .collect();

    Ok(Json(MomentRecommendationResponse {
        items,
        page: page.page,
        page_size: page.page_size,
        total: page.total,
        generated_at: page.generated_at,
    }))
}

/// `GET /hot-actress-releases`
///
/// # 分页校验用**专用错误码**
///
/// 上游 `validate_page(..., error_code="invalid_hot_actress_release_filter")`。
/// 照抄 —— 客户端要靠它区分「分页参数错了」与「筛选条件错了」。
///
/// # `total` 是**打分后的候选数**，不是数据库行数
///
/// 上游 `len(scored_movies)`。所以 `total` 依赖打分结果（要跑完历史证据 +
/// 候选 + 排序），**不能用 `COUNT(*)` 顶替**。这也是该端点比看起来贵的原因。
async fn list_hot_actress_releases(
    State(state): State<AppState>,
    _user: CurrentUser,
    EnvelopeQuery(query): EnvelopeQuery<BoundedPageQuery>,
) -> Result<Json<PageResponse<HotActressReleaseMovieResource>>, ErrorResponse> {
    // 打分、分页、校验、装配（卡片 + 女优）都在服务层。**校验也在那里**
    // （它要在打分之前跑），所以路由这边不再调一次 `validate_page`。
    let page = HotActressReleaseQuery::new(state.db())
        .list_items(query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|item| HotActressReleaseMovieResource::from_item(item, &secret, now))
        .collect();

    // **回显请求的 page / page_size**，不是服务归一后的值（与上游
    // `PageResponse` 一致：客户端据此拼下一页 URL）。
    Ok(Json(PageResponse {
        items,
        page: query.page,
        page_size: query.page_size,
        total: page.total,
    }))
}
