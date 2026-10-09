//! 热播女优新作（上游 `hot_actress_release_service.py`，9.3KB）。
//!
//! **纯 PostgreSQL** —— 候选行来自 `media` / `movie` / `actor` 的联表。
//!
//! # 逻辑形状
//!
//! 上游 `:50-219` 是三步：
//!
//! 1. `_history_actor_evidence`（`:50`）—— 从**历史**数据算每位女优的权重
//! 2. `_candidate_rows`（`:101`）—— 取窗口期内的候选影片
//! 3. `_scored_movies`（`:123`）—— 用证据加权打分并排序，分页在
//!    `_page_resources`（`:160`）
//!
//! # 一处容易漏的语义
//!
//! `_release_date`（`:119`）接受 `date | datetime` 两种类型 —— 数据库里
//! `release_date` 可能是纯日期也可能是带时间的，**混着存**。所以 Rust 侧
//! 拿到字段后必须做一次归一化，不能假定类型。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 列表条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotActressReleaseItem {
    pub movie_id: i64,
    pub title: Option<String>,
    pub release_date: Option<String>,
    pub actress_names: Vec<String>,
    /// 综合得分。**已按窗口与证据加权**，不是单个女优的分数。
    pub score: f64,
}

/// 分页结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotActressReleasePage {
    pub items: Vec<HotActressReleaseItem>,
    pub next_cursor: Option<String>,
}

/// 读侧服务。
pub struct HotActressReleaseService;

impl HotActressReleaseService {
    /// 列出热播女优新作（分页）。上游 `list_items`（`:219`）。
    pub fn list_items(
        page_size: Option<i64>,
        cursor: Option<&str>,
        window_days: Option<i64>,
    ) -> Result<HotActressReleasePage, ServiceError> {
        todo!("骨架：照上游 `:50-219` 三步实现（历史证据 -> 候选行 -> 加权打分 -> 分页）")
    }

    /// 把 `release_date` 归一化成日期。
    ///
    /// 上游 `_release_date`（`:119-121`）接受 `date | datetime` —— 库里两种
    /// 都存，**混着**。不归一化的话带时间的值会带着时分秒进比较，导致
    /// 窗口边界差几个小时。
    pub fn release_date(value: chrono::NaiveDate) -> chrono::NaiveDate {
        value
    }
}