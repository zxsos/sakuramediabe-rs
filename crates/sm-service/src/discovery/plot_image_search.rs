//! 剧情图检索（上游 `movie_plot_image_search_service.py`，11KB / 284 行）。
//!
//! 依赖：推理客户端 [`super::embedding`] + 剧情图向量集合
//! [`qdrant::plot_image::PlotImageVectorStore`]。两者已在 Rust 侧就位。
//!
//! # 与 [`super::image_search`] 的关系：**刻意平行，不合并**
//!
//! 两个服务的会话、游标、分页、过期清理逻辑几乎逐行对应，但：
//!
//! | | `image_search` | 本模块 |
//! |---|---|---|
//! | 集合 | 缩略图 `media_thumbnail_vectors_siglip2_v1` | 剧情图 `movie_plot_image_vectors_siglip2_v1` |
//! | 结果条目 | 缩略图 | 剧情图 + 影片链接 |
//! | 过滤维度 | `movie_id` + `media_id` | 只有 `movie_id` |
//!
//! **合并的代价**是一个带 `enum` 分支的服务：每个方法都要 match 哪种集合、
//! 哪种条目、哪些 payload 索引。而两组差异没有一处能靠「参数化」消掉 ——
//! payload 索引不同、条目构造不同、链接查询不同。所以各写一份，
//! 让分页/游标那几行重复。
//!
//! 重复量有界：会话 + 游标 + 分页约 40 行，参考
//! [`super::image_search`] 的同名方法。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::embedding::EmbeddingClient;
use super::qdrant::plot_image::PlotImageVectorStore;
use crate::error::ServiceError;

/// 会话 id（与图搜**不通用** —— 绑不同集合）。
pub type PlotImageSearchSessionId = String;

/// 检索结果条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotImageSearchItem {
    /// 剧情图 id。
    pub plot_image_id: i64,
    pub movie_id: Option<i64>,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
    pub url: Option<String>,
    /// 所属影片标题，延迟拼。
    pub movie_title: Option<String>,
}

/// 检索结果分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotImageSearchPage {
    pub items: Vec<PlotImageSearchItem>,
    pub next_cursor: Option<String>,
}

/// 剧情图检索服务。
pub struct MoviePlotImageSearchService {
    store: Arc<PlotImageVectorStore>,
    embedding: Arc<EmbeddingClient>,
}

impl MoviePlotImageSearchService {
    /// 构造。
    pub fn new(store: Arc<PlotImageVectorStore>, embedding: Arc<EmbeddingClient>) -> Self {
        Self { store, embedding }
    }

    /// 建会话并返回第一页（以图为 query）。上游 `create_session_and_first_page`（`:103`）。
    pub async fn create_session_and_first_page(
        &self,
        image_bytes: &[u8],
        page_size: Option<i64>,
    ) -> Result<(PlotImageSearchSessionId, PlotImageSearchPage), ServiceError> {
        todo!("骨架：照上游 `:103-138` 实现")
    }

    /// 建会话并返回第一页（以文本为 query）。上游 `create_text_session_and_first_page`（`:139`）。
    pub async fn create_text_session_and_first_page(
        &self,
        query: &str,
        page_size: Option<i64>,
    ) -> Result<(PlotImageSearchSessionId, PlotImageSearchPage), ServiceError> {
        todo!("骨架：照上游 `:139-169` 实现")
    }

    /// 翻页。上游 `list_results`（`:170`）。
    pub async fn list_results(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<PlotImageSearchPage, ServiceError> {
        todo!("骨架：照上游 `:170-183` 实现")
    }

    /// 真正的分页查询。上游 `_search_page`（`:184-242`）。
    pub async fn search_page(
        &self,
        vector: Vec<f32>,
        exclude_ids: &[i64],
        page_size: i64,
        offset: i64,
    ) -> Result<Vec<PlotImageSearchItem>, ServiceError> {
        todo!("骨架：照上游 `:184-242` 实现")
    }

    /// 页大小归一化。上游 `_page_size`（`:72-80`）。
    pub fn page_size(page_size: Option<i64>) -> i64 {
        todo!("骨架：照上游 `:72-80` 实现")
    }

    /// 归一化请求里的 id 列表。上游 `_normalize_ids`（`:44-45`）。
    ///
    /// **空列表归一成 `None`（不过滤）** —— 上游是 `if not ids: return None`，
    /// Python 里 `[]` 是 falsy。
    ///
    /// ⚠️ **我早先写的是 `ids.map(|s| s.to_vec())` —— 那是错的**，它让
    /// `Some(&[])` 透传成「排除零部影片」，与上游相反。见
    /// [`super::image_search`] 模块文档里的更正。
    ///
    /// **直接复用图搜那一份**（上游两个服务用的是同一个函数），不重复实现 ——
    /// 这次的教训正是「两份实现漂移」。
    pub fn normalize_ids(ids: Option<&[i64]>) -> Option<Vec<i64>> {
        super::image_search::normalize_ids(ids)
    }
}