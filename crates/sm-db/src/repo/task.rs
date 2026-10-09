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

use crate::common::page::{Page, PageRequest};
use crate::common::update::UpdateSet;
use crate::error::DbError;
use crate::paged_list;
use crate::repo::Ctx;
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

/// 领取范围（上游的「并发道」lane）。
///
/// 把队列按 `task_key` 切开，让不同 worker 各领一条道。`include` 与
/// `exclude` **同时**给出时以 `include` 为准 —— 上游也是先判
/// `if include_task_keys:` 再判 `exclude`，两者都给等于「只领 include
/// 里的、但排除 exclude 的」，那个组合没有实际意义。
#[derive(Debug, Clone, Default)]
pub struct TaskLanes {
    /// 只领这些 `task_key`。**空 = 不限制**。
    pub include: Vec<String>,
    /// 不领这些 `task_key`。**空 = 不限制**。
    pub exclude: Vec<String>,
}

impl TaskLanes {
    /// 只领 `include` 里的。
    pub fn including(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            include: keys.into_iter().map(Into::into).collect(),
            exclude: Vec::new(),
        }
    }

    /// 领除 `exclude` 外的。
    pub fn excluding(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            include: Vec::new(),
            exclude: keys.into_iter().map(Into::into).collect(),
        }
    }

    /// 两个集合都空 —— 等价于不限制，此时不该发条件。
    pub fn is_unrestricted(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
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
        self.claim_in(lease_duration, None).await
    }

    /// 领取一个待执行任务，**限定在若干 `task_key` 内/外**。
    ///
    /// 对应上游 `claim_next(include_task_keys=..., exclude_task_keys=...)`。
    /// 那个参数叫「并发道」（lane）：把队列按 `task_key` 切成互不干扰的
    /// 若干条，让不同 worker 各领一条道上的任务。
    ///
    /// # 为什么用 `= ANY($n)` / `<> ALL($n)` 而不是拼 `IN (...)`
    ///
    /// 变长列表拼进 SQL 字面量需要在 `format!` 里生成占位符，那正是本仓库
    /// 明确排除的做法。数组绑定让 SQL 保持字面量。
    ///
    /// # `exclude` 用 `<> ALL` 而不是 `NOT IN`
    ///
    /// `NOT IN` 遇到 NULL 会返回 NULL（既非真也非假），而 `task_key` 是
    /// NOT NULL，所以这里其实等价 —— 但 `<> ALL` 的三值逻辑行为与
    /// 「不在集合内」这个意图一致，不依赖列的可空性。
    ///
    /// # 空集合 = 不加限制
    ///
    /// `include` 给空集时**不**加条件（否则会领不到任何东西）；`exclude`
    /// 给空集同理。这与上游 `if include_task_keys:` 的真值判断一致 ——
    /// 上游传空 set 与传 None 行为相同。
    pub async fn claim_in(
        &self,
        lease_duration: chrono::Duration,
        lanes: Option<&TaskLanes>,
    ) -> Result<Option<ClaimedTask>, DbError> {
        let now = crate::common::time::now_utc();
        let lease_expires_at = now + lease_duration;

        let (include, exclude) = lanes
            .map(|l| (l.include.clone(), l.exclude.clone()))
            .unwrap_or_default();

        // 三个分支各一条字面量 SQL，不做拼接 —— 占位符数量随条件变化，
        // 拼出来的东西无法用 bind 表达。
        let row = if !include.is_empty() {
            sqlx::query_as::<_, BackgroundTaskRun>(
                "UPDATE background_task_run \
                 SET state = $1, started_at = $2, lease_expires_at = $3, updated_at = $2 \
                 WHERE id = ( \
                     SELECT id FROM background_task_run \
                     WHERE state = $4 \
                       AND (scheduled_at IS NULL OR scheduled_at <= $2) \
                       AND task_key = ANY($5) \
                     ORDER BY scheduled_at NULLS FIRST, id \
                     FOR UPDATE SKIP LOCKED \
                     LIMIT 1 \
                 ) RETURNING *",
            )
            .bind(task_state::RUNNING)
            .bind(now)
            .bind(lease_expires_at)
            .bind(task_state::PENDING)
            .bind(&include)
            .fetch_optional(&self.pool)
            .await?
        } else if !exclude.is_empty() {
            sqlx::query_as::<_, BackgroundTaskRun>(
                "UPDATE background_task_run \
                 SET state = $1, started_at = $2, lease_expires_at = $3, updated_at = $2 \
                 WHERE id = ( \
                     SELECT id FROM background_task_run \
                     WHERE state = $4 \
                       AND (scheduled_at IS NULL OR scheduled_at <= $2) \
                       AND task_key <> ALL($5) \
                     ORDER BY scheduled_at NULLS FIRST, id \
                     FOR UPDATE SKIP LOCKED \
                     LIMIT 1 \
                 ) RETURNING *",
            )
            .bind(task_state::RUNNING)
            .bind(now)
            .bind(lease_expires_at)
            .bind(task_state::PENDING)
            .bind(&exclude)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, BackgroundTaskRun>(
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
            .await?
        };

        // `lease_expires_at` 取**数据库读回的值**，而不是本地算出的那个。
        //
        // PostgreSQL 的 `timestamp` 是微秒精度，而 `chrono::NaiveDateTime`
        // 是纳秒 —— 写进去时被截断，读回来与本地值相差几百纳秒。
        //
        // worker 用这个值判断「租约是否快到期」，本地值偏大就可能**晚几百
        // 纳秒**才认为超时，续租时机随之偏移。库里那份才是真正生效的
        // 契约，所以直接用它。
        Ok(row.map(|run| {
            let lease_expires_at = run.lease_expires_at.unwrap_or(lease_expires_at);
            ClaimedTask {
                run,
                lease_expires_at,
            }
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

    /// **批量**延长租约，返回真正被续上的行数。
    ///
    /// 对应上游 `TaskQueueService.renew_leases`。与逐个调
    /// [`Self::renew_lease`] 的区别有二，且都重要：
    ///
    /// 1. **不报错。** 单个版在任务已被回收时返回 `Err`，而批量版的语义是
    ///    「有多少续上了」—— worker 一次心跳管一批任务，其中一个被回收不该
    ///    让整批心跳失败。所以逐个调用的写法必须绕开单方法的错误路径。
    /// 2. **一次往返。** worker 的心跳周期是秒级，批次可能有几十个 id。
    ///
    /// `state = 'running'` 条件在 SQL 里，所以已被回收（变回 pending）的行
    /// 天然被排除 —— 这正是「续租不能复活别人的任务」那条规则。
    pub async fn renew_leases(
        &self,
        ids: &[i32],
        lease_duration: chrono::Duration,
    ) -> Result<u64, DbError> {
        // 与 `count_by_playlists` 同理：空输入直接返回，不发查询。
        if ids.is_empty() {
            return Ok(0);
        }
        let now = crate::common::time::now_utc();
        let result = sqlx::query(
            "UPDATE background_task_run \
             SET lease_expires_at = $2, updated_at = $3 \
             WHERE id = ANY($1) AND state = $4",
        )
        .bind(ids)
        .bind(now + lease_duration)
        .bind(now)
        .bind(task_state::RUNNING)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 按 `mutex_key` 查当前持有者。**至多一行。**
    ///
    /// 存在的理由是 `conflict = "raise"` 那条路径要报出**是谁**挡住了：
    /// 上游 `TaskQueueConflictError` 带 `blocking_task_run_id`，客户端据此
    /// 提示「这个任务正在执行中」。
    ///
    /// # 为什么可能查不到
    ///
    /// 调用方是「INSERT 撞了唯一约束 → 再查一次」。这两步之间**没有**事务
    /// 包裹，所以持有者可能刚好结束并释放了 `mutex_key`（`finish` / `fail`
    /// 都会把它置空）。那时返回 `None` 是**正确**的，含义是「冲突已消失」，
    /// 上游同样会传 `blocking=None`。
    pub async fn find_by_mutex_key(
        &self,
        mutex_key: &str,
    ) -> Result<Option<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE mutex_key = $1 LIMIT 1",
        )
        .bind(mutex_key.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 该 `task_key` 在队 / 在跑的**那一行**（状态页判「重建中」用）。
    ///
    /// 上游 `StatusService._is_image_search_rebuilding`（`status_service.py:592-602`）：
    /// `WHERE task_key = ? AND state IN ('pending','running')` 取 `.first()`，再看
    /// 那一行的 `params.reset` 是不是 `true`。
    ///
    /// **只取一行、且不加 `ORDER BY`，是照抄上游** —— 多个活跃行时取哪一行由
    /// 数据库决定。刻意不「改进」成 `EXISTS(… AND params->>'reset' = 'true')`：
    /// 那会在多行情形下给出与上游不同的答案，而本接口是排查用的诊断页，
    /// 与上游一致比「更聪明」重要。何况同一个 `task_key` 同时有 pending 与
    /// running 本身就说明并发控制出了问题，这时纠结取哪一行没有意义。
    pub async fn find_active_by_task_key(
        &self,
        task_key: &str,
    ) -> Result<Option<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE task_key = $1 AND state = ANY($2) LIMIT 1",
        )
        .bind(task_key.trim())
        .bind(task_state::ACTIVE)
        .fetch_optional(&self.pool)
        .await?)
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
        // 三件套要么都有效，要么**全部**清空 —— 包括 `text`。
        //
        // 此前只清 `current` / `total`，`text` 原样保留。那会留下一行
        // `progress_current = NULL, progress_total = NULL,
        // progress_text = '正在处理第 3 层'`：文本描述了一个不存在的
        // 量化进度，读出来自相矛盾。文本是进度的**说明**，没有数值进度
        // 时它没有意义。
        let quantifiable =
            matches!((progress.current, progress.total), (Some(_), Some(t)) if t > 0);
        let (current, total, text) = if quantifiable {
            (progress.current, progress.total, progress.text.as_deref())
        } else {
            (None, None, None)
        };
        let text = text.map(str::trim).filter(|s| !s.is_empty());
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET progress_current = $2, progress_total = $3, progress_text = $4, updated_at = $5 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(current)
        .bind(total)
        .bind(text)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
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
        .bind(task_state::COMPLETED)
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
    ///
    /// 不写 `result_summary`：需要带结构化失败原因时用
    /// [`Self::fail_with_summary`]。
    pub async fn fail(&self, id: i32, error: &str) -> Result<BackgroundTaskRun, DbError> {
        self.fail_with_summary(id, error, None).await
    }

    /// 标记失败，并写入 `result_summary`。
    ///
    /// # 为什么需要单独一个方法
    ///
    /// 上游把失败原因**分类**写进 `result_summary`，最常见的是租约回收：
    ///
    /// ```python
    /// TaskRunService.fail_task_run(
    ///     task_run.id,
    ///     error_message=LEASE_EXPIRED_ERROR_MESSAGE,
    ///     result_summary={INTERNAL_FAILURE_CODE_KEY: FAILURE_CODE_QUEUE_LEASE_EXPIRED},
    /// )
    /// ```
    ///
    /// 那个 `_failure_code` 是调用方**区分失败原因**的唯一依据 —— 用户手动
    /// 取消、租约过期、执行进程重启，三者的 `error_message` 都是一句话，
    /// 只有这个码能告诉客户端「要不要提示重试」。
    ///
    /// `summary` 为 `None` 时写 `'{}'`（该列的 DEFAULT），而不是写 NULL ——
    /// 它是 `JsonTextField NOT NULL DEFAULT '{}}'`，写 NULL 会违反约束。
    pub async fn fail_with_summary(
        &self,
        id: i32,
        error: &str,
        summary: Option<&serde_json::Value>,
    ) -> Result<BackgroundTaskRun, DbError> {
        let now = crate::common::time::now_utc();
        let summary = summary.map_or_else(|| "{}".to_owned(), ToString::to_string);
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $2, error_message = $3, result_summary = $4, finished_at = $5, \
                 lease_expires_at = NULL, mutex_key = NULL, updated_at = $5 \
             WHERE id = $1 AND state = $6 RETURNING *",
        )
        .bind(id)
        .bind(task_state::FAILED)
        .bind(error)
        .bind(summary)
        .bind(now)
        .bind(task_state::RUNNING)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            DbError::business(ENTITY, format!("任务 {id} 不在 running 状态，不能置为失败"))
        })?;
        Ok(row)
    }

    // ---------------------------------------------------------------- 终态 CAS
    //
    // 下面四个方法与上面 `finish` / `fail` / `report_progress` **语义不同**，
    // 不是重复实现。区别有两条，都是上游 `activity/task_runs.py` 与
    // `task_queue_service.py` 的真实差异：
    //
    // | | `finish` / `fail`（队列路径） | 本组（服务层终态） |
    // |---|---|---|
    // | 合法来源状态 | 仅 `running` | `pending` + `running`（`task_state::ACTIVE`） |
    // | `result_summary` | **覆盖** | **合并**（`merge_summary`） |
    // | `result_text` | 调用方给什么就写什么 | 缺省时由 summary 格式化兜底 |
    // | 输掉竞争 | 报 `business` 错误 | 返回 `(当前行, false)` |
    //
    // 「输掉竞争」那条是 worker 正确性的前提：两个执行器可能同时收口同一行，
    // 唯一赢家由行锁裁决，输的一方**必须读到持久终态并服从它**，而不是
    // 抛错 —— 上游 `task_execution.py` 正是据此决定「本地成功也不能覆盖
    // 已持久化的失败终态」。

    /// 合并 `result_summary` 并序列化成该列的 TEXT 形态。
    ///
    /// 合并规则见 [`crate::system::activity::result_summary::merge`]，
    /// 公开成仓储方法只为少写一次 `to_column_text`。
    fn merge_summary_column(base: Option<&str>, patch: Option<&serde_json::Value>) -> String {
        let base_value = crate::system::activity::result_summary::from_column_text(base);
        let merged = crate::system::activity::result_summary::merge(Some(&base_value), patch);
        crate::system::activity::result_summary::to_column_text(&merged)
    }

    /// 仅执行 `pending -> running`；其它状态**原样返回该行**且不产生任何写入。
    ///
    /// 对应上游 `TaskRunService.mark_task_run_running`
    /// （`task_runs.py:166-177`）。`started_at` 仅在为空时补，重复调用不改写。
    ///
    /// 返回 `None` **只表示行不存在**。已是 `running` 或终态的行返回它自己 ——
    /// 上游是先 `SELECT ... FOR UPDATE` 再判状态，非 pending 就原样返回。
    /// 用 `WHERE state = 'pending'` 一把梭虽然少一次往返，却把「非 pending」
    /// 折叠成 `None`，调用方分不清「行没了」和「行已是终态」，而这两种情况的
    /// 处置完全不同。
    pub async fn mark_running(&self, id: i32) -> Result<Option<BackgroundTaskRun>, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };

        if current.state != task_state::PENDING {
            tx.rollback().await?;
            return Ok(Some(current));
        }

        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $2, started_at = COALESCE(started_at, $3), updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(task_state::RUNNING)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(row))
    }

    /// 收口为 `completed`。CAS 范围是 [`task_state::ACTIVE`]。
    ///
    /// `summary` 与行内已有摘要**合并**；`text` 为空时由合并后的摘要
    /// 格式化兜底（上游 `format_result_text`）。**同时释放 `mutex_key` 与
    /// 租约** —— 理由同 [`Self::finish`]。
    ///
    /// 返回值三态：
    ///
    /// | 返回 | 含义 |
    /// |---|---|
    /// | `None` | 行不存在 |
    /// | `Some((row, true))` | **本调用赢得了转移** |
    /// | `Some((row, false))` | 行已是终态，`row` 是锁内读到的既有状态 |
    ///
    /// 那个 bool 不能从 `row` 的字段推断出来 —— 一个「本来就 completed」的
    /// 行和一个「刚被我收成 completed」的行，字段完全一样。必须由 CAS 的
    /// 落败/成功直接给出。
    pub async fn complete_active(
        &self,
        id: i32,
        summary: Option<&serde_json::Value>,
        text: Option<&str>,
    ) -> Result<Option<(BackgroundTaskRun, bool)>, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };

        if !task_state::is_active(&current.state) {
            tx.rollback().await?;
            return Ok(Some((current, false)));
        }

        let merged = Self::merge_summary_column(current.result_summary.as_deref(), summary);
        // 上游是 `result_text or format_result_text(result_summary)`：两者都
        // 没有值时写 NULL 而不是空串。`text` 列存不透明文本，空串与 NULL 在
        // 客户端渲染上不等价（前者会渲染出一个空的文本节点）。
        let text: Option<String> = text
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                crate::system::activity::result_summary::format_text(Some(merged.as_str()))
            });
        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $2, result_summary = $3, result_text = $4, error_message = NULL, \
                 finished_at = $5, lease_expires_at = NULL, mutex_key = NULL, updated_at = $5 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(task_state::COMPLETED)
        .bind(merged)
        .bind(text.as_deref())
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some((row, true)))
    }

    /// 收口为 `failed`。CAS 范围与摘要合并规则同 [`Self::complete_active`]。
    pub async fn fail_active(
        &self,
        id: i32,
        error: &str,
        summary: Option<&serde_json::Value>,
    ) -> Result<Option<(BackgroundTaskRun, bool)>, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };

        if !task_state::is_active(&current.state) {
            tx.rollback().await?;
            return Ok(Some((current, false)));
        }

        let merged = Self::merge_summary_column(current.result_summary.as_deref(), summary);
        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET state = $2, error_message = $3, result_summary = $4, finished_at = $5, \
                 lease_expires_at = NULL, mutex_key = NULL, updated_at = $5 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(task_state::FAILED)
        .bind(error)
        .bind(merged)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some((row, true)))
    }

    /// 更新进行中任务的**显式**进度字段。终态行原样返回且零写入。
    ///
    /// 与 [`Self::report_progress`] 的区别是**逐字段可选**：这里只为传入的
    /// 字段写值（`COALESCE` 保住未传入的），而 `report_progress` 的三件套
    /// 要么全有效要么全清空。上游 `update_task_run_progress`
    /// （`task_runs.py:180`）是前者。
    ///
    /// 摘要补丁在行锁内合并 —— 否则两个并发 reporter 会互相覆盖键位。
    pub async fn report_progress_active(
        &self,
        id: i32,
        progress: &TaskProgress,
        summary_patch: Option<&serde_json::Value>,
    ) -> Result<Option<BackgroundTaskRun>, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };

        if !task_state::is_active(&current.state) {
            tx.rollback().await?;
            return Ok(Some(current));
        }

        let merged = Self::merge_summary_column(current.result_summary.as_deref(), summary_patch);
        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, BackgroundTaskRun>(
            "UPDATE background_task_run \
             SET progress_current = COALESCE($2, progress_current), \
                 progress_total = COALESCE($3, progress_total), \
                 progress_text = COALESCE($4, progress_text), \
                 result_summary = $5, \
                 updated_at = $6 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(progress.current)
        .bind(progress.total)
        .bind(progress.text.as_deref())
        .bind(merged)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(row))
    }

    /// 只合并 `result_summary`，**不看 state、不碰 progress**。返回是否命中行。
    ///
    /// # 为什么不能复用 `report_progress*`
    ///
    /// `report_progress_active` 的语义是「任务在跑」：state 不是活动态就**直接
    /// 返回、不写库**。而这里要写的是**已到终态**的任务 —— 失败项重试
    /// （`ImportTaskService::enqueue_failed_item_retry`）给刚入队的重试回写
    /// `state=queued` / `retry_task_run_id` 时，原任务早已 `completed`/`failed`。
    /// 用它会把回写静默丢掉，而客户端已经拿到了 202。
    ///
    /// # 合并是**顶层键覆盖**
    ///
    /// 见 [`crate::system::activity::result_summary::merge`]：patch 里的
    /// `failed_files` 是数组，**整段替换**（不是逐元素合并）—— 正是上游
    /// `_replace_failure_item` 的语义（它也是整段写回）。
    ///
    /// 行不存在返回 `false`（不报错）：调用方要据此**撤销刚入队的任务**并报
    /// 404，而不是让一个查不到的任务行把 500 抛给用户。
    pub async fn merge_result_summary(
        &self,
        id: i32,
        patch: Option<&serde_json::Value>,
    ) -> Result<bool, DbError> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(false);
        };

        let merged = Self::merge_summary_column(current.result_summary.as_deref(), patch);
        sqlx::query(
            "UPDATE background_task_run SET result_summary = $2, updated_at = $3 WHERE id = $1",
        )
        .bind(id)
        .bind(merged)
        .bind(crate::common::time::now_utc())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 每个 `task_key` 各自**最新一条**运行记录，返回 `task_key -> 行`。
    ///
    /// 对应上游 `_latest_task_run_by_key`（`api/routers/system/jobs.py:21-32`）。
    ///
    /// # 为什么用子查询而不是「全部拉回来在内存里去重」
    ///
    /// `background_task_run` 是**只增表** —— 一个每小时跑一次的任务跑一年就是
    /// 8000 行。全表拉回进程只为取 21 个最大值，代价随时间线性增长。子查询让
    /// 数据库只返回「任务个数」那么多行。
    ///
    /// # 「最新」取 `MAX(id)` 而不是 `MAX(started_at)`
    ///
    /// `id` 自增，最大即最新。用 `started_at` 有两个坑：`pending` 行的
    /// `started_at` 是 NULL（排序时被丢到一边），而两行同一微秒写入时无法区分。
    /// 上游也是按 `MAX(id)` 分组。
    pub async fn latest_by_task_keys(
        &self,
        task_keys: &[String],
    ) -> Result<std::collections::HashMap<String, BackgroundTaskRun>, DbError> {
        if task_keys.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run WHERE id IN ( \
                 SELECT MAX(id) FROM background_task_run \
                 WHERE task_key = ANY($1) GROUP BY task_key \
             )",
        )
        .bind(task_keys)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.task_key.clone(), row))
            .collect())
    }

    paged_list! {
        /// 任务台账的分页查询，带三个可选筛选与排序。对应上游
        /// `TaskRunService.list_task_runs`（`activity/task_runs.py:347-367`）。
        ///
        /// # 筛选用 `($n IS NULL OR …)` 而不是拼 SQL
        ///
        /// 三个筛选都可选，拼 SQL 就得为 8 种组合各写一条语句，其中一条会长到
        /// 没法读。`IS NULL OR` 让**语句恒定**、完全参数化，代价是索引在
        /// 「不筛选」时用不上 —— 对一个任务中心页面（每次几十行）可接受。
        ///
        /// # 排序用 `CASE` 而不是拼 `ORDER BY`
        ///
        /// 同理：`ORDER BY` 不能绑参数，六种排序拼六条语句的话 `count` 与
        /// `items` 都要复制一遍。`CASE` 后面是**值比较**
        /// （`$4 = 'started_at:desc'`），绑进来的字符串不会进 SQL 文本，
        /// 所以没有注入面。
        ///
        /// 六个 `CASE` 恒有一个命中，其余产出 NULL。未命中的分支不影响结果
        /// （排序键为 NULL 的行彼此等价），末位的 `id` 次级键让同一时刻的
        /// 多次写入顺序稳定 —— 否则翻页时同一条记录可能出现在两页。
        ///
        /// `sort` 的合法值由 service 层校验（`activity::filters` 的白名单），
        /// 这里不校验：传了非法值会退化成「只按 id DESC 排」，那是一个
        /// **稳定但无意义**的顺序，比报错更容易被误认为「排序没生效」。
        pub async fn list_runs(
            &self,
            state: Option<String>,
            trigger_type: Option<String>,
            task_key: Option<String>,
            sort: String,
        ) -> Result<Page<BackgroundTaskRun>, DbError> {
            count = "SELECT COUNT(*) FROM background_task_run \
                     WHERE ($1::text IS NULL OR state = $1) \
                       AND ($2::text IS NULL OR trigger_type = $2) \
                       AND ($3::text IS NULL OR task_key = $3)",
            items = "SELECT * FROM background_task_run \
                     WHERE ($1::text IS NULL OR state = $1) \
                       AND ($2::text IS NULL OR trigger_type = $2) \
                       AND ($3::text IS NULL OR task_key = $3) \
                     ORDER BY \
                       CASE WHEN $4 = 'started_at:desc' THEN started_at END DESC, \
                       CASE WHEN $4 = 'started_at:asc'  THEN started_at END ASC,  \
                       CASE WHEN $4 = 'created_at:desc' THEN created_at END DESC, \
                       CASE WHEN $4 = 'created_at:asc'  THEN created_at END ASC,  \
                       CASE WHEN $4 = 'updated_at:desc' THEN updated_at END DESC, \
                       CASE WHEN $4 = 'updated_at:asc'  THEN updated_at END ASC,  \
                       id DESC \
                     LIMIT $5 OFFSET $6",
        }
    }

    /// 列出**进行中**（`pending` + `running`）的运行记录，新的在前。
    ///
    /// 对应上游 `list_active_task_runs`（`task_runs.py:370-376`）。**刻意不
    /// 分页** —— 上游返回一个完整列表，语义是「现在有什么在跑」而不是
    /// 「翻页看历史上跑过什么」；分页会把进行中的任务挤到第二页，而那正是
    /// 最需要被看到的那批。
    ///
    /// 排序 `started_at DESC, id DESC` 与上游一致。`pending` 行的
    /// `started_at` 是 NULL 会排到最后 —— 它们还没开始，「最新的在前」这个
    /// 语义对它们不成立，上游同样如此。
    pub async fn list_active_runs(&self) -> Result<Vec<BackgroundTaskRun>, DbError> {
        sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE state = ANY($1) ORDER BY started_at DESC, id DESC",
        )
        .bind(task_state::ACTIVE)
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// 列出**全部**租约过期的 `running` 行。**刻意不分页。**
    ///
    /// 与 [`Self::list_stale_leases`] 的区别是**必须一次拿全**：
    /// 后者是给「任务中心页面」看的（分页正确），而回收是 worker 的
    /// housekeeper —— 只收第一页会让剩下的僵尸行永远留在队列里，而且
    /// 每轮都重复收同样那批。
    ///
    /// `ORDER BY id` 让回收顺序确定，便于复现与测试。
    pub async fn list_all_stale_leases(
        &self,
        now: NaiveDateTime,
    ) -> Result<Vec<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE state = $1 AND lease_expires_at IS NOT NULL AND lease_expires_at < $2 \
             ORDER BY id",
        )
        .bind(task_state::RUNNING)
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 列出**全部**「曾经领取、`scheduled_at` 非空」的 `running` 行。
    /// **刻意不分页**，理由同 [`Self::list_all_stale_leases`]。
    ///
    /// 这是「上一个进程遗留的任务」的判定 —— **不看租约**。进程刚启动时
    /// 那一批行的租约还没到期，等它们到期要白等最多 300 秒
    /// （`sm_service::system::task_queue::DEFAULT_LEASE_SECONDS`）。
    ///
    /// `scheduled_at IS NULL` 的行**不**返回：那类行不是队列元素
    /// （`is_claimable` 虽把 NULL 当可领，但它们不由本队列写入），
    /// 判失败会误伤别的写入方。上游同样带这个条件。
    pub async fn list_interrupted(&self) -> Result<Vec<BackgroundTaskRun>, DbError> {
        Ok(sqlx::query_as::<_, BackgroundTaskRun>(
            "SELECT * FROM background_task_run \
             WHERE state = $1 AND scheduled_at IS NOT NULL \
             ORDER BY id",
        )
        .bind(task_state::RUNNING)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 列出可领取的任务（不改变状态）。用于诊断与「队列有多深」的回答。
    ///
    /// **刻意不分页。** 与 [`list_pending_thumbnails`](crate::repo::media::MediaRepository::list_pending_thumbnails)
    /// 同理：这是 worker 循环的队列扫描，语义是「给我 N 条待办」。
    /// 分页会让 worker 反复取第 1 页，而队列持续增长。
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

    paged_list! {
        /// 列出租约已过期的僵尸任务（不改变状态）。**分页。**
        ///
        /// 给人看：运维要判断「有多少任务卡住了」。分页让那个数字准确。
        ///
        /// # 为什么 `state` 内联而不是绑成占位符
        ///
        /// `paged_list!` 按**参数声明顺序**绑定，每个参数恰好绑一次：
        /// 这里只有 `now -> $1`，随后是 limit/offset。而 `state` 是编译期
        /// 常量（`task_state::RUNNING`），不是调用方能影响的值。
        ///
        /// 此前写的是 `state = $1`，可那个 `$1` 绑的是 `now`，于是
        /// PostgreSQL 报：
        ///
        /// ```text
        /// operator does not exist: character varying = timestamp without time zone
        /// ```
        pub async fn list_stale_leases(
            &self,
            now: NaiveDateTime,
        ) -> Result<Page<BackgroundTaskRun>, DbError> {
            count = "SELECT COUNT(*) FROM background_task_run \
                     WHERE state = 'running' AND lease_expires_at IS NOT NULL \
                       AND lease_expires_at < $1",
            items = "SELECT * FROM background_task_run \
                     WHERE state = 'running' AND lease_expires_at IS NOT NULL \
                       AND lease_expires_at < $1 \
                     ORDER BY lease_expires_at, id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 按 `task_key` 列出运行历史。**分页。**
        ///
        /// 索引是 `(task_key, created_at)`，所以按 created_at 倒序可走索引。
        /// 「这个定时任务最近跑了多少次、每次结果如何」是用户可见的查询。
        pub async fn list_by_task_key(
            &self,
            task_key: &str,
        ) -> Result<Page<BackgroundTaskRun>, DbError> {
            count = "SELECT COUNT(*) FROM background_task_run WHERE task_key = $1",
            items = "SELECT * FROM background_task_run \
                     WHERE task_key = $1 ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
        }
    }

    /// 库里出现过的全部 `task_key`。
    ///
    /// 保留期清理按 key 逐个处理，所以需要先枚举。顺序不保证 ——
    /// 调用方不依赖顺序。
    pub async fn distinct_task_keys(&self) -> Result<Vec<String>, DbError> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT task_key FROM background_task_run ORDER BY task_key",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 保留期分界：该 key 的第 `retention` 条**终态**记录（按 `id` 倒序）的 id。
    ///
    /// `None` 表示终态记录不足 `retention` 条，无需清理。
    ///
    /// # `retention` 为 0 的语义
    ///
    /// `OFFSET 0` 返回最新那条终态，于是分界就是它 —— 配合
    /// [`Self::delete_terminal_through_in`] 就等于「终态记录一条不留」。
    /// 上游 `activity_task_run_retention_per_key` 允许配 0，语义一致。
    ///
    /// # 为什么必须带 `state` 过滤
    ///
    /// 保留额只在**终态**记录里算。若不过滤，一条陈旧的 `pending` 会占掉
    /// 保留额，把真正的历史挤掉；而更糟的是分界 id 之后所有 id 更小的行
    /// 都会被删 —— 包括那条 `pending`。上游明确写了这条：
    /// 「pending/running 无论 id 多旧都不能被清理」。
    pub async fn retention_threshold_id(
        &self,
        task_key: &str,
        retention: i64,
    ) -> Result<Option<i32>, DbError> {
        if retention < 0 {
            return Err(DbError::business(ENTITY, "retention 不能为负"));
        }
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT id FROM background_task_run \
             WHERE task_key = $1 AND state IN ($2, $3) \
             ORDER BY id DESC LIMIT 1 OFFSET $4",
        )
        .bind(task_key)
        .bind(task_state::COMPLETED)
        .bind(task_state::FAILED)
        .bind(retention)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 删掉该 key 下 `id <= threshold_id` 的全部终态记录。返回删了几行。
    ///
    /// 事务内变体：调用方需要与「把通知的 `related_task_run_id` 置空」放进
    /// 同一个事务 —— 否则删完之后通知会指向不存在的行。
    pub async fn delete_terminal_through_in(
        &self,
        ctx: &mut Ctx<'_>,
        task_key: &str,
        threshold_id: i32,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM background_task_run \
             WHERE task_key = $1 AND state IN ($2, $3) AND id <= $4",
        )
        .bind(task_key)
        .bind(task_state::COMPLETED)
        .bind(task_state::FAILED)
        .bind(threshold_id)
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 列出将被 [`Self::delete_terminal_through_in`] 删掉的那批 id。
    ///
    /// 单独一个方法是因为调用方需要**先**拿到 id 去解除通知的引用，而删除
    /// 与解除引用必须在同一个事务里。两条语句各写一遍条件是「同一个 WHERE
    /// 写两遍」的经典隐患 —— 改了一处忘了另一处，删掉的行与解除引用的行
    /// 就会错位，而这种错不会报错，只留下悬挂引用。
    pub async fn stale_terminal_ids_in(
        &self,
        ctx: &mut Ctx<'_>,
        task_key: &str,
        threshold_id: i32,
    ) -> Result<Vec<i32>, DbError> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT id FROM background_task_run \
             WHERE task_key = $1 AND state IN ($2, $3) AND id <= $4",
        )
        .bind(task_key)
        .bind(task_state::COMPLETED)
        .bind(task_state::FAILED)
        .bind(threshold_id)
        .fetch_all(ctx.conn().await?.as_conn())
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

        // 两步：先 UPDATE，按 rows_affected 判定命中；再 SELECT 读回。
        //
        // 与 `MovieRepository::update` 同一个理由：`UPDATE ... RETURNING *`
        // 把「0 行命中」与「解码失败」压成同一个 Err，排查时看不出是哪个。
        // 之前这里用的是 `query_as` + `fetch_optional`，结果 0 行命中被报成
        // NotFound —— 而真相是解码失败：那一版给 `updated_at` 绑的是
        // JSONB 值（`Value` 把所有 `set()` 的值都包成了 `Json`），
        // 赋给 timestamp 列时类型不匹配。
        //
        // 拆开后命中判定是一个确定的数字，NotFound 也就能和「真的没这行」
        // 区分开 —— 而这正是把 bug 定位到根因的那一步。
        let query = fields
            .iter()
            .fold(sqlx::query(super::movie::safe_sql(sql)), |q, (_, v)| {
                super::movie::bind_value_exec(q, v)
            });
        let result = query
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        if result.rows_affected() == 0 {
            return Err(DbError::not_found(ENTITY, id));
        }
        // 命中之后再读回。UPDATE 与随后的 SELECT 之间理论上仍有窗口
        // （并发删除），所以这里用 ok_or_else 把「读不到」也归为
        // NotFound —— 调用方看到的都是同一件事：这一行现在不存在。
        match self.find_by_id(id).await? {
            Some(row) => Ok(row),
            None => Err(DbError::not_found(ENTITY, id)),
        }
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
