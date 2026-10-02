//! `system::activity` 模型（2 张表）。
//!
//! 对应 `src/model/system/activity.py`。
//!
//! # `background_task_run` 是任务队列，不是日志表
//!
//! 表头注释写得很直接：**「pending 行即队列元素；lease_expires_at 过期即可回收」**。
//! 配套索引 `(state, scheduled_at)` 服务于领取路径：
//!
//! ```sql
//! WHERE state = 'pending' AND scheduled_at <= now ORDER BY id
//! ```
//!
//! 所以「取一个待执行任务」是本表的主查询，而不是按时间范围查历史。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `background_task_run.state` 取值。
///
/// **注意来源差异**：`PENDING` 有源码证据（列默认值 `"pending"`，且队列
/// 查询 `WHERE state='pending'`），`RUNNING` 由租约语义推导 —— 表头注释说
/// 「lease_expires_at 过期即可回收」，隐含存在一个「已领取未回收」的中间态。
/// 其余终态名在 `src/model/` 下没有常量定义，实际字面量由 service 层决定，
/// 需要在迁移 service 时与上游对齐。
pub mod task_state {
    /// 初始态，也是队列元素。**源码可证**。
    pub const PENDING: &str = "pending";

    /// 已领取、持有租约。由租约回收语义推导。
    pub const RUNNING: &str = "running";

    /// 正常结束。由 `finished_at` 的存在推导，具体字面量待与 service 对齐。
    pub const SUCCEEDED: &str = "succeeded";

    /// 异常结束。由 `error_message` 的存在推导，具体字面量待与 service 对齐。
    pub const FAILED: &str = "failed";

    /// 是否为终态。
    pub fn is_terminal(state: &str) -> bool {
        state == SUCCEEDED || state == FAILED
    }
}

/// `background_task_run` 表：后台任务台账兼队列。
#[derive(Debug, Clone, FromRow)]
pub struct BackgroundTaskRun {
    pub id: i64,
    /// 任务类型键，与 `task_name` 配合定位处理器。
    pub task_key: String,
    /// 人类可读的任务名。
    pub task_name: String,
    /// 触发方式（定时 / 手动 / 事件等）。数据库无 CHECK 约束。
    pub trigger_type: String,
    /// 互斥键。**单列唯一索引** —— 同刻只允许一个同键任务在跑。
    ///
    /// 为 NULL 表示不参与互斥；因为 NULL 不参与唯一约束，
    /// 多个无互斥需求的任务可以共存。
    pub mutex_key: Option<String>,
    /// 状态，默认 `pending`。
    pub state: String,
    /// 进度三件套。三者都可空 —— 不可量化的任务不填。
    pub progress_current: Option<i32>,
    pub progress_total: Option<i32>,
    pub progress_text: Option<String>,
    /// 结构化结果摘要，`JsonTextField`。
    pub result_summary: Option<String>,
    /// 文本结果。
    pub result_text: Option<String>,
    pub error_message: Option<String>,
    /// 实际开始执行的时刻。
    pub started_at: Option<NaiveDateTime>,
    /// 结束时刻。与 `started_at` 一起构成耗时统计。
    pub finished_at: Option<NaiveDateTime>,
    /// 入参 JSON。注释强调「pending 行即队列元素」——参数随行一起排队。
    pub params: Option<String>,
    /// 计划执行时刻。NULL 通常表示「立即可领」。
    pub scheduled_at: Option<NaiveDateTime>,
    /// 租约到期时刻。过期的 running 行可被回收成 pending。
    pub lease_expires_at: Option<NaiveDateTime>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl BackgroundTaskRun {
    /// 该行是否可被领取执行。
    ///
    /// 对应队列查询 `state = 'pending' AND scheduled_at <= now`。
    /// `scheduled_at` 为 NULL 视为立即可领。
    pub fn is_claimable(&self, now: NaiveDateTime) -> bool {
        self.state == task_state::PENDING && self.scheduled_at.is_none_or(|at| at <= now)
    }

    /// 是否持有租约。
    pub fn has_lease(&self) -> bool {
        self.lease_expires_at.is_some()
    }

    /// 租约是否已过期 —— 满足 `state = running` 时可回收。
    ///
    /// 没有这一步，一个崩溃的 worker 会让任务永久卡在 running。
    pub fn lease_expired(&self, now: NaiveDateTime) -> bool {
        self.lease_expires_at.is_some_and(|exp| exp < now)
    }

    /// 是否处于「曾经领取、租约已失效、状态仍是 running」的僵尸态。
    pub fn is_stale_lease(&self, now: NaiveDateTime) -> bool {
        self.state == task_state::RUNNING && self.lease_expired(now)
    }

    /// 是否持有互斥键。
    pub fn is_mutex_guarded(&self) -> bool {
        self.mutex_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }

    /// 进度百分比。缺少总量或总量为 0 时返回 `None`。
    pub fn progress_ratio(&self) -> Option<f64> {
        let current = self.progress_current?;
        let total = self.progress_total?;
        if total <= 0 {
            return None;
        }
        Some(current as f64 / total as f64)
    }
}

/// 系统通知的分类。
pub mod notification_category {
    pub const ALL: [&str; 0] = [];
}

/// `system_notification` 表：站内通知。
///
/// # 两套字段并存，迁移时不要合并
///
/// 注释：「事件身份与展示关联分离：旧 related_resource_* 继续服务现有 API」。
///
/// | 用途 | 字段 |
/// |---|---|
/// | 事件身份 | `event_type` / `resource_type` / `resource_id` |
/// | 展示关联（遗留） | `related_resource_type` / `related_resource_id` / `related_task_run_id` |
///
/// 新旧两套并存是过渡期的有意设计，合并会破坏现有 API 契约。
#[derive(Debug, Clone, FromRow)]
pub struct SystemNotification {
    pub id: i64,
    /// 通知分类，有索引。
    pub category: String,
    pub title: String,
    pub content: String,
    /// 事件类型。新身份字段。
    pub event_type: Option<String>,
    /// 去重键。**唯一索引** —— 同一事件只产生一条通知。
    ///
    /// NULL 不参与唯一约束，所以没有去重需求的普通通知不受影响。
    pub dedupe_key: Option<String>,
    /// 事件资源类型。新身份字段。
    pub resource_type: Option<String>,
    /// 事件资源 ID。新身份字段，与 `resource_type` 配对使用。
    pub resource_id: Option<i32>,
    pub is_read: bool,
    pub read_at: Option<NaiveDateTime>,
    /// 关联任务台账。删台账行只置空，通知保留。
    pub related_task_run_id: Option<i64>,
    /// 遗留展示关联类型。
    pub related_resource_type: Option<String>,
    /// 遗留展示关联 ID。
    pub related_resource_id: Option<i32>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl SystemNotification {
    /// 是否启用去重。
    pub fn is_deduplicated(&self) -> bool {
        self.dedupe_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }

    /// 是否属于遗留的展示关联模式。
    pub fn uses_legacy_relation(&self) -> bool {
        self.related_resource_type.is_some() || self.related_resource_id.is_some()
    }

    /// 是否走新事件身份模型。
    pub fn uses_event_identity(&self) -> bool {
        self.event_type.is_some()
    }

    /// 已读标记与 `read_at` 是否一致。
    ///
    /// 两者可以不一致：先置 `is_read` 再补 `read_at` 是常见的两步写法，
    /// 所以这里只报告矛盾，不假定哪边是真相。
    pub fn read_state_inconsistent(&self) -> bool {
        self.is_read != self.read_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn run(
        state: &str,
        scheduled: Option<NaiveDateTime>,
        lease: Option<NaiveDateTime>,
    ) -> BackgroundTaskRun {
        BackgroundTaskRun {
            id: 1,
            task_key: "probe".to_owned(),
            task_name: "probe".to_owned(),
            trigger_type: "manual".to_owned(),
            mutex_key: None,
            state: state.to_owned(),
            progress_current: None,
            progress_total: None,
            progress_text: None,
            result_summary: None,
            result_text: None,
            error_message: None,
            started_at: None,
            finished_at: None,
            params: None,
            scheduled_at: scheduled,
            lease_expires_at: lease,
            created_at: None,
            updated_at: None,
        }
    }

    fn note() -> SystemNotification {
        SystemNotification {
            id: 1,
            category: "plugin".to_owned(),
            title: "t".to_owned(),
            content: "c".to_owned(),
            event_type: None,
            dedupe_key: None,
            resource_type: None,
            resource_id: None,
            is_read: false,
            read_at: None,
            related_task_run_id: None,
            related_resource_type: None,
            related_resource_id: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn pending_row_is_the_queue_element() {
        // 对应队列查询 state='pending' AND scheduled_at <= now
        let now = at(2026, 10, 2, 12, 0);
        assert!(run(task_state::PENDING, None, None).is_claimable(now));
        assert!(run(task_state::PENDING, Some(at(2026, 10, 2, 11, 0)), None).is_claimable(now));
        assert!(
            !run(task_state::PENDING, Some(at(2026, 10, 2, 13, 0)), None).is_claimable(now),
            "尚未到计划时刻，不可领取"
        );
        assert!(
            !run(task_state::RUNNING, None, None).is_claimable(now),
            "只有 pending 行是队列元素"
        );
    }

    #[test]
    fn expired_lease_is_reclaimable() {
        // 没有这一步，崩溃的 worker 会让任务永久卡在 running。
        let now = at(2026, 10, 2, 12, 0);
        let stale = run(task_state::RUNNING, None, Some(at(2026, 10, 2, 11, 0)));
        assert!(stale.has_lease());
        assert!(stale.lease_expired(now));
        assert!(stale.is_stale_lease(now));

        let fresh = run(task_state::RUNNING, None, Some(at(2026, 10, 2, 13, 0)));
        assert!(!fresh.lease_expired(now));
        assert!(!fresh.is_stale_lease(now));

        let unleased = run(task_state::RUNNING, None, None);
        assert!(!unleased.has_lease());
        assert!(!unleased.is_stale_lease(now), "无租约的行不由租约逻辑回收");
    }

    #[test]
    fn mutex_key_presence_distinguishes_guarded_tasks() {
        let mut r = run(task_state::PENDING, None, None);
        assert!(!r.is_mutex_guarded());
        r.mutex_key = Some("plugin:actor:sync".to_owned());
        assert!(r.is_mutex_guarded());
        r.mutex_key = Some("  ".to_owned());
        assert!(!r.is_mutex_guarded(), "空白视同无互斥需求");
    }

    #[test]
    fn progress_ratio_needs_positive_total() {
        let mut r = run(task_state::RUNNING, None, None);
        assert_eq!(r.progress_ratio(), None);
        r.progress_current = Some(3);
        r.progress_total = Some(0);
        assert_eq!(r.progress_ratio(), None, "总量为 0 不做除法");
        r.progress_total = Some(4);
        assert_eq!(r.progress_ratio(), Some(0.75));
    }

    #[test]
    fn notification_keeps_legacy_and_event_identity_side_by_side() {
        let mut n = note();
        n.event_type = Some("plugin.sync.completed".to_owned());
        n.dedupe_key = Some("plugin:actor:2026-10-02".to_owned());
        n.resource_type = Some("actor".to_owned());
        n.resource_id = Some(42);
        n.related_task_run_id = Some(7);
        n.related_resource_type = Some("task_run".to_owned());
        n.related_resource_id = Some(7);
        // 注释：事件身份与展示关联分离，旧 related_resource_* 继续服务现有 API。
        assert!(n.uses_event_identity());
        assert!(n.uses_legacy_relation(), "两套字段并存是有意的过渡设计");
        assert!(n.is_deduplicated());
    }

    #[test]
    fn dedupe_key_optional() {
        let mut n = note();
        assert!(!n.is_deduplicated(), "无去重需求的普通通知不受唯一索引影响");
        assert!(!n.uses_event_identity());
        assert!(!n.uses_legacy_relation());
        n.dedupe_key = Some("k".to_owned());
        assert!(n.is_deduplicated());
    }

    #[test]
    fn reports_read_flag_timestamp_mismatch() {
        let mut n = note();
        n.is_read = true;
        assert!(
            n.read_state_inconsistent(),
            "先置标记再补时间是常见两步写法"
        );
        n.read_at = Some(at(2026, 10, 2, 12, 0));
        assert!(!n.read_state_inconsistent());
        n.is_read = false;
        assert!(n.read_state_inconsistent());
    }
}
