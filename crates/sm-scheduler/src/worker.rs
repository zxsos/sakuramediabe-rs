//! 任务队列 worker：领取 → 执行 → 收口。
//!
//! 对应上游 `src/scheduler/worker.py`（180 行）。上游那份叫
//! `TaskWorker`，跑在 APS 进程内：**N 条领取线程 + 1 条 housekeeper 线程**。
//! 本模块是同样的结构，用 tokio 任务代替线程。
//!
//! # 与调度器的分工
//!
//! [`crate::tick`] 是**生产者**：cron 到点就入队，一个进程一份。本模块是
//! **消费者**：从队列领取并执行。两者共享 `background_task_run` 这一张表，
//! 但不共享状态 —— 调度器崩了不影响在跑的任务，worker 崩了不影响 cron。
//!
//! 真正的队列语义（互斥、租约、`FOR UPDATE SKIP LOCKED`）在
//! [`sm_service::system::task_queue`]，本模块只做「领哪一条」与「领到之后
//! 怎么办」。
//!
//! # 并发道（lane）
//!
//! 上游 `queue_tasks.py` 把任务分三条道，每道独立并发度：
//!
//! | 道 | 并发 | 归属 |
//! |---|---|---|
//! | `default` | 4（被 `scheduler.worker_default_concurrency` 覆盖） | 其余全部任务 |
//! | `import` | 2 | `library_import` |
//! | `transfer` | 1 | `media_storage_transfer` |
//!
//! **default 道必须排除专属道的 key** —— 否则一个 4 并发的 default 道会把
//! 2 并发的导入任务也抢走 4 份，「导入道限流」就形同虚设。这条规则由
//! [`NON_DEFAULT_LANE_TASK_KEYS`] 表达，与上游同名同义。
//!
//! # 未知任务键**明确失败**，不静默跳过
//!
//! 上游 `_execute` 在注册表里查不到 `task_key` 时抛
//! `JobExecutionError(f"task_key 未在注册表中: …")` 并写 failed，注释写明
//! 「避免无限重领」。本模块照抄：处理器注册表里没有该键就收口为 failed。
//!
//! 另一半原因是**静默跳过更糟** —— 任务被反复领取、反复跳过、永不失败，
//! 队列看起来在动而任务从来没跑过。这类故障在任务中心里表现为「一直转圈」。
//! 失败至少会出现在通知里。
//!
//! # 单进程
//!
//! 上游 `--workers 1`（插件是进程内 Python 包）。两个 worker 同时 tick 会
//! 重复入队，`mutex_key`（`aps:` + `task_key`）是唯一防线。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use sm_db::repo::TaskLanes;
use sm_db::Db;
use sm_service::system::activity::{run_task, TaskHandler, TaskRunError, TaskRunService};
use sm_service::system::activity_cleanup::RetentionPolicy;
use sm_service::system::optional_services::job_disabled_reason;
use sm_service::system::ActivityCleanupService;
use sm_service::system::task_queue::{TaskQueueService, DEFAULT_LEASE_SECONDS};
use sm_service::system::ConfigService;
use tracing::{error, info, warn};

/// 默认道。承载除专属道外的全部任务。
pub const LANE_DEFAULT: &str = "default";
/// 导入道。2 并发。
pub const LANE_IMPORT: &str = "import";
/// 存储迁移道。1 并发。
pub const LANE_TRANSFER: &str = "transfer";

/// 各道的默认并发度。对应上游 `LANE_CONCURRENCY`
/// （`queue_tasks.py:23-27`），其中 `default` 会被配置
/// `scheduler.worker_default_concurrency` 覆盖。
pub const LANE_CONCURRENCY: [(&str, usize); 3] = [
    (LANE_DEFAULT, 4),
    (LANE_IMPORT, 2),
    (LANE_TRANSFER, 1),
];

/// default 道领取时**必须排除**的 `task_key`。
///
/// 对应上游 `NON_DEFAULT_LANE_TASK_KEYS`（`queue_tasks.py:97-101`）：从
/// `QUEUE_TASK_REGISTRY` 里筛出 `lane != default` 的键。
///
/// 那些键由 producer 入队（本仓库的 `task_queue` 是唯一的入队路径），而它们
/// 各自属于专属道 —— 少了这份排除，default 道的 4 个并发会抢走本该限流的
/// 导入任务。
pub const NON_DEFAULT_LANE_TASK_KEYS: [&str; 2] = ["library_import", "media_storage_transfer"];

/// 领取线程的轮询间隔。上游 `CLAIM_POLL_INTERVAL_SECONDS = 1.0`。
pub const CLAIM_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 领取失败时的退避倍数。上游 `self._stop.wait(self._poll_interval * 5)`。
const CLAIM_ERROR_BACKOFF: u32 = 5;

/// housekeeper 的间隔下限（秒）。上游 `max(lease_seconds // 3, 5)` 的那个 5。
const HOUSEKEEPING_FLOOR_SECONDS: u64 = 5;

/// 构建某个任务的执行体。
///
/// 收 `&Value` 形参（持久化的 `params`）是因为**带参任务**要从这里读
/// `params`（上游 `JobDefinition.build_executor`）。无参任务的实现忽略它。
pub type HandlerFactory =
    Box<dyn Fn(&Db, &Value) -> Result<TaskHandler, WorkerError> + Send + Sync>;

/// 领域状态的收口钩子。对应上游 `JobDefinition.business_recovery`。
///
/// 触发时机有三处（上游 `worker.py`）：启动时恢复中断任务、任务崩溃后、
/// 回收过期租约后。**不是**每次失败后 —— 上游只在「本执行体抛了异常」与
/// 「租约被回收」这两种「可能留下半成品」的情形收口。
///
/// 收口本身要查库，所以是异步的 —— 与 [`HandlerFactory`] 同样的理由。
pub type BusinessRecovery = Box<
    dyn Fn(&Db) -> Pin<Box<dyn Future<Output = Result<(), WorkerError>> + Send>> + Send + Sync,
>;

/// worker 侧的错误。
#[derive(Debug)]
pub enum WorkerError {
    /// 注册表里没有这个 `task_key` 的处理器。
    ///
    /// 收口为 `failed` 而非跳过，见模块文档。
    NoHandler(String),
    /// 处理器自己说参数不合法。对应上游 `build_executor` 抛
    /// `JobExecutionError`。
    HandlerParams { task_key: String, reason: String },
    /// 处理器或收口钩子内部的数据库错误。
    Service(sm_service::error::ServiceError),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHandler(key) => write!(f, "task_key 未在处理器注册表中: {key}"),
            Self::HandlerParams { task_key, reason } => {
                write!(f, "任务 {task_key} 的持久参数与声明不匹配: {reason}")
            }
            Self::Service(error) => write!(f, "{}", error.code()),
        }
    }
}

impl From<sm_service::error::ServiceError> for WorkerError {
    fn from(value: sm_service::error::ServiceError) -> Self {
        Self::Service(value)
    }
}

/// 处理器注册表。
///
/// 键是 `task_key`。**没注册的键会让任务失败**（见模块文档），所以这张表
/// 天然就是一份「已落地任务」的清单。
#[derive(Default)]
pub struct HandlerRegistry {
    factories: HashMap<String, HandlerFactory>,
    /// `task_key` → 收口钩子。缺项表示该任务没有领域状态要收。
    recoveries: HashMap<String, BusinessRecovery>,
}

impl HandlerRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个处理器。
    pub fn register(&mut self, task_key: &str, factory: HandlerFactory) -> &mut Self {
        self.factories.insert(task_key.to_owned(), factory);
        self
    }

    /// 注册一个收口钩子。
    pub fn register_recovery(&mut self, task_key: &str, recovery: BusinessRecovery) -> &mut Self {
        self.recoveries.insert(task_key.to_owned(), recovery);
        self
    }

    /// 该键是否已落地。
    pub fn contains(&self, task_key: &str) -> bool {
        self.factories.contains_key(task_key)
    }

    /// 已落地的键，按字典序。
    pub fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.factories.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    /// 该键是否有收口钩子。
    pub fn has_recovery(&self, task_key: &str) -> bool {
        self.recoveries.contains_key(task_key)
    }

    fn build(
        &self,
        db: &Db,
        task_key: &str,
        params: &Value,
    ) -> Result<TaskHandler, WorkerError> {
        self.factories
            .get(task_key)
            .ok_or_else(|| WorkerError::NoHandler(task_key.to_owned()))?(db, params)
    }

    async fn run_recovery(&self, db: &Db, task_key: &str) {
        let Some(recovery) = self.recoveries.get(task_key) else {
            return;
        };
        if let Err(error) = recovery(db).await {
            // 收口钩子失败**不重试**：它要收的是「上一个执行体留下的半成品」，
            // 而那个执行体已经不在了。再跑一次任务不会让它重新收口。
            error!(
                task_key,
                error = %error,
                "领域状态收口失败，残留状态需要人工介入"
            );
        }
    }
}

/// 已落地的内建处理器。
///
/// # 现在只有一个
///
/// 21 个任务里 20 个的 service 还没写（Qdrant / zip / provider 各挡一批，
/// 见 `docs/service-progress.md`）。**不注册就没有处理器**，那些任务被领到
/// 时会明确 `failed` 并写清「未在处理器注册表中」，而不是静默跳过。
///
/// `activity_record_cleanup` 是唯一一个 service 已就位的（`system` 域），
/// 用它把链路端到端跑通。
pub fn builtin_handlers() -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();

    registry.register(
        "activity_record_cleanup",
        Box::new(|db: &Db, _params: &Value| {
            // 写 `Db::clone(db)` 而不是 `db.clone()` —— 后者走 `Clone for &T`
            // 返回 `&Db`，move 进闭包就变成生命周期错误。
            let cleanup_db = Db::clone(db);
            // 显式标注 `TaskHandler`：`Box::new(closure)` 的目标类型推不出来
            // （闭包返回的 async block 也要装箱），`as TaskHandler` 也一样。
            let handler: TaskHandler = Box::new(move |reporter| {
                Box::pin(async move {
                    let policy = RetentionPolicy::default();
                    let service = ActivityCleanupService::new(&cleanup_db);
                    match service.cleanup(policy).await {
                        Ok(stats) => {
                            let mut summary = serde_json::Map::new();
                            summary.insert(
                                "deleted_task_runs".to_owned(),
                                Value::from(stats.deleted_task_runs),
                            );
                            reporter
                                .emit(None, None, Some("活动记录清理完成"), None)
                                .await
                                .map_err(|error| format!("进度上报失败：{}", error.code()))?;
                            Ok(Value::Object(summary))
                        }
                        Err(error) => Err(format!("活动记录清理失败：{}", error.code())),
                    }
                })
            });
            Ok(handler)
        }),
    );

    registry
}

/// worker 的构造参数。
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// 各道并发度。空 = 用 [`LANE_CONCURRENCY`]。
    pub lanes: HashMap<String, usize>,
    /// 租约秒数。`None` = [`DEFAULT_LEASE_SECONDS`]。
    pub lease_seconds: Option<i64>,
    /// 领取轮询间隔。
    pub poll_interval: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            lanes: HashMap::new(),
            lease_seconds: None,
            poll_interval: CLAIM_POLL_INTERVAL,
        }
    }
}

impl WorkerConfig {
    /// 生效的租约秒数。
    pub fn lease_seconds(&self) -> i64 {
        self.lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS)
    }

    /// 生效的并发道。
    ///
    /// 配置里给出 `default` 时以配置为准（上游
    /// `settings.scheduler.worker_default_concurrency`），其余仍取默认。
    pub fn resolved_lanes(&self) -> HashMap<String, usize> {
        let mut lanes: HashMap<String, usize> = LANE_CONCURRENCY
            .iter()
            .map(|(name, slots)| ((*name).to_owned(), *slots))
            .collect();
        for (name, slots) in &self.lanes {
            if *slots > 0 {
                lanes.insert(name.clone(), *slots);
            }
        }
        lanes
    }

    /// housekeeper 间隔：`max(lease_seconds / 3, 5)`。
    ///
    /// 除以 3 是为了在一半租约用完前续上；那个下限 5 秒防的是租约被配得
    /// 很小时（1 秒）导致 housekeeper 疯狂轮询。
    pub fn housekeeping_interval(&self) -> Duration {
        let by_lease = (self.lease_seconds() / 3).max(HOUSEKEEPING_FLOOR_SECONDS as i64) as u64;
        Duration::from_secs(by_lease)
    }
}

/// 领取道对 `task_key` 的筛选条件。
///
/// default 道**排除**专属道；专属道**只领**自己那条道。
fn lanes_for(name: &str) -> TaskLanes {
    if name == LANE_DEFAULT {
        TaskLanes::excluding(NON_DEFAULT_LANE_TASK_KEYS)
    } else {
        TaskLanes::including(NON_DEFAULT_LANE_TASK_KEYS
            .iter()
            .copied()
            .filter(|key| lane_of(key) == name))
    }
}

/// 一个 `task_key` 属于哪条道。
///
/// 专属道的归属写死，与上游 `JobDefinition.lane` 的默认值对应 —— 上游那
/// 两条队列任务都显式声明了 `lane`，其余都是 `default`。
pub fn lane_of(task_key: &str) -> &'static str {
    match task_key {
        "library_import" => LANE_IMPORT,
        "media_storage_transfer" => LANE_TRANSFER,
        _ => LANE_DEFAULT,
    }
}

/// 后台 worker 句柄。
///
/// 组合根（`sm-server`）拿它 [`spawn`](TaskWorkerHandle::spawn)，关停时
/// [`shutdown`](TaskWorkerHandle::shutdown)。
pub struct TaskWorkerHandle {
    stop: Arc<AtomicBool>,
    joins: Vec<tokio::task::JoinHandle<()>>,
}

impl TaskWorkerHandle {
    /// 停止全部领取线程与 housekeeper，并等它们结束。
    ///
    /// **等**而不是 abort：正在执行的任务被 abort 会留下一行
    /// `running` 且租约未续，只能等租约到期被回收（最多
    /// [`DEFAULT_LEASE_SECONDS`] 秒）。领取线程在下一轮循环开头就会看到
    /// stop 标志，所以这个 join 不会等太久。
    pub async fn shutdown(mut self) -> Result<(), sm_service::error::ServiceError> {
        self.stop.store(true, Ordering::Relaxed);
        for join in self.joins.drain(..) {
            join.await.map_err(|error| {
                sm_service::error::ServiceError::from(
                    sm_service::error::ProgrammerError::new(format!(
                        "worker 任务异常结束：{error}"
                    )),
                )
            })?;
        }
        Ok(())
    }
}

/// 组装并启动 worker。
pub struct TaskWorker;

impl TaskWorker {
    /// 启动 worker：**每道 N 条领取线程 + 1 条 housekeeper**。
    ///
    /// 启动顺序与上游 `TaskWorker.start`（`worker.py:55-82`）一致：
    /// 先恢复中断任务并收口其领域状态，**再**开领取线程 —— 反了会让一条
    /// 「上个进程遗留的 running 行」被立刻领走，而它的领域状态还没收。
    pub async fn spawn(
        db: Db,
        handlers: Arc<HandlerRegistry>,
        config: WorkerConfig,
        config_service: ConfigService,
    ) -> Result<TaskWorkerHandle, sm_service::error::ServiceError> {
        let queue = TaskQueueService::new(&db);
        let stop = Arc::new(AtomicBool::new(false));
        let in_flight: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
        let mut joins = Vec::new();

        // ① 恢复上个进程遗留的任务，并收口它们的领域状态。
        let interrupted = queue.recover_interrupted_runs().await?;
        if !interrupted.is_empty() {
            info!(
                count = interrupted.len(),
                keys = ?interrupted
                    .iter()
                    .map(|run| run.task_key.as_str())
                    .collect::<Vec<_>>(),
                "发现上个进程遗留的任务运行"
            );
        }
        for run in &interrupted {
            handlers.run_recovery(&db, &run.task_key).await;
        }

        // ② 领取线程。
        for (lane, slots) in config.resolved_lanes() {
            for index in 0..slots {
                let db = db.clone();
                let queue = queue.clone();
                let handlers = Arc::clone(&handlers);
                let stop = Arc::clone(&stop);
                let in_flight = Arc::clone(&in_flight);
                let config_service = config_service.clone();
                let poll = config.poll_interval;
                let lease = config.lease_seconds();
                let lane_name = lane.clone();
                joins.push(tokio::spawn(async move {
                    claim_loop(
                        ClaimContext {
                            db,
                            queue,
                            handlers,
                            stop,
                            in_flight,
                            config_service,
                            poll,
                            lease,
                        },
                        lane_name,
                        index,
                    )
                    .await;
                }));
            }
        }

        // ③ housekeeper。
        {
            let db = db.clone();
            let queue = queue.clone();
            let handlers = Arc::clone(&handlers);
            let stop = Arc::clone(&stop);
            let in_flight = Arc::clone(&in_flight);
            let interval = config.housekeeping_interval();
            let lease = config.lease_seconds();
            joins.push(tokio::spawn(async move {
                housekeeping_loop(
                    HousekeepingContext {
                        db,
                        queue,
                        handlers,
                        stop,
                        in_flight,
                        lease,
                    },
                    interval,
                )
                .await;
            }));
        }

        info!(
            lanes = ?config.resolved_lanes(),
            lease_seconds = config.lease_seconds(),
            // tracing 的字段值不接受 `Vec<&str>`，要手工拼。
            handlers = %handlers.keys().join(","),
            "task worker 已启动"
        );

        Ok(TaskWorkerHandle { stop, joins })
    }
}

struct ClaimContext {
    db: Db,
    queue: TaskQueueService,
    handlers: Arc<HandlerRegistry>,
    stop: Arc<AtomicBool>,
    in_flight: Arc<Mutex<Vec<i32>>>,
    config_service: ConfigService,
    poll: Duration,
    lease: i64,
}

async fn claim_loop(ctx: ClaimContext, lane: String, index: usize) {
    let lanes = lanes_for(&lane);
    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            return;
        }
        let claimed = match ctx
            .queue
            .claim_next(Some(ctx.lease), Some(lanes.clone()))
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                // 领取失败退避 5 倍：数据库抖动时不要空转刷日志。
                error!(lane = %lane, index, code = error.code(), "worker 领取失败");
                tokio::time::sleep(ctx.poll * CLAIM_ERROR_BACKOFF).await;
                continue;
            }
        };
        let Some(claimed) = claimed else {
            // 队列为空是常态，不是错误。
            tokio::time::sleep(ctx.poll).await;
            continue;
        };
        execute(&ctx, claimed).await;
    }
}

async fn execute(ctx: &ClaimContext, claimed: sm_db::repo::ClaimedTask) {
    let run = &claimed.run;
    let task_key = run.task_key.clone();
    let task_run_id = run.id;

    // ① 功能停用：收口为 completed 但标记 skipped，**不发通知**。
    //
    // 用 completed 而不是 failed 是上游的选择（`worker.py:114-117`）：功能
    // 没开不是任务的错。failed 会让任务中心一片红，而用户什么都没做。
    if let Ok(values) = ctx.config_service.snapshot() {
        if let Some(reason) = job_disabled_reason(&task_key, &values) {
            let mut summary = serde_json::Map::new();
            summary.insert("skipped".to_owned(), Value::Bool(true));
            summary.insert("reason".to_owned(), Value::String(reason.clone()));
            let tasks = TaskRunService::new(&ctx.db);
            if let Err(error) = tasks
                .complete_task_run(
                    task_run_id,
                    Some(&Value::Object(summary)),
                    Some(&reason),
                    false,
                )
                .await
            {
                error!(task_key, task_run_id, code = error.code(), "跳过任务的收口失败");
            } else {
                info!(task_key, task_run_id, reason = %reason, "任务因功能停用被跳过");
            }
            return;
        }
    }

    // ② 解析执行体。查不到就明确失败 —— 见模块文档。
    let params = sm_db::system::activity::result_summary::from_column_text(run.params.as_deref());
    let handler = match ctx.handlers.build(&ctx.db, &task_key, &params) {
        Ok(handler) => handler,
        Err(error) => {
            let tasks = TaskRunService::new(&ctx.db);
            if let Err(failure) = tasks
                .fail_task_run(task_run_id, &error.to_string(), None, true)
                .await
            {
                error!(task_key, task_run_id, code = failure.code(), "未知任务键的收口失败");
            } else {
                warn!(
                    task_key,
                    task_run_id,
                    reason = %error,
                    "任务没有已落地的处理器，已收口为 failed"
                );
            }
            return;
        }
    };

    // ③ 登记在飞行中，供 housekeeper 续租。
    ctx.in_flight
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(task_run_id);

    let outcome = run_task(
        &ctx.db,
        handler,
        task_run_id,
        Some(&task_key),
        true,
    )
    .await;

    // ④ 无论成败都要移出在飞行集合 —— 否则 housekeeper 会一直续一条已经
    // 终态的行的租约，而它已经不需要租约了。
    {
        let mut guard = ctx
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|id| *id != task_run_id);
    }

    match outcome {
        Ok(value) => {
            info!(task_key, task_run_id, result = %value, "任务完成");
        }
        Err(TaskRunError::Failed(message)) => {
            // 失败已由 run_task 收口，这里只补领域状态。
            error!(task_key, task_run_id, reason = %message, "任务失败");
            ctx.handlers.run_recovery(&ctx.db, &task_key).await;
        }
        Err(TaskRunError::Finalized { state, .. }) => {
            // 别人收的终态。不重试、不改判 —— 服从持久状态。
            warn!(task_key, task_run_id, state, "本执行器未赢得状态转移，已服从持久终态");
        }
        Err(TaskRunError::Service(error)) => {
            error!(task_key, task_run_id, code = error.code(), "任务执行时服务层出错");
        }
    }
}

struct HousekeepingContext {
    db: Db,
    queue: TaskQueueService,
    handlers: Arc<HandlerRegistry>,
    stop: Arc<AtomicBool>,
    in_flight: Arc<Mutex<Vec<i32>>>,
    lease: i64,
}

async fn housekeeping_loop(ctx: HousekeepingContext, interval: Duration) {
    loop {
        // 睡满一轮再干活，且把 stop 与间隔合成一次等待 —— shutdown 不用等满。
        if wait_or_stop(&ctx.stop, interval).await {
            return;
        }
        renew_in_flight_leases(&ctx).await;
        recover_expired_leases(&ctx).await;
    }
}

/// 睡 `interval`；被 stop 唤醒时返回 `true`。
async fn wait_or_stop(stop: &AtomicBool, interval: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + interval;
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline - now;
        // 每 200ms 看一次停止标志：既不让 shutdown 等满一整个租约的三分之一，
        // 也不必引入一个可唤醒的 stop 通道。
        tokio::time::sleep(remaining.min(Duration::from_millis(200))).await;
    }
}

async fn renew_in_flight_leases(ctx: &HousekeepingContext) {
    let ids: Vec<i32> = ctx
        .in_flight
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if ids.is_empty() {
        return;
    }
    if let Err(error) = ctx.queue.renew_leases(&ids, Some(ctx.lease)).await {
        // 续租失败的后果是这批任务被当成僵尸回收 —— 所以要记。
        error!(count = ids.len(), code = error.code(), "续租失败");
    }
}

async fn recover_expired_leases(ctx: &HousekeepingContext) {
    let recovered = match ctx.queue.recover_expired_leases(None).await {
        Ok(recovered) => recovered,
        Err(error) => {
            error!(code = error.code(), "回收过期租约失败");
            return;
        }
    };
    if recovered.is_empty() {
        return;
    }
    info!(count = recovered.len(), "回收了过期租约的任务运行");
    // 回收意味着上一个执行体可能留下了半成品领域状态。
    for run in recovered {
        ctx.handlers.run_recovery(&ctx.db, &run.task_key).await;
    }
}
