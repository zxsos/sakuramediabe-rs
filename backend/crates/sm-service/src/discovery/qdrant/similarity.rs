//! 影片相似度索引：**稀疏向量 + 别名蓝绿重建**。
//!
//! 参照物：上游 `service/discovery/qdrant_movie_similarity_store.py`（264 行）。
//!
//! # 为什么单独一个模块，不并进 `dense`
//!
//! | | 稠密（`dense.rs`） | 相似度（本模块） |
//! |---|---|---|
//! | 向量 | 稠密 f32 + COSINE + FLOAT16 | **稀疏**（倒排索引） |
//! | 集合寻址 | 直接用集合名 | **经别名**，查询只见 `ALIAS_NAME` |
//! | 写入 | 幂等 upsert 到固定集合 | 写进**带版本前缀的新集合** |
//! | 切换 | 无 | `activate_collection` 原子换别名（蓝绿） |
//! | 就绪判定 | 集合存在即可 | **别名已挂上**才可查（`is_ready`） |
//!
//! 把它们塞进一个泛型核心会让那个核心同时承担两套语义。**宁可两个模块。**
//!
//! # 蓝绿重建的形状
//!
//! ```text
//!   重建中：movie_metadata_similarity_v1_<新>   <- 灌数据
//!   查询走：movie_metadata_similarity           <- 别名，指向已就绪集合
//!
//!   activate_collection(新):
//!     同一个请求里 [DeleteAlias(旧目标), CreateAlias(新)]   <- 原子
//!   之后：旧集合由调用方按 list_index_collections() 清理
//! ```
//!
//! 上游 `:176` 点明原子性的意义：「同一请求内删旧 alias、挂新 alias，查询侧
//! 不会看到半成品索引」。拆成两次调用的话，中间会有一瞬别名不存在，查询侧
//! 会误判成「索引未就绪」。
//!
//! # 一处必须绕过高层客户端的地方
//!
//! `Qdrant` 的 `update_aliases` 是**私有**的（`collection.rs:321`），而且即使
//! 公开也只接受**单个** action —— 内部包成
//! `ChangeAliases { actions: vec![单个] }`。**高层 API 表达不了「删旧 + 挂新
//! 同一请求」**，而那正是本索引的核心操作。
//!
//! 所以 [`activate_collection`](MovieSimilarityStore::activate_collection)
//! 直接用生成的 gRPC stub `collections_client::CollectionsClient`，它有公开的
//! `update_aliases(ChangeAliases)`。代价是要自己建一条 `tonic::Channel`
//! （`Qdrant` 的内部 channel 不暴露）。
//!
//! **这个代价划算**：别名切换每次蓝绿重建只发生一次，为它多一条短连接远小于
//! 「拆成两次调用」导致线上短暂 503 的代价。
//!
//! # 错误语义：**两类，且只有一类会变成 HTTP 错误**
//!
//! 上游有三个异常，但调用方（`recommendation_service.py:335-348`）的处置是：
//!
//! | 上游异常 | 调用方 | HTTP |
//! |---|---|---|
//! | `MovieSimilarityIndexNotReadyError` | 抛 ApiError | **503** `movie_similarity_index_not_ready` |
//! | `MovieSimilarityUnavailableError` | **warn 日志 + 返回空列表** | **不是错误** |
//!
//! 第二个尤其要紧：**Qdrant 故障在影片详情页是不该报错的** —— 上游注释写
//! 「Qdrant 故障只降级相似度信号，不让详情页整体报错」。
//!
//! 所以本模块用 `SimilarityQueryError` 表达分类，**不套 `ServiceError`** ——
//! `Unavailable` 若被包成 `ServiceError`，调用方只能 `?` 上去，详情页就 503 了。
//! 错误分类是**调用方契约的一部分**，不是实现细节。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qdrant_client::qdrant::{
    collections_client::CollectionsClient, AliasOperations, ChangeAliases, CountPointsBuilder,
    CreateAlias, CreateCollection, Datatype, DeleteAlias, GetPointsBuilder, PointId, PointStruct,
    QueryBatchPointsBuilder, QueryPoints, SparseIndexConfig, SparseVector, SparseVectorParams,
    UpsertPointsBuilder,
};
use qdrant_client::{Payload, Qdrant, QdrantError};

use crate::error::ServiceError;

use super::dense::{
    map_qdrant_error, normalize_score, CLIENT_TIMEOUT_SECONDS, UPSERT_RETRY_DELAYS_SECONDS,
};

/// 查询别名。查询侧**只认这个名字**，不认任何具体集合。
pub const ALIAS_NAME: &str = "movie_metadata_similarity";
/// 集合名前缀。带版本号是为了将来换索引算法时旧集合能被识别并清理。
pub const COLLECTION_PREFIX: &str = "movie_metadata_similarity_v1_";
/// 稀疏向量在集合内的名字。本索引只有一个向量空间，所以固定。
pub const VECTOR_NAME: &str = "metadata";

/// 影片相似度查询失败。分类是**调用方契约的一部分**，见模块文档。
#[derive(Debug)]
pub enum SimilarityQueryError {
    /// Qdrant 当前不可用。调用方应当**降级**：只丢相似度信号，其余照常。
    Unavailable {
        /// 底层原因，写进日志用。
        detail: String,
    },
    /// 索引尚未完成首次构建（别名还没挂上）。**这个会恢复**（等重建完），
    /// 所以它才对应 503「重试有意义」那一类。
    NotReady,
}

impl std::fmt::Display for SimilarityQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { detail } => write!(f, "影片相似度服务不可用：{detail}"),
            Self::NotReady => write!(f, "影片相似度索引尚未完成首次构建"),
        }
    }
}

impl std::error::Error for SimilarityQueryError {}

impl From<QdrantError> for SimilarityQueryError {
    fn from(error: QdrantError) -> Self {
        Self::Unavailable {
            detail: error.to_string(),
        }
    }
}

impl SimilarityQueryError {
    /// 转成 `ServiceError`，供路由层直接返回。
    ///
    /// **正常路径上只有 `NotReady` 会走到这里。** `Unavailable` 的处置是调用方
    /// 降级成空列表 —— 路由层若拿到它，应当返回 200 + 空相似度列表。
    pub fn into_service_error(self) -> ServiceError {
        match self {
            Self::NotReady => ServiceError::unavailable(
                "movie_similarity_index_not_ready",
                "影片相似度索引尚未完成首次构建",
            ),
            Self::Unavailable { detail } => {
                ServiceError::unavailable("movie_similarity_unavailable", detail)
            }
        }
    }
}

/// 一条相似影片命中。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovieSimilarityHit {
    /// 目标影片 id。
    pub movie_id: i64,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
}

/// 稀疏向量点：(影片 id, 倒排索引, 权重)。
///
/// 索引由调用方算好（BM25 之类），本模块只负责存取 —— 与上游
/// `upsert_sparse_points`（`:120-146`）的边界一致：**它不碰分词与打分**。
pub type SparsePoint = (i64, Vec<u32>, Vec<f32>);

/// 影片相似度索引的存储。
///
/// **不 derive `Debug`** —— `Qdrant` 内部持有 tonic channel，没有 `Debug`。
pub struct MovieSimilarityStore {
    client: Qdrant,
    /// 就绪状态缓存。
    ///
    /// 上游 `:51-52` 说明了为什么可以永久缓存：「alias 只会被原子替换、不会被
    /// 删除，就绪状态单调递增，只缓存已就绪」。所以一旦为真就永不为假。
    ///
    /// 用原子而不是普通 bool 是因为 `is_ready(&self)` 要能被并发调用。
    alias_ready: AtomicBool,
}
impl MovieSimilarityStore {
    /// 连上 Qdrant。
    ///
    /// `url` 必须是 **gRPC 端口** —— 上游默认 `http://qdrant:6333` 是 REST，
    /// 而本客户端走 gRPC（上游那个默认值给 REST 客户端用的）。调用方要转换。
    pub fn connect(url: &str, api_key: Option<&str>) -> Result<Self, ServiceError> {
        let mut config = qdrant_client::config::QdrantConfig::from_url(url.trim_end_matches('/'));
        config.timeout = Duration::from_secs(CLIENT_TIMEOUT_SECONDS);
        if let Some(key) = api_key {
            config.api_key = Some(key.to_owned());
        }
        let client = config.build().map_err(|error| map_qdrant_error(&error))?;
        Ok(Self {
            client,
            alias_ready: AtomicBool::new(false),
        })
    }

    /// 别名当前指向哪个集合；未挂上返回 `None`。
    pub async fn alias_target(&self) -> Result<Option<String>, SimilarityQueryError> {
        let response = self.client.list_aliases().await?;
        Ok(response
            .aliases
            .into_iter()
            .find(|alias| alias.alias_name == ALIAS_NAME)
            .map(|alias| alias.collection_name))
    }

    /// 索引是否已就绪（别名已挂上）。
    ///
    /// 命中缓存后**不再问 Qdrant**（就绪状态单调递增，缓存不会过期）。
    /// 反过来「未就绪」每次都要真的问，因为它会变成就绪。
    pub async fn is_ready(&self) -> Result<bool, SimilarityQueryError> {
        if self.alias_ready.load(Ordering::Acquire) {
            return Ok(true);
        }
        if self.alias_target().await?.is_none() {
            return Ok(false);
        }
        self.alias_ready.store(true, Ordering::Release);
        Ok(true)
    }

    /// 列出本索引前缀下的全部集合，用于清理历史遗留（上游 `:89-99`）。
    pub async fn list_index_collections(&self) -> Result<Vec<String>, SimilarityQueryError> {
        let response = self.client.list_collections().await?;
        Ok(response
            .collections
            .into_iter()
            .map(|collection| collection.name)
            .filter(|name| name.starts_with(COLLECTION_PREFIX))
            .collect())
    }

    /// 建一个稀疏向量集合。
    ///
    /// 参数照抄上游 `:103-116`，其中两个容易漏：
    ///
    /// - `vectors_config` 是**空 map**，不是稠密向量配置。本索引只用稀疏向量，
    ///   但 Qdrant 要求显式声明「没有稠密空间」—— 传 None 会被当成默认稠密
    ///   空间（维度 4）而后续写入全失败。
    /// - 稀疏索引 `on_disk = false` + FLOAT16。上游注释：「30w 影片下倒排也只有
    ///   百万级 posting，常驻内存换查询延迟」。
    pub async fn create_collection(&self, collection: &str) -> Result<(), SimilarityQueryError> {
        let mut sparse_map = HashMap::new();
        sparse_map.insert(
            VECTOR_NAME.to_owned(),
            SparseVectorParams {
                index: Some(SparseIndexConfig {
                    // 上游 :110 传 on_disk=False，注释写「30w 影片下倒排也只有百万级
                    // posting，常驻内存换查询延迟」。同样已 deprecated 但语义照抄。
                    #[allow(deprecated)]
                    on_disk: Some(false),
                    datatype: Some(Datatype::Float16 as i32),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let request = CreateCollection {
            collection_name: collection.to_owned(),
            vectors_config: Some(qdrant_client::qdrant::VectorsConfig {
                config: Some(qdrant_client::qdrant::vectors_config::Config::ParamsMap(
                    qdrant_client::qdrant::VectorParamsMap {
                        map: HashMap::new(),
                    },
                )),
            }),
            sparse_vectors_config: Some(qdrant_client::qdrant::SparseVectorConfig {
                map: sparse_map,
            }),
            // 上游 :115 传了 on_disk_payload=True。它已被 Qdrant 标记 deprecated
            // （新字段是 PayloadStorageParams），但**语义照抄** —— 本索引的
            // payload 只有点 id、没有过滤字段，落盘省内存。
            //
            // 不静默丢掉：丢掉就与上游行为不一致，而这个字段正是要照抄的东西。
            // #[allow(deprecated)] 把决定留在代码里，将来确认该移除时删掉即可。
            #[allow(deprecated)]
            on_disk_payload: Some(true),
            ..Default::default()
        };
        self.client.create_collection(request).await?;
        Ok(())
    }

    /// 精确点数（上游 `:148-156`）。
    pub async fn count(&self, collection: &str) -> Result<u64, SimilarityQueryError> {
        let response = self
            .client
            .count(CountPointsBuilder::new(collection.to_owned()).exact(true))
            .await?;
        Ok(response.result.map(|result| result.count).unwrap_or(0))
    }

    /// 删集合。集合不存在**不算失败** —— 清理历史遗留时会重复删同一个。
    pub async fn delete_collection(&self, collection: &str) -> Result<(), SimilarityQueryError> {
        match self.client.delete_collection(collection.to_owned()).await {
            Ok(_) => Ok(()),
            Err(error) => {
                let mapped = map_qdrant_error(&error);
                if mapped.status == 503 {
                    return Err(SimilarityQueryError::Unavailable {
                        detail: mapped.api.message,
                    });
                }
                tracing::warn!(collection, error = %error, "删除向量集合失败（可能不存在）");
                Ok(())
            }
        }
    }
}
impl MovieSimilarityStore {
    /// 把稀疏点灌进指定集合（**不是别名** —— 重建时写的是新集合）。
    ///
    /// 退避重试与稠密那套同源：重建是后台任务，偶发断连重试即可。
    /// **只重试传输层（503）** —— 协议层错误（502）重试无意义。
    pub async fn upsert_sparse_points(
        &self,
        collection: &str,
        points: &[SparsePoint],
    ) -> Result<(), SimilarityQueryError> {
        if points.is_empty() {
            return Ok(());
        }
        let qdrant_points: Vec<PointStruct> = points
            .iter()
            .map(|(movie_id, indices, values)| {
                // indices 与 values 必须等长，否则 Qdrant 拒收。上游没做这个校验
                //（`:131-135` 直接构造），**在这里补**：放调用方意味着每个调用点
                // 都要写一遍，漏一处就是一条 400。
                assert_eq!(
                    indices.len(),
                    values.len(),
                    "稀疏向量 indices 与 values 必须等长：movie_id={movie_id}"
                );
                let mut vectors: HashMap<String, qdrant_client::qdrant::Vector> = HashMap::new();
                vectors.insert(
                    VECTOR_NAME.to_owned(),
                    qdrant_client::qdrant::Vector::new_sparse(indices.clone(), values.clone()),
                );
                PointStruct::new(PointId::from(*movie_id as u64), vectors, Payload::new())
            })
            .collect();
        self.upsert_with_retry(collection, qdrant_points).await
    }

    async fn upsert_with_retry(
        &self,
        collection: &str,
        points: Vec<PointStruct>,
    ) -> Result<(), SimilarityQueryError> {
        let mut last = String::new();
        for attempt in 0..=UPSERT_RETRY_DELAYS_SECONDS.len() {
            let request =
                UpsertPointsBuilder::new(collection.to_owned(), points.clone()).wait(true);
            match self.client.upsert_points(request).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    let mapped = map_qdrant_error(&error);
                    if mapped.status != 503 {
                        return Err(SimilarityQueryError::Unavailable {
                            detail: mapped.api.message,
                        });
                    }
                    tracing::warn!(attempt, error = %error, "向量库写入失败，退避后重试");
                    last = mapped.api.message;
                    if let Some(delay) = UPSERT_RETRY_DELAYS_SECONDS.get(attempt) {
                        tokio::time::sleep(Duration::from_secs(*delay)).await;
                    }
                }
            }
        }
        Err(SimilarityQueryError::Unavailable { detail: last })
    }

    /// 原子换别名：删旧挂新。返回**旧的目标集合名**（`None` = 首次挂载）。
    ///
    /// 删旧与挂新必须在**同一个请求**里（上游 `:158-183`）。拆成两次的话，
    /// 中间那一瞬别名不存在，查询侧会把它读成「索引未就绪」而返回 503 ——
    /// 一次计划内的重建不该让线上短暂不可用。
    ///
    /// 走裸 stub 的理由见模块文档「一处必须绕过高层客户端的地方」。
    pub async fn activate_collection(
        &self,
        collection: &str,
    ) -> Result<Option<String>, SimilarityQueryError> {
        let old = self.alias_target().await?;
        let mut actions = Vec::with_capacity(2);
        if old.is_some() {
            actions.push(AliasOperations {
                action: Some(
                    qdrant_client::qdrant::alias_operations::Action::DeleteAlias(DeleteAlias {
                        alias_name: ALIAS_NAME.to_owned(),
                    }),
                ),
            });
        }
        actions.push(AliasOperations {
            action: Some(
                qdrant_client::qdrant::alias_operations::Action::CreateAlias(CreateAlias {
                    collection_name: collection.to_owned(),
                    alias_name: ALIAS_NAME.to_owned(),
                }),
            ),
        });
        let channel = self.stub_channel().await?;
        let mut stub = CollectionsClient::new(channel);
        stub.update_aliases(ChangeAliases {
            actions,
            timeout: None,
        })
        .await
        .map_err(|status| SimilarityQueryError::Unavailable {
            detail: status.message().to_owned(),
        })?;
        // 别名已挂上 —— 就绪状态单调递增，可以永久缓存了（上游 `:182`）。
        self.alias_ready.store(true, Ordering::Release);
        Ok(old)
    }

    /// 为别名切换单独建一条 channel。
    ///
    /// `Qdrant` 的内部 channel 是私有的（`mod.rs:91`），只能从公开的
    /// `config.uri` 重建。**只在这条罕见路径上建**，见模块文档的权衡说明。
    async fn stub_channel(&self) -> Result<tonic::transport::Channel, SimilarityQueryError> {
        let config = &self.client.config;
        tonic::transport::Channel::from_shared(config.uri.clone())
            .map_err(|error| SimilarityQueryError::Unavailable {
                detail: format!("向量库地址无法解析：{error}"),
            })?
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .connect()
            .await
            .map_err(|error| SimilarityQueryError::Unavailable {
                detail: format!("无法连接向量库：{error}"),
            })
    }
}
impl MovieSimilarityStore {
    /// 批量检索相似影片。
    ///
    /// 流程照抄上游 `:191-258`，四处容易照抄错的：
    ///
    /// 1. **源 id 去重且保序**（`dict.fromkeys`）。同一影片被问两次就查两次。
    /// 2. **`limit + 1`**（`:223`）。多取一个是为了剔掉「自己命中自己」那一行
    ///    —— 稀疏检索必然把自己排第一（分数恒为 1），只取 `limit` 个的话剔完
    ///    少一条。
    /// 3. **请求里给过的源 id 全部要有键**（`:242` 的初值），但索引里查不到
    ///    向量的那些**不出现在结果里**。调用方按 key 取，别假设每个 id 都有。
    /// 4. **未就绪 → `NotReady`**，不是返回空。空结果的含义是「索引里没有相似
    ///    的」，与「还没索引好」是两件事 —— 混起来会让重建期间静默出空推荐。
    pub async fn search_many(
        &self,
        source_movie_ids: &[i64],
        limit: i64,
    ) -> Result<HashMap<i64, Vec<MovieSimilarityHit>>, SimilarityQueryError> {
        // 1. 去重保序
        let mut unique: Vec<i64> = Vec::with_capacity(source_movie_ids.len());
        for id in source_movie_ids {
            if !unique.contains(id) {
                unique.push(*id);
            }
        }
        // 上游 `:198-199`：空源或 limit<=0 时给每个源一个空列表。
        if unique.is_empty() || limit <= 0 {
            return Ok(unique.into_iter().map(|id| (id, Vec::new())).collect());
        }
        // 4. 未就绪不是空结果
        if !self.is_ready().await? {
            return Err(SimilarityQueryError::NotReady);
        }

        // 取回源影片的稀疏向量（经别名，所以拿到的是当前就绪版本）
        let ids: Vec<PointId> = unique.iter().map(|id| PointId::from(*id as u64)).collect();
        let retrieved = self
            .client
            .get_points(
                GetPointsBuilder::new(ALIAS_NAME.to_owned(), ids)
                    .with_payload(false)
                    .with_vectors(true),
            )
            .await?;

        let mut sources: Vec<(i64, SparseVector)> = Vec::new();
        for record in retrieved.result {
            let movie_id = match record.id.as_ref().and_then(point_id_number) {
                Some(id) => id,
                None => continue,
            };
            // 3. 取不到向量的源影片被跳过
            match record.vectors.and_then(named_sparse_vector) {
                Some(vector) => sources.push((movie_id, vector)),
                None => continue,
            }
        }

        if sources.is_empty() {
            return Ok(unique.into_iter().map(|id| (id, Vec::new())).collect());
        }

        // 2. limit + 1
        let query_points: Vec<QueryPoints> = sources
            .iter()
            .map(|(_, vector)| QueryPoints {
                collection_name: ALIAS_NAME.to_owned(),
                query: Some(qdrant_client::qdrant::Query {
                    variant: Some(qdrant_client::qdrant::query::Variant::Nearest(
                        qdrant_client::qdrant::VectorInput {
                            variant: Some(qdrant_client::qdrant::vector_input::Variant::Sparse(
                                vector.clone(),
                            )),
                        },
                    )),
                }),
                using: Some(VECTOR_NAME.to_owned()),
                limit: Some(limit as u64 + 1),
                with_payload: Some(qdrant_client::qdrant::WithPayloadSelector {
                    selector_options: Some(
                        qdrant_client::qdrant::with_payload_selector::SelectorOptions::Enable(
                            false,
                        ),
                    ),
                }),
                // 其余字段（filter / params / offset / read_consistency …）用默认值。
                // 刻意**不**显式列出：QueryPoints 有 11 个字段，全列出来只为绕开
                // 一条 lint 反而更容易漏 —— 将来协议加字段时显式写法会静默不跟进。
                ..Default::default()
            })
            .collect();

        let batch = QueryBatchPointsBuilder::new(ALIAS_NAME.to_owned(), query_points);
        let responses = self.client.query_batch(batch).await?.result;

        // 3. 请求里给过的全部要有键
        let mut results: HashMap<i64, Vec<MovieSimilarityHit>> =
            unique.into_iter().map(|id| (id, Vec::new())).collect();
        for ((source_id, _), batch_result) in sources.iter().zip(responses) {
            let mut hits = Vec::new();
            for point in batch_result.result {
                let target = match point.id.as_ref().and_then(point_id_number) {
                    Some(id) => id,
                    None => continue,
                };
                // 剔掉自己（上游 `:247-248`）
                if target == *source_id {
                    continue;
                }
                hits.push(MovieSimilarityHit {
                    movie_id: target,
                    score: normalize_score(point.score),
                });
                if hits.len() as i64 >= limit {
                    break;
                }
            }
            results.insert(*source_id, hits);
        }
        Ok(results)
    }
}

/// `PointId` 的数字分支 -> i64。非数字（UUID）返回 `None`。
fn point_id_number(id: &PointId) -> Option<i64> {
    match &id.point_id_options {
        Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(num)) => Some(*num as i64),
        _ => None,
    }
}

/// 从读取结果里挑出本索引那个命名稀疏向量。
///
/// `RetrievedPoint.vectors` 是 `VectorsOutput`（oneof），有两种形态：
/// 单个 `VectorOutput`（未命名空间）与 `NamedVectorsOutput`（命名空间）。
/// **本索引的集合是带名字建的**（`create_collection` 里 `SparseVectorConfig.map`
/// 键为 `metadata`），所以走 `NamedVectorsOutput` 分支；单分支只是兜底。
fn named_sparse_vector(output: qdrant_client::qdrant::VectorsOutput) -> Option<SparseVector> {
    use qdrant_client::qdrant::vectors_output::VectorsOptions;
    match output.vectors_options? {
        VectorsOptions::Vector(single) => sparse_of(single.into_vector()),
        VectorsOptions::Vectors(named) => named
            .vectors
            .get(VECTOR_NAME)
            .and_then(|vector| sparse_of(vector.clone().into_vector())),
    }
}
/// `vector_output::Vector` 的稀疏分支。
///
/// 没有 `try_into_sparse()`（只有 `try_into_dense()`），所以显式 match。
/// 稠密/多维分支返回 `None` —— 本索引只建了稀疏空间，拿到别的类型说明集合
/// 被改过，静默跳过比 panic 合适（调用方会把它当「这个源影片没有向量」）。
fn sparse_of(vector: qdrant_client::qdrant::vector_output::Vector) -> Option<SparseVector> {
    match vector {
        qdrant_client::qdrant::vector_output::Vector::Sparse(sparse) => Some(sparse),
        _ => None,
    }
}
