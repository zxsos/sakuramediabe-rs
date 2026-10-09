//! 剧照向量存储。
//!
//! 上游 `qdrant_plot_image_store.py`。那个类**继承** `QdrantThumbnailStore`，
//! 只覆盖四样：集合名、payload 索引字段、记录 → point、命中解析。
//! 共用部分全在 [`crate::discovery::qdrant::dense`]。
//!
//! 与缩略图的差异只有两点：payload 只有 `movie_id`（没有 `media_id` /
//! `offset_seconds`），点 id 是 `plot_image_id`。

use std::collections::HashMap;

use qdrant_client::qdrant::{
    point_id::PointIdOptions, value::Kind as PayloadKind, PointStruct, ScoredPoint,
    Value as PayloadValue,
};
use qdrant_client::Payload;

use crate::discovery::qdrant::dense::{
    normalize_score, DenseStore, DenseStoreStatus, PLOT_IMAGE_COLLECTION, PLOT_IMAGE_PAYLOAD_INDEX,
};
use crate::error::ServiceError;

/// 一条剧照向量记录（上游 `:10-13`）。
#[derive(Debug, Clone, PartialEq)]
pub struct PlotImageVectorRecord {
    /// 点 id = 剧照 id。
    pub plot_image_id: i64,
    pub movie_id: i64,
    pub vector: Vec<f32>,
}

/// 一条检索命中（上游 `:16-19`）。`score` 已归一化到 `[0, 1]`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotImageVectorSearchHit {
    pub plot_image_id: i64,
    pub movie_id: i64,
    pub score: f32,
}

/// 剧照向量存储。
///
/// 同样不 derive `Debug`（内层 [`DenseStore`] 没有 `Debug`）。
#[derive(Clone)]
pub struct PlotImageVectorStore {
    inner: DenseStore,
}

impl PlotImageVectorStore {
    /// 连上 Qdrant 并绑定剧照集合。
    pub fn connect(url: &str, api_key: Option<&str>) -> Result<Self, ServiceError> {
        Ok(Self {
            inner: DenseStore::connect(
                url,
                api_key,
                PLOT_IMAGE_COLLECTION,
                PLOT_IMAGE_PAYLOAD_INDEX,
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

    /// 建表 / 校验。
    pub async fn ensure_table(&self, vector_size: usize) -> Result<(), ServiceError> {
        self.inner.ensure_table(vector_size).await
    }

    /// 建 payload 索引（只有 `movie_id`）。
    pub async fn ensure_scalar_indices(&self) -> Result<(), ServiceError> {
        self.inner.ensure_scalar_indices().await
    }

    /// 批量写入。payload 只有 `movie_id`（上游 `:34`）。
    pub async fn upsert_records(
        &self,
        records: &[PlotImageVectorRecord],
    ) -> Result<(), ServiceError> {
        if records.is_empty() {
            return Ok(());
        }
        let points = records
            .iter()
            .map(|record| {
                let mut payload = Payload::new();
                payload.insert("movie_id", PayloadValue::from(record.movie_id));
                PointStruct::new(record.plot_image_id as u64, record.vector.clone(), payload)
            })
            .collect();
        self.inner.upsert_points(points).await
    }

    /// 按剧照 id 删除（上游 `:40-41` 直接转调 `delete_by_thumbnail_ids`）。
    pub async fn delete_by_plot_image_ids(
        &self,
        plot_image_ids: &[i64],
    ) -> Result<(), ServiceError> {
        self.inner.delete_ids(plot_image_ids).await
    }

    /// 清空（删整个集合）。
    pub async fn clear(&self) -> Result<(), ServiceError> {
        self.inner.clear().await
    }

    /// 精确计数。
    pub async fn count(&self) -> Result<u64, ServiceError> {
        self.inner.count().await
    }

    /// 检索。**永不失败**，理由同缩略图。
    pub async fn search(
        &self,
        query_vector: &[f32],
        limit: usize,
        offset: usize,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
    ) -> Result<Vec<PlotImageVectorSearchHit>, ServiceError> {
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
    pub async fn status(&self) -> Result<DenseStoreStatus, ServiceError> {
        self.inner.status().await
    }
}

/// 命中 → 结构体。字段缺失就跳过这条，理由同 `thumbnail.rs` 的 `parse_hit`
/// （对上游「缺键就炸整个查询」的有意偏离）。
fn parse_hit(point: &ScoredPoint) -> Option<PlotImageVectorSearchHit> {
    let movie_id = payload_i64(&point.payload, "movie_id")?;
    // `ScoredPoint.id` 是 `Option<PointId>`，而 `PointId` 又是 oneof
    // （`point_id_options`，数字分支叫 `Num`）。UUID 形式的点 id 在这个
    // 集合里不该出现（我们总是用 i64 剧照 id），出现就跳过，别把它
    // 伪装成一个数字。
    let plot_image_id = match point.id.as_ref().map(|id| &id.point_id_options) {
        Some(Some(PointIdOptions::Num(num))) => *num,
        _ => {
            tracing::warn!(id = ?point.id, "剧照集合里出现非数字点 id，已跳过");
            return None;
        }
    };
    Some(PlotImageVectorSearchHit {
        plot_image_id: plot_image_id as i64,
        movie_id,
        score: normalize_score(point.score),
    })
}

/// 从 `ScoredPoint.payload` 里取整数。
///
/// `ScoredPoint.payload` 是**裸 `HashMap<String, Value>`**（不是写入时用的
/// `Payload` 包装类型），所以直接 `get` 就行，不需要 `deserialize` 转换。
/// `Value` 本身是 struct + `value::Kind` oneof，整数分支叫 `IntegerValue`。
///
/// **不复用上游「`int(payload[...])` 强转」的口径**：那会把 `"12"` 这种
/// 脏值悄悄变成 12，而我们要的是「脏数据就丢掉这条」。
pub(crate) fn payload_i64(payload: &HashMap<String, PayloadValue>, key: &str) -> Option<i64> {
    match payload.get(key)? {
        PayloadValue {
            kind: Some(PayloadKind::IntegerValue(value)),
        } => Some(*value),
        other => {
            tracing::warn!(key, value = ?other, "payload 字段不是整数，已跳过该条");
            None
        }
    }
}
