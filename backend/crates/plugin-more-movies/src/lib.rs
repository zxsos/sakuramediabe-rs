//! 更多影片 / 更多影片榜单插件的 Rust 实现。
//!
//! # 上游对应
//!
//! 两个 Python 插件合在一个仓库（抓取与解析代码共用，各自独立的二进制与
//! `plugin_id`）：
//!
//! | 上游 | 这里 |
//! |---|----|
//! | `sakuramedia_more_movies/plugin.py:register` | 二进制 `sakuramedia_more_movies` 的 [`service::MoreMoviesControl`]（`register`） |
//! | `sakuramedia_more_movies/sync.py:run_sync` | [`service::MoreMoviesControl`] 的 `run_job`（`sakuramedia_more_movies_sync`） |
//! | `sakuramedia_more_movies/javdb.py:fetch_latest_page` | [`javdb::fetch_latest_page`] |
//! | `sakuramedia_more_movies/heat.py:calculate_heat` | [`heat::calculate_heat`] |
//! | `sakuramedia_more_movies/settings.py:MoreMoviesSettings` | [`settings::MoreMoviesSettings`] |
//! | `sakuramedia_more_rank_movies/plugin.py:register` | 二进制 `sakuramedia_more_rank_movies` 的 [`service::RankMoviesControl`]（`register`） |
//! | `sakuramedia_more_rank_movies/boards.py:build_ranking_sources` | [`service::ranking_sources`] |
//! | `sakuramedia_more_rank_movies/minnano.py` | [`minnano`] |
//! | `sakuramedia_more_rank_movies/javlibrary.py` | [`javlibrary`] |
//! | `sakuramedia_more_rank_movies/sync_jobs.py:build_jobs` | [`service::RankMoviesControl`] 的 `run_job`（两个 task_key） |
//!
//! # 扩展点与任务
//!
//! - `sakuramedia_more_movies`：只有后台任务（`sakuramedia_more_movies_sync`，
//!   cron `0 6 * * *`），不声明扩展点。
//! - `sakuramedia_more_rank_movies`：声明 `discovery.ranking_source` 扩展点
//!   （Minnano AV 1 个榜单 + JavLibrary 2 个榜单）与两个任务 —— 定时全量
//!   （`sakuramedia_more_rank_movies_sync`）与手动单榜
//!   （`sakuramedia_more_rank_movies_sync_board`，吃 `board_key` +
//!   可选 `period`）。
//!
//! # 与上游不同的三处
//!
//! 1. **JavDB 列表走插件自己的 HTTP**，不是宿主的 `JavdbProvider`。Rust 契约
//!    （v0.2.0）没有「任意 JavDB 请求」的宿主 RPC，只有榜单查询；签名算法
//!    （`jdsignature`）是公开的（上游 `javdb.py:_get_sign`），插件自己实现。
//! 2. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`
//!    —— 进程内插件才有后者那条通道（与 `plugin-javbus-metadata` 同一理由）。
//! 3. **回调用宿主 `SAKURAMEDIA_HOST_GRPC_ADDR`**：查重走
//!    `PluginHost::FindMoviesByNumbers`，入库走 `PluginHost::ImportMovieByNumber`，
//!    榜单同步走 `PluginHost::SyncRankingSources` / `SyncRankingBoard`。

pub mod heat;
pub mod html;
pub mod javdb;
pub mod javlibrary;
pub mod minnano;
pub mod service;
pub mod settings;

/// 上游 `manifest.json` 的 `plugin_id`（更多影片）。
pub const MORE_MOVIES_PLUGIN_ID: &str = "sakuramedia_more_movies";

/// 上游 `manifest.json` 的 `plugin_id`（更多影片榜单）。
pub const RANK_MOVIES_PLUGIN_ID: &str = "sakuramedia_more_rank_movies";

/// `discovery.ranking_source` 扩展点 key（上游
/// `src/plugins/extensions/ranking.py:RANKING_SOURCE_EXTENSION_KEY`）。
pub const RANKING_SOURCE_KEY: &str = "discovery.ranking_source";

/// 起 `sakuramedia_more_movies`（只控制面：`register` + 一个任务）。
///
/// **进程式与进程内共用同一份装配**：`settings` 从 `Value` 解析、宿主端点显式
/// 传入，构造时定死，任务里不再读进程环境。
pub async fn serve_more_movies(
    addr: std::net::SocketAddr,
    plugin_id: String,
    settings: serde_json::Value,
    host_endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
    use tonic::transport::Server;

    let settings = settings::MoreMoviesSettings::from_json(&settings);
    let control = service::MoreMoviesControl::new(plugin_id, settings, host_endpoint)?;
    Server::builder()
        .add_service(PluginControlServer::new(control))
        .serve(addr)
        .await?;
    Ok(())
}

/// 起 `sakuramedia_more_rank_movies`（控制面 + `discovery.ranking_source`
/// 扩展点，同一个端口 —— 理由见 `service.rs` 的模块文档）。
pub async fn serve_rank_movies(
    addr: std::net::SocketAddr,
    plugin_id: String,
    settings: serde_json::Value,
    host_endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
    use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionServiceServer;
    use tonic::transport::Server;

    let settings = settings::RankMoviesSettings::from_json(&settings);
    let ranking = service::RankingService::new(settings.clone());
    let control = service::RankMoviesControl::new(plugin_id, settings, host_endpoint);
    Server::builder()
        .add_service(PluginControlServer::new(control))
        .add_service(RankingSourceExtensionServiceServer::new(ranking))
        .serve(addr)
        .await?;
    Ok(())
}
