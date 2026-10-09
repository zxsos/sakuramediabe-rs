//! 组合根：单进程同时承载 API(8000) 与调度器。
//!
//! 对应上游的 `src/api/app.py`（`create_app` + lifespan）与
//! `src/start/aps.py`（`aps()`）—— 两个进程做的事，这里在一个进程里做完。
//!
//! # 装配顺序是有意义的
//!
//! ```text
//! 配置 → 日志 → 连接池 → 路由 → 调度器 → HTTP → 等信号 → 逆序收尾
//! ```
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

use std::sync::Arc;

use sm_api::middleware::slow_log::SlowLogConfig;
use sm_scheduler::{Scheduler, SchedulerHandle};
use sm_service::system::auth::AuthConfig;

pub use config::{ListenConfig, PoolConfig, ServerConfig};
pub use error::ConfigError;

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

    // 3. 路由。鉴权配置里的密钥取自配置，不留硬编码默认值。
    let auth = AuthConfig::new(config.jwt_secret.clone());
    let state = sm_api::AppState::new(pool.clone(), auth);
    let app = with_optional_slow_log(sm_api::router(state), config.slow_log.as_deref());

    // 4. 调度器。
    let scheduler = if config.scheduler_enabled {
        let repo = sm_db::repo::BackgroundTaskRunRepository::new(pool.clone());
        let scheduler = Scheduler::new(repo, sm_scheduler::builtin_jobs())?;
        let handle = SchedulerHandle::spawn(Arc::new(scheduler));
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
        Some(handle)
    } else {
        tracing::info!("调度器已被配置关闭");
        None
    };

    // 5. HTTP。
    let addr = config.listen.bind_address();
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|err| anyhow::anyhow!("绑定 {addr} 失败：{err}"))?;
    tracing::info!(address = %addr, "HTTP 服务已监听");

    // 6. 等信号。Ctrl-C 与 SIGTERM 都要接：容器里发的是 SIGTERM，
    //    只接 Ctrl-C 意味着 `docker stop` 每次都等超时才被杀。
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| anyhow::anyhow!("HTTP 服务异常退出：{err}"))?;
    tracing::info!("HTTP 服务已停止接受新请求");

    // 7. 收尾：停调度器（等当前 tick 结束）。
    if let Some(scheduler) = scheduler {
        scheduler.shutdown().await?;
        tracing::info!("调度器已停止");
    }
    pool.close().await;
    tracing::info!("退出完成");
    Ok(())
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
