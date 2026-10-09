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
//! 其余 22 个文件（`movie_image_service` / `tag_service` 等）与
//! `movie_service` 的剩余 20 条端点随 `catalog` 域推进时补齐。
//!
//! # 一个文件横跨两个域，是上游的结构决定的
//!
//! `movie_resolution_service.py` 放在 `catalog/` 下，但被
//! `collections/playlist_service.py` 导入两次（筛选用 `resolution_exists_expression`，
//! 档位聚合用 `resolution_level_expression`）。这里跟着上游放在 `catalog`
//! 而不是 `collections` —— 免得影片卡片那一侧（未来的 `catalog` 端点）要反向
//! 依赖 `collections`。
//!
//! # 待铺清单（上游 21 个文件，按建议顺序）
//!
//! 顺序按「依赖关系」而非字母序 —— 靠前的被靠后的引用。
//!
//! | 顺序 | 文件 | 挡在前面的是什么 |
//! |---|---|---|
//! | 1 | `movie_ownership_gateway.rs` | **无**（纯 DB + jsonb） |
//! | 1 | `actor_ownership_gateway.rs` | **无**（纯 DB + jsonb） |
//! | 1 | `movie_list_media.rs` | 无（14 行，纯 DB） |
//! | 2 | `movie_heat.rs` ✅ | 无（已铺） |
//! | 2 | `movie_task.rs` | 无（48 行） |
//! | 2 | `movie_subscription_search_state.rs` | 无（纯 DB） |
//! | 3 | `image_cleanup.rs` | 需读文件系统 |
//! | 3 | `movie_asset_pack.rs` | 需读文件系统（zip） |
//! | 4 | `catalog_import.rs` | metadata_source + 依赖 image_cleanup |
//! | 4 | `metadata_source.rs` | 插件 ABI |
//! | 4 | `movie_image.rs` | 出网 + Pillow/cv2 |
//! | 5 | 其余 11 个 | 见各文件 |

pub mod actor;
pub mod actor_merge;
pub mod actor_ownership_gateway;
pub mod catalog_import;
pub mod image_cleanup;
pub mod metadata_source;
pub mod movie;
pub mod movie_asset_pack;
pub mod movie_asset_pack_backfill;
pub mod movie_heat;
pub mod movie_image;
pub mod movie_interaction_sync;
pub mod movie_javdb_backfill;
pub mod movie_list_media;
pub mod movie_metadata_refresh;
pub mod movie_metadata_search;
pub mod movie_ownership_gateway;
pub mod movie_subtitle;
pub mod movie_subscription;
pub mod movie_subscription_search_state;
pub mod movie_task;
pub mod movie_thin_cover_backfill;
pub mod resolution;
pub mod subtitle_asset;
pub mod subscribed_actor_movie_sync;
pub mod tag;
