//! SakuraMedia 女优资料补全插件：后台任务的 Rust 实现。
//!
//! # 上游对应
//!
//! `tinypinglite/sakuramedia-actor-metadata`（Python）：`plugin.py` 注册、
//! `jobs.py` 任务主循环、`sources.py` 抓取与解析、`state.py` 重试状态、
//! `settings.py` 配置、`manifest.json`。
//!
//! | 上游 | 这里 |
//! |---|---
//! | `plugin.py:register` | [`service::Control`]（`register`，声明 `JobDefinition`） |
//! | `jobs.py:run` / `process` | [`service`] / [`jobs::process`] |
//! | `sources.py:Sources` | [`sources::Sources`]（trait 化为 [`jobs::ProfileSource`]） |
//! | `sources.py:parse_minnanoav` | [`sources::parse_minnanoav`] |
//! | `sources.py:normalize_fields` | [`sources::normalize_fields`] |
//! | `state.py:State` | [`state::State`]（rusqlite） |
//! | `settings.py:Settings` | [`settings::Settings`] |
//! | `bs4.BeautifulSoup` | [`html`]（本仓库手写的扫描器 + 迷你 DOM） |
//!
//! # 三处与上游不同
//!
//! 1. **宿主调用走 gRPC**。上游是进程内 `context.actors` / `context.movies`；
//!    这里任务通过 `SAKURAMEDIA_HOST_GRPC_ADDR` 连回宿主的 `PluginHost`
//!    （见 [`service`]）。
//! 2. **`writable_fields` 的归属检查读 `field_owners`**（见 [`jobs`] 模块文档）：
//!    `ActorSnapshot.field_owners` 把「字段 → owner」的映射带出来了，照上游
//!    `snapshot.owners.get(key)` 的语义判归属（缺键 = 无人接管 = 可写）。
//! 3. **MinnanoAV 的站内校验按配置的基址走**（见 [`sources`] 模块文档）：
//!    测试时可指向本地假服务。

pub mod html;
pub mod jobs;
pub mod service;
pub mod settings;
pub mod sources;
pub mod state;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_actor_metadata";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "SakuraMedia 女优资料补全";

/// 把本插件的全部 gRPC service 起在 `addr` 上。
///
/// **进程式与进程内共用同一份装配**（与 `plugin-javbus-metadata` 的 `serve`
/// 同形）：
///
/// - **进程式**：可执行文件（`src/bin/sakuramedia_actor_metadata.rs`）从
///   `SAKURAMEDIA_PLUGIN_*` 环境变量取地址 / id / 配置文件，解析成 `settings`
///   后调这里；
/// - **进程内**：组合根（`sm-server`）直接把 `settings` 与 `host_endpoint`
///   传进来，在同一进程里起一个 loopback 服务 —— 不起子进程，省掉一整套运行时。
///
/// `host_endpoint` 本插件**用得着**：任务要反向回调宿主的 `PluginHost`
/// （`ListActors` / `PatchActor` / ...），见 [`service::Control::with_runtime`]。
pub async fn serve(
    addr: std::net::SocketAddr,
    plugin_id: String,
    settings: serde_json::Value,
    host_endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
    use tonic::transport::Server;

    let settings = settings::Settings::from_json(&settings);
    Server::builder()
        .add_service(PluginControlServer::new(service::Control::with_runtime(
            plugin_id,
            settings,
            host_endpoint,
        )))
        .serve(addr)
        .await?;
    Ok(())
}
