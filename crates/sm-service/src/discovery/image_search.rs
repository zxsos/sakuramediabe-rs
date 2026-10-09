//! 图搜会话与检索（上游 `image_search_service.py`，12.3KB / 301 行）。
//!
//! 依赖：推理客户端 [`super::embedding`] + 稠密向量库 [`qdrant::dense`]。
//! 两者已在 Rust 侧就位，**无外部阻塞**。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::embedding::EmbeddingClient;
use super::qdrant::dense::DenseStore;
use crate::error::ServiceError;

/// 会话 id。
///
/// **会话不是纯不透明串** —— 它编码了 embedding 空间与向量维度，
/// 这样老会话在换模型后会自然失效。上游 `_encode_cursor`（`:55`）处理游标，
/// `_get_session_model`（`:92`）从会话还原模型。
pub type ImageSearchSessionId = String;

/// 检索结果条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSearchItem {
    /// 缩略图 id。
    pub thumbnail_id: i64,
    pub media_id: i64,
    pub movie_id: Option<i64>,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
    /// 图片地址，延迟到真正要返回时才拼。
    pub url: Option<String>,
}

/// 检索结果分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSearchPage {
    pub items: Vec<ImageSearchItem>,
    pub next_cursor: Option<String>,
}

/// 图搜服务。
pub struct ImageSearchService {
    store: Arc<DenseStore>,
    embedding: Arc<EmbeddingClient>,
}

impl ImageSearchService {
    /// 构造。
    pub fn new(store: Arc<DenseStore>, embedding: Arc<EmbeddingClient>) -> Self {
        Self { store, embedding }
    }

    /// 建会话并返回第一页（以图为 query）。上游 `create_session_and_first_page`（`:114`）。
    pub async fn create_session_and_first_page(
        &self,
        image_bytes: &[u8],
        page_size: Option<i64>,
    ) -> Result<(ImageSearchSessionId, ImageSearchPage), ServiceError> {
        todo!("骨架：照上游 `:114-152` 实现")
    }

    /// 建会话并返回第一页（以文本为 query）。上游 `create_text_session_and_first_page`（`:153`）。
    pub async fn create_text_session_and_first_page(
        &self,
        query: &str,
        page_size: Option<i64>,
    ) -> Result<(ImageSearchSessionId, ImageSearchPage), ServiceError> {
        todo!("骨架：照上游 `:153-182` 实现")
    }

    /// 翻页。上游 `list_results`（`:183`）。
    pub async fn list_results(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<ImageSearchPage, ServiceError> {
        todo!("骨架：照上游 `:183-192` 实现")
    }

    /// 真正的分页查询。上游 `_search_page`（`:193-253`）。
    pub async fn search_page(
        &self,
        vector: Vec<f32>,
        exclude_ids: &[i64],
        page_size: i64,
        offset: i64,
    ) -> Result<Vec<ImageSearchItem>, ServiceError> {
        todo!("骨架：照上游 `:193-253` 实现")
    }

    /// 归一化请求里的 id 列表：`None` = 不排除；空列表 = 排除全部（要区别于 None）。
    ///
    /// 上游 `_normalize_ids`（`:49-52`）。**这个区别是契约的一部分** ——
    /// `ids=[]` 与不传 `ids` 语义完全不同，合并会让「显式排除全部」变成
    /// 「不过滤」。
    pub fn normalize_ids(ids: Option<&[i64]>) -> Option<Vec<i64>> {
        ids.map(|slice| slice.to_vec())
    }

    /// 页大小归一化并夹到上下界。上游 `_normalize_page_size`（`:78-86`）。
    pub fn normalize_page_size(page_size: Option<i64>) -> i64 {
        todo!("骨架：照上游 `:78-86` 实现（带默认与上下界）")
    }
}