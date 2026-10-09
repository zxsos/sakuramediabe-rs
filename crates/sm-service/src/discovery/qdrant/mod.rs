//! Qdrant 向量库存储层。
//!
//! # 现状：稠密向量部分已搬，相似影片那套还没搬
//!
//! 上游 `discovery` 里碰 Qdrant 的 11 个文件分两类：
//!
//! | | 文件 | 本 crate |
//! |---|---|---|
//! | 稠密向量 + 标量过滤 | `qdrant_thumbnail_store`(18KB)、`qdrant_plot_image_store`(2.9KB) | ✅ [`dense`] + [`thumbnail`] + [`plot_image`] |
//! | **稀疏向量 + 别名切换** | `qdrant_movie_similarity_store`(9.4KB) | ❌ 未做，见下 |
//!
//! ## 为什么相似影片那套是单独的活
//!
//! `QdrantMovieSimilarityStore` 与稠密那套**结构不同**，不是同一个核心的
//! 第三个实例：
//!
//! - 用**稀疏向量**（`upsert_sparse_points`）而非稠密
//! - 用**别名切换**做蓝绿重建（`ALIAS_NAME` / `list_index_collections` /
//!   `create_collection` / `activate_collection`）—— 重建完才切别名，
//!   避免重建期间搜索结果为空
//! - 有 `is_ready()` 这类就绪判定
//!
//! 把这些塞进 [`dense`] 会让那个核心同时承担两套语义。**宁可分两个模块，
//! 也不要为了「统一」造一个带 `enum` 分支的存储层。**

pub mod dense;
pub mod plot_image;
pub mod thumbnail;

pub use dense::{
    normalize_score, DenseStore, DenseStoreStatus, PLOT_IMAGE_COLLECTION, PLOT_IMAGE_PAYLOAD_INDEX,
    THUMBNAIL_COLLECTION, THUMBNAIL_PAYLOAD_INDEX,
};
pub use plot_image::{PlotImageVectorRecord, PlotImageVectorSearchHit, PlotImageVectorStore};
pub use thumbnail::{ThumbnailVectorRecord, ThumbnailVectorSearchHit, ThumbnailVectorStore};
