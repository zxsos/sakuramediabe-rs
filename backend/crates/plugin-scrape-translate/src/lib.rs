//! SakuraMedia 影片文案抓取与翻译：后台任务型插件的 Rust 实现。
//!
//! # 上游对应
//!
//! `upstream/sakuramedia_movie_scrape_translate/`（Python）：`plugin.py`
//! 注册、`jobs.py` 任务管线、`dmm.py` DMM 抓取、`translation.py` 翻译客户端、
//! `state.py` SQLite 状态、`settings.py` 配置、`prompts/` 翻译提示词。
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `plugin.py:register` | [`service::Control`]（`register`，声明 3 个 job） |
//! | `jobs.py:build_jobs` | [`service::job_definitions`] |
//! | `jobs.py:run_pipeline` | [`jobs::run_pipeline`] |
//! | `jobs.py:_movie_pages` / `_writable` / `context.movies` | [`service::GrpcMovieStore`]（反向调宿主 `PluginHost`） |
//! | `dmm.py:DmmClient.fetch` | [`dmm::DmmClient::fetch`] |
//! | `dmm.py:_Page` | [`html::DmmPage`]（栈式扫描器，见它的模块文档） |
//! | `translation.py:OpenAITranslationClient` | [`translation::TranslationClient`] |
//! | `translation.py:normalize_translation` | [`translation::normalize_translation`] |
//! | `translation.py:load_prompt` | `prompts/` + `include_str!`（编译期嵌入） |
//! | `state.py:DmmState` | [`state::DmmState`] |
//! | `settings.py:DmmSettings` | [`settings::Settings`] |
//!
//! # 与上游不同的地方
//!
//! 1. **任务是 gRPC 流式**，不是 Python 的 `JobDefinition(handler=...)` 回调：
//!    宿主调 `PluginControl.RunJob`，插件用 `stream JobEvent` 回进度，终态
//!    摘要放在最后一个 `JobEvent.result` 里。
//! 2. **影片存取反向调宿主**，不是进程内 `context.movies`：`jobs` 模块把
//!    「取数 / 翻译 / 状态」做实，影片的列举 / 读取 / 写回由
//!    [`service::GrpcMovieStore`] 打到宿主的 `PluginHost`
//!    （`FindMoviesByNumbers` / `ListMovies` / `GetMovie` / `PatchMovie`）。
//! 3. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`
//!    —— 进程内插件才有后者那条通道。
//! 4. **提示词编译期嵌入**：上游运行时读 `prompts/*.md`；这里
//!    `include_str!` 进二进制，少一次文件依赖，宿主也不用再拷 prompts 目录。

pub mod dmm;
pub mod html;
pub mod jobs;
pub mod service;
pub mod settings;
pub mod state;
pub mod translation;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_movie_scrape_translate";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "SakuraMedia 影片文案抓取与翻译";

/// 把本插件的全部 gRPC service 起在 `addr` 上。
///
/// **进程式与进程内共用同一份装配**：本插件是纯任务型，只有控制面
/// （`PluginControl`：`register` + 三个任务），不声明扩展点。`settings` 从
/// `Value` 解析成 [`settings::Settings`]，连 `host_endpoint` 一起交给
/// [`service::Control::with_runtime`] —— 进程内多插件共用一份进程环境，从
/// `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读会互相覆盖。
///
/// `host_endpoint` 本插件**用得着**：管线的影片列举 / 读取 / 写回都要反向调
/// 宿主的 `PluginHost`（见 `service.rs` 的模块文档）。
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
