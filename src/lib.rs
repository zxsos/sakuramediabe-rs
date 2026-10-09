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
//! | `jobs.py:build_jobs` | [`service::JOB_DEFINITIONS`] |
//! | `jobs.py:run_pipeline` | [`jobs::run_pipeline`] |
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
//! 2. **影片列表与写回走宿主扩展点**，不是 `context.movies`：进程拆分后没有
//!    进程内宿主对象；`jobs` 模块把「取数 / 翻译 / 状态」做实，影片的列举与
//!    写回由宿主在任务编排层完成（`MovieStore` trait 标出边界）。
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
