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
//! | `GET /status/image-search` | `StatusService.get_image_search_status` | 阻塞：探测客户端（见 handler 文档）|
//! | `GET /status/metadata-providers/{provider}/test` | `StatusService.test_metadata_provider` | **已落**（host 照上游硬编码）|
//!
//! ⚠️ 上表倒数第二列原先写的是 `POST /status/metadata-provider/test` —— **两处都
//! 错**：动词是 **GET**（上游 `status.py:60` `@router.get`），路径有 `providers`
//! 的 `s` 且带 `{provider}` 段。代码一直是对的，错的是这张表；我照它去改代码，
//! 反而改坏了（详见下面 handler 的文档）。**这类表要当索引看，别当规格用。**
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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_service::system::optional_services::image_search_enabled;
use sm_service::system::status::{
    ImageSearchProbe, InsightsResource, StatusEmbeddingServiceSummary,
    StatusImageSearchIndexSpaceSummary, StatusImageSearchIndexingSummary,
    StatusImageSearchResource, StatusImageSearchVectorStoreSummary,
    StatusMetadataProviderTestError, StatusMetadataProviderTestResource, StatusResource,
    StatusService, TrendBucket, TrendGranularity, WatchTrendRange, WatchTrendResource,
};

use crate::auth::CurrentUser;
use crate::config::{snapshot_or_500, string_at};
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
        // 是 **GET**（上游 `status.py:60` `@router.get`）。
        //
        // ⚠️ 我一度把它「修」成了 POST，理由是「模块文档第 12 行写着 POST」——
        // 而那一行文档才是错的（见文件头表格的注）。**别照文档改代码，去读上游。**
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
/// # ⚠️ 骨架期这里写着「读数据库单例表，**不问 Qdrant**」—— 写反了
///
/// 上游 `StatusService.get_image_search_status`（`status_service.py:410-443`）
/// 在**图搜已启用**时会发起网络探测：
///
/// 1. `_probe_embedding_service()`（`:525`）—— `get_embedding_client().describe()`；
/// 2. `_probe_image_search_vector_store()`（`:549`）—— `get_qdrant_thumbnail_store().inspect_status()`；
/// 3. `_indexing_status()`（`:576`）—— 读 `MediaThumbnail` 的待处理/失败计数；
/// 4. `ImageSearchIndexSpaceService.get_status(...)` —— **这才轮到**
///    `image_search_index_state` 单例表。
///
/// 也就是说「单例表」只是返回体里 `index_space` 这一项的来源，**不是整个端点**。
/// 只有图搜**未启用**（`image_search_enabled()` 为假）时才是纯静态响应
/// （`:411-423`），不发任何网络请求。
///
/// 结论：本端点不是「读张表就行」。零件其实**都在** —— `EmbeddingClient::describe`、
/// `DenseStore::status`、`ImageSearchIndexSpaceService::get_status` 都已零 `todo!()`，
/// 缺的只是把它们从 `AppState` 递进服务层的那层接线（本仓没有上游那种模块级单例）。
/// 已落地，见 [`sm_service::system::status::StatusService::get_image_search_status`]。
async fn get_image_search_status(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<ImageSearchStatusResource>, ErrorResponse> {
    let snapshot = snapshot_or_500(&state)?;
    // ★ `enabled` 取**配置开关**，不是「服务存不存在」。开关开着但
    //   `inference_base_url` 为空时，组合根建不出服务；那种情况上游报
    //   `enabled: true` 并把原因写进 `embedding_service.error`，而不是「没开」。
    //   见 `ImageSearchProbe::enabled` 的文档。
    let probe = ImageSearchProbe {
        enabled: image_search_enabled(&snapshot),
        service: state.image_search().map(|service| &**service),
        // 未启用时也要回显这两个地址（上游 `:418-419` 用的是
        // `settings.qdrant.url` 与 `QdrantThumbnailStore.COLLECTION_NAME`）。
        inference_base_url: string_at(&snapshot, "image_search", "inference_base_url")
            .unwrap_or_default(),
        qdrant_url: string_at(&snapshot, "qdrant", "url").unwrap_or_default(),
    };
    let status = StatusService::new(state.db())
        .get_image_search_status(probe)
        .await?;
    Ok(Json(status.into()))
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
///
/// # 曾经卡在「JavDB 的 host 从哪来」—— 已拍板照上游**硬编码**
///
/// 零件其实一直都在：[`sm_service::catalog::javdb::JavdbProvider`] 已实现
/// `MetadataProvider::get_movie_by_number`（`javdb.rs:244`，零 `todo!()`）。
/// 缺的只是 **host**：全仓没有任何生产代码构造过它，也没有 host 常量或配置键；
/// 上游是硬编码（`metadata/factory.py:15`）。现在照抄，见
/// [`sm_service::system::status::JAVDB_HOST`]（那里写了为什么选硬编码而不是配置键）。
///
/// # 失败也回 **200**
///
/// 这是一份**诊断报告**：`healthy: false` + `error.type` 三选一
/// （`metadata_not_found` / `metadata_request_error` / `unexpected_error`）。
/// 与 [`super::download_clients`] 的 `test_download_client` 同一条取舍 ——
/// 回 5xx 会让客户端分不清「源挂了」与「本服务挂了」。形状由服务层的
/// `probe_javdb` 给出。
async fn test_metadata_provider(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(provider): Path<String>,
) -> Result<Json<MetadataProviderTestResource>, ErrorResponse> {
    let normalized = provider.trim().to_lowercase();
    if normalized != "javdb" {
        // 显式给 422：`ErrorResponse` 没有 `validation` 便捷构造（那是
        // service 层 `ServiceError::validation` 的事），端点层一律用 `new`。
        //
        // `details.provider` 回显**原始**入参（上游 `{"provider": provider}` ——
        // 是 `provider` 不是 `normalized_provider`），客户端据此看到自己到底发了
        // 什么。骨架期漏了这个 details。
        let mut details = serde_json::Map::new();
        details.insert(
            "provider".to_owned(),
            serde_json::Value::from(provider.as_str()),
        );
        return Err(ErrorResponse::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_metadata_provider",
            format!("未知的元数据来源：{provider}"),
        )
        .with_details(details));
    }
    // 传**归一化**后的值（上游 `StatusService.test_metadata_provider(normalized_provider)`）。
    Ok(Json(
        StatusService::test_metadata_provider(&normalized)
            .await
            .into(),
    ))
}

// ================================================================ 元数据源探测的响应体

/// `GET /status/metadata-providers/{provider}/test` 的响应
/// （上游 `StatusMetadataProviderTestResource`）。
///
/// 字段名与上游逐字一致（`SchemaModel` 没有 alias generator，就是 snake_case）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataProviderTestResource {
    pub healthy: bool,
    /// Pydantic `datetime` 字面量形状。
    pub checked_at: String,
    pub provider: String,
    pub movie_number: String,
    pub elapsed_ms: i64,
    /// 健康时为 `null`。**不省略键** —— 上游 pydantic 默认就输出 `null`。
    pub error: Option<MetadataProviderTestErrorResource>,
    /// 以下四项只在健康时有值（同样输出 `null`，不省略）。
    pub javdb_id: Option<String>,
    pub title: Option<String>,
    pub actors_count: Option<i64>,
    pub tags_count: Option<i64>,
}

/// 探测失败的原因（上游 `StatusMetadataProviderTestError`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataProviderTestErrorResource {
    /// 上游字段名就是 `type`。
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    pub method: Option<String>,
    pub url: Option<String>,
    pub resource: Option<String>,
    pub lookup_value: Option<String>,
}

impl From<StatusMetadataProviderTestError> for MetadataProviderTestErrorResource {
    fn from(value: StatusMetadataProviderTestError) -> Self {
        Self {
            error_type: value.error_type,
            message: value.message,
            method: value.method,
            url: value.url,
            resource: value.resource,
            lookup_value: value.lookup_value,
        }
    }
}

impl From<StatusMetadataProviderTestResource> for MetadataProviderTestResource {
    fn from(value: StatusMetadataProviderTestResource) -> Self {
        Self {
            healthy: value.healthy,
            checked_at: value.checked_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            provider: value.provider,
            movie_number: value.movie_number,
            elapsed_ms: value.elapsed_ms,
            error: value.error.map(Into::into),
            javdb_id: value.javdb_id,
            title: value.title,
            actors_count: value.actors_count,
            tags_count: value.tags_count,
        }
    }
}

// ================================================================ 图搜状态的响应体

/// `GET /status/image-search` 的响应（上游 `StatusImageSearchResource`）。
///
/// 字段名与上游逐字一致，**嵌套键也一致**（`embedding_service` /
/// `image_search_vector_store` / `indexing` / `index_space`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchStatusResource {
    pub enabled: bool,
    pub healthy: bool,
    /// Pydantic `datetime` 字面量形状。
    pub checked_at: String,
    pub embedding_service: EmbeddingServiceSummaryResource,
    pub image_search_vector_store: ImageSearchVectorStoreSummaryResource,
    pub indexing: ImageSearchIndexingSummaryResource,
    pub index_space: ImageSearchIndexSpaceSummaryResource,
}

/// 推理服务探测结果（上游 `StatusEmbeddingServiceSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingServiceSummaryResource {
    pub healthy: bool,
    pub endpoint: Option<String>,
    pub space_id: Option<String>,
    pub dimension: Option<u64>,
    /// 模态，升序。没有模态时输出 `[]`，**不是** `null`。
    pub modalities: Vec<String>,
    pub error: Option<String>,
}

/// 向量库探测结果（上游 `StatusImageSearchVectorStoreSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchVectorStoreSummaryResource {
    pub healthy: bool,
    pub url: String,
    pub collection_name: String,
    pub exists: bool,
    pub points_count: Option<u64>,
    pub vector_size: Option<u64>,
    pub vector_dtype: Option<String>,
    pub collection_status: Option<String>,
    pub error: Option<String>,
}

/// 索引积压（上游 `StatusImageSearchIndexingSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchIndexingSummaryResource {
    pub pending_thumbnails: i64,
    pub failed_thumbnails: i64,
}

/// 索引空间状态（上游 `StatusImageSearchIndexSpaceSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchIndexSpaceSummaryResource {
    /// `ready` / `rebuild_required` / `uninitialized` / `unavailable`。
    pub state: String,
    pub indexed_space_id: Option<String>,
    pub current_space_id: Option<String>,
    pub is_rebuilding: bool,
}

impl From<StatusImageSearchResource> for ImageSearchStatusResource {
    fn from(value: StatusImageSearchResource) -> Self {
        Self {
            enabled: value.enabled,
            healthy: value.healthy,
            checked_at: value.checked_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            embedding_service: value.embedding_service.into(),
            image_search_vector_store: value.image_search_vector_store.into(),
            indexing: value.indexing.into(),
            index_space: value.index_space.into(),
        }
    }
}

impl From<StatusEmbeddingServiceSummary> for EmbeddingServiceSummaryResource {
    fn from(value: StatusEmbeddingServiceSummary) -> Self {
        Self {
            healthy: value.healthy,
            endpoint: value.endpoint,
            space_id: value.space_id,
            dimension: value.dimension,
            modalities: value.modalities,
            error: value.error,
        }
    }
}

impl From<StatusImageSearchVectorStoreSummary> for ImageSearchVectorStoreSummaryResource {
    fn from(value: StatusImageSearchVectorStoreSummary) -> Self {
        Self {
            healthy: value.healthy,
            url: value.url,
            collection_name: value.collection_name,
            exists: value.exists,
            points_count: value.points_count,
            vector_size: value.vector_size,
            vector_dtype: value.vector_dtype,
            collection_status: value.collection_status,
            error: value.error,
        }
    }
}

impl From<StatusImageSearchIndexingSummary> for ImageSearchIndexingSummaryResource {
    fn from(value: StatusImageSearchIndexingSummary) -> Self {
        Self {
            pending_thumbnails: value.pending_thumbnails,
            failed_thumbnails: value.failed_thumbnails,
        }
    }
}

impl From<StatusImageSearchIndexSpaceSummary> for ImageSearchIndexSpaceSummaryResource {
    fn from(value: StatusImageSearchIndexSpaceSummary) -> Self {
        Self {
            state: value.state,
            indexed_space_id: value.indexed_space_id,
            current_space_id: value.current_space_id,
            is_rebuilding: value.is_rebuilding,
        }
    }
}
