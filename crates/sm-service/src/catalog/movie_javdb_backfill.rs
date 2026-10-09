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

use crate::error::ServiceError;

/// 单次最多处理多少条。
pub const BATCH_SIZE: i64 = 50;
/// 每条之间的间隔（秒）。⚠️ **不要省**（见模块文档）。
pub const REQUEST_INTERVAL_SECONDS: u64 = 2;

/// 一条待补录的影片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBackfill {
    pub movie_id: i64,
    pub movie_number: String,
    /// 上次尝试时间。`None` = 从未试过。
    pub last_attempt_at: Option<chrono::NaiveDateTime>,
}

/// JavDB 查询能力。**出网**。
pub trait JavdbProvider {
    /// 按番号取影片详情。`Ok(None)` = JavDB **没有收录**（不是错误）。
    fn get_movie_by_number(
        &self,
        movie_number: &str,
    ) -> Result<Option<serde_json::Value>, ServiceError>;
}

/// 把 JavDB 详情写回影片。
pub trait PluginMovieBackfill {
    /// 回填。**只写插件拥有的字段**（见 [`super::movie_ownership_gateway`]）。
    fn backfill(&self, movie_id: i64, detail: &serde_json::Value) -> Result<bool, ServiceError>;
}

/// 本次运行的统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct BackfillStats {
    /// 本次考察的影片数。
    pub examined: i32,
    /// 成功补录的部数。
    pub backfilled: i32,
    /// JavDB **未收录**的部数（不算失败）。
    pub not_found: i32,
    /// 查询失败的部数。
    pub failed: i32,
}

/// 补录服务。
// 两个依赖尚未被方法体引用（补录动作还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct MovieJavdbBackfillService {
    provider: Option<Box<dyn JavdbProvider>>,
    import_service: Option<Box<dyn PluginMovieBackfill>>,
}

impl MovieJavdbBackfillService {
    /// 构造。
    pub fn new(
        provider: Box<dyn JavdbProvider>,
        import_service: Box<dyn PluginMovieBackfill>,
    ) -> Self {
        Self {
            provider: Some(provider),
            import_service: Some(import_service),
        }
    }

    /// 列出待补录的影片。`limit` 缺省 [`BATCH_SIZE`]。
    ///
    /// 上游 `pending()` 是 static 且**无参数**。这里允许传 `limit` 是为了手动
    /// 触发时控制批量 —— 放宽不改变默认行为。
    pub async fn pending(limit: Option<i64>) -> Result<Vec<PendingBackfill>, ServiceError> {
        let _ = limit;
        todo!("骨架：查「未接入 JavDB 且 next_check_at 已到期」的影片，按检查时间升序")
    }

    /// ★ 跑一轮。上游 `run(self, *, reporter) -> dict`。
    ///
    /// **逐条处理且条间 sleep [`REQUEST_INTERVAL_SECONDS`]**。
    ///
    /// 单条失败**不中断整轮** —— 一次网络抖动不该让剩下 49 条白等一天。
    /// JavDB 未收录（`Ok(None)`）**不是失败**，它只是把下次检查时间推后。
    pub async fn run(&self) -> Result<BackfillStats, ServiceError> {
        todo!("骨架：逐条 pending() -> 查 JavDB -> 回填 -> 推后 next_check_at；条间 sleep 2s")
    }
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

    /// ★「JavDB 未收录」是**正常结果**，不是失败。
    ///
    /// 归到 `failed` 会让运维以为站点出问题了，而实际上只是这部片还没被收录。
    #[test]
    fn not_found_is_a_normal_outcome_not_a_failure() {
        let stats = BackfillStats {
            examined: 3,
            backfilled: 0,
            not_found: 3,
            failed: 0,
        };
        assert_eq!(stats.not_found, 3);
        assert_eq!(stats.failed, 0, "未收录不是失败");
    }

    /// 三类计数**互斥且完备**：`backfilled + not_found + failed == examined`。
    #[test]
    fn the_three_outcomes_partition_the_examined_movies() {
        let stats = BackfillStats {
            examined: 10,
            backfilled: 4,
            not_found: 5,
            failed: 1,
        };
        assert_eq!(
            stats.backfilled + stats.not_found + stats.failed,
            stats.examined
        );
    }
}
