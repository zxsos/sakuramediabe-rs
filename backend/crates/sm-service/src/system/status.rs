//! 状态页聚合，对应上游 `src/service/system/status_service.py`（602 行）。
//!
//! # 落地情况
//!
//! | 上游方法 | 端点 | 状态 |
//! |---|---|---|
//! | `get_status` | `GET /status` | **已落** |
//! | `get_insights` | `GET /status/insights` | **已落**（磁盘空间那三列为 `null`） |
//! | `get_watch_trend` | `GET /status/watch-trend` | **已落** |
//! | `test_metadata_provider` | `GET /status/metadata-providers/{provider}/test` | **已落**（host 照上游硬编码，见 [`JAVDB_HOST`]） |
//! | `get_image_search_status` | `GET /status/image-search` | 阻塞：需要 `discovery` 域的 embedding / Qdrant **探测客户端** |
//!
//! ⚠️ 上表原先写着 `test_metadata_provider` 的端点是
//! `POST /status/metadata-provider/test`，**两处都错**（动词上游是 `GET`，
//! `status.py:60`；路径有 `providers` 的 `s` 且带 `{provider}` 段）。那张错表
//! 还害得另一处照着它去改代码 —— 经过见 `docs/handoff.md` §7.2h。
//!
//! `get_image_search_status` 仍阻塞不是「没有依赖」：图搜那套（Qdrant store 与
//! embedding 客户端）已经落了大半，缺的是**健康探测**那一段 ——
//! 见 `crate::discovery` 与 `sm-api` 侧 handler 的文档。
//!
//! # `get_insights` 的磁盘空间三列返回 `null`
//!
//! 上游调 `MediaLibraryService.storage_space_usages()`，那是 `playback` 域
//! 的**真实磁盘探测**（`statvfs`）。这里没有那个 service，所以三个字段
//! 返回 `null` 而不是 `0` —— `0` 会被客户端渲染成「磁盘满了」，而事实是
//! 「没探测」。字段必须存在（客户端读它），值待补。这个取舍记在
//! `MediaLibraryUsage` 的字段文档里。
//!
//! # 观看趋势的时区
//!
//! 上游用 `get_runtime_timezone()` 把 UTC 时间戳转成**本地日期**再分桶 ——
//! 「今天看了什么」是按用户所在时区的日历日算的，不是 UTC 日。
//! 本仓库的 `sm_scheduler::RuntimeTimezone` 已经存在，直接复用；
//! 不引入「假 UTC」的分桶，否则跨时区用户的「今天」会整体偏移一天。

use std::time::Instant;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use sm_db::common::time::now_utc;
use sm_db::repo::discovery::PendingImageRepository;
use sm_db::repo::{
    BackgroundTaskRunRepository, ClipCollectionRepository, MomentCollectionRepository,
    PlaylistRepository, StatsRepository, VideoCollectionRepository,
};
use sm_db::transfers::downloads::{download_state, import_status};
use sm_db::Db;

use crate::catalog::javdb::JavdbProvider;
use crate::catalog::metadata_source::{MetadataProvider, MetadataSourceError};
use crate::discovery::image_search::ImageSearchService;
use crate::discovery::image_search_space::STATE_UNAVAILABLE;
use crate::discovery::qdrant::THUMBNAIL_COLLECTION;
use crate::error::ServiceError;

/// 后端版本的环境变量名。上游 `BACKEND_VERSION_ENV_KEY`。
pub const BACKEND_VERSION_ENV_KEY: &str = "SAKURAMEDIA_BACKEND_VERSION";

/// 未注入版本时的回退值。上游 `BACKEND_VERSION_DEFAULT`。
pub const BACKEND_VERSION_DEFAULT: &str = "dev-local";

/// 下载任务的六个用户视角桶。**顺序即响应字段顺序。**
///
/// 对应上游 `StatusService.DOWNLOAD_TASK_BUCKETS`。
pub const DOWNLOAD_TASK_BUCKETS: [&str; 6] = [
    "downloading",
    "importing",
    "imported",
    "import_failed",
    "skipped",
    "download_failed",
];

// ================================================================ 类型

/// `GET /status` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusResource {
    pub backend_version: String,
    pub actors: ActorSummary,
    pub movies: MovieSummary,
    pub media_files: MediaFileSummary,
    pub media_libraries: MediaLibrarySummary,
    pub thumbnails: ThumbnailSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActorSummary {
    pub female_total: i64,
    pub female_subscribed: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MovieSummary {
    pub total: i64,
    pub subscribed: i64,
    /// 有至少一条 `valid` 媒体的影片数。
    pub playable: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaFileSummary {
    pub total: i64,
    pub total_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaLibrarySummary {
    pub total: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThumbnailSummary {
    /// 还要生成缩略图的媒体数（口径与缩略图任务的候选查询一致）。
    pub pending_media: i64,
    /// 退避等待中的媒体数。
    pub retry_wait_media: i64,
    /// 终态失败的媒体数。
    pub terminal_failed_media: i64,
    /// 已产出的缩略图文件数。
    pub total: i64,
}

/// `GET /status/insights` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InsightsResource {
    pub download_tasks: DownloadTaskSummary,
    pub media_libraries: Vec<MediaLibraryUsage>,
    pub collections: CollectionsSummary,
}

/// 下载任务的六分类计数。
///
/// 六个桶之和**恒等于** `total` —— 上游用 `total=sum(counts.values())` 保证
/// 这一点，而它之所以能保证，是因为折叠函数对未知取值也返回某个桶
/// （见 [`download_bucket`]）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DownloadTaskSummary {
    pub total: i64,
    pub downloading: i64,
    pub importing: i64,
    pub imported: i64,
    pub import_failed: i64,
    pub skipped: i64,
    pub download_failed: i64,
}

impl DownloadTaskSummary {
    /// 从六个桶的计数构造，`total` 取**它们的和**而不是另行 COUNT。
    ///
    /// 这样 `total` 与六个桶在结构上不可能不一致 —— 少算一个分组就表现为
    /// 「六个桶加起来不等于总数」，而那正是最容易出问题的地方。
    pub fn from_buckets(counts: &DownloadTaskBuckets) -> Self {
        let downloading = counts.get("downloading");
        let importing = counts.get("importing");
        let imported = counts.get("imported");
        let import_failed = counts.get("import_failed");
        let skipped = counts.get("skipped");
        let download_failed = counts.get("download_failed");
        Self {
            total: downloading + importing + imported + import_failed + skipped + download_failed,
            downloading,
            importing,
            imported,
            import_failed,
            skipped,
            download_failed,
        }
    }
}

/// 六个桶的原始计数。缺失的桶按 0 计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadTaskBuckets {
    counts: std::collections::HashMap<&'static str, i64>,
}

impl DownloadTaskBuckets {
    /// 全 0 起步。**六个桶都必须先初始化为 0** —— 上游是
    /// `counts = {bucket: 0 for bucket in DOWNLOAD_TASK_BUCKETS}`，
    /// 少初始化一个就会在响应里少一个键（而不是少一个计数）。
    pub fn zeroed() -> Self {
        let mut counts = std::collections::HashMap::new();
        for bucket in DOWNLOAD_TASK_BUCKETS {
            counts.insert(bucket, 0);
        }
        Self { counts }
    }

    pub fn get(&self, bucket: &str) -> i64 {
        self.counts.get(bucket).copied().unwrap_or(0)
    }

    /// 累加。桶名未知时**静默丢弃**。
    ///
    /// 静默是刻意的：`download_bucket` 只返回 `DOWNLOAD_TASK_BUCKETS` 里的
    /// 名字（下面有测试钉住），所以这里不可能被用到；而真的 panic 会在
    /// 一个纯统计路径上把整个状态页打成 500。
    pub fn add(&mut self, bucket: &str, n: i64) {
        if let Some(slot) = self.counts.get_mut(bucket) {
            *slot += n;
        }
    }
}

/// 单个媒体库的占用。对应上游 `StatusMediaLibraryUsage`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaLibraryUsage {
    pub library_id: i32,
    pub name: String,
    pub provider_key: String,
    pub file_count: i64,
    pub total_size_bytes: i64,
    /// 磁盘总量。**`None` = 未探测**（本批没有 `playback` 域的
    /// `storage_space_usages`）。不是 `0` —— 0 会被渲染成「磁盘满了」。
    pub space_total_bytes: Option<i64>,
    /// 已用空间。`None` 同上。
    pub space_used_bytes: Option<i64>,
    /// 剩余空间。`None` 同上。
    pub space_free_bytes: Option<i64>,
}

/// 合集计数。`count` 是合集数、`item_count` 是成员行数。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionCounts {
    pub count: i64,
    pub item_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionsSummary {
    /// **不含系统列表**（最近播放）。与 `GET /playlists?include_system=false`
    /// 同一口径。
    pub playlists: CollectionCounts,
    pub video_collections: CollectionCounts,
    pub clip_collections: CollectionCounts,
    pub moment_collections: CollectionCounts,
}

/// 观看趋势的时间范围。对应上游 `StatusWatchTrendRange`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchTrendRange {
    Last7Days,
    #[default]
    Last30Days,
    Last90Days,
    LastYear,
    All,
}

impl WatchTrendRange {
    /// 解析查询参数。**大小写不敏感**，非法值返回 `None`。
    ///
    /// 上游是 enum，FastAPI 对非法值返回 **422**。这里返回 `Option`，
    /// 由路由层决定状态码与错误码。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "7d" | "last_7_days" => Some(Self::Last7Days),
            "30d" | "last_30_days" => Some(Self::Last30Days),
            "90d" | "last_90_days" => Some(Self::Last90Days),
            "1y" | "last_year" => Some(Self::LastYear),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// 该范围的**桶粒度**。
    ///
    /// 7/30/90 天按天，其余按月。上游 `WATCH_TREND_DAY_COUNTS` 只列了三个
    /// 按天的范围，`1y` 与 `all` 落到 `else` 分支即按月。
    pub fn granularity(self) -> TrendGranularity {
        match self {
            Self::Last7Days | Self::Last30Days | Self::Last90Days => TrendGranularity::Day,
            Self::LastYear | Self::All => TrendGranularity::Month,
        }
    }

    /// 该范围按天的桶数（含今天）。`None` = 不按天。
    pub fn day_count(self) -> Option<i64> {
        match self {
            Self::Last7Days => Some(7),
            Self::Last30Days => Some(30),
            Self::Last90Days => Some(90),
            Self::LastYear | Self::All => None,
        }
    }
}

/// 桶粒度。对应上游 `StatusWatchTrendGranularity`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendGranularity {
    Day,
    Month,
}

/// `GET /status/watch-trend` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTrendResource {
    pub range: WatchTrendRange,
    pub granularity: TrendGranularity,
    /// 窗口内**去重**看过多少部影片。
    pub watched_movie_count: i64,
    /// 逐桶计数。**桶数固定**（即使某桶为 0 也有），前端才能画连续坐标轴。
    pub buckets: Vec<TrendBucket>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendBucket {
    /// `YYYY-MM-DD`（按天）或 `YYYY-MM`（按月）。
    pub period: String,
    pub count: i64,
}

// ================================================================ 元数据源探测

/// JavDB 的 host（**不带** `https://`）。上游 `metadata/factory.py:15`
/// `JAVDB_HOST = "jdforrepam.com"` —— **硬编码**，本仓照抄。
///
/// # 这是一次**拍板**，不是顺手写死
///
/// 上游自己就是硬编码（没做成配置），所以「照抄」= 行为与上游逐字一致。代价是
/// 换域名要改代码 —— 真到那天再补 `metadata.javdb_host` 也不迟（那时才知道要
/// 不要校验、要不要热更新）。反过来先配置化，就得凭空定默认值、校验规则与
/// 文档，而这些没人要。取舍记在 `docs/handoff.md` §7.5。
///
/// 它同时是 `catalog` 那 6 条（`movie_metadata_refresh` / `movie_metadata_search`
/// 等）缺的同一块东西：**全仓原本没有任何生产代码构造过 `JavdbProvider`**。
pub const JAVDB_HOST: &str = "jdforrepam.com";

/// 探测用的固定番号。上游 `status_service.py:85`
/// `METADATA_PROVIDER_TEST_MOVIE_NUMBER = "SSNI-888"`。
pub const METADATA_PROVIDER_TEST_MOVIE_NUMBER: &str = "SSNI-888";

/// 探测失败的原因（上游 `StatusMetadataProviderTestError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMetadataProviderTestError {
    /// 上游字段名就是 `type`（Rust 是关键字，DTO 层 `rename`）。
    ///
    /// 上游三个取值：`metadata_not_found` / `metadata_request_error` /
    /// `unexpected_error`。
    pub error_type: String,
    pub message: String,
    /// 以下四个字段上游会带（请求错误带 `method` / `url`，没收录带 `resource` /
    /// `lookup_value`），但**本仓的 [`MetadataSourceError`] 不携带这些信息**
    /// —— 它只有一个 `String`。所以这四项恒为 `None`。
    ///
    /// 留着而不是删掉：字段在客户端契约里存在（上游总会输出它们，值为 `null`
    /// 时也一样），删掉会让响应少四个键。
    pub method: Option<String>,
    pub url: Option<String>,
    pub resource: Option<String>,
    pub lookup_value: Option<String>,
}

/// 元数据源探测报告（上游 `StatusMetadataProviderTestResource`）。
///
/// # 不健康也是 **200**
///
/// 这是一份**诊断报告**，不是请求失败 —— 与 `indexer-settings/test`、
/// `download-clients/test` 同一条取舍。返回 5xx 会让客户端无法区分
/// 「源挂了」与「本服务挂了」。
#[derive(Debug, Clone, PartialEq)]
pub struct StatusMetadataProviderTestResource {
    pub healthy: bool,
    /// naive UTC。DTO 层格式化成上游的 datetime 字面量。
    pub checked_at: NaiveDateTime,
    pub provider: String,
    pub movie_number: String,
    pub elapsed_ms: i64,
    /// 健康时为 `None`（DTO 层输出 `null`，**不省略键**）。
    pub error: Option<StatusMetadataProviderTestError>,
    /// 以下四项只在健康时有值。
    pub javdb_id: Option<String>,
    pub title: Option<String>,
    pub actors_count: Option<i64>,
    pub tags_count: Option<i64>,
}

/// `GET /status/image-search` 的响应体。上游 `StatusImageSearchResource`
/// （`schema/system/status.py:80-87`）。
#[derive(Debug, Clone, PartialEq)]
pub struct StatusImageSearchResource {
    /// 图搜是否启用 —— 即组合根有没有建 [`ImageSearchService`]。
    pub enabled: bool,
    /// **只看推理服务与向量库两项**（上游 `status_service.py:432`）。
    /// 索引空间要重建、有积压，都不影响它 —— 那些是「可查但降级」，不是故障。
    pub healthy: bool,
    /// naive UTC。DTO 层格式化成上游的 datetime 字面量。
    pub checked_at: NaiveDateTime,
    pub embedding_service: StatusEmbeddingServiceSummary,
    pub image_search_vector_store: StatusImageSearchVectorStoreSummary,
    pub indexing: StatusImageSearchIndexingSummary,
    pub index_space: StatusImageSearchIndexSpaceSummary,
}

/// 推理服务探测结果。上游 `StatusEmbeddingServiceSummary`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusEmbeddingServiceSummary {
    pub healthy: bool,
    /// 配置里的推理服务地址。**失败时也要回** —— 客户端要能显示「连的是哪儿」。
    pub endpoint: Option<String>,
    pub space_id: Option<String>,
    pub dimension: Option<u64>,
    /// 模态，**升序**（上游那句 `sorted(...)`）。
    pub modalities: Vec<String>,
    pub error: Option<String>,
}

/// 向量库探测结果。上游 `StatusImageSearchVectorStoreSummary`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusImageSearchVectorStoreSummary {
    pub healthy: bool,
    pub url: String,
    pub collection_name: String,
    pub exists: bool,
    /// 集合不存在时为 `None`（上游早返回分支）。
    pub points_count: Option<u64>,
    pub vector_size: Option<u64>,
    /// REST 风格小写串，如 `float16`。字面量来源见 `qdrant::dense::datatype_name`。
    pub vector_dtype: Option<String>,
    /// REST 风格小写串，如 `green`。
    pub collection_status: Option<String>,
    pub error: Option<String>,
}

/// 索引积压。上游 `StatusImageSearchIndexingSummary`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusImageSearchIndexingSummary {
    pub pending_thumbnails: i64,
    pub failed_thumbnails: i64,
}

/// 索引空间状态。上游 `StatusImageSearchIndexSpaceSummary`。
///
/// 刻意**不实现 `Default`**：`state` 是四值枚举的字面量，让它能默认为 `""`
/// 只会给未来的自己留一个能编译的错值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusImageSearchIndexSpaceSummary {
    /// 四值之一，见 [`crate::discovery::image_search_space`] 的 `STATE_*`。
    pub state: String,
    pub indexed_space_id: Option<String>,
    pub current_space_id: Option<String>,
    /// 有 `image_search_index` 任务在队 / 在跑，且那次运行带 `reset`。
    pub is_rebuilding: bool,
}

/// `GET /status/image-search` 需要的外部依赖。
///
/// 上游用模块级单例（`image_search_enabled()` / `get_embedding_client()` /
/// `get_qdrant_thumbnail_store()`）取这几样；本仓**没有全局注册表** —— 组合根在
/// `sm-api`，实例都挂在 `AppState` 上。所以由路由层显式传进来。
///
/// [`ImageSearchProbe::service`] 为 `None` 即「未启用」：组合根根本没建
/// [`ImageSearchService`]，此时两个地址仍要照配置回显。
pub struct ImageSearchProbe<'a> {
    /// **配置层面**的开关，即 `image_search_enabled()`（`qdrant` 与
    /// `image_search` 两个 `enabled` 都为真）。
    ///
    /// ⚠️ 刻意与 `service.is_some()` 分开：开关开着但
    /// `image_search.inference_base_url` 为空时，组合根**建不出**服务
    /// （`sm-server` 会 warn 后返回 `None`），而上游此时仍报 `enabled: true`。
    /// 两者合一就会把「配置错误」谎报成「功能没开」。
    pub enabled: bool,
    pub service: Option<&'a ImageSearchService>,
    /// 配置里的推理服务地址。
    pub inference_base_url: &'a str,
    /// 配置里的 Qdrant 地址。
    pub qdrant_url: &'a str,
}

/// 探推理服务。上游 `_probe_embedding_service`（`status_service.py:524-546`）。
///
/// **从不失败**：连不上也要交出一份带 `error` 的报告 —— 那正是本端点的用途。
///
/// 上游把 `EmbeddingClientError`（取 `.message`）与其它异常（取 `str(exc)`）
/// 分成两支、错误文案略不同；本仓 `EmbeddingClient::describe` 统一返回
/// [`ServiceError`]，没有这个区分，两支并成一支。
async fn probe_embedding_service(
    service: &ImageSearchService,
    endpoint: &str,
) -> StatusEmbeddingServiceSummary {
    match service.embedding().describe().await {
        Ok(space) => StatusEmbeddingServiceSummary {
            healthy: true,
            endpoint: Some(endpoint.to_owned()),
            space_id: Some(space.space_id),
            dimension: Some(space.dimension as u64),
            // `BTreeSet` 迭代本来就是升序 —— 上游那句 `sorted(...)` 在这里是免费的。
            modalities: space.modalities.into_iter().collect(),
            error: None,
        },
        Err(error) => StatusEmbeddingServiceSummary {
            healthy: false,
            endpoint: Some(endpoint.to_owned()),
            // 取 `message` 而不是整个错误：上游这里用的是 `EmbeddingClientError.message`
            // （`status_service.py:534`），响应里只放那句文案。
            error: Some(error.api.message.clone()),
            ..StatusEmbeddingServiceSummary::default()
        },
    }
}

/// 探向量库。上游 `_probe_image_search_vector_store`（`status_service.py:548-573`）。
///
/// **从不失败**：健康与错误都走报告。
///
/// ★ 集合**不存在也算健康**：那只说明首次索引还没跑，不是故障。`status()` 能用
/// `Ok` 回来就代表 Qdrant 答了话；真连不上时它会 `Err`（见 `DenseStore::status`
/// 里那段「刻意不复用 `exists()`」）。上游 `inspect_status` 同样只在异常时才
/// 把健康置false。
async fn probe_vector_store(
    service: &ImageSearchService,
    qdrant_url: &str,
) -> StatusImageSearchVectorStoreSummary {
    let store = service.store();
    let collection_name = store.collection_name().to_owned();
    match store.status().await {
        Ok(status) => StatusImageSearchVectorStoreSummary {
            healthy: true,
            url: qdrant_url.to_owned(),
            collection_name,
            exists: status.exists,
            points_count: status.exists.then_some(status.points),
            vector_size: status.vector_size,
            vector_dtype: status.vector_dtype,
            collection_status: status.collection_status,
            error: None,
        },
        Err(error) => StatusImageSearchVectorStoreSummary {
            healthy: false,
            url: qdrant_url.to_owned(),
            collection_name,
            exists: false,
            error: Some(error.api.message.clone()),
            ..StatusImageSearchVectorStoreSummary::default()
        },
    }
}

// ================================================================ service

/// 状态页 service。
#[derive(Debug, Clone)]
pub struct StatusService {
    stats: StatsRepository,
    playlists: PlaylistRepository,
    video_collections: VideoCollectionRepository,
    moment_collections: MomentCollectionRepository,
    clip_collections: ClipCollectionRepository,
    /// 图搜索引的缩略图计数（`GET /status/image-search`）。
    pending_images: PendingImageRepository,
    /// 图搜索引任务是否在跑（同上）。
    task_runs: BackgroundTaskRunRepository,
}

impl StatusService {
    pub fn new(db: &Db) -> Self {
        Self {
            stats: StatsRepository::new(db.clone()),
            playlists: PlaylistRepository::new(db.clone()),
            video_collections: VideoCollectionRepository::new(db.clone()),
            moment_collections: MomentCollectionRepository::new(db.clone()),
            clip_collections: ClipCollectionRepository::new(db.clone()),
            pending_images: PendingImageRepository::new(db.clone()),
            task_runs: BackgroundTaskRunRepository::new(db.clone()),
        }
    }

    /// `GET /status/image-search`。
    ///
    /// 上游 `StatusService.get_image_search_status`（`status_service.py:409-443`）。
    ///
    /// # 未启用时**什么都不碰**
    ///
    /// `probe.service` 为 `None` 就返回一串静态值，**不探推理服务、不探 Qdrant、
    /// 不查库**（上游同理）。但 Qdrant 地址与集合名仍要回 —— 客户端靠它们显示
    /// 「配置指向哪儿」。
    ///
    /// # 与上游唯一的实质差异：依赖是注入的
    ///
    /// 上游 `cls._probe_*` 内部直接摸模块级单例；本仓把依赖收进
    /// [`ImageSearchProbe`]，由路由层从 `AppState` 取。行为一致，只是没有全局态。
    pub async fn get_image_search_status(
        &self,
        probe: ImageSearchProbe<'_>,
    ) -> Result<StatusImageSearchResource, ServiceError> {
        let checked_at = now_utc();
        if !probe.enabled {
            return Ok(StatusImageSearchResource {
                enabled: false,
                healthy: false,
                checked_at,
                embedding_service: StatusEmbeddingServiceSummary::default(),
                image_search_vector_store: StatusImageSearchVectorStoreSummary {
                    url: probe.qdrant_url.to_owned(),
                    collection_name: THUMBNAIL_COLLECTION.to_owned(),
                    ..StatusImageSearchVectorStoreSummary::default()
                },
                indexing: StatusImageSearchIndexingSummary::default(),
                index_space: StatusImageSearchIndexSpaceSummary {
                    state: STATE_UNAVAILABLE.to_owned(),
                    indexed_space_id: None,
                    current_space_id: None,
                    is_rebuilding: false,
                },
            });
        }

        // 开关开着却没有服务：唯一来源是 `inference_base_url` 为空（组合根 warn
        // 后不建）。上游此时仍会走进探测 —— 推理服务那项报错，但 Qdrant 那项
        // **能成功**。这里没有客户端可探，只能把两项都报成失败并写明原因。
        //
        // ★ 这是**已知偏差**，且只出现在一种配置错误下；两种表现都指向同一件
        // 事（去配 `inference_base_url`），比谎报 `enabled: false` 好得多。
        let Some(service) = probe.service else {
            return Ok(StatusImageSearchResource {
                enabled: true,
                healthy: false,
                checked_at,
                embedding_service: StatusEmbeddingServiceSummary {
                    healthy: false,
                    endpoint: Some(probe.inference_base_url.to_owned()),
                    error: Some("图搜已启用但推理服务地址为空，未能建立图搜服务".to_owned()),
                    ..StatusEmbeddingServiceSummary::default()
                },
                image_search_vector_store: StatusImageSearchVectorStoreSummary {
                    url: probe.qdrant_url.to_owned(),
                    collection_name: THUMBNAIL_COLLECTION.to_owned(),
                    error: Some("图搜服务未建立，向量库未能探测".to_owned()),
                    ..StatusImageSearchVectorStoreSummary::default()
                },
                indexing: self.image_search_indexing_summary().await?,
                index_space: StatusImageSearchIndexSpaceSummary {
                    state: STATE_UNAVAILABLE.to_owned(),
                    indexed_space_id: None,
                    current_space_id: None,
                    is_rebuilding: false,
                },
            });
        };

        // 两个探测都不返回 `Err`：「连不上」也是一种要**报告**的结果，
        // 不该把整页打成 500（同 `download-clients/test` 的取舍）。
        let embedding_service = probe_embedding_service(service, probe.inference_base_url).await;
        let image_search_vector_store = probe_vector_store(service, probe.qdrant_url).await;
        // 这两个才真会失败（要查库），失败即 500 —— 与上游一致：
        // 上游这两句没有 try/except。
        let indexing = self.image_search_indexing_summary().await?;
        // 推理服务不健康时不传空间号：让状态机走 `unavailable` 分支，
        // 从而跳过那次 `has_completed_index_records`（要 EXISTS 两张表）。
        let current_space_id = if embedding_service.healthy {
            embedding_service.space_id.as_deref()
        } else {
            None
        };
        let space = service.space().get_status(current_space_id).await?;
        Ok(StatusImageSearchResource {
            enabled: true,
            healthy: embedding_service.healthy && image_search_vector_store.healthy,
            checked_at,
            embedding_service,
            image_search_vector_store,
            indexing,
            index_space: StatusImageSearchIndexSpaceSummary {
                state: space.state,
                indexed_space_id: space.indexed_space_id,
                current_space_id: space.current_space_id,
                is_rebuilding: self.is_image_search_rebuilding().await?,
            },
        })
    }

    /// 缩略图的索引积压。上游 `_indexing_status`（`status_service.py:575-590`）。
    ///
    /// **只数 `media_thumbnail`**：既不 join `movie`，也不并 `movie_plot_image`
    /// —— 与 `PendingImageRepository::pending_count` 是两个口径，理由见那个方法。
    async fn image_search_indexing_summary(
        &self,
    ) -> Result<StatusImageSearchIndexingSummary, ServiceError> {
        use sm_db::playback::media::image_search_index_status::{FAILED, PENDING};

        Ok(StatusImageSearchIndexingSummary {
            pending_thumbnails: self
                .pending_images
                .count_thumbnails_with_status(PENDING)
                .await?,
            failed_thumbnails: self
                .pending_images
                .count_thumbnails_with_status(FAILED)
                .await?,
        })
    }

    /// 是否有 `image_search_index` 任务在队 / 在跑且带 `reset`。
    /// 上游 `_is_image_search_rebuilding`（`status_service.py:592-602`）。
    ///
    /// 上游取的是**那一行的** `params.reset`：查询没有 `ORDER BY`，多行时取到哪
    /// 一行由数据库决定。本仓照抄这个语义（含这点不体面），见
    /// `BackgroundTaskRunRepository::find_active_by_task_key`。
    ///
    /// `params` 是 JSON 文本列；解析失败按「没有 reset」处理 —— 与上游
    /// `(params or {}).get("reset") is True` 在 `params` 为空时同路。
    async fn is_image_search_rebuilding(&self) -> Result<bool, ServiceError> {
        /// 图搜索引任务的 `task_key`（上游 `status_service.py:597` 的字面量）。
        const TASK_KEY: &str = "image_search_index";

        let Some(run) = self.task_runs.find_active_by_task_key(TASK_KEY).await? else {
            return Ok(false);
        };
        let reset = run
            .params
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|params| params.get("reset").and_then(serde_json::Value::as_bool));
        Ok(reset == Some(true))
    }

    /// `GET /status/metadata-providers/{provider}/test`。
    ///
    /// 上游 `StatusService.test_metadata_provider`（`status_service.py:446-483`）。
    ///
    /// # 不返回 `Result`
    ///
    /// 上游这个方法**从不抛**：三种异常全被捕成 `error` 字段里的报告。所以这里
    /// 也用不返回 `Result` 的签名 —— 让「它一定给得出报告」这件事在类型上就成立，
    /// 调用方不必编一个不可能发生的错误分支。
    ///
    /// 建 provider 失败（host 常量写坏）也走报告：那正是本端点的用途。
    pub async fn test_metadata_provider(provider: &str) -> StatusMetadataProviderTestResource {
        let normalized = provider.trim().to_lowercase();
        let start = Instant::now();
        if normalized != "javdb" {
            // 路由层已用 422 挡住非 javdb（上游同理），所以这条是**不可达**的兜底
            // —— 上游那个 `raise ValueError(...)` 同样只在绕过路由时才会走到。
            return failed_report(
                &normalized,
                start,
                "unexpected_error",
                format!("不支持的元数据来源：{provider}"),
            );
        }
        let client = match JavdbProvider::new(JAVDB_HOST) {
            Ok(client) => client,
            Err(error) => {
                return failed_report(&normalized, start, "unexpected_error", describe(&error));
            }
        };
        probe_javdb(&client, &normalized, start).await
    }
}

/// 用**注入的** provider 探测 —— 与 [`StatusService::test_metadata_provider`] 同一
/// 条路径，只是 provider 由调用方给。
///
/// # 为什么留这个缝
///
/// 生产那条把 host 写死在常量里（照上游），于是**没法**指向本地假 JavDB ——
/// 而「健康时四个统计字段填对没有」只有真发一次请求才测得到。这个缝就是为此
/// 存在，与 [`JavdbProvider::with_base_url`] 的理由一致。
///
/// `started` 由调用方给：让 `elapsed_ms` 覆盖「建 provider + 发请求」整段，
/// 与上游在 `test_metadata_provider` 开头取 `start_at` 一致。
pub async fn probe_javdb<P: MetadataProvider>(
    client: &P,
    provider: &str,
    started: Instant,
) -> StatusMetadataProviderTestResource {
    match client
        .get_movie_by_number(METADATA_PROVIDER_TEST_MOVIE_NUMBER)
        .await
    {
        // 上游 `_test_javdb_provider`：健康时四个统计字段来自 `detail`。
        Ok(Some(movie)) => StatusMetadataProviderTestResource {
            healthy: true,
            checked_at: now_utc(),
            provider: provider.to_owned(),
            movie_number: METADATA_PROVIDER_TEST_MOVIE_NUMBER.to_owned(),
            elapsed_ms: elapsed_ms(started),
            error: None,
            javdb_id: movie
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            // 上游是 `movie.get("title") or ""` —— 恒有值（可能空串），不是 `None`。
            title: Some(
                movie
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            actors_count: Some(count_movie_actors(movie.get("actors"))),
            tags_count: Some(count_movie_tags(movie.get("tags"))),
        },
        // 上游这里其实是抛 `MetadataNotFoundError`（`data.movie` 为空时），本仓
        // provider 把它收成了 `Ok(None)` —— 归到同一档。
        Ok(None) => failed_report(
            provider,
            started,
            "metadata_not_found",
            format!("JavDB 没有 {METADATA_PROVIDER_TEST_MOVIE_NUMBER}"),
        ),
        Err(error) => {
            let (error_type, message) = classify(&error);
            failed_report(provider, started, error_type, message)
        }
    }
}

/// 失败报告的公共部分（上游 `_build_metadata_provider_failure`）。
fn failed_report(
    provider: &str,
    started: Instant,
    error_type: &str,
    message: String,
) -> StatusMetadataProviderTestResource {
    StatusMetadataProviderTestResource {
        healthy: false,
        checked_at: now_utc(),
        provider: provider.to_owned(),
        movie_number: METADATA_PROVIDER_TEST_MOVIE_NUMBER.to_owned(),
        elapsed_ms: elapsed_ms(started),
        error: Some(StatusMetadataProviderTestError {
            error_type: error_type.to_owned(),
            message,
            // 本仓的 `MetadataSourceError` 不带这四项 —— 见字段文档。
            method: None,
            url: None,
            resource: None,
            lookup_value: None,
        }),
        javdb_id: None,
        title: None,
        actors_count: None,
        tags_count: None,
    }
}

/// [`MetadataSourceError`] → 上游的三个 `error.type`（`status_service.py:451-482`）。
///
/// - `NotFound` → `metadata_not_found`（上游 `MetadataNotFoundError`）
/// - `RequestFailed` → `metadata_request_error`（上游 `MetadataRequestError`）
/// - 其余 → `unexpected_error`（上游那个 `except Exception` 兜底）
fn classify(error: &MetadataSourceError) -> (&'static str, String) {
    match error {
        MetadataSourceError::NotFound => (
            "metadata_not_found",
            format!("JavDB 没有 {METADATA_PROVIDER_TEST_MOVIE_NUMBER}"),
        ),
        MetadataSourceError::RequestFailed(message) => ("metadata_request_error", message.clone()),
        MetadataSourceError::InvalidDelivery(message) | MetadataSourceError::Disabled(message) => {
            ("unexpected_error", message.clone())
        }
    }
}

/// provider 构造失败时的消息（构造只可能因 host 空而失败）。
fn describe(error: &MetadataSourceError) -> String {
    match error {
        MetadataSourceError::RequestFailed(message) => message.clone(),
        other => format!("{other:?}"),
    }
}

fn elapsed_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// 数 `actors` 里**有效**的条目。上游 `_build_movie_actors`（`javdb.py:920-946`）：
/// 非对象跳过，**`id` 为假值也跳过**。
///
/// 字段不是数组（或缺失）时返回 0 —— 上游 `_normalize_movie_list_field`
/// （`javdb.py:838-860`）把「null / 不是 list」都当成空列表。
fn count_movie_actors(value: Option<&serde_json::Value>) -> i64 {
    value
        .and_then(serde_json::Value::as_array)
        .map_or(0, |entries| {
            entries
                .iter()
                .filter(|entry| entry.is_object() && actor_id_is_truthy(entry.get("id")))
                .count() as i64
        })
}

/// 上游 `if not actor_id: continue` —— Python 的假值包括 `None` / `0` / `""`。
/// 这里覆盖实际会出现的两种：空串与非 0 数字。
fn actor_id_is_truthy(id: Option<&serde_json::Value>) -> bool {
    match id {
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(serde_json::Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

/// 数 `tags` 里有效的条目。上游 `_build_movie_tags`（`javdb.py:1000-1018`）：
/// **只跳过非对象**，不像演员那样还查 `id`（它用 `str(tag.get("id", ""))` 兜空）。
fn count_movie_tags(value: Option<&serde_json::Value>) -> i64 {
    value
        .and_then(serde_json::Value::as_array)
        .map_or(0, |entries| {
            entries.iter().filter(|entry| entry.is_object()).count() as i64
        })
}

impl StatusService {
    /// `GET /status`。
    ///
    /// 五组标量查询。**顺序与上游一致**，虽然它们互相独立 —— 写成并发
    /// 更好但会让「先查谁」的顺序变成不确定的，而状态页的快照一致性
    /// 本来就无法保证（没有事务）。保持顺序可读。
    pub async fn get_status(&self) -> Result<StatusResource, ServiceError> {
        Ok(StatusResource {
            backend_version: resolve_backend_version(),
            actors: ActorSummary {
                female_total: self.stats.female_actor_count().await?,
                female_subscribed: self.stats.female_actor_subscribed_count().await?,
            },
            movies: MovieSummary {
                total: self.stats.movie_count().await?,
                subscribed: self.stats.subscribed_movie_count().await?,
                playable: self.stats.playable_movie_count().await?,
            },
            media_files: MediaFileSummary {
                total: self.stats.media_count().await?,
                total_size_bytes: self.stats.media_total_size_bytes().await?,
            },
            media_libraries: MediaLibrarySummary {
                total: self.stats.media_library_count().await?,
            },
            thumbnails: ThumbnailSummary {
                pending_media: self.stats.pending_thumbnail_media_count().await?,
                retry_wait_media: self.stats.retry_wait_thumbnail_media_count().await?,
                terminal_failed_media: self.stats.terminal_failed_thumbnail_media_count().await?,
                total: self.stats.thumbnail_total().await?,
            },
        })
    }

    /// `GET /status/insights`。
    pub async fn get_insights(&self) -> Result<InsightsResource, ServiceError> {
        Ok(InsightsResource {
            download_tasks: self.download_task_summary().await?,
            media_libraries: self.media_library_usages().await?,
            collections: self.collection_summaries().await?,
        })
    }

    /// 下载任务按 `state × import_status` 折叠成六分类。
    async fn download_task_summary(&self) -> Result<DownloadTaskSummary, ServiceError> {
        let mut buckets = DownloadTaskBuckets::zeroed();
        for (state, import, total) in self.stats.download_task_groups().await? {
            buckets.add(download_bucket(&state, &import), total);
        }
        Ok(DownloadTaskSummary::from_buckets(&buckets))
    }

    /// 每个媒体库一栏，**以媒体库表为基准**遍历。
    ///
    /// 基准的选择是刻意的：以库表为准才能让「配了库但一个文件都没进去」
    /// 出现在结果里 —— 那是一个需要被看见的状态（配置好了、库挂载了，
    /// 但扫描没产出任何文件）。反过来（以 `media` 为基准）那个库会消失。
    async fn media_library_usages(&self) -> Result<Vec<MediaLibraryUsage>, ServiceError> {
        let usage: std::collections::HashMap<i32, (i64, i64)> = self
            .stats
            .media_usage_by_library()
            .await?
            .into_iter()
            .map(|(library_id, file_count, total_size_bytes)| {
                (library_id, (file_count, total_size_bytes))
            })
            .collect();

        // 库表本身没有仓储方法返回全部行（只有分页），所以这里直接查 ——
        // 状态页要的是**全部**库，分页会让它只看到前 20 个。
        let libraries: Vec<(i32, String, String)> =
            sqlx::query_as("SELECT id, name, provider_key FROM media_library ORDER BY id")
                .fetch_all(self.stats.pool())
                .await?;

        Ok(libraries
            .into_iter()
            .map(|(library_id, name, provider_key)| {
                let (file_count, total_size_bytes) =
                    usage.get(&library_id).copied().unwrap_or((0, 0));
                MediaLibraryUsage {
                    library_id,
                    name,
                    provider_key,
                    file_count,
                    total_size_bytes,
                    // 三个空间字段恒为 None —— 见类型文档。
                    space_total_bytes: None,
                    space_used_bytes: None,
                    space_free_bytes: None,
                }
            })
            .collect())
    }

    /// 四类合集的 (合集数, 成员数)。
    async fn collection_summaries(&self) -> Result<CollectionsSummary, ServiceError> {
        let (playlist_count, playlist_items) = self.playlists.collection_counts(false).await?;
        let (video_count, video_items) = self.video_collections.collection_counts().await?;
        let (clip_count, clip_items) = self.clip_collections.collection_counts().await?;
        let (moment_count, moment_items) = self.moment_collections.collection_counts().await?;
        Ok(CollectionsSummary {
            playlists: CollectionCounts {
                count: playlist_count,
                item_count: playlist_items,
            },
            video_collections: CollectionCounts {
                count: video_count,
                item_count: video_items,
            },
            clip_collections: CollectionCounts {
                count: clip_count,
                item_count: clip_items,
            },
            moment_collections: CollectionCounts {
                count: moment_count,
                item_count: moment_items,
            },
        })
    }

    /// `GET /status/watch-trend`。
    ///
    /// `timezone_offset_hours` 是**运行时**时区相对 UTC 的偏移（可为负）。
    /// 上游用 `get_runtime_timezone()`，本仓库用显式参数 ——
    /// 让「本地日」的算法可以被单元测试钉住，而不是依赖机器的 TZ。
    pub async fn get_watch_trend(
        &self,
        range: WatchTrendRange,
        timezone_offset_hours: i64,
    ) -> Result<WatchTrendResource, ServiceError> {
        let granularity = range.granularity();
        let today = local_today(timezone_offset_hours);
        let start = self.trend_start_date(range, today).await?;

        // 没有起点 -> 空桶列表。上游 `ALL` 范围下无记录时同样返回空。
        let Some(start) = start else {
            return Ok(WatchTrendResource {
                range,
                granularity,
                watched_movie_count: 0,
                buckets: Vec::new(),
            });
        };

        // 半开区间 [本地当日零点, 本地明日零点)。闭区间会把窗口外的脏
        // 时间戳算进来，而分桶时它会落进错误的桶。
        let window_start = local_day_start_utc(start, timezone_offset_hours);
        let window_end = local_day_start_utc(today + Duration::days(1), timezone_offset_hours);

        let rows = self
            .stats
            .watched_progress(window_start, window_end)
            .await?;

        // 逐桶去重：同一部影片一天内看多次只算一次。
        // 用 `HashMap<period, HashSet<movie>>` 而不是直接计数。
        let mut per_period: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
        let mut all_movies: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (watched_at, movie_number) in rows {
            let period = trend_period(to_local(watched_at, timezone_offset_hours), granularity);
            all_movies.insert(movie_number.clone());
            per_period.entry(period).or_default().insert(movie_number);
        }

        let buckets = trend_periods(start, today, granularity)
            .into_iter()
            .map(|period| {
                let count = per_period.get(&period).map_or(0, |set| set.len() as i64);
                TrendBucket { period, count }
            })
            .collect();
        Ok(WatchTrendResource {
            range,
            granularity,
            watched_movie_count: all_movies.len() as i64,
            buckets,
        })
    }

    /// 窗口起点。`None` = 由最早的观看记录决定（`ALL`），而现在没有记录。
    async fn trend_start_date(
        &self,
        range: WatchTrendRange,
        today: NaiveDate,
    ) -> Result<Option<NaiveDate>, ServiceError> {
        if let Some(days) = range.day_count() {
            // 含今天：`30` 天 = 今天往前 29 天。
            return Ok(Some(today - Duration::days(days - 1)));
        }
        if range == WatchTrendRange::LastYear {
            // 本月 1 号往前推 11 个月 = 含本月共 12 个桶。
            // `from_ymd_opt` 对合法输入不会失败，而这里构造的月/年都直接
            // 取自 `today`，所以 `expect` 不可达。
            let first_of_this_month = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .expect("本年本月是合法日期");
            return Ok(Some(shift_month(
                first_of_this_month,
                -(LAST_YEAR_MONTHS - 1),
            )));
        }
        // ALL：起点是最早一次有效观看。没有记录就是 None。
        Ok(self
            .stats
            .earliest_watched_at()
            .await?
            .map(|earliest| to_local(earliest, 0).date()))
    }
}

// ================================================================ 自由函数

/// `LAST_YEAR` 的桶数。上游 `WATCH_TREND_LAST_YEAR_MONTHS = 12`。
pub const LAST_YEAR_MONTHS: i64 = 12;

/// 把 `state × import_status` 折叠成用户视角的桶。**返回的六个名字必在其中。**
///
/// 对应上游 `_download_task_bucket`（`status_service.py:320`）。三条规则：
///
/// 1. `state = completed` 时看 `import_status`：
///    `completed` → `imported`；`skipped` → `skipped`；
///    `pending`/`running` → `importing`；**其余** → `import_failed`。
/// 2. `state = failed` → `download_failed`。
/// 3. 其余（`queued` / `submitted` / `downloading` / 任何未知值）→
///    `downloading`。
///
/// # 最后那条 `else` 是**契约**，不是兜底
///
/// 上游注释写明「未知取值统一向非终态桶靠，保证总数不漏」。所以一个从旧版本
/// 升级后出现的、新 `state` 字面量，会被算进「下载中」而不是消失 ——
/// 六个桶之和恒等于 `total` 就是靠这一条。若改成丢弃，状态页会安静地少算。
pub fn download_bucket(state: &str, import: &str) -> &'static str {
    if state == download_state::COMPLETED {
        return match import {
            import_status::COMPLETED => "imported",
            import_status::SKIPPED => "skipped",
            // 上游 `UNFINISHED_IMPORT_STATUSES` —— 还在途的算「导入中」
            other if import_status::UNFINISHED.contains(&other) => "importing",
            _ => "import_failed",
        };
    }
    if state == download_state::FAILED {
        return "download_failed";
    }
    "downloading"
}

/// 后端版本：环境变量优先，缺省 `dev-local`。
pub fn resolve_backend_version() -> String {
    std::env::var(BACKEND_VERSION_ENV_KEY)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| BACKEND_VERSION_DEFAULT.to_owned())
}

/// 运行时「今天」（按偏移后的本地日期）。
pub fn local_today(offset_hours: i64) -> NaiveDate {
    (sm_db::common::time::now_utc() + Duration::hours(offset_hours)).date()
}

/// UTC 时间戳 → 本地 naive 时间。
pub fn to_local(utc: NaiveDateTime, offset_hours: i64) -> NaiveDateTime {
    utc + Duration::hours(offset_hours)
}

/// 本地某日 00:00 对应的 UTC 时刻。
///
/// 上游是 `to_db_utc_naive(datetime.combine(local_date, midnight), assume_tz=tz)` ——
/// **假设**本地时区是**整小时**偏移。真实时区可能有半小时（如印度 `+05:30`），
/// 那会让窗口整体偏移 30 分钟。这里保留「整小时」的前提并写明：偏移由调用方
/// 传入，而本仓库的调度器同样按整小时处理时区（见
/// `sm_scheduler::RuntimeTimezone`）。两者一致比各自精确更重要 ——
/// 不一致会让「今天看的」和「今天排的任务」落在不同窗口里。
pub fn local_day_start_utc(local_date: NaiveDate, offset_hours: i64) -> NaiveDateTime {
    local_date.and_hms_opt(0, 0, 0).expect("00:00 是合法时刻") - Duration::hours(offset_hours)
}

/// 粒度对应的桶标签。
///
/// 收 `NaiveDateTime` 而不是 `NaiveDate`：上游 `_watch_trend_period` 收的是
/// `date | datetime`，而分桶的实际输入是转换后的本地**时刻**。两种都支持
/// 在这里统一成时刻，代价是按天分桶时多取一次 `.date()`。
pub fn trend_period(local: NaiveDateTime, granularity: TrendGranularity) -> String {
    match granularity {
        TrendGranularity::Day => local.date().format("%Y-%m-%d").to_string(),
        // 按月分桶时**只到月**：`%Y-%m`。多带一个日会让每个月的桶都不同。
        TrendGranularity::Month => local.format("%Y-%m").to_string(),
    }
}

/// 窗口内的桶标签序列。**空桶也产出**（计数为 0）。
pub fn trend_periods(
    start: NaiveDate,
    today: NaiveDate,
    granularity: TrendGranularity,
) -> Vec<String> {
    // 桶标签只用到日期部分，所以统一在 00:00 上计算 —— 日与月两种粒度
    // 都与「时刻里的时分秒」无关。
    let at_midnight = |date: NaiveDate| date.and_hms_opt(0, 0, 0).expect("00:00 是合法时刻");

    match granularity {
        TrendGranularity::Day => {
            let mut out = Vec::new();
            let mut cursor = start;
            while cursor <= today {
                out.push(trend_period(at_midnight(cursor), granularity));
                cursor += Duration::days(1);
            }
            out
        }
        TrendGranularity::Month => {
            let last_month = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                .expect("本月 1 号是合法日期");
            let mut cursor =
                NaiveDate::from_ymd_opt(start.year(), start.month(), 1).expect("合法日期");
            let mut out = Vec::new();
            while cursor <= last_month {
                out.push(trend_period(at_midnight(cursor), granularity));
                cursor = shift_month(cursor, 1);
            }
            out
        }
    }
}

/// 月份平移，**结果永远是某月 1 号**。
///
/// 上游 `_shift_month` 的实现（`month_index // 12, month_index % 12 + 1`）。
/// 直接用 `chrono` 的月份加法会让「1 月 31 日 + 1 月」变成 2 月 28/29 日，
/// 与上游不同。
pub fn shift_month(source: NaiveDate, months: i64) -> NaiveDate {
    let index = i64::from(source.year()) * 12 + i64::from(source.month() - 1) + months;
    NaiveDate::from_ymd_opt(
        (index.div_euclid(12)) as i32,
        (index.rem_euclid(12) + 1) as u32,
        1,
    )
    .expect("平移后的月份 1 号是合法日期")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_day_ranges_bucket_by_day_and_the_rest_by_month() {
        for range in [
            WatchTrendRange::Last7Days,
            WatchTrendRange::Last30Days,
            WatchTrendRange::Last90Days,
        ] {
            assert_eq!(range.granularity(), TrendGranularity::Day, "{range:?}");
        }
        for range in [WatchTrendRange::LastYear, WatchTrendRange::All] {
            assert_eq!(range.granularity(), TrendGranularity::Month, "{range:?}");
        }
    }

    #[test]
    fn day_ranges_include_today() {
        // 30 天 = 今天 + 往前 29 天，共 30 个桶
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let start = today - Duration::days(30 - 1);
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 2, 14).unwrap());
        let periods = trend_periods(start, today, TrendGranularity::Day);
        assert_eq!(periods.len(), 30);
        assert_eq!(periods.first().map(String::as_str), Some("2026-02-14"));
        assert_eq!(periods.last().map(String::as_str), Some("2026-03-15"));
    }

    #[test]
    fn day_buckets_are_dense_and_end_on_today() {
        let today = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let periods = trend_periods(
            NaiveDate::from_ymd_opt(2026, 12, 29).unwrap(),
            today,
            TrendGranularity::Day,
        );
        assert_eq!(periods, vec!["2026-12-29", "2026-12-30", "2026-12-31"]);
    }

    #[test]
    fn month_buckets_land_on_the_first_and_carry_no_day() {
        let start = NaiveDate::from_ymd_opt(2025, 4, 1).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let periods = trend_periods(start, today, TrendGranularity::Month);
        // 2025-04 .. 2026-03 共 12 个
        assert_eq!(periods.len(), 12);
        assert_eq!(periods.first().map(String::as_str), Some("2025-04"));
        assert_eq!(periods.last().map(String::as_str), Some("2026-03"));
        // 绝不出现日 —— 否则每个月的桶标签都不同，跨月聚合会失效
        assert!(periods.iter().all(|p| p.len() == 7), "{periods:?}");
    }

    #[test]
    fn last_year_is_twelve_months_ending_with_the_current_one() {
        let today = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let first = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let start = shift_month(first, -(LAST_YEAR_MONTHS - 1));
        assert_eq!(start, NaiveDate::from_ymd_opt(2025, 4, 1).unwrap());
        assert_eq!(
            trend_periods(start, today, TrendGranularity::Month).len(),
            12
        );
    }

    #[test]
    fn shifting_months_always_lands_on_the_first() {
        // 12 月 + 1 月 = 明年 1 月，不是「13 月」
        let dec = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        assert_eq!(
            shift_month(dec, 1),
            NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()
        );
        // 1 月 - 1 月 = 去年 12 月，不是「0 月」或负数
        let jan = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert_eq!(
            shift_month(jan, -1),
            NaiveDate::from_ymd_opt(2025, 12, 1).unwrap()
        );
        // 跨 11 个月
        assert_eq!(
            shift_month(jan, 11),
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()
        );
    }

    #[test]
    fn the_window_is_half_open_on_local_midnights() {
        // UTC+8：本地 2026-03-15 00:00 = UTC 2026-03-14 16:00
        let start = local_day_start_utc(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(), 8);
        assert_eq!(
            start,
            NaiveDate::from_ymd_opt(2026, 3, 14)
                .unwrap()
                .and_hms_opt(16, 0, 0)
                .unwrap()
        );
        // UTC-5：本地 00:00 = UTC 05:00（同一天）
        let start = local_day_start_utc(NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(), -5);
        assert_eq!(
            start,
            NaiveDate::from_ymd_opt(2026, 3, 15)
                .unwrap()
                .and_hms_opt(5, 0, 0)
                .unwrap()
        );
    }

    #[test]
    fn the_window_end_is_the_day_after_today() {
        // 闭区间会把窗口外的脏时间戳算进来 —— 端点必须落在明天零点
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let end = local_day_start_utc(today + Duration::days(1), 0);
        assert!(end > local_day_start_utc(today, 0));
    }

    // ------------------------------------------------------ 六分类折叠

    #[test]
    fn a_completed_download_folds_by_its_import_status() {
        let s = download_state::COMPLETED;
        assert_eq!(download_bucket(s, import_status::COMPLETED), "imported");
        assert_eq!(download_bucket(s, import_status::SKIPPED), "skipped");
        assert_eq!(download_bucket(s, import_status::PENDING), "importing");
        assert_eq!(download_bucket(s, import_status::RUNNING), "importing");
    }

    /// 未知 `import_status` 落到 `import_failed` —— 上游的 `else` 分支。
    ///
    /// 这条是本轮修掉 `DONE = "done"` 那个缺陷的**直接原因**：若写库时用了
    /// `done`，这里就会判成 `import_failed`，让「导入失败」的数字等于
    /// 「导入成功」的数字。
    #[test]
    fn an_unknown_import_status_falls_into_import_failed() {
        let s = download_state::COMPLETED;
        for bogus in ["done", "imported", "", "COMPLETED", "succeeded"] {
            assert_eq!(
                download_bucket(s, bogus),
                "import_failed",
                "{bogus:?} 应落到 import_failed"
            );
        }
    }

    #[test]
    fn a_failed_download_is_always_download_failed() {
        // 无论 import_status 是什么 —— 下载本身就失败了，导入无从谈起
        for import in [
            import_status::PENDING,
            import_status::COMPLETED,
            import_status::SKIPPED,
            "done",
        ] {
            assert_eq!(
                download_bucket(download_state::FAILED, import),
                "download_failed"
            );
        }
    }

    #[test]
    fn in_flight_states_fold_into_downloading() {
        for state in [
            download_state::QUEUED,
            download_state::SUBMITTED,
            download_state::DOWNLOADING,
            "some-future-state",
            "",
        ] {
            assert_eq!(
                download_bucket(state, import_status::PENDING),
                "downloading",
                "{state:?} 应落到 downloading"
            );
        }
    }

    /// 折叠函数**只**返回六个已知桶名 —— 这是 `add()` 不 panic 的前提。
    #[test]
    fn the_fold_never_produces_an_unknown_bucket() {
        let states = [
            download_state::QUEUED,
            download_state::SUBMITTED,
            download_state::DOWNLOADING,
            download_state::COMPLETED,
            download_state::FAILED,
            "future",
            "",
        ];
        let imports = [
            import_status::PENDING,
            import_status::RUNNING,
            import_status::COMPLETED,
            import_status::FAILED,
            import_status::SKIPPED,
            "done",
            "",
        ];
        for state in states {
            for import in imports {
                let bucket = download_bucket(state, import);
                assert!(
                    DOWNLOAD_TASK_BUCKETS.contains(&bucket),
                    "{state:?}×{import:?} 产出了未知桶 {bucket:?}"
                );
            }
        }
    }

    #[test]
    fn the_six_buckets_are_all_present_and_distinct() {
        assert_eq!(DOWNLOAD_TASK_BUCKETS.len(), 6);
        let mut sorted = DOWNLOAD_TASK_BUCKETS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 6, "桶名不能重复");
    }

    /// 六桶之和**恒等于** total —— 上游 `total=sum(counts.values())` 的保证。
    #[test]
    fn the_bucket_sum_always_equals_the_total() {
        let mut buckets = DownloadTaskBuckets::zeroed();
        assert_eq!(DownloadTaskSummary::from_buckets(&buckets).total, 0);
        // 初始化后六个桶都存在，响应里一个键都不会少
        for bucket in DOWNLOAD_TASK_BUCKETS {
            assert_eq!(buckets.get(bucket), 0, "{bucket} 未初始化");
        }
        buckets.add("imported", 3);
        buckets.add("skipped", 2);
        buckets.add("download_failed", 1);
        let summary = DownloadTaskSummary::from_buckets(&buckets);
        assert_eq!(summary.total, 6);
        assert_eq!(
            summary.total,
            summary.downloading
                + summary.importing
                + summary.imported
                + summary.import_failed
                + summary.skipped
                + summary.download_failed
        );
    }

    /// 未知桶名被**静默丢弃**，而 `total` 仍等于六桶之和。
    ///
    /// 所以「折叠产出了未知桶名」这件事不会让 total 与六桶脱节 ——
    /// 代价是那一批计数丢失。这正是 `add` 静默而非 panic 的原因：
    /// 折叠函数已被测试钉死只在六个名字里返回，panic 是不可达的。
    #[test]
    fn an_unknown_bucket_name_is_dropped_rather_than_counted() {
        let mut buckets = DownloadTaskBuckets::zeroed();
        buckets.add("imported", 4);
        buckets.add("not-a-bucket", 99);
        let summary = DownloadTaskSummary::from_buckets(&buckets);
        assert_eq!(summary.imported, 4);
        assert_eq!(summary.total, 4, "未知桶不该被算进 total");
    }

    #[test]
    fn the_backend_version_falls_back_when_the_env_is_absent_or_blank() {
        // 变量名带仓库前缀，测试里不设它，所以走回退分支
        assert_eq!(resolve_backend_version(), BACKEND_VERSION_DEFAULT);
        assert_eq!(BACKEND_VERSION_DEFAULT, "dev-local");
    }

    #[test]
    fn range_parsing_is_case_insensitive_and_typed() {
        assert_eq!(
            WatchTrendRange::parse("7d"),
            Some(WatchTrendRange::Last7Days)
        );
        assert_eq!(
            WatchTrendRange::parse(" 30D "),
            Some(WatchTrendRange::Last30Days)
        );
        assert_eq!(
            WatchTrendRange::parse("1y"),
            Some(WatchTrendRange::LastYear)
        );
        assert_eq!(WatchTrendRange::parse("all"), Some(WatchTrendRange::All));
        for bogus in ["", "1d", "yesterday", "7", "365d"] {
            assert_eq!(WatchTrendRange::parse(bogus), None, "{bogus:?} 应非法");
        }
    }
}
