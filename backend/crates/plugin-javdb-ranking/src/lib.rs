//! JavDB 排行榜插件：`discovery.ranking_source` 扩展点的 Rust 实现。
//!
//! # 上游对应
//!
//! `sakuramedia_javdb_ranking`（Python，0.1.4）：
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `plugin.py:register` | [`service::Control`]（`register`） |
//! | `boards.py:build_ranking_source` | [`boards`]（榜单声明 + 取数映射） |
//! | `sync_jobs.py:build_jobs` | [`service::Control::jobs`] |
//! | `context.sync_ranking_*` | 宿主的 `SyncRankingSources` / `SyncRankingBoard` rpc |
//! | `context.build_javdb_provider` | 宿主的 `GetJavdbRankNumbers` rpc |
//! | `settings.py:JavdbRankingSettings` | [`settings::Settings`] |
//!
//! # 榜单抓取在**宿主**那一侧
//!
//! 本插件不持有 JavDB 客户端：出网细节（UA、签名头、代理策略）与登录态归宿主
//! 一处管，插件只把账号透传进去（`host.proto` 的 `GetJavdbRankNumbers` 原话）。
//! 本仓早期这里有一份自己抓榜单页的 `javdb.rs` —— 那条路已删除：它与
//! 「排行同步由宿主编排」的分工矛盾，而且插件自己抓会让 UA / 账号这类东西
//! 出现两份实现。
//!
//! # 与上游不同的两处（都是形态差异，不是行为差异）
//!
//! 1. **配置从参数/文件读**：进程内由组合根把 `plugins.<id>.settings` 直接传进
//!    [`serve`]；进程式由可执行文件读 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`。
//! 2. **定时任务由宿主调度**：本插件在 `register` 里声明 `JobDefinition`，
//!    宿主按 cron 拉起 `run_job`；插件自己不带 cron 库、不起后台线程。

pub mod boards;
pub mod service;
pub mod settings;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_javdb_ranking";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "SakuraMedia JavDB 排行榜";

/// `discovery.ranking_source` 扩展点 key。
///
/// 与宿主侧是同一个字面量，但插件不依赖宿主实现，所以各持一份 —— 它是
/// proto 里写死的协议常量。
pub const RANKING_SOURCE_KEY: &str = "discovery.ranking_source";

/// 排行榜来源的全局唯一 key。
pub const SOURCE_KEY: &str = "javdb";

/// 把本插件的全部 gRPC service 起在 `addr` 上。
///
/// **进程式与进程内共用同一份装配**（与 `plugin-javbus-metadata::serve` 同形）：
///
/// - **进程式**：可执行文件从 `SAKURAMEDIA_PLUGIN_*` 环境变量取地址 / id /
///   配置文件，解析后调这里；
/// - **进程内**：组合根（`sm-server`）直接传 `settings` 与 `host_endpoint`，
///   在同一进程里起一个 loopback 服务，不起子进程。
///
/// `host_endpoint` 是**本插件自己的**宿主端点：两个同步任务与 `FetchRanking`
/// 都要用它连回宿主的 `PluginHost`。没有它就按 `failed_precondition` 失败 ——
/// 静默跳过会让任务「看起来跑了但什么都没干」。
///
/// ⚠️ **注册阶段不联网**（proto 原话）：这里只装配声明，宿主客户端在每次任务 /
/// 取数时才连。
pub async fn serve(
    addr: std::net::SocketAddr,
    plugin_id: String,
    settings: serde_json::Value,
    host_endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
    use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionServiceServer;
    use tonic::transport::Server;

    let settings = settings::Settings::from_json(&settings);
    Server::builder()
        .add_service(PluginControlServer::new(service::Control::with_runtime(
            plugin_id,
            host_endpoint.clone(),
        )))
        .add_service(RankingSourceExtensionServiceServer::new(
            service::Ranking::with_runtime(settings, host_endpoint),
        ))
        .serve(addr)
        .await?;
    Ok(())
}
