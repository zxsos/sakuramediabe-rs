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

/// 任务队列互斥键的命名空间前缀。对应上游
/// `src/service/system/task_queue_service.py:29` 的 `QUEUE_MUTEX_PREFIX`。
///
/// # 为什么住在 `sm-db` 而不是 service / scheduler
///
/// 三处都要它，而分属三个 crate：
///
/// - `sm_scheduler::tick` —— cron 触发时构造互斥键（coalesce 的防线）
/// - `sm_service::system::task_queue` —— 队列本体的入队/冲突判定
/// - `BackgroundTaskRunRepository` —— 唯一索引的语义解释
///
/// `sm-service` 与 `sm-scheduler` **互不依赖**（都只依赖 `sm-db`），所以放在
/// 任何一个里都会让另一侧新增依赖边。而它确实是个**存储契约**：唯一索引
/// `UNIQUE(mutex_key)` 圈定的就是这个命名空间，且存量库里已经有 `aps:`
/// 开头的行 —— 换前缀会让在跑的任务与新调度的任务**互相不认**，且那种
/// 不一致不报错，只表现为「同一个任务被并发跑了两份」。
pub const QUEUE_MUTEX_PREFIX: &str = "aps:";

/// 由 `task_key` 构造互斥键。
///
/// 与上游 `TaskQueueService.build_mutex_key` 逐字一致（`"aps:" + task_key`）。
#[must_use]
pub fn build_mutex_key(task_key: &str) -> String {
    format!("{QUEUE_MUTEX_PREFIX}{task_key}")
}

/// `background_task_run.state` 取值。
///
/// 上游有**显式白名单**（`src/service/system/activity/task_runs.py:21`）：
///
/// ```python
/// ALLOWED_TASK_STATES = {"pending", "running", "completed", "failed"}
/// ```
///
/// 四个状态就是全部 —— 写进库里的任何其它字面量都是非法的。
///
/// # 「完成」叫 `completed` 而不是 `succeeded`
///
/// 这个字面量是**库契约**：任务中心（Flutter）按它渲染状态。
/// 本仓库此前把成功态写成 `succeeded`，于是
/// [`crate::repo::BackgroundTaskRunRepository::finish`] 写出的行落在白名单
/// 之外，而上游按 `("completed", "failed")` 判终态 —— 结果是**已完成的任务
/// 永远不会被保留期清理**，且客户端读不到「已完成」。
///
/// # 别和 `thumbnail_generation_state` 合并
///
/// [`crate::playback::media::thumbnail_state`] 里那个 `SUCCEEDED` 的字面量
/// **确实是** `"succeeded"`（上游 `THUMBNAIL_STATE_SUCCEEDED`）。两个状态机
/// 里有同名的常量、不同的字面量，看起来像笔误其实不是 —— 合并它们会让
/// 两个表的状态判断同时失效。
pub mod task_state {
    /// 初始态，也是队列元素。
    pub const PENDING: &str = "pending";

    /// 已领取、持有租约。
    pub const RUNNING: &str = "running";

    /// 正常结束。**注意字面量是 `completed`**，见模块文档。
    pub const COMPLETED: &str = "completed";

    /// 异常结束。
    pub const FAILED: &str = "failed";

    /// 白名单里的全部状态。用于校验「这个字面量能不能写进库」。
    pub const ALL: [&str; 4] = [PENDING, RUNNING, COMPLETED, FAILED];

    /// 「进行中」的两个状态。
    ///
    /// 对应上游 `ACTIVE_TASK_RUN_STATES = {"pending", "running"}`
    /// （`activity/task_runs.py:22`）。**终态转移的合法来源集合** ——
    /// 状态转移的 CAS 必须判在这两个值上，而不是只判 `running`。
    ///
    /// # 为什么不能只判 `running`
    ///
    /// worker 领取后本就是 `running`，所以队列路径上看不出差别。但收口
    /// 转移的**合法来源**是两个状态：`pending` 行也可能被直接收口，例如
    /// 尚未领取就被判定「功能停用」而跳过（`activity/worker` 的
    /// `job_disabled_reason` 分支走的就是这条路）。只判 `running` 会让那条
    /// 路径永远转移失败、任务卡在 `pending` 直到租约回收。
    pub const ACTIVE: [&str; 2] = [PENDING, RUNNING];

    /// 是否为进行中状态。
    pub fn is_active(state: &str) -> bool {
        ACTIVE.contains(&state)
    }

    /// 是否为合法状态。
    pub fn is_valid(state: &str) -> bool {
        ALL.contains(&state)
    }

    /// 是否为终态。
    ///
    /// 终态是**保留期清理的判据**（`activity_cleanup_service`）——
    /// `pending` / `running` 无论多旧都不会被删。
    pub fn is_terminal(state: &str) -> bool {
        state == COMPLETED || state == FAILED
    }
}

/// `background_task_run` 表：后台任务台账兼队列。
#[derive(Debug, Clone, FromRow)]
pub struct BackgroundTaskRun {
    pub id: i32,
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
///
/// 对应上游 `ALLOWED_NOTIFICATION_CATEGORIES`
/// （`activity/notifications.py:18`）。白名单是**显式**的：写入前归一化并
/// 校验，非法值报 422 `invalid_activity_filter` 而不是静默落到 `info`。
///
/// 那句「`normalized_category or "info"`」的兜底只在**上游已校验过**的前提下
/// 才安全 —— `create` / `create_once` 都先过 [`crate::repo`] 侧的归一化。
/// 直接往仓储塞一个未知分类不构成受支持的用法。
pub mod notification_category {
    /// 提醒。
    pub const REMINDER: &str = "reminder";
    /// 信息（默认分类）。
    pub const INFO: &str = "info";
    /// 警告。
    pub const WARNING: &str = "warning";
    /// 错误。
    pub const ERROR: &str = "error";

    /// 白名单里的全部分类。
    pub const ALL: [&str; 4] = [REMINDER, INFO, WARNING, ERROR];

    /// 是否为合法分类。
    pub fn is_valid(category: &str) -> bool {
        ALL.contains(&category)
    }

    /// 未指定分类时的兜底值。
    pub const DEFAULT: &str = INFO;
}

/// `result_summary` 列（`JsonTextField`，TEXT 里的 JSON 文本）的展示规则。
///
/// 对应上游 `activity/task_runs.py:51-65`。
pub mod result_summary {
    use serde_json::Value;

    /// 合并两个摘要。`patch` 为空时原样返回 base 的克隆。
    ///
    /// 对应上游 `merge_summary`（`activity/task_runs.py:43-49`）：
    ///
    /// ```python
    /// merged = dict(base_summary); merged.update(summary_patch)
    /// ```
    ///
    /// 键序即插入序（依赖工作区给 `serde_json` 开了 `preserve_order`）——
    /// `format_text` 的输出要能与上游逐字比对。
    ///
    /// base 非法或不是对象时按空对象处理：该列是
    /// `JsonTextField NOT NULL DEFAULT '{}'`，出现别的形状只能是有人绕过
    /// 仓储直接写过，此时整条失败比静默丢键位更糟。
    pub fn merge(base: Option<&Value>, patch: Option<&Value>) -> Value {
        let mut merged = match base {
            Some(Value::Object(map)) => Value::Object(map.clone()),
            _ => Value::Object(serde_json::Map::new()),
        };
        if let (Some(target), Some(Value::Object(source))) = (merged.as_object_mut(), patch) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        merged
    }

    /// 把 `Value` 序列化成该列的 TEXT 形态。非对象按 `{}` 落库。
    pub fn to_column_text(value: &Value) -> String {
        if value.is_object() {
            value.to_string()
        } else {
            "{}".to_owned()
        }
    }

    /// 解析该列的 TEXT。非对象或非法一律按空对象。
    pub fn from_column_text(raw: Option<&str>) -> Value {
        match raw {
            Some(text) if !text.trim().is_empty() => {
                serde_json::from_str::<Value>(text).unwrap_or(Value::Object(serde_json::Map::new()))
            }
            _ => Value::Object(serde_json::Map::new()),
        }
    }

    /// 标量转字符串。**两处方言差异**。
    ///
    /// | 类型 | 本实现 | 上游 `str(value)` |
    /// |---|---|---|
    /// | 字符串 | `x`（裸内容） | `x` |
    /// | 布尔 | `true` / `false` | 同 |
    ///
    /// 字符串这条容易踩：`Value::to_string()` 产出的是 **JSON 字面量**，
    /// 会带上双引号（`"x"`）。直接用它会让 `result_text` 变成
    /// `name="alice"`，而客户端与上游对拍期望的是 `name=alice`。
    fn format_scalar(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            Value::Bool(flag) => {
                if *flag {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                }
            }
            other => other.to_string(),
        }
    }

    /// 把摘要格式化成一行人类可读文本。
    ///
    /// `key=value` 以空格连接，跳过容器（对象 / 数组）与 `null` —— 那些值
    /// 塞进一行文本读不出来。空摘要或全是容器值时返回 `None`。
    ///
    /// # 键序即插入序
    ///
    /// 依赖工作区给 `serde_json` 开了 `preserve_order`（根 `Cargo.toml`），
    /// 于是遍历序与上游 Python `dict` 的插入序一致 —— 输出文本因此可以
    /// 逐字比对。换成 `BTreeMap` 排序会让这条性质消失。
    pub fn format_text(summary_json: Option<&str>) -> Option<String> {
        let raw = summary_json?;
        if raw.trim().is_empty() {
            return None;
        }
        let parsed: Value = serde_json::from_str(raw).ok()?;
        let object = parsed.as_object()?;
        let mut fragments = Vec::new();
        for (key, value) in object {
            match value {
                Value::Object(_) | Value::Array(_) | Value::Null => continue,
                other => fragments.push(format!("{key}={}", format_scalar(other))),
            }
        }
        if fragments.is_empty() {
            None
        } else {
            Some(fragments.join(" "))
        }
    }
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
    pub id: i32,
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
    pub related_task_run_id: Option<i32>,
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
            category: "info".to_owned(),
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
