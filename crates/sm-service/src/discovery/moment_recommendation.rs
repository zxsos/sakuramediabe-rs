//! 瞬时推荐（上游 `moment_recommendation_service.py`，24KB / 580 行）。
//!
//! 依赖：推理客户端 [`super::embedding`]、稠密向量库
//! [`super::qdrant::dense`]、相似影片 [`super::recommendation`]。
//! **三者均已在 Rust 侧就位，本文件无外部阻塞。**
//!
//! # 形状：三个候选源合并后统一排名
//!
//! ```text
//!   种子（近期热度高的媒体缩略图）──> 推理服务取向量
//!        │
//!        ├──> 源 A 视觉相似   （Qdrant 稠密检索，:201）
//!        ├──> 源 B 相似影片   （recommendation 稀疏索引，:309）
//!        └──> 源 C 热门候选   （纯 DB，:356）
//!
//!   合并去重 ─> _rank_candidates(:393) ─> top N ─> 存快照 / 直接返回
//! ```
//!
//! # 一处贯穿全文的概念：`target_ratio`
//!
//! `_safe_ratio`（`:111`）算的是**场景在影片里的时间比例**
//! （`offset_seconds / duration_seconds`），返回 `Option<f64>` —— 时长未知的
//! 媒体返回 `None`。
//!
//! 它贯穿候选收集与选图：`_choose_thumbnail_from_media_thumbnails`（`:259`）、
//! `_choose_thumbnail_for_media`（`:273`）、`_choose_thumbnail_for_movie`（`:282`）
//! 都按它挑**宽高比接近该场景的缩略图** —— 因为推荐展示的是「某个时间点的画面」，
//! 拿一张比例不符的图会误导。
//!
//! **所以 `None` 必须一路传播，不能在中间换成 0.0** —— 比例未知时「不筛」
//! 与「筛成比例 0」是两种行为。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::embedding::EmbeddingClient;
use super::qdrant::dense::DenseStore;
use crate::error::ServiceError;

/// 种子数量上限（上游 `MOMENT_RECOMMENDATION_SEED_LIMIT`）。
pub const SEED_LIMIT: usize = 32;

/// 种子：一条待取向量的缩略图。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MomentSeed {
    media_id: i64,
    thumbnail_id: i64,
    /// 该缩略图所属影片。
    movie_id: Option<i64>,
}

/// 候选：合并三个源后的统一形状。
#[derive(Debug, Clone)]
struct MomentCandidate {
    movie_id: i64,
    /// 得分。三源量纲不同，归一化后才可比 —— 见模块文档。
    score: f32,
    /// 命中原因码，供上层出文案。
    reason_codes: Vec<String>,
    /// 该场景在影片里的时间比例；`None` = 时长未知。
    target_ratio: Option<f64>,
}

/// 单条推荐。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MomentRecommendationItem {
    pub movie_id: i64,
    pub title: Option<String>,
    pub media_id: Option<i64>,
    pub thumbnail_id: Option<i64>,
    pub thumbnail_url: Option<String>,
    pub score: f32,
    /// 该场景在影片里的时间比例（秒）。
    pub target_offset_seconds: Option<i64>,
    /// 推荐理由码。
    pub reason_codes: Vec<String>,
}

/// 瞬时推荐分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MomentRecommendationPage {
    pub items: Vec<MomentRecommendationItem>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
}

/// 瞬时推荐服务。
pub struct MomentRecommendationService {
    store: Arc<DenseStore>,
    embedding: Arc<EmbeddingClient>,
}

impl MomentRecommendationService {
    /// 构造。
    pub fn new(store: Arc<DenseStore>, embedding: Arc<EmbeddingClient>) -> Self {
        Self { store, embedding }
    }

    /// 生成瞬时推荐并落快照。上游 `generate_recommendations`（`:415-508`，95 行）。
    pub async fn generate_recommendations(&self, limit: usize) -> Result<usize, ServiceError> {
        todo!("骨架：照上游 `:415-508` 实现（种子 -> 三源候选 -> 合并去重 -> 排名 -> 落快照）")
    }

    /// 读已存的瞬时推荐快照（分页）。上游 `list_items`（`:510-580`）。
    pub async fn list_items(page: i64, page_size: i64) -> Result<MomentRecommendationPage, ServiceError> {
        todo!("骨架：照上游 `:510-580` 实现")
    }

    /// 场景时间比例。`duration_seconds` 未知时返回 `None`。
    ///
    /// 上游 `_safe_ratio`（`:111-116`）。**`None` 必须一路传播** —— 比例未知
    /// 时「不筛缩略图」与「筛成比例 0」是两种行为，后者会全选到竖图。
    pub fn safe_ratio(offset_seconds: i64, duration_seconds: Option<i64>) -> Option<f64> {
        duration_seconds
            .filter(|duration| *duration > 0)
            .map(|duration| offset_seconds as f64 / duration as f64)
    }

    /// 热度分。上游 `_heat_score`（`:107`）。
    pub fn heat_score(heat: Option<i64>) -> f64 {
        todo!("骨架：照上游 `:107-109` 实现")
    }

    /// 候选排名。上游 `_rank_candidates`（`:393-414`）。
    ///
    /// 三源量纲不同（余弦相似 / 稀疏得分 / 热度），**必须先归一化再比**，
    /// 否则热门源会把视觉相似源压掉。
    pub fn rank_candidates(candidates: &mut [MomentCandidate], limit: usize) {
        todo!("骨架：照上游 `:393-414` 实现（归一化 + 排序 + 取 top N）")
    }

    /// 合并去重：同影片多源命中时取最高分并**合并 reason_codes**。
    ///
    /// 上游 `_add_candidate`（`:187-200`）就是这个职责。合并理由码而不是取
    /// 一条 —— 「视觉相似」+「热门」同时命中比单一理由更有说服力。
    fn add_candidate(pool: &mut HashMap<i64, MomentCandidate>, candidate: MomentCandidate) {
        todo!("骨架：照上游 `:187-200` 实现")
    }
}