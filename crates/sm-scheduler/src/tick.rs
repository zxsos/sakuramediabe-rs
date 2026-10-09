//! 到点入队的 tick 循环。
//!
//! 对应上游 `enqueue_scheduled_job`（`src/start/aps.py:101-119`）——
//! **只入队，不执行**。执行在 worker 侧。
//!
//! # 每次 tick 做两件事
//!
//! 1. [`reclaim_stale`](sm_db::repo::BackgroundTaskRunRepository::reclaim_stale)：
//!    把租约过期的 running 行收回 pending。不做这一步，一个崩溃的 worker
//!    会让任务**永久**卡在 running（state 是 running 就永远不会被 claim 选中）。
//! 2. 对每个到点的任务 `enqueue`，`trigger_type = "scheduled"`。
//!
//! # 冲突是**跳过**，不是错误
//!
//! ```python
//! task_run = TaskQueueService.enqueue(..., conflict="skip")
//! if task_run is None:
//!     logger.info("定时任务已在队列或执行中，本次触发按 coalesce 丢弃 ...")
//! ```
//!
//! 上游把它叫做 coalesce（`job_defaults={"coalesce": True, "max_instances": 1}`）：
//! 上一轮还在跑，这一轮的触发**直接丢掉**，不排队。所以 `enqueue` 撞
//! `UNIQUE(mutex_key)` 时必须降级为 info 日志 + 跳过，返回「本次没有入队」。
//!
//! 这里按**错误码**（`23505`）判定而不是约束名 —— 约束名会随上游 DDL 变动，
//! 绑死名字会让这条幂等路径在某次变更后静默失效。
//!
//! # 单实例语义
//!
//! 上游 `--workers 1`（`docker/backend/supervisord.conf:16`），因为插件是
//! 进程内 Python 包。Rust 侧同理单进程，所以**两个实例同时跑 tick 会重复
//! 入队** —— 而 `mutex_key` 就是那道防线：第二个实例的 INSERT 撞唯一约束，
//! 按上面的规则被跳过。写成「多实例安全」是错的。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sm_db::repo::{BackgroundTaskRunRepository, NewTaskRun};

use crate::cron_spec::{RuntimeTimezone, ScheduledJob};

/// 互斥键前缀。上游 `QUEUE_MUTEX_PREFIX`，注释写明「与 APS 现有互斥命名空间
/// 保持一致」—— 存量库里已经有 `aps:` 开头的行，换前缀会让在跑的任务
/// 与新调度的任务**互相不认**。
pub const QUEUE_MUTEX_PREFIX: &str = "aps:";

/// cron 触发的 `trigger_type`。
pub const TRIGGER_SCHEDULED: &str = "scheduled";
/// 启动引导任务的 `trigger_type`（上游 `_schedule_bootstrap_job`）。
pub const TRIGGER_STARTUP: &str = "startup";

/// 一次 tick 的结果，供测试与诊断断言。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// 本次真正入队的任务键。
    pub enqueued: Vec<&'static str>,
    /// 到点但因互斥键被占用而跳过的任务键。
    pub skipped: Vec<&'static str>,
    /// 本次触发时 `enqueue` 失败（不是约束冲突）的任务键。
    pub failed: Vec<&'static str>,
    /// 回收的僵尸任务行数。
    pub reclaimed: u64,
}

impl TickReport {
    /// 本次是否没有任何数据库写入。
    pub fn is_noop(&self) -> bool {
        self.enqueued.is_empty() && self.skipped.is_empty() && self.failed.is_empty()
    }
}

/// 调度器状态：已编译的任务 + 各自的下一次触发时刻。
///
/// 时刻存在内存里而不是库里，所以**重启后所有任务都要等下一个 cron 点**
/// 才入队一次。上游 APS 同样不持久化下次触发时刻（`replace_existing=True`
/// 下 APS 会从当前时间重新算），所以这不是退化。
#[derive(Debug)]
pub struct Scheduler {
    repo: BackgroundTaskRunRepository,
    jobs: Vec<ScheduledJob>,
    timezone: RuntimeTimezone,
    /// 每个任务的下一次触发时刻（UTC）。`Mutex` 而不是 `RwLock`：tick 是
    /// 单线程循环，读写都在同一个任务里，锁只用于让 `shutdown` 能读到。
    next_fire: Mutex<HashMap<&'static str, DateTime<Utc>>>,
    /// 每轮的间隔。1 秒是上游 APS 的粒度 —— `download_task_sync` 是
    /// `* * * * *`，粒度粗了会系统性迟到。
    interval: Duration,
}

impl Scheduler {
    /// 编译注册表并算出每个任务的下一次触发时刻。
    pub fn new(
        repo: BackgroundTaskRunRepository,
        specs: impl IntoIterator<Item = crate::cron_spec::JobSpec>,
    ) -> Result<Self, crate::cron_spec::ScheduleError> {
        Self::with_timezone(
            repo,
            specs,
            RuntimeTimezone::from_env(),
            Duration::from_secs(1),
        )
    }

    /// 指定时区与 tick 间隔。测试用它把时区与时钟固定下来。
    pub fn with_timezone(
        repo: BackgroundTaskRunRepository,
        specs: impl IntoIterator<Item = crate::cron_spec::JobSpec>,
        timezone: RuntimeTimezone,
        interval: Duration,
    ) -> Result<Self, crate::cron_spec::ScheduleError> {
        let mut jobs = Vec::new();
        let now = Utc::now();
        let mut next_fire = HashMap::new();
        for spec in specs {
            // 非法 cron 在这里就失败：带着一个每分钟 panic 的后台任务跑服务，
            // 比启动失败更难查。
            if let Some(job) = ScheduledJob::compile(spec)? {
                if let Some(fire) = job.next_fire_after(now, &timezone) {
                    next_fire.insert(job.spec().task_key, fire);
                }
                // `next_fire_after` 返回 None 的任务（如「2 月 30 日」）不登记，
                // 于是它永远不到点、也永远不会入队 —— 而不是每 tick 空转。
                jobs.push(job);
            }
        }
        Ok(Self {
            repo,
            jobs,
            timezone,
            next_fire: Mutex::new(next_fire),
            interval,
        })
    }

    /// 时区名，仅用于启动日志。
    pub fn timezone_name(&self) -> &'static str {
        self.timezone.display_name()
    }

    /// tick 间隔。
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// 已注册的任务键。
    pub fn task_keys(&self) -> Vec<&'static str> {
        self.jobs.iter().map(|j| j.spec().task_key).collect()
    }

    /// 某个任务的下一次触发时刻（UTC）。
    ///
    /// 存在的理由是**启动日志**与诊断：上游 `aps()` 启动时把
    /// `key=cron` 全部打进日志（`cron_info`），运维据此确认定时任务配对了。
    /// `None` 表示该任务没有下一次（`manual_only` 或表达式永不匹配）。
    pub fn next_fire_at(&self, task_key: &str) -> Option<DateTime<Utc>> {
        self.next_fire.lock().ok()?.get(task_key).copied()
    }

    /// 启动日志用的「任务 = cron」摘要，格式对齐上游 `cron_info`。
    pub fn cron_summary(&self) -> Vec<(&'static str, &'static str)> {
        self.jobs
            .iter()
            .filter_map(|j| j.spec().cron.map(|cron| (j.spec().task_key, cron)))
            .collect()
    }

    /// 跑一次 tick。
    ///
    /// 单独暴露是为了让测试能用固定时刻驱动，不依赖真实时钟。
    pub async fn tick_once(&self, now: DateTime<Utc>) -> TickReport {
        let mut report = TickReport::default();

        // 僵尸回收先于入队：否则一个刚被回收的任务会在同一轮里既被重排
        // 又被重新入队，顺序上说不通。
        match self.repo.reclaim_stale(now.naive_utc()).await {
            Ok(reclaimed) => report.reclaimed = reclaimed,
            Err(err) => {
                // 回收失败不该让整轮调度停摆 —— 它只是让僵尸多留一轮。
                tracing::warn!(error = %err, "回收僵尸任务失败，本轮继续");
            }
        }

        let due = self.take_due(now);
        for (task_key, job) in due {
            match self.enqueue_scheduled(&job).await {
                Ok(true) => report.enqueued.push(task_key),
                Ok(false) => report.skipped.push(task_key),
                Err(err) => {
                    tracing::error!(task_key, error = %err, "定时任务入队失败");
                    report.failed.push(task_key);
                }
            }
        }
        report
    }

    /// 到点的任务，并把它们的下一次触发时刻推进到 `now` 之后。
    fn take_due(&self, now: DateTime<Utc>) -> Vec<(&'static str, ScheduledJob)> {
        let Ok(mut next_fire) = self.next_fire.lock() else {
            // 上一轮 panic 时 mutex 被毒化。这里返回「没有到点任务」而不是
            // 传播 panic：调度循环不该被一次 panic 带走。
            tracing::error!("调度状态锁已毒化，本轮不做入队");
            return Vec::new();
        };
        let mut due = Vec::new();
        for job in &self.jobs {
            let key = job.spec().task_key;
            let Some(fire) = next_fire.get(key).copied() else {
                continue;
            };
            if fire > now {
                continue;
            }
            // 推进到「now 之后的下一次」而不是「上次 fire 之后的下一次」——
            // 后者会让停机 1 小时的任务每分钟入队一次（coalesce 的语义是
            // 积压即丢弃，不是补跑）。
            if let Some(next) = job.next_fire_after(now, &self.timezone) {
                next_fire.insert(key, next);
            } else {
                // 「2 月 30 日」这种永不匹配的任务：摘掉登记，不再检查。
                next_fire.remove(key);
                tracing::warn!(task_key = key, "该任务没有下一次触发时间，已停止调度");
            }
            due.push((key, job.clone()));
        }
        due
    }

    /// 入队一次。`Ok(false)` 表示因互斥键被占用而跳过。
    async fn enqueue_scheduled(&self, job: &ScheduledJob) -> Result<bool, sm_db::DbError> {
        let spec = job.spec();
        let new = NewTaskRun {
            task_key: spec.task_key.to_owned(),
            task_name: spec.display_name.to_owned(),
            trigger_type: TRIGGER_SCHEDULED.to_owned(),
            // 互斥键 = `aps:` + task_key。**不能省**：省了就没有 coalesce，
            // 每分钟的两个下载任务会把队列塞满同 key 的 pending 行。
            mutex_key: Some(format!("{QUEUE_MUTEX_PREFIX}{}", spec.task_key)),
            params: None,
            // 立刻可领。`scheduled_at` 记录的是入队时间，上游也是如此
            // （「所有 task_run 都是队列托管行；scheduled_at 记录进入队列的时间」）。
            scheduled_at: None,
        };
        match self.repo.enqueue(&new).await {
            Ok(_) => Ok(true),
            Err(err) if is_mutex_conflict(&err) => {
                tracing::info!(
                    task_key = spec.task_key,
                    "定时任务已在队列或执行中，本次触发按 coalesce 丢弃"
                );
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }
}

/// 是不是互斥键冲突（`UNIQUE` 违例）。
///
/// 判定走 [`sm_db::DbError::is_unique_violation`]，即**只认 SQLSTATE
/// `23505`**。这里刻意不绑 `background_task_run_mutex_key_uniq` 这个约束名：
/// 约束名会随上游 DDL 调整，绑死它会让这条 coalesce 路径在某次变更后
/// 静默失效 —— 症状是「每小时任务在第二次触发时起就一直报错」。
///
/// 也不能只看 `ConstraintViolation` 这个变体：外键与 CHECK 违例同样是它，
/// 把它们当 coalesce 跳过会掩盖真实缺陷。
pub fn is_mutex_conflict(err: &sm_db::DbError) -> bool {
    err.is_unique_violation()
}

/// 反复跑 tick，直到 `stop` 被置起。
///
/// 单实例语义：见模块文档。`stop` 是 `&AtomicBool`，`shutdown` 靠它停下循环
/// 并等当前这轮结束 —— 不打断正在进行的入队。
pub async fn run(scheduler: std::sync::Arc<Scheduler>, stop: &std::sync::atomic::AtomicBool) {
    let interval = scheduler.interval();
    let mut ticker = tokio::time::interval(interval);
    // `MissedTickBehavior::Delay`：跳过错过的 tick 而不是补跑。与 coalesce
    // 语义一致 —— 停机 1 小时不该在恢复后瞬间入队 60 个任务。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!("调度循环收到停止信号");
            return;
        }
        ticker.tick().await;
        let report = scheduler.tick_once(Utc::now()).await;
        if !report.is_noop() || report.reclaimed > 0 {
            tracing::debug!(
                enqueued = report.enqueued.len(),
                skipped = report.skipped.len(),
                failed = report.failed.len(),
                reclaimed = report.reclaimed,
                "调度 tick"
            );
        }
    }
}
