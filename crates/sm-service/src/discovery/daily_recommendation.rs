//! 每日推荐（上游 `daily_recommendation_service.py`，18.7KB / 476 行）。
//!
//! # 唯一同时消费**两个其他域**的推荐服务
//!
//! | 来源 | 用到 | 上游位置 |
//! |---|---|---|
//! | 相似度分 | [`super::recommendation`]（稀疏索引） | `_load_similarity_scores`（`:184`） |
//! | 排行榜分 | [`super::ranking`]（**读侧**） | `_load_ranking_scores`（`:220`） |
//! | 热度分 | 纯 DB | `_load_heat_scores`（`:211`） |
//! | 新鲜度分 | 纯 DB | `_build_freshness_scores`（`:237`） |
//!
//! **注意它用的是 `ranking` 的读侧**（`RankingCatalogService`），不是写侧 ——
//! 所以**它不依赖 provider 插件**。这个区分要紧：写侧（`RankingSyncService`）
//! 是 `discovery` 里唯一被 provider 卡住的东西，而这个文件只用读侧。
//!
//! # 与 `moment` 的分工
//!
//! | | 本模块（每日） | [`super::moment_recommendation`]（瞬时） |
//! |---|---|---|
//! | 刷新 | 每天一个**快照**，日期为键 | 每次请求现算 |
//! | 存储 | 落快照表 | 落快照表 |
//! | 相似度来源 | 仅稀疏索引 | 稀疏索引 **+ 稠密视觉检索** |
//!
//! 所以每日推荐**不需要推理服务** —— 它只用稀疏相似度。`moment` 才需要
//! 推理客户端取种子向量。
//!
//! # `_reason_texts` 是与前端的契约
//!
//! `_reason_texts`（`:258-260`）把**理由码**翻成展示文案。理由码是内部
//! 标识（`visual_similar` / `hot_ranked` / `fresh_release` …），文案是给用户看的。
//! **码到文案的映射不能散在调用方** —— 换文案就得改多处，而且会漏。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::recommendation::MovieRecommendationService;
use crate::error::ServiceError;

/// 候选影片。
#[derive(Debug, Clone)]
struct CandidateMovie {
    movie_id: i64,
    title: Option<String>,
    release_date: Option<chrono::NaiveDate>,
    heat: Option<i64>,
}

/// 打分后的推荐。
#[derive(Debug, Clone)]
struct ScoredRecommendation {
    movie_id: i64,
    title: Option<String>,
    score: f64,
    /// 理由码；`generate_latest_snapshot` 据此出文案。
    reason_codes: Vec<String>,
}

/// 单条每日推荐。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRecommendationItem {
    pub movie_id: i64,
    pub title: Option<String>,
    pub poster_url: Option<String>,
    pub score: f64,
    /// 展示用推荐理由（已由 [`reason_texts`] 从理由码翻出）。
    pub reasons: Vec<String>,
}

/// 每日推荐分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRecommendationPage {
    pub items: Vec<DailyRecommendationItem>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
    /// 快照日期。**分页要带日期** —— 否则跨零点翻页会混到两天的快照。
    pub snapshot_date: Option<String>,
}

/// 每日推荐服务。
pub struct DailyRecommendationService {
    similarity: MovieRecommendationService,
}

impl DailyRecommendationService {
    /// 构造。
    pub fn new(similarity: MovieRecommendationService) -> Self {
        Self { similarity }
    }

    /// 生成当天的推荐快照。上游 `generate_latest_snapshot`（`:348-413`，66 行）。
    pub async fn generate_latest_snapshot(
        &self,
        target_date: Option<chrono::NaiveDate>,
    ) -> Result<usize, ServiceError> {
        todo!("骨架：照上游 `:348-413` 实现（候选 -> 四源打分 -> 排名 -> 落当日快照）")
    }

    /// 读某天的快照（分页）。上游 `list_items`（`:415-476`）。
    pub async fn list_items(
        page: i64,
        page_size: i64,
        target_date: Option<chrono::NaiveDate>,
    ) -> Result<DailyRecommendationPage, ServiceError> {
        todo!("骨架：照上游 `:415-476` 实现")
    }

    /// 快照日期：`None` → 今天。上游 `_snapshot_date`（`:108-110`）。
    pub fn snapshot_date(target_date: Option<chrono::NaiveDate>) -> chrono::NaiveDate {
        target_date.unwrap_or_else(|| chrono::Local::now().date_naive())
    }

    /// 名次衰减。上游 `_rank_decay`（`:116-120`）。
    ///
    /// 榜次第 n 名 → 权重。`weight` 是整体系数。
    pub fn rank_decay(rank: i64, weight: f64) -> f64 {
        todo!("骨架：照上游 `:116-120` 实现")
    }

    /// 归一化到 [0, 1]。上游 `_normalize`（`:111-114`）。
    pub fn normalize(value: f64) -> f64 {
        todo!("骨架：照上游 `:111-114` 实现")
    }

    /// 理由码 → 展示文案。上游 `_reason_texts`（`:258-260`）。
    ///
    /// **映射必须集中在这里** —— 散到调用方会导致换文案时漏改，且不同端点
    /// 的同一理由会显示不同文案。未知码降级为原样返回，不丢。
    pub fn reason_texts(reason_codes: &[String]) -> Vec<String> {
        todo!("骨架：照上游 `:258-260` 实现（码到文案的集中映射表）")
    }

    /// 给候选打分。上游 `_score_movies`（`:262-346`，85 行，本文件主体）。
    ///
    /// 四路分数（相似度 / 热度 / 排行榜 / 新鲜度）加权合并，权重不同量纲要
    /// 先各自归一化。
    pub async fn score_movies(
        &self,
        candidates: &[CandidateMovie],
        seed_ids: &[i64],
    ) -> Result<Vec<ScoredRecommendation>, ServiceError> {
        todo!("骨架：照上游 `:262-346` 实现（四路分数归一化后加权合并）")
    }
}