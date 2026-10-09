//! `catalog` 子域。
//!
//! 上游 `src/service/catalog/` 有 27 个文件 / 7,556 行，是七个子域里最大的
//! 一个。本模块目前只有 [`resolution`] 一块 —— 它被 `collections` 的
//! `list_playlist_resolutions` 与 `list_playlist_movies` 引用，所以先落地。
//!
//! 其余 26 个文件（`movie_service` / `actor_service` / `movie_image_service`
//! 等）随 `catalog` 域推进时补齐。
//!
//! # 一个文件横跨两个域，是上游的结构决定的
//!
//! `movie_resolution_service.py` 放在 `catalog/` 下，但被
//! `collections/playlist_service.py` 导入两次（筛选用 `resolution_exists_expression`，
//! 档位聚合用 `resolution_level_expression`）。这里跟着上游放在 `catalog`
//! 而不是 `collections` —— 免得影片卡片那一侧（未来的 `catalog` 端点）要反向
//! 依赖 `collections`。

pub mod resolution;
