//! 剧情图检索（上游 `movie_plot_image_search_service.py`，11KB / 284 行）。
//!
//! 依赖：推理客户端 [`super::embedding`] + 剧情图向量库
//! [`super::qdrant::plot_image::PlotImageVectorStore`] + 链接查询。**均已就位。**
//!
//! # 与 [`super::image_search`] 的关系：**刻意平行，游标与归一化直接复用**
//!
//! 上游这两个服务有**大段逐行相同**的代码：`CURSOR_VERSION = 1`、游标格式、
//! `_normalize_ids`、`_purge_expired_sessions`、会话表、甚至 `list_results` 的
//! 404 处理。**Rust 侧只保留一份实现**，本模块直接调用
//! [`super::image_search`] 的纯函数。
//!
//! 差异只有三处：
//!
//! | | 图搜 | 剧情图搜 |
//! |---|---|---|
//! | store | 缩略图集合 | 剧情图集合 |
//! | 结果条目 | `thumbnail_id` + `media_id` | `plot_image_id` + `movie_id` + 番号 |
//! | 命中过滤 | 只有 `score_threshold` | 阈值 **+ 缺链接**（见下）|
//!
//! # ★ 本模块解决了一个我在图搜里记为「无法避免」的问题
//!
//! 图搜那边我写了：`DenseStore::search` 没有 `score_threshold` 参数，阈值只能
//! 在应用层过滤，于是「多取一条判断有没有下一页」的那条**可能被阈值滤掉**，导致
//! **明明还有下一条却返回 `next_cursor = None`**，且没有任何报错。
//!
//! **上游的剧情图这版用 `while` 循环解决了它**（`:184-233`）：
//!
//! ```python
//! while len(items) < session.page_size:
//!     hits = search(..., batch_size, raw_offset, ...)
//!     if not hits: break
//!     for hit in hits:
//!         raw_offset += 1
//!         item = build_item(hit, ...)      # 阈值/缺链接 -> None
//!         if item is not None: items.append(item)
//!     if 满页 or 取空: break
//! ```
//!
//! **两个关键点**：
//!
//! 1. **循环到填满为止**，所以过滤掉几个就多扫几个 —— 页面永远是满的（除非真的
//!    没有更多）。
//! 2. **`raw_offset` 一边扫一边推进** —— 游标记的是**原始扫描位置**，不是
//!    「返回了几条」。否则翻页会重复或跳过。
//!
//! **所以图搜那边也要改成这个模式。** 已记在 [`super::image_search`] 里。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sm_db::repo::recommendation::MovieFeatureRepository;

use super::embedding::EmbeddingClient;
use super::image_search::{ImageSearchLimits, decode_cursor, encode_cursor, normalize_ids,
                          validate_score_threshold};
use super::qdrant::plot_image::PlotImageVectorStore;
use crate::error::ServiceError;

/// 检索结果条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotImageSearchItem {
    pub plot_image_id: i64,
    pub movie_id: Option<i64>,
    /// 影片番号。**要回表才有** —— 向量库里没有。
    pub movie_number: Option<String>,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
}

/// 检索结果分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotImageSearchPage {
    pub items: Vec<PlotImageSearchItem>,
    pub next_cursor: Option<String>,
}

/// 会话 + 第一页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotImageSearchSessionPage {
    pub session_id: String,
    pub status: String,
    pub page_size: i64,
    pub next_cursor: Option<String>,
    pub items: Vec<PlotImageSearchItem>,
}

/// 扫库批大小的**下界**。对应上游 `image_search.search_scan_batch_size`
/// （默认 100，已移植在 `config_schema.rs:400`）。
///
/// 存在的理由：一次只取 `page_size` 条的话，**过滤掉几个就得再扫一轮**，
/// 而每轮都是一次网络往返。取 `max(page_size, 100)` 让常见的「阈值滤掉几个」
/// 在**一轮内**就填满。
pub const SEARCH_SCAN_BATCH_SIZE: i64 = 100;

/// 剧情图检索服务。
pub struct MoviePlotImageSearchService {
    store: Arc<PlotImageVectorStore>,
    embedding: Arc<EmbeddingClient>,
    links: MovieFeatureRepository,
    limits: ImageSearchLimits,
}

impl MoviePlotImageSearchService {
    /// 构造。
    pub fn new(
        store: Arc<PlotImageVectorStore>,
        embedding: Arc<EmbeddingClient>,
        links: MovieFeatureRepository,
        limits: ImageSearchLimits,
    ) -> Self {
        Self { store, embedding, links, limits }
    }

    /// 页大小归一化。**直接复用图搜那一份**（上游两处逻辑相同）。
    pub fn page_size(&self, page_size: Option<i64>) -> Result<i64, ServiceError> {
        super::image_search::normalize_page_size(
            page_size,
            self.limits.default_page_size,
            self.limits.max_page_size,
        )
    }
}
impl MoviePlotImageSearchService {
    /// ★ 循环扫描直到填满页面。
    ///
    /// # 为什么必须循环而不能「多取一条」
    ///
    /// 命中项有**两个**被丢弃的理由：
    ///
    /// 1. `score_threshold` 不达标
    /// 2. **查不到链接** —— `_get_links` 用 `INNER JOIN movie` 且排除黑名单，
    ///    所以「没有关联影片」或「影片在黑名单里」的剧情图**根本查不出来**
    ///    → `link is None` → 丢弃
    ///
    /// 「多取一条判断有没有下一页」的做法在这里**不成立**：那一条可能被丢掉，
    /// 于是 `next_cursor` 变成 `None` 而后面还有内容 —— **静默截断**。
    ///
    /// # `raw_offset` 一边扫一边推进
    ///
    /// 游标记的是**原始扫描位置**，不是「返回了几条」。若用 `items.len()` 当
    /// offset，翻页会重复或跳过 —— 因为被丢掉的那几条**仍然消耗了扫描位置**。
    ///
    /// # 循环什么时候停
    ///
    /// | 条件 | 含义 |
    /// |---|---|
    /// | `items.len() == page_size` | 满了 |
    /// | `hits.is_empty()` | 扫完了 |
    /// | `hits.len() < batch_size` | **不满一批 = 没有更多了** |
    ///
    /// 第三条是提前退出优化：不满一批说明已经到末尾（上游 `:232`）。
    pub async fn search_page(
        &self,
        vector: &[f32],
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
        score_threshold: Option<f64>,
        page_size: i64,
        start_offset: usize,
    ) -> Result<PlotImageSearchPage, ServiceError> {
        let page_size = page_size.max(1) as usize;
        let batch_size = page_size.max(SEARCH_SCAN_BATCH_SIZE as usize) as i64;
        let mut items: Vec<PlotImageSearchItem> = Vec::with_capacity(page_size);
        // 原始扫描位置。**不是** `start_offset + items.len()`。
        let mut raw_offset = start_offset;

        loop {
            let hits = self
                .store
                .search(
                    vector.to_vec(),
                    batch_size as usize,
                    raw_offset,
                    movie_ids,
                    exclude_movie_ids,
                )
                .await?;
            if hits.is_empty() {
                break;
            }
            let hit_count = hits.len();
            // 先把这一批的链接一次查回来（**避免逐条查**）。
            let plot_image_ids: Vec<i32> = hits.iter().map(|hit| hit.plot_image_id as i32).collect();
            let links = self.links.plot_image_links(&plot_image_ids).await?;

            for hit in hits {
                // **先推进 raw_offset** —— 哪怕这一条被丢掉，位置也消耗了。
                raw_offset += 1;
                let Some(link) = links.get(&(hit.plot_image_id as i32)) else {
                    // 缺链接：没有关联影片，或影片在黑名单里。
                    tracing::warn!(
                        plot_image_id = hit.plot_image_id,
                        "剧情图检索命中查不到链接（无关联影片或影片在黑名单里），丢弃"
                    );
                    continue;
                };
                let score = super::qdrant::dense::normalize_score(hit.score);
                if let Some(threshold) = score_threshold {
                    if (score as f64) < threshold {
                        continue;
                    }
                }
                items.push(PlotImageSearchItem {
                    plot_image_id: hit.plot_image_id as i64,
                    movie_id: link.movie_id.map(|id| id as i64),
                    movie_number: link.movie_number.clone(),
                    score,
                });
                if items.len() == page_size {
                    break;
                }
            }

            if items.len() == page_size || hit_count < batch_size as usize {
                break;
            }
        }

        // 「还有下一页」的判定：扫描位置**没到底**。
        //
        // 上游 `:216-221` 的判定更严（`index < len(hits)` 或「再查一条看看」），
        // 那是因为它在 `len(items) == page_size` 那一刻就 break 了，
        // **不确定后面还有没有**。这里改成「循环自然结束」后，
        // `raw_offset < total` 就够了 —— 但我们不知道 total。
        //
        // **保守做法**：只要本轮是「因为满了」而 break，就认为还有更多
        // （`raw_offset` 恰好等于已扫过的末尾，可能还有也可能没有）。
        // 多给一个游标的后果是：客户端多翻一页拿到空列表 —— **可接受**。
        // 少给游标的后果是：**后面的结果永远看不到** —— 不可接受。
        let next_cursor = if items.len() == page_size {
            Some(encode_cursor(raw_offset as i64)?)
        } else {
            None
        };
        items.truncate(page_size);
        Ok(PlotImageSearchPage { items, next_cursor })
    }
}
impl MoviePlotImageSearchService {
    /// 以图为 query 建会话并返回第一页。
    ///
    /// 步骤顺序与图搜**完全一致**（校验 → 就绪闸门 → 清过期 → 取向量 → 建会话
    /// → 补排除条件 → 检索），理由见 [`super::image_search`] 的模块文档。
    pub async fn create_session_and_first_page(
        &self,
        image_bytes: &[u8],
        page_size: Option<i64>,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
        score_threshold: Option<f64>,
    ) -> Result<PlotImageSearchSessionPage, ServiceError> {
        if image_bytes.is_empty() {
            return Err(ServiceError::validation("image_search_empty_image", "image file is empty"));
        }
        let page_size = self.page_size(page_size)?;
        let movie_ids = normalize_ids(movie_ids);
        let exclude_movie_ids = normalize_ids(exclude_movie_ids);
        validate_score_threshold(score_threshold)?;

        self.ensure_searchable_index().await?;
        super::image_search::purge_expired_sessions().await?;
        let vector = self.embed_one_image(image_bytes).await?;
        self.finish_session(vector, page_size, movie_ids, exclude_movie_ids, score_threshold)
            .await
    }

    /// 以文本为 query 建会话并返回第一页。
    pub async fn create_text_session_and_first_page(
        &self,
        text: &str,
        page_size: Option<i64>,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
        score_threshold: Option<f64>,
    ) -> Result<PlotImageSearchSessionPage, ServiceError> {
        if text.trim().is_empty() {
            return Err(ServiceError::validation("image_search_empty_text", "text is empty"));
        }
        let page_size = self.page_size(page_size)?;
        let movie_ids = normalize_ids(movie_ids);
        let exclude_movie_ids = normalize_ids(exclude_movie_ids);
        validate_score_threshold(score_threshold)?;

        self.ensure_searchable_index().await?;
        super::image_search::purge_expired_sessions().await?;
        let vector = self.embed_one_text(text).await?;
        self.finish_session(vector, page_size, movie_ids, exclude_movie_ids, score_threshold)
            .await
    }

    /// 建会话 + 补排除条件 + 检索第一页。
    async fn finish_session(
        &self,
        vector: Vec<f32>,
        page_size: i64,
        movie_ids: Option<Vec<i64>>,
        exclude_movie_ids: Option<Vec<i64>>,
        score_threshold: Option<f64>,
    ) -> Result<PlotImageSearchSessionPage, ServiceError> {
        let session = super::image_search::create_session(
            &self.links,
            vector.clone(),
            page_size,
            score_threshold,
            self.limits.session_ttl_seconds,
        )
        .await?;
        // 补排除条件。`NewImageSearchSession` 没有这两个字段（见 image_search 的
        // 缺口说明），所以要单独写 —— 紧跟创建，不留窗口期。
        self.links
            .set_filters(&session.session_id, to_i32(movie_ids.as_deref()), to_i32(exclude_movie_ids.as_deref()))
            .await?;
        let page = self
            .search_page(
                &vector,
                movie_ids.as_deref(),
                exclude_movie_ids.as_deref(),
                score_threshold,
                page_size,
                0,
            )
            .await?;
        Ok(PlotImageSearchSessionPage {
            session_id: session.session_id,
            status: session.status,
            page_size,
            next_cursor: page.next_cursor,
            items: page.items,
        })
    }

    /// 翻页。
    ///
    /// **404**（会话不存在或过期）与**400**（游标非法）两种错误都要能出现 ——
    /// 与图搜一致。
    pub async fn list_results(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<PlotImageSearchSessionPage, ServiceError> {
        super::image_search::purge_expired_sessions().await?;
        let session = super::image_search::require_session(&self.links, session_id).await?;
        let offset = match cursor {
            Some(cursor) => decode_cursor(cursor)?,
            None => 0,
        };
        let vector = super::image_search::session_vector(&session)?;
        let movie_ids = super::image_search::session_id_list(session.movie_ids.as_deref());
        let exclude_movie_ids =
            super::image_search::session_id_list(session.exclude_movie_ids.as_deref());
        let page = self
            .search_page(
                &vector,
                movie_ids.as_deref(),
                exclude_movie_ids.as_deref(),
                session.score_threshold,
                session.page_size as i64,
                offset as usize,
            )
            .await?;
        Ok(PlotImageSearchSessionPage {
            session_id: session.session_id,
            status: session.status,
            page_size: session.page_size as i64,
            next_cursor: page.next_cursor,
            items: page.items,
        })
    }
}