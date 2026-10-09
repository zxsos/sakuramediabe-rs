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
use sm_service::catalog::movie_asset_pack_backfill::MovieAssetPackBackfillService;
use sm_service::catalog::movie_heat::MovieHeatService;
use sm_service::catalog::movie_task::MovieTaskService;
use sm_service::system::activity::{run_task, TaskHandler, TaskRunError, TaskRunService};
use sm_service::system::activity_cleanup::RetentionPolicy;
use sm_service::system::optional_services::job_disabled_reason;
use sm_service::system::task_queue::{TaskQueueService, DEFAULT_LEASE_SECONDS};
use sm_service::system::ActivityCleanupService;
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
pub const LANE_CONCURRENCY: [(&str, usize); 3] =
    [(LANE_DEFAULT, 4), (LANE_IMPORT, 2), (LANE_TRANSFER, 1)];

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

/// handler 需要的**进程级依赖**。
///
/// # 为什么是这个形状
///
/// [`HandlerFactory`] 的签名是 `Fn(&Db, &Value) -> Result<TaskHandler, _>` ——
/// **每次调用只拿到 `&Db` 与参数**。而 `image_search_index` 还需要配置
/// （`image_search.inference_base_url`）与 Qdrant 端点。
///
/// 三种做法：
///
/// | 做法 | 问题 |
/// |---|---|
/// | 改 `HandlerFactory` 签名 | 破坏所有已注册的 handler，且 `&Db` 是每次调用的，配置不是 |
/// | 用全局 `static` | 测试没法注入不同配置 —— 而 `AppState::auth` 之所以进 state 就是为了这个 |
/// | **工厂闭包捕获 `Arc<HandlerDeps>`** | ✅ 无破坏、可注入、`Fn + Send + Sync` 满足 |
///
/// 选第三种。`ConfigService` 的 `Clone` 只复制一个 `PathBuf`，很便宜。
#[derive(Debug, Clone)]
pub struct HandlerDeps {
    /// 配置服务。`image_search_index` 从这里读 `image_search.*`。
    pub config: sm_service::system::config::ConfigService,
    /// Qdrant 端点。**只存连接信息，不存活客户端** ——
    /// 建客户端可能失败，而那应该由工厂返回的 `Err` 报出来，
    /// 而不是让 `builtin_handlers()` 整个 panic。
    pub qdrant: QdrantEndpoint,
}

/// Qdrant 连接信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QdrantEndpoint {
    /// gRPC 端点。**注意上游 `qdrant.url` 默认是 REST 端口**（`http://qdrant:6333`），
    /// 而 `qdrant-client` 走 gRPC —— 调用方要转换。
    pub url: String,
    pub api_key: Option<String>,
}

impl QdrantEndpoint {
    /// 从配置快照读。
    ///
    /// **缺 `qdrant` 节时给空串**（而不是报错）—— 那等价于「Qdrant 没配」，
    /// 由 `image_search_enabled` 那道闸门去拦。**这里报错会让「没配 Qdrant」
    /// 变成进程起不来。**
    pub fn from_snapshot(values: &serde_json::Value) -> Self {
        let section = values.get("qdrant");
        Self {
            url: section
                .and_then(|q| q.get("url"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            api_key: section
                .and_then(|q| q.get("api_key"))
                .and_then(serde_json::Value::as_str)
                .filter(|key| !key.is_empty())
                .map(str::to_owned),
        }
    }

    /// 是否配了端点。
    pub fn is_configured(&self) -> bool {
        !self.url.trim().is_empty()
    }
}
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
pub type BusinessRecovery =
    Box<dyn Fn(&Db) -> Pin<Box<dyn Future<Output = Result<(), WorkerError>> + Send>> + Send + Sync>;

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

    fn build(&self, db: &Db, task_key: &str, params: &Value) -> Result<TaskHandler, WorkerError> {
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
/// # 现在有六个
///
/// 19 个内建 cron 任务里 13 个的 service 还没写（zip / provider 各挡一批，
/// 见 `docs/service-progress.md`）。**不注册就没有处理器**，那些任务被领到
/// 时会明确 `failed` 并写清「未在处理器注册表中」，而不是静默跳过。
///
/// | task_key | service 域 | 外部依赖 |
/// |---|---|---|
/// | `activity_record_cleanup` | `system` | 无 |
/// | `movie_asset_pack_backfill` | `catalog` | 无（只读库 + 本地图片根）|
/// | `movie_heat_update` | `catalog` | 无 |
/// | `image_search_index` | `discovery` | 推理服务 + Qdrant（都已在 `sm-service` 侧就位）|
/// | `movie_similarity_recompute` | `discovery` | Qdrant（稀疏向量通路）|
/// | `daily_recommendation_generate` | `discovery` | Qdrant（**可选**：不可用时只丢相似度那一路）|
///
/// `activity_record_cleanup` 先把链路端到端跑通；`image_search_index` 是第一个
/// 带外部依赖的 handler（依赖通路 [`HandlerDeps`] 就是为它加的）；
/// `movie_asset_pack_backfill` 则是第一个**纯本地**的长任务 ——
/// 它同时是 `manual_only`，所以「不注册 handler」对它等于功能完全不存在。
///
/// # 依赖怎么进的 handler
///
/// [`builtin_handlers`] 接收 [`HandlerDeps`] 并用 `Arc` 捕获进每个工厂闭包。
/// 组合根 `sm-server` 负责读配置并注入 —— 它是唯一同时看得见
/// `ConfigService` 与 Qdrant 端点的地方。
///
/// **没有改 `HandlerFactory` 的签名**：它是 `Fn(&Db, &Value)`，每次调用只拿到
/// 数据库连接，而配置是进程级的。改签名会破坏所有已注册 handler。
pub fn builtin_handlers(deps: HandlerDeps) -> HandlerRegistry {
    // 闭包捕获用 `Arc` —— `HandlerFactory` 要求 `Fn + Send + Sync + 'static`，
    // 而工厂是**多次**调用的（每个任务运行一次），不能把依赖 move 进去。
    let deps = Arc::new(deps);
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

    // `image_search_index` —— 图搜索索引构建 / 重建。
    //
    // **第二个落地的 handler**，也是第一个「有外部依赖」的：推理服务（取向量）
    // + Qdrant（存向量）。三个依赖都已在 `sm-service` 侧就位，所以它能真正
    // 端到端跑。
    //
    // # `params.reset` 决定单阶段还是双阶段
    //
    // | 来源 | params | 行为 |
    // |---|---|---|
    // | cron tick | 无 / `{}` | 单阶段：只补齐 PENDING 的图片 |
    // | `POST /image-search/reset` | `{"reset": true}` | 双阶段：先清库重建，再全量索引 |
    //
    // 这个 `reset` 键是 `discovery::image_search_reset::reset_params()` 定的
    // **契约** —— 那边改名这里就静默失效（变成单阶段，`reset: true` 被忽略）。
    // 每个工厂各持一份 `Arc` —— `move` 闭包会把捕获的变量**整个搬走**，
    // 第二个工厂拿不到同一个 `Arc`。这是三个 handler 共用一份依赖的写法。
    let image_search_deps = Arc::clone(&deps);
    registry.register(
        "image_search_index",
        Box::new(move |db: &Db, params: &Value| {
            // 工厂是 `Fn`（每个任务运行调一次），所以每次 **clone**，不能把
            // 捕获的那个 move 进 handler。
            let deps = Arc::clone(&image_search_deps);
            // handler future 要求 `'static`：`db` / `params` 都是工厂的
            // **借用参数**，必须克隆成自有值才能进 async block。
            let db = Db::clone(db);
            let params = params.clone();
            let handler: TaskHandler = Box::new(move |reporter| {
                Box::pin(async move {
                    // `reset` 决定单阶段还是双阶段（见上面的表格）。
                    let reset = params
                        .get("reset")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let batch_size =
                        image_search_batch_size(&deps.config, "index_upsert_batch_size");
                    let inference_batch =
                        image_search_batch_size(&deps.config, "inference_batch_size");

                    // 图搜未启用时**正常返回 None**（不干活），不是 Err ——
                    // `optional_services::job_disabled_reason` 已经把这类任务
                    // 在任务中心置灰，这里再报错会让「未启用」看起来像「坏了」。
                    let Some(mut service) = build_image_search_service(&db, &deps)? else {
                        // **不能返回 `Ok(None)`** —— `TaskHandlerResult = Result<Value, String>`，
                        // `None` 编不过。用一个显式的 `skipped` 摘要。
                        //
                        // 键名 `skipped` 是自定的：不像 `stats` 那样要与上游逐字
                        // 一致，因为上游「未启用」时 worker 根本不领这个任务。
                        return Ok(serde_json::json!({
                            "skipped": true,
                            "reason": "image_search 未启用",
                        }));
                    };
                    let sink = progress_sink_for(&reporter);
                    let summary = service
                        .index_pending_images(batch_size, inference_batch, reset, Some(sink))
                        .await
                        .map_err(|error| format!("图搜索索引失败：{}", error.code()))?;
                    // `summary` 的键与上游**逐字一致** —— 它会进 `signal_scores`
                    // 一类的列，改名会让历史记录对不上。
                    serde_json::to_value(summary)
                        .map_err(|error| format!("摘要序列化失败：{error}"))
                })
            });
            Ok(handler)
        }),
    );

    // `movie_similarity_recompute` —— 影片相似度全量重算。
    //
    // **第三个落地的 handler**，与 `image_search_index` 共用 Qdrant 端点但走
    // **另一条通路**：`image_search` 用稠密向量（`DenseStore`），这里用
    // **稀疏向量**（`MovieSimilarityStore`，带蓝绿集合切换）。
    //
    // # 为什么这个任务的失败后果比图搜更大
    //
    // 相似度索引是**每日推荐的主信号**（权重 8/19）。它坏掉时每日推荐不会
    // 报错 —— 那一路信号降级成 0，所有推荐退化成冷启动（见
    // `daily_recommendation` 的三种制度）。所以**这个任务静默失败比报错更
    // 糟**，不能像图搜那样「未启用就跳过」——
    // `job_disabled_reason("movie_similarity_recompute", …)` 已经在领取前
    // 拦住了未启用的情况，走到工厂里就说明**该跑**。
    let similarity_deps = Arc::clone(&deps);
    registry.register(
        "movie_similarity_recompute",
        Box::new(move |db: &Db, _params: &Value| {
            let deps = Arc::clone(&similarity_deps);
            // 同上：handler future 要 `'static`，`db` 必须克隆成自有值。
            let db = Db::clone(db);
            let handler: TaskHandler = Box::new(move |reporter| {
                Box::pin(async move {
                    // Qdrant 没配端点 = 配置自相矛盾（`qdrant.enabled` 为真
                    // 却没 url），**报错**而不是跳过 —— 见
                    // `build_image_search_service` 里同一条的说明。
                    // 走到这里必然已启用（领取前 `job_disabled_reason` 已拦过），
                    // 所以 `None` 是「不该发生」。但 `Option` 必须拆开才能调方法 ——
                    // 用 `Err` 而不是 `unwrap()`：panic 会让整个 worker 进程退出。
                    let service = build_movie_similarity_service(&db, &deps)?
                        .ok_or_else(|| "movie_similarity 未启用，却仍被领取".to_owned())?;
                    let sink = progress_sink_for(&reporter);
                    let stats = service
                        .recompute_all(Some(sink))
                        .await
                        .map_err(|error| format!("影片相似度重算失败：{}", error.code()))?;
                    // 键名与上游逐字一致 —— 会进 `signal_scores` 一类的列。
                    serde_json::to_value(stats).map_err(|error| format!("摘要序列化失败：{error}"))
                })
            });
            Ok(handler)
        }),
    );

    // `daily_recommendation_generate` —— 每日推荐快照生成。
    //
    // **第五个落地的 handler**，也是第一个「Qdrant 可用性不影响成败」的：
    // 相似度只是六路信号里的一路（权重 8/19），Qdrant 不可用时其余五路照常
    // 打分（上游 `:196-199` 捕获 `MovieSimilarityIndexError` 后 `return {}`）。
    //
    // # 与 `movie_similarity_recompute` 的对照
    //
    // 那个任务**就是**在维护相似度索引，Qdrant 不可用等于任务失败；
    // 这个任务只是**消费**它，不可用时降级。所以这里 `build_similarity_store`
    // 返回 `Option`（未启用 / 未配置都是 `None`），而
    // `build_movie_similarity_service` 在配置矛盾时报错。
    //
    // # 没有参数
    //
    // 上游 registry 的 handler 是
    // `lambda reporter, _params: DailyRecommendationService.generate_latest_snapshot(
    //     progress_callback=reporter.progress_callback)` —— `target_date` 与
    // `limit` 都走默认（今天 / 200）。
    let daily_deps = Arc::clone(&deps);
    registry.register(
        "daily_recommendation_generate",
        Box::new(move |db: &Db, _params: &Value| {
            let deps = Arc::clone(&daily_deps);
            let db = Db::clone(db);
            let handler: TaskHandler = Box::new(move |reporter| {
                Box::pin(async move {
                    let similarity = build_similarity_store(&deps)?;
                    let service = sm_service::discovery::daily_recommendation::DailyRecommendationService::new(&db);
                    let sink = progress_sink_for(&reporter);
                    let stats = service
                        .generate_latest_snapshot(
                            None,
                            sm_service::discovery::daily_recommendation::DAILY_RECOMMENDATION_LIMIT,
                            similarity.as_ref(),
                            Some(sink),
                        )
                        .await
                        .map_err(|error| format!("每日推荐快照生成失败：{}", error.code()))?;
                    // 键名与上游 stats dict 逐字一致（进 `result_summary`）。
                    serde_json::to_value(stats).map_err(|error| format!("摘要序列化失败：{error}"))
                })
            });
            Ok(handler)
        }),
    );

    // `movie_asset_pack_backfill` —— 存量影片图片打包回填。
    //
    // **第四个落地的 handler，也是第一个不依赖任何外部服务的**：它只读库 +
    // 读写本地图片根（`movie_asset_pack` / `media_paths` 的构件早已落地），
    // 不需要 Qdrant、推理服务或插件。
    //
    // # 它是 `manual_only`，所以「没 handler」比别处更致命
    //
    // 上游它是三条无 cron 的任务之一（另两条是 `media_video_info_backfill`
    // 与 `media_thumbnail_pack_backfill`）：**只能手动触发**。没注册 handler
    // 的话，手动触发也只会以 `NoHandler` 失败 —— 等于这个功能完全不存在
    // （而它修的是「早期版本没建包」的存量数据）。
    let backfill_deps = Arc::clone(&deps);
    registry.register(
        "movie_asset_pack_backfill",
        Box::new(move |db: &Db, _params: &Value| {
            let deps = Arc::clone(&backfill_deps);
            let db = Db::clone(db);
            let handler: TaskHandler = Box::new(move |reporter| {
                Box::pin(async move {
                    let service = MovieAssetPackBackfillService::new(&db, &deps.config);
                    let sink = progress_sink_for(&reporter);
                    let stats = service
                        .backfill(Some(sink))
                        .await
                        .map_err(|error| format!("影片图片打包回填失败：{}", error.code()))?;
                    // 键名与上游 `backfill` 返回的 dict 逐字一致（会进
                    // `result_summary`，客户端按那些键读数字）。
                    serde_json::to_value(stats).map_err(|error| format!("摘要序列化失败：{error}"))
                })
            });
            Ok(handler)
        }),
    );

    // `movie_heat_update` —— 热度重算。`params` 有内容就只算那一部，没有就全表。
    //
    // 上游的注册项（`scheduler/registry.py:55-60`）就是这个两分支：
    //
    // ```python
    // def _run_movie_heat(reporter, params):
    //     return (MovieTaskService.execute_movie_heat(reporter, params)
    //             if params else MovieHeatService.update_movie_heat())
    // ```
    //
    // **两个分支缺一不可**：手动触发带 `{movie_number}` 走单部（
    // `POST /movies/{n}/heat-recompute`），cron 那条不带参数走全表。
    // 只实现一支的话，另一条入口会静默地做错事 —— 比如把「重算一部」
    // 变成「扫 30 万行」。
    //
    // `params` 的"空"按**非空对象**判（Python 里空 dict 是 falsy）。
    registry.register(
        "movie_heat_update",
        Box::new(move |db: &Db, params: &Value| {
            let db = Db::clone(db);
            let params = params.clone();
            let handler: TaskHandler = Box::new(move |_reporter| {
                Box::pin(async move {
                    let service = MovieTaskService::new(&db);
                    let is_single_movie =
                        params.as_object().is_some_and(|object| !object.is_empty());
                    if is_single_movie {
                        return service
                            .execute_movie_heat(&params)
                            .await
                            .map_err(|error| format!("影片热度重算失败：{}", error.code()));
                    }
                    let stats = MovieHeatService::new(&db)
                        .update_movie_heat()
                        .await
                        .map_err(|error| format!("影片热度重算失败：{}", error.code()))?;
                    serde_json::to_value(stats).map_err(|error| format!("摘要序列化失败：{error}"))
                })
            });
            Ok(handler)
        }),
    );
    registry
}

/// 进度 sink 的具体类型。
///
/// `image_search_index::ProgressSink` 与 `recommendation::ProgressSink` 是
/// **两个同形的类型别名**（`handoff.md` 纪律第 7 条意义上又一处重复，但拆
/// 它们要动两个模块的公开签名，这轮不做）。别名是透明的，所以这**一个**
/// 具体类型同时满足两处。
///
/// # 为什么必须 `'static` + 参数要拷进 future
///
/// sink 被 move 进 worker 的 handler future，而 `TaskHandler` 要求
/// `Pin<Box<dyn Future + Send>>` —— 隐含 `'static`。所以：
///
/// - 闭包**拥有** reporter 的一个 `Clone`（`TaskRunReporter` 是 `Clone`），
///   不能借用外层的那个；
/// - `text: &str` 与 `patch: Option<&Value>` 是**调用时**的借用，返回的
///   future 活得更久 —— 先把它们拷成 `String` / `Value` 再 `async move`。
type SharedProgressSink = Box<
    dyn FnMut(
            Option<i32>,
            Option<i32>,
            &str,
            Option<&Value>,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send,
>;

/// 把一个 reporter 包成进度 sink。
fn progress_sink_for(
    reporter: &sm_service::system::activity::TaskRunReporter,
) -> SharedProgressSink {
    let reporter = reporter.clone();
    Box::new(move |current, total, text, patch| {
        let reporter = reporter.clone();
        let text = text.to_owned();
        let patch = patch.cloned();
        Box::pin(async move {
            reporter
                .emit(current, total, Some(&text), patch.as_ref())
                .await
                // `emit` 返回 `ServiceError`，而 sink 契约要 `String`。
                .map_err(|error| format!("进度上报失败：{}", error.code()))
        })
    })
}

/// 读 `image_search.<key>` 的批量大小，缺省 16。
///
/// 上游 `settings.image_search.index_upsert_batch_size` 与
/// `inference_batch_size` 都有默认值，且 `max(1, ...)` —— **0 或负数会被抬到 1**
/// 而不是报错（`image_search_index_service.py:72-73`）。照抄。
fn image_search_batch_size(config: &sm_service::system::config::ConfigService, key: &str) -> i64 {
    // `snapshot()` 返回 `Result`。读不到就**回落默认值**而不是传播错误 ——
    // 批量大小不是「能不能干活」的判据，读失败按上游的缺省 16 走。
    let Ok(snapshot) = config.snapshot() else {
        return 16;
    };
    snapshot
        .get("image_search")
        .and_then(|section| section.get(key))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(16)
        .max(1)
}

/// 构造 `ImageSearchIndexService`。`None` = 图搜未启用。
fn build_image_search_service(
    db: &Db,
    deps: &HandlerDeps,
) -> Result<Option<sm_service::discovery::image_search_index::ImageSearchIndexService>, String> {
    use sm_service::discovery::embedding::EmbeddingClient;
    use sm_service::discovery::image_search_index::ImageSearchIndexService;
    // 服务要的是**两个具体集合的 store**（`ThumbnailVectorStore` /
    // `PlotImageVectorStore`），不是裸 `DenseStore`：写侧的 `upsert_records`
    // 在这两个类型上，记录 → point 的映射也在它们各自的文件里。
    use sm_service::discovery::qdrant::dense::{
        PLOT_IMAGE_COLLECTION, PLOT_IMAGE_PAYLOAD_INDEX, THUMBNAIL_COLLECTION,
        THUMBNAIL_PAYLOAD_INDEX,
    };
    use sm_service::discovery::qdrant::plot_image::PlotImageVectorStore;
    use sm_service::discovery::qdrant::thumbnail::ThumbnailVectorStore;

    // `snapshot()` 返回 `Result`。**这里不 `?`** —— 读不到配置时按「未启用」
    // 处理（返回 `None`）。理由：调用点在 worker 的领取循环里，让它因为配置
    // 读失败而把整条任务判失败不合适。
    //
    // 组合根那侧**已经把 worker 启动时的读配置错误显式抛出**了，所以走到这里
    // 读失败只可能是运行中文件被删 —— 那种情况下「图搜不可用」是正确判断。
    let Ok(snapshot) = deps.config.snapshot() else {
        return Ok(None);
    };
    // 未启用 -> None。不报错：那不是故障。
    if !sm_service::system::optional_services::image_search_enabled(&snapshot) {
        return Ok(None);
    }
    if !deps.qdrant.is_configured() {
        // 这一条**报错**而不是 None —— `image_search_enabled` 已经检查过
        // `qdrant.enabled`，走到这里说明配置自相矛盾（enabled 为真但没 url）。
        return Err("image_search 已启用但 qdrant.url 为空：配置不一致".to_owned());
    }
    let base = deps.qdrant.url.trim_end_matches('/');
    let embedding = || -> Result<EmbeddingClient, String> {
        let base_url = snapshot
            .get("image_search")
            .and_then(|section| section.get("inference_base_url"))
            .and_then(serde_json::Value::as_str)
            .ok_or("image_search.inference_base_url 未配置")?;
        let api_key = snapshot
            .get("image_search")
            .and_then(|section| section.get("inference_api_key"))
            .and_then(serde_json::Value::as_str)
            // `EmbeddingClient::new` 收 `Option<String>`，不是 `Option<&str>`。
            .map(str::to_owned);
        Ok(EmbeddingClient::new(
            base_url,
            api_key,
            std::time::Duration::from_secs(120),
            std::time::Duration::from_secs(10),
        ))
    };
    // `api_key` 是 `Option<&str>` 而 `deps` 是借用 —— 直接传 `as_deref()`。
    //
    // `payload_index_fields` 传各自那份常量：缩略图按 `movie_id` + `media_id`
    // 过滤，剧情图**只有** `movie_id`（见 dense.rs 的两个常量）。
    let api_key = deps.qdrant.api_key.as_deref();
    // `DenseStore::connect` 返回 `ServiceError`，而本函数的错误是 `String` ——
    // 显式转（worker 的错误就是字符串，没有 `From<ServiceError>`）。
    let connect = |collection: &str,
                   payload_index_fields: &'static [&'static str]|
     -> Result<sm_service::discovery::qdrant::dense::DenseStore, String> {
        sm_service::discovery::qdrant::dense::DenseStore::connect(
            base,
            api_key,
            collection,
            payload_index_fields,
        )
        .map_err(|error| format!("向量库连接失败：{}", error.code()))
    };
    Ok(Some(ImageSearchIndexService::new(
        Arc::new(ThumbnailVectorStore::with_store(connect(
            THUMBNAIL_COLLECTION,
            THUMBNAIL_PAYLOAD_INDEX,
        )?)),
        Arc::new(PlotImageVectorStore::with_store(connect(
            PLOT_IMAGE_COLLECTION,
            PLOT_IMAGE_PAYLOAD_INDEX,
        )?)),
        Arc::new(embedding()?),
        sm_db::repo::discovery::PendingImageRepository::new(db.clone()),
        sm_db::repo::discovery::ImageSearchIndexStateRepository::new(db.clone()),
    )))
}

/// 构造 `MovieRecommendationService`。Qdrant 未启用时返回 `None`。
///
/// # 与 [`build_image_search_service`] 的三处差别
///
/// | | `image_search_index` | `movie_similarity_recompute` |
/// |---|---|---|
/// | 能力开关 | `image_search_enabled`（两个开关） | `movie_similarity_enabled`（只要 Qdrant） |
/// | 构造失败 | `None`（未启用，正常跳过） | **不适用** —— 走到这里必然已启用 |
/// | 推理服务 | 需要（`EmbeddingClient`） | **不需要** —— 稀疏向量全在 DB 侧算 |
///
/// 第三行是这个函数**不需要** `EmbeddingClient` 的原因：相似度用的是
/// 演员/标签的 **IDF 加权稀疏向量**（`recommendation::build_sparse_vector`），
/// 没有图片，自然没有推理这一跳。
fn build_movie_similarity_service(
    db: &Db,
    deps: &HandlerDeps,
) -> Result<Option<sm_service::discovery::recommendation::MovieRecommendationService>, String> {
    use sm_service::discovery::qdrant::similarity::MovieSimilarityStore;
    use sm_service::discovery::recommendation::MovieRecommendationService;

    // 与 `build_image_search_service` 同一条理由：读不到配置按「未启用」处理，
    // 不让整个 worker 因配置读失败而把任务判失败。
    let Ok(snapshot) = deps.config.snapshot() else {
        return Ok(None);
    };
    if !sm_service::system::optional_services::movie_similarity_enabled(&snapshot) {
        return Ok(None);
    }
    if !deps.qdrant.is_configured() {
        return Err("movie_similarity 已启用但 qdrant.url 为空：配置不一致".to_owned());
    }
    let base = deps.qdrant.url.trim_end_matches('/');
    let store = MovieSimilarityStore::connect(base, deps.qdrant.api_key.as_deref())
        .map_err(|error| format!("向量库连接失败：{}", error.code()))?;
    Ok(Some(MovieRecommendationService::new(
        store,
        sm_db::repo::recommendation::MovieFeatureRepository::new(db.clone()),
    )))
}

/// 构造 `MovieSimilarityStore`。**未启用或未配置端点时返回 `None`（合法状态）。**
///
/// # 与 [`build_movie_similarity_service`] 的差别：这里的 `None` 不是「跳过任务」
///
/// `daily_recommendation_generate` 的任务不是「维护相似度索引」，而是**消费**它。
/// 索引不可用时每日推荐仍有热度 / 榜单 / 新鲜度等五路信号（上游
/// `daily_recommendation_service.py:196-199` 把 `MovieSimilarityIndexError`
/// 捕获成空表），所以这里：
///
/// | 情形 | 本函数 | 后果 |
/// |---|---|---|
/// | `movie_similarity_enabled` 为假 | `None` | 相似度全程 0，推荐照常产出 |
/// | `qdrant.enabled` 真但 `url` 空（配置矛盾） | `None` + warn | 同上 —— **不报错** |
/// | 端点可连 | `Some(store)` | 正常 |
///
/// `build_movie_similarity_service` 在第 2 行那种情形**报错**，因为它的任务
/// 离了 Qdrant 什么也做不了；这里离了它还能产出推荐。
fn build_similarity_store(
    deps: &HandlerDeps,
) -> Result<Option<sm_service::discovery::qdrant::similarity::MovieSimilarityStore>, String> {
    use sm_service::discovery::qdrant::similarity::MovieSimilarityStore;

    // 与 `build_image_search_service` 同一条理由：读不到配置按「未启用」处理，
    // 不让整个 worker 因配置读失败而把任务判失败。
    let Ok(snapshot) = deps.config.snapshot() else {
        return Ok(None);
    };
    if !sm_service::system::optional_services::movie_similarity_enabled(&snapshot) {
        return Ok(None);
    }
    if !deps.qdrant.is_configured() {
        // 这里是**降级**而不是报错 —— 见上面的表格。配置矛盾值得一条日志，
        // 但不该让「每日推荐」整个停摆。
        warn!("movie_similarity 已启用但 qdrant.url 为空，每日推荐将跳过相似度信号");
        return Ok(None);
    }
    let base = deps.qdrant.url.trim_end_matches('/');
    MovieSimilarityStore::connect(base, deps.qdrant.api_key.as_deref())
        .map(Some)
        .map_err(|error| format!("向量库连接失败：{}", error.code()))
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
        TaskLanes::including(
            NON_DEFAULT_LANE_TASK_KEYS
                .iter()
                .copied()
                .filter(|key| lane_of(key) == name),
        )
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
/// 组合根（`sm-server`）拿它启动，关停时调
/// [`shutdown`](TaskWorkerHandle::shutdown)，
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
                sm_service::error::ServiceError::from(sm_service::error::ProgrammerError::new(
                    format!("worker 任务异常结束：{error}"),
                ))
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
                error!(
                    task_key,
                    task_run_id,
                    code = error.code(),
                    "跳过任务的收口失败"
                );
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
                error!(
                    task_key,
                    task_run_id,
                    code = failure.code(),
                    "未知任务键的收口失败"
                );
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

    let outcome = run_task(&ctx.db, handler, task_run_id, Some(&task_key), true).await;

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
            warn!(
                task_key,
                task_run_id, state, "本执行器未赢得状态转移，已服从持久终态"
            );
        }
        Err(TaskRunError::Service(error)) => {
            error!(
                task_key,
                task_run_id,
                code = error.code(),
                "任务执行时服务层出错"
            );
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
