//! 瞬时推荐（上游 `moment_recommendation_service.py`，24KB / 580 行）。
//!
//! 依赖：推理客户端 [`super::embedding`]、稠密向量库
//! [`super::qdrant::dense`]、相似影片 [`super::recommendation`]。
//! **三者均已在 Rust 侧就位，本文件无外部阻塞。**
//!
//! # 形状：三个候选源合并后统一排名
//!
//! ```text
//!   种子（近期打点的缩略图）──> 推理服务取向量
//!        │
//!        ├──> 源 A 视觉相似   （Qdrant 稠密检索，:201）
//!        ├──> 源 B 相似影片   （recommendation 稀疏索引，:309）
//!        └──> 源 C 热门候选   （纯 DB，:356）
//!
//!   合并去重 ─> rank_candidates(:393) ─> top N ─> 存快照 / 直接返回
//! ```
//!
//! # 三个源是**按需降级**的，不是并行的
//!
//! 上游 `:426-432`：
//!
//! | 条件 | 动作 |
//! |---|---|
//! | 有种子 | 跑源 A |
//! | 收集到的候选 **< limit** 且有种子 | 才跑源 B |
//! | 收集到的候选 **< limit** | 才跑源 C |
//!
//! 也就是说源 B / C 是**兜底**而非补充：候选够了就不再查相似度，省一次 Qdrant
//! 往返。照抄这个短路，否则每次生成都会多打一轮向量库。
//!
//! # 一处贯穿全文的概念：`target_ratio`
//!
//! [`MomentRecommendationService::safe_ratio`] 算的是**场景在影片里的时间比例**
//! （`offset_seconds / duration_seconds`），返回 `Option<f64>` —— 时长未知的
//! 媒体返回 `None`。
//!
//! 它决定选哪张缩略图（`choose_thumbnail`）：推荐展示的是「某个时间点的画面」，
//! 拿一张比例不符的图会误导。
//!
//! **`None` 的兜底是 [`POPULAR_TARGET_RATIO`]（0.35），不是 0.0** ——
//! 比例未知时退到「影片前 35% 处」这个中性位置。把它换成 0.0 会全选到片头
//! 字幕/黑场那一段的图。
//!
//! # ⚠️ 我早先在骨架里写错的两处，已按上游改正
//!
//! **1. 去重键是 `thumbnail_id`，不是 `movie_id`。** 上游
//! `candidates_by_thumbnail_id`（`:425`）以**缩略图**为键 —— 同一部影片
//! 的**不同时刻**是**不同的推荐条目**。按影片去重会把一部影片的多个时刻
//! 压成一条，那正是「推荐时刻」这个功能的核心。
//!
//! **2. 合并时是「整条替换」，不是「合并理由码」。** 上游 `_add_candidate`
//! （`:196-199`）比较 `(score, -STRATEGY_PRIORITY[...])` 后**整个候选对象
//! 替换**旧的，`reason` 也随之被替换。我早先写的是「取最高分并合并
//! `reason_codes`」—— 那是**另一套语义**，且本仓库根本没有 `reason_codes`
//! 这个字段（上游只有单个 `reason` 字符串）。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use sm_db::repo::moment::{MediaThumbnailRepository, MomentRecommendationRepository};
use sm_db::Db;

use super::embedding::EmbeddingClient;
use super::qdrant::dense::DenseStore;
use crate::catalog::movie::{MovieCard, MovieService};
use crate::error::ServiceError;

/// 单次生成最多落多少条（上游 `MOMENT_RECOMMENDATION_LIMIT`）。
pub const LIMIT: usize = 300;
/// 种子数量上限（上游 `MOMENT_RECOMMENDATION_SEED_LIMIT`）。
pub const SEED_LIMIT: usize = 30;
/// 每个种子的视觉召回数（上游 `VISUAL_SEARCH_PER_SEED_LIMIT`）。
pub const VISUAL_SEARCH_PER_SEED_LIMIT: usize = 40;
/// 每个种子的相似影片召回数（上游 `SIMILAR_MOVIE_PER_SEED_LIMIT`）。
pub const SIMILAR_MOVIE_PER_SEED_LIMIT: usize = 50;
/// **同一部影片最多入选几条**（上游 `MAX_RECOMMENDATIONS_PER_MOVIE`）。
pub const MAX_RECOMMENDATIONS_PER_MOVIE: usize = 3;
/// 比例未知时的兜底目标位置（上游 `POPULAR_TARGET_RATIO`）。
pub const POPULAR_TARGET_RATIO: f64 = 0.35;

/// 三个候选源的策略码。
pub mod strategy {
    /// 源 A：与种子画面视觉相似。
    pub const VISUAL: &str = "visual";
    /// 源 B：来自相似影片的相近时刻。
    pub const SIMILAR_MOVIE: &str = "similar_movie";
    /// 源 C：来自热门影片的精选时刻。
    pub const POPULAR: &str = "popular";
}

/// 策略优先级。**数字越小越优先** —— 分数相同时靠它决定谁胜出。
///
/// 上游 `STRATEGY_PRIORITY`（`:54-58`）。视觉相似排第一是有意的：同分时
/// 「和你收藏的画面像」比「热门」更能说明推荐理由。
pub fn strategy_priority(strategy: &str) -> i64 {
    match strategy {
        strategy::VISUAL => 0,
        strategy::SIMILAR_MOVIE => 1,
        strategy::POPULAR => 2,
        // 上游 `.get(strategy, 99)` —— 未知策略排到最后，不是报错。
        _ => 99,
    }
}

/// 策略 → 展示文案（上游 `REASON_TEXTS`，`:59-63`）。
///
/// **只有这三个键。** 未收录的策略返回 `None`，调用方要能处理「有策略但无
/// 文案」—— 漏文案比多一条看不懂的文案好。
pub fn reason_text(strategy: &str) -> Option<&'static str> {
    Some(match strategy {
        strategy::VISUAL => "与你收藏的时刻画面相似",
        strategy::SIMILAR_MOVIE => "来自相似影片的相近时刻",
        strategy::POPULAR => "来自热门影片的精选时刻",
        _ => return None,
    })
}

/// 种子：一条待取向量的打点。
#[derive(Debug, Clone, PartialEq)]
pub struct MomentSeed {
    pub point_id: i64,
    pub media_id: i64,
    pub thumbnail_id: i64,
    pub movie_id: i64,
    /// 该打点在影片里的时间偏移（秒）。
    pub offset_seconds: i64,
    /// 该媒体时长；`None` = 库里没回填。
    pub duration_seconds: Option<i64>,
    /// 新鲜度权重 `1 - index / total`（上游 `:153`）。
    pub recency_score: f64,
}

/// 候选：三个源合并后的统一形状（上游 `_MomentCandidate`，`:75-87`）。
///
/// 只存 **id** 不存完整模型 —— 候选池可能有上千条，带上完整 `Media` /
/// `Movie` 会把内存拖垮（与 `daily_recommendation` 同一个理由）。
#[derive(Debug, Clone, PartialEq)]
pub struct MomentCandidate {
    pub thumbnail_id: i64,
    pub media_id: i64,
    pub movie_id: i64,
    /// 影片热度，**只用于同分 tie-break**（`:399`），不参与打分。
    pub movie_heat: Option<i64>,
    pub score: f64,
    pub strategy: &'static str,
    /// 展示文案。由 [`reason_text`] 得出，**不随候选池传递**。
    pub reason: &'static str,
    pub seed_point_id: Option<i64>,
    pub seed_thumbnail_id: Option<i64>,
    pub source_movie_id: Option<i64>,
    /// 视觉相似原始分。`popular` 源为 `None`。
    pub visual_score: Option<f64>,
    /// 影片相似度原始分。`visual` / `popular` 源为 `None`。
    pub movie_similarity_score: Option<f64>,
}

/// 一行已落库的时刻推荐（上游 `MomentRecommendation` 表的投影）。
///
/// **不含 `image` / `movie`** —— 那两个是 `MovieListItemResource` 与
/// `ImageResource`，要签名密钥，属于 API 层。它们装在
/// [`MomentRecommendationCard`] 里与这一行同行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MomentRecommendationRow {
    pub recommendation_id: i64,
    pub rank: i64,
    pub score: f64,
    pub strategy: String,
    pub reason: String,
    pub media_id: i64,
    pub thumbnail_id: i64,
    /// 该缩略图在影片里的偏移（秒）。
    pub offset_seconds: i64,
    pub movie_id: i64,
}

impl MomentRecommendationRow {
    /// 仓储元组 → 具名行。
    ///
    /// 位置对应 `sm_db::repo::moment` 里那条投影的九列：
    /// `(recommendation_id, rank, score, strategy, reason, media_id,
    /// thumbnail_id, offset_seconds, movie_id)` —— 前四个里三个是 `i32`，
    /// **位置写错不会编译失败**，所以只在仓储那一处做这个换算（`list_items`
    /// 是唯一消费方）。
    fn from_tuple(row: sm_db::repo::moment::MomentRecommendationRow) -> Self {
        Self {
            recommendation_id: i64::from(row.0),
            rank: i64::from(row.1),
            score: row.2,
            strategy: row.3,
            reason: row.4,
            media_id: i64::from(row.5),
            thumbnail_id: i64::from(row.6),
            offset_seconds: i64::from(row.7),
            movie_id: i64::from(row.8),
        }
    }
}

/// 分页（对应上游 `MomentRecommendationPageResource`）。
///
/// **有 `total`，也有 `generated_at`** —— 我早先在路由骨架里断言「专用资源
/// 所以没有总数字段」，那是**错的**：上游 `schema/discovery/
/// moment_recommendations.py:21-26` 两个字段都有。
/// ⚠️ `items` 的条数**可能小于 `page_size`，也可能小于 `total`**：装配时
/// 取不到缩略图图片或影片卡片的行会被跳过且**不补位**（见
/// [`MomentRecommendationQuery::list_items`]）。
///
/// **不派生 `Serialize`**：`MovieCard` 是服务层形态（不是线格式），页面的
/// 线格式由 `sm-api` 的 `MomentRecommendationResponse` 决定 —— 与
/// `daily_recommendation` 的做法一致。
#[derive(Debug, Clone)]
pub struct MomentRecommendationPage {
    pub items: Vec<MomentRecommendationCard>,
    pub page: i64,
    pub page_size: i64,
    /// **仍然有效的推荐数**（媒体有效 + 影片未黑名单），不是表里的总行数。
    pub total: i64,
    /// 最近一次生成时间，形如 `2026-10-09T12:34:56`（本仓统一格式，秒以下
    /// 截断）。`None` = 还没生成过。
    pub generated_at: Option<String>,
}

/// 生成统计。**键名与上游逐字一致** —— 它会进任务运行的 `summary`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerateStats {
    pub seed_points: usize,
    pub visual_candidates: usize,
    pub similar_candidates: usize,
    pub popular_candidates: usize,
    pub stored_items: usize,
}

/// 缩略图的图片素材（签名所需的两个字段）。
///
/// `origin` 是**未签名**的相对路径 —— 签名要密钥，在 API 层做。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailImage {
    pub id: i32,
    pub origin: String,
}

/// 一条读侧装配结果：快照行 + 缩略图图片 + 影片卡片。
///
/// 后两者的**原始素材**（服务层类型）而不是线格式 DTO —— 上游
/// `list_items` 里 `MovieListResource.from_attributes_model(...)` 与
/// `ImageResource.from_attributes_model(thumbnail.image)` 都在 API 层做，
/// 因为要签名密钥。这里给出 `MovieCard` / [`ThumbnailImage`]，路由照着
/// `movies.rs` 的同一套（`MovieListItemResource::from_movie_card`）转。
///
/// # 这替换了骨架期的 `PageContext` trait
///
/// 那个 trait 想的是「service 不该知道卡片 DTO 的形状，让 API 层注入取数」。
/// 但 `MovieCard` 本来就是**服务层类型**（`crate::catalog::movie`），
/// `movies.rs` 与 `daily_recommendation` 走的都是「服务给卡片、路由签名」——
/// 再为一处读侧发明一个返回 `serde_json::Value` 的注入缝，只会让形状无人
/// 校验（`Value` 里字段写错不会编译失败）。用 trait 的唯一好处是「不起
/// 数据库依赖」，而 `daily_recommendation` 已经证明读侧持有 `Db` 是可接受的。
#[derive(Debug, Clone)]
pub struct MomentRecommendationCard {
    /// 快照行本身。
    pub row: MomentRecommendationRow,
    /// 该缩略图的图片。**取不到就整条跳过**（上游 `:556`）。
    pub image: ThumbnailImage,
    /// 影片卡片（[`MovieService::load_cards`] 的产物）。取不到同样跳过。
    pub card: MovieCard,
}

/// 读取已存的瞬时推荐快照（`GET /moment-recommendations`）。
///
/// # 为什么与生成侧分开
///
/// 生成侧（[`MomentRecommendationService`]）要 Qdrant 稠密库 + 推理客户端，
/// 而这条端点**只读库**。合成一个类型、让路由去构造向量库依赖，或者为它把
/// 服务塞进 `AppState`，都是「为一个只读端点把生成侧的依赖拖成进程级」。
/// 同一模块里 `hot_actress_release::HotActressReleaseQuery` 是同一取舍。
#[derive(Debug, Clone)]
pub struct MomentRecommendationQuery {
    db: Db,
}

impl MomentRecommendationQuery {
    /// 构造。取 `&Db` 并克隆（与 `DailyRecommendationService::new` 同形）。
    pub fn new(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    /// 分页参数非法时的错误码。
    ///
    /// 上游 `list_items`（`moment_recommendation_service.py:520`）传
    /// `error_code="invalid_moment_recommendation_filter"`。
    pub const INVALID_FILTER: &str = "invalid_moment_recommendation_filter";

    /// 校验分页参数。违规 → 422 [`Self::INVALID_FILTER`]。
    ///
    /// ★ **三个 discovery 端点的分页口径是统一的**（都走
    /// `sm_core::pagination::validate_page`，`1 <= page`、`1 <= page_size <= 100`）。
    /// 路由文档里曾写「moment 没有任何边界，`page_size=100000` 合法」—— 那是
    /// 把**上游 pydantic 的 Query 边界**当成了唯一一道闸：边界有两道，而
    /// 服务层这道对三条流一视同仁。
    ///
    /// 真正的差别只在**错误码**：daily / hot-actress 在上游被 pydantic 先拦
    /// （`validation_error`），moment 无 Query 边界，直接落到服务层的专用码。
    #[allow(clippy::result_large_err)]
    fn validate_page(page: i64, page_size: i64) -> Result<(), ServiceError> {
        sm_core::pagination::validate_page(page, page_size).map_err(|error| {
            let details = match error.details() {
                serde_json::Value::Object(map) => map,
                other => {
                    let mut map = serde_json::Map::new();
                    map.insert("page".to_owned(), other);
                    map
                }
            };
            ServiceError::validation_with(Self::INVALID_FILTER, error.message(), details)
        })
    }

    /// 读快照并装配成卡片。上游 `list_items`（`:509-580`）。
    ///
    /// # 三件事与上游逐条对齐
    ///
    /// 1. **`total` 是「仍然有效的推荐数」**（`count_valid`：媒体有效 + 影片
    ///    未黑名单），**不是**表里行数 —— 失效媒体的行还在表里。
    /// 2. **取不到缩略图图片或影片卡片的行直接跳过，且不补位**（上游
    ///    `:556-557` 的 `continue`）→ `items.len()` 可能小于 `page_size`，
    ///    也可能小于 `total`。这是个**故意的**不齐：补位会让客户端以为还有
    ///    下一页。
    ///
    ///    ⚠️ **这条在正常路径上不可达**：`moment_recommendation` 的
    ///    `thumbnail_id` / `media_id` / `movie_id` 三列都是
    ///    `ON DELETE CASCADE`（`docker/schema.sql:522-526`），而 `list_valid`
    ///    对 `media` / `movie` 都是 INNER JOIN —— 所以「行还在、缩略图/影片
    ///    没了」这种**状态**造不出来。它接的只是**两次查询之间的并发删除**
    ///    （毫秒级窗口），与 `daily_recommendation` 那处跳过同类。保留它是因为
    ///    上游有、且窗口真实存在；但别以为它能被稳定触发（见
    ///    `moment_recommendations_http.rs` 的 `a_deleted_thumbnail_takes_the_recommendation_row_with_it`）。
    /// 3. **`generated_at` 每次现取**（`latest_generated_at`），不是快照行上的
    ///    时间 —— 整表替换（见 `repo::moment` 模块文档）保证全表同一个时间，
    ///    但空表时它是 `None`（「还没生成过」），行上取不到这个信息。
    pub async fn list_items(
        &self,
        page: i64,
        page_size: i64,
    ) -> Result<MomentRecommendationPage, ServiceError> {
        Self::validate_page(page, page_size)?;
        let repo = MomentRecommendationRepository::new(self.db.clone());
        // ⚠️ `list_valid` 的第二个参数是 **offset 不是页码**（上游 Peewee
        // `.paginate(page, page_size)` 是 1 基，仓储那层已经换算成 SQL 的
        // `OFFSET`）。直接把 `page` 递进去，`page=1` 会跳掉第一条 ——
        // 而这个偏移量错法只在**恰好有跨页数据**时才看得出来。
        let offset = (page - 1) * page_size;
        let rows = repo.list_valid(offset, page_size).await?;
        let total = repo.count_valid().await?;
        let generated_at = repo.latest_generated_at().await?;

        // 两批取数都只在有行时才发查询（空列表在 SQL 里是语法错，仓储已挡，
        // 这里再挡一次是为了省一次往返）。
        let thumbnail_ids: Vec<i32> = rows.iter().map(|row| row.6).collect();
        let images: HashMap<i32, ThumbnailImage> = MediaThumbnailRepository::new(self.db.clone())
            .images_by_ids(&thumbnail_ids)
            .await?
            .into_iter()
            .map(|(thumbnail_id, image_id, image_origin)| {
                (
                    thumbnail_id,
                    ThumbnailImage {
                        id: image_id,
                        origin: image_origin,
                    },
                )
            })
            .collect();

        let movie_ids: Vec<i32> = rows.iter().map(|row| row.8).collect();
        let mut cards: HashMap<i32, MovieCard> = MovieService::new(&self.db)
            .load_cards(&movie_ids)
            .await?
            .into_iter()
            .map(|card| (card.movie.id, card))
            .collect();

        let items = rows
            .into_iter()
            .filter_map(|row| {
                // 缩略图/影片在分页与装配之间被删（或媒体被判失效）—— 跳过。
                let image = images.get(&row.6)?.clone();
                let card = cards.remove(&row.8)?;
                Some(MomentRecommendationCard {
                    row: MomentRecommendationRow::from_tuple(row),
                    image,
                    card,
                })
            })
            .collect();

        Ok(MomentRecommendationPage {
            items,
            page,
            page_size,
            total,
            generated_at: generated_at.map(|time| time.format("%Y-%m-%dT%H:%M:%S").to_string()),
        })
    }
}

/// 瞬时推荐服务。
// 两个依赖尚未被方法体引用（`generate_recommendations` 还是 `todo!()`）。
#[allow(dead_code)]
pub struct MomentRecommendationService {
    store: Arc<DenseStore>,
    embedding: Arc<EmbeddingClient>,
}

impl MomentRecommendationService {
    /// 构造。
    pub fn new(store: Arc<DenseStore>, embedding: Arc<EmbeddingClient>) -> Self {
        Self { store, embedding }
    }

    /// 种子新鲜度权重。上游 `:153`。
    ///
    /// `total <= 1` 时**给 1.0**：分母会退化成 0，只有一个种子时它必须拿满分，
    /// 否则唯一的候选得分被抹平。
    pub fn recency_score(index: usize, total: usize) -> f64 {
        if total <= 1 {
            return 1.0;
        }
        1.0 - (index as f64 / total as f64)
    }

    /// 场景时间比例。`duration_seconds` 未知或非正时返回 `None`。
    ///
    /// 上游 `_safe_ratio`（`:110-115`）。**注意有 clamp**：
    /// `max(0.0, min(1.0, offset / duration))` —— 打点偏移可能越界（脏数据或
    /// 重新封装导致时长变短），不夹住会算出 `> 1.0` 的比例，再拿去
    /// `int(duration * ratio)` 定位缩略图就会落到列表末尾之外。
    ///
    /// **`None` 必须一路传播**（见模块文档），它的兜底是
    /// [`POPULAR_TARGET_RATIO`] 而不是 0.0。
    pub fn safe_ratio(offset_seconds: i64, duration_seconds: Option<i64>) -> Option<f64> {
        let duration = duration_seconds.unwrap_or(0);
        if duration <= 0 {
            return None;
        }
        Some((offset_seconds as f64 / duration as f64).clamp(0.0, 1.0))
    }

    /// 热度分。上游 `_heat_score`（`:106-108`）：`clamp(heat / 100, 0, 1)`。
    ///
    /// **分母写死 100**，不是 95 分位（那是 `daily_recommendation` 的做法）。
    /// 两者不要混用：这里 heat 是 0~100 的整数刻度，量纲本来就固定。
    pub fn heat_score(heat: Option<i64>) -> f64 {
        (heat.unwrap_or(0) as f64 / 100.0).clamp(0.0, 1.0)
    }

    /// 目标偏移秒数。`target_ratio` 为 `None` 时退到 [`POPULAR_TARGET_RATIO`]。
    ///
    /// 上游散在三处的 `int((media.duration_seconds or 0) * ratio)`。抽出来是
    /// 因为**三处必须一致** —— 视觉源、相似源、热门源各算一次，比例算法
    /// 有一处不同就会让同一部影片在三源里被选到不同时刻。
    pub fn desired_offset(duration_seconds: Option<i64>, target_ratio: Option<f64>) -> i64 {
        let ratio = target_ratio.unwrap_or(POPULAR_TARGET_RATIO);
        (duration_seconds.unwrap_or(0) as f64 * ratio) as i64
    }

    /// 源 A 打分：视觉相似为主，热度与新鲜度为辅。
    ///
    /// 上游 `:237`：`0.75 * visual + 0.15 * heat + 0.10 * recency`。
    pub fn score_visual(visual: f64, heat: f64, recency: f64) -> f64 {
        0.75 * visual + 0.15 * heat + 0.10 * recency
    }

    /// 源 B 打分。上游 `:335`：`0.65 * similarity + 0.20 * heat + 0.15 * recency`。
    ///
    /// 视觉权重比源 A 低（0.65 < 0.75）且热度更高 —— 相似影片是**间接**信号，
    /// 不如直接看到画面相似可信。
    pub fn score_similar_movie(similarity: f64, heat: f64, recency: f64) -> f64 {
        0.65 * similarity + 0.20 * heat + 0.15 * recency
    }

    /// 源 C 打分。上游 `:373`：**只有热度**。
    ///
    /// 没有 recency 项 —— 热门源与种子无关（它甚至不需要种子）。写成
    /// `0.60 * heat` 而不是归一到 1.0：三个源的量纲要可比，热门源
    /// 刻意压低，让它在同分时更少胜出。
    pub fn score_popular(heat: f64) -> f64 {
        0.60 * heat
    }

    /// 合并去重。**键是 `thumbnail_id`**（见模块文档）。
    ///
    /// 冲突时按 `(score, -priority)` 取大，**整条替换** —— `reason` 也跟着
    /// 换成胜出那条的。上游 `:196-199`。
    ///
    /// 返回是否**新增**了条目（上游靠 `len()` 变化统计 `added_count`）。
    pub fn add_candidate(
        pool: &mut HashMap<i64, MomentCandidate>,
        candidate: MomentCandidate,
    ) -> bool {
        match pool.get(&candidate.thumbnail_id) {
            None => {
                pool.insert(candidate.thumbnail_id, candidate);
                true
            }
            Some(existing) => {
                let existing_key = (existing.score, -strategy_priority(existing.strategy) as f64);
                let candidate_key = (
                    candidate.score,
                    -strategy_priority(candidate.strategy) as f64,
                );
                if candidate_key > existing_key {
                    pool.insert(candidate.thumbnail_id, candidate);
                }
                // 无论是否替换，`added_count` 都不涨 —— 上游只在
                // `len()` 变大时计数。
                false
            }
        }
    }

    /// 候选排名。上游 `_rank_candidates`（`:392-413`）。
    ///
    /// 排序键四段：**分数降序 → 策略优先级升序 → 热度降序 → thumbnail_id 升序**。
    /// 后三段都是**确定性 tie-break** —— 没有它们，同分候选的顺序会随
    /// `HashMap` 迭代顺序抖动，而分页会把这个抖动暴露给用户（翻页时同一条
    /// 出现在两页）。
    ///
    /// ⚠️ **我早先写的注释是错的**：我写「三源量纲不同，必须先归一化再比」。
    /// 上游**没有归一化这一步** —— 归一化发生在更早的 `heat_score` /
    /// `clamp` 里。这里就是直接比原始加权和。
    ///
    /// 再叠一条**每片最多 [`MAX_RECOMMENDATIONS_PER_MOVIE`] 条**：避免一部
    /// 热门影片刷满整个推荐池。上游 `:403-410` 是 `continue`（跳过），
    /// **不补位** —— 所以返回值可能比 `limit` 短。
    pub fn rank_candidates(
        candidates: &mut [MomentCandidate],
        limit: usize,
    ) -> Vec<MomentCandidate> {
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| strategy_priority(a.strategy).cmp(&strategy_priority(b.strategy)))
                .then_with(|| b.movie_heat.unwrap_or(0).cmp(&a.movie_heat.unwrap_or(0)))
                .then_with(|| a.thumbnail_id.cmp(&b.thumbnail_id))
        });
        let mut per_movie: HashMap<i64, usize> = HashMap::new();
        let mut ranked: Vec<MomentCandidate> = Vec::with_capacity(limit.min(candidates.len()));
        for candidate in candidates.iter() {
            let count = per_movie.get(&candidate.movie_id).copied().unwrap_or(0);
            if count >= MAX_RECOMMENDATIONS_PER_MOVIE {
                continue;
            }
            ranked.push(candidate.clone());
            per_movie.insert(candidate.movie_id, count + 1);
            if ranked.len() >= limit {
                break;
            }
        }
        ranked
    }

    /// 生成瞬时推荐并落快照。上游 `generate_recommendations`（`:415-508`，95 行）。
    ///
    /// **仍是 `todo!()`，但只剩一块前置**：`sm_db::repo::moment`（本轮新增）
    /// 已覆盖取种子、按 id / 按影片取缩略图、热门候选、整表替换、分页读、
    /// 计数与最近生成时间；打分与排名是本文件的纯函数。唯一还缺的是
    /// **读种子图字节** —— 上游 `_read_seed_image_bytes`（`:165-173`）读
    /// `image.origin` 指向的磁盘文件，而 `image` 表只有路径列
    /// （`schema.sql:144-149`），本仓库还没有 image store 模块。
    pub async fn generate_recommendations(
        &self,
        limit: usize,
    ) -> Result<GenerateStats, ServiceError> {
        let _ = limit;
        todo!("骨架：编排已就位，只差 image store 的读图字节（种子/选图/热门/落库/打分的仓储都已在 sm_db::repo::moment）")
    }
}
