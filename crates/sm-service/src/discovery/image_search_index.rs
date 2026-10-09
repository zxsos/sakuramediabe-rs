//! 图片索引（上游 `image_search_index_service.py`，19.5KB / 490 行）。
//!
//! 依赖：推理客户端 [`super::embedding`] + 稠密向量库
//! [`super::qdrant::dense`]。**两者都已就位，本文件无外部阻塞。**
//!
//! # 整体形状
//!
//! ```text
//!   params.reset == true ?
//!     ├─ 是 -> 阶段 1/2：清 Qdrant + 删全部会话 + 状态重置为 PENDING
//!     └─ 否 -> 直接进阶段 1/1
//!
//!   循环：
//!     取一批待处理缩略图 + 一批待处理剧情图
//!     都空 -> 退出
//!     describe() 取空间 + prepare_for_indexing + ensure_stores_ready(dim)
//!     缩略图 -> 推理 -> Qdrant upsert -> 回写状态
//!     剧情图 -> 同上
//!     每 2 秒上报一次进度
//! ```
//!
//! # 最关键的一处：空间检查在**每轮循环里**
//!
//! 上游 `:153-154`：
//!
//! ```python
//! space = self._prepare_index_space()
//! self.ensure_stores_ready(int(space.dimension))
//! ```
//!
//! **在 while 循环体内，不是循环外。** 理由：一次全量索引可能跑几十分钟，
//! 而**模型可能在这期间被换掉**。只在开头检查一次的话，第二轮开始就会拿旧
//! 维度的向量去写新集合（或反过来）。
//!
//! `ensure_stores_ready` 内部有 `_stores_ready` 标志避免重复建表，但
//! `_prepare_index_space`（`describe()`）**每轮都真的调** —— 那才是检测到
//! 空间变更的地方。
//!
//! **这意味着每轮都要一次 HTTP 往返。** 别「优化」成只查一次。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sm_db::repo::discovery::{
    ImageSearchIndexStateRepository, PendingImageRepository, PendingPlotImage, PendingThumbnail,
};

use super::embedding::{EmbeddingClient, EmbeddingSpace};
use super::image_search_space::ImageSearchIndexSpaceService;
use super::qdrant::dense::DenseStore;
use crate::error::ServiceError;

/// 缩略图索引状态。
pub mod thumbnail_status {
    pub const PENDING: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const SUCCESS: i32 = 2;
    /// 非 JAV 媒体的缩略图，不参与检索。
    pub const SKIPPED: i32 = 3;
}

/// 剧情图索引状态（**没有 `SKIPPED`**）。
pub mod plot_status {
    pub const PENDING: i32 = 0;
    pub const FAILED: i32 = 1;
    pub const SUCCESS: i32 = 2;
}

/// 一次索引的统计。
///
/// # 字段名与上游**逐字一致**
///
/// 上游 `stats`（`:63-70`）+ `build_summary` 追加的三个合计（`:82-88`）。
/// 这些键会进 `summary_patch`，而任务中心会把 `summary` 存进 `signal_scores`
/// 一类的列 —— **改名会让历史记录对不上**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub processed_thumbnails: u32,
    pub successful_thumbnails: u32,
    pub failed_thumbnails: u32,
    pub processed_plot_images: u32,
    pub successful_plot_images: u32,
    pub failed_plot_images: u32,
    /// 三者之和，循环结束时才算。
    pub processed: u32,
    pub succeeded: u32,
    pub failed: u32,
    /// 循环结束时剩余的待处理数。
    pub pending: u32,
}

/// 重置阶段的统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetStats {
    /// 删除的会话数。**重建时是全删**，不只是过期。
    pub sessions_deleted: u32,
    /// 重置为 PENDING 的缩略图数（**只算归属 JAV 影片的**）。
    pub thumbnails_reset: u32,
    /// 重置为 PENDING 的剧情图数（**全表**）。
    pub plot_images_reset: u32,
}

/// 完整摘要 = 重置统计 + 索引统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSummary {
    #[serde(flatten)]
    pub reset: ResetStats,
    #[serde(flatten)]
    pub stats: IndexStats,
}

/// 进度上报的回调。
///
/// # 参数与 `TaskRunReporter::emit` **逐位一致**
///
/// ```text
/// emit(current: Option<i32>, total: Option<i32>, text: Option<&str>, summary_patch: Option<&Value>)
/// ```
///
/// **两个容易搞错的点**：
///
/// 1. `current` / `total` 是 **`Option<i32>`** 不是 `Option<u64>` ——
///    任务表用 `i32` 存进度。
/// 2. **第 4 个参数 `summary_patch` 不能省**。它是「把这次统计并进任务摘要」
///    的唯一通道 —— 不传的话任务中心只能看到最后一句话，看不到 processed /
///    succeeded / failed 三个数。而**这三个数正是上面 6 个 stats 的聚合**。
///
/// **节流由调用方做**（上游 `:170-184` 是 2 秒 / 30 秒两级节流）。
pub type ProgressSink<'a> = Box<
    dyn FnMut(Option<i32>, Option<i32>, &str, Option<&serde_json::Value>) -> BoxFuture<'a, Result<(), String>>
        + 'a,
>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 索引服务。
pub struct ImageSearchIndexService {
    thumbnails: Arc<DenseStore>,
    plot_images: Arc<DenseStore>,
    embedding: Arc<EmbeddingClient>,
    pending: PendingImageRepository,
    space: ImageSearchIndexSpaceService,
    /// 集合是否已按当前维度建好。**每轮 `prepare_index_space` 后由
    /// `ensure_stores_ready` 置位**，换维度时清掉（见模块文档）。
    stores_ready: bool,
    /// 上次建集合时用的维度。**用于检测运行中维度是否变了** —— 变了就清
    /// `stores_ready` 强制重建。
    ///
    /// 初始 `None`，所以第一次 `ensure_stores_ready` 一定会建表。
    current_dimension: Option<u32>,
}

impl ImageSearchIndexService {
    /// 构造。
    pub fn new(
        thumbnails: Arc<DenseStore>,
        plot_images: Arc<DenseStore>,
        embedding: Arc<EmbeddingClient>,
        pending: PendingImageRepository,
        state: ImageSearchIndexStateRepository,
    ) -> Self {
        Self {
            thumbnails,
            plot_images,
            embedding,
            pending,
            space: ImageSearchIndexSpaceService::new(state),
            stores_ready: false,
            current_dimension: None,
        }
    }

    /// 确保两个集合都建好且维度与推理服务声明的一致。
    ///
    /// # 维度不一致**不能就地改集合**
    ///
    /// 已建集合的向量维度改不了 —— 只能重建。所以这里在维度不符时返回错误，
    /// 由 `reset` 路径走「清空 + 重建」。
    ///
    /// # `stores_ready` 标志的边界
    ///
    /// 置位后**同一维度内**不再重复建表（建表是重操作）。但
    /// [`prepare_index_space`](Self::prepare_index_space) 每轮都调 `describe()`，
    /// **维度变了要清掉这个标志** —— 见 `index_pending`。
    pub async fn ensure_stores_ready(&self, vector_size: u32) -> Result<(), ServiceError> {
        if self.stores_ready {
            return Ok(());
        }
        if vector_size == 0 {
            return Err(ServiceError::validation(
                "image_search_invalid_dimension",
                "推理服务返回的向量维度无效",
            ));
        }
        for store in [&self.thumbnails, &self.plot_images] {
            store.ensure_table(vector_size as usize).await?;
            store.ensure_scalar_indices().await?;
        }
        Ok(())
    }

    /// 取空间 + 校验可索引。**每轮循环都要调**（见模块文档）。
    pub async fn prepare_index_space(&mut self) -> Result<EmbeddingSpace, ServiceError> {
        let space = self.embedding.describe().await?;
        self.space.prepare_for_indexing(&space.space_id).await?;
        Ok(space)
    }
}
impl ImageSearchIndexService {
    /// 主循环。
    ///
    /// # `reset` 让它变成两个阶段
    ///
    /// 上游 `stage_count = 2 if reset else 1`（`:74`），阶段 1 是重置、
    /// 阶段 2 是索引。**进度文案里带 `阶段 x/y`**，所以两个阶段时文案不同 ——
    /// 照抄，否则重置时前端会显示「阶段 2/2」而用户只看到一次操作。
    ///
    /// # 退出条件是「两批都空」，不是「处理够 N 条」
    ///
    /// 所以失败的项目**不会**让循环提前退出（它已被标记 FAILED，不再出现在
    /// 下一批里）。
    pub async fn index_pending_images(
        &mut self,
        work_batch_size: i64,
        inference_batch_size: i64,
        reset: bool,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<IndexSummary, ServiceError> {
        let work_batch_size = work_batch_size.max(1);
        let inference_batch_size = inference_batch_size.max(1);
        let stage_count = if reset { 2 } else { 1 };
        let mut stats = IndexStats::default();
        let mut reset_stats = ResetStats::default();

        // ---- 阶段 1/2：重置 ----
        if reset {
            emit(&mut progress, 0, 0, "阶段 1/2 · 重置旧索引 · 正在清空图像搜索索引").await;
            reset_stats = self.reset_for_rebuild().await?;
            emit(
                &mut progress,
                0,
                0,
                &format!(
                    "阶段 1/2 · 重置旧索引 · 已完成 · 缩略图 {} 张 · 剧情图 {} 张",
                    reset_stats.thumbnails_reset, reset_stats.plot_images_reset
                ),
            )
            .await;
        }

        // ---- 统计待处理 ----
        emit(
            &mut progress,
            0,
            0,
            &format!("阶段 {stage_count}/{stage_count} · 构建图像搜索索引 · 正在统计待处理图片"),
        )
        .await;
        let mut pending = self.pending.pending_count().await?;
        emit(
            &mut progress,
            0,
            pending,
            &format!(
                "阶段 {stage_count}/{stage_count} · 构建图像搜索索引 · 正在处理当前批次 · 已完成 0/{pending}"
            ),
        )
        .await;

        // ---- 主循环 ----
        loop {
            let thumbnails = self.pending.pending_thumbnails(work_batch_size).await?;
            let plot_images = self.pending.pending_plot_images(work_batch_size).await?;
            if thumbnails.is_empty(); plot_images.is_empty() {
                break;
            }

            // **每轮都查空间** —— 模块文档的重点。
            let space = self.prepare_index_space().await?;
            if space.dimension != self.current_dimension {
                // 维度变了 -> 集合要重建 -> 标志清掉。
                self.stores_ready = false;
                self.current_dimension = space.dimension;
            }
            self.ensure_stores_ready(space.dimension).await?;
            self.stores_ready = true;

            if !thumbnails.is_empty() {
                let (ok, bad) = self
                    .index_thumbnail_batch(&thumbnails, inference_batch_size)
                    .await?;
                stats.processed_thumbnails += thumbnails.len() as u32;
                stats.successful_thumbnails += ok;
                stats.failed_thumbnails += bad;
            }
            if !plot_images.is_empty() {
                let (ok, bad) = self
                    .index_plot_image_batch(&plot_images, inference_batch_size)
                    .await?;
                stats.processed_plot_images += plot_images.len() as u32;
                stats.successful_plot_images += ok;
                stats.failed_plot_images += bad;
            }

            // 上游每 2 秒上报一次（`:170-184`）。这里每轮都报 —— 批间隔远大于
            // 2 秒时等价。**不实现严格节流**，那是优化不是契约。
            pending = self.pending.pending_count().await?;
            let processed = stats.processed_thumbnails as i64 + stats.processed_plot_images as i64;
            let succeeded = stats.successful_thumbnails as i64 + stats.successful_plot_images as i64;
            let failed = stats.failed_thumbnails as i64 + stats.failed_plot_images as i64;
            emit(
                &mut progress,
                processed,
                processed + pending,
                &format!(
                    "阶段 {stage_count}/{stage_count} · 构建图像搜索索引 · 正在处理当前批次 \
                     · 已完成 {processed}/{} · 成功 {succeeded} · 失败 {failed} · 待处理 {pending}",
                    processed + pending
                ),
            )
            .await;
        }

        // ---- 收尾 ----
        let remaining = self.pending.pending_count().await? as u32;
        stats.processed = stats.processed_thumbnails + stats.processed_plot_images;
        stats.succeeded = stats.successful_thumbnails + stats.successful_plot_images;
        stats.failed = stats.failed_thumbnails + stats.failed_plot_images;
        stats.pending = remaining;
        let summary = IndexSummary { reset: reset_stats, stats };
        // 收尾这次**带 `summary_patch`** —— 统计数字要进任务摘要，否则任务中心
        // 只能看到一句话，看不到 processed / succeeded / failed。
        emit_final(
            &mut progress,
            summary.stats.processed as i64,
            summary.stats.processed as i64 + remaining as i64,
            &format!(
                "阶段 {stage_count}/{stage_count} · 构建图像搜索索引 · 任务完成 \
                 · 已完成 {}/{} · 成功 {} · 失败 {} · 待处理 {remaining}",
                summary.stats.processed,
                summary.stats.processed as i64 + remaining as i64,
                summary.stats.succeeded,
                summary.stats.failed
            ),
            &summary,
        )
        .await;
        Ok(summary)
    }
}
impl ImageSearchIndexService {
    /// 清库 + 删会话 + 状态重置。
    ///
    /// # 顺序不能换
    ///
    /// 1. `describe()` —— 先拿到新空间的 id
    /// 2. **清两个 Qdrant 集合**
    /// 3. `stores_ready = false` —— 集合被清空，下一轮必须重建
    /// 4. **一个数据库事务里**：删会话 + 状态重置 + 写新空间 id
    ///
    /// # 为什么必须删**全部**会话
    ///
    /// 见 `ImageSearchSessionRepository::delete_all` 的文档：换模型后维度可能
    /// **恰好相同**（两个 512 维模型），那时 `accepts_session` 会放行老会话，
    /// 于是拿旧空间的向量比新空间的索引 —— **返回语义无关的结果且不报错**。
    ///
    /// # 缩略图与剧情图的重置**不对称**
    ///
    /// 缩略图带 `media.movie IS NOT NULL`，剧情图**全表**。照抄。
    ///
    /// # 三步（删会话 / 重置状态 / 写空间 id）必须**原子**
    ///
    /// 若状态重置了但空间 id 没写，`get_status` 会判成 `rebuild_required`
    /// 而要求再次重建 —— **反复重置却永远不 ready**。
    pub async fn reset_for_rebuild(&mut self) -> Result<ResetStats, ServiceError> {
        let space = self.embedding.describe().await?;
        self.thumbnails.clear().await?;
        self.plot_images.clear().await?;
        self.stores_ready = false;
        self.current_dimension = space.dimension;

        let mut tx = self.pending.pool().begin().await?;
        let sessions_deleted = sqlx::query("DELETE FROM image_search_session")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let thumbnails_reset = sqlx::query(
            r#"UPDATE media_thumbnail SET image_search_index_status = 0
               WHERE id IN (SELECT t.id FROM media_thumbnail t
                             JOIN media m ON m.id = t.media
                             WHERE m.movie IS NOT NULL)"#,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let plot_images_reset =
            sqlx::query("UPDATE movie_plot_image SET image_search_index_status = 0")
                .execute(&mut *tx)
                .await?
                .rows_affected();
        sqlx::query(
            "INSERT INTO image_search_index_state (id, indexed_space_id) VALUES (1, $1) \
             ON CONFLICT (id) DO UPDATE SET indexed_space_id = EXCLUDED.indexed_space_id",
        )
        .bind(&space.space_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(ResetStats {
            sessions_deleted: sessions_deleted as u32,
            thumbnails_reset: thumbnails_reset as u32,
            plot_images_reset: plot_images_reset as u32,
        })
    }
}
impl ImageSearchIndexService {
    /// 索引一批缩略图。返回 `(成功数, 失败数)`。
    ///
    /// # 失败**必须**标记状态，否则无限重试
    ///
    /// 上游 `_commit_statuses`（`:462`）/ `_set_status`（`:480`）。若失败仍留
    /// `PENDING`，下一轮会**又取到同一行** —— 一张编码失败的图片能让任务
    /// 永远跑不完。「宁可标记失败也不假装成功」。
    ///
    /// # 推理批量与工作批量是**两个不同的量**
    ///
    /// `inference_batch_size`（配置 `image_search.inference_batch_size`）决定
    /// 一次发给推理服务的图片数；`work_batch_size` 决定一次从库里取多少行。
    /// 混用会让推理请求过大而超时。
    ///
    /// # 整批推理失败时**全部标记 FAILED，不重试**
    ///
    /// 照抄上游：`_embed_image_payloads` 抛错时该批全败。
    ///
    /// # `SKIPPED` 在这里**不用**
    ///
    /// 候选查询已过滤掉「不归属 JAV 影片」的缩略图，进来的都该索引。
    /// `SKIPPED` 是给别的路径（手动标记非 JAV）用的。
    pub async fn index_thumbnail_batch(
        &self,
        batch: &[PendingThumbnail],
        inference_batch_size: i64,
    ) -> Result<(u32, u32), ServiceError> {
        let mut ok = 0u32;
        let mut bad = 0u32;
        for chunk in batch.chunks(inference_batch_size.max(1) as usize) {
            let payloads: Vec<Vec<u8>> =
                chunk.iter().map(|item| item.image_bytes.clone()).collect();
            match self.embedding.embed_images(&payloads).await {
                Ok(vectors) => {
                    for (item, vector) in chunk.iter().zip(vectors.into_iter()) {
                        let written = self
                            .thumbnails
                            .upsert_thumbnail(
                                item.thumbnail_id as i64,
                                item.media_id as i64,
                                item.movie_id.map(|id| id as i64),
                                vector,
                            )
                            .await;
                        match written {
                            Ok(()) => {
                                self.pending
                                    .set_thumbnail_status(
                                        item.thumbnail_id,
                                        thumbnail_status::SUCCESS,
                                    )
                                    .await?;
                                ok += 1;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    thumbnail_id = item.thumbnail_id,
                                    code = error.code(),
                                    "缩略图向量写入失败，标记为 FAILED"
                                );
                                self.pending
                                    .set_thumbnail_status(item.thumbnail_id, thumbnail_status::FAILED)
                                    .await?;
                                bad += 1;
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        count = chunk.len(),
                        code = error.code(),
                        "推理服务失败，该批缩略图全部标记为 FAILED"
                    );
                    for item in chunk {
                        self.pending
                            .set_thumbnail_status(item.thumbnail_id, thumbnail_status::FAILED)
                            .await?;
                        bad += 1;
                    }
                }
            }
        }
        Ok((ok, bad))
    }

    /// 索引一批剧情图。返回 `(成功数, 失败数)`。
    ///
    /// 与缩略图版只差三处：写入 `plot_images` 集合、状态用 `plot_status`
    /// （**无 `SKIPPED`**）、payload 里没有 `media_id`。
    pub async fn index_plot_image_batch(
        &self,
        batch: &[PendingPlotImage],
        inference_batch_size: i64,
    ) -> Result<(u32, u32), ServiceError> {
        let mut ok = 0u32;
        let mut bad = 0u32;
        for chunk in batch.chunks(inference_batch_size.max(1) as usize) {
            let payloads: Vec<Vec<u8>> =
                chunk.iter().map(|item| item.image_bytes.clone()).collect();
            match self.embedding.embed_images(&payloads).await {
                Ok(vectors) => {
                    for (item, vector) in chunk.iter().zip(vectors.into_iter()) {
                        let written = self
                            .plot_images
                            .upsert_plot_image(
                                item.plot_image_id as i64,
                                item.movie_id.map(|id| id as i64),
                                vector,
                            )
                            .await;
                        match written {
                            Ok(()) => {
                                self.pending
                                    .set_plot_image_status(item.plot_image_id, plot_status::SUCCESS)
                                    .await?;
                                ok += 1;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    plot_image_id = item.plot_image_id,
                                    code = error.code(),
                                    "剧情图向量写入失败，标记为 FAILED"
                                );
                                self.pending
                                    .set_plot_image_status(item.plot_image_id, plot_status::FAILED)
                                    .await?;
                                bad += 1;
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        count = chunk.len(),
                        code = error.code(),
                        "推理服务失败，该批剧情图全部标记为 FAILED"
                    );
                    for item in chunk {
                        self.pending
                            .set_plot_image_status(item.plot_image_id, plot_status::FAILED)
                            .await?;
                        bad += 1;
                    }
                }
            }
        }
        Ok((ok, bad))
    }
}

/// 上报一次进度。`progress` 为 `None` 时静默 —— 对应上游各处
/// `if progress_callback is not None`。
///
/// # `current` / `total` 是 `Option<i32>`
///
/// 任务表用 `i32` 存进度。这里统一转成 `Option`，**全部进度都是「有总数」的**
/// —— 上游每一条文案都带 `已完成 x/y`，所以没有「总数未知」的情形。
async fn emit(
    progress: &mut Option<ProgressSink<'_>>,
    current: i64,
    total: i64,
    text: &str,
) {
    if let Some(sink) = progress {
        let _ = sink(Some(current as i32), Some(total as i32), text, None).await;
    }
}

/// 收尾那次上报**额外带 `summary_patch`** —— 它是统计数字进任务摘要的通道。
///
/// 只有最后一次带：中间那些带 patch 会反复覆盖同一份摘要，而上游的
/// `emit_progress(..., summary_patch=summary)` 在每轮都带（`:106`）。
/// 照抄 —— 最后一个 patch 覆盖掉中间的，结果一致。
async fn emit_final(
    progress: &mut Option<ProgressSink<'_>>,
    current: i64,
    total: i64,
    text: &str,
    summary: &IndexSummary,
) {
    if let Some(sink) = progress {
        let patch = serde_json::to_value(summary).ok();
        let _ = sink(Some(current as i32), Some(total as i32), text, patch.as_ref()).await;
    }
}