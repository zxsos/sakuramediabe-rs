//! `discovery` 域：以图搜与推荐为主。
//!
//! 参照物：上游 `src/service/discovery/`（16 个文件 / 171.4KB）。
//!
//! # 进度：**文件框架 16/16 全部铺完** —— 方法体待实现
//!
//! **本轮只铺框架**：签名、类型、错误语义、模块文档按上游定好，方法体是
//! `todo!()`。**没有编译验证**（重构阶段，按要求不做）—— 所以下列「已就位」
//! 指的是**类型与依赖已铺好且经代码走查确认对得上上游**，不是「验证过能跑」。
//!
//! | 上游文件 | 大小 | 本 crate | 外部依赖 |
//! |---|---|---|---|
//! | `qdrant_thumbnail_store.py` | 18KB | [`qdrant::dense`] + [`qdrant::thumbnail`] | Qdrant |
//! | `qdrant_movie_similarity_store.py` | 9.4KB | [`qdrant::similarity`] | Qdrant |
//! | `qdrant_plot_image_store.py` | 2.9KB | [`qdrant::plot_image`] | Qdrant |
//! | `embedding_client.py` | 5.4KB | [`embedding`] | 推理服务 |
//! | `image_search_service.py` | 12.3KB | [`image_search`] | 推理 + Qdrant |
//! | `image_search_index_service.py` | 19.5KB | [`image_search_index`] | 推理 + Qdrant |
//! | `image_search_index_space_service.py` | 4.5KB | [`image_search_space`] | 无（纯 PG） |
//! | `image_search_input.py` | 848B | [`image_search_space::normalize_image_search_query`] | 无 |
//! | `image_search_reset_service.py` | 980B | [`image_search_reset`] | 无（纯 PG） |
//! | `movie_plot_image_search_service.py` | 11KB | [`plot_image_search`] | 推理 + Qdrant |
//! | `ranking_service.py` | 21.7KB | [`ranking`] | 写侧要 provider 插件 |
//! | `hot_actress_release_service.py` | 9.3KB | [`hot_actress_release`] | 无（纯 PG） |
//! | `recommendation_service.py` | 15.1KB | [`recommendation`] | Qdrant（稀疏） |
//! | `moment_recommendation_service.py` | 24KB | [`moment_recommendation`] | 推理 + Qdrant + 相似影片 |
//! | `daily_recommendation_service.py` | 18.7KB | [`daily_recommendation`] | 相似影片 + **ranking 读侧** |
//!
//! # 三个「不卡 Qdrant」的文件值得单独说
//!
//! 早期文档把整个 `discovery` 都标成「卡 Qdrant」（11 个文件），那是用关键词 grep
//! 判定的。**按 import 段精确判定**（`handoff.md` 第 47~52 行的纪律）后，
//! [`ranking`]、[`hot_actress_release`]、[`image_search_space`]、[`image_search_reset`]
//! 这四个**只 import `peewee` / `src.model` / `PIL` / `optional_services`，
//! 纯 PostgreSQL，零外部依赖** —— 其中 [`ranking`] 是整个域最大的单文件（21.7KB）。
//!
//! `sm-db/src/discovery/rankings.rs`（13.4KB）早就为 [`ranking`] 写好了。

pub mod daily_recommendation;
pub mod embedding;
pub mod hot_actress_release;
pub mod image_search;
pub mod image_search_index;
pub mod image_search_reset;
pub mod image_search_space;
pub mod moment_recommendation;
pub mod plot_image_search;
pub mod qdrant;
pub mod ranking;
pub mod recommendation;

pub use image_search_space::{ImageSearchIndexRebuildRequired, ImageSearchIndexSpaceStatus};
pub use qdrant::similarity::{MovieSimilarityHit, SimilarityQueryError};