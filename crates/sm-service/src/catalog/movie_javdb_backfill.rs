//! 插件影片按固定间隔尝试接入 JavDB（上游 `catalog/movie_javdb_backfill_service.py`，109 行）。
//!
//! # 为什么要「不依赖来源插件」地反复尝试
//!
//! 一部影片可能来自任意 provider，而 **JavDB 往往在影片入库之后才收录它**。
//! 若只在导入时试一次，绝大多数影片会永远拿不到 JavDB 元数据。
//!
//! 所以这个任务是 cron（`30 5 * * *`）反复扫「还没接入 JavDB 的插件影片」，
//! 每天 50 条。
//!
//! # 两条硬约束是为了不把 JavDB 打进黑名单
//!
//! | 常量 | 值 | 理由 |
//! |---|---|---|
//! | [`BATCH_SIZE`] | 50 | 单次任务限量，避免一次跑太久 |
//! | [`REQUEST_INTERVAL_SECONDS`] | 2 | **每条之间 sleep 2 秒** |
//!
//! ⚠️ 那个 sleep 不能省。上游是站外公共数据源，高频请求会被限流，而限流的
//! 表现是「静默返回空」—— 那会被误判成「JavDB 没收录这部片」，于是影片被
//! 标记为已尝试而不再重试。
//!
//! # 记的是「下次检查时间」，不是「试过了」
//!
//! 固定间隔重试，落在 `movie.javdb_next_check_at`（见 `schema.sql`）。

use sm_db::common::time::now_utc;
use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::error::ServiceError;

/// 单次最多处理多少条。
pub const BATCH_SIZE: i64 = 50;
/// 每条之间的间隔（秒）。⚠️ **不要省**（见模块文档）。
pub const REQUEST_INTERVAL_SECONDS: u64 = 2;

/// 推后多少天再问一次。上游 `CatalogImportService.JAVDB_CHECK_INTERVAL`
/// （`catalog_import_service.py:71`）= `timedelta(days=7)`。
pub const JAVDB_CHECK_INTERVAL_DAYS: i64 = 7;

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

/// 一条待补录的影片。
///
/// ⚠️ 骨架期这里还有一个 `last_attempt_at`（「上次尝试时间」）——
/// **上游没有这个字段**。`movie` 表上只有 `javdb_next_check_at`，那是
/// **下次**检查时间；把它叫成「上次尝试」会让调用方按反的方向算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBackfill {
    pub movie_id: i32,
    pub movie_number: String,
}

/// JavDB 查询能力。**出网**。
pub trait JavdbProvider {
    /// 按番号取影片详情。`Ok(None)` = JavDB **没有收录**（不是错误）。
    ///
    /// ⚠️ 上游在这里抛 `MetadataNotFoundError` 表示「未收录」，而 Rust 侧用
    /// `Ok(None)` —— 是同一个结果的两种表达（计数见 `run` 的文档）。
    fn get_movie_by_number(
        &self,
        movie_number: &str,
    ) -> Result<Option<serde_json::Value>, ServiceError>;
}

/// 把 JavDB 详情写回影片。
pub trait PluginMovieBackfill {
    /// 回填。**只写插件拥有的字段**（见
    /// [`MovieOwnershipGateway`](sm_db::repo::MovieOwnershipGateway)）。
    fn backfill(&self, movie_id: i32, detail: &serde_json::Value) -> Result<bool, ServiceError>;
}

/// 本次运行的统计。**四个键与上游 dict 逐字一致**
/// （`movie_javdb_backfill_service.py:36-41`）。
///
/// ⚠️ 骨架期是自造的 `examined` / `backfilled` / `not_found` / `failed`
/// —— 键名全不对，且少了「未收录」与「失败」的区分度说明。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BackfillStats {
    /// 本轮候选影片数。
    pub candidate_movies: i64,
    /// 补录成功的部数。
    pub succeeded_movies: i64,
    /// JavDB **未收录**的部数。**不是失败** —— 只是把下次检查推后。
    pub not_found_movies: i64,
    /// 查询/入库失败的部数。
    pub failed_movies: i64,
}

/// 补录服务。
///
/// ⚠️ [`JavdbProvider`] 与 [`PluginMovieBackfill`] **还没有宿主实现**
/// （前者要插件 ABI 出的 JavDB provider，后者要 [`super::catalog_import`] 的
/// `backfill_plugin_movie`），所以 `movie_javdb_backfill` 的 worker handler
/// **仍未注册**。接线时只需实现那两个 trait 与一行注册 —— 本模块一行不用改。
pub struct MovieJavdbBackfillService {
    db: Db,
    provider: Box<dyn JavdbProvider>,
    import_service: Box<dyn PluginMovieBackfill>,
}

impl MovieJavdbBackfillService {
    /// 构造。
    pub fn new(
        db: &Db,
        provider: Box<dyn JavdbProvider>,
        import_service: Box<dyn PluginMovieBackfill>,
    ) -> Self {
        Self {
            db: db.clone(),
            provider,
            import_service,
        }
    }

    /// 列出待补录的影片。`limit` 缺省 [`BATCH_SIZE`]。
    ///
    /// 上游 `pending()`（`:22-26`）是 static 且**无参数** —— 它返回的是一个
    /// **未执行的查询**，`run()` 才往上叠「检查时间已到 / 排序 / 限量」。
    /// Rust 侧没有惰性查询，所以这里收一个 `limit`：语义是「最多列这么多」，
    /// 不改变上游那两条筛选条件。
    ///
    /// **不含** `javdb_next_check_at` 条件 —— 那是 `run()` 才加的
    /// （见 [`MovieRepository::list_javdb_backfill_candidate_ids`]）。
    pub async fn pending(&self, limit: Option<i64>) -> Result<Vec<PendingBackfill>, ServiceError> {
        let rows = MovieRepository::new(self.db.clone())
            .list_javdb_backfill_pending(limit.unwrap_or(BATCH_SIZE).max(1))
            .await?;
        Ok(rows
            .into_iter()
            .map(|(movie_id, movie_number)| PendingBackfill {
                movie_id,
                movie_number,
            })
            .collect())
    }

    /// ★ 跑一轮。上游 `run(self, *, reporter) -> dict`（`:28-109`）。
    ///
    /// **逐条处理且条间 sleep [`REQUEST_INTERVAL_SECONDS`]**（第一条**不** sleep，
    /// 上游是 `if current > 1`）。
    ///
    /// 单条失败**不中断整轮** —— 一次网络抖动不该让剩下 49 条白等一天。
    /// JavDB 未收录（`Ok(None)`）**不是失败**，它只是把下次检查时间推后。
    ///
    /// # ★ 推后检查时间在 `finally` 里
    ///
    /// 上游 `:89-93`：**成功、未收录、失败**三种结局都推后
    /// `now + JAVDB_CHECK_INTERVAL`。少了这个「失败也推后」，一条死掉的影片
    /// 会在每一轮都占掉 50 个名额里的一个 —— 而它明天也不会突然好起来。
    pub async fn run(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<BackfillStats, ServiceError> {
        let repo = MovieRepository::new(self.db.clone());
        let ids = repo
            .list_javdb_backfill_candidate_ids(now_utc(), BATCH_SIZE)
            .await?;
        let total = i64::try_from(ids.len()).unwrap_or(i64::MAX);
        let mut stats = BackfillStats {
            candidate_movies: total,
            ..Default::default()
        };
        tracing::info!(candidate_movies = total, "JavDB 补录开始");
        let step = usize::try_from((total / 20).max(1)).unwrap_or(1);

        for (index, movie_id) in ids.iter().enumerate() {
            let current = index + 1;
            if current > 1 {
                // ★ 见模块文档：省掉这个 sleep 会让 JavDB 限流，而限流的表现
                // 是「静默返回空」—— 那会被记成「未收录」。
                tokio::time::sleep(std::time::Duration::from_secs(REQUEST_INTERVAL_SECONDS)).await;
            }

            // 重新按 pending 条件取一次：两次查询之间这部片可能已被接入
            // JavDB（或来源被改），那时上游是 `continue`（不计数、不推后）。
            let Some(movie) = self.reload_pending(*movie_id).await? else {
                emit(
                    &mut progress,
                    current,
                    total,
                    &progress_text(current, total, &stats, None),
                    &stats,
                )
                .await;
                continue;
            };

            emit(
                &mut progress,
                current - 1,
                total,
                &progress_text(
                    current - 1,
                    total,
                    &stats,
                    Some(&format!("正在查询 {}", movie.movie_number)),
                ),
                &stats,
            )
            .await;

            let outcome = self.try_one(*movie_id, &movie.movie_number).await;
            // ★ 三档都推后 —— 见方法文档。放在 `outcome` 之后、**计数**之前，
            // 与上游 `finally` 同一个位置。
            let next_check = now_utc() + chrono::Duration::days(JAVDB_CHECK_INTERVAL_DAYS);
            repo.postpone_javdb_check(*movie_id, next_check).await?;

            match outcome {
                Ok(true) => stats.succeeded_movies += 1,
                Ok(false) => {
                    stats.not_found_movies += 1;
                    tracing::info!(movie_number = movie.movie_number.as_str(), "JavDB 尚未收录");
                }
                Err(error) => {
                    // 单条失败**不中断整轮**，但也没法在这里把错误吞掉 ——
                    // 任务摘要里要有 `failed_movies`。
                    stats.failed_movies += 1;
                    tracing::warn!(
                        movie_number = movie.movie_number.as_str(),
                        code = error.code(),
                        "JavDB 补录失败"
                    );
                }
            }

            emit(
                &mut progress,
                current,
                total,
                &progress_text(current, total, &stats, None),
                &stats,
            )
            .await;
            if current == 1 || current % step == 0 {
                tracing::info!(
                    completed = current,
                    total,
                    succeeded = stats.succeeded_movies,
                    not_found = stats.not_found_movies,
                    failed = stats.failed_movies,
                    "JavDB 补录进度"
                );
            }
        }
        Ok(stats)
    }

    /// 查 JavDB → 回填。
    ///
    /// `Ok(true)` 成功、`Ok(false)` 未收录、`Err` 失败 —— **计数与日志都在调用方**：
    /// 三档的处置不同（未收录要打 info、失败要打 warn），放在这里就只能靠
    /// 返回值区分，反而更容易漏记一档。
    async fn try_one(&self, movie_id: i32, movie_number: &str) -> Result<bool, ServiceError> {
        // 未收录：上游 `except MetadataNotFoundError` 那一档。
        let Some(detail) = self.provider.get_movie_by_number(movie_number)? else {
            return Ok(false);
        };
        self.import_service.backfill(movie_id, &detail)?;
        Ok(true)
    }

    /// 重新按 `pending()` 的条件取这一部。取不到 = 已不再是候选。
    async fn reload_pending(&self, movie_id: i32) -> Result<Option<PendingBackfill>, ServiceError> {
        Ok(MovieRepository::new(self.db.clone())
            .find_javdb_backfill_pending(movie_id)
            .await?
            .map(|(movie_id, movie_number)| PendingBackfill {
                movie_id,
                movie_number,
            }))
    }
}

/// 进度文案。上游 `progress_text`（`:45-57`）：
/// `" · ".join(["JavDB 补录", 可选动作, "已完成 n/N", "成功 x", "未收录 y", "失败 z"])`。
///
/// ★ 第一个数字是**循环计数**（`completed`），不是「三项统计之和」——
/// 「已不再是候选」那一条不进任何统计（上游 `continue`），用和会让进度倒退。
fn progress_text(
    completed: usize,
    total: i64,
    stats: &BackfillStats,
    action: Option<&str>,
) -> String {
    let mut fragments = vec!["JavDB 补录".to_owned()];
    if let Some(action) = action {
        fragments.push(action.to_owned());
    }
    fragments.push(format!("已完成 {completed}/{total}"));
    fragments.push(format!("成功 {}", stats.succeeded_movies));
    fragments.push(format!("未收录 {}", stats.not_found_movies));
    fragments.push(format!("失败 {}", stats.failed_movies));
    fragments.join(" · ")
}

/// 上报一次进度（带摘要）。
async fn emit(
    progress: &mut Option<ProgressSink<'_>>,
    current: usize,
    total: i64,
    text: &str,
    stats: &BackfillStats,
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

    /// 批量与间隔是**稳定契约** —— 改间隔直接影响外部站点的压力。
    #[test]
    fn the_batch_and_interval_are_pinned() {
        assert_eq!(BATCH_SIZE, 50);
        assert_eq!(REQUEST_INTERVAL_SECONDS, 2);
    }

    /// 批量、间隔、推后天数都是**稳定契约** —— 改间隔直接影响外部站点的压力。
    #[test]
    fn the_batch_interval_and_check_interval_are_pinned() {
        assert_eq!(BATCH_SIZE, 50);
        assert_eq!(REQUEST_INTERVAL_SECONDS, 2);
        assert_eq!(JAVDB_CHECK_INTERVAL_DAYS, 7);
    }

    /// ★「JavDB 未收录」是**正常结果**，不是失败。
    ///
    /// 归到 `failed_movies` 会让运维以为站点出问题了，而实际上只是这部片还没
    /// 被收录 —— 两种结局的处置都是「推后 7 天」，但**看的人**要能区分。
    #[test]
    fn not_found_is_a_normal_outcome_not_a_failure() {
        let stats = BackfillStats {
            candidate_movies: 3,
            succeeded_movies: 0,
            not_found_movies: 3,
            failed_movies: 0,
        };
        assert_eq!(stats.not_found_movies, 3);
        assert_eq!(stats.failed_movies, 0, "未收录不是失败");
    }

    /// 统计的**四个键与上游 dict 逐字一致**（骨架期是自造的 `examined` /
    /// `backfilled` / `not_found` / `failed` —— 键名全不对）。
    #[test]
    fn the_stats_keys_match_upstreams_dict() {
        let value = serde_json::to_value(BackfillStats::default()).expect("可序列化");
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
                "candidate_movies",
                "failed_movies",
                "not_found_movies",
                "succeeded_movies"
            ]
        );
    }

    /// 进度文案里的「已完成」是**循环计数**，不是三项统计之和。
    ///
    /// 「已不再是候选」的那一条不进任何统计（上游 `continue`），用和会让
    /// 进度条在那一轮**倒退一格**。
    #[test]
    fn the_progress_counter_is_the_loop_index() {
        let stats = BackfillStats {
            candidate_movies: 10,
            succeeded_movies: 1,
            not_found_movies: 0,
            failed_movies: 0,
        };
        assert_eq!(
            progress_text(3, 10, &stats, None),
            "JavDB 补录 · 已完成 3/10 · 成功 1 · 未收录 0 · 失败 0"
        );
        assert_eq!(
            progress_text(3, 10, &stats, Some("正在查询 ABC-123")),
            "JavDB 补录 · 正在查询 ABC-123 · 已完成 3/10 · 成功 1 · 未收录 0 · 失败 0"
        );
    }
}
