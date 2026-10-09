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

/// 新片刷新间隔（天）。
pub const RECENT_REFRESH_INTERVAL_DAYS: i64 = 2;
/// 旧片刷新间隔（天）。
pub const MIDDLE_REFRESH_INTERVAL_DAYS: i64 = 7;

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

/// 「新片」的判定天数。上游从配置读，**不写死**。
pub fn recent_window_days() -> i64 {
    todo!("骨架：从 config 读「新片」窗口")
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
}
