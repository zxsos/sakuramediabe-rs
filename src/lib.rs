//! 合集判定插件：按时长 / 番号特征 / 标签自动标记合集影片的 Rust 实现。
//!
//! # 上游对应
//!
//! `tinypinglite/sakuramedia_judge_collecttion_movie`（Python）：`plugin.py`
//! 注册与判定主循环、`settings.py` 配置。
//!
//! | 上游 | 这里 |
//! |---|
//! | `plugin.py:register` | [`service::Control`]（`register`，声明 `JobDefinition`） |
//! | `plugin.py:judge_movies` | [`service::Control`]（`run_job`）+ [`judge`] |
//! | `plugin.py:_normalize_movie_number` | [`judge::normalize_movie_number`] |
//! | `settings.py:DurationCollectionSettings` | [`settings::DurationCollectionSettings`] |
//!
//! # 与上游不同的两处
//!
//! 1. **宿主调用面是 gRPC**。上游是进程内插件，直接拿 `PluginContext`
//!    调 `movies.list_page` / `movies.patch`。这里走契约仓的 `PluginHost`
//!    服务（`ListMovies` / `PatchMovie`），由 [`host::HostMovies`] 抽象，
//!    `run_job` 里用 tonic 客户端实现。
//! 2. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是
//!    `context.settings` —— 进程外插件没有那条通道（与
//!    `sakuramedia-javbus-metadata` 同一个理由）。
//!
//! # owner 约定
//!
//! 契约里 `MovieSnapshot.owners` 是 `repeated string`，Python 侧是
//! `dict[field, owner]`。本插件按 `"<field>=<owner>"` 解析（也接受
//! `"<field>:<owner>"`，取第一段分隔符切分），只认 `is_collection`
//! 这一项 —— 与上游 `snapshot.owners.get("is_collection")` 同语义。

pub mod host;
pub mod judge;
pub mod service;
pub mod settings;

/// 上游 `manifest.json` 的 `plugin_id`。**拼写沿用上游**（`collecttion`
/// 双写 t），宿主按 `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行
/// 文件，所以它也决定了目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_judge_collecttion_movie";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "按时长/番号特征/标签判定合集影片";
