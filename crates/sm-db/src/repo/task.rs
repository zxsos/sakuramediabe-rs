//! `background_task_run` 表仓储：后台队列。
//!
//! # 这不是日志表，是队列
//!
//! 表头注释：「pending 行即队列元素；lease_expires_at 过期即可回收」。
//! 所以主查询是「取一个待执行任务」，而不是按时间范围查历史。
//! 配套索引 `(state, scheduled_at)` 正好服务这个查询。
//!
//! # `mutex_key` 是单列唯一索引 —— 一个必须处理的陷阱
//!
//! ```sql
//! CREATE UNIQUE INDEX background_task_run_mutex_key_uniq
//!   ON background_task_run (mutex_key);
//! ```
//!
//! 注意它**不含 `state`**，也不含部分索引条件。所以：
//!
//! | 时刻 | 行状态 | `mutex_key='k'` | 下一个同 key 任务 |
//! |---|---|---|---|
//! | 排队中 | pending | 占用 | 插不进去 ✓ 互斥生效 |
//! | 执行中 | running | 占用 | 插不进去 ✓ |
//! | **已结束** | succeeded | **仍占用** | **永远插不进去** ✗ |
//!
//! 第三行是问题所在。任务跑完后那一行还在库里，`mutex_key` 还留着，
//! 于是**同一个互斥键的下一个任务永久无法创建** —— 而这是最常见的
//! 场景：定时任务每小时跑一次，第二次就会撞唯一约束。
//!
//! 本仓储的处理：[`BackgroundTaskRunRepository::finish`] 在置终态的同一条
//! UPDATE 里把 `mutex_key` 置 `NULL`。利用的是「NULL 不参与唯一约束」
//! 这条 PostgreSQL 规则（模型注释也写了「为 NULL 表示不参与互斥」）。
//!
//! **没有改 schema**，与上游保持同构；而语义变成了本来的意思：
//! 「同一时刻最多一个任务持有该互斥键」。
//!
//! # 领占用 SKIP LOCKED，不靠先查后改
//!
//! 「先 SELECT 找一行、再 UPDATE 它」在多 worker 下会重复领取：
//! 两个 worker 可能读到同一行 pending。用单语句
//! `UPDATE ... WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED) RETURNING *`
//! 让行锁做排他，只有一个 worker 能拿到。

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::common::update::UpdateSet;
use crate::error::DbError;
use crate::system::activity::{task_state, BackgroundTaskRun};

/// 实体名，用于错误分类。
const ENTITY: &str = "BackgroundTaskRun";

/// 排入一个后台任务。
#[derive(Debug, Clone)]
pub struct NewTaskRun {
    /// 处理器定位键。
    pub task_key: String,
    /// 人类可读名称。
    pub task_name: String,
    pub trigger_type: String,
    /// 互斥键。`None` 或空白表示不参与互斥。
    pub mutex_key: Option<String>,
    /// 入参 JSON。**`JsonTextField`** —— 存的是 TEXT 列里的 JSON 文本。
    pub params: Option<serde_json::Value>,
    /// 计划执行时刻。`None` 表示立即可领。
    pub scheduled_at: Option<NaiveDateTime>,
}

impl NewTaskRun {
    /// 归一化并校验。
    ///
    /// 空白 `mutex_key` 归一为 `None`，因为模型已经把「空白」定义为
    /// 不参与互斥（`is_mutex_guarded`），而唯一索引不认这个区分 ——
    /// 空白串照样占用唯一值。
    fn normalize(&self) -> Result<NormalizedTask, DbError> {
        let task_key = self.task_key.trim();
        if task_key.is_empty() {
            return Err(DbError::business(ENTITY, "task_key 不能为空"));
        }
        if self.task_name.trim().is_empty() {
            return Err(DbError::business(ENTITY, "task_name 不能为空"));
        }
        if self.trigger_type.trim().is_empty() {
            return Err(DbError::business(ENTITY, "trigger_type 不能为空"));
        }

        let mutex_key = self
            .mutex_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let params = self.params.as_ref().map(|v| v.to_string());

        Ok(NormalizedTask {
            task_key: task_key.to_owned(),
            task_name: self.task_name.trim().to_owned(),
            trigger_type: self.trigger_type.trim().to_owned(),
            mutex_key,
            params,
            scheduled_at: self.scheduled_at,
        })
    }
}

/// 归一化后的任务，字段都已是可直接落库的形式。
struct NormalizedTask {
    task_key: String,
    task_name: String,
    trigger_type: String,
    mutex_key: Option<String>,
    params: Option<String>,
    scheduled_at: Option<NaiveDateTime>,
}

/// 一次领取的结果。
///
/// `None` 表示队列为空。**不表示出错** —— 空闲 worker 反复领取是正常的。
///
/// 刻意**不**派生 `PartialEq`/`Eq`：它内含 `BackgroundTaskRun`（一整行
/// 数据库记录），而「两个领取结果相等」不是一个有意义的问题。要断言
/// 领取结果就断言 `run.id` 或 `run.state`。
#[derive(Debug, Clone)]
pub struct ClaimedTask {
    pub run: BackgroundTaskRun,
    /// 租约到期时刻。worker 必须在此之前完成或续租。
    pub lease_expires_at: NaiveDateTime,
}

/// 进度上报。
#[derive(Debug, Clone, Default)]
pub struct TaskProgress {
    pub current: Option<i32>,
    pub total: Option<i32>,
    pub text: Option<String>,
}

/// 任务的完成结果。
#[derive(Debug, Clone, Default)]
pub struct TaskOutcome {
    /// `JsonTextField` 结构化摘要。
    pub summary: Option<serde_json::Value>,
    /// 文本结果。
    pub text: Option<String>,
}

/// `background_task_run` 表仓储。
#[derive(Debug, Clone)]
pub struct BackgroundTaskRunRepository {
    pool: PgPool,
}

impl BackgroundTaskRunRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 排入一个任务。
    ///
    /// `state` 不在这里赋值 —— 走数据库 DEFAULT `'pending'`。
    /// `result_summary` 同理走 DEFAULT `'{}'`。
    ///
    /// 唯一约束冲突（同 `mutex_key` 已有行）返回
    /// [`DbError::ConstraintViolation`]（409）—— 调用方应理解为
    /// 「该互斥键正被占用，稍后重试」，而不是重试无益的脏数据。
    pub async fn enqueue(&self, new: &NewTaskRun) -> Result<BackgroundTaskRun, DbError> {
        let n = new.normalize()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, BackgroundTaskRun>(
            "INSERT INTO background_task_run ( \
                     task_key, task_name, trigger_type, mutex_key, params, scheduled_at, \
                     created_at, updated_at \
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $7) RETURNING *",
        )
        .bind(&n.task_key)
        .bind(&n.task_name)
        .bind(&n.trigger_type)
        .bind(&n.mutex_key)
        .bind(&n.params)
        .bind(n.scheduled_at)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 领取一个待执行任务。
    ///
    /// 单语句 `FOR UPDATE SKIP LOCKED` —— 并发 worker 各自拿到不同的行。
    /// `scheduled_at IS NULL` 视为立即可领（与 `is_claimable` 一致）。
    ///
    /// 领取时立刻置 `state = 'running'` 并写 `started_at` 与租约：
    /// 「领取」与「标记执行中」必须在同一条语句里，否则进程在两条语句
    /// 之间崩溃会留下一个 pending 行被反复领取。
    pub async fn claim(
        &self,
        lease_duration: chrono::Duration,
    ) -> Result<Option<ClaimedTask>, DbError> {
        let now = crate::common::time::now_utc();
        let lease_expires_at = now + lease_duration;

        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $1, started_at = $2, lease_expires_at = $3, updated_at = $2 \
             WHERE id = ( \
                 SELECT id FROM background_task_run \
                 WHERE state = $4 \
                   AND (scheduled_at IS NULL OR scheduled_at <= $2) \
                 ORDER BY scheduled_at NULLS FIRST, id \
                 FOR UPDATE SKIP LOCKED \
                 LIMIT 1 \
             ) RETURNING *",
        )
        .bind(task_state::RUNNING)
        .bind(now)
        .bind(lease_expires_at)
        .bind(task_state::PENDING)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|run| ClaimedTask {
            run,
            lease_expires_at,
        }))
    }

    /// 回收租约已过期的僵尸任务。
    ///
    /// 没有这一步，一个崩溃的 worker 会让任务**永久**卡在 running：
    /// 状态是 running 就永远不会被 `claim` 选中。
    ///
    /// 回到 `pending` 而**不是**直接判失败：租约过期只说明「没人确认完成」，
    /// 任务可能已部分执行。是否重试由 service 层按 `task_key` 的语义决定。
    /// 回到 pending 允许它重跑，也让 `attempt` 计数类逻辑留在 service 层。
    pub async fn reclaim_stale(&self, now: NaiveDateTime) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE background_task_run \
             SET state = $1, started_at = NULL, lease_expires_at = NULL, updated_at = $2 \
             WHERE state = $3 AND lease_expires_at IS NOT NULL AND lease_expires_at < $2",
        )
        .bind(task_state::PENDING)
        .bind(now)
        .bind(task_state::RUNNING)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 延长租约。长任务应周期性调用。
    ///
    /// 只对 `running` 且 `mutex_key` 仍归该行有效 —— 若任务已被回收
    /// （变回 pending），续租必须失败，否则会「复活」一个别人正在跑的任务。
    pub async fn renew_lease(
        &self,
        id: i32,
        lease_duration: chrono::Duration,
    ) -> Result<BackgroundTaskRun, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET lease_expires_at = $2, updated_at = $3 \
             WHERE id = $1 AND state = $4 RETURNING *",
        )
        .bind(id)
        .bind(now + lease_duration)
        .bind(now)
        .bind(task_state::RUNNING)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            DbError::business(
                ENTITY,
                format!("任务 {id} 不在 running 状态（可能已被回收），续租失败"),
            )
        })
    }

    /// 上报进度。
    ///
    /// 用通用 `update` 之外的专用方法：进度三件套**要么都不填，
    /// 要么都填**（见模型注释「不可量化的任务不填」），而 `progress_ratio`
    /// 在 `total <= 0` 时返回 `None`。这里让调用方能只写一半时保持沉默，
    /// 由本方法把不完整的组合清空，避免留下 `current=5, total=NULL` 这种
    /// 读出来「无法计算进度」的行。
    pub async fn report_progress(
        &self,
        id: i32,
        progress: &TaskProgress,
    ) -> Result<BackgroundTaskRun, DbError> {
        let (current, total) = match (progress.current, progress.total) {
            (Some(c), Some(t)) if t > 0 => (Some(c), Some(t)),
            _ => (None, None),
        };
        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET progress_current = $2, progress_total = $3, progress_text = $4, updated_at = $5 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(current)
        .bind(total)
        .bind(
            progress
                .text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))?;
        Ok(row)
    }

    /// 标记成功。**同时释放 `mutex_key`。**
    ///
    /// 释放互斥键是必须的，原因见模块文档：唯一索引不含 `state`，
    /// 不释放的话同键的下一个任务永远插不进来。
    pub async fn finish(
        &self,
        id: i32,
        outcome: &TaskOutcome,
    ) -> Result<BackgroundTaskRun, DbError> {
        let now = crate::common::time::now_utc();
        let summary = outcome
            .summary
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_owned());

        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $2, result_summary = $3, result_text = $4, error_message = NULL, \
                 finished_at = $5, lease_expires_at = NULL, mutex_key = NULL, updated_at = $5 \
             WHERE id = $1 AND state = $6 RETURNING *",
        )
        .bind(id)
        .bind(task_state::SUCCEEDED)
        .bind(summary)
        .bind(outcome.text.as_deref())
        .bind(now)
        .bind(task_state::RUNNING)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            DbError::business(ENTITY, format!("任务 {id} 不在 running 状态，不能置为成功"))
        })?;

        debug_assert!(
            !row.is_mutex_guarded(),
            "完成后必须释放互斥键，否则同键任务永久无法创建"
        );
        Ok(row)
    }

    /// 标记失败。**同样释放 `mutex_key`。**
    ///
    /// 失败也要释放 —— 保留一个失败的 key 会让重试永远撞唯一约束，
    /// 而重试正是失败后最该做的事。
    pub async fn fail(&self, id: i32, error: &str) -> Result<BackgroundTaskRun, DbError> {
        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $2, error_message = $3, finished_at = $4, \
                 lease_expires_at = NULL, mutex_key = NULL, updated_at = $4 \
             WHERE id = $1 AND state = $5 RETURNING *",
        )
        .bind(id)
        .bind(task_state::FAILED)
        .bind(error)
        .bind(now)
        .bind(task_state::RUNNING)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            DbError::business(ENTITY, format!("任务 {id} 不在 running 状态，不能置为失败"))
        })?;
        Ok(row)
    }

    /// 列出可领取的任务（不改变状态）。用于诊断与「队列有多深」的回答。
    pub async fn list_claimable(
        &self,
        now: NaiveDateTime,
        limit: i64,
    ) -> Result<Vec<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE state = $1 AND (scheduled_at IS NULL OR scheduled_at <= $2) \
             ORDER BY scheduled_at NULLS FIRST, id LIMIT $3",
        )
        .bind(task_state::PENDING)
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 列出租约已过期的僵尸任务（不改变状态）。
    pub async fn list_stale_leases(
        &self,
        now: NaiveDateTime,
    ) -> Result<Vec<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE state = $1 AND lease_expires_at IS NOT NULL AND lease_expires_at < $2 \
             ORDER BY lease_expires_at, id",
        )
        .bind(task_state::RUNNING)
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按 `task_key` 列出最近若干次运行。
    ///
    /// 索引是 `(task_key, created_at)`，所以按 created_at 倒序可走索引。
    pub async fn list_by_task_key(
        &self,
        task_key: &str,
        limit: i64,
    ) -> Result<Vec<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE task_key = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(task_key.trim())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 通用更新（改 `task_name` / `scheduled_at` 等非状态字段）。
    ///
    /// **不**用于改 `state` —— 状态迁移必须走
    /// [`claim`](BackgroundTaskRunRepository::claim) /
    /// [`finish`](BackgroundTaskRunRepository::finish) /
    /// [`fail`](BackgroundTaskRunRepository::fail) /
    /// [`reclaim_stale`](BackgroundTaskRunRepository::reclaim_stale)，
    /// 它们各自携带必要的时间戳与约束。
    ///
    /// 同理不允许改 `mutex_key`：绕过 `finish` 的释放逻辑会让互斥键
    /// 永久占用。
    pub async fn update_metadata(
        &self,
        id: i32,
        mut set: UpdateSet<'_>,
    ) -> Result<BackgroundTaskRun, DbError> {
        for (name, _) in set.fields() {
            match *name {
                "task_name" | "trigger_type" | "scheduled_at" | "params" => {}
                other => {
                    return Err(DbError::business(
                        ENTITY,
                        format!("`{other}` 不能经 update_metadata 修改"),
                    ))
                }
            }
        }
        set.touch();
        // 字段占位符从 $1 起、id 放最后 —— 与 SET/WHERE 的书写顺序一致。
        let assignments = set.assignments(1);
        let fields = set.finish(ENTITY)?;
        let sql = format!(
            "UPDATE background_task_run SET {assignments} WHERE id = ${}",
            fields.len() + 1
        );

        let query = fields.iter().fold(
            sqlx::query_as::<_, BackgroundTaskRun>(super::movie::safe_sql(sql)),
            |q, (_, v)| super::movie::bind_value(q, v),
        );
        let row = query
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        row.ok_or_else(|| DbError::not_found(ENTITY, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_task(key: &str) -> NewTaskRun {
        NewTaskRun {
            task_key: key.to_owned(),
            task_name: format!("{key} 任务"),
            trigger_type: "manual".to_owned(),
            mutex_key: None,
            params: None,
            scheduled_at: None,
        }
    }

    #[test]
    fn blank_required_fields_are_rejected() {
        let mut t = new_task("k");
        assert!(t.normalize().is_ok());
        t.task_key = "   ".to_owned();
        assert!(t.normalize().is_err(), "空 task_key");
        t = new_task("k");
        t.task_name = " ".to_owned();
        assert!(t.normalize().is_err(), "空 task_name");
        t = new_task("k");
        t.trigger_type = String::new();
        assert!(t.normalize().is_err(), "空 trigger_type");
    }

    #[test]
    fn blank_mutex_key_normalises_to_none() {
        // 空白与 None 在模型里都表示「不参与互斥」，而唯一索引不认这个
        // 区分 —— 空白串照样占用唯一值，所以必须归一掉。
        let mut t = new_task("k");
        t.mutex_key = Some("   ".to_owned());
        assert_eq!(t.normalize().unwrap().mutex_key, None);

        t.mutex_key = Some("  real-key  ".to_owned());
        assert_eq!(
            t.normalize().unwrap().mutex_key.as_deref(),
            Some("real-key"),
            "非空白键应 trim 后保留"
        );
    }

    #[test]
    fn params_are_stored_as_json_text_not_null() {
        // result_summary 的 DEFAULT 是 '{}'，params 则是 JsonTextField：
        // 未提供时写 NULL，不能写 '{}' —— 两者语义不同（未传 vs 传了空对象）。
        let mut t = new_task("k");
        assert_eq!(t.normalize().unwrap().params, None);
        t.params = Some(json!({"a": 1}));
        assert_eq!(
            t.normalize().unwrap().params.as_deref(),
            Some("{\"a\":1}"),
            "序列化文本，而不是 JSON 字符串字面量"
        );
    }

    #[test]
    fn trigger_type_has_no_check_constraint_so_anything_goes() {
        // 模型注释明说「数据库无 CHECK 约束」，字面量由 service 层决定。
        // 仓储层不校验它，否则会在上游扩展触发方式时把写入挡掉。
        let mut t = new_task("k");
        t.trigger_type = "future-kind-2099".to_owned();
        assert_eq!(t.normalize().unwrap().trigger_type, "future-kind-2099");
    }
}
