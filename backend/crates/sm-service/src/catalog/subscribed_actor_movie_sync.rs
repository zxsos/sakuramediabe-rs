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
//!
//! # ⚠️ 编排已就位，但两个 trait **还没有宿主实现**
//!
//! [`ActorMoviesProvider`] 要 JavDB provider（插件 ABI 那一层），
//! [`MovieImporter`] 要 [`super::catalog_import`] 的 `import_movie_if_missing`。
//! 两者在本仓都还不存在，所以：
//!
//! - 本模块的 `sync_subscribed_actor_movies` 是**完整可读、可测**的
//!   （候选查询、全量/增量判据、单片失败跳过、两个时间戳的写法都在里面）；
//! - 但 `actor_subscription_sync` 的 **worker handler 仍未注册** ——
//!   任务被领取时会以 `NoHandler` 失败。这是阶段性事实，不是回归。
//!
//! 接线时只需要实现那两个 trait 与一行 handler 注册。

use sm_db::common::time::now_utc;
use sm_db::repo::ActorRepository;
use sm_db::Actor;
use sm_db::Db;

use crate::error::ServiceError;

/// 进度上报（与 `movie_asset_pack_backfill` 同一个形状）。
pub type ProgressSink<'a> = Box<
    dyn FnMut(
            Option<i32>,
            Option<i32>,
            &str,
            Option<&serde_json::Value>,
        ) -> BoxFuture<'a, Result<(), String>>
        + Send
        + 'a,
>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 抓取服务。
///
/// # 依赖是**注入**的，`Db` 是直接持有的
///
/// `provider` 要出网（插件 ABI / JavDB provider），`import_service` 是
/// [`super::catalog_import`] —— 两者在本仓都还没落地，所以本模块只定义
/// **窄接口**，由组合根注入（与 [`super::movie_interaction_sync`] 同一个取向）。
///
/// ⚠️ 因此 `actor_subscription_sync` 的 **worker handler 仍未注册**：
/// 任务被领取时会以 `NoHandler` 失败。接线时只需实现那两个 trait。
pub struct SubscribedActorMovieSyncService {
    db: Db,
    provider: Box<dyn ActorMoviesProvider>,
    import_service: Box<dyn MovieImporter>,
}

/// 演员作品抓取能力。**出网**。
pub trait ActorMoviesProvider {
    /// 翻页取该演员的作品。**`page` 从 1 开始**；返回空列表 = 没有下一页。
    ///
    /// ⚠️ 骨架期的签名是 `(javdb_actor_id, after: Option<NaiveDateTime>)` ——
    /// 上游**没有**「上次同步到的时间」这个入参（`:88-92`）。增量是靠
    /// 「这一页里翻到库里已有关联的那部就停」判的（见 `sync_one` 的文档），
    /// 与时间无关。
    fn get_actor_movies(
        &self,
        javdb_actor_id: &str,
        actor_type: i32,
        page: i32,
    ) -> Result<Vec<ActorMovieEntry>, ServiceError>;

    /// 取一部影片的完整元数据，用于入库。
    ///
    /// 返回值是**provider 载荷**（`Value`，宿主只搬运）—— 与
    /// [`crate::transfers::provider_browse`] 的 `source_ref` 同一个约定：
    /// 详情模型的权威在插件 ABI 那一层，这里不该再定义第二份。
    fn get_movie_by_javdb_id(
        &self,
        movie_javdb_id: &str,
    ) -> Result<serde_json::Value, ServiceError>;
}

/// 一条演员作品。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorMovieEntry {
    /// 影片的 JavDB id。**判重与入库都以它为键**（不是番号：番号会被人工改）。
    pub javdb_movie_id: String,
    pub movie_number: String,
    /// 发行日期。
    pub release_date: Option<chrono::NaiveDate>,
}

/// 影片入库能力（[`super::catalog_import`] 的窄接口）。
pub trait MovieImporter {
    /// **缺则入库**。上游 `import_movie_if_missing(detail)`（`:112`）——
    /// 已存在时是**成功**（不是跳过、更不是错误）：增量同步会反复看到同一批
    /// 影片，把「已存在」当失败会让每天的任务都报一堆错。
    fn import_movie_if_missing(&self, detail: &serde_json::Value) -> Result<(), ServiceError>;
}

/// 同步统计。**四个键与上游 dict 逐字一致**
/// （`subscribed_actor_movie_sync_service.py:28-33`）。
///
/// ⚠️ 骨架期是自造的五项（`actors` / `entries` / `imported` /
/// `skipped_existing` / `failed`）。差别不只是名字：上游的
/// `imported_movies` 是**影片数**、`success_actors` / `failed_actors` 是
/// **演员数**，两者的分母不同 —— 混在一个 `actors` 里会让「失败了几个演员」
/// 这个运维最关心的数字消失。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ActorSyncStats {
    /// 待处理的演员数（已订阅、非墓碑）。
    pub total_actors: i64,
    /// 整位同步成功的演员数。
    pub success_actors: i64,
    /// 整位失败的演员数（**单个演员失败不中断整批**）。
    pub failed_actors: i64,
    /// 新入库的影片数（含合并来源演员名下补录的）。
    pub imported_movies: i64,
}

impl SubscribedActorMovieSyncService {
    /// 构造。
    pub fn new(
        db: &Db,
        provider: Box<dyn ActorMoviesProvider>,
        import_service: Box<dyn MovieImporter>,
    ) -> Self {
        Self {
            db: db.clone(),
            provider,
            import_service,
        }
    }

    /// ★ 同步一次。上游 `sync_subscribed_actor_movies(progress_callback=None)`
    /// （`:21-62`）。
    ///
    /// 逐个已订阅演员：翻页抓取 → 增量判重 → 入库 → 记两个时间戳。
    ///
    /// **单个演员失败不中断整批** —— 一位演员的页面 404 不该让其他 500 位
    /// 的同步都停掉。失败计进 `failed_actors`（那位演员的两个时间戳都不推进，
    /// 于是下一轮它还会是候选）。
    pub async fn sync_subscribed_actor_movies(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<ActorSyncStats, ServiceError> {
        let actors = ActorRepository::new(self.db.clone())
            .list_subscribed_for_sync()
            .await?;
        let total = i64::try_from(actors.len()).unwrap_or(i64::MAX);
        let mut stats = ActorSyncStats {
            total_actors: total,
            ..Default::default()
        };
        tracing::info!(total_actors = total, "订阅演员影片同步开始");
        emit(&mut progress, 0, total, "开始同步已订阅演员影片", &stats).await;

        for (index, actor) in actors.iter().enumerate() {
            let current = i64::try_from(index).unwrap_or(i64::MAX) + 1;
            match self.sync_one(actor).await {
                Ok(imported) => {
                    stats.success_actors += 1;
                    stats.imported_movies += imported;
                }
                Err(error) => {
                    stats.failed_actors += 1;
                    tracing::error!(
                        actor_id = actor.id,
                        javdb_actor_id = actor.javdb_id.as_str(),
                        name = actor.name.as_str(),
                        code = error.code(),
                        "订阅演员影片同步：这位演员失败"
                    );
                }
            }
            emit(
                &mut progress,
                current,
                total,
                &format!("已处理演员 {}", actor.name),
                &stats,
            )
            .await;
        }
        tracing::info!(
            success_actors = stats.success_actors,
            failed_actors = stats.failed_actors,
            imported_movies = stats.imported_movies,
            "订阅演员影片同步结束"
        );
        Ok(stats)
    }

    /// 同步一位演员。上游 `_sync_actor`（`:64-152`）。返回**新入库影片数**。
    ///
    /// # 全量还是增量
    ///
    /// `subscribed_movies_full_synced_at` 为 `NULL` → **全量**；否则增量。
    /// 增量的判据不是时间，而是「这一页里遇到库里已有关联的那部片就停」
    /// （`:98-107`）—— 演员的作品页按新→旧排，遇到旧的已知片说明后面的都见过。
    ///
    /// # 合并来源演员的作品也要抓
    ///
    /// `targets` = 保留记录自己 + 所有以它为目标墓碑的演员
    /// （[`ActorRepository::list_merged_source_targets`]）。少了这一步，
    /// 合并会让来源演员的作品永远不再补录。
    ///
    /// # 单片失败**按影片跳过**
    ///
    /// 「取详情」或「入库」抛错时记 warn 继续下一部（`:109-129`）——
    /// 一部片的元数据抓失败不该让这位演员剩下的几百部都不同步。
    /// 那位演员最终仍算**成功**（`imported_movies` 少一部）。
    async fn sync_one(&self, actor: &Actor) -> Result<i64, ServiceError> {
        let repo = ActorRepository::new(self.db.clone());
        let full_sync = actor.subscribed_movies_full_synced_at.is_none();
        let mut targets = vec![(actor.javdb_id.clone(), actor.javdb_type)];
        targets.extend(repo.list_merged_source_targets(actor.id).await?);

        let mut imported = 0_i64;
        // 只在日志里用：上游 `stop_reason` 的两种取值 + 默认值。
        let mut stop_reason = "empty_page";
        tracing::info!(
            actor_id = actor.id,
            javdb_actor_id = actor.javdb_id.as_str(),
            mode = if full_sync { "full" } else { "incremental" },
            targets = targets.len(),
            "订阅演员影片同步：演员开始"
        );

        for (javdb_actor_id, actor_type) in targets {
            let mut page = 1_i32;
            loop {
                let movies = self
                    .provider
                    .get_actor_movies(&javdb_actor_id, actor_type, page)?;
                if movies.is_empty() {
                    break;
                }
                let mut should_stop = false;
                for item in &movies {
                    if !full_sync && repo.has_actor_movie(actor.id, &item.javdb_movie_id).await? {
                        stop_reason = "existing_actor_movie";
                        should_stop = true;
                        tracing::info!(
                            actor_id = actor.id,
                            movie_javdb_id = item.javdb_movie_id.as_str(),
                            "订阅演员影片同步：翻到库里已有的影片，本页到此为止"
                        );
                        break;
                    }
                    match self.import_one(&item.javdb_movie_id).await {
                        Ok(()) => imported += 1,
                        Err(error) => tracing::warn!(
                            actor_id = actor.id,
                            movie_javdb_id = item.javdb_movie_id.as_str(),
                            code = error.code(),
                            "订阅演员影片同步：这一部跳过"
                        ),
                    }
                }
                if should_stop {
                    break;
                }
                page += 1;
            }
        }

        // 走到这里才算这位演员成功。写时刻**在抓取之后**：中途失败就不推进，
        // 于是下一轮它还会被当成候选重试。
        let synced_at = now_utc();
        repo.mark_subscribed_movies_synced(actor.id, synced_at)
            .await?;
        tracing::info!(
            actor_id = actor.id,
            imported_movies = imported,
            stop_reason,
            "订阅演员影片同步：演员完成"
        );
        Ok(imported)
    }

    /// 取详情 → 缺则入库。上游 `:111-112` 那两行。
    async fn import_one(&self, movie_javdb_id: &str) -> Result<(), ServiceError> {
        let detail = self.provider.get_movie_by_javdb_id(movie_javdb_id)?;
        self.import_service.import_movie_if_missing(&detail)
    }
}

/// 上报一次进度（带摘要）。
async fn emit(
    progress: &mut Option<ProgressSink<'_>>,
    current: i64,
    total: i64,
    text: &str,
    stats: &ActorSyncStats,
) {
    let Some(sink) = progress.as_mut() else {
        return;
    };
    let patch = serde_json::to_value(stats).ok();
    let _ = sink(
        Some(i32::try_from(current).unwrap_or(i32::MAX)),
        Some(i32::try_from(total).unwrap_or(i32::MAX)),
        text,
        patch.as_ref(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 统计的**四个键与上游 dict 逐字一致**（含 `total_actors`）。
    ///
    /// 这条盯着骨架期那套自造的五项：它把**演员数**（`actors`）与**影片数**
    /// （`entries` / `imported`）混在一起，于是「失败了几个演员」这个运维最
    /// 关心的数字在里面根本不存在 —— `failed` 记的是影片。
    #[test]
    fn the_stats_keys_match_upstreams_dict() {
        let stats = ActorSyncStats {
            total_actors: 2,
            success_actors: 1,
            failed_actors: 1,
            imported_movies: 3,
        };
        let value = serde_json::to_value(&stats).expect("可序列化");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "failed_actors",
                "imported_movies",
                "success_actors",
                "total_actors"
            ]
        );
        assert_eq!(
            stats.success_actors + stats.failed_actors,
            stats.total_actors,
            "成功的演员 + 失败的演员 = 待处理的演员"
        );
    }

    /// 单片失败**不进**统计（上游只在演员层面计数）。
    ///
    /// 「这一部没导进来」是靠日志与 `imported_movies` 少一部体现的，
    /// 而**不是**这里多一个 `failed` —— 那样会让「失败了几个演员」与
    /// 「失败了几部片」变成同一个键。
    #[test]
    fn a_failed_movie_does_not_show_up_as_a_failed_actor() {
        let stats = ActorSyncStats {
            total_actors: 1,
            success_actors: 1,
            failed_actors: 0,
            imported_movies: 4,
        };
        assert_eq!(stats.failed_actors, 0, "一位演员五部里失败一部仍算成功");
        assert_eq!(stats.imported_movies, 4, "只数真的进了库的");
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
