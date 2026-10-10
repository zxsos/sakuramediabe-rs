//! 影片互动数刷新（上游 `catalog/movie_interaction_sync_service.py`，163 行）。
//!
//! # 任务键 `movie_interaction_sync`，cron `0 5 * * *`
//!
//! 热度靠互动数撑着（见 [`super::movie_heat`] 的四项权重），而互动数在**别的
//! 站**上。所以这个任务定期同步回来并重算热度。
//!
//! # 按影片时间戳分两档刷新，不是全量
//!
//! | 档 | 间隔 | 为什么 |
//! |---|---|---|
//! | 新片 | [`RECENT_REFRESH_INTERVAL_DAYS`] | 互动数变化快，多刷 |
//! | 旧片 | [`MIDDLE_REFRESH_INTERVAL_DAYS`] | 变化慢，少刷 |
//!
//! 全量刷 = 每天几百次外部请求；全不刷 = 热度过期。
//!
//! ⚠️ 还有**第三档**：发行日超过 [`MIDDLE_WINDOW_DAYS`]（180 天）的影片**不在
//! 候选集**里（上游 `:56-60`），除非它从未同步过，或已订阅且订阅晚于上次同步。
//! 这个「要不要管它」的判断发生在候选查询里，[`is_due`] 管不到 ——
//! 见 [`MIDDLE_WINDOW_DAYS`] 的文档。
//!
//! # 只同步五个字段
//!
//! [`INTERACTION_FIELDS`]。**不要**顺带同步封面、简介 —— 那些走
//! [`super::movie_metadata_refresh`]。两个任务职责缠在一起就难排查。
//!
//! # 同步完**只重算该部影片**的热度
//!
//! 用 `update_single_movie_heat` 而非全表重算 —— 后者会在一次同步后触发
//! 30 万行的 UPDATE，每天 5 点一次纯浪费。
//!
//! # ⚠️ 编排已就位，但两个 trait **还没有宿主实现**
//!
//! [`InteractionProvider`] 要 JavDB 元数据 provider（插件 ABI 那一层），
//! [`InteractionWriter`] 要用 `catalog_import` 的字段写回 + `movie_heat`
//! 的单部重算。两者都在宿主侧不存在，所以：
//!
//! - 本模块的 `run()` 是**完整可读、可测**的（候选 SQL、八项计数、
//!   单部失败不中断都在里面）；
//! - 但 `movie_interaction_sync` 的 **worker handler 仍未注册** ——
//!   任务被领取时会以 `NoHandler` 失败。这是阶段性事实，不是回归。
//!
//! 接线时**只需要实现那两个 trait**（外加一行 handler 注册），
//! 本文件一行都不用改 —— 判据不散在两处。

use sm_db::common::time::now_utc;
use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::error::ServiceError;

/// 任务键。与 `cron_spec::builtin_jobs` 一致。
pub const TASK_KEY: &str = "movie_interaction_sync";

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

/// 只同步这五个字段。**不要**扩充。
pub const INTERACTION_FIELDS: [&str; 5] = [
    "score",
    "score_number",
    "watched_count",
    "want_watch_count",
    "comment_count",
];

/// 新片刷新间隔（天）。上游 `RECENT_REFRESH_INTERVAL`（`:28`）。
pub const RECENT_REFRESH_INTERVAL_DAYS: i64 = 2;
/// 旧片刷新间隔（天）。上游 `MIDDLE_REFRESH_INTERVAL`（`:29`）。
pub const MIDDLE_REFRESH_INTERVAL_DAYS: i64 = 7;

/// 「新片」窗口（天）：发行日在此以内 → 用 [`RECENT_REFRESH_INTERVAL_DAYS`]。
///
/// 上游硬编码 `now - timedelta(days=60)`（`:43`），**不来自配置**。
pub const RECENT_WINDOW_DAYS: i64 = 60;

/// 「中段」窗口（天）：发行日在此以内（但已在 [`RECENT_WINDOW_DAYS`] 之外）
/// → 用 [`MIDDLE_REFRESH_INTERVAL_DAYS`]。
///
/// 上游硬编码 `now - timedelta(days=180)`（`:44`）。
///
/// ⚠️ **超过这个窗口的旧片根本不在候选集里**（`:56-60`：`release_date >= 180 天前`
/// 是那一档的条件之一），除非它从未同步过，或已订阅且订阅时刻晚于上次同步
/// （`:46-51`）。那个过滤发生在**候选查询**里 —— 也就是 [`is_due`] **表达不了**
/// 的部分：`is_due` 只看「距上次同步够不够久」，不看「还该不该管这部片」。
/// 实现 `run()` 的候选查询时别漏掉这一档。
pub const MIDDLE_WINDOW_DAYS: i64 = 180;

/// 该影片是否已**老到不再自动刷新**（发行日超过 [`MIDDLE_WINDOW_DAYS`]）。
///
/// 只表达日期那一半判据；「从未同步过」「订阅晚于同步」仍会让它进入候选集，
/// 所以候选查询要写成
/// `is_never_synced || subscribed_after_sync || !is_beyond_middle_window(..)` 的形状，
/// 而不是拿这个函数当全部条件。
pub fn is_beyond_middle_window(
    release_date: Option<chrono::NaiveDate>,
    now: chrono::NaiveDateTime,
) -> bool {
    // 没有发行日期时**不算**「老到不管」—— 上游那个条件是
    // `release_date >= middle_since`，NULL 不满足它，于是会落到「从未同步」
    // 或「订阅晚于同步」那两支去决定。这里返回 false 与之一致：
    // 由调用方继续看另外两支。
    match release_date {
        Some(release) => now.date().signed_duration_since(release).num_days() > MIDDLE_WINDOW_DAYS,
        None => false,
    }
}

/// 一份互动数快照。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InteractionSnapshot {
    pub score: f64,
    pub score_number: i64,
    pub watched_count: i64,
    pub want_watch_count: i64,
    pub comment_count: i64,
}

/// JavDB 互动数查询。**出网**。
pub trait InteractionProvider {
    /// 按 JavDB id 取互动数。`Ok(None)` = 该站没有这部片。
    fn get_interactions(
        &self,
        javdb_movie_id: &str,
    ) -> Result<Option<InteractionSnapshot>, ServiceError>;
}

/// 写回的结果。对应上游 `_fetch_and_apply` 的 `tuple[bool, int]`
/// （`movie_interaction_sync_service.py:65-82`）。
///
/// 两个数都要，因为它们进的是**两个不同的统计键**：
/// `changed` → `updated_movies` / `unchanged_movies`，
/// `heat_updated` → `heat_updated_movies`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InteractionWriteOutcome {
    /// 互动数**有变化**（上游：`bool(updated_fields)`）。
    /// 五个字段一个都没变就是 `false`，但那**照样算刷新成功**。
    pub changed: bool,
    /// 因此被重算热度的行数（上游 `update_single_movie_heat` 的返回值）。
    pub heat_updated: i64,
}

/// 互动数写回 + 重算热度。
pub trait InteractionWriter {
    /// 写互动数、**重算该部影片的热度**（不是全表），并记下同步时刻。
    ///
    /// 上游在**同一个事务**里做这三件事（`:67-81`）：写五个字段、
    /// 按新值重算这部片的热度、把 `interaction_synced_at` 置为现在。
    /// 第三件**即使互动数没变也要做** —— 否则下次调度会立刻再请求一次。
    fn write_interactions(
        &self,
        movie_id: i32,
        snapshot: &InteractionSnapshot,
    ) -> Result<InteractionWriteOutcome, ServiceError>;
}

/// 同步统计。**八个键与上游 `run` 的 dict 逐字一致**
/// （`movie_interaction_sync_service.py:86-95`）。
///
/// ⚠️ 骨架期是自造的四项（`examined` / `updated` / `not_found` / `failed`）。
/// 其中最容易错的是 **`not_found`**：上游**没有**这个键 ——
/// 「JavDB 上查不到这部片」被记进 **`failed_movies`** 并进 `failed_movie_ids`
/// （`:123-130`，日志文案写的是 skipped，但计数是 failed）。
/// 按那个错形状接线，运维会看到「失败 0」而实际有一批影片根本没同步。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InteractionSyncStats {
    /// 候选影片数（SQL 那一层筛出来的）。
    pub candidate_movies: i64,
    /// **处理过**的部数（成功 + 失败，含抛错的那一档）。
    pub processed_movies: i64,
    pub succeeded_movies: i64,
    /// 失败的部数（含「JavDB 上查不到」）。
    pub failed_movies: i64,
    /// 互动数**有变化**的部数。
    pub updated_movies: i64,
    /// 查到了但五个字段一个都没变的部数。
    pub unchanged_movies: i64,
    /// 被重算热度的行数（**不是**影片数：一行一部，所以两者通常相等）。
    pub heat_updated_movies: i64,
    /// 失败影片的 id，**按发生顺序**。
    ///
    /// 上游把它一起存进 `result_summary` —— 「失败了几部」不够用，
    /// 得知道是**哪几部**才能重跑或排查。
    pub failed_movie_ids: Vec<i32>,
}

/// 「新片」窗口：发行日在这么多天以内的按 [`RECENT_REFRESH_INTERVAL_DAYS`] 刷。
///
/// ⚠️ 这里曾经写着「上游从配置读，**不写死**」—— **那是错的**。上游
/// `movie_interaction_sync_service.py:43` 就是硬编码
/// `recent_since = now - timedelta(days=60)`，没有配置项。照抄 60。
pub fn recent_window_days() -> i64 {
    RECENT_WINDOW_DAYS
}

/// 该影片现在该刷吗（两档间隔）。**纯函数**。
///
/// ⚠️ 边界用 `>=`：恰好等于间隔时**算该刷**。用 `>` 会让「刚好差一天」的
/// 影片被永久跳过（每次判定都是 6.99 天）。
///
/// 没有发行日期时按**旧片**处理（刷得少）—— 不知道新老时保守一点。
pub fn is_due(
    last_synced_at: Option<chrono::NaiveDateTime>,
    release_date: Option<chrono::NaiveDate>,
    now: chrono::NaiveDateTime,
) -> bool {
    let Some(last) = last_synced_at else {
        return true; // 从未同步过 -> 优先
    };
    let interval = match release_date {
        Some(release) => {
            let age = now.date().signed_duration_since(release).num_days();
            if age <= recent_window_days() {
                RECENT_REFRESH_INTERVAL_DAYS
            } else {
                MIDDLE_REFRESH_INTERVAL_DAYS
            }
        }
        None => MIDDLE_REFRESH_INTERVAL_DAYS,
    };
    now.signed_duration_since(last).num_days() >= interval
}

/// 同步服务。
///
/// # 依赖是**注入**的（两个 trait），而 `Db` 是直接持有的
///
/// `provider` 要出网、`writer` 要写库并重算热度 —— 这两件事在本仓分别属于
/// 插件 ABI 与 `catalog_import` 的编排，宿主侧还没有。所以本模块只定义
/// **窄接口**，由组合根注入（与 `movie_thin_cover_backfill` 的取向一致）。
///
/// 但**候选查询**留在这里：它是纯 SQL、没有外部依赖，注入它只会让
/// 「哪些影片该刷」这条判据散到两个地方。
pub struct MovieInteractionSyncService {
    db: Db,
    provider: Box<dyn InteractionProvider>,
    writer: Box<dyn InteractionWriter>,
}

impl MovieInteractionSyncService {
    /// 构造。
    pub fn new(
        db: &Db,
        provider: Box<dyn InteractionProvider>,
        writer: Box<dyn InteractionWriter>,
    ) -> Self {
        Self {
            db: db.clone(),
            provider,
            writer,
        }
    }

    /// ★ 跑一轮。上游 `run(self, *, reporter) -> dict`（`:84-163`）。
    ///
    /// # 单部失败**不中断**整批
    ///
    /// 一部片的网络抖动不该让其余几百部都停。失败计进 `failed_movies` 并把 id
    /// 记进 `failed_movie_ids`，然后继续。
    ///
    /// # 「查不到」也记 **failed**
    ///
    /// [`InteractionProvider::get_interactions`] 返回 `Ok(None)`（该站没有这部
    /// 片）时，上游的 `MetadataNotFoundError` 分支把它计入 `failed_movies`
    /// （`:123-130`）—— 日志文案写的是 skipped，但**计数是 failed**。
    /// 这里照抄：把它单独记成「跳过」会让「失败 0」看起来像一切正常。
    pub async fn run(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<InteractionSyncStats, ServiceError> {
        let now = now_utc();
        let candidate_ids = MovieRepository::new(self.db.clone())
            .list_interaction_sync_candidate_ids(
                now - chrono::Duration::days(RECENT_WINDOW_DAYS),
                now - chrono::Duration::days(MIDDLE_WINDOW_DAYS),
                now - chrono::Duration::days(RECENT_REFRESH_INTERVAL_DAYS),
                now - chrono::Duration::days(MIDDLE_REFRESH_INTERVAL_DAYS),
            )
            .await?;

        let total = i64::try_from(candidate_ids.len()).unwrap_or(i64::MAX);
        let mut stats = InteractionSyncStats {
            candidate_movies: total,
            ..Default::default()
        };
        // 日志节流：上游 `step = max(total // 20, 1)`。
        let step = usize::try_from((total / 20).max(1)).unwrap_or(1);

        for (index, movie_id) in candidate_ids.iter().enumerate() {
            let current = index + 1;
            // 上游在这一步之前先取一次影片（拿番号显示在进度文案里）。
            let movie_number = self.movie_number(*movie_id).await;
            emit(
                &mut progress,
                current - 1,
                total,
                &progress_text(
                    &stats,
                    Some(&format!(
                        "正在刷新 {}",
                        movie_number.as_deref().unwrap_or("(已删除)")
                    )),
                ),
                &stats,
            )
            .await;

            match self.refresh_one(*movie_id).await {
                Ok(Some((changed, heat_updated))) => {
                    stats.succeeded_movies += 1;
                    if changed {
                        stats.updated_movies += 1;
                    } else {
                        stats.unchanged_movies += 1;
                    }
                    stats.heat_updated_movies += heat_updated;
                }
                // `Ok(None)` = 影片行没了（两次查询之间被删）或 provider 说没有。
                // 两者上游都落在失败档（见方法文档）。
                Ok(None) => {
                    stats.failed_movies += 1;
                    stats.failed_movie_ids.push(*movie_id);
                }
                Err(error) => {
                    stats.failed_movies += 1;
                    stats.failed_movie_ids.push(*movie_id);
                    tracing::warn!(movie_id, code = error.code(), "影片互动数同步失败");
                }
            }
            stats.processed_movies += 1;

            emit(
                &mut progress,
                current,
                total,
                &progress_text(&stats, None),
                &stats,
            )
            .await;
            if current == 1 || current % step == 0 {
                tracing::info!(
                    completed = current,
                    total,
                    succeeded = stats.succeeded_movies,
                    failed = stats.failed_movies,
                    "影片互动数同步进度"
                );
            }
        }
        Ok(stats)
    }

    /// 刷新一部：查 JavDB → 写回 + 重算热度。
    ///
    /// `Ok(None)` 有三种来源，上游全归失败档：影片行没了、没有 `javdb_id`、
    /// provider 说该站没有这部片。
    async fn refresh_one(&self, movie_id: i32) -> Result<Option<(bool, i64)>, ServiceError> {
        let Some(javdb_id) = self.javdb_id(movie_id).await else {
            return Ok(None);
        };
        let Some(snapshot) = self.provider.get_interactions(&javdb_id)? else {
            return Ok(None);
        };
        let outcome = self.writer.write_interactions(movie_id, &snapshot)?;
        Ok(Some((outcome.changed, outcome.heat_updated)))
    }

    /// 取 `javdb_id`。影片行没了或该列为空都返回 `None`
    /// （上游 `Movie.get_by_id` 抛 `DoesNotExist`，落失败档）。
    async fn javdb_id(&self, movie_id: i32) -> Option<String> {
        MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await
            .ok()
            .flatten()
            .and_then(|movie| movie.javdb_id)
            .filter(|id| !id.is_empty())
    }

    /// 取番号，**只用于进度文案**。取不到不算失败（下一步照样会走失败档）。
    async fn movie_number(&self, movie_id: i32) -> Option<String> {
        MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await
            .ok()
            .flatten()
            .map(|movie| movie.movie_number)
    }
}

/// 进度文案。上游 `progress_text`（`:100-111`）：
/// `" · ".join(["影片互动数同步", 可选动作, "已完成 n/N", "成功 x", "失败 y"])`。
fn progress_text(stats: &InteractionSyncStats, action: Option<&str>) -> String {
    let mut fragments = vec!["影片互动数同步".to_owned()];
    if let Some(action) = action {
        fragments.push(action.to_owned());
    }
    fragments.push(format!(
        "已完成 {}/{}",
        stats.processed_movies, stats.candidate_movies
    ));
    fragments.push(format!("成功 {}", stats.succeeded_movies));
    fragments.push(format!("失败 {}", stats.failed_movies));
    fragments.join(" · ")
}

/// 上报一次进度（带摘要）。两边都调它 —— 上游循环里的两处 `emit` 只差
/// `current` 与文案。
async fn emit(
    progress: &mut Option<ProgressSink<'_>>,
    current: usize,
    total: i64,
    text: &str,
    stats: &InteractionSyncStats,
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

    fn at(text: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").expect("时间")
    }

    /// 从未同步过的影片**必须**立刻刷。
    #[test]
    fn a_never_synced_movie_is_always_due() {
        assert!(is_due(None, None, at("2026-10-05T00:00:00")));
    }

    /// ★ 边界**包含**：恰好等于间隔时该刷。
    #[test]
    fn the_interval_boundary_is_inclusive() {
        let last = at("2026-10-01T00:00:00");
        assert!(is_due(Some(last), None, at("2026-10-08T00:00:00")));
        assert!(!is_due(Some(last), None, at("2026-10-07T23:59:59")));
    }

    /// 新片比旧片刷得**更勤**。
    #[test]
    fn recent_movies_refresh_more_often() {
        let last = at("2026-10-03T00:00:00");
        let now = at("2026-10-05T00:00:00");
        let recent = chrono::NaiveDate::from_ymd_opt(2026, 10, 2).expect("日期");
        let old = chrono::NaiveDate::from_ymd_opt(2026, 6, 27).expect("日期");
        assert!(is_due(Some(last), Some(recent), now), "新片 2 天就该刷");
        assert!(!is_due(Some(last), Some(old), now), "旧片 7 天才刷");
    }

    /// ★ 两个窗口是**上游写死的 60 / 180**，不是配置项。
    ///
    /// 这条用例盯着一个曾经写错的文档：`recent_window_days()` 的注释原来说
    /// 「上游从配置读」—— 上游 `movie_interaction_sync_service.py:43-44` 是
    /// 硬编码。值不值得用测试钉住：窗口改一天，全站请求量就变一个量级。
    #[test]
    fn the_windows_are_hardcoded_upstream_values() {
        assert_eq!(recent_window_days(), 60);
        assert_eq!(MIDDLE_WINDOW_DAYS, 180);
    }

    /// 统计的**八个键与上游 dict 逐字一致**。
    ///
    /// 这条盯着骨架期那个自造的四项形状：最要命的是 `not_found` ——
    /// 上游**没有**这个键，「JavDB 上查不到」是记进 `failed_movies` 的。
    /// 按错形状接线，运维看到「失败 0」而实际有一批影片根本没同步。
    #[test]
    fn the_stats_keys_match_upstreams_dict() {
        let stats = InteractionSyncStats {
            candidate_movies: 10,
            processed_movies: 10,
            succeeded_movies: 8,
            failed_movies: 2,
            updated_movies: 5,
            unchanged_movies: 3,
            heat_updated_movies: 5,
            failed_movie_ids: vec![7, 9],
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
                "candidate_movies",
                "failed_movie_ids",
                "failed_movies",
                "heat_updated_movies",
                "processed_movies",
                "succeeded_movies",
                "unchanged_movies",
                "updated_movies",
            ]
        );
        assert_eq!(
            (
                stats.succeeded_movies + stats.failed_movies,
                stats.processed_movies
            ),
            (10, 10),
            "成功 + 失败 = 处理过的部数"
        );
        assert_eq!(
            (
                stats.updated_movies + stats.unchanged_movies,
                stats.succeeded_movies
            ),
            (8, 8),
            "有变化 + 没变化 = 成功的部数"
        );
    }

    /// 进度文案与上游 `progress_text` 逐字一致（含 `·` 分隔符与动作段）。
    #[test]
    fn the_progress_text_matches_upstream() {
        let stats = InteractionSyncStats {
            candidate_movies: 10,
            processed_movies: 3,
            succeeded_movies: 2,
            failed_movies: 1,
            ..Default::default()
        };
        assert_eq!(
            progress_text(&stats, None),
            "影片互动数同步 · 已完成 3/10 · 成功 2 · 失败 1"
        );
        assert_eq!(
            progress_text(&stats, Some("正在刷新 ABC-123")),
            "影片互动数同步 · 正在刷新 ABC-123 · 已完成 3/10 · 成功 2 · 失败 1"
        );
    }

    /// ★ 180 天之外的旧片：**日期这一半的判据**判定为「不再管」。
    ///
    /// 它只看日期（上游那个档的条件之一）；「从未同步」与「订阅晚于同步」
    /// 是另外两支，由候选查询处理 —— 所以这里同时钉住「NULL 发行日不算老到
    /// 不管」，否则会有一批没有发行日的影片被这条判据误杀。
    #[test]
    fn the_middle_window_cutoff_only_reads_the_release_date() {
        let now = at("2026-10-05T00:00:00");
        let fresh = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).expect("日期");
        let inside = chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("日期"); // 157 天
        let beyond = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("日期"); // 277 天

        assert!(!is_beyond_middle_window(Some(fresh), now));
        assert!(!is_beyond_middle_window(Some(inside), now));
        assert!(is_beyond_middle_window(Some(beyond), now));
        assert!(
            !is_beyond_middle_window(None, now),
            "没有发行日期时不能凭这条判据把它排除掉"
        );
    }
}
