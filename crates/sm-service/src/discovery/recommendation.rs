//! 影片推荐（上游 `recommendation_service.py`，15.1KB / 380 行）。
//!
//! 依赖：稀疏向量库 [`super::qdrant::similarity::MovieSimilarityStore`]（已就位）
//! 与影片特征查询（`sm_db::repo::recommendation::MovieFeatureRepository`）。
//!
//! # 本模块是推荐族的**底座**
//!
//! [`super::daily_recommendation`] 与 [`super::moment_recommendation`] 都消费本
//! 模块产出的相似影片。所以本模块先落地，另两个才有意义。
//!
//! # 稀疏向量的形状：**演员与标签在不相交的索引空间**
//!
//! ```text
//!   演员 actor_id -> 索引 actor_id * 2      （偶数）
//!   标签 tag_id   -> 索引 tag_id * 2 + 1  （奇数）
//! ```
//!
//! `*2` / `*2+1` 把两类特征**放在同一个稀疏向量里但索引永不相撞**。这是整个
//! 设计里最关键的一步 —— 不用它就得开两个向量空间，而 Qdrant 的稀疏索引
//! 只支持一个。
//!
//! # 三处最容易照抄错的地方
//!
//! **1. 索引必须排序。** 上游 `weighted_features.sort(key=lambda item: item[0])`
//! （`:161`）。稀疏向量的 `indices` 要求**严格升序** —— 不排序 Qdrant 会拒收
//! 或返回错误结果。
//!
//! **2. DF 缺失取 0 是正常路径，不是兜底。** 上游注释（`:137`）：「重建期间
//! 新入库的演员/标签取 DF=0（IDF 拉满）」—— 新特征因此更容易被匹配上。
//!
//! **3. 两类特征各自归一化后再按权重缩放。**
//!
//! ```text
//!   actor_norm = sqrt(Σ idf²)
//!   actor_scale = sqrt(SIM_WEIGHT_ACTOR) / actor_norm
//! ```
//!
//! 注意是 **`sqrt(0.6)`** 而不是 `0.6` —— 因为稀疏点积的相似度里每类贡献与
//! 权重的**平方**成正比，取平方根才能让 0.6/0.4 的比例在最终相似度上成立。
//! 写成 `0.6` 会让演员权重实际变成 0.36。
//!
//! # 一处安全细节：**别名切换失败时什么都不删**
//!
//! 上游 `:277` 的注释：「alias 切换结果存在网络层歧义，切换报错时不能删除
//! 可能已激活的新集合」。切换请求可能**已经生效**只是响应丢了 —— 此时删新
//! 集合等于删掉正在服务的索引。
//!
//! 对照：**写入阶段失败是删新集合重抛**（`:263-272`），那时别名还指着旧
//! 集合，删新的安全。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sm_db::repo::recommendation::MovieFeatureRepository;

use super::qdrant::similarity::{
    MovieSimilarityHit, MovieSimilarityStore, SimilarityQueryError, SparsePoint,
};
use crate::error::ServiceError;

/// 演员在相似度里的权重。对应上游 `SIM_WEIGHT_ACTOR = 0.6`。
pub const SIM_WEIGHT_ACTOR: f64 = 0.6;
/// 标签在相似度里的权重。对应上游 `SIM_WEIGHT_TAG = 0.4`。
pub const SIM_WEIGHT_TAG: f64 = 0.4;
/// 扫描影片的分页大小。对应上游 `FEATURE_PAGE_SIZE = 2000`。
pub const FEATURE_PAGE_SIZE: i64 = 2000;
/// 写入批大小。对应上游 `INDEX_BATCH_SIZE = 1000`。
pub const INDEX_BATCH_SIZE: usize = 1000;

/// 演员索引偏移：`* 2`（偶数）。
pub const ACTOR_INDEX_SCALE: i32 = 2;
/// 标签索引偏移：`* 2 + 1`（奇数）。
pub const TAG_INDEX_OFFSET: i32 = 1;

/// 文档频次表。
#[derive(Debug, Clone, Default)]
pub struct DocumentFrequencies {
    pub actors: HashMap<i32, i64>,
    pub tags: HashMap<i32, i64>,
}

impl DocumentFrequencies {
    /// 演员 DF。**缺失取 0**（见模块文档第 2 条）。
    pub fn actor_df(&self, actor_id: i32) -> i64 {
        self.actors.get(&actor_id).copied().unwrap_or(0)
    }

    /// 标签 DF。**缺失取 0**。
    pub fn tag_df(&self, tag_id: i32) -> i64 {
        self.tags.get(&tag_id).copied().unwrap_or(0)
    }
}

/// 平滑 IDF：`ln((total + 1) / (df + 1)) + 1.0`。
///
/// # `+1` 的两处都不能省
///
/// - 分母 `df + 1`：df=0（**新特征**）时不会除零
/// - 分子 `total + 1`：影片数很小时不至于出现负 IDF
/// - 末尾 `+ 1.0`：保证 IDF 恒为正 —— 否则 `total == df` 时 IDF = 0，
///   那个特征被完全忽略
pub fn idf(df: i64, total_movies: i64) -> f64 {
    (((total_movies + 1) as f64) / ((df + 1) as f64)).ln() + 1.0
}

/// 构造一部影片的稀疏向量。**纯函数** —— 不碰数据库，可直接测。
///
/// 返回 `(indices, values)`，**indices 严格升序**。
///
/// # 一部影片可能**没有向量**
///
/// 既无演员也无标签时返回 `(vec![], vec![])`。**不返回 `None`** ——
/// 上游在 `_iter_movie_features` 就把这种影片跳过了（不 constructions 出向量），
/// 所以「空向量」在这里不该出现；真出现了（数据在扫描期间变了），
/// `upsert_sparse_points` 的 `indices.is_empty()` 检查会挡下。
pub fn build_sparse_vector(
    actor_ids: &[i32],
    tag_ids: &[i32],
    df: &DocumentFrequencies,
    total_movies: i64,
) -> (Vec<u32>, Vec<f32>) {
    let mut weighted: Vec<(u32, f32)> = Vec::with_capacity(actor_ids.len() + tag_ids.len());

    // ---- 演员：偶数索引 ----
    if !actor_ids.is_empty() {
        let idfs: Vec<f64> = actor_ids
            .iter()
            .map(|id| idf(df.actor_df(*id), total_movies))
            .collect();
        let norm = idfs.iter().map(|v| v * v).sum::<f64>().sqrt();
        if norm > 0.0 {
            // **`sqrt(SIM_WEIGHT_ACTOR)`** 而非 `SIM_WEIGHT_ACTOR`（见模块文档第 3 条）
            let scale = SIM_WEIGHT_ACTOR.sqrt() / norm;
            for (actor_id, idf_value) in actor_ids.iter().zip(idfs) {
                let index = (actor_id * ACTOR_INDEX_SCALE) as u32;
                weighted.push((index, (idf_value * scale) as f32));
            }
        }
    }

    // ---- 标签：奇数索引 ----
    if !tag_ids.is_empty() {
        let idfs: Vec<f64> = tag_ids
            .iter()
            .map(|id| idf(df.tag_df(*id), total_movies))
            .collect();
        let norm = idfs.iter().map(|v| v * v).sum::<f64>().sqrt();
        if norm > 0.0 {
            let scale = SIM_WEIGHT_TAG.sqrt() / norm;
            for (tag_id, idf_value) in tag_ids.iter().zip(idfs) {
                let index = (tag_id * ACTOR_INDEX_SCALE + TAG_INDEX_OFFSET) as u32;
                weighted.push((index, (idf_value * scale) as f32));
            }
        }
    }

    // **必须排序**（模块文档第 1 条）
    weighted.sort_by_key(|(index, _)| *index);
    let indices = weighted.iter().map(|(index, _)| *index).collect();
    let values = weighted.iter().map(|(_, value)| *value).collect();
    (indices, values)
}
/// 重建统计。键名与上游**逐字一致**（会进任务摘要）。
// ⚠️ **不能** derive `Copy`：`previous_collection: Option<String>` 带堆分配。
// （骨架期这里写了 `Copy`，编译期就被拦下。）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecomputeStats {
    /// 非集合影片总数（IDF 的分母）。
    pub total_movies: i64,
    /// 实际入索引的影片数。**可能小于 `total_movies`** —— 无特征的影片被跳过。
    pub indexed_movies: i64,
    pub actor_features: i64,
    pub tag_features: i64,
    /// 清理掉的孤儿集合数。
    pub purged_collections: i64,
    /// 蓝绿切换切走的旧集合名。`None` = 首次挂载。
    pub previous_collection: Option<String>,
}

/// 进度上报。签名与 `image_search_index` 那套一致（`Option<i32>` + patch）。
///
/// `+ Send` 的理由同那边：sink 要进 worker 的 handler future（`TaskHandler`
/// 要求 `Send`）。
pub type ProgressSink<'a> = Box<
    dyn FnMut(
            Option<i32>,
            Option<i32>,
            &str,
            Option<&serde_json::Value>,
        ) -> BoxFuture<'a, Result<(), String>>
        + Send
        + 'a,
>;
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 推荐服务。
pub struct MovieRecommendationService {
    store: MovieSimilarityStore,
    features: MovieFeatureRepository,
}

impl MovieRecommendationService {
    /// 构造。
    ///
    /// `store` **按值**持有而 `similarity.rs` 里的 `search_many` 是 `&self` ——
    /// 所以克隆成本只是 `Arc` 计数。
    pub fn new(store: MovieSimilarityStore, features: MovieFeatureRepository) -> Self {
        Self { store, features }
    }

    /// 清理历史遗留集合。
    ///
    /// 别名切换失败或进程中断会留下未被引用的集合。**在创建新集合之前跑** ——
    /// 否则会把上次刚建的（还没切别名的）也删掉。
    ///
    /// **单个删除失败只记 warn 不中断**（上游 `:177-182`）—— 清理是尽力而为，
    /// 一个删不掉的集合不该让整次重建失败。
    pub async fn purge_orphan_collections(&self) -> Result<i64, ServiceError> {
        // `SimilarityQueryError` **不实现** `Into<ServiceError>` 的 blanket 转换 ——
        // 那会让 `Unavailable` 也变成 503，把「降级」这条语义悄悄吃掉。
        // 所以每个调用点显式 `map_err`，让「这里要报错」是看得见的。
        let active = self
            .store
            .alias_target()
            .await
            .map_err(SimilarityQueryError::into_service_error)?;
        let mut purged = 0i64;
        let collections = self
            .store
            .list_index_collections()
            .await
            .map_err(SimilarityQueryError::into_service_error)?;
        for name in collections {
            if Some(&name) == active.as_ref() {
                continue;
            }
            match self.store.delete_collection(&name).await {
                Ok(()) => purged += 1,
                Err(error) => {
                    tracing::warn!(collection = %name, detail = %error, "清理遗留集合失败");
                }
            }
        }
        if purged > 0 {
            tracing::warn!(purged_collections = purged, "已清理遗留影片相似度索引集合");
        }
        Ok(purged)
    }
}
impl MovieRecommendationService {
    /// 全量重算：流式扫描 → 写新集合 → **点数校验** → 原子切别名 → 删旧集合。
    ///
    /// # 「点数校验」是切别名前的最后一道闸
    ///
    /// 上游 `:256-262`：`stored_count != indexed_movies` 就抛错。
    ///
    /// **没有它的话**，一次部分失败的写入会被静默接受 —— 别名切过去之后索引里
    /// 少了一半影片，而 `list_similar` 只会返回「没有相似影片」，看起来像
    /// 「这些影片确实没有相似的」而不是「索引不完整」。**那个错误会一直藏着
    /// 直到有人来问「为什么这部片子没有相似影片」。**
    ///
    /// # 失败时的清理**分两种**，别弄反
    ///
    /// | 阶段 | 动作 | 理由 |
    /// |---|---|---|
    /// | **写入阶段**失败 | 删新集合、重抛 | 别名还指着旧集合，删新的安全 |
    /// | **别名切换**失败 | **什么都不删**、重抛 | 切换可能**已生效**只是响应丢了 |
    ///
    /// 第二条是上游 `:277` 的注释：「alias 切换结果存在网络层歧义，切换报错时
    /// 不能删除可能已激活的新集合」。**删了等于删掉正在服务的索引。**
    pub async fn recompute_all(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<RecomputeStats, ServiceError> {
        let total_movies = self.features.total_movies().await?;
        // DF **先于**特征扫描加载 —— 扫描期间新入库的演员/标签取 DF=0。
        let df = DocumentFrequencies {
            actors: self.features.actor_document_frequencies().await?,
            tags: self.features.tag_document_frequencies().await?,
        };
        let mut stats = RecomputeStats {
            total_movies,
            actor_features: df.actors.values().sum(),
            tag_features: df.tags.values().sum(),
            ..Default::default()
        };
        emit(
            &mut progress,
            0,
            total_movies,
            "开始构建影片相似度索引",
            &stats,
        )
        .await;

        stats.purged_collections = self.purge_orphan_collections().await?;
        // 集合名带纳秒时间戳 —— 天然唯一，且一眼看出建于何时。
        let collection = format!(
            "{}{}",
            super::qdrant::similarity::COLLECTION_PREFIX,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );
        self.store
            .create_collection(&collection)
            .await
            .map_err(SimilarityQueryError::into_service_error)?;

        // ---- 写入阶段（失败删新集合）----
        let write_result = self
            .write_all(&collection, &df, total_movies, &mut stats, &mut progress)
            .await;
        if let Err(error) = write_result {
            if let Err(cleanup) = self.store.delete_collection(&collection).await {
                tracing::warn!(collection = %collection, detail = %cleanup, "清理失败的新集合失败");
            }
            return Err(error);
        }

        // ---- 点数校验：切别名前的最后一道闸 ----
        let stored = self
            .store
            .count(&collection)
            .await
            .map_err(SimilarityQueryError::into_service_error)?;
        if stored != stats.indexed_movies as u64 {
            let _ = self.store.delete_collection(&collection).await;
            return Err(ServiceError::validation(
                "movie_similarity_count_mismatch",
                format!(
                    "影片相似度索引点数校验失败 expected={} actual={stored}",
                    stats.indexed_movies
                ),
            ));
        }

        // ---- 原子切别名（失败什么都不删）----
        let previous = self
            .store
            .activate_collection(&collection)
            .await
            .map_err(SimilarityQueryError::into_service_error)?;
        stats.previous_collection = previous.clone();
        if let Some(old) = previous.as_ref() {
            if old != &collection {
                if let Err(error) = self.store.delete_collection(old).await {
                    tracing::warn!(collection = %old, detail = %error, "清理旧集合失败");
                }
            }
        }

        emit(
            &mut progress,
            total_movies,
            total_movies,
            "影片相似度索引构建完成",
            &stats,
        )
        .await;
        Ok(stats)
    }

    /// 流式扫描 + 分批写入。
    ///
    /// **keyset 分页**（`id > last`）而不用 `OFFSET` —— 大偏移量下 `OFFSET`
    /// 要扫过并丢弃前面的行，30 万影片重建时越来越慢。
    async fn write_all(
        &self,
        collection: &str,
        df: &DocumentFrequencies,
        total_movies: i64,
        stats: &mut RecomputeStats,
        progress: &mut Option<ProgressSink<'_>>,
    ) -> Result<(), ServiceError> {
        let mut last_movie_id = 0i32;
        let mut batch: Vec<SparsePoint> = Vec::with_capacity(INDEX_BATCH_SIZE);
        loop {
            let movie_ids = self
                .features
                .page_movie_ids(last_movie_id, FEATURE_PAGE_SIZE)
                .await?;
            if movie_ids.is_empty() {
                break;
            }
            last_movie_id = *movie_ids.last().expect("刚判过非空");
            for features in self
                .features
                .features_for_movies(&movie_ids)
                .await?
                .values()
            {
                // 既无演员也无标签 -> 构造不出向量，**跳过不入索引**。
                // 所以 `indexed_movies` 通常小于 `total_movies`。
                if features.actor_ids.is_empty() && features.tag_ids.is_empty() {
                    continue;
                }
                let (indices, values) =
                    build_sparse_vector(&features.actor_ids, &features.tag_ids, df, total_movies);
                batch.push((features.movie_id as i64, indices, values));
            }
            if batch.len() >= INDEX_BATCH_SIZE {
                // `take` 而不是 `batch.clone()`：整批已经写完，留一个空 Vec 继续攒。
                self.flush(collection, std::mem::take(&mut batch), stats, progress)
                    .await?;
            }
            // 不满一页说明已到末尾，省掉一次必然为空的探测。
            if movie_ids.len() < FEATURE_PAGE_SIZE as usize {
                break;
            }
        }
        self.flush(collection, batch, stats, progress).await
    }

    /// 写入整批后再累加计数并上报。
    ///
    /// **顺序要紧**：先写成功，再动 `indexed_movies`。反了会让统计数大于索引里的
    /// 实际点数 —— 而上面的点数校验会因此**必然失败**。
    async fn flush(
        &self,
        collection: &str,
        batch: Vec<SparsePoint>,
        stats: &mut RecomputeStats,
        progress: &mut Option<ProgressSink<'_>>,
    ) -> Result<(), ServiceError> {
        if batch.is_empty() {
            return Ok(());
        }
        let written = batch.len() as i64;
        self.store
            .upsert_sparse_points(collection, &batch)
            .await
            .map_err(SimilarityQueryError::into_service_error)?;
        stats.indexed_movies += written;
        emit(
            progress,
            stats.indexed_movies,
            stats.total_movies,
            &format!("已索引 {}/{}", stats.indexed_movies, stats.total_movies),
            stats,
        )
        .await;
        Ok(())
    }
}

/// 上报进度 + 摘要。`progress` 为 `None` 时静默。
async fn emit(
    progress: &mut Option<ProgressSink<'_>>,
    current: i64,
    total: i64,
    text: &str,
    stats: &RecomputeStats,
) {
    if let Some(sink) = progress {
        let patch = serde_json::to_value(stats).ok();
        let _ = sink(
            Some(current as i32),
            Some(total as i32),
            text,
            patch.as_ref(),
        )
        .await;
    }
}
/// 相似影片条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMovieItem {
    pub movie_id: i64,
    pub title: Option<String>,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
}

impl MovieRecommendationService {
    /// 查相似影片。**降级路径返回空列表，不返回 Err。**
    ///
    /// # 这是「错误分类属于调用方契约」的落点
    ///
    /// | 上游异常 | 本函数 |
    /// |---|---|
    /// | `NotReady` | **返 `Err`** —— 索引没建好，要 503 让用户重试 |
    /// | `Unavailable` | **返回空列表 + warn** —— Qdrant 故障不该让影片详情页整体报错 |
    ///
    /// 上游 `recommendation_service.py:341-348` 的注释：「与每日/瞬时推荐保持一致：
    /// Qdrant 故障只降级相似度信号，不让详情页整体报错」。
    ///
    /// **「返回 Vec 而不是 Result」是刻意的类型选择** —— 让降级在类型上就是默认值，
    /// 而不是靠每个调用方记得写 `if let Err(...) { return vec![] }`。
    /// **靠调用方自觉的降级，迟早会漏一处。**
    pub async fn search_similar_movies(
        &self,
        source_movie_id: i64,
        limit: i64,
    ) -> Result<Vec<MovieSimilarityHit>, SimilarityQueryError> {
        match self.store.search_many(&[source_movie_id], limit).await {
            Ok(map) => Ok(map.get(&source_movie_id).cloned().unwrap_or_default()),
            Err(SimilarityQueryError::NotReady) => Err(SimilarityQueryError::NotReady),
            Err(SimilarityQueryError::Unavailable { detail }) => {
                tracing::warn!(
                    source_movie_id,
                    detail,
                    "相似影片查询跳过：影片相似度服务不可用（只丢相似度信号）"
                );
                Ok(Vec::new())
            }
        }
    }

    /// 列出相似影片的 id 与分数。**`Unavailable` 降级成空列表。**
    ///
    /// 上游 `list_similar`（`:313`）。`NotReady` 由调用方转 503。
    pub async fn list_similar(
        &self,
        source_movie_id: i64,
        limit: i64,
    ) -> Result<Vec<SimilarMovieItem>, ServiceError> {
        let hits = self
            .search_similar_movies(source_movie_id, limit)
            .await
            .map_err(SimilarityQueryError::into_service_error)?;
        Ok(hits
            .into_iter()
            .map(|hit| SimilarMovieItem {
                movie_id: hit.movie_id,
                title: None,
                score: hit.score,
            })
            .collect())
    }
}
