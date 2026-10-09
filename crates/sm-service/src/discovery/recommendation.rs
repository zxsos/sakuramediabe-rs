//! 影片推荐（上游 `recommendation_service.py`，15.1KB / 380 行）。
//!
//! 依赖：稀疏向量库 [`qdrant::similarity::MovieSimilarityStore`]（刚做完）。
//! 稀疏向量的**算法**在 PostgreSQL 侧（本模块算 BM25 风格的倒排），
//! Qdrant 只存与查 —— 与上游边界一致。
//!
//! # 与「每日推荐」「瞬时推荐」的关系
//!
//! 上游还有 `daily_recommendation_service.py`（18.7KB）与
//! `moment_recommendation_service.py`（24KB），它们**依赖本模块**产出相似影片。
//! 所以本模块是推荐族的底座，先落地它另两个才有意义。
//!
//! # 一处关键的错误契约
//!
//! 本模块的查询**不总是抛错** —— 见
//! [`qdrant::similarity::SimilarityQueryError`]：
//!
//! - `IndexNotReady` → 503（会恢复，重试有意义）
//! - `Unavailable` → **降级成空相似度列表**，不让影片详情页整体报错
//!
//! 下面 [`MovieRecommendationService::list_similar`] 的签名刻意**不返回
//! `Result`** 给那条降级路径，而是返回 `Vec` —— 让「降级」在类型上就是默认值，
//! 而不是靠调用方记得写 `if let Err(...) { return vec![] }`。
//! **靠调用方自觉的降级，迟早会漏一处。**

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::qdrant::similarity::{MovieSimilarityHit, MovieSimilarityStore, SimilarityQueryError,
                                SparsePoint};
use crate::error::ServiceError;

/// 相似影片条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMovieItem {
    pub movie_id: i64,
    pub title: Option<String>,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
    /// 当前用户能否播放（转码/下载状态）。
    pub can_play: bool,
}

/// 相似影片分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMoviePage {
    pub items: Vec<SimilarMovieItem>,
    pub next_cursor: Option<String>,
}

/// 重算结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RecomputeOutcome {
    /// 写入的影片数。
    pub written: u32,
    /// 清理的孤儿集合数。
    pub purged_collections: u32,
    /// 旧集合名（蓝绿切换的返回，上层据此删除）。
    pub previous_collection: Option<String>,
}

/// 推荐服务。
pub struct MovieRecommendationService {
    store: Arc<MovieSimilarityStore>,
}

impl MovieRecommendationService {
    /// 构造。
    pub fn new(store: Arc<MovieSimilarityStore>) -> Self {
        Self { store }
    }

    /// 全量重算稀疏索引。上游 `recompute_all`（`:189-302`，113 行，本文件主体）。
    ///
    /// **写进新集合再原子切别名**（[`MovieSimilarityStore::activate_collection`]），
    /// 所以重算期间线上查询照常走旧集合。
    pub async fn recompute_all(&self) -> Result<RecomputeOutcome, ServiceError> {
        todo!("骨架：照上游 `:189-302` 实现（特征倒排 -> 新集合 -> 原子切别名）")
    }

    /// 查相似影片。**降级路径不返回 Err。**
    ///
    /// 上游 `search_similar_movies`（`:303`）。`Unavailable` 时返回空列表并记
    /// warn —— 与 [`SimilarityQueryError`] 文档里说的调用方契约一致。
    pub async fn search_similar_movies(
        &self,
        source_movie_id: i64,
        limit: i64,
    ) -> Vec<MovieSimilarityHit> {
        match self.store.search_many(&[source_movie_id], limit).await {
            Ok(map) => map.get(&source_movie_id).cloned().unwrap_or_default(),
            Err(SimilarityQueryError::NotReady) => {
                // 这个**不降级**：索引没建好是「503 重试有意义」那一类，
                // 与 Qdrant 故障不同。抛出去。
                panic!("骨架：应把 NotReady 转成 503，见 list_similar 的签名说明")
            }
            Err(SimilarityQueryError::Unavailable { detail }) => {
                tracing::warn!(
                    source_movie_id,
                    detail,
                    "相似影片查询跳过：影片相似度服务不可用（只丢相似度信号）"
                );
                Vec::new()
            }
        }
    }

    /// 列出相似影片（带分页与可播放状态）。上游 `list_similar`（`:313`）。
    pub async fn list_similar(
        &self,
        source_movie_id: i64,
        limit: i64,
    ) -> Result<SimilarMoviePage, ServiceError> {
        todo!("骨架：照上游 `:313-376` 实现（NotReady -> 503，Unavailable -> 空列表）")
    }

    /// 供路由直接用的资源包装。上游 `list_similar_resources`（`:377`）。
    pub async fn list_similar_resources(
        &self,
        source_movie_id: i64,
        limit: Option<i64>,
    ) -> Result<SimilarMoviePage, ServiceError> {
        todo!("骨架：照上游 `:377-380` 实现（调 list_similar 并填 movie 字段）")
    }

    /// 构造单条稀疏向量。上游 `_build_sparse_vector`（`:129-166`）。
    ///
    /// 倒排的算法在**本模块**，不在 Qdrant —— 与上游
    /// `qdrant_movie_similarity_store` 只做存取的边界一致。
    pub fn build_sparse_vector(
        feature: &HashMap<String, f64>,
        document_frequencies: &HashMap<String, f64>,
        total_documents: f64,
    ) -> SparsePoint {
        todo!("骨架：照上游 `:129-166` 实现（BM25 风格加权）")
    }
}