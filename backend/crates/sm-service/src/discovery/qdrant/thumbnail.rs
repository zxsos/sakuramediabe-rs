//! 缩略图向量存储。
//!
//! 上游 `qdrant_thumbnail_store.py`。共用逻辑在
//! [`crate::discovery::qdrant::dense`]，这里只提供差异：记录 → point 的
//! 映射，以及命中 → 结构体的解析。

use qdrant_client::qdrant::{point_id::PointIdOptions, Value as PayloadValue};
use qdrant_client::Payload;

use crate::discovery::qdrant::dense::{
    normalize_score, DenseStore, THUMBNAIL_COLLECTION, THUMBNAIL_PAYLOAD_INDEX,
};

/// 一条缩略图向量记录（上游 `:21-26`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ThumbnailVectorRecord {
    /// 点 id = 缩略图 id。
    pub thumbnail_id: i64,
    pub media_id: i64,
    pub movie_id: i64,
    /// 该帧在视频里的秒偏移。
    pub offset_seconds: i64,
    pub vector: Vec<f32>,
}

/// 一条检索命中（上游 `:29-34`）。`score` 已归一化到 `[0, 1]`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThumbnailVectorSearchHit {
    pub thumbnail_id: i64,
    pub media_id: i64,
    pub movie_id: i64,
    pub offset_seconds: i64,
    pub score: f32,
}

/// 缩略图向量存储。
///
/// 同样不 derive `Debug`（内层 [`DenseStore`] 没有 `Debug`）。
#[derive(Clone)]
pub struct ThumbnailVectorStore {
    inner: DenseStore,
}

impl ThumbnailVectorStore {
    /// 连上 Qdrant 并绑定缩略图集合。
    pub fn connect(url: &str, api_key: Option<&str>) -> Result<Self, crate::error::ServiceError> {
        Ok(Self {
            inner: DenseStore::connect(
                url,
                api_key,
                THUMBNAIL_COLLECTION,
                THUMBNAIL_PAYLOAD_INDEX,
            )?,
        })
    }

    /// 用已有核心构造（测试用）。
    pub fn with_store(inner: DenseStore) -> Self {
        Self { inner }
    }

    /// 底层核心。
    pub fn inner(&self) -> &DenseStore {
        &self.inner
    }

    /// 建表 / 校验。`vector_size` 来自 `EmbeddingSpace::dimension`。
    pub async fn ensure_table(&self, vector_size: usize) -> Result<(), crate::error::ServiceError> {
        self.inner.ensure_table(vector_size).await
    }

    /// 建 payload 索引（`movie_id` + `media_id`）。
    pub async fn ensure_scalar_indices(&self) -> Result<(), crate::error::ServiceError> {
        self.inner.ensure_scalar_indices().await
    }

    /// 批量写入。上游 `upsert_records`（`:323`）。
    pub async fn upsert_records(
        &self,
        records: &[ThumbnailVectorRecord],
    ) -> Result<(), crate::error::ServiceError> {
        if records.is_empty() {
            return Ok(());
        }
        let points = records
            .iter()
            .map(|record| {
                let mut payload = Payload::new();
                payload.insert("media_id", PayloadValue::from(record.media_id));
                payload.insert("movie_id", PayloadValue::from(record.movie_id));
                payload.insert("offset_seconds", PayloadValue::from(record.offset_seconds));
                qdrant_client::qdrant::PointStruct::new(
                    record.thumbnail_id as u64,
                    record.vector.clone(),
                    payload,
                )
            })
            .collect();
        self.inner.upsert_points(points).await
    }

    /// 按缩略图 id 删除。
    pub async fn delete_by_thumbnail_ids(
        &self,
        thumbnail_ids: &[i64],
    ) -> Result<(), crate::error::ServiceError> {
        self.inner.delete_ids(thumbnail_ids).await
    }

    /// 按 media id 删除 —— 删一部影片的所有缩略图向量。
    pub async fn delete_by_media_id(
        &self,
        media_id: i64,
    ) -> Result<(), crate::error::ServiceError> {
        self.inner.delete_where_field("media_id", &[media_id]).await
    }

    /// 清空（删整个集合）。上游 `clear`（`:369`）。
    pub async fn clear(&self) -> Result<(), crate::error::ServiceError> {
        self.inner.clear().await
    }

    /// 精确计数。
    pub async fn count(&self) -> Result<u64, crate::error::ServiceError> {
        self.inner.count().await
    }

    /// 检索。**永不失败** —— 集合不存在或查询出错都返回空列表，
    /// 理由见 [`crate::discovery::qdrant::dense`] 模块文档第 1 条。
    pub async fn search(
        &self,
        query_vector: &[f32],
        limit: usize,
        offset: usize,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
    ) -> Result<Vec<ThumbnailVectorSearchHit>, crate::error::ServiceError> {
        let points = self
            .inner
            .search(
                query_vector.to_vec(),
                limit,
                offset,
                movie_ids,
                exclude_movie_ids,
            )
            .await?;
        Ok(points.iter().filter_map(parse_hit).collect())
    }

    /// 集合状态。
    pub async fn status(
        &self,
    ) -> Result<crate::discovery::qdrant::dense::DenseStoreStatus, crate::error::ServiceError> {
        self.inner.status().await
    }
}

/// 命中 → 结构体。字段缺失或类型不对就**丢掉这一条**（返回 `None`）。
///
/// 上游是直接 `payload["media_id"]`，缺键会抛 `KeyError` 让整个查询炸掉
/// （`:439-441`）。这里改成跳过并 warn —— 一条脏数据不该让整页图搜变 500。
/// **这是对上游的有意偏离**，理由是健壮性；行为差异记在这里以免被当成 bug。
fn parse_hit(point: &qdrant_client::qdrant::ScoredPoint) -> Option<ThumbnailVectorSearchHit> {
    use crate::discovery::qdrant::plot_image::payload_i64;

    // `ScoredPoint.payload` 是裸 `HashMap<String, Value>`，直接 `get`。
    let media_id = payload_i64(&point.payload, "media_id")?;
    let movie_id = payload_i64(&point.payload, "movie_id")?;
    let offset_seconds = payload_i64(&point.payload, "offset_seconds")?;
    // `ScoredPoint.id` 是 `Option<PointId>`，而 `PointId` 又是 oneof
    // （`point_id_options`，数字分支叫 `Num`）。UUID 形式的点 id 在这个
    // 集合里不该出现（我们总是用 i64 缩略图 id），出现就跳过，别把它
    // 伪装成一个数字。
    let thumbnail_id = match point.id.as_ref().map(|id| &id.point_id_options) {
        Some(Some(PointIdOptions::Num(num))) => *num,
        _ => {
            tracing::warn!(id = ?point.id, "缩略图集合里出现非数字点 id，已跳过");
            return None;
        }
    };
    Some(ThumbnailVectorSearchHit {
        thumbnail_id: thumbnail_id as i64,
        media_id,
        movie_id,
        offset_seconds,
        score: normalize_score(point.score),
    })
}
