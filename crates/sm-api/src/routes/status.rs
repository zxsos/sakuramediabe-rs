//! `GET /status*` —— 状态页的四个端点。
//!
//! # 与上游 `src/api/routers/system/status.py` 的对应
//!
//! | 上游端点 | 依赖 | 状态 |
//! |---|---|---|
//! | `GET /status/capabilities` | `optional_services.capabilities()` | **已落** |
//! | `GET /status` | `StatusService.get_status` | **已落**（本文件） |
//! | `GET /status/insights` | `StatusService.get_insights` | **已落**（本文件） |
//! | `GET /status/watch-trend` | `StatusService.get_watch_trend` | **已落**（本文件） |
//! | `GET /status/image-search` | `StatusService.get_image_search_status` | 阻塞：`discovery` 域 |
//! | `POST /status/metadata-provider/test` | `StatusService.test_metadata_provider` | 阻塞：`metadata` 域 |
//!
//! # 鉴权挂在 handler 上
//!
//! 上游这个 router 用 router 级 `dependencies=[Depends(get_current_user)]`，
//! 本仓库统一把 `CurrentUser` 写成 handler 参数（理由见
//! [`crate::routes::playlists`] 的模块文档）。
//!
//! # `watch-trend` 的 `range` 缺失时用默认值
//!
//! 上游是 `range: StatusWatchTrendRange = Query(default=LAST_30_DAYS)`。
//! 显式传了非法值则 **422**（FastAPI 对 enum 的行为），与
//! [`crate::query`] 里那些「非法值降级」的约定**不同** —— 那里降级是因为
//! 上游本身宽松，而这里上游严格，所以照上游。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::system::status::{
    InsightsResource, StatusResource, StatusService, TrendBucket, TrendGranularity,
    WatchTrendRange, WatchTrendResource,
};

use crate::auth::CurrentUser;
use crate::dto::CapabilitiesResource;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(get_status).fallback(method_not_allowed))
        .route(
            "/status/insights",
            get(get_insights).fallback(method_not_allowed),
        )
        .route(
            "/status/watch-trend",
            get(get_watch_trend).fallback(method_not_allowed),
        )
        .route(
            "/status/capabilities",
            get(get_capabilities).fallback(method_not_allowed),
        )
        // ★ 路径是 `/status/...`（三段）而不是 `/{provider}`（两段）——
        // 上游的 router prefix 就是 `/status`，容易误以为末段是变量。
        .route(
            "/status/image-search",
            get(get_image_search_status).fallback(method_not_allowed),
        )
        .route(
            "/status/metadata-providers/{provider}/test",
            get(test_metadata_provider).fallback(method_not_allowed),
        )
}

// ================================================================ /status

/// `GET /status` 的响应体。字段集合照抄上游 `StatusResource`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub backend_version: String,
    pub actors: ActorSummaryResponse,
    pub movies: MovieSummaryResponse,
    pub media_files: MediaFileSummaryResponse,
    pub media_libraries: MediaLibrarySummaryResponse,
    pub thumbnails: ThumbnailSummaryResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorSummaryResponse {
    pub female_total: i64,
    pub female_subscribed: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovieSummaryResponse {
    pub total: i64,
    pub subscribed: i64,
    pub playable: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFileSummaryResponse {
    pub total: i64,
    pub total_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaLibrarySummaryResponse {
    pub total: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailSummaryResponse {
    pub pending_media: i64,
    pub retry_wait_media: i64,
    pub terminal_failed_media: i64,
    pub total: i64,
}

impl From<StatusResource> for StatusResponse {
    fn from(v: StatusResource) -> Self {
        Self {
            backend_version: v.backend_version,
            actors: ActorSummaryResponse {
                female_total: v.actors.female_total,
                female_subscribed: v.actors.female_subscribed,
            },
            movies: MovieSummaryResponse {
                total: v.movies.total,
                subscribed: v.movies.subscribed,
                playable: v.movies.playable,
            },
            media_files: MediaFileSummaryResponse {
                total: v.media_files.total,
                total_size_bytes: v.media_files.total_size_bytes,
            },
            media_libraries: MediaLibrarySummaryResponse {
                total: v.media_libraries.total,
            },
            thumbnails: ThumbnailSummaryResponse {
                pending_media: v.thumbnails.pending_media,
                retry_wait_media: v.thumbnails.retry_wait_media,
                terminal_failed_media: v.thumbnails.terminal_failed_media,
                total: v.thumbnails.total,
            },
        }
    }
}

async fn get_status(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<StatusResponse>, ErrorResponse> {
    Ok(Json(
        StatusService::new(state.db()).get_status().await?.into(),
    ))
}

// ================================================================ /status/insights

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsightsResponse {
    pub download_tasks: DownloadTaskSummaryResponse,
    pub media_libraries: Vec<MediaLibraryUsageResponse>,
    pub collections: CollectionsSummaryResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadTaskSummaryResponse {
    pub total: i64,
    pub downloading: i64,
    pub importing: i64,
    pub imported: i64,
    pub import_failed: i64,
    pub skipped: i64,
    pub download_failed: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaLibraryUsageResponse {
    pub library_id: i32,
    pub name: String,
    pub provider_key: String,
    pub file_count: i64,
    pub total_size_bytes: i64,
    /// 磁盘总量。**`null` = 未探测**（缺 `playback` 域的
    /// `storage_space_usages`）。不是 `0` —— 0 会被渲染成「磁盘满了」。
    pub space_total_bytes: Option<i64>,
    pub space_used_bytes: Option<i64>,
    pub space_free_bytes: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionCountsResponse {
    pub count: i64,
    pub item_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionsSummaryResponse {
    pub playlists: CollectionCountsResponse,
    pub video_collections: CollectionCountsResponse,
    pub clip_collections: CollectionCountsResponse,
    pub moment_collections: CollectionCountsResponse,
}

impl From<InsightsResource> for InsightsResponse {
    fn from(v: InsightsResource) -> Self {
        Self {
            download_tasks: DownloadTaskSummaryResponse {
                total: v.download_tasks.total,
                downloading: v.download_tasks.downloading,
                importing: v.download_tasks.importing,
                imported: v.download_tasks.imported,
                import_failed: v.download_tasks.import_failed,
                skipped: v.download_tasks.skipped,
                download_failed: v.download_tasks.download_failed,
            },
            media_libraries: v
                .media_libraries
                .into_iter()
                .map(|u| MediaLibraryUsageResponse {
                    library_id: u.library_id,
                    name: u.name,
                    provider_key: u.provider_key,
                    file_count: u.file_count,
                    total_size_bytes: u.total_size_bytes,
                    space_total_bytes: u.space_total_bytes,
                    space_used_bytes: u.space_used_bytes,
                    space_free_bytes: u.space_free_bytes,
                })
                .collect(),
            collections: CollectionsSummaryResponse {
                playlists: CollectionCountsResponse {
                    count: v.collections.playlists.count,
                    item_count: v.collections.playlists.item_count,
                },
                video_collections: CollectionCountsResponse {
                    count: v.collections.video_collections.count,
                    item_count: v.collections.video_collections.item_count,
                },
                clip_collections: CollectionCountsResponse {
                    count: v.collections.clip_collections.count,
                    item_count: v.collections.clip_collections.item_count,
                },
                moment_collections: CollectionCountsResponse {
                    count: v.collections.moment_collections.count,
                    item_count: v.collections.moment_collections.item_count,
                },
            },
        }
    }
}

async fn get_insights(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<InsightsResponse>, ErrorResponse> {
    Ok(Json(
        StatusService::new(state.db()).get_insights().await?.into(),
    ))
}

// ================================================================ /status/watch-trend

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchTrendResponse {
    /// 原样回显请求的范围。上游是 enum，序列化成其字面量。
    pub range: String,
    /// `day` 或 `month`。
    pub granularity: String,
    pub watched_movie_count: i64,
    pub buckets: Vec<TrendBucketResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrendBucketResponse {
    /// `YYYY-MM-DD`（按天）或 `YYYY-MM`（按月）。
    pub period: String,
    pub count: i64,
}

/// `GET /status/watch-trend` 的查询参数。
///
/// **不写 `default`** —— `range` 缺失与「传了非法值」必须能区分：
/// 前者回落到 `LAST_30_DAYS`，后者要 422。所以这里是 `Option<String>`，
/// 在 handler 里显式处理两种情况。
#[derive(Debug, Deserialize)]
struct WatchTrendQuery {
    range: Option<String>,
}

impl From<WatchTrendResource> for WatchTrendResponse {
    fn from(v: WatchTrendResource) -> Self {
        Self {
            range: range_literal(v.range).to_owned(),
            granularity: granularity_literal(v.granularity).to_owned(),
            watched_movie_count: v.watched_movie_count,
            buckets: v
                .buckets
                .into_iter()
                .map(|b: TrendBucket| TrendBucketResponse {
                    period: b.period,
                    count: b.count,
                })
                .collect(),
        }
    }
}

/// 范围 → 上游 enum 的字面量。
///
/// 上游 `StatusWatchTrendRange` 的取值是 `7d` / `30d` / `90d` / `1y` / `all`
/// —— 响应里回显的必须是**这些**而不是 `last_7_days` 那种内部命名，
/// 因为客户端按字面量分支。
pub fn range_literal(range: WatchTrendRange) -> &'static str {
    match range {
        WatchTrendRange::Last7Days => "7d",
        WatchTrendRange::Last30Days => "30d",
        WatchTrendRange::Last90Days => "90d",
        WatchTrendRange::LastYear => "1y",
        WatchTrendRange::All => "all",
    }
}

pub fn granularity_literal(granularity: TrendGranularity) -> &'static str {
    match granularity {
        TrendGranularity::Day => "day",
        TrendGranularity::Month => "month",
    }
}

async fn get_watch_trend(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<WatchTrendQuery>,
) -> Result<Json<WatchTrendResponse>, ErrorResponse> {
    // 缺失 -> 默认 30 天；显式非法 -> 422。上游 enum 的行为。
    let range = match query.range.as_deref() {
        None => WatchTrendRange::default(),
        Some(raw) => WatchTrendRange::parse(raw).ok_or_else(|| {
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                "Invalid watch trend range",
            )
        })?,
    };

    // 时区偏移取运行时的 UTC 偏移（小时）。上游用 `get_runtime_timezone()`，
    // 本仓库显式传 —— 让「本地日」的算法可被单元测试钉住。
    let offset_hours = local_utc_offset_hours();

    let trend = StatusService::new(state.db())
        .get_watch_trend(range, offset_hours)
        .await?;
    Ok(Json(WatchTrendResponse::from(trend)))
}

/// 运行时的 UTC 偏移（小时，向下取整）。
///
/// **与调度器用同一个来源** —— 否则「今天看的」与「今天排的任务」会落在
/// 不同的窗口里。`sm_scheduler` 已经有这个计算，这里不重复实现。
fn local_utc_offset_hours() -> i64 {
    // 全库时间列都是 UTC naive；偏移只需要「现在比 UTC 快几个小时」。
    let utc_now = chrono::Utc::now();
    let local_now = chrono::Local::now();
    (local_now.naive_utc() - utc_now.naive_utc()).num_hours()
}

// ================================================================ /status/capabilities

async fn get_capabilities(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<CapabilitiesResource>, ErrorResponse> {
    let capabilities = sm_service::system::optional_services::capabilities_of(state.config())?;
    Ok(Json(CapabilitiesResource::from(capabilities)))
}

/// `GET /status/image-search` —— 图搜索引状态。
///
/// # 读**数据库单例表**，不是问 Qdrant
///
/// 理由与 `discovery::image_search_space` 相同：`/status/*` 是高频轮询接口，
/// 每次问 Qdrant 是一次网络往返。状态是「索引记录在哪个嵌入空间」这件事，
/// 由 `image_search_index_state`（id 恒为 1）持有。
async fn get_image_search_status(
    State(_state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接 StatusService::get_image_search_status（读单例表，不问 Qdrant）")
}

/// `GET /status/metadata-providers/{provider}/test` —— 探测元数据源。
///
/// # ★ 只接受 `javdb` 一个值，其它一律 **422 `invalid_metadata_provider`**
///
/// 上游是 `if normalized_provider not in {"javdb"}: raise ApiError(422, ...)`。
///
/// ⚠️ 归一化在**比较之前**：`provider.strip().lower()`。所以 `/test/JAVDB`
/// 合法、`/test/javdb%20` 也合法（strip 掉空白）。
///
/// 别把它做成「枚举所有已装 provider」—— 那会让「探测一个不存在的源」变成
/// 404，而客户端需要区分「源名写错了」（422，改请求）与「源不可用」（5xx）。
async fn test_metadata_provider(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let normalized = provider.trim().to_lowercase();
    if normalized != "javdb" {
        // 显式给 422：`ErrorResponse` 没有 `validation` 便捷构造（那是
        // service 层 `ServiceError::validation` 的事），端点层一律用 `new`。
        return Err(ErrorResponse::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_metadata_provider",
            format!("未知的元数据来源：{provider}"),
        ));
    }
    todo!("骨架：调 JavDB 探测；不可用时返回诊断结果而非 5xx（同 download-clients/test 的取舍）")
}
