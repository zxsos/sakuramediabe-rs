//! SubtitleCat 中文字幕插件：按番号抓取 `zh-CN` 字幕的 Rust 实现。
//!
//! # 上游对应
//!
//! `tinypinglite/sakuramedia_subtitlecat`（Python）：
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `plugin.py:register` | [`service::Control`]（`register`） |
//! | `subtitlecat.py:SubtitleCatClient.fetch_chinese_subtitles` | [`subtitlecat::SubtitleCatClient::fetch_chinese_subtitles`] |
//! | `subtitlecat.py:normalize_movie_number` | [`subtitlecat::normalize_movie_number`] |
//! | `subtitlecat.py:_LinkParser` | [`html`]（本仓库手写的扫描器，见它的模块文档） |
//! | `settings.py:SubtitleCatSettings` | [`settings::Settings`] |
//! | `jobs.py` | [`jobs`] |
//! | `state.py:SubtitleCatFetchState` | [`state::FetchState`] |
//! | `manifest.json` | [`PLUGIN_ID`] / [`DISPLAY_NAME`] |
//!
//! # 宿主能力从哪来
//!
//! 上游是进程内插件，`context.movies` / `context.import_subtitle` /
//! `context.data_dir` 直接拿。拆成 gRPC 插件后：
//!
//! - `context.movies.*` / `context.import_subtitle` → `PluginHost` 的
//!   `FindMoviesByNumbers` / `ListMovies` / `ImportSubtitle`，
//!   端点由宿主给（进程式走 `SAKURAMEDIA_HOST_GRPC_ADDR`，进程内由组合根传）；
//! - `context.data_dir` → `RunJobRequest.data_dir`（宿主保证可读写、重装保留）
//!   —— 抓取状态文件就落在那里。
//!
//! # 与上游不同的地方（汇总，细则在各模块文档里）
//!
//! 1. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`
//!    —— 进程内插件才有后者那条通道。
//! 2. **多一个 `base_url` 配置**（上游写死在代码里），供测试与镜像站用。
//! 3. **进度事件不带 `summary_patch`**：proto 的 `ProgressEvent` 只有
//!    `current / total / text`。

pub mod html;
pub mod jobs;
pub mod service;
pub mod settings;
pub mod state;
pub mod subtitlecat;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_subtitlecat";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "SakuraMedia SubtitleCat 中文字幕";

/// 把本插件的全部 gRPC service 起在 `addr` 上。
///
/// **进程式与进程内共用同一份装配**：本插件只有控制面（`PluginControl`：
/// `register` + 两个任务），不声明扩展点。`settings` 从 `Value` 解析成
/// [`settings::Settings`]，连 `host_endpoint` 一起交给
/// [`service::Control::with_runtime`] —— 进程内多插件共用一份进程环境，从
/// `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读会互相覆盖。
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
