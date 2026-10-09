//! Qdrant 稠密向量存储的共用核心。
//!
//! # 为什么不直接写两个 store
//!
//! 上游 `QdrantPlotImageStore` **继承** `QdrantThumbnailStore`
//! （`qdrant_plot_image_store.py:22`），只覆盖四样东西：集合名、payload
//! 索引字段、记录 → point 的映射、命中的解析。其余（建表、校验、upsert
//! 重试、删除、清空、过滤构造、分数归一化）全是共用的。
//!
//! 所以这里抽成 [`DenseStore`]，两个具体 store 只提供「差异」那几个方法。
//! **不要**把两个 store 写成两份平铺代码 —— 上游用继承解决的同一个问题，
//! 抄成两份就等于把上游已经收敛的重复重新引入。
//!
//! # 三条不能改的语义（逐条对应上游）
//!
//! **1. `search` 永不失败，只返回空列表。** 上游 `:415-430` 把
//! 「集合不存在」与「任何异常」都处理成 `return []` + `logger.warning`。
//! 这是刻意的：图搜是**增强功能**，向量库挂了不该让整个页面 500。
//! 把它改成返回 `Err` 会把这个设计意图抹掉 —— 上层一 `?` 就变成 500。
//!
//! **2. 写入要重试，查询不重试。** `UPSERT_RETRY_DELAYS_SECONDS` 是
//! `(3, 10, 20, 60, 60)`（`:43`），5 次、累计约 153 秒。索引任务跑在后台，
//! 偶发断连重试即可；而查询是前台交互路径，重试只会放大延迟。
//!
//! **3. 分数归一化到 `[0, 1]`。** 上游 `:442`：
//! `max(0.0, min(1.0, (score + 1.0) / 2.0))`。COSINE 距离的原始分在
//! `[-1, 1]`，前端要的是百分制。**这个公式不能简化** —— 直接用原始分会让
//! 「完全不相似」显示成 0 分而不是 50 分。

use std::time::Duration;

use qdrant_client::qdrant::{
    vectors_config, CollectionStatus, Condition, CreateCollectionBuilder, Datatype, Distance,
    FieldType, Filter, HnswConfigDiff, MaxOptimizationThreadsBuilder, OptimizersConfigDiff,
    PointId, PointStruct, QueryPointsBuilder, ScoredPoint, SearchParams, VectorParamsBuilder,
};
use qdrant_client::{Qdrant, QdrantError};

use crate::error::ServiceError;

/// 缩略图集合名。**嵌了模型名与版本**（`siglip2_v1`）—— 上游 `:38-39` 解释过：
/// v0.5.3 有个同名集合存的是 JoyTag 向量，换名是为了避免旧向量被当成
/// SigLIP2 数据复用。**改名等于换模型，必须重建索引。**
pub const THUMBNAIL_COLLECTION: &str = "media_thumbnail_vectors_siglip2_v1";
/// 剧照集合名。同样带 `siglip2_v1`。
pub const PLOT_IMAGE_COLLECTION: &str = "movie_plot_image_vectors_siglip2_v1";

/// 缩略图的 payload 索引字段（上游 `:40`）。
pub const THUMBNAIL_PAYLOAD_INDEX: &[&str] = &["movie_id", "media_id"];
/// 剧照的 payload 索引字段（`qdrant_plot_image_store.py:24`）—— 只要 `movie_id`。
pub const PLOT_IMAGE_PAYLOAD_INDEX: &[&str] = &["movie_id"];

/// HNSW 的 `m`。上游 `:44`。
const HNSW_M: u64 = 16;
/// 建索引时的 `ef_construct`。上游 `:45`。
const HNSW_EF_CONSTRUCT: u64 = 128;
/// 查询时的 `hnsw_ef`。上游 `:46`。
pub const HNSW_EF_SEARCH: u64 = 128;
/// 后台 rebuild 的并发上限。上游 `:47-48` 特意限成 1，注释写明
/// 「默认会用满所有 CPU 核，避免索引任务时把宿主机拖垮」。
const MAX_OPTIMIZATION_THREADS: u64 = 1;
/// 建索引并发上限。上游 `:49`。
const HNSW_MAX_INDEXING_THREADS: u64 = 2;
/// 提高触发阈值以降低 rebuild 频率。上游 `:50-51`，注释写明代价是
/// 「未索引区变大、部分搜索走暴力扫描」。
const INDEXING_THRESHOLD: u64 = 50_000;

/// 普通请求超时（秒）。上游 `:41`。
pub const CLIENT_TIMEOUT_SECONDS: u64 = 30;
/// 清库超时（秒）。上游 `:42` —— 删集合比普通操作慢得多。
pub const CLEAR_TIMEOUT_SECONDS: u64 = 300;
/// upsert 的重试退避（秒）。上游 `:43`，累计约 153 秒。
pub const UPSERT_RETRY_DELAYS_SECONDS: [u64; 5] = [3, 10, 20, 60, 60];

/// Qdrant 侧的错误码。
pub mod code {
    /// 连不上或超时 → 503（可退避重试）。
    pub const UNAVAILABLE: &str = "vector_store_unavailable";
    /// 连上了但没成功 → 502（换参数/换依赖后重试）。
    pub const FAILED: &str = "vector_store_failed";
    /// 集合的向量参数与要求不符 → 409（要重建索引，不该重试）。
    pub const MISMATCH: &str = "vector_store_mismatch";
}

/// 把 Qdrant 错误映射成 [`ServiceError`]。
///
/// 分界沿用 [`crate::error::ServiceError::unavailable`] 与 `bad_gateway`
/// 定的「能不能重试」：传输层 → 503，其余 → 502。
///
/// # 原始错误必须留下痕迹
///
/// 映射会把细节压成一句话，所以**同时**做两件事：`tracing::warn!` 记全量
/// 错误，并把它塞进 `details` 的 `qdrant_error` 键。理由很直接：第一次
/// 接真实实例时，`create_collection` 报了 502 而消息只有
/// 「Vector store request failed」—— **完全无法判断是端口错、参数不被
/// 支持、还是集合已存在**。压细节的代价是让这类问题只能靠猜。
///
/// 这里**不**把原始错误拼进 `message`：那个字段是面向用户的中文提示，
/// 而 Qdrant 的英文错误里可能带地址与内部结构。
pub fn map_qdrant_error(error: &QdrantError) -> ServiceError {
    // `QdrantError` 走 tonic，传输层错误表现为 status 里的
    // `Unavailable` / `DeadlineExceeded`，而连接失败是 `tonic` 的
    // transport error。用消息匹配是不得已 —— tonic 把这两类都压成
    // `Status`，没有更细的分类可用。这与
    // `discovery::embedding::map_send_error` 记录的是同一类限制。
    let text = error.to_string();
    let transport = text.contains("transport error")
        || text.contains("ServiceUnavailable")
        || text.contains("DeadlineExceeded")
        || text.contains("connection");
    tracing::warn!(
        error = %error,
        transport,
        collection = ?text,
        "向量库请求失败"
    );
    let mut details = serde_json::Map::new();
    details.insert(
        "qdrant_error".to_owned(),
        serde_json::Value::String(text.clone()),
    );
    if transport {
        return ServiceError::unavailable(code::UNAVAILABLE, "Vector store is unreachable");
    }
    ServiceError::bad_gateway(code::FAILED, "Vector store request failed", details)
}

/// COSINE 原始分 → `[0, 1]`。照抄上游 `:442`。
pub fn normalize_score(score: f32) -> f32 {
    ((score + 1.0) / 2.0).clamp(0.0, 1.0)
}

/// 构造 `movie_id` 的过滤条件。
///
/// 照抄上游 `_build_filter`（`:386-401`）：
///
/// - `movie_ids` → **must** + 单个 `MatchAny`（不是每 id 一个条件）
/// - `exclude_movie_ids` → **must_not** + 单个 `MatchAny`
/// - 两者都空 → `None`（**不加过滤**，而不是加一个恒真的过滤器 —— 后者
///   会让 Qdrant 走过滤路径，丢掉 HNSW 加速）
/// - id **去重且保序**（上游用 `dict.fromkeys`）
pub fn build_movie_filter(
    movie_ids: Option<&[i64]>,
    exclude_movie_ids: Option<&[i64]>,
) -> Option<Filter> {
    let mut must = Vec::new();
    let mut must_not = Vec::new();
    if let Some(ids) = movie_ids {
        let unique = dedup_preserving_order(ids);
        if !unique.is_empty() {
            must.push(Condition::matches("movie_id", unique));
        }
    }
    if let Some(ids) = exclude_movie_ids {
        let unique = dedup_preserving_order(ids);
        if !unique.is_empty() {
            must_not.push(Condition::matches("movie_id", unique));
        }
    }
    if must.is_empty() && must_not.is_empty() {
        return None;
    }
    let mut filter = Filter::default();
    if !must.is_empty() {
        filter.must = must;
    }
    if !must_not.is_empty() {
        filter.must_not = must_not;
    }
    Some(filter)
}

/// 去重且保序。`MatchAny` 传重复 id 没有意义，但**顺序**要保持以便可复现。
fn dedup_preserving_order(ids: &[i64]) -> Vec<i64> {
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

/// 稠密向量集合的共用核心。
///
/// **不 derive `Debug`** —— `Qdrant` 内部持有 tonic channel，没有 `Debug`
/// 实现。硬凑一个只会打印出内部连接细节（地址、channel 状态），对调试无益
/// 且可能带出凭据。所以这里只 `Clone`。
#[derive(Clone)]
pub struct DenseStore {
    client: Qdrant,
    collection: String,
    payload_index_fields: &'static [&'static str],
}

impl DenseStore {
    /// 连上 Qdrant。
    ///
    /// - `url`：`qdrant.url` 配置项，**去掉尾斜杠**（上游 `:60` 同样处理）。
    ///   **注意端口**：Qdrant 的 gRPC 在 **6334**，REST 在 6333，而本客户端
    ///   走 gRPC。配置里的默认值 `http://qdrant:6333` 是 REST 端口 ——
    ///   调用方要传 gRPC 端口。`QdrantConfig::from_url` 不做端口推断，原样使用。
    /// - `api_key`：`qdrant.api_key`，可空。为空时不带认证头
    pub fn connect(
        url: &str,
        api_key: Option<&str>,
        collection: &str,
        payload_index_fields: &'static [&'static str],
    ) -> Result<Self, ServiceError> {
        let mut config = qdrant_client::config::QdrantConfig::from_url(url.trim_end_matches('/'));
        config.timeout = Duration::from_secs(CLIENT_TIMEOUT_SECONDS);
        if let Some(key) = api_key {
            config.api_key = Some(key.to_owned());
        }
        let client = config.build().map_err(|error| {
            ServiceError::validation(code::FAILED, format!("无法连接向量库：{error}"))
        })?;
        Ok(Self {
            client,
            collection: collection.to_owned(),
            payload_index_fields,
        })
    }

    /// 集合名。
    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// 集合是否存在。**出错时返回 `false`** —— 上游 `_collection_exists`
    /// 用 `getattr` 兜底后仍会抛，而调用方（`search`）把异常当「不存在」。
    pub async fn exists(&self) -> bool {
        self.client
            .collection_exists(self.collection.clone())
            .await
            .unwrap_or(false)
    }

    /// 集合名。状态探测要把它回显给客户端 —— 让调用方问本 store 而不是去引
    /// 常量，将来两个 store 的集合名各自变化时不会有人抄错。
    pub fn collection_name(&self) -> &str {
        &self.collection
    }

    /// 建表；已存在则**校验**而不是重建。
    ///
    /// 照抄上游 `ensure_table`（`:128-158`）+ `_validate_collection`
    /// （`:160-182`）。三条校验（size / distance / dtype）任一不符就报错：
    ///
    /// - **size 不符** → 换了 embedding 模型，**必须重建索引**
    /// - **distance 不符** → 相似度算错，搜出来的东西没有意义
    /// - **dtype 不符** → 精度不同，阈值失去可比性
    ///
    /// 报 409（[`ServiceError::conflict`] 那一类）而不是 502：重试没有用，
    /// 唯一的出路是换模型后重建。
    pub async fn ensure_table(&self, vector_size: usize) -> Result<(), ServiceError> {
        if vector_size == 0 {
            return Err(ServiceError::validation(
                "validation_error",
                "vector_size must be positive",
            ));
        }
        if self.exists().await {
            return self.validate_collection(vector_size).await;
        }
        let vectors = VectorParamsBuilder::new(vector_size as u64, Distance::Cosine)
            .datatype(Datatype::Float16)
            .on_disk(true)
            .build();
        let hnsw = HnswConfigDiff {
            m: Some(HNSW_M),
            ef_construct: Some(HNSW_EF_CONSTRUCT),
            max_indexing_threads: Some(HNSW_MAX_INDEXING_THREADS),
            // 上游 `:150` 传了 `on_disk=True`，这里照搬。
            //
            // 该字段在客户端已标 deprecated（Qdrant 官方废弃了「HNSW 索引落盘」，
            // 因为收益极少而查询更慢）。**但不静默丢掉** —— 丢掉就与上游
            // 行为不一致，而「上游设了什么」正是这里要照抄的东西。
            // `#[allow(deprecated)]` 压掉警告，把这个决定显式留在代码里；
            // 将来若确认该字段该彻底移除，删掉这一行即可。
            #[allow(deprecated)]
            on_disk: Some(true),
            ..Default::default()
        };
        let optimizers = OptimizersConfigDiff {
            // 上游 `:47-48` 特意限成 1，注释写明「默认会用满所有 CPU 核，
            // 避免索引任务时把宿主机拖垮」。这个字段在 prost 里是 oneof
            // （可能是具体线程数，也可能是「自适应」），所以要用 builder
            // 而不是直接塞 u64。
            max_optimization_threads: Some(
                MaxOptimizationThreadsBuilder::threads(MAX_OPTIMIZATION_THREADS).build(),
            ),
            indexing_threshold: Some(INDEXING_THRESHOLD),
            ..Default::default()
        };
        self.client
            .create_collection(
                CreateCollectionBuilder::new(self.collection.clone())
                    .vectors_config(vectors)
                    .hnsw_config(hnsw)
                    .optimizers_config(optimizers)
                    // 非索引 payload 落盘，避免普通元数据常驻内存（上游 `:156`）。
                    .on_disk_payload(true),
            )
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        Ok(())
    }

    /// 校验已存在集合的向量参数是否与要求一致。
    async fn validate_collection(&self, expected_size: usize) -> Result<(), ServiceError> {
        let info = self
            .client
            .collection_info(self.collection.clone())
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        // `GetCollectionInfoResponse` 是 gRPC 包装层，真正的内容在 `result` 里。
        let Some(params) = info
            .result
            .as_ref()
            .and_then(|info| info.config.as_ref())
            .and_then(|config| config.params.as_ref())
        else {
            // 上游 `:162-164`：取不到向量参数就直接放过。
            return Ok(());
        };
        // `VectorsConfig` 是 oneof：单向量是 `Params`，命名向量是 `ParamsMap`。
        // 上游 `_get_vector_params`（`:162`）取不到就放过，这里照搬。
        //
        // **命名向量（`ParamsMap`）不校验**：本模块只创建单向量集合，
        // 那条分支属于「不该出现的状态」，猜它的语义反而可能写错校验。
        let config = match params
            .vectors_config
            .as_ref()
            .and_then(|c| c.config.as_ref())
        {
            Some(vectors_config::Config::Params(params)) => Some(params),
            // `ParamsMap`（命名向量）与 `None`（未设置）都落到这里 ——
            // 写成两个显式分支会被 clippy 判为 redundant guard。
            _ => None,
        };
        let Some(config) = config else {
            return Ok(());
        };
        // `VectorParams` 里 `size` 是裸 `u64`、`distance` 是裸 `i32`，
        // 而 `datatype` 是 `Option<i32>`（prost 对 enum 的编码不一致，别猜）。
        // **解不出枚举也要报**，不能当成「匹配」放过 —— 那正是「集合是用
        // 另一套参数建的」的情形，放过等于在没有校验的情况下继续写向量。
        if config.size as usize != expected_size {
            return Err(Self::mismatch(
                "vector size",
                &expected_size.to_string(),
                &config.size.to_string(),
            ));
        }
        match Distance::try_from(config.distance) {
            Ok(Distance::Cosine) => {}
            Ok(actual) => return Err(Self::mismatch("distance", "cosine", &format!("{actual:?}"))),
            Err(_) => {
                return Err(Self::mismatch(
                    "distance",
                    "cosine",
                    &format!("unrecognized({})", config.distance),
                ))
            }
        }
        match config.datatype {
            Some(raw) => match Datatype::try_from(raw) {
                Ok(Datatype::Float16) => {}
                Ok(actual) => {
                    return Err(Self::mismatch(
                        "datatype",
                        "float16",
                        &format!("{actual:?}"),
                    ))
                }
                Err(_) => {
                    return Err(Self::mismatch(
                        "datatype",
                        "float16",
                        &format!("unrecognized({raw})"),
                    ))
                }
            },
            None => return Err(Self::mismatch("datatype", "float16", "unset")),
        }
        Ok(())
    }

    /// 构造参数不符的错误。上游抛 `ValueError`，这里是 409 而非 502 ——
    /// 重试不会让配置变对，唯一的出路是重建索引。
    fn mismatch(field: &str, expected: &str, actual: &str) -> ServiceError {
        ServiceError::conflict(
            code::MISMATCH,
            format!("Qdrant 集合的 {field} 不符：expected={expected}, actual={actual}"),
            None,
        )
    }

    /// 建 payload 标量索引。
    ///
    /// 没有它，`movie_id` 过滤会退化成全量扫描 —— 上游 `ensure_scalar_indices`
    /// 就是为这个存在的。**重复调用是安全的**（Qdrant 幂等）。
    pub async fn ensure_scalar_indices(&self) -> Result<(), ServiceError> {
        for field in self.payload_index_fields {
            self.client
                .create_field_index(
                    qdrant_client::qdrant::CreateFieldIndexCollectionBuilder::new(
                        self.collection.clone(),
                        (*field).to_owned(),
                        FieldType::Keyword,
                    )
                    // 等索引建完再返回，否则紧接着的第一次过滤查询
                    // 仍会退化成全量扫描。
                    .wait(true),
                )
                .await
                .map_err(|error| map_qdrant_error(&error))?;
        }
        Ok(())
    }

    /// 写入点。**仅在传输失败时重试**，退避见
    /// [`UPSERT_RETRY_DELAYS_SECONDS`]。
    pub async fn upsert_points(&self, points: Vec<PointStruct>) -> Result<(), ServiceError> {
        if points.is_empty() {
            // 上游 `:26-27` 的 `if not records: return` —— 空批次直接返回，
            // 不发请求。批处理末批恰好为空时不该报错。
            return Ok(());
        }
        let mut last: Option<ServiceError> = None;
        for (attempt, delay) in std::iter::once(0u64)
            .chain(UPSERT_RETRY_DELAYS_SECONDS.iter().copied())
            .enumerate()
        {
            if attempt > 0 {
                tracing::warn!(
                    collection = %self.collection,
                    attempt,
                    delay_seconds = delay,
                    "向量库写入失败，退避后重试"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            match self
                .client
                .upsert_points(
                    qdrant_client::qdrant::UpsertPointsBuilder::new(
                        self.collection.clone(),
                        points.clone(),
                    )
                    // 等写完再返回，否则紧接着的 search 可能读不到。
                    .wait(true),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let mapped = map_qdrant_error(&error);
                    // 只重试传输层失败。协议层失败（502）重试没有意义 ——
                    // 同样的请求再发一次还是会失败。
                    if mapped.status != 503 {
                        return Err(mapped);
                    }
                    last = Some(mapped);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ServiceError::unavailable(code::UNAVAILABLE, "Vector store is unreachable")
        }))
    }

    /// 按点 id 删除。
    pub async fn delete_ids(&self, ids: &[i64]) -> Result<(), ServiceError> {
        if ids.is_empty() {
            return Ok(());
        }
        let unique = dedup_preserving_order(ids);
        self.client
            .delete_points(
                qdrant_client::qdrant::DeletePointsBuilder::new(self.collection.clone())
                    .points(
                        unique
                            .into_iter()
                            // `PointId` 只实现 `From<u64>`，而我们的 id 是
                            // i64（数据库主键）。库里不存在负数主键，所以
                            // `as u64` 是安全的；真出现负数会变成一个
                            // 巨大的正数 id，删不掉任何东西而不是删错。
                            .map(|id| PointId::from(id as u64))
                            .collect::<Vec<_>>(),
                    )
                    .wait(true),
            )
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        Ok(())
    }

    /// 按 payload 字段删除（上游 `delete_by_media_id` 的做法）。
    pub async fn delete_where_field(
        &self,
        field: &str,
        values: &[i64],
    ) -> Result<(), ServiceError> {
        if values.is_empty() {
            return Ok(());
        }
        let unique = dedup_preserving_order(values);
        self.client
            .delete_points(
                qdrant_client::qdrant::DeletePointsBuilder::new(self.collection.clone())
                    .points(Filter::must([Condition::matches(field, unique)]))
                    .wait(true),
            )
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        Ok(())
    }

    /// 精确计数。用于状态展示与「索引是否为空」判断。
    pub async fn count(&self) -> Result<u64, ServiceError> {
        let response = self
            .client
            .count(
                qdrant_client::qdrant::CountPointsBuilder::new(self.collection.clone()).exact(true),
            )
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        Ok(response.result.map(|r| r.count).unwrap_or(0))
    }

    /// **删掉整个集合**（不是删点）。上游 `clear`（`:369-384`）这么做。
    ///
    /// 集合不存在时直接成功 —— 「已经清干净了」和「清干净了」对调用方
    /// 没有区别。
    pub async fn clear(&self) -> Result<(), ServiceError> {
        if !self.exists().await {
            return Ok(());
        }
        self.client
            .delete_collection(
                qdrant_client::qdrant::DeleteCollectionBuilder::new(self.collection.clone())
                    .timeout(CLEAR_TIMEOUT_SECONDS),
            )
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        Ok(())
    }

    /// 向量检索。**永不失败** —— 见模块文档第 1 条。
    ///
    /// 返回的分数已归一化到 `[0, 1]`。
    pub async fn search(
        &self,
        vector: Vec<f32>,
        limit: usize,
        offset: usize,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
    ) -> Result<Vec<ScoredPoint>, ServiceError> {
        // 参数校验在**本地**做，不发请求。上游 `:411-414` 抛 `ValueError`。
        if limit == 0 {
            return Err(ServiceError::validation(
                "validation_error",
                "limit must be positive",
            ));
        }
        if !self.exists().await {
            // 集合还不存在 —— 不是错误，是「还没索引过」。
            return Ok(Vec::new());
        }
        let filter = build_movie_filter(movie_ids, exclude_movie_ids);
        // 先 `build()` 再直接设字段 —— 这是本客户端文档示例的用法
        //（lib.rs 的示例里就是 `search_points.filter = Some(...)`）。
        // `QueryPointsBuilder` 的 setter 覆盖不全，硬凑 setter 名字不如
        // 直接改结构体字段。
        let mut request = QueryPointsBuilder::new(self.collection.clone())
            .query(vector)
            .limit(limit as u64)
            .offset(offset as u64)
            .with_payload(true)
            .with_vectors(false)
            .build();
        if let Some(filter) = filter {
            request.filter = Some(filter);
        }
        request.params = Some(SearchParams {
            hnsw_ef: Some(HNSW_EF_SEARCH),
            ..Default::default()
        });
        match self.client.query(request).await {
            Ok(response) => Ok(response.result),
            Err(error) => {
                // 吞掉异常并返回空 —— 上游 `:428-430`。理由见模块文档第 1 条。
                tracing::warn!(
                    collection = %self.collection,
                    error = %error,
                    "向量检索失败，按「无结果」处理"
                );
                Ok(Vec::new())
            }
        }
    }

    /// 集合是否存在（供状态接口用），带已索引点数与向量参数。
    ///
    /// 点数走 `count(exact=true)` 而不是从 `collection_info` 里取 ——
    /// 后者的 `CollectionInfo` 只带 segment 数与状态枚举，点数在
    /// `CollectionStatus` 内部、不是本客户端的稳定接口。直接问 `count`
    /// 更省事也更准。
    ///
    /// 向量维度 / 数据类型 / 集合状态则**必须**从 `collection_info` 取
    /// （`count` 给不了），取法与 `Self::validate_collection` 同一套路径。
    pub async fn status(&self) -> Result<DenseStoreStatus, ServiceError> {
        // ⚠️ **刻意不复用 `self.exists()`**：那个方法把错误吞成 `false`
        //（`search` 路径要求「向量库挂了也当没结果」，见模块文档第 1 条），
        // 而状态接口必须把「连不上」如实报成 `Err` —— 否则 Qdrant 宕机会被
        // 显示成 `exists: false` + 健康，也就是「一切正常，只是还没建集合」，
        // 正好是本端点要消灭的那种误报。
        //
        // 上游 `_collection_exists` 在这里同样不吞异常
        //（`qdrant_thumbnail_store.py:117-126`：只有降级分支才 `try/except`，
        // 主分支让异常冒到 `inspect_status` 的 `except` → `healthy: False`）。
        let exists = self
            .client
            .collection_exists(self.collection.clone())
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        if !exists {
            return Ok(DenseStoreStatus {
                exists: false,
                points: 0,
                vector_size: None,
                vector_dtype: None,
                collection_status: None,
            });
        }
        let info = self
            .client
            .collection_info(self.collection.clone())
            .await
            .map_err(|error| map_qdrant_error(&error))?;
        let result = info.result.as_ref();
        // 命名向量（`ParamsMap`）与「取不到向量参数」都落到 `None`，与
        // `validate_collection` 同样的取舍：本模块只建单向量集合，那条分支
        // 属于不该出现的状态，猜它的语义反而可能显示错。
        let vector_params = result
            .and_then(|info| info.config.as_ref())
            .and_then(|config| config.params.as_ref())
            .and_then(|params| params.vectors_config.as_ref())
            .and_then(|config| config.config.as_ref())
            .and_then(|config| match config {
                vectors_config::Config::Params(params) => Some(params),
                _ => None,
            });
        Ok(DenseStoreStatus {
            exists: true,
            points: self.count().await?,
            vector_size: vector_params.map(|params| params.size),
            vector_dtype: vector_params.and_then(|params| datatype_name(params.datatype)),
            collection_status: result
                .map(|info| info.status)
                .and_then(collection_status_name),
        })
    }
}

/// 集合状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenseStoreStatus {
    /// 集合是否已建。
    pub exists: bool,
    /// 已索引的点数。
    pub points: u64,
    /// 向量维度。集合未建、或取不到向量参数时为 `None`。
    pub vector_size: Option<u64>,
    /// 向量数据类型，REST 风格小写串（如 `float16`）。
    pub vector_dtype: Option<String>,
    /// 集合状态，REST 风格小写串（如 `green`）。
    pub collection_status: Option<String>,
}

/// `Datatype` → REST 风格小写串（上游 `_enum_value` 的产出）。
///
/// 字面量取自上游自己的黄金用例 `tests/api/test_status_api.py:201`
/// （`"vector_dtype": "float16"`），**不是**把 proto 枚举名小写化猜出来的 ——
/// 猜成 `Float16` 会让客户端按 `float16` 匹配时对不上。
///
/// `UnknownDatatype`（0）与将来新增的类型返回 `None`：上游那套 REST 枚举里
/// 没有对应字面量，编一个反而更难排查。
fn datatype_name(datatype: Option<i32>) -> Option<String> {
    match datatype? {
        value if value == Datatype::Float16 as i32 => Some("float16".to_owned()),
        value if value == Datatype::Float32 as i32 => Some("float32".to_owned()),
        value if value == Datatype::Uint8 as i32 => Some("uint8".to_owned()),
        _ => None,
    }
}

/// `CollectionStatus` → REST 风格小写串。字面量来源同 [`datatype_name`]
/// （`tests/api/test_status_api.py:202` 的 `"collection_status": "green"`）。
fn collection_status_name(status: i32) -> Option<String> {
    match status {
        value if value == CollectionStatus::Green as i32 => Some("green".to_owned()),
        value if value == CollectionStatus::Yellow as i32 => Some("yellow".to_owned()),
        value if value == CollectionStatus::Red as i32 => Some("red".to_owned()),
        value if value == CollectionStatus::Grey as i32 => Some("grey".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 锁住 `vector_dtype` / `collection_status` 的**字面量形状**。
    ///
    /// 上游这两个值走 `_enum_value()`，产出是 REST 风格小写串，证据是上游自己的
    /// 黄金用例 `tests/api/test_status_api.py:201-202`
    /// （`"vector_dtype": "float16"`、`"collection_status": "green"`）。
    ///
    /// 这不是「我的函数返回我写下的常量」那种同义反复 —— 它锁的是一个**跨仓
    /// 事实**：一旦有人把它改成 proto 枚举名小写化之外的形状（`Float16`、整数
    /// `3`、`green_`），客户端按 REST 值匹配就对不上，而线上表现只是状态页那两格
    /// 显示怪值，没人会去查。
    #[test]
    fn qdrant_enums_map_to_the_rest_literals_upstream_reports() {
        let cases = [
            (Some(Datatype::Float16 as i32), Some("float16")),
            (Some(Datatype::Float32 as i32), Some("float32")),
            (Some(Datatype::Uint8 as i32), Some("uint8")),
            // 未知 / 未设：宁可为 `None`，也不要编一个字面量出来。
            (Some(0), None),
            (None, None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                datatype_name(input).as_deref(),
                expected,
                "datatype={input:?}"
            );
        }

        let statuses = [
            (CollectionStatus::Green as i32, "green"),
            (CollectionStatus::Yellow as i32, "yellow"),
            (CollectionStatus::Red as i32, "red"),
            (CollectionStatus::Grey as i32, "grey"),
        ];
        for (input, expected) in statuses {
            assert_eq!(
                collection_status_name(input).as_deref(),
                Some(expected),
                "status={input}"
            );
        }
        assert_eq!(collection_status_name(0), None);
    }

    #[test]
    fn score_is_normalized_from_cosine_range() {
        // COSINE 原始分域是 [-1, 1]，前端要 [0, 1]
        assert!((normalize_score(1.0) - 1.0).abs() < 1e-6);
        assert!((normalize_score(-1.0) - 0.0).abs() < 1e-6);
        assert!((normalize_score(0.0) - 0.5).abs() < 1e-6);
        // 越界必须夹紧，不能返回 1.2 或 -0.1
        assert_eq!(normalize_score(5.0), 1.0);
        assert_eq!(normalize_score(-5.0), 0.0);
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        assert_eq!(dedup_preserving_order(&[3, 1, 3, 2, 1]), vec![3, 1, 2]);
        assert!(dedup_preserving_order(&[]).is_empty());
    }

    /// 两个都空 → **None**（不加过滤），而不是加一个恒真过滤器。
    ///
    /// 后者会让 Qdrant 走过滤路径、丢掉 HNSW 加速 —— 是个静默的性能回归。
    #[test]
    fn empty_filter_means_no_filter_at_all() {
        assert!(build_movie_filter(None, None).is_none());
        assert!(build_movie_filter(Some(&[]), Some(&[])).is_none());
    }

    #[test]
    fn include_and_exclude_land_in_must_and_must_not() {
        let filter = build_movie_filter(Some(&[7, 8]), Some(&[9])).expect("应构造出过滤器");
        assert_eq!(filter.must.len(), 1, "include 应合成单个 MatchAny 条件");
        assert_eq!(filter.must_not.len(), 1);
        assert!(filter.should.is_empty());

        let only_include = build_movie_filter(Some(&[1]), None).expect("应构造出过滤器");
        assert_eq!(only_include.must.len(), 1);
        assert!(only_include.must_not.is_empty());
    }
}
