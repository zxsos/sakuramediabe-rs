//! 持久任务队列内核，对应上游
//! `src/service/system/task_queue_service.py`（263 行）。
//!
//! # 队列的元素就是 `background_task_run` 的 pending 行
//!
//! 上游的模块 docstring 把这一点讲死了，本仓库完全沿用：没有独立的队列表，
//! 没有 broker。四个原语各管一件事：
//!
//! | 原语 | 上游 | 本模块 |
//! |---|---|---|
//! | 入队 | `enqueue(conflict=skip/raise)` | [`TaskQueueService::enqueue`] |
//! | 领取 | `claim_next` | [`TaskQueueService::claim_next`] |
//! | 续租 | `renew_leases` | [`TaskQueueService::renew_leases`] |
//! | 回收过期租约 | `recover_expired_leases` | [`TaskQueueService::recover_expired_leases`] |
//! | 回收中断任务 | `recover_interrupted_runs` | [`TaskQueueService::recover_interrupted_runs`] |
//!
//! # 互斥键是 coalesce 的实现方式，不是附加功能
//!
//! `UNIQUE(mutex_key)` 与 `state` 无关，所以「同 `task_key` 最多一个
//! pending/running 行」是数据库保证的。cron 每分钟触发一次而任务要跑
//! 五分钟时，靠的就是撞唯一约束 → 跳过（上游 APS 的 `coalesce=True` +
//! `max_instances=1`）。省掉 `mutex_key` 会让队列堆满同 key 的行。
//!
//! # 与上游的**三处刻意差异**
//!
//! **① 租约过期的默认处置是「回 pending」而不是「判失败」。**
//! 见 [`TaskQueueService::recover_expired_leases`] 的详细说明 —— 简短版：
//! 租约过期只说明「没人确认完成」，任务可能已部分执行；能不能重试取决于
//! `task_key` 的语义，那是 service 层的判断。`reclaim_stale`（回 pending）
//! 与本方法（判失败）都保留，由调用方按任务性质选。
//!
//! **② `enqueue` 的冲突策略是显式枚举，不是异常。** 上游用
//! `TaskQueueConflictError`（一个 `RuntimeError`）表达 `conflict="raise"`，
//! 那个异常**没有对应的 API 响应** —— 它只被 worker 内部调用。这里用
//! [`EnqueueOutcome`] 枚举返回，理由见该类型文档。
//!
//! **③ `settle_bootstrap_blocker` 未落地。** 那是启动引导任务的冲突收敛，
//! 依赖 `trigger_type = "startup"` 的就绪判定，而那两个任务
//! （`gfriends_filetree_refresh` / `movie_similarity_recompute`）的 service
//! 还没写。硬写只会得到一个没有调用方的函数。
//!
//! # 与 `sm_scheduler` 的分工
//!
//! 调度器是**生产者**（cron 到点就入队），本模块是**队列本体**（互斥、
//! 领取、租约、回收）。互斥键前缀因此住在
//! [`sm_db::system::activity::QUEUE_MUTEX_PREFIX`] —— 那是两个 crate
//! 唯一的公共依赖，而前缀是存储契约（存量库里已有 `aps:` 开头的行）。

use sm_db::repo::{BackgroundTaskRunRepository, ClaimedTask, NewTaskRun, TaskLanes};
use sm_db::system::activity::{build_mutex_key, BackgroundTaskRun};
use sm_db::Db;

use crate::error::ServiceError;

/// 默认租约秒数。上游 `DEFAULT_LEASE_SECONDS = 300`。
///
/// 300 秒的依据是「任务要么在 5 分钟内跑完，要么进程已死」—— 心跳会周期
/// 续租，所以真正的判据是**有没有人在续租**，不是 300 这个数本身。
pub const DEFAULT_LEASE_SECONDS: i64 = 300;

/// 租约过期的失败文案。上游 `LEASE_EXPIRED_ERROR_MESSAGE`，逐字一致 ——
/// 客户端可能按文案做本地化映射。
pub const LEASE_EXPIRED_ERROR_MESSAGE: &str = "任务租约过期，执行进程已中断，任务按失败回收";

/// 执行进程重启的失败文案。对应上游 `recover_interrupted_runs` 里内联的
/// 那句「任务执行进程重启，执行已中断」。
pub const INTERRUPTED_ERROR_MESSAGE: &str = "任务执行进程重启，执行已中断";

/// `result_summary` 里失败码的键。上游 `INTERNAL_FAILURE_CODE_KEY`。
pub const INTERNAL_FAILURE_CODE_KEY: &str = "_failure_code";

/// 租约过期的失败码。上游 `FAILURE_CODE_QUEUE_LEASE_EXPIRED`。
pub const FAILURE_CODE_QUEUE_LEASE_EXPIRED: &str = "queue_lease_expired";

/// 两个内建引导任务的键，上游 `BOOTSTRAP_QUEUE_TASK_KEYS`。
pub const BOOTSTRAP_QUEUE_TASK_KEYS: [&str; 2] =
    ["gfriends_filetree_refresh", "movie_similarity_recompute"];

/// 入队结果。
///
/// # 为什么不用异常表达 `conflict="raise"`
///
/// 上游抛 `TaskQueueConflictError`，而那个异常在 API 层**没有对应响应** ——
/// 它只在 worker 内部被调用（`aps.py` 的手动触发路径）。既然如此，把它建模成
/// 一个正常返回的枚举有三个好处：
///
/// - 「被同键任务挡住」是一个**预期内**的业务结果，不是异常；
/// - 调用方不必 `catch` 一个只在某个分支才需要的类型；
/// - 两种结局都在返回值里，`match` 漏掉一支会编译报错 —— 而 `catch`
///   很容易被漏掉。
///
/// 阻塞方的 id 仍然带出来（上游 `blocking_task_run_id`），供调用方记日志。
///
/// # 刻意不派生 `PartialEq`
///
/// 与 [`sm_db::repo::ClaimedTask`] 同一个理由：内含
/// `BackgroundTaskRun`（一整行数据库记录），而「两个入队结果相等」不是
/// 一个有意义的问题。要断言就断言 `run.id` 或变体本身。
///
/// # 为什么 `Enqueued` 装箱
///
/// `BackgroundTaskRun` 有 30 个字段、336 字节，不装箱的话这个枚举是
/// 344 字节 —— 而 `Skipped` 只有 8 字节。于是每次 `match` 都要搬 344 字节，
/// 而 cron 每秒一轮、入队路径返回值只被看一眼就丢掉。
///
/// 这与 [`crate::error::ServiceError`] 把 `ApiError` 装箱是同一条理由：
/// 代价是一次堆分配，收益是热路径上的移动按小变体计。
#[derive(Debug, Clone)]
pub enum EnqueueOutcome {
    /// 已入队。**装箱** —— 见类型文档。
    Enqueued(Box<BackgroundTaskRun>),
    /// 因互斥键被占用而跳过（`conflict = "skip"`，cron 的 coalesce 语义）。
    Skipped {
        /// 当前持有该互斥键的行 id，便于诊断。
        blocking_task_run_id: Option<i32>,
    },
}

impl EnqueueOutcome {
    /// 是否真的入队了。cron 的 tick 只需要这个布尔。
    pub fn is_enqueued(&self) -> bool {
        matches!(self, Self::Enqueued(_))
    }

    /// 入队的那一行。`Skipped` 返回 `None`。
    pub fn enqueued(&self) -> Option<&BackgroundTaskRun> {
        match self {
            Self::Enqueued(run) => Some(run),
            Self::Skipped { .. } => None,
        }
    }
}

/// 任务队列 service。
///
/// 持有仓储而不是用类方法 —— 依赖必须显式持有，见
/// [`crate::collections::playlist::PlaylistService`] 的同一条理由。
#[derive(Debug, Clone)]
pub struct TaskQueueService {
    runs: BackgroundTaskRunRepository,
}

impl TaskQueueService {
    pub fn new(db: &Db) -> Self {
        Self {
            runs: BackgroundTaskRunRepository::new(db.clone()),
        }
    }

    /// 由 `task_key` 构造互斥键。
    ///
    /// 直接转发 [`sm_db::system::activity::build_mutex_key`]，让前缀只有
    /// 一个真相源。
    pub fn mutex_key(task_key: &str) -> String {
        build_mutex_key(task_key)
    }

    /// 入队一次执行。
    ///
    /// `conflict` 决定撞上唯一约束时的行为：
    ///
    /// - [`ConflictPolicy::Skip`] —— 返回 [`EnqueueOutcome::Skipped`]。
    ///   **cron 必须用这个**：积压即丢弃，不排队。
    /// - [`ConflictPolicy::Raise`] —— 返回 [`EnqueueOutcome::Skipped`] 且
    ///   `blocking_task_run_id` 为 `Some`。手动触发用这个，好让调用方
    ///   告诉用户「这个任务正在跑」。
    ///
    /// # 为什么两种策略都返回 `Skipped` 而不是让调用方区分
    ///
    /// 见 [`EnqueueOutcome`] 的文档。语义差别体现在**是否检查**
    /// `blocking_task_run_id`：scheduled 路径不关心是谁挡着，manual 路径关心。
    ///
    /// # 只有唯一违例才降级
    ///
    /// 其它错误（外键被拒、CHECK 不通过、连接失败）必须原样冒泡。把它们
    /// 当成「跳过」会让一个真实缺陷表现为「任务偶尔不跑」。
    pub async fn enqueue(
        &self,
        task_key: &str,
        trigger_type: &str,
        task_name: Option<&str>,
        params: Option<serde_json::Value>,
        conflict: ConflictPolicy,
    ) -> Result<EnqueueOutcome, ServiceError> {
        let task_key = task_key.trim();
        if task_key.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "task_key cannot be empty",
            ));
        }
        // `task_name` 缺省时回落到 `task_key`：仓储层要求非空，而
        // 任务中心展示的 `cli_help` 本来就常常等于 key。
        let new = NewTaskRun {
            task_key: task_key.to_owned(),
            task_name: task_name.unwrap_or(task_key).trim().to_owned(),
            trigger_type: trigger_type.trim().to_owned(),
            mutex_key: Some(Self::mutex_key(task_key)),
            params,
            // **`Some(now)` 而不是 `None`** —— 上游的模块 docstring 写明
            // 「所有 task_run 都是队列托管行；scheduled_at 记录进入队列的
            // 时间」。这不是装饰：
            //
            // `recover_interrupted_runs` 判定「上一个进程遗留的任务」靠的
            // 就是 `scheduled_at IS NOT NULL`（与上游同条件）。写 NULL 会让
            // 本模块入队的每一行都被那个过滤排除掉 —— 于是
            // `recover_interrupted_runs` 对自己的任务**永远回收不到**，
            // 崩溃后只能等租约到期（最多 300 秒）才动。
            //
            // `claim` 把 `scheduled_at IS NULL` 也当可领，所以填 now 不影响
            // 领取：`scheduled_at <= now` 在同一刻成立。
            scheduled_at: Some(sm_db::common::time::now_utc()),
        };

        match self.runs.enqueue(&new).await {
            Ok(run) => Ok(EnqueueOutcome::Enqueued(Box::new(run))),
            Err(err) if err.is_unique_violation() => {
                // `Skip` 与 `Raise` 的差别**不在返回值**，而在调用方是否
                // 去看 `blocking_task_run_id`。cron 不关心是谁挡着（只看
                // 「没入队」），手动触发要告诉用户「正在跑的是哪一条」。
                // 两种都返回 `Skipped` —— 见 `EnqueueOutcome` 的文档。
                let blocking = match conflict {
                    ConflictPolicy::Skip => None,
                    ConflictPolicy::Raise => {
                        // 冲突可能刚好消失（持有者已结束并释放了 mutex_key），
                        // 所以这里**允许**查不到 —— 那是合法的「阻塞方 id = None」。
                        self.runs
                            .find_by_mutex_key(&build_mutex_key(task_key))
                            .await?
                            .map(|r| r.id)
                    }
                };
                Ok(EnqueueOutcome::Skipped {
                    blocking_task_run_id: blocking,
                })
            }
            Err(err) => Err(err.into()),
        }
    }

    /// 领取最早到期的可领行，置 `running` 并发放租约。
    ///
    /// `lanes` 为 `None`（或两个集合都空）时不限定 `task_key`。
    ///
    /// `None` 表示队列为空 —— **不是出错**。空闲 worker 反复领取是正常的，
    /// 所以这里不返回错误。
    pub async fn claim_next(
        &self,
        lease_seconds: Option<i64>,
        lanes: Option<TaskLanes>,
    ) -> Result<Option<ClaimedTask>, ServiceError> {
        let lease_seconds = lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS);
        let lanes = lanes.filter(|l| !l.is_unrestricted());
        Ok(self
            .runs
            .claim_in(chrono::Duration::seconds(lease_seconds), lanes.as_ref())
            .await?)
    }

    /// 批量续租，返回真正被续上的行数。
    ///
    /// 空批次返回 0 且不发查询。返回值**可能小于**入参长度 —— 已被回收的
    /// 行不会复活，调用方据此判断「有几个任务掉出去了」。
    pub async fn renew_leases(
        &self,
        task_run_ids: &[i32],
        lease_seconds: Option<i64>,
    ) -> Result<u64, ServiceError> {
        let lease_seconds = lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS);
        Ok(self
            .runs
            .renew_leases(task_run_ids, chrono::Duration::seconds(lease_seconds))
            .await?)
    }

    /// 回收租约已过期的 `running` 行，**判为失败**并写入失败码。
    ///
    /// 对应上游 `recover_expired_leases`。
    ///
    /// # 为什么判失败而不是回 pending
    ///
    /// 上游判失败。仓库里另有 [`reclaim_stale`](BackgroundTaskRunRepository::reclaim_stale)
    /// 回 pending —— 两者都保留，因为**这是两种不同的业务选择**：
    ///
    /// | 场景 | 该用哪个 |
    /// |---|---|
    /// | 任务可重入、重跑安全（如元数据抓取） | `reclaim_stale` → pending |
    /// | 任务重跑会造成外部副作用（上传、删除、扣配额） | 本方法 → failed |
    ///
    /// 把两者合成一个会让其中一种任务永远错：重入型任务被判 failed 后要靠
    /// 人工重跑，不可重入型任务被回 pending 后会**反复执行**。
    ///
    /// # `now` 只取一次
    ///
    /// 与上游一致：整个回收过程用同一个时间基准，循环内不再重取。否则一批
    /// 里后面的行会因为循环耗时而「看起来更过期」，行为随机器负载漂移。
    ///
    /// 返回被回收的行。
    pub async fn recover_expired_leases(
        &self,
        error_message: Option<&str>,
    ) -> Result<Vec<BackgroundTaskRun>, ServiceError> {
        let now = sm_db::common::time::now_utc();
        let candidates = self.runs.list_all_stale_leases(now).await?;
        let message = error_message.unwrap_or(LEASE_EXPIRED_ERROR_MESSAGE);
        let summary = serde_json::json!({
            INTERNAL_FAILURE_CODE_KEY: FAILURE_CODE_QUEUE_LEASE_EXPIRED,
        });

        let mut recovered = Vec::new();
        for run in candidates {
            // 列表与 UPDATE 之间没有事务，所以必须**在锁外再判一次**：
            // worker 的心跳可能刚好续上了租约，那一行不该被收。
            if !run.is_stale_lease(now) {
                continue;
            }
            match self
                .runs
                .fail_with_summary(run.id, message, Some(&summary))
                .await
            {
                Ok(failed) => recovered.push(failed),
                // 已经不是 running（被别的回收路径判掉了）—— 跳过，不是错误。
                Err(err) if is_state_conflict(&err) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(recovered)
    }

    /// 回收「上一个执行进程遗留」的 `running` 行 —— 判失败。
    ///
    /// 对应上游 `recover_interrupted_runs`。**必须在 worker 领取循环启动前
    /// 调用一次**：那不是租约到期，而是进程死了。
    ///
    /// # 与 `recover_expired_leases` 的区别
    ///
    /// 这里**不看租约**。`scheduled_at` 非空的 `running` 行一定是别人领过
    /// 的，而本进程刚启动 —— 那些行属于上一个已经死掉的进程，等它的租约到期
    /// 要再等最多 300 秒，白白拖慢启动。
    ///
    /// # `scheduled_at IS NULL` 的行不回收
    ///
    /// 与上游一致：那类行不是队列元素（`is_claimable` 把它们视为可领，但
    /// 它们不是本模块入队产生的），贸然判失败会误伤别的写入方。
    pub async fn recover_interrupted_runs(&self) -> Result<Vec<BackgroundTaskRun>, ServiceError> {
        let interrupted = self.runs.list_interrupted().await?;
        let mut failed = Vec::new();
        for run in interrupted {
            match self.runs.fail(run.id, INTERRUPTED_ERROR_MESSAGE).await {
                Ok(row) => failed.push(row),
                Err(err) if is_state_conflict(&err) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(failed)
    }
}

/// 入队冲突时的策略。
///
/// # 为什么是类型而不是字符串
///
/// 上游是 `conflict: str = "skip"`，非法值抛 `ValueError`。那意味着
/// `"Skip"`（首字母大写）会走到唯一违例之外的路径、报一个完全误导的错误。
/// 枚举让这件事在编译期就不可能。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// 撞上就跳过。**cron 走这条**（coalesce 语义）。
    #[default]
    Skip,
    /// 撞上时把阻塞方的行 id 带出来，供调用方提示用户。手动触发走这条。
    Raise,
}

/// 是不是「状态已不是 running」造成的业务错误。
///
/// 仓储层的 `fail` / `fail_with_summary` 在 `WHERE state = 'running'` 没命中时
/// 返回 `DbError::Business`。那是**并发下的正常结局**（另一条回收路径抢先），
/// 不是缺陷 —— 所以批量回收要跳过而不是冒泡。
fn is_state_conflict(err: &sm_db::DbError) -> bool {
    matches!(err, sm_db::DbError::Business { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_db::system::activity::QUEUE_MUTEX_PREFIX;

    #[test]
    fn the_mutex_key_is_the_aps_prefix_plus_the_key() {
        // 与上游 `f"aps:{task_key}"` 逐字一致 —— 存量库里已有这个前缀的行
        assert_eq!(
            TaskQueueService::mutex_key("download_task_sync"),
            format!("{QUEUE_MUTEX_PREFIX}download_task_sync")
        );
        assert_eq!(TaskQueueService::mutex_key("x"), "aps:x");
    }

    #[test]
    fn the_default_conflict_policy_is_skip() {
        // cron 不显式传参时必须是 skip，否则积压的任务会开始排队
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::Skip);
    }

    #[test]
    fn the_bootstrap_task_keys_match_upstream() {
        assert_eq!(
            BOOTSTRAP_QUEUE_TASK_KEYS,
            ["gfriends_filetree_refresh", "movie_similarity_recompute"]
        );
    }

    #[test]
    fn the_failure_code_goes_under_the_internal_key() {
        // 客户端按这个键取值，改名等于让它读不到
        let summary = serde_json::json!({
            INTERNAL_FAILURE_CODE_KEY: FAILURE_CODE_QUEUE_LEASE_EXPIRED,
        });
        assert_eq!(summary[INTERNAL_FAILURE_CODE_KEY], "queue_lease_expired");
    }

    #[test]
    fn the_default_lease_is_five_minutes() {
        assert_eq!(DEFAULT_LEASE_SECONDS, 300);
    }
}
