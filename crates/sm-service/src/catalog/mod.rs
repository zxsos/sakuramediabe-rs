//! `catalog` 子域。
//!
//! 上游 `src/service/catalog/` 有 27 个文件 / 7,556 行，是七个子域里最大的
//! 一个。已落四块：
//!
//! | 模块 | 上游 | 说明 |
//! |---|---|---|
//! | [`resolution`] | `movie_resolution_service.py` 的档位部分 | 被 `collections` 引用，先落地 |
//! | [`actor`] | `actor_service.py`（827 行） | 11 个端点里可做的 9 个方法 |
//! | [`actor_merge`] | `actor_merge_service.py`（208 行） | `POST /actors/{id}/merge` |
//! | [`movie`] | `movie_service.py`（1,177 行）的订阅状态流转 | 5 个方法：番号定位 + 批量订阅/退订 |
//!
//! 其余 23 个文件（`movie_image_service` / `tag_service` 等）与
//! `movie_service` 的剩余 20 条端点随 `catalog` 域推进时补齐。
//!
//! # 一个文件横跨两个域，是上游的结构决定的
//!
//! `movie_resolution_service.py` 放在 `catalog/` 下，但被
//! `collections/playlist_service.py` 导入两次（筛选用 `resolution_exists_expression`，
//! 档位聚合用 `resolution_level_expression`）。这里跟着上游放在 `catalog`
//! 而不是 `collections` —— 免得影片卡片那一侧（未来的 `catalog` 端点）要反向
//! 依赖 `collections`。

pub mod actor;
pub mod actor_merge;
pub mod movie;
pub mod movie_subscription;
pub mod resolution;
pub mod tag;
