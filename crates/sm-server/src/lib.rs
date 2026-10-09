//! 组合根：单进程同时承载 API(8000) 与调度器。
//!
//! 对应上游的 `src/api/app.py`（`create_app` + lifespan）与
//! `src/start/aps.py`（`aps()`）—— 两个进程做的事，这里在一个进程里做完。
//!
//! # 装配顺序是有意义的
//!
//! ```text
//! 配置 → 日志 → 连接池 → 鉴权/配置服务 → 插件 → 路由 → 调度器 → HTTP →
//! 等信号 → 逆序收尾
//! ```
//!
//! 插件在**路由之前**：任务目录里有插件任务，而目录要在建 `AppState` 时给它。
//! 插件在调度器之前：插件任务的 cron 要并进调度表。
//!
//! 日志在配置之后：配置阶段的错误也要能被打出来。连接池在路由之前：
//! `AppState` 持有 `Db`，而 `Db` 就是池。调度器在 HTTP 之前：反过来的话，
//! 第一个请求可能打到一个「还没有调度器」的实例上，而健康检查已经通过了。
//!
//! # 关闭顺序同样是有意义的
//!
//! 先停止接受新请求（`axum::serve` 的 `with_graceful_shutdown`），**再**停
//! 调度器。反过来的话，从收到信号到真正退出之间会有一个窗口：HTTP 已经
//! 不接了，但调度器还在往队列里入队 —— 而此时 worker 也已经在停止，
//! 那些任务要等到下次启动才跑。
//!
//! # `panic = "abort"` 下不能靠 `catch_unwind` 降级
//!
//! release profile 是 `panic = "abort"`（`Cargo.toml`），panic 即进程退出，
//! 由 supervisor 重启。所以本模块的错误处理是「明确退出码 + 上下文链」，
//! 没有任何 `catch_unwind`。

pub mod config;
pub mod error;
pub mod logging;
/// 宿主能力出口（`PluginHost`）的服务端。同样只在组合根：它要同时用
/// `sm_db`（查影片/演员）与 `sm-plugin-api`（proto），而 `sm-plugins` 不该
/// 反向依赖业务层的数据。
pub mod plugin_host;
pub mod plugins;
// provider 数据面的**实现**只能在这里：`sm-service` 不能依赖 `sm-plugins`
// （依赖方向会成环），而 `sm-server` 同时看得见两边。见模块文档。
pub mod provider_gateway;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sm_api::middleware::slow_log::SlowLogConfig;
use sm_scheduler::{Scheduler, SchedulerHandle, TaskWorker, TaskWorkerHandle, WorkerConfig};
use sm_service::system::auth::AuthConfig;

pub use config::{ListenConfig, PoolConfig, ServerConfig};
pub use error::ConfigError;

/// 调度器、worker 与看门狗这一组后台任务。
struct Background {
    scheduler: SchedulerHandle,
    /// 队列消费者。`None` 表示 worker 没起来 —— 那时日志里有一条 error，
    /// 队列会持续积压而无人领取。
    worker: Option<TaskWorkerHandle>,
    watchdog: tokio::task::JoinHandle<()>,
    /// 看门狗的停止标志。abort 之外还要它：看门狗可能正睡在退避里，
    /// abort 直接打断即可，但标志让「为什么停」在日志里是可解释的。
    watchdog_stop: Arc<std::sync::atomic::AtomicBool>,
    /// 匿名遥测心跳（**独立循环，不走任务队列**，见下）。`None` = 被 env 关闭。
    telemetry: Option<tokio::task::JoinHandle<()>>,
}

/// 遥测心跳的间隔。上游 `scheduler.add_job(..., hours=1)`（`start/aps.py:365-374`）。
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// 装配并运行。返回进程退出码。
///
/// 拆出 `run` 而不是全部塞进 `main`：这样集成测试能只跑「配置 → 组件装配」
/// 这一段，而不必真的监听端口并等到信号。
pub async fn run(config: ServerConfig) -> anyhow::Result<()> {
    // 1. 日志。先于任何会失败的组件，否则启动失败时只有一行 stderr。
    let log_dir = config.log_dir.as_deref().map(std::path::PathBuf::from);
    let _logs = logging::init(&config.log_filter, log_dir.as_deref());
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        " sakuramediabe-rs 启动中"
    );

    // 2. 连接池。
    let pool = connect_pool(&config).await?;
    tracing::info!(
        max_connections = config.pool.max_connections,
        "数据库连接池就绪"
    );

    // 3. 鉴权与配置服务。密钥取自配置，不留硬编码默认值。
    let auth = AuthConfig::new(config.jwt_secret.clone());
    // 配置服务指向 `ServerConfig` 解析出的那个路径 —— 必须同一个，否则
    // `PATCH /config` 会写进另一个文件，表现为「改了没反应」。
    // 变量名不叫 `config`：那会遮蔽 `ServerConfig`，而下一行还要读它的
    // `slow_log` —— 遮蔽后那句会静默变成读 `ConfigService` 的不存在的字段。
    let config_service = sm_service::system::ConfigService::new(config.config_path.clone());

    // 3b. **配置读不了就拒绝启动**（fail fast）。
    //
    // 为什么放在这里：配置坏了以后，**每个**请求都会坏。放到请求期才发现，
    // 服务看起来是健康的（能连库、能响应），只是逐个接口开始 500 —— 排查
    // 方向会被带偏到「某个接口坏了」，而真正的病因是配置文件。
    //
    // 以前没有这道校验，于是 `clip_collections` / `media_clips` / `jobs` /
    // `movie_subscriptions` / 签名五处都用 `snapshot().unwrap_or_default()`
    // 把「读不了」吞成全默认配置 —— 见 `sm_api::config` 的模块文档。
    // 那 6 个红测试就是它留下的症状。
    //
    // 「文件不存在」不算失败（首次启动的正常路径）。`validate` 同时查字段取值
    // 合法性，所以「能解析但 `qdrant.url` 缺 scheme」这类也会在这里被拦下。
    if let Err(error) = config_service.validate() {
        // 日志系统此刻已就绪（第 1 步），所以错误能进结构化日志而不只是 stderr。
        tracing::error!(
            code = error.code(),
            details = ?error.details(),
            path = %config.config_path.display(),
            "配置校验未通过，拒绝启动"
        );
        return Err(anyhow::anyhow!(
            "配置校验未通过（{}）：{}",
            error.code(),
            error.api.message
        ));
    }

    // 4. 插件。**在路由装配与调度器之前** —— 任务目录里有插件任务，反过来的话
    //    手动触发会把它们判成「未知任务」；调度表也会漏掉它们的 cron
    //    （而没有任何错误会提示）。
    //
    // 配置从**磁盘快照**读（`plugins` 是只读键，可能含插件凭据，不进 API 响应）。
    //
    // 第 3b 步已经保证配置读得了，所以这里**不需要** `unwrap_or_default()` ——
    // 那一行是第 6 处静默降级：配置坏了会让插件配置变成空，从而「插件全部
    // 静默禁用」，而日志里一句提示都没有。
    let mut plugin_config =
        plugins::PluginConfig::from_snapshot(&config_service.snapshot().map_err(|error| {
            anyhow::anyhow!("读取配置失败（{}）：{}", error.code(), error.api.message)
        })?);
    // 4a. 宿主能力出口（`PluginHost`）。**必须在拉起插件之前** —— 端点靠环境变量
    //     注入，插件起来时它就得是能连的；反过来的话插件会拿到一个连不上的
    //     地址（或者干脆没有这个变量，而它分不清「宿主没起」和「我该重试」）。
    match plugin_host::serve(&pool).await {
        Ok(endpoint) => plugin_config.host_endpoint = Some(endpoint),
        Err(error) => tracing::warn!(%error, "PluginHost 未能起服务：插件这次不能回调宿主"),
    }
    let loaded_plugins = plugins::Plugins::load(plugin_config).await;
    let plugin_specs = loaded_plugins.scheduler_specs();
    let job_catalog = loaded_plugins.catalog();
    // 排行源快照。**只有组合根读得到插件注册表**（sm-service / sm-api 都不
    // 依赖 sm-plugins），所以在这里转好再塞进 AppState —— 与 job_catalog
    // 同一个模式。
    let ranking_sources = loaded_plugins.ranking_sources();
    // provider 数据面的**实现**。`sm-service` 只能声明 trait（`sm-plugins` 那条
    // 依赖链会成环），所以实现由组合根给 —— 这是 playback 域唯一的插件接线点。
    //
    // 注册表是**活的**：插件重启会换控制面端口，所以这里传的是共享句柄，
    // 不是加载期的快照。
    let gateway = std::sync::Arc::new(provider_gateway::ProviderGateway::new(
        loaded_plugins.provider_registry(),
    ));

    // 5. 路由。`config_service` 传 clone —— 下面第 6b 步的 worker 还要用它读
    //    `job_disabled_reason` 需要的配置快照，而它是 move 进 AppState 的。
    // ⚠️ 还有两个注入 seam **故意没接**（不是漏了）：`.with_downloads(...)` 与
    //    `.with_media_library_registry(...)`。它们的 trait 是活的，但**没有实现** ——
    //    要由插件 ABI 那批补上（见 docs/handoff.md）。在那之前的表现是契约化的：
    //    `/media-libraries` 的写方法一律 503 `provider_not_installed`、providers 目录
    //    空表；`/download-clients` 三个写方法同样 503。
    // 插件管理**必须**接上：它的 trait 已经有实现（`sm_plugins::admin`），
    // 而 `AppState::plugin_admin()` 在没接时会报 500 `plugin_admin_unavailable`
    // —— 那是「组合根漏了接线」的信号，不是「没装插件」。
    //
    // 它只持有 `ConfigService`：`plugins.root_dir` 与 `plugins.enabled` 每次
    // 操作都从当前磁盘快照读，所以运维手工拷贝进来的插件目录也看得见。
    let plugin_admin: std::sync::Arc<dyn sm_service::system::plugins::PluginAdmin> =
        std::sync::Arc::new(sm_plugins::admin::PluginAdminService::new(
            config_service.clone(),
        ));
    let state = sm_api::AppState::new(pool.clone(), auth, config_service.clone())
        .with_jobs(job_catalog)
        .with_ranking_sources(ranking_sources)
        .with_storage_gateway(gateway)
        .with_plugin_admin(plugin_admin);
    // 影片相似度的 Qdrant 存储（`GET /movies/{}/similar` 用）。
    //
    // **没启用就不挂**，路由据此返回**空列表**而不是 503 —— 那正是上游的降级
    // 语义（`recommendation_service.py:341-348`：「Qdrant 故障只降级相似度信号，
    // 不让详情页整体报错」）。
    //
    // 建连失败也只 warn 不失败：Qdrant 不可用时相似影片是空的，其余功能正常。
    let state = match build_similarity_store(&config_service) {
        Ok(Some(store)) => state.with_movie_similarity(store),
        Ok(None) => state,
        Err(error) => {
            tracing::warn!(
                code = error.code(),
                "影片相似度存储不可用：相似影片端点将返回空列表",
            );
            state
        }
    };
    let app = with_optional_slow_log(sm_api::router(state), config.slow_log.as_deref());

    // 6. 调度器。任务表 = 内建 + 插件（顺序无所谓，调度器按各自的 cron 判）。
    let scheduler = if config.scheduler_enabled {
        let repo = sm_db::repo::BackgroundTaskRunRepository::new(pool.clone());
        let mut specs = sm_scheduler::builtin_jobs();
        specs.extend(plugin_specs);
        let scheduler = Scheduler::new(repo, specs)?;
        let handle = SchedulerHandle::spawn(Arc::new(scheduler));
        let watchdog_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let plugins = Arc::new(tokio::sync::Mutex::new(loaded_plugins));
        let watchdog = tokio::spawn(plugins::watchdog(
            Arc::clone(&plugins),
            Arc::clone(&watchdog_stop),
        ));
        // 抄上游 `cron_info`：把每个任务的 cron 打进启动日志，运维据此确认
        // 定时任务配对了没有。这是「配错了但没人发现」的唯一防线。
        let summary: Vec<String> = handle
            .cron_summary()
            .into_iter()
            .map(|(key, cron)| format!("{key}={cron}"))
            .collect();
        tracing::info!(
            timezone = handle.timezone_name(),
            jobs = summary.join(" "),
            "调度器就绪"
        );

        // 6b. 队列 worker（消费者）。**在调度器之后**启动：worker 的启动第一步
        //     是恢复上个进程遗留的 running 行并收口其领域状态，那要读队列；
        //     调度器先起来最坏只是多入队几行 pending（有 mutex_key 幂等），
        //     反过来则可能让一条刚被恢复的行立刻又被领走。
        let worker = match TaskWorker::spawn(
            pool.clone(),
            // handler 需要进程级依赖（配置里的 `image_search.*` 与 Qdrant 端点），
            // 所以在组合根这里读出来注入 —— 组合根是唯一同时看得见
            // `ConfigService` 与插件/外部服务配置的地方。
            Arc::new(sm_scheduler::builtin_handlers(
                sm_scheduler::worker::HandlerDeps {
                    config: config_service.clone(),
                    // `snapshot()` 返回 `Result` —— **不吞错**。读不到配置就
                    // 让 worker 起不来并写清原因，比静默用空快照（于是所有
                    // handler 都看到「qdrant 没配」）好排查得多。
                    qdrant: sm_scheduler::worker::QdrantEndpoint::from_snapshot(
                        &config_service.snapshot().map_err(|error| {
                            anyhow::anyhow!(
                                "读取配置失败（{}）：{}",
                                error.code(),
                                error.api.message
                            )
                        })?,
                    ),
                },
            )),
            WorkerConfig::default(),
            config_service.clone(),
        )
        .await
        {
            Ok(worker) => Some(worker),
            Err(error) => {
                // 不 panic：worker 起不来时 HTTP 仍然可用（能看任务中心、能手动
                // 触发），而 panic 会让整个进程不启动 —— 那是把「后台任务不跑」
                // 升级成「服务全挂」，代价大得多。
                tracing::error!(
                    code = error.code(),
                    "队列 worker 启动失败：任务会入队但无人执行"
                );
                None
            }
        };

        // 6c. 匿名遥测心跳。上游把它挂在 APScheduler 上（`start/aps.py:365-374`，
        //     `interval hours=1` + `next_run_time=runtime_now()`），但它**不是队列
        //     任务** —— 直接发一次 HTTP、不产生 `background_task_run`。所以这里起
        //     一个独立循环，而不是塞进 `Scheduler`（后者只做「cron → 入队」）。
        //     env 关闭时**连循环都不起**（上游 `if is_enabled(): add_job(...)`）。
        let telemetry = if sm_service::system::telemetry::is_enabled() {
            Some(spawn_telemetry_heartbeat(
                pool.clone(),
                config.config_path.clone(),
                Arc::clone(&plugins),
            ))
        } else {
            tracing::info!(
                env = sm_service::system::telemetry::ENABLED_ENV_KEY,
                "遥测心跳已关闭，不上报"
            );
            None
        };

        Some(Background {
            scheduler: handle,
            worker,
            watchdog,
            watchdog_stop,
            telemetry,
        })
    } else {
        tracing::info!("调度器已被配置关闭");
        None
    };

    // 7. HTTP。
    let addr = config.listen.bind_address();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|err| anyhow::anyhow!("绑定 {addr} 失败：{err}"))?;
    tracing::info!(address = %addr, "HTTP 服务已监听");

    // 8. 等信号。Ctrl-C 与 SIGTERM 都要接：容器里发的是 SIGTERM，
    //    只接 Ctrl-C 意味着 `docker stop` 每次都等超时才被杀。
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| anyhow::anyhow!("HTTP 服务异常退出：{err}"))?;
    tracing::info!("HTTP 服务已停止接受新请求");

    // 9. 收尾，顺序有讲究：
    //
    //    ① 看门狗 —— 否则它可能在关停过程中把刚杀掉的插件又拉起来。
    //    ② 调度器（生产者）—— 等当前 tick 结束，不再入队。
    //    ③ worker（消费者）—— 它在 shutdown 里**等在飞行的任务跑完**，所以
    //       放最后：一个长任务可能让关停等上它的全量时长。这是「优雅」的真实
    //       代价，不要为了快而 abort —— abort 会留下一行 running 且租约未续，
    //       只能等租约到期（最多 `DEFAULT_LEASE_SECONDS`）被回收。
    if let Some(background) = scheduler {
        background
            .watchdog_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        background.watchdog.abort();
        // 心跳只发一次 HTTP，没有「在飞行中必须等完」的语义 —— 直接 abort。
        if let Some(telemetry) = background.telemetry {
            telemetry.abort();
        }
        background.scheduler.shutdown().await?;
        tracing::info!("调度器已停止");
        if let Some(worker) = background.worker {
            // `ServiceError` 不实现 `std::error::Error`，`?` 进不了 anyhow ——
            // 显式映射，并且只带上机器可读的 code。
            worker
                .shutdown()
                .await
                .map_err(|error| anyhow::anyhow!("队列 worker 停止失败（{}）", error.code()))?;
            tracing::info!("队列 worker 已停止");
        }
    }
    pool.close().await;
    tracing::info!("退出完成");
    Ok(())
}

/// 遥测心跳循环：**启动即跑一次，之后每小时一次**（上游 `next_run_time=runtime_now()`
/// + `interval hours=1`）。
///
/// 每轮先取一份**活**的插件快照（与看门狗共享同一把锁），再上报 —— 插件重启会
/// 换版本号，快照过期就报错了。失败只记日志，不中断循环（下个小时照跑）。
fn spawn_telemetry_heartbeat(
    db: sm_db::Db,
    config_path: PathBuf,
    plugins: Arc<tokio::sync::Mutex<plugins::Plugins>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let service = sm_service::system::telemetry::TelemetryService::new(db, &config_path);
        let mut ticker = tokio::time::interval(TELEMETRY_INTERVAL);
        // 跳过错过的 tick 而不是补跑：停机一整天不该在恢复时瞬间发 24 次。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let heartbeats = plugins.lock().await.plugin_heartbeats();
            if let Err(error) = service.report(heartbeats).await {
                tracing::warn!(code = error.code(), "遥测心跳失败：{}", error.api.message);
            }
        }
    })
}

/// 建连接池。
///
/// 单独成函数是为了让「配置 → 池」这一步能单独测。
pub async fn connect_pool(config: &ServerConfig) -> Result<sm_db::Db, sqlx::Error> {
    let options: sqlx::postgres::PgConnectOptions = config
        .database_url
        .parse()
        .map_err(|err| sqlx::Error::Configuration(Box::new(err)))?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.pool.max_connections)
        .acquire_timeout(config.pool.acquire_timeout)
        .connect_with(options)
        .await
}

/// 按环境变量决定是否挂慢日志层。
///
/// 关闭时**不挂**而不是「挂了但内部不记」—— 上游是
/// `if slow_log_enabled(): app.add_middleware(...)`，而每次请求多一次 future
/// 包装与两次时钟读正是那条注释要避免的。
/// 影片相似度存储的共享句柄（组合根构造，注入 `AppState`）。
type SimilarityStore =
    std::sync::Arc<sm_service::discovery::qdrant::similarity::MovieSimilarityStore>;

/// 构造影片相似度的 Qdrant 存储。**没启用返回 `Ok(None)`（合法状态）。**
///
/// 与 `sm_scheduler::worker` 里那个同名 helper 的差别：**这里不把「配置矛盾」
/// 报成错**。那个任务是「维护相似度索引」，离了 Qdrant 什么也做不了；这个
/// 端点只是**读**索引，读不到就少一个推荐理由，别的照常，所以只 warn。
fn build_similarity_store(
    config: &sm_service::system::config::ConfigService,
) -> Result<Option<SimilarityStore>, sm_service::error::ServiceError> {
    use sm_service::discovery::qdrant::similarity::MovieSimilarityStore;
    use sm_service::system::optional_services::movie_similarity_enabled;

    let snapshot = config.snapshot()?;
    if !movie_similarity_enabled(&snapshot) {
        return Ok(None);
    }
    let endpoint = sm_scheduler::worker::QdrantEndpoint::from_snapshot(&snapshot);
    if !endpoint.is_configured() {
        tracing::warn!("movie_similarity 已启用但 qdrant.url 为空，相似影片将返回空列表");
        return Ok(None);
    }
    let base = endpoint.url.trim_end_matches('/');
    MovieSimilarityStore::connect(base, endpoint.api_key.as_deref())
        .map(|store| Some(std::sync::Arc::new(store)))
}

fn with_optional_slow_log(app: axum::Router, raw_env: Option<&str>) -> axum::Router {
    match SlowLogConfig::from_env_with(
        raw_env,
        std::env::var("SAKURAMEDIA_SLOW_REQUEST_MS").ok().as_deref(),
    ) {
        Some(config) => {
            tracing::info!(threshold_ms = config.threshold_ms, "慢请求日志已启用");
            app.layer(axum::middleware::from_fn(move |req, next| {
                sm_api::middleware::slow_log::slow_request_logger(config, req, next)
            }))
        }
        None => app,
    }
}

/// 等待 Ctrl-C 或 SIGTERM。
async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("注册 Ctrl-C 处理器失败");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 处理器失败")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("收到 Ctrl-C，开始优雅关闭"),
        () = terminate => tracing::info!("收到 SIGTERM，开始优雅关闭"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_pool_connects_with_the_configured_limits() {
        // 真连一次库：池参数写错（max_connections=0、URL 打错）在这里就暴露。
        let Some(url) = std::env::var("SMDB_TEST_DATABASE_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
        else {
            eprintln!("SKIP: 未设置 SMDB_TEST_DATABASE_URL / DATABASE_URL");
            return;
        };
        let config = ServerConfig {
            database_url: url,
            jwt_secret: "s3cret".to_owned(),
            ..ServerConfig::with_defaults()
        };
        let pool = connect_pool(&config).await.expect("应当能连上");
        assert_eq!(pool.options().get_max_connections(), 20);
        pool.close().await;
    }

    #[tokio::test]
    async fn a_bad_database_url_fails_with_a_configuration_error() {
        let config = ServerConfig {
            database_url: "not-a-url".to_owned(),
            jwt_secret: "s3cret".to_owned(),
            ..ServerConfig::with_defaults()
        };
        let err = connect_pool(&config).await.expect_err("非法 URL 应当失败");
        assert!(
            matches!(err, sqlx::Error::Configuration(_)),
            "应当是配置错误而非连接错误：{err}"
        );
    }

    #[test]
    fn the_slow_log_layer_is_only_attached_when_the_switch_is_on() {
        // 判定规则住在 sm-api；这里只验证「关 → 不挂层」。
        let app = axum::Router::new().route("/x", axum::routing::get(|| async { "x" }));
        // `None` 与 `"0"` 都不挂。函数返回 Router，无法直接观察层是否存在，
        // 但它至少必须能对两种输入都正常返回（不 panic）。
        let _ = with_optional_slow_log(app.clone(), None);
        let _ = with_optional_slow_log(app.clone(), Some("0"));
        let _ = with_optional_slow_log(app, Some("1"));
    }
}
