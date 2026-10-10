//! SubtitleCat 中文字幕插件：按番号抓取 `zh-CN` 字幕的 Rust 实现。
//!
//! # 上游对应
//!
//! `tinypinglite/sakuramedia_subtitlecat`（Python）：`plugin.py` 注册、
//! `subtitlecat.py` 搜索与下载、`settings.py` 配置、`jobs.py` 手动/订阅任务、
//! `state.py` 抓取状态、`manifest.json`。
//!
//! | 上游 | 这里 |
//! |---|
//! | `plugin.py:register` | [`service::Control`]（`register`） |
//! | `subtitlecat.py:SubtitleCatClient.fetch_chinese_subtitles` | [`subtitlecat::SubtitleCatClient::fetch_chinese_subtitles`] |
//! | `subtitlecat.py:normalize_movie_number` | [`subtitlecat::normalize_movie_number`] |
//! | `subtitlecat.py:_LinkParser` | [`html`]（本仓库手写的扫描器，见它的模块文档） |
//! | `settings.py:SubtitleCatSettings` | [`settings::Settings`] |
//! | `jobs.py:build_jobs` | [`service::Control::run_job`]（两个 task_key） |
//! | `state.py:SubtitleCatFetchState` | 未移植（见下） |
//!
//! # 与上游不同的地方
//!
//! 1. **`state.py` 未移植**。上游用 SQLite 记「已抓取」避免订阅任务重复抓；
//!    Rust 侧 `RunJob` 是无状态调用，状态应由宿主侧维护（或后续扩展点补）。
//!    当前实现每次调用都实时抓取。
//! 2. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`
//!    —— 进程内插件才有后者那条通道。
//! 3. **字幕导入走宿主 `ImportSubtitle` 回调**：`RunJob` 的结果 `Struct` 里带
//!    回字幕字节（base64），由宿主侧落盘与入库；插件不直接写宿主库表。

pub mod html;
pub mod service;
pub mod settings;
pub mod subtitlecat;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_subtitlecat";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "SakuraMedia SubtitleCat 中文字幕";
