//! 后台任务调度：cron 解析 + 到点幂等入队 + 队列 worker。
//!
//! 替代上游 APScheduler + `TaskWorker`（`src/start/aps.py` +
//! `src/scheduler/worker.py`）。上游 `aps()` 起了两样东西，本 crate 对应两个
//! 模块：
//!
//! | 模块 | 角色 | 上游 |
//! |---|---|---|
//! | [`tick`] | **生产者**：cron 到点入队，一个进程一份 | `BlockingScheduler` |
//! | [`worker`] | **消费者**：领取 → 执行 → 收口 | `TaskWorker` |
//!
//! # 为什么是 `cron` + 自研 tick，而不是 `tokio-cron-scheduler`
//!
//! ADR §2/§3.1 的决策：后者自带 job store 与执行器，与 `background_task_run`
//! 这个持久队列形成**两套状态源** —— 「任务有没有在跑」在 APS 的内存 job
//! 里问一次、在数据库里问一次，两次答案会不一致。而上游的语义明确是
//! 「cron 只入队，执行在 worker」（`enqueue_scheduled_job` 的 docstring）。
//!
//! 所以这里只解析表达式，触发判定是自己那个 200 行的 tick。
//!
//! # 执行侧依赖 `sm-service`
//!
//! [`worker`] 要为每条任务构造执行体，而执行体落在五个业务域里
//! （`catalog` / `playback` / `transfers` / `discovery` / `system`）。
//! 依赖方向是 `sm-scheduler → sm-service`，**不能反** —— 反了会成环，
//! 因为 `sm-service` 侧的 `task_queue` 要用互斥键前缀，而那属于
//! `sm_db::system::activity`（`sm-db` 是两者共同的依赖）。
//!
//! # 已落地与未落地
//!
//! [`worker::builtin_handlers`] 目前只注册了 `activity_record_cleanup` ——
//! 21 个任务里其余 20 个的 service 还没写（`docs/service-progress.md` 有
//! 逐条阻塞原因）。未注册的任务被领到时会**明确 `failed`** 并写清原因，
//! 而不是静默跳过；理由见 [`worker`] 模块文档。
//!
//! # 启动引导任务（`trigger_type = "startup"`）不在本批
//!
//! 上游对 `gfriends_filetree_refresh` 与 `movie_similarity_recompute` 各加一个
//! 一次性 date job，条件是「缓存缺失或 TTL 过期」/「相似度索引未就绪」——
//! 两个判断都要读别的域的状态。硬写一个固定的引导任务会让它在不需要时也跑，
//! 所以 [`tick::TRIGGER_STARTUP`] 这个常量先留着，等对应域落地。
//!
//! # 启动引导任务（`trigger_type = "startup"`）不在本批
//!
//! 上游对 `gfriends_filetree_refresh` 与 `movie_similarity_recompute` 各加一个
//! 一次性 date job，条件是「缓存缺失或 TTL 过期」/「相似度索引未就绪」——
//! 两个判断都要读别的域的状态。硬写���个固定的引导任务会让它在不需要时也跑，
//! 所以 [`tick::TRIGGER_STARTUP`] 这个常量先留着，等对应域落地。
//!
//! # 单实例
//!
//! 上游 `--workers 1`（插件是进程内 Python 包）。两个实例同时 tick 会重复
//! 入队，`mutex_key`（`aps:` + `task_key`）是唯一防线 —— 而那条路径是
//! 「撞约束即跳过」，见 [`tick`] 模块文档。

pub mod cron_spec;
pub mod tick;
pub mod worker;

pub use cron_spec::{builtin_jobs, JobSpec, RuntimeTimezone, ScheduleError, ScheduledJob};
pub use tick::{TickReport, QUEUE_MUTEX_PREFIX};
pub use worker::{
    builtin_handlers, HandlerRegistry, TaskWorker, TaskWorkerHandle, WorkerConfig, WorkerError,
    LANE_CONCURRENCY, LANE_DEFAULT, LANE_IMPORT, LANE_TRANSFER, NON_DEFAULT_LANE_TASK_KEYS,
};

use std::sync::Arc;

use tokio::task::JoinHandle;

pub use crate::tick::Scheduler;

/// 后台调度句柄。
///
/// 组合根（`sm-server`）拿它 [`spawn`](SchedulerHandle::spawn)，关闭时
/// [`shutdown`](SchedulerHandle::shutdown)。`spawn` 之后句柄本身不再被读，
/// 所以 `shutdown` 走共享的停止标志而不是等 `JoinHandle`。
pub struct SchedulerHandle {
    scheduler: Arc<Scheduler>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SchedulerHandle {
    /// 启动后台 tick 循环。
    pub fn spawn(scheduler: Arc<Scheduler>) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let join = tokio::spawn({
            let scheduler = Arc::clone(&scheduler);
            let stop = Arc::clone(&stop);
            async move {
                tick::run(scheduler, stop.as_ref()).await;
            }
        });
        Self {
            scheduler,
            stop,
            join: Some(join),
        }
    }

    /// 停止调度并等当前这轮结束。
    ///
    /// **等**而不是 abort：正在进行的入队被打断会留下「插入了一半」的
    /// 状态判断难题，而一次 tick 的代价是几条 INSERT，可以等完。
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            // 循环醒来后最多再跑一轮 `tick_once`（一次完整 tick 的时间），
            // 所以这个 join 不会等到下一轮 cron。
            join.await
                .map_err(|err| anyhow::anyhow!("调度循环任务异常结束：{err}"))?;
        }
        Ok(())
    }

    /// 手工跑一次 tick。给「立刻补一次」的场景与测试用。
    pub async fn tick_once(&self) -> TickReport {
        self.scheduler.tick_once(chrono::Utc::now()).await
    }

    /// 已注册的任务键。
    pub fn task_keys(&self) -> Vec<String> {
        self.scheduler.task_keys()
    }

    /// 时区名，仅用于启动日志。
    pub fn timezone_name(&self) -> &'static str {
        self.scheduler.timezone_name()
    }

    /// 启动日志用的「任务 = cron」摘要，格式对齐上游 `cron_info`。
    pub fn cron_summary(&self) -> Vec<(&str, &str)> {
        self.scheduler.cron_summary()
    }

    /// 某个任务的下一次触发时刻（UTC）。诊断用。
    pub fn next_fire_at(&self, task_key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        self.scheduler.next_fire_at(task_key)
    }
}
