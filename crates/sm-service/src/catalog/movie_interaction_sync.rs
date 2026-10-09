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

use crate::error::ServiceError;

/// 任务键。与 `cron_spec::builtin_jobs` 一致。
pub const TASK_KEY: &str = "movie_interaction_sync";

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

/// 互动数写回 + 重算热度。
pub trait InteractionWriter {
    /// 写互动数并**重算该部影片的热度**（不是全表）。
    fn write_interactions(
        &self,
        movie_id: i64,
        snapshot: &InteractionSnapshot,
    ) -> Result<(), ServiceError>;
}

/// 同步统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct InteractionSyncStats {
    pub examined: i32,
    /// 互动数**有变化**的部数。
    pub updated: i32,
    /// 查不到的部数。**不是失败**。
    pub not_found: i32,
    pub failed: i32,
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
// 两个依赖尚未被方法体引用（同步动作还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct MovieInteractionSyncService {
    provider: Option<Box<dyn InteractionProvider>>,
    writer: Option<Box<dyn InteractionWriter>>,
}

impl MovieInteractionSyncService {
    /// 构造。
    pub fn new(provider: Box<dyn InteractionProvider>, writer: Box<dyn InteractionWriter>) -> Self {
        Self {
            provider: Some(provider),
            writer: Some(writer),
        }
    }

    /// ★ 跑一轮。上游 `run(self, *, reporter) -> dict`。
    ///
    /// 单部失败**不中断**整批 —— 一部片的网络抖动不该让其余都停。
    pub async fn run(&self) -> Result<InteractionSyncStats, ServiceError> {
        todo!("骨架：查到期影片 -> 逐部查 JavDB -> 写互动数 + 重算该部热度")
    }
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
