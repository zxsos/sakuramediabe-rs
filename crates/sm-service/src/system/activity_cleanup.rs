//! 活动中心保留期清理，对应上游
//! `src/service/system/activity_cleanup_service.py`（101 行）。
//!
//! 由调度器的 `activity_record_cleanup` 任务调用（cron `30 5 * * *`，
//! 见 `sm_scheduler::builtin_jobs`），也支持手动触发。
//!
//! # 两条保留期规则
//!
//! | 表 | 规则 | 上游配置字段 |
//! |---|---|---|
//! | `background_task_run` | 每个 `task_key` 只保留最近 N 条**终态**记录 | `activity_task_run_retention_per_key`（默认 200） |
//! | `system_notification` | 删「已读且 `read_at` 早于 N 天前」的通知；**未读一律保留** | `activity_notification_read_retention_days`（默认 3） |
//!
//! # 三条容易写错的
//!
//! **① 保留额只在终态里算。** 上游的阈值查询带 `state IN ("completed",
//! "failed")` 过滤。若不过滤，一条陈旧的 `pending` 会占掉保留额，把真正的
//! 历史挤掉；更糟的是分界 id 之后所有 id 更小的行都会被删 —— **包括那条
//! `pending`**。所以「pending / running 无论多旧都不能被清理」是硬要求。
//!
//! **② 删台账前先把通知的 `related_task_run_id` 置空。** 上游显式做了这一步，
//! 注释写着「避免悬挂引用，不依赖数据库级联行为」。通知是独立实体，它自己
//! 的保留期由规则 ② 管 —— 台账被回收后，「某某任务失败了」这条通知仍然是
//! 用户要看的事实。两步必须同事务，否则中间那一瞬通知指向已删的行。
//!
//! **③ 通知用 `read_at` 而不是 `created_at` 作为窗口基准。** 30 天前创建、
//! 昨天才读的通知，按 `read_at` 算还有保留期。上游用的就是 `read_at`。
//!
//! # 上游还有一个计数，本批不实现
//!
//! `cleanup()` 还调 `MovieMetadataSearchService.cleanup_search_assets()`，
//! 返回的 `stats` 里有第三个键 `deleted_metadata_search_assets`。那是
//! `catalog` 域的资产清理（上游 27 文件 / 7,556 行，未开工）。
//!
//! 所以 [`ActivityCleanupStats`] 只有两个字段 —— **不返回那个 0**。填 0 会让
//! 任务中心显示「清理了 0 项」而实际是「没做」，比缺字段更难排查。
//! 客户端读的两个键都在。

use std::collections::BTreeMap;

use serde::Serialize;
use sm_db::common::time::now_utc;
use sm_db::repo::{BackgroundTaskRunRepository, Ctx, SystemNotificationRepository};
use sm_db::Db;

use crate::error::ServiceError;

/// 清理结果。会进 `background_task_run.result_summary`（JsonTextField），
/// 在任务中心显示 —— 所以字段名**就是**客户端要读的键。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ActivityCleanupStats {
    /// 删掉的 `background_task_run` 行数。
    pub deleted_task_runs: u64,
    /// 删掉的 `system_notification` 行数。
    pub deleted_notifications: u64,
}

/// 保留期参数。默认值抄自上游 `Settings.scheduler`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// 每个 `task_key` 保留多少条终态运行记录。
    pub task_runs_per_key: i64,
    /// 已读通知保留多少天。
    pub read_notification_days: i64,
}

impl Default for RetentionPolicy {
    /// 上游 `activity_task_run_retention_per_key = 200`、
    /// `activity_notification_read_retention_days = 3`。
    fn default() -> Self {
        Self {
            task_runs_per_key: 200,
            read_notification_days: 3,
        }
    }
}

impl RetentionPolicy {
    /// 校验。负值一律拒绝 —— 它会让阈值查询的 `OFFSET` 变成「从末尾倒数」，
    /// 语义完全不是「保留 N 条」，而是「删掉最新的 N 条」。
    pub fn validate(&self) -> Result<(), ServiceError> {
        for (name, value) in [
            ("task_runs_per_key", self.task_runs_per_key),
            ("read_notification_days", self.read_notification_days),
        ] {
            if value < 0 {
                return Err(ServiceError::validation(
                    "validation_error",
                    format!("{name} 不能为负"),
                ));
            }
        }
        Ok(())
    }
}

/// 活动中心清理 service。
pub struct ActivityCleanupService {
    tasks: BackgroundTaskRunRepository,
    notifications: SystemNotificationRepository,
    pool: Db,
}

impl ActivityCleanupService {
    pub fn new(db: &Db) -> Self {
        Self {
            tasks: BackgroundTaskRunRepository::new(db.clone()),
            notifications: SystemNotificationRepository::new(db.clone()),
            pool: db.clone(),
        }
    }

    /// 跑一次清理。
    ///
    /// # 为什么逐 key 提交而不是全局一个事务
    ///
    /// 19 个内建任务各删一次，一个大事务会把所有行锁攥到最后一刻。而每步之间
    /// 本来就不需要一致性 —— 下一轮清理会把没清完的接着清，而清理**幂等**
    /// （下次的阈值只会更高）。
    pub async fn cleanup(
        &self,
        policy: RetentionPolicy,
    ) -> Result<ActivityCleanupStats, ServiceError> {
        policy.validate()?;
        let deleted_task_runs = self.cleanup_task_runs(policy.task_runs_per_key).await?;
        let deleted_notifications = self
            .cleanup_notifications(policy.read_notification_days)
            .await?;
        Ok(ActivityCleanupStats {
            deleted_task_runs,
            deleted_notifications,
        })
    }

    /// 按 key 清理运行记录。返回总共删了几行。
    async fn cleanup_task_runs(&self, retention: i64) -> Result<u64, ServiceError> {
        let keys = self.tasks.distinct_task_keys().await?;
        // BTreeMap 只为让日志按 key 有序输出，便于与上游逐 key 对照；不影响语义。
        let mut per_key: BTreeMap<&str, u64> = BTreeMap::new();
        for key in &keys {
            let deleted = self.cleanup_one_key(key, retention).await?;
            if deleted > 0 {
                per_key.insert(key.as_str(), deleted);
            }
        }
        let total: u64 = per_key.values().sum();
        if total > 0 {
            tracing::info!(
                retention,
                keys = ?per_key,
                deleted = total,
                "已清理任务运行记录"
            );
        }
        Ok(total)
    }

    /// 清理一个 `task_key` 的终态记录。
    async fn cleanup_one_key(&self, task_key: &str, retention: i64) -> Result<u64, ServiceError> {
        // 终态记录不足 `retention` 条 → 阈值查询给出 None → 无事可做。
        let Some(threshold_id) = self
            .tasks
            .retention_threshold_id(task_key, retention)
            .await?
        else {
            return Ok(0);
        };

        // 两步同事务：先置空通知外键，再删台账。顺序反了就会出现
        // 「通知指向刚被删的行」的窗口。
        let mut tx = self.pool.begin().await?;
        let deleted = {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            let stale_ids = self
                .tasks
                .stale_terminal_ids_in(&mut ctx, task_key, threshold_id)
                .await?;
            if stale_ids.is_empty() {
                0
            } else {
                // 先解除引用。通知本身**不删** —— 它的保留期由规则 ② 管。
                self.notifications
                    .detach_task_runs_in(&mut ctx, &stale_ids)
                    .await?;
                self.tasks
                    .delete_terminal_through_in(&mut ctx, task_key, threshold_id)
                    .await?
            }
        };
        tx.commit().await?;
        Ok(deleted)
    }

    /// 清理已读且过期的通知。
    async fn cleanup_notifications(&self, retention_days: i64) -> Result<u64, ServiceError> {
        let cutoff = now_utc() - chrono::Duration::days(retention_days);
        let mut tx = self.pool.begin().await?;
        let deleted = {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            self.notifications
                .delete_read_before_in(&mut ctx, cutoff)
                .await?
        };
        tx.commit().await?;
        if deleted > 0 {
            tracing::info!(retention_days, deleted, "已清理已读通知");
        }
        Ok(deleted)
    }
}
