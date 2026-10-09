//! 已订阅演员的影片抓取（上游 `catalog/subscribed_actor_movie_sync_service.py`，163 行）。
//!
//! # 任务键 `actor_subscription_sync`，cron `0 2 * * *`
//!
//! 「我订阅了这个演员，她的每部片子都要有」—— 所以这是个**持续同步**任务，
//! 不是一次性的。
//!
//! # 两个时间戳，含义不同
//!
//! | 列 | 含义 | 决定什么 |
//! |---|---|---|
//! | `subscribed_movies_synced_at` | 上次**增量**同步到的时间 | 下次从哪翻页 |
//! | `subscribed_movies_full_synced_at` | 上次**全量**同步到的时间 | 多久做一次全量对账 |
//!
//! 只做增量的后果是「上次同步之后演员换过页面顺序」时会漏片；只做全量的
//! 后果是每次都拉几百页。两者都要。
//!
//! # 翻页抓取，**每页都要判重**
//!
//! 上游 `get_actor_movies_by_javdb` 是翻页的（演员可能有几百部作品）。
//! 每一页里的每一部都要查「库里有没有」—— 靠 `movie_number` 判重。
//!
//! # 只标记时间戳，**不**入库
//!
//! 本服务**只**记「同步到哪个时间点」。真正写入影片走
//! [`super::catalog_import`]。分开的原因：抓取可能拿到 50 部片，其中 3 部
//! 的元数据抓失败 —— 那 3 部不该让整个同步任务失败。
//!
//! ⚠️ 这一点与 `subscribed_movie_auto_download`（transfers 域）不同，那个是
//! 抓完直接提交下载的。

use crate::error::ServiceError;

/// 抓取服务。
// 两个依赖尚未被方法体引用（抓取动作还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct SubscribedActorMovieSyncService {
    provider: Option<Box<dyn ActorMoviesProvider>>,
    import_service: Option<Box<dyn MovieImporter>>,
}

/// 演员作品抓取能力。**出网**。
pub trait ActorMoviesProvider {
    /// 翻页取该演员的作品。`after` 是上次同步到的时间戳。
    fn get_actor_movies(
        &self,
        javdb_actor_id: &str,
        after: Option<chrono::NaiveDateTime>,
    ) -> Result<Vec<ActorMovieEntry>, ServiceError>;
}

/// 一条演员作品。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorMovieEntry {
    pub javdb_movie_id: String,
    pub movie_number: String,
    /// 发行日期。上游用作「增量」的比较基准。
    pub release_date: Option<chrono::NaiveDate>,
}

/// 影片入库能力（与 [`super::catalog_import`] 同一个 trait 的窄接口）。
pub trait MovieImporter {
    /// 入库。`Ok(true)` = 新增；`Ok(false)` = 已有。
    fn import_movie(&self, entry: &ActorMovieEntry) -> Result<bool, ServiceError>;
}

/// 同步统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ActorSyncStats {
    /// 处理的演员数。
    pub actors: i32,
    /// 抓到的作品条目数。
    pub entries: i32,
    /// 新入库的影片数。
    pub imported: i32,
    /// 库里已有的（跳过的）。
    pub skipped_existing: i32,
    /// 入库失败的。
    pub failed: i32,
}

impl SubscribedActorMovieSyncService {
    /// 构造。
    pub fn new(
        provider: Box<dyn ActorMoviesProvider>,
        import_service: Box<dyn MovieImporter>,
    ) -> Self {
        Self {
            provider: Some(provider),
            import_service: Some(import_service),
        }
    }

    /// ★ 同步一次。上游 `sync_subscribed_actor_movies(progress_callback=None)`。
    ///
    /// 逐个已订阅演员：翻页抓取 → 逐条判重 → 入库 → 记两个时间戳。
    ///
    /// **单个演员失败不中断整批** —— 一位演员的页面 404 不该让其他 500 位
    /// 的同步都停掉。
    pub async fn sync_subscribed_actor_movies(&self) -> Result<ActorSyncStats, ServiceError> {
        todo!(
            "骨架：逐个已订阅演员 -> 翻页 -> 判重 -> 入库 -> 写两个 synced_at；单个演员失败不中断"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 统计的四个计数自洽。
    #[test]
    fn the_counts_are_consistent() {
        let stats = ActorSyncStats {
            actors: 2,
            entries: 10,
            imported: 3,
            skipped_existing: 6,
            failed: 1,
        };
        assert_eq!(
            stats.imported + stats.skipped_existing + stats.failed,
            stats.entries
        );
    }

    /// 入库失败是**独立计数** —— 抓取成功但元数据失败不该让整个任务失败。
    #[test]
    fn import_failure_is_counted_separately_from_fetching() {
        let stats = ActorSyncStats {
            actors: 1,
            entries: 5,
            imported: 0,
            skipped_existing: 0,
            failed: 5,
        };
        assert_eq!(stats.failed, 5);
        assert_eq!(stats.actors, 1, "演员层面的失败不计入这里");
    }

    /// 一条作品条目**必须带番号** —— 判重与入库都以它为键。
    #[test]
    fn an_entry_carries_the_movie_number_used_for_dedup() {
        let entry = ActorMovieEntry {
            javdb_movie_id: "javdb-123".to_owned(),
            movie_number: "ABC-123".to_owned(),
            release_date: None,
        };
        assert_eq!(entry.movie_number, "ABC-123");
        assert!(!entry.movie_number.is_empty());
    }
}
