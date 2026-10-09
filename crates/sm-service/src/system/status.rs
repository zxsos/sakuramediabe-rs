//! 状态页聚合，对应上游 `src/service/system/status_service.py`（602 行）。
//!
//! # 本批落地三个方法，另两个被阻塞
//!
//! | 上游方法 | 端点 | 状态 |
//! |---|---|---|
//! | `get_status` | `GET /status` | **已落** |
//! | `get_insights` | `GET /status/insights` | **已落**（磁盘空间那三列为 `null`） |
//! | `get_watch_trend` | `GET /status/watch-trend` | **已落** |
//! | `get_image_search_status` | `GET /status/image-search` | 阻塞：需要 `discovery` 域的 embedding / Qdrant 客户端 |
//! | `test_metadata_provider` | `POST /status/metadata-provider/test` | 阻塞：需要 `metadata` 域的 JavDB provider |
//!
//! 被阻塞的两者都不是「难写」，而是**没有依赖可调** —— 它们的主体是对
//! Qdrant 与 JavDB 发网络请求并解读响应。写一个只会返回 `unhealthy` 的
//! 假实现比不写更糟：客户端会把它当成「服务真的挂了」。
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

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use sm_db::repo::{
    ClipCollectionRepository, MomentCollectionRepository, PlaylistRepository, StatsRepository,
    VideoCollectionRepository,
};
use sm_db::transfers::downloads::{download_state, import_status};
use sm_db::Db;

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

// ================================================================ service

/// 状态页 service。
#[derive(Debug, Clone)]
pub struct StatusService {
    stats: StatsRepository,
    playlists: PlaylistRepository,
    video_collections: VideoCollectionRepository,
    moment_collections: MomentCollectionRepository,
    clip_collections: ClipCollectionRepository,
}

impl StatusService {
    pub fn new(db: &Db) -> Self {
        Self {
            stats: StatsRepository::new(db.clone()),
            playlists: PlaylistRepository::new(db.clone()),
            video_collections: VideoCollectionRepository::new(db.clone()),
            moment_collections: MomentCollectionRepository::new(db.clone()),
            clip_collections: ClipCollectionRepository::new(db.clone()),
        }
    }

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
