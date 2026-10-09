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
//! | 1 | `movie_ownership_gateway.rs` | ✅ **已在 `sm-db` 落地**（[`sm_db::repo::gateway`]）—— 见下 |
//! | 1 | `actor_ownership_gateway.rs` | ✅ **已在 `sm-db` 落地**（[`sm_db::repo::ActorOwnershipGateway`]）—— 同上 |
//! | 1 | ~~`movie_list_media.rs`~~ | ✅ **不需要**：真实现在 `playback::media_summary`（见下） |
//! | 2 | `movie_heat.rs` ✅ | 无（已铺） |
//! | 2 | `movie_task.rs` | 无（48 行） |
//! | 2 | `movie_subscription_search_state.rs` | 无（纯 DB） |
//! | 3 | `image_cleanup.rs` | 需读文件系统 |
//! | 3 | `movie_asset_pack.rs` | 需读文件系统（zip） |
//! | 4 | `catalog_import.rs` | metadata_source + 依赖 image_cleanup |
//! | 4 | `metadata_source.rs` | 插件 ABI |
//! | 4 | `movie_image.rs` | 出网 + Pillow/cv2 |
//! | 5 | 其余 11 个 | 见各文件 |
//!
//! # 两个字段主权网关**都不在本目录**
//!
//! 它们整个落在 [`sm_db::repo::gateway`]
//! （[`sm_db::repo::MovieOwnershipGateway`] / [`sm_db::repo::ActorOwnershipGateway`]）：
//! 每条入口都是「单条条件 UPDATE + jsonb + 乐观锁」，属仓储的活；而 service
//! 侧的调用方（[`movie`] / [`catalog_import`] / [`movie_javdb_backfill`]）
//! 直接收一个网关字段。
//!
//! 这里**原来各有一份同名骨架**，各自带一套杜撰的白名单：
//!
//! - 影片那份只有 **2** 个字段（`is_collection` / `is_blacklisted`），而上游
//!   `PROTECTED_MOVIE_FIELDS` 是 **6** 个（再加 `title` / `summary` /
//!   `maker_name` / `director_name`）—— 谁引用了它，谁的插件补录就会把
//!   `title` 静默拒掉；
//! - 演员那份用的是**黑名单**（「除 `javdb_id` / 头像 / 订阅之外都能写」），
//!   而上游是**9 字段白名单** —— 黑名单会放过 `name` / `alias_name` 这些
//!   网关根本不认的字段。
//!
//! 两份都已删除。`sm-db` 里那份白名单与上游逐字段对齐，并有对拍测试。
//!
//! 同一条路还清掉了一个：`movie_list_media.rs`（上游 14 行，只做「把媒体摘要挂
//! 到影片卡片上」）。它的骨架签名是**同步**的 `fn attach_movie_list_media(
//! movies: &mut [MovieCard])` —— 而这个函数必须查库（一趟 `IN` 查询），同步
//! 签名根本落不了地。真实现在 `playback::media_summary::attach_movie_list_media`
//! （`async`、收连接池与番号列表），`movie` 与 `collections::playlist` 两处都在
//! 用它。留下的那个骨架只会让人照着错的形状去实现。

pub mod actor;
pub mod actor_merge;
pub mod catalog_import;
pub mod image_cleanup;
// 路径原语与图片包字节原语。上游在 `common/media_paths.py` 与
// `common/image_store.py`，本仓没有 `common` 层，而它们的用户
// （`image_cleanup` / `movie_asset_pack` / 未来的 `media_thumbnail_service`）
// 跨 `catalog` 与 `playback` 两侧 —— 谁都不该被另一个服务"拥有"。
pub mod image_store;
pub mod media_paths;
pub mod metadata_source;
pub mod movie;
pub mod movie_asset_pack;
pub mod movie_asset_pack_backfill;
pub mod movie_heat;
pub mod movie_image;
pub mod movie_interaction_sync;
pub mod movie_javdb_backfill;
pub mod movie_metadata_refresh;
pub mod movie_metadata_search;
pub mod movie_subscription;
pub mod movie_subscription_search_state;
pub mod movie_subtitle;
pub mod movie_task;
pub mod movie_thin_cover_backfill;
pub mod resolution;
pub mod subscribed_actor_movie_sync;
pub mod subtitle_asset;
pub mod tag;
