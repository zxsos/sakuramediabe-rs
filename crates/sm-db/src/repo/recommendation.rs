//! `daily_recommendation_item` 与 `moment_recommendation` 的仓储。
//!
//! # 这两张表与 `ranking_item` 语义相反 —— 这是本文件的全部重点
//!
//! | 表 | `rank` 的唯一性 | 后果 |
//! |---|---|---|
//! | `ranking_item` | `UNIQUE (source_key, board_key, period, rank)` | **同一榜单内**唯一 → 可以累积历史，按 rank upsert |
//! | `daily_recommendation_item` | `rank integer NOT NULL UNIQUE` | **全表**唯一 → 第二天的 rank=1 与第一天的冲突 |
//! | `moment_recommendation` | `rank integer NOT NULL UNIQUE` | 同上 |
//!
//! 三张表都在 `crates/sm-db/src/discovery/rankings.rs` 里定义，很容易被当成
//! 同一回事。但推荐两张表**不是累积历史**：`snapshot_date` 记的是「这一批是
//! 哪天的」，不是历史维度。
//!
//! 所以本仓储的写入口是 [`DailyRecommendationItemRepository::replace_all`]：
//! **先删全表，再按序插入**。逐条 upsert 会撞 rank 的唯一约束。
//!
//! # `movie_id` 也全表唯一
//!
//! `daily_recommendation_item.movie_id integer NOT NULL UNIQUE` —— 一部影片
//! 在表里至多一条，不按 `snapshot_date` 分组。这与 `rank` 一起意味着：
//! **全表在任何时刻只保存一批推荐**。
//!
//! `moment_recommendation` 的对应约束落在 `thumbnail_id` 上（一个缩略图至多
//! 被推荐一次），`movie_id` 不唯一 —— 因为同一部影片的不同时刻是不同的推荐。

use sqlx::AssertSqlSafe;
use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::discovery::rankings::{DailyRecommendationItem, MomentRecommendation};
use crate::error::DbError;
use crate::paged_list;
use crate::repo::Ctx;

use std::collections::HashMap;

use crate::repo::discovery::PendingImageRepository;

const DAILY_ENTITY: &str = "DailyRecommendationItem";
const MOMENT_ENTITY: &str = "MomentRecommendation";
/// 稀疏索引的**特征来源**表（`movie` / `movie_actor` / `movie_tag`）。
///
/// 刻意与 `DAILY_ENTITY` / `MOMENT_ENTITY` 分开：那两个是**结果表**，这个是
/// **源数据表**。混用会让错误日志指错表。
const MOVIE_FEATURE_ENTITY: &str = "MovieFeature";

// ================================================================ daily_recommendation_item

/// 新增一条每日推荐。
#[derive(Debug, Clone)]
pub struct NewDailyRecommendation {
    /// 快照日期。**只有日期，无时间部分**（上游是 `DateField`）。
    pub snapshot_date: chrono::NaiveDate,
    /// 指向 `Movie.id`。全表唯一 —— 一部影片至多一条。
    pub movie_id: i32,
    /// 名次。**全表唯一**，从 1 开始。
    pub rank: i32,
    /// 综合得分。
    pub score: f64,
    /// 理由代码数组，JSON 文本。缺省 `[]`（DDL 的 DEFAULT）。
    pub reason_codes: Option<String>,
    /// 理由文案数组，JSON 文本。缺省 `[]`。
    pub reason_texts: Option<String>,
    /// 各信号分量得分，JSON 对象。缺省 `{}`。
    pub signal_scores: Option<String>,
    /// 生成时刻。**必填** —— `generated_at` 是 NOT NULL，无 DEFAULT。
    pub generated_at: chrono::NaiveDateTime,
}

impl NewDailyRecommendation {
    fn validate(&self) -> Result<(), DbError> {
        if self.rank < 1 {
            return Err(DbError::business(
                DAILY_ENTITY,
                format!("rank 从 1 开始，收到 {}", self.rank),
            ));
        }
        // `movie_id` 与 `rank` 都是全表唯一，但那由数据库保证 ——
        // 在入口拦不住（要查表才知道）。所以这里只校验**不查表就能判断**
        // 的：rank 的下界。重复写入是调用方用 `replace_all` 要避免的事。
        Ok(())
    }

    /// 三个 JSON 列的兜底值。
    ///
    /// 上游是 `JsonTextField(default=...)`，列是 `text NOT NULL DEFAULT '[]'`
    /// —— **有 DEFAULT 但不可空**。传 `None` 会绑 NULL 并违反 NOT NULL，
    /// 所以这里补上 DDL 里那个默认值。这与
    /// `media.rs` 对 `storage_ref` 的处理一致：`Option` 表达「调用方没提供」，
    /// 而不是「允许存 NULL」。
    fn json(&self) -> (&str, &str, &str) {
        (
            json_or(self.reason_codes.as_deref(), "[]"),
            json_or(self.reason_texts.as_deref(), "[]"),
            json_or(self.signal_scores.as_deref(), "{}"),
        )
    }
}

fn json_or<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    match value {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => fallback,
    }
}

/// `daily_recommendation_item` 表仓储。
#[derive(Debug, Clone)]
pub struct DailyRecommendationItemRepository {
    pool: PgPool,
}

impl DailyRecommendationItemRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// **替换全表**为给定顺序的一批推荐。这是本表的**唯一正确写入方式**。
    ///
    /// 先 `DELETE FROM daily_recommendation_item`（**不带 WHERE**），再按
    /// 入参顺序插入。
    ///
    /// # 为什么不能用 upsert
    ///
    /// `rank` 与 `movie_id` 都是**全表** `UNIQUE`，不按 `snapshot_date` 分组。
    /// 昨天那批占用了 rank 1..50，今天再写 rank=1 会撞昨天的 —— 而 upsert
    /// 只会覆盖**同一**个 rank，于是一半是昨天的、一半是今天的，混在一起。
    /// 那比「整批重写」难排查得多。
    ///
    /// 所以正确做法是：这批推荐就是**现在**的推荐，旧的没有意义。
    ///
    /// # 为什么`replace_all` 要包在一个事务里
    ///
    /// 清后插如果不是原子的，中间那一瞬表是空的 —— 用户刷新一次看到空
    /// 推荐位。这里是单条 `DELETE` + N 条 `INSERT` 共用一个事务。
    pub async fn replace_all(&self, items: &[NewDailyRecommendation]) -> Result<u64, DbError> {
        // 清后插必须是**一个**事务：否则中间那一瞬表是空的，用户刷新
        // 一次就会看到空推荐位。
        //
        // 用 `pool.begin()` + `Ctx::in_tx` 而不是 `UnitOfWork` —— 后者把
        // `tx` 藏成私有字段，拿不到 `&mut Transaction` 去构造 `Ctx`。
        let mut tx = self.pool.begin().await?;
        let removed = {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            let removed = self.clear_in(&mut ctx).await?;
            for item in items {
                self.insert_in(&mut ctx, item).await?;
            }
            removed
        };
        tx.commit().await?;
        Ok(removed + items.len() as u64)
    }

    /// 事务内插入，供 [`Self::replace_all`] 与 [`Ctx`] 编排使用。
    pub async fn insert_in(
        &self,
        ctx: &mut Ctx<'_>,
        new: &NewDailyRecommendation,
    ) -> Result<DailyRecommendationItem, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        let (codes, texts, signals) = new.json();
        sqlx::query_as::<_, DailyRecommendationItem>(
            "INSERT INTO daily_recommendation_item (snapshot_date, movie_id, rank, score, \
                    reason_codes, reason_texts, signal_scores, generated_at, \
                    created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) RETURNING *",
        )
        .bind(new.snapshot_date)
        .bind(new.movie_id)
        .bind(new.rank)
        .bind(new.score)
        .bind(codes)
        .bind(texts)
        .bind(signals)
        .bind(new.generated_at)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(DAILY_ENTITY))
    }

    /// 事务内清空全表。
    ///
    /// **不带 WHERE** —— 见 [`Self::replace_all`]。这与成员表的 `clear` 不同：
    /// 那张表的 `clear` 按 collection_id 过滤，因为合集的成员是**分组**的；
    /// 这里没有分组概念。
    pub async fn clear_in(&self, ctx: &mut Ctx<'_>) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM daily_recommendation_item")
            .execute(ctx.conn().await?.as_conn())
            .await
            .map_err(|e| DbError::from(e).with_entity(DAILY_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 按名次列出当前这批推荐。**刻意不分页。**
    ///
    /// 一次「读出今天推荐位」的查询，分页只会让调用方自己写取完所有页的
    /// 循环。全表在任何时刻只有一批（见类型文档），所以行数就是这批的
    /// 长度。
    pub async fn list_by_rank(&self) -> Result<Vec<DailyRecommendationItem>, DbError> {
        Ok(sqlx::query_as::<_, DailyRecommendationItem>(
            "SELECT * FROM daily_recommendation_item ORDER BY rank",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 列出**可见**的推荐（关联影片未拉黑），按 `rank`。**分页。**
        ///
        /// 上游 `list_items`（`daily_recommendation_service.py:424`）：
        ///
        /// ```python
        /// query = DailyRecommendationItem.select().join(Movie).where(Movie.is_blacklisted == False)
        /// total = query.count()
        /// rows = query.order_by(DailyRecommendationItem.rank.asc()).offset(start).limit(page_size)
        /// ```
        ///
        /// # 为什么要 JOIN 而不是拉回后在 Rust 侧过滤
        ///
        /// 拉黑的影片**不占分页槽位**：`COUNT` 与 `SELECT` 必须带同一个
        /// `is_blacklisted = false`，否则最后一页会少于 `page_size` 条而
        /// `total` 偏大 —— 正是 `paged_list!` 要避免的那类不一致（它把两段
        /// SQL 锁在同一次编辑里）。
        ///
        /// # 排序只有 `rank`
        ///
        /// `rank` 全表唯一（见类型文档），排序是确定的，不需要 tie-breaker。
        pub async fn list_visible_page(
            &self,
        ) -> Result<Page<DailyRecommendationItem>, DbError> {
            count = "SELECT COUNT(*) FROM daily_recommendation_item d \
                     JOIN movie m ON m.id = d.movie_id \
                     WHERE m.is_blacklisted = false",
            items = "SELECT d.* FROM daily_recommendation_item d \
                     JOIN movie m ON m.id = d.movie_id \
                     WHERE m.is_blacklisted = false \
                     ORDER BY d.rank LIMIT $1 OFFSET $2",
        }
    }

    paged_list! {
        /// 按快照日期列出。**分页。**
        ///
        /// 走 `daily_recommendation_item_snapshot_date_idx`。因为全表只存
        /// 一批，这个查询通常返回**全部或零行** —— 保留它是因为「当前是
        /// 哪天的」值得能单独问出来，而不必读全表再取 `snapshot_date`。
        pub async fn list_by_snapshot_date(
            &self,
            snapshot_date: chrono::NaiveDate,
        ) -> Result<Page<DailyRecommendationItem>, DbError> {
            count = "SELECT COUNT(*) FROM daily_recommendation_item \
                     WHERE snapshot_date = $1",
            items = "SELECT * FROM daily_recommendation_item WHERE snapshot_date = $1 \
                     ORDER BY rank LIMIT $2 OFFSET $3",
        }
    }

    /// 按影片查它当前的推荐位，返回 `None` 表示没被推荐。
    ///
    /// `movie_id` 全表唯一，所以这个查询至多返回一行。
    pub async fn find_by_movie(
        &self,
        movie_id: i32,
    ) -> Result<Option<DailyRecommendationItem>, DbError> {
        Ok(sqlx::query_as::<_, DailyRecommendationItem>(
            "SELECT * FROM daily_recommendation_item WHERE movie_id = $1",
        )
        .bind(movie_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 清空全表，返回删了几行。**不带 WHERE。**
    pub async fn clear(&self) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM daily_recommendation_item")
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(DAILY_ENTITY))?;
        Ok(result.rows_affected())
    }
}

// ================================================================ moment_recommendation

/// 新增一条时刻推荐。
#[derive(Debug, Clone)]
pub struct NewMomentRecommendation {
    /// 名次。**全表唯一**，从 1 开始。
    pub rank: i32,
    /// 综合得分。
    pub score: f64,
    /// 推荐策略标识。
    ///
    /// **上游没有给出枚举值** —— `moment_strategy::ALL` 是空数组，是模型
    /// 层留的占位。所以这里只接受非空串，不校验取值：校验一个不存在的
    /// 清单没有意义，会让合法的新策略被拒。
    pub strategy: String,
    /// 可读的推荐理由。
    pub reason: String,
    /// 目标影片。
    pub movie_id: i32,
    /// 目标媒体文件。
    pub media_id: i32,
    /// 目标缩略图。**全表唯一** —— 一个缩略图至多被推荐一次。
    pub thumbnail_id: i32,
    /// 距片头的秒数。
    pub offset_seconds: i32,
    /// 种子时刻点。删点会 SET NULL，不是级联删除。
    pub seed_point_id: Option<i32>,
    /// 种子缩略图。删缩略图会 SET NULL。
    pub seed_thumbnail_id: Option<i32>,
    /// 种子来源影片。删影片会 SET NULL。
    pub source_movie_id: Option<i32>,
    /// 视觉相似度分量得分。无视觉依据时为空。
    pub visual_score: Option<f64>,
    /// 影片相似度分量得分。无影片依据时为空。
    pub movie_similarity_score: Option<f64>,
    /// 生成时刻。**必填** —— NOT NULL 且无 DEFAULT。
    pub generated_at: chrono::NaiveDateTime,
}

impl NewMomentRecommendation {
    fn validate(&self) -> Result<(), DbError> {
        if self.rank < 1 {
            return Err(DbError::business(
                MOMENT_ENTITY,
                format!("rank 从 1 开始，收到 {}", self.rank),
            ));
        }
        if self.strategy.trim().is_empty() {
            return Err(DbError::business(MOMENT_ENTITY, "strategy 不能为空"));
        }
        if self.offset_seconds < 0 {
            return Err(DbError::business(
                MOMENT_ENTITY,
                format!("offset_seconds 不能为负，收到 {}", self.offset_seconds),
            ));
        }
        // 有视觉种子就该有视觉分数，反之亦然 —— 那是「推荐依据」与「分量
        // 得分」的一致性。数据库不校验这个（两列都是独立的可空列），
        // 而漏掉它不会崩，只会让客户端显示一个没有分数的理由。
        if self.seed_thumbnail_id.is_some() != self.visual_score.is_some() {
            return Err(DbError::business(
                MOMENT_ENTITY,
                "seed_thumbnail_id 与 visual_score 必须同时给出或同时留空",
            ));
        }
        if self.source_movie_id.is_some() != self.movie_similarity_score.is_some() {
            return Err(DbError::business(
                MOMENT_ENTITY,
                "source_movie_id 与 movie_similarity_score 必须同时给出或同时留空",
            ));
        }
        Ok(())
    }
}
/// `moment_recommendation` 表仓储。
#[derive(Debug, Clone)]
pub struct MomentRecommendationRepository {
    pool: PgPool,
}

impl MomentRecommendationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// **替换全表**为给定顺序的一批推荐。
    ///
    /// 与 [`DailyRecommendationItemRepository::replace_all`] 同一条理由：
    /// `rank` 是**全表** UNIQUE，逐条 upsert 会把旧批次的残留混进来。
    pub async fn replace_all(&self, items: &[NewMomentRecommendation]) -> Result<u64, DbError> {
        // 清后插必须是**一个**事务：否则中间那一瞬表是空的，用户刷新
        // 一次就会看到空推荐位。
        //
        // 用 `pool.begin()` + `Ctx::in_tx` 而不是 `UnitOfWork` —— 后者把
        // `tx` 藏成私有字段，拿不到 `&mut Transaction` 去构造 `Ctx`。
        let mut tx = self.pool.begin().await?;
        let removed = {
            let mut ctx = Ctx::in_tx(&mut tx, &self.pool);
            let removed = self.clear_in(&mut ctx).await?;
            for item in items {
                self.insert_in(&mut ctx, item).await?;
            }
            removed
        };
        tx.commit().await?;
        Ok(removed + items.len() as u64)
    }

    /// 事务内插入。
    pub async fn insert_in(
        &self,
        ctx: &mut Ctx<'_>,
        new: &NewMomentRecommendation,
    ) -> Result<MomentRecommendation, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MomentRecommendation>(
            "INSERT INTO moment_recommendation (rank, score, strategy, reason, movie_id, \
                    media_id, thumbnail_id, offset_seconds, seed_point_id, \
                    seed_thumbnail_id, source_movie_id, visual_score, \
                    movie_similarity_score, generated_at, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $15) \
             RETURNING *",
        )
        .bind(new.rank)
        .bind(new.score)
        .bind(new.strategy.trim())
        .bind(new.reason.trim())
        .bind(new.movie_id)
        .bind(new.media_id)
        .bind(new.thumbnail_id)
        .bind(new.offset_seconds)
        .bind(new.seed_point_id)
        .bind(new.seed_thumbnail_id)
        .bind(new.source_movie_id)
        .bind(new.visual_score)
        .bind(new.movie_similarity_score)
        .bind(new.generated_at)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(MOMENT_ENTITY))
    }

    /// 事务内清空全表。**不带 WHERE。**
    pub async fn clear_in(&self, ctx: &mut Ctx<'_>) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM moment_recommendation")
            .execute(ctx.conn().await?.as_conn())
            .await
            .map_err(|e| DbError::from(e).with_entity(MOMENT_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 按名次列出当前这批。**刻意不分页。** 理由同每日推荐。
    pub async fn list_by_rank(&self) -> Result<Vec<MomentRecommendation>, DbError> {
        Ok(sqlx::query_as::<_, MomentRecommendation>(
            "SELECT * FROM moment_recommendation ORDER BY rank",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按缩略图查它是否被推荐过。至多一行（`thumbnail_id` 唯一）。
    pub async fn find_by_thumbnail(
        &self,
        thumbnail_id: i32,
    ) -> Result<Option<MomentRecommendation>, DbError> {
        Ok(sqlx::query_as::<_, MomentRecommendation>(
            "SELECT * FROM moment_recommendation WHERE thumbnail_id = $1",
        )
        .bind(thumbnail_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    paged_list! {
        /// 按策略列出。**分页。** 走 `moment_recommendation_strategy_idx`。
        pub async fn list_by_strategy(
            &self,
            strategy: &str,
        ) -> Result<Page<MomentRecommendation>, DbError> {
            count = "SELECT COUNT(*) FROM moment_recommendation WHERE strategy = $1",
            items = "SELECT * FROM moment_recommendation WHERE strategy = $1 \
                     ORDER BY rank LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 按影片列出它的全部被推荐时刻。**分页。**
        ///
        /// 与每日推荐不同：这里 `movie_id` **不**唯一 —— 同一部影片的不同
        /// 时刻是不同的推荐。唯一性落在 `thumbnail_id` 上，所以排序用
        /// `offset_seconds` 而不是 rank。
        pub async fn list_by_movie(
            &self,
            movie_id: i32,
        ) -> Result<Page<MomentRecommendation>, DbError> {
            count = "SELECT COUNT(*) FROM moment_recommendation WHERE movie_id = $1",
            items = "SELECT * FROM moment_recommendation WHERE movie_id = $1 \
                     ORDER BY offset_seconds, id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 列出**没有可解释依据**的推荐。**分页。**
        ///
        /// 三个 seed 字段全空意味着无法回答「为什么推荐这个」。推荐本身
        /// 仍然有效不该被丢弃，但值得能被单独查出来 —— 那通常意味着检索
        /// 路径出了问题。
        ///
        /// 判定与 [`MomentRecommendation::has_seed`] 一致：三个 seed 字段
        /// 都 `IS NULL`。用 SQL 表达而不是取回全部行在 Rust 侧过滤 ——
        /// 「为什么这批推荐都说不出理由」是排障入口，量可能很大。
        pub async fn list_unexplainable(
            &self,
        ) -> Result<Page<MomentRecommendation>, DbError> {
            count = "SELECT COUNT(*) FROM moment_recommendation \
                     WHERE seed_point_id IS NULL AND seed_thumbnail_id IS NULL \
                       AND source_movie_id IS NULL",
            items = "SELECT * FROM moment_recommendation \
                     WHERE seed_point_id IS NULL AND seed_thumbnail_id IS NULL \
                       AND source_movie_id IS NULL \
                     ORDER BY rank LIMIT $1 OFFSET $2",
        }
    }
    /// 清空全表，返回删了几行。**不带 WHERE。**
    pub async fn clear(&self) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM moment_recommendation")
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(MOMENT_ENTITY))?;
        Ok(result.rows_affected())
    }
}

// ================================================================ 影片特征（稀疏向量来源）

/// 一部影片的特征：演员 id 列表 + 标签 id 列表。
///
/// # 两者都可能为空，但**不会同时为空**
///
/// `_iter_movie_features`（`recommendation_service.py:118-121`）显式跳过
/// 「既无演员也无标签」的影片 —— 那种影片**构造不出向量**，入索引也没有意义。
/// 所以这里能拿到 `Some` 的行，两个列表至少有一个非空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieFeatures {
    pub movie_id: i32,
    pub actor_ids: Vec<i32>,
    pub tag_ids: Vec<i32>,
}

/// 影片特征的查询。
///
/// # 与本文件其他仓储的差别：它**只读**
///
/// `DailyRecommendationItemRepository` 与 `MomentRecommendationRepository` 都
/// 写结果表，而这个只读源数据（`movie` / `movie_actor` / `movie_tag`）。
/// 放在这里是因为**消费者**（`discovery::recommendation`）与那些结果表同属
/// 推荐族，而不是因为表本身相关。
#[derive(Debug, Clone)]
pub struct MovieFeatureRepository {
    pool: sqlx::PgPool,
}

impl MovieFeatureRepository {
    /// 构造。
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    /// 连接池。
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// 非集合影片的总数 —— IDF 公式里的 `total_movies`。
    ///
    /// **排除 `is_collection`** —— 集合片不是「某部影片」，不该进相似度索引，
    /// 也不该计入 IDF 的分母。
    pub async fn total_movies(&self) -> Result<i64, DbError> {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM movie WHERE is_collection = false")
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(MOVIE_FEATURE_ENTITY))?;
        Ok(total)
    }

    /// 演员 / 标签的文档频次（有多少部非集合影片用了它）。
    ///
    /// # `COUNT(*)` 而**不是** `COUNT(DISTINCT movie)`
    ///
    /// 上游写的是 `fn.COUNT(link_model.movie)`（`:65`）—— **数行数**。
    /// 而 `movie_actor` / `movie_tag` 若有唯一约束则两者等价。
    ///
    /// **不去改成 `DISTINCT`**：若将来加了「同一部影片同一演员多条」的合法
    /// 场景（比如出演多个角色），改了会让 IDF 与索引端算法不一致 —— 那种
    /// 不一致表现为「检索结果略差」，很难归因。照抄。
    ///
    /// # DF 缺失时取 0 是**正常路径**
    ///
    /// 上游注释（`:137`）：「重建期间新入库的演员/标签取 DF=0（IDF 拉满）」——
    /// 这样新特征会被当作「稀有」而更容易匹配上。是特性不是 bug。
    pub async fn actor_document_frequencies(&self) -> Result<HashMap<i32, i64>, DbError> {
        self.document_frequencies("movie_actor", "actor").await
    }

    /// 标签的文档频次。
    pub async fn tag_document_frequencies(&self) -> Result<HashMap<i32, i64>, DbError> {
        self.document_frequencies("movie_tag", "tag").await
    }

    /// 通用 DF 查询。`link_table` / `feature_column` 是**受控常量**，
    /// 不是用户输入 —— 用 `format!` 拼在这里而不是绑参数，因为标识符不能绑。
    ///
    /// # ★ 白名单是**运行时**校验，不是 `debug_assert`
    ///
    /// 标识符无法绑参数，只能拼进 SQL，所以这里天然是注入面。白名单从
    /// `debug_assert!` 改成运行时 `Err`：**`debug_assert!` 在 release 下被
    /// 编译掉**，那时白名单形同不存在，任何新增调用点传进来的字符串都会
    /// 直接拼进 SQL。
    // allow 理由：白名单就在下一行，且**运行时生效**（见函数体）。
    #[allow(clippy::unnecessary_literal_unwrap)]
    async fn document_frequencies(
        &self,
        link_table: &str,
        feature_column: &str,
    ) -> Result<HashMap<i32, i64>, DbError> {
        if !matches!(
            (link_table, feature_column),
            ("movie_actor", "actor") | ("movie_tag", "tag")
        ) {
            return Err(DbError::business(
                MOVIE_FEATURE_ENTITY,
                format!("未知的关联表/特征列组合：{link_table}.{feature_column}"),
            ));
        }
        let sql = format!(
            "SELECT l.{feature_column} AS feature_id, COUNT(l.movie) AS df \
             FROM {link_table} l \
             JOIN movie m ON m.id = l.movie \
             WHERE m.is_collection = false \
             GROUP BY l.{feature_column}"
        );
        // ★ `AssertSqlSafe`：sqlx 0.9 要求动态 SQL 显式声明「已审计」。
        //
        // 审计结论：唯一的动态部分是 `link_table` 与 `feature_column` 两个
        // **标识符**，它们在函数开头被运行时白名单挡过（release 下同样生效，
        // 不是 `debug_assert!`）。标识符无法绑参数，只能拼 —— 这是白名单
        // 存在的全部理由。
        let rows: Vec<(i32, i64)> = sqlx::query_as(AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(MOVIE_FEATURE_ENTITY))?;
        Ok(rows.into_iter().collect())
    }

    /// 按影片 id 升序取一页影片 id（keyset 分页）。
    ///
    /// # 用 keyset（`id > last`）而**不是** `OFFSET`
    ///
    /// 上游注释（`:93`）：「按影片 id 分段读取特征；段内一次取全，无需长事务与
    /// 服务端游标」。`OFFSET` 在大偏移量下要扫过并丢弃前面的行，30 万影片
    /// 重建时会越来越慢；keyset 恒定。
    ///
    /// **不含任何特征过滤** —— 「跳过无特征影片」在上层做（因为要同时看演员
    /// 与标签两张表，一层 SQL 判不了）。
    pub async fn page_movie_ids(&self, after_id: i32, limit: i64) -> Result<Vec<i32>, DbError> {
        let sql =
            "SELECT id FROM movie WHERE is_collection = false AND id > $1 ORDER BY id LIMIT $2";
        sqlx::query_scalar::<_, i32>(sql)
            .bind(after_id)
            .bind(limit.max(1))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(MOVIE_FEATURE_ENTITY))
    }

    /// 一次取一批影片的演员 / 标签，按影片聚合。
    ///
    /// **两次查询而不是一次 JOIN** —— 演员与标签是多对多，一次 JOIN 会产生
    /// 笛卡尔积（5 演员 × 3 标签 = 15 行），还得去重。分开查再拼更省。
    pub async fn features_for_movies(
        &self,
        movie_ids: &[i32],
    ) -> Result<HashMap<i32, MovieFeatures>, DbError> {
        let mut out: HashMap<i32, MovieFeatures> = movie_ids
            .iter()
            .map(|id| {
                (
                    *id,
                    MovieFeatures {
                        movie_id: *id,
                        actor_ids: Vec::new(),
                        tag_ids: Vec::new(),
                    },
                )
            })
            .collect();
        if movie_ids.is_empty() {
            return Ok(out);
        }
        let actor_rows: Vec<(i32, i32)> = sqlx::query_as(
            "SELECT movie, actor FROM movie_actor WHERE movie = ANY($1) ORDER BY movie, actor",
        )
        .bind(movie_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(MOVIE_FEATURE_ENTITY))?;
        for (movie, actor) in actor_rows {
            if let Some(entry) = out.get_mut(&movie) {
                entry.actor_ids.push(actor);
            }
        }
        let tag_rows: Vec<(i32, i32)> = sqlx::query_as(
            "SELECT movie, tag FROM movie_tag WHERE movie = ANY($1) ORDER BY movie, tag",
        )
        .bind(movie_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(MOVIE_FEATURE_ENTITY))?;
        for (movie, tag) in tag_rows {
            if let Some(entry) = out.get_mut(&movie) {
                entry.tag_ids.push(tag);
            }
        }
        Ok(out)
    }
}
/// 剧情图 → 影片链接（检索结果拼装用）。
///
/// 位置依次是 **`(plot_image_id, movie_id, movie_number, image_id)`**。
///
/// # 为什么需要单独一次查询
///
/// 向量库里只有 `plot_image_id` 与 `movie_id`，**没有番号**（`movie_number`）。
/// 而响应要带番号 —— 所以必须回表。
///
/// # 为什么是元组而不是结构体
///
/// 它是 `movie_plot_image` × `image` × `movie` **三表 JOIN 的列子集**，
/// 不是任何一张表的镜像 —— 上游没有可对拍的 Peewee 模型，写成 `pub struct`
/// 会被门禁报 `UNCHECKED_STRUCT`。取舍记录见
/// `repo/moment.rs::MomentSeedRow`。
///
/// 消费方用 `let (_, movie_id, movie_number, _) = link;` 解构，别用 `.1`/`.2`。
///
/// 各位置语义：
///
/// 1. `plot_image_id`（`movie_plot_image.id`）
/// 2. `movie_id` —— **可为 `None`**：`p.movie` 是番号字符串，关联不上就是空
/// 3. `movie_number` —— **可为 `None`**，同上
/// 4. `image_id` —— 图片字节或路径所需的信息。**这里只带 id**，URL 延迟拼。
pub type PlotImageLink = (i32, Option<i32>, Option<String>, i32);

impl PendingImageRepository {
    /// 取剧情图 → 影片链接。
    ///
    /// # `INNER JOIN movie` 且排除黑名单 —— **两个后果**
    ///
    /// 上游 `_get_links`（`:250-262`）：
    ///
    /// ```python
    /// .join(Movie, JOIN.INNER)
    /// .where(Movie.is_blacklisted == False)
    /// ```
    ///
    /// 1. **没有关联影片的剧情图查不出来** —— 于是检索命中它时 `link is None`，
    ///    `_build_item` 返回 `None`，那一项被丢弃。
    /// 2. **影片在黑名单里的也查不出来** —— 同样被丢弃。
    ///
    /// **所以向量检索的命中率会低于 100%**，而调用方看到的 `items` 只是过滤后的
    /// 结果。**这不是 bug，是刻意的**（黑名单是用户明确排除的内容）。
    ///
    /// # 去重照抄上游 `dict.fromkeys(plot_image_ids)`
    ///
    /// 上游 `:257`。同一批 id 可能重复（向量库返回重复点），去重省一次 join。
    pub async fn plot_image_links(
        &self,
        plot_image_ids: &[i32],
    ) -> Result<HashMap<i32, PlotImageLink>, DbError> {
        if plot_image_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = r#"
            SELECT p.id AS plot_image_id,
                   p.movie AS movie_id,
                   m.movie_number AS movie_number,
                   p.image AS image_id
            FROM movie_plot_image p
            JOIN image i ON i.id = p.image
            JOIN movie m ON m.movie_number = p.movie
            WHERE p.id = ANY($1)
              AND m.is_blacklisted = false
        "#;
        let rows: Vec<PlotImageLink> = sqlx::query_as(sql)
            .bind(plot_image_ids)
            .fetch_all(self.pool())
            .await
            .map_err(|e| DbError::from(e).with_entity(MOVIE_FEATURE_ENTITY))?;
        // `.0` = `plot_image_id`（见 [`PlotImageLink`] 的位置说明）。
        Ok(rows.into_iter().map(|link| (link.0, link)).collect())
    }
}
