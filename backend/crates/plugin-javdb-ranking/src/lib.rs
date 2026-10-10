//! JavDB 排行榜插件：`discovery.ranking_source` 扩展点的 Rust 实现。
//!
//! # 上游对应
//!
//! `upstream/sakuramedia_javdb_ranking/`（Python）：`plugin.py` 注册、
//! `javdb.py` 榜单抓取与解析、`settings.py` 配置、`manifest.json`。
//!
//! | 上游 | 这里 |
//! |---|
//! | `plugin.py:register` | [`service::Control`]（`register`） |
//! | `javdb.py:fetch_ranking` | [`javdb::JavDbSource::fetch_ranking`] |
//! | `javdb.py` 榜单页解析 | [`javdb::parse_ranking_page`] |
//! | `settings.py:Settings` | [`settings::Settings`] |
//!
//! # 榜单
//!
//! | board_key | 显示名 | 上游路径 |
//! |---|---|---|
//! | `hot` | 热播 | `/`（首页热播） |
//! | `top_rated` | 高评分 | `/rankings/movies?p=1&m=…` |
//! | `censored` | 有码 | `/rankings/movies?m=censored` |
//! | `uncensored` | 无码 | `/rankings/movies?m=uncensored` |
//! | `fc2` | FC2 | `/rankings/movies?m=fc2` |
//! | `top250` | TOP250 | `/rankings/movies/top250` |
//!
//! # 与上游不同的两处
//!
//! 1. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`
//!    —— 进程内插件才有后者那条通道。
//! 2. **定时任务由宿主调度**：本插件在 `register` 里声明 `JobDefinition`，
//!    宿主按 cron 拉起 `run_job`；插件自己不带 cron 库、不起后台线程。

pub mod javdb;
pub mod service;
pub mod settings;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_javdb_ranking";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "JavDB 排行榜";

/// `discovery.ranking_source` 扩展点 key。
///
/// 与宿主侧是同一个字面量，但插件不依赖宿主实现，所以各持一份 —— 它是
/// proto 里写死的协议常量。
pub const RANKING_SOURCE_KEY: &str = "discovery.ranking_source";

/// 排行榜来源的全局唯一 key。
pub const SOURCE_KEY: &str = "javdb";
