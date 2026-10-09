//! 图片索引（上游 `image_search_index_service.py`，19.5KB / 490 行）。
//!
//! 依赖：推理客户端 [`super::embedding`] + 稠密向量库 [`qdrant::dense`]。
//! 两者都已在 Rust 侧就位，所以本文件**没有外部阻塞**。
//!
//! # 三条数据流
//!
//! ```text
//!   待索引缩略图 ──┐
//!   待索引剧情图 ──┼─> 归一化 ─> 推理服务取向量 ─> Qdrant upsert ─> 标记已索引
//!                  │
//!   索引空间变更 ──┘（变了就 _reset_for_rebuild）
//! ```
//!
//! # 两处上游的细节
//!
//! **1. 批大小与分批。** `_index_thumbnail_batch`（`:284`）与
//! `_index_plot_image_batch`（`:352`）分开，两条链路的向量空间与集合不同。
//!
//! **2. 失败要标记而不是静默跳过。** `_commit_statuses`（`:462`）与
//! `_set_status`（`:480`）：失败的记录要写回状态，否则下次扫描会无限重试同一条
//! 坏数据。这是「宁可标记失败也不假装成功」的典型。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::embedding::EmbeddingClient;
use super::qdrant::dense::DenseStore;
use crate::error::ServiceError;

/// 一批索引的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IndexBatchOutcome {
    /// 成功写入向量库。
    pub indexed: u32,
    /// 处理失败并已标记状态。
    pub failed: u32,
    /// 因推理服务或向量库不可用而未处理。
    pub skipped: u32,
}

/// 索引服务。
pub struct ImageSearchIndexService {
    thumbnails: Arc<DenseStore>,
    plot_images: Arc<DenseStore>,
    embedding: Arc<EmbeddingClient>,
}

impl ImageSearchIndexService {
    /// 构造。
    pub fn new(
        thumbnails: Arc<DenseStore>,
        plot_images: Arc<DenseStore>,
        embedding: Arc<EmbeddingClient>,
    ) -> Self {
        Self { thumbnails, plot_images, embedding }
    }

    /// 确保两个集合都建好且维度与推理服务声明的一致。上游 `ensure_stores_ready`（`:50`）。
    ///
    /// 维度不一致要走重建，**不是**就地改集合 —— 已建集合的维度改不了。
    pub async fn ensure_stores_ready(&self, vector_size: u32) -> Result<(), ServiceError> {
        todo!("骨架：照上游 `:50-58` 实现（维度不符走重建）")
    }

    /// 处理待索引的图片。上游 `index_pending_images`（`:60-201`，140 行，本文件主体）。
    pub async fn index_pending_images(&self, batch_size: usize) -> Result<IndexBatchOutcome, ServiceError> {
        todo!("骨架：照上游 `:60-201` 实现（缩略图 + 剧情图两条链）")
    }

    /// 空间变更后重置索引。上游 `_reset_for_rebuild`（`:207-231`）。
    pub async fn reset_for_rebuild(&self) -> Result<ImageSearchResetResultAlias, ServiceError> {
        todo!("骨架：照上游 `:207-231` 实现")
    }

    /// 删除某媒体的全部向量。上游 `delete_media_vectors`（`:490`）。
    pub async fn delete_media_vectors(&self, media_id: i64) -> Result<u64, ServiceError> {
        todo!("骨架：照上游 `:490+` 实现（按 payload 的 media_id 过滤删除）")
    }
}

/// 重置结果别名。
///
/// 与 [`super::image_search_reset::ImageSearchResetResult`] 同型 —— 这里重置
/// 的就是同一批表。写成别名而不是复制一份类型，是为了让两边将来改字段时
/// 不会只改一处。
pub type ImageSearchResetResultAlias = super::image_search_reset::ImageSearchResetResult;