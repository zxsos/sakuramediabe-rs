//! 后台任务调度：cron 解析 + 到点幂等入队。
//!
//! 替代上游 APScheduler + `TaskWorker` 的**调度那一半**（`src/start/aps.py`）。
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
//! # 本批**只做入队**，不做执行
//!
//! 上游 `aps()` 起了两样东西：`BlockingScheduler`（cron → 入队）与
//! `TaskWorker`（领取 → 执行）。本仓库的 worker 还没有，而执行逻辑分布在
//! `catalog` / `playback` / `transfers` / `discovery` / `system` 五个域
//! （共 114 个 service 文件），目前只落地 `system` 与 `collections`。
//! [`tick::Scheduler::tick_once`] 因此只写队列，`claim` 留给下一切片。
//!
//! 顺带把 `reclaim_stale` 放在了 tick 里（上游在 worker 循环里做）：
//! 没有 worker 的这段时间里，僵尸行只能靠 tick 回收。
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

pub use cron_spec::{builtin_jobs, JobSpec, RuntimeTimezone, ScheduleError, ScheduledJob};
pub use tick::{TickReport, QUEUE_MUTEX_PREFIX};

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
    pub fn task_keys(&self) -> Vec<&'static str> {
        self.scheduler.task_keys()
    }

    /// 时区名，仅用于启动日志。
    pub fn timezone_name(&self) -> &'static str {
        self.scheduler.timezone_name()
    }

    /// 启动日志用的「任务 = cron」摘要，格式对齐上游 `cron_info`。
    pub fn cron_summary(&self) -> Vec<(&'static str, &'static str)> {
        self.scheduler.cron_summary()
    }

    /// 某个任务的下一次触发时刻（UTC）。诊断用。
    pub fn next_fire_at(&self, task_key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        self.scheduler.next_fire_at(task_key)
    }
}
