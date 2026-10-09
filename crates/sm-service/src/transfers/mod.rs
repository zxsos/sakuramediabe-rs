//! `transfers` 子域（上游 23 文件 / 4,248 行）。
//!
//! 已落两块，都在「不依赖插件 ABI」的那一侧：
//!
//! | 模块 | 上游 | 说明 |
//! |---|---|---|
//! | [`torznab`] | `downloads/clients/torznab.py` | 跨索引器搜索与候选构造 |
//! | [`download_search`] | `downloads/search_service.py` | `GET /download-candidates` |
//!
//! 其余部分未开工：转存/下载编排（`import_service` / `task_service` /
//! `media_transfer_task_service`）、`downloads` 客户端注册与优先选择。它们
//! 大多落在「插件 ABI 宿主 `sm-plugins`」这条依赖后面 —— 见
//! `docs/service-progress.md` 的阻塞地图。

pub mod download_search;

pub mod torznab;
