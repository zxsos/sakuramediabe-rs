//! 影片订阅台账，对应上游 `src/service/catalog/movie_subscription_service.py`
//! （318 行）的**状态计数**部分。
//!
//! # 状态的唯一定义在数据库里
//!
//! 七项状态（除 `all`）由 [`sm_db::repo::movie`] 里那一个 `CASE` 表达式判定，
//! 本模块只做分组计数的组装 —— 上游注释写明这是为了避免「SQL 一套、Python 一套」
//! 的漂移。所以这里**不重新判状态**，只是把 `GROUP BY` 的结果填进八个字段。
//!
//! # `total` 是各状态之和
//!
//! 上游 `total=sum(counts.values())`。因为 `CASE` 的分支互斥且必有兜底
//! （`pending`），所以「各状态之和恒等于订阅总数」这一性质自动成立 —— 测试
//! 钉了它，一旦有人往 `CASE` 里加了一个不可达分支，这条断言会红。

use chrono::NaiveDateTime;
use sm_db::catalog::asset::Image;
use sm_db::catalog::movie::Movie;
use sm_db::common::time::now_utc;
use sm_db::repo::{ImageRepository, MovieRepository};
use sm_db::Db;
use std::collections::HashMap;

use crate::catalog::movie_subscription_search_state::MovieSubscriptionSearchStateService;
use crate::error::ServiceError;

/// 订阅列表的排序键（上游 `MovieSubscriptionSort` 的七个取值）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubscriptionSort {
    /// 上游缺省。
    #[default]
    SubscribedAtDesc,
    SubscribedAtAsc,
    ReleaseDateDesc,
    ReleaseDateAsc,
    LastSearchedAtDesc,
    LastSearchedAtAsc,
    AttemptCountDesc,
}

impl SubscriptionSort {
    /// `ORDER BY` 片段。全部是常量。
    ///
    /// 五个排序列都**允许为空**（未订阅时间、无发布日期、从未查过），空值统一
    /// 排到最后（上游 `nullable=True`），次级排序用 `id` 同向 —— 否则同值的
    /// 影片在两次刷新之间会换位置。
    pub fn order_by(self) -> &'static str {
        match self {
            Self::SubscribedAtDesc => "m.subscribed_at DESC NULLS LAST, m.id DESC",
            Self::SubscribedAtAsc => "m.subscribed_at ASC NULLS LAST, m.id ASC",
            Self::ReleaseDateDesc => "m.release_date DESC NULLS LAST, m.id DESC",
            Self::ReleaseDateAsc => "m.release_date ASC NULLS LAST, m.id ASC",
            Self::LastSearchedAtDesc => {
                "m.subscription_search_last_attempted_at DESC NULLS LAST, m.id DESC"
            }
            Self::LastSearchedAtAsc => {
                "m.subscription_search_last_attempted_at ASC NULLS LAST, m.id ASC"
            }
            Self::AttemptCountDesc => {
                "m.subscription_search_attempt_count DESC NULLS LAST, m.id DESC"
            }
        }
    }
}

/// `GET /movie-subscriptions` 的筛选位。
#[derive(Debug, Clone, Default)]
pub struct SubscriptionListParams {
    /// `None` = 全部（上游 `MovieSubscriptionStatus.ALL`）。
    pub status: Option<String>,
    pub sort: SubscriptionSort,
    pub search: Option<String>,
    /// 配置：`subscription_search_stale_attempt_limit`。
    pub attempt_limit: i32,
    /// 配置：`subscription_search_fresh_days`。
    pub fresh_days: i64,
}

/// 订阅列表项（上游 `MovieSubscriptionListItemResource`）。
///
/// **不派生 `PartialEq`**：里面带着 `Image`，而图片模型没有实现它（它是数据源
/// 镜像，不是值对象）。测试要比较就比具体字段。
#[derive(Debug, Clone)]
pub struct SubscriptionListItem {
    pub movie_id: i32,
    pub movie_number: String,
    pub title: String,
    pub cover_image: Option<Image>,
    /// 窄版海报：移动端订阅行用它，桌面端用 `cover_image`。
    pub thin_cover_image: Option<Image>,
    pub release_date: Option<NaiveDateTime>,
    pub subscribed_at: Option<NaiveDateTime>,
    /// 七项状态之一，由数据库里那个 `CASE` 判定。
    pub status: String,
    /// 上映日期在「新鲜期」内 —— 新鲜期内失败不消耗重试预算。
    pub is_fresh: bool,
    pub attempt_count: i32,
    pub attempt_limit: i32,
    pub last_searched_at: Option<NaiveDateTime>,
    pub last_error: Option<String>,
    pub import_status: Option<String>,
    /// 该影片已判死的下载任务数：试过几个种子都失败了。
    pub dead_download_task_count: i64,
    pub media_count: i64,
}

/// 订阅状态计数（上游 `MovieSubscriptionStatusCountsResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubscriptionStatusCounts {
    pub total: i64,
    /// 本地已有媒体。
    pub imported: i64,
    /// 有活跃下载任务且导入还在途。
    pub downloading: i64,
    /// 有活跃下载任务、但导入这趟已跑完却没进库（卡在入库）。
    pub import_failed: i64,
    pub pending: i64,
    /// 查过了但没找到资源。
    pub missing: i64,
    /// 重试预算用尽。
    pub exhausted: i64,
    /// 查询失败（不含「没找到资源」）。
    pub failed: i64,
}

/// 影片订阅 service。
#[derive(Debug, Clone)]
pub struct MovieSubscriptionService {
    movies: MovieRepository,
    images: ImageRepository,
}

impl MovieSubscriptionService {
    pub fn new(db: &Db) -> Self {
        Self {
            movies: MovieRepository::new(db.clone()),
            images: ImageRepository::new(db.clone()),
        }
    }

    /// `GET /movie-subscriptions`。对应上游 `list_subscriptions`。
    ///
    /// # 顺序必须由分页查询给出
    ///
    /// `_build_items` 里字典按 id 取回会丢掉排序，所以上游专门注释了「保持分页
    /// 查询给出的顺序」。这里同样按 id 列表重排，不再排第二次。
    ///
    /// # 四条辅助查询，条数与页大小无关
    ///
    /// 封面（宽+窄一次查）、媒体计数、判死任务计数、最新导入状态 —— 不是 N+1。
    pub async fn list_subscriptions(
        &self,
        params: &SubscriptionListParams,
        page: i64,
        page_size: i64,
    ) -> Result<sm_db::common::Page<SubscriptionListItem>, ServiceError> {
        let status = params.status.as_deref();
        let search = params.search.as_deref();
        let total = self.movies.count_subscriptions(status, search).await?;
        let offset = (page - 1).max(0) * page_size;
        let rows = self
            .movies
            .list_subscription_ids(status, search, params.sort.order_by(), page_size, offset)
            .await?;

        let ids: Vec<i32> = rows.iter().map(|(movie_id, _)| *movie_id).collect();
        let status_by_id: HashMap<i32, String> = rows.into_iter().collect();

        let movies = self.movies.find_by_ids(&ids).await?;
        // 分页给出的顺序就是 `ids` 的顺序；`find_by_ids` 回的是 map，必须按它重排。
        let ordered: Vec<Movie> = ids
            .iter()
            .filter_map(|id| movies.get(id).cloned())
            .collect();

        let image_ids: Vec<i32> = ordered
            .iter()
            .flat_map(|movie| [movie.cover_image_id, movie.thin_cover_image_id])
            .flatten()
            .collect();
        let images = self.images.find_by_ids(&image_ids).await?;

        let numbers: Vec<String> = ordered
            .iter()
            .map(|movie| movie.movie_number.clone())
            .collect();
        let media_counts = self.movies.count_media_by_numbers(&numbers).await?;
        let dead_counts = self.movies.count_failed_tasks_by_numbers(&numbers).await?;
        let import_statuses = self
            .movies
            .latest_import_status_by_numbers(&numbers)
            .await?;

        let now = now_utc();
        let items = ordered
            .into_iter()
            .map(|movie| SubscriptionListItem {
                cover_image: movie.cover_image_id.and_then(|id| images.get(&id).cloned()),
                thin_cover_image: movie
                    .thin_cover_image_id
                    .and_then(|id| images.get(&id).cloned()),
                // 判据只此一份：`release_date > now - fresh_days`，见
                // `movie_subscription_search_state::MovieSubscriptionSearchStateService::is_fresh`
                // —— 那里是状态机判定「失败要不要扣预算」用的同一个函数，
                // 两处各写一遍的话，「列表里显示新鲜的影片」与「不扣预算的影片」
                // 会有一天对不上。
                is_fresh: MovieSubscriptionSearchStateService::is_fresh(
                    movie.release_date,
                    params.fresh_days,
                    now,
                ),
                media_count: media_counts.get(&movie.movie_number).copied().unwrap_or(0),
                dead_download_task_count: dead_counts
                    .get(&movie.movie_number)
                    .copied()
                    .unwrap_or(0),
                import_status: import_statuses.get(&movie.movie_number).cloned(),
                status: status_by_id.get(&movie.id).cloned().unwrap_or_default(),
                attempt_limit: params.attempt_limit,
                movie_id: movie.id,
                movie_number: movie.movie_number,
                title: movie.title,
                release_date: movie.release_date,
                subscribed_at: movie.subscribed_at,
                attempt_count: movie.subscription_search_attempt_count,
                last_searched_at: movie.subscription_search_last_attempted_at,
                last_error: movie.subscription_search_last_error,
            })
            .collect();

        Ok(sm_db::common::Page::new(items, total))
    }

    /// `GET /movie-subscriptions/status-counts`。对应上游 `count_by_status`。
    ///
    /// 数据库只回**出现过的**状态（没出现的那几项是 0），所以这里从 `Default`
    /// 起步再按行填充，而 `total` 用各行求和 —— 与上游的 `sum(counts.values())`
    /// 同义。
    pub async fn count_by_status(&self) -> Result<SubscriptionStatusCounts, ServiceError> {
        let rows = self.movies.count_subscription_statuses().await?;
        let mut counts = SubscriptionStatusCounts::default();
        for (status, total) in rows {
            match status.as_str() {
                "imported" => counts.imported = total,
                "downloading" => counts.downloading = total,
                "import_failed" => counts.import_failed = total,
                "pending" => counts.pending = total,
                "missing" => counts.missing = total,
                "exhausted" => counts.exhausted = total,
                "failed" => counts.failed = total,
                // 未知状态**不静默丢弃** —— 那说明 `CASE` 与本模块的枚举不同步了。
                other => {
                    tracing::warn!(status = %other, "订阅状态计数收到未知状态，CASE 与 DTO 不同步");
                }
            }
            counts.total += total;
        }
        Ok(counts)
    }
}
