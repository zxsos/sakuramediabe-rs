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
//! 2. **`writable_fields` 的归属检查放宽**（见 [`jobs`] 模块文档）：Rust 契约
//!    只给出去重后的 owner 列表，字段级归属拿不到；只判「值为空」。
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
