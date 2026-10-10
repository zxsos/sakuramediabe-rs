//! `ranking_item`、`image_search_session`、`image_search_index_state`
//! 三张表的仓储。
//!
//! 这三张表构成 `discovery` 域：一个完整但此前空白的子系统。三者**不互相
//! 引用**（只有 `ranking_item.movie_id` 指向 `movie`），但它们回答的是同一个
//! 问题 —— 「有哪些内容、怎么找到它」，所以放在一起。
//!
//! # 每张表都有一个「反直觉之处」，且都写在模型上
//!
//! | 表 | 反直觉之处 |
//! |---|---|
//! | `ranking_item` | 唯一索引是 `(source_key, board_key, period, rank)` —— **同一榜单内** rank 唯一，所以这张表**可以**累积历史；而同文件的 `daily_recommendation_item` 的 `rank` 是全局唯一，必须每次清空重写。两者语义相反 |
//! | `image_search_session` | `session_id` 有 UNIQUE 约束，而上游**又**建了一条 `image_search_session_session_id_idx` —— 冗余索引 |
//! | `image_search_index_state` | **单例表**：`id` 恒为 1，只有两列，**无时间戳**。写入必须 upsert，insert 第二次就会撞主键 |
//!
//! # 单例表为什么不能有 `created_at` / `updated_at`
//!
//! 全库第二张不继承 `TimestampedMixin` 的表（第一张是 `schema_migration`）。
//! 嵌入空间变更的时刻由外部记录，本表只持有**当前值** —— 它是状态，不是
//! 事件日志。加时间戳会诱使后来者把它当历史表读。
//!
//! # `indexed_space_id` 是兼容性关键，本文件最重要的一处
//!
//! SigLIP2 模型一换，嵌入维度就变，旧向量无法与新查询向量比较。客户端
//! 持有的会话里存着查询向量 —— 若空间已切换而会话仍在有效期内，检索结果
//! 会**静默出错**：不报错，只是变差。
//!
//! 所以 [`ImageSearchIndexStateRepository::session_is_usable`] 把两侧放在
//! 一起判断，而不是让每个调用方自己记得比对。那正是「检查器在无法判断时
//! 选择沉默」的同一个形状 —— 本仓库已经因此吃过几次亏。

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::discovery::image_search::{
    ImageSearchIndexState, ImageSearchSession, IMAGE_SEARCH_STATE_ID,
};
use crate::discovery::rankings::RankingItem;
use crate::error::DbError;
use crate::paged_list;

const RANKING_ENTITY: &str = "RankingItem";
const SESSION_ENTITY: &str = "ImageSearchSession";
const INDEX_STATE_ENTITY: &str = "ImageSearchIndexState";

// ================================================================ ranking_item

/// 新增一条榜单条目。
#[derive(Debug, Clone)]
pub struct NewRankingItem {
    /// 数据源标识（如 `javdb`）。
    pub source_key: String,
    /// 榜单标识。
    pub board_key: String,
    /// 周期。**空串**代表不限定周期（如总榜），不是 `None`。
    pub period: String,
    /// 名次，从 1 开始。
    pub rank: i32,
    /// 影片番号。与 `movie_id` 冗余并存。
    pub movie_number: String,
    /// 指向 `Movie.id`。**必填** —— 见类型说明。
    pub movie_id: i32,
}

impl NewRankingItem {
    fn validate(&self) -> Result<(), DbError> {
        for (name, value) in [
            ("source_key", self.source_key.as_str()),
            ("board_key", self.board_key.as_str()),
            ("movie_number", self.movie_number.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(DbError::business(
                    RANKING_ENTITY,
                    format!("{name} 不能为空"),
                ));
            }
        }
        if self.rank < 1 {
            return Err(DbError::business(
                RANKING_ENTITY,
                format!("rank 从 1 开始，收到 {}", self.rank),
            ));
        }
        Ok(())
    }

    /// 归一后的插入参数。`period` 不 trim 成 `None` —— 它是 `NOT NULL
    /// DEFAULT ''`，空串**就是**「不限定周期」的合法取值。
    fn normalized(&self) -> Result<(&str, &str, &str, i32, &str, i32), DbError> {
        self.validate()?;
        Ok((
            self.source_key.trim(),
            self.board_key.trim(),
            self.period.trim(),
            self.rank,
            self.movie_number.trim(),
            self.movie_id,
        ))
    }
}

/// `ranking_item` 表仓储。
#[derive(Debug, Clone)]
pub struct RankingItemRepository {
    pool: PgPool,
}

impl RankingItemRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 写入一条榜单条目，**按 rank 幂等**。
    ///
    /// `ON CONFLICT (source_key, board_key, period, rank) DO UPDATE` ——
    /// 同一榜单的同一名次被再次写入时更新它，而不是报错。
    ///
    /// 这是**正确**的幂等方式，因为唯一索引就是那四列：榜单的身份就是
    /// 「哪个源、哪个榜、哪个周期」，名次是它在该榜单内的位置。重抓一次
    /// 榜单，第 3 名换了影片就该覆盖第 3 名那行。
    ///
    /// 注意与 `daily_recommendation_item` 的差别：那张表的 `rank` 是**全局**
    /// 唯一，所以它必须每次清空重写。这里不需要 —— 见类型文档。
    pub async fn upsert(&self, new: &NewRankingItem) -> Result<RankingItem, DbError> {
        let (source, board, period, rank, number, movie_id) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, RankingItem>(
            "INSERT INTO ranking_item (source_key, board_key, period, rank, movie_number, \
                                      movie_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
             ON CONFLICT (source_key, board_key, period, rank) \
             DO UPDATE SET movie_number = EXCLUDED.movie_number, \
                           movie_id = EXCLUDED.movie_id, \
                           updated_at = EXCLUDED.updated_at \
             RETURNING *",
        )
        .bind(source)
        .bind(board)
        .bind(period)
        .bind(rank)
        .bind(number)
        .bind(movie_id)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(RANKING_ENTITY))
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn upsert_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        new: &NewRankingItem,
    ) -> Result<RankingItem, DbError> {
        let (source, board, period, rank, number, movie_id) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, RankingItem>(
            "INSERT INTO ranking_item (source_key, board_key, period, rank, movie_number, \
                                      movie_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
             ON CONFLICT (source_key, board_key, period, rank) \
             DO UPDATE SET movie_number = EXCLUDED.movie_number, \
                           movie_id = EXCLUDED.movie_id, \
                           updated_at = EXCLUDED.updated_at \
             RETURNING *",
        )
        .bind(source)
        .bind(board)
        .bind(period)
        .bind(rank)
        .bind(number)
        .bind(movie_id)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(RANKING_ENTITY))
    }

    /// 列出某个榜单的全部条目，按名次。**刻意不分页。**
    ///
    /// 走 `ranking_item_source_key_board_key_period_idx`，而
    /// `ORDER BY rank` 与唯一索引
    /// `(source_key, board_key, period, rank)` 的顺序一致，所以不需要额外
    /// 排序步骤。
    ///
    /// **刻意不分页**：「这个榜单长什么样」是一次性读全 —— 榜单通常是
    /// 10 到 100 条，分页只会让调用方自己写取完所有页的循环。
    pub async fn list_by_board(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
    ) -> Result<Vec<RankingItem>, DbError> {
        Ok(sqlx::query_as::<_, RankingItem>(
            "SELECT * FROM ranking_item \
             WHERE source_key = $1 AND board_key = $2 AND period = $3 \
             ORDER BY rank",
        )
        .bind(source_key.trim())
        .bind(board_key.trim())
        .bind(period.trim())
        .fetch_all(&self.pool)
        .await?)
    }

    /// 某榜单的前 N 条。N 大于榜单长度时返回全部。
    pub async fn top_by_board(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
        limit: i32,
    ) -> Result<Vec<RankingItem>, DbError> {
        let limit = limit.max(1);
        Ok(sqlx::query_as::<_, RankingItem>(
            "SELECT * FROM ranking_item \
             WHERE source_key = $1 AND board_key = $2 AND period = $3 \
             ORDER BY rank LIMIT $4",
        )
        .bind(source_key.trim())
        .bind(board_key.trim())
        .bind(period.trim())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 某条目在各榜单上的名次。**分页。**
        ///
        /// 走 `ranking_item_movie_id_fk`。排序用
        /// `(source_key, board_key, period, rank)` —— 榜单身份在前，
        /// 名次在后，于是同一榜单的多条记录必定相邻。
        pub async fn list_by_movie(
            &self,
            movie_id: i32,
        ) -> Result<Page<RankingItem>, DbError> {
            count = "SELECT COUNT(*) FROM ranking_item WHERE movie_id = $1",
            items = "SELECT * FROM ranking_item WHERE movie_id = $1 \
                     ORDER BY source_key, board_key, period, rank LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 列出某个数据源下所有榜单的身份三元组。**分页。**
        ///
        /// 走 `ranking_item_source_key_board_key_period_idx`。
        ///
        /// 返回 `(source_key, board_key, period)` 而非整行 —— 「有哪些榜单」
        /// 是导航需要的最小信息，带上 `id` 与时间戳只会让调用方多解构一次。
        /// `DISTINCT ON` 按索引顺序去重，所以不需要额外排序步骤。
        pub async fn list_boards(
            &self,
            source_key: &str,
        ) -> Result<Page<(String, String, String)>, DbError> {
            count = "SELECT COUNT(*) FROM (SELECT DISTINCT ON (board_key, period) \
                     board_key, period FROM ranking_item WHERE source_key = $1) d",
            items = "SELECT DISTINCT ON (board_key, period) board_key, period, source_key \
                     FROM ranking_item WHERE source_key = $1 \
                     ORDER BY board_key, period LIMIT $2 OFFSET $3",
        }
    }

    /// 删掉某个榜单的全部条目，返回删了几行。
    ///
    /// ⚠️ **重抓一个榜单时确实要先用它**（[`Self::delete_board_in`] 的事务内变体
    /// 同理）：上游 `_replace_scope_items`（`ranking_service.py:357-373`）就是
    /// 「删该 scope 全部条目 + 重新插入」。只逐条 `upsert` **删不掉「这次没有的
    /// 名次」** —— 榜单从 100 条缩到 80 条，第 81..100 名会永远留在库里。
    ///
    /// 代价是名次没变的条目也会被删掉重建（`created_at` 因此重置）。上游接受
    /// 这个代价，本仓照抄。
    pub async fn delete_board(
        &self,
        source_key: &str,
        board_key: &str,
        period: &str,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM ranking_item \
             WHERE source_key = $1 AND board_key = $2 AND period = $3",
        )
        .bind(source_key.trim())
        .bind(board_key.trim())
        .bind(period.trim())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(RANKING_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    ///
    /// 排行同步的「先删后插」必须两半同事务（见
    /// `sm_service::discovery::ranking::RankingSyncService::replace_scope`）
    /// —— 中间失败会留下一个空 scope。
    pub async fn delete_board_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        source_key: &str,
        board_key: &str,
        period: &str,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM ranking_item \
             WHERE source_key = $1 AND board_key = $2 AND period = $3",
        )
        .bind(source_key.trim())
        .bind(board_key.trim())
        .bind(period.trim())
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(RANKING_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 某个榜单下**已经有条目的**周期列表（升序）。
    ///
    /// 给排行同步用的：上游 `_iter_sync_targets` 逐个周期调 `_scope_has_items`
    /// （`ranking_service.py:464-474`）判断「这个周期已有数据没」，再把结果喂给
    /// 插件的 `should_fetch(period, has_items)`。宿主这边一次查完整个榜更方便，
    /// 语义相同。
    ///
    /// 返回里**可能出现空串** —— 单期榜（总榜）的周期就是空串。
    pub async fn distinct_periods(
        &self,
        source_key: &str,
        board_key: &str,
    ) -> Result<Vec<String>, DbError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT period FROM ranking_item \
             WHERE source_key = $1 AND board_key = $2 ORDER BY period",
        )
        .bind(source_key.trim())
        .bind(board_key.trim())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(period,)| period).collect())
    }

    /// 候选影片在各榜单上的名次行 `(movie_id, rank, period)`。**刻意不限榜单。**
    ///
    /// 对应上游 `_load_ranking_scores`（`daily_recommendation_service.py:219-234`）：
    ///
    /// ```python
    /// rows = RankingItem.select(RankingItem.movie, RankingItem.rank, RankingItem.period)
    ///     .where(RankingItem.movie.in_(candidate_ids))
    /// ```
    ///
    /// # 为什么**不跨源去重**、也不在 SQL 里聚合
    ///
    /// 同一部影片可能同时挂在几个源 / 几个榜上。上游把这些行**全取回来**，
    /// 在 Python 侧按 `(rank, period)` 衰减后取**每个周期的最大值**再比。
    ///
    /// 聚合条件（周期权重表 + `RANK_DECAY_WINDOW`）在服务层，SQL 里再写一份
    /// 就成了两处真相 —— 而它们的分歧表现为「推荐次序略有出入」，
    /// 归因成本远高于多读几行。
    ///
    /// **返回顺序不排序**：服务层的 `ranking_scores` 取最大值，与行序无关。
    /// 加 `ORDER BY` 只会让这个查询多一次排序步骤。
    pub async fn list_rank_rows_for_movies(
        &self,
        movie_ids: &[i32],
    ) -> Result<Vec<(i32, i32, String)>, DbError> {
        if movie_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as(
            "SELECT movie_id, rank, period FROM ranking_item WHERE movie_id = ANY($1)",
        )
        .bind(movie_ids)
        .fetch_all(&self.pool)
        .await?)
    }
}

// ================================================================ image_search_session

/// 新建一次图搜会话。
#[derive(Debug, Clone)]
pub struct NewImageSearchSession {
    /// 对外暴露的会话 id，唯一。
    pub session_id: String,
    /// 每页条数。缺省 20（与 DDL 的 DEFAULT 一致）。
    pub page_size: i32,
    /// SigLIP2 查询向量，JSON 文本。
    pub query_vector: Option<String>,
    /// 相似度阈值。为空表示不设下限。
    pub score_threshold: Option<f64>,
    /// 过期时刻。**必填** —— `expires_at` 是 `NOT NULL`，没有 DEFAULT。
    pub expires_at: chrono::NaiveDateTime,
}

impl NewImageSearchSession {
    fn validate(&self) -> Result<(), DbError> {
        if self.session_id.trim().is_empty() {
            return Err(DbError::business(SESSION_ENTITY, "session_id 不能为空"));
        }
        if self.page_size < 1 {
            return Err(DbError::business(
                SESSION_ENTITY,
                format!("page_size 至少为 1，收到 {}", self.page_size),
            ));
        }
        Ok(())
    }
}

/// `image_search_session` 表仓储。
#[derive(Debug, Clone)]
pub struct ImageSearchSessionRepository {
    pool: PgPool,
}

impl ImageSearchSessionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 新建会话。状态取 DDL 的 DEFAULT `ready`。
    pub async fn create(&self, new: &NewImageSearchSession) -> Result<ImageSearchSession, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, ImageSearchSession>(
            "INSERT INTO image_search_session (session_id, page_size, query_vector, \
                                              score_threshold, expires_at, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING *",
        )
        .bind(new.session_id.trim())
        .bind(new.page_size)
        .bind(new.query_vector.as_deref())
        .bind(new.score_threshold)
        .bind(new.expires_at)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))
    }

    /// 按 `session_id` 查询。走 UNIQUE 约束。
    pub async fn find_by_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ImageSearchSession>, DbError> {
        Ok(sqlx::query_as::<_, ImageSearchSession>(
            "SELECT * FROM image_search_session WHERE session_id = $1",
        )
        .bind(session_id.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 推进分页：写入下一批命中的 id 与游标。
    ///
    /// `next_cursor` 传 `None` 表示**没有下一页了** —— 而 `movie_ids` 仍要
    /// 写，因为最后一批结果本身是有值的。
    ///
    /// 返回是否真的更新了。会话不存在时返回 `Ok(false)` 而不是错误 ——
    /// 清理任务可能刚把它删掉，调用方不该为此中断整个扫描。
    pub async fn save_page(
        &self,
        session_id: &str,
        movie_ids: &[i32],
        next_cursor: Option<&str>,
    ) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        let ids_json = serde_json::to_string(movie_ids).map_err(|e| {
            DbError::business(SESSION_ENTITY, format!("序列化 movie_ids 失败: {e}"))
        })?;
        let result = sqlx::query(
            "UPDATE image_search_session \
             SET movie_ids = $2, next_cursor = $3, updated_at = $4 WHERE session_id = $1",
        )
        .bind(session_id.trim())
        .bind(&ids_json)
        .bind(next_cursor)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 设置要排除的影片 id（翻页时避免重复命中）。
    pub async fn set_exclusions(
        &self,
        session_id: &str,
        exclude_movie_ids: &[i32],
    ) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        let json = serde_json::to_string(exclude_movie_ids)
            .map_err(|e| DbError::business(SESSION_ENTITY, format!("序列化失败: {e}")))?;
        let result = sqlx::query(
            "UPDATE image_search_session SET exclude_movie_ids = $2, updated_at = $3 \
             WHERE session_id = $1",
        )
        .bind(session_id.trim())
        .bind(&json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    paged_list! {
        /// 列出已过期的会话。**分页。**
        ///
        /// 走 `image_search_session_expires_at_idx` —— 那条索引**存在**
        /// 就是为了清理任务。
        pub async fn list_expired(
            &self,
            now: chrono::NaiveDateTime,
        ) -> Result<Page<ImageSearchSession>, DbError> {
            count = "SELECT COUNT(*) FROM image_search_session WHERE expires_at <= $1",
            items = "SELECT * FROM image_search_session WHERE expires_at <= $1 \
                     ORDER BY expires_at, id LIMIT $2 OFFSET $3",
        }
    }

    /// 删掉所有过期会话，返回删了几行。清理任务的实际动作。
    pub async fn delete_expired(&self, now: chrono::NaiveDateTime) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM image_search_session WHERE expires_at <= $1")
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 按 id 删除一个会话。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM image_search_session WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
}
// ================================================================ image_search_index_state（单例）

/// `image_search_index_state` 表仓储：**单例**。
///
/// 全表一行，`id` 恒为 [`IMAGE_SEARCH_STATE_ID`]。所以没有 `insert` ——
/// 只有 `set_indexed_space`（upsert）与 `get`。
#[derive(Debug, Clone)]
pub struct ImageSearchIndexStateRepository {
    pool: PgPool,
}

impl ImageSearchIndexStateRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 读当前嵌入空间。**行不存在时返回 `None`。**
    ///
    /// 不返回默认值：没有「默认嵌入空间」这回事 —— 猜一个空间 id 会让
    /// 检索**静默**用错维度，那正是本表要防的事。
    pub async fn get(&self) -> Result<Option<ImageSearchIndexState>, DbError> {
        Ok(sqlx::query_as::<_, ImageSearchIndexState>(
            "SELECT * FROM image_search_index_state WHERE id = $1",
        )
        .bind(IMAGE_SEARCH_STATE_ID)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 写入当前嵌入空间。**upsert**，因为这是单例表。
    ///
    /// `ON CONFLICT (id) DO UPDATE` —— 第二次调用更新那一行，第三次还是。
    /// 用 `insert` 的话第二次就会撞主键，而「模型换了要更新空间 id」恰恰
    /// 是这张表最频繁的写操作。
    ///
    /// 空白 `space_id` 按业务错误拒绝：把空间 id 写成空串，等于让所有会话
    /// 的兼容性判断失去依据，而那正是静默错误的入口。
    pub async fn set_indexed_space(
        &self,
        space_id: &str,
    ) -> Result<ImageSearchIndexState, DbError> {
        let space_id = space_id.trim();
        if space_id.is_empty() {
            return Err(DbError::business(
                INDEX_STATE_ENTITY,
                "indexed_space_id 不能为空：空值会让会话兼容性判断失去依据",
            ));
        }
        sqlx::query_as::<_, ImageSearchIndexState>(
            "INSERT INTO image_search_index_state (id, indexed_space_id) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET indexed_space_id = EXCLUDED.indexed_space_id \
             RETURNING *",
        )
        .bind(IMAGE_SEARCH_STATE_ID)
        .bind(space_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(INDEX_STATE_ENTITY))
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn set_indexed_space_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        space_id: &str,
    ) -> Result<ImageSearchIndexState, DbError> {
        let space_id = space_id.trim();
        if space_id.is_empty() {
            return Err(DbError::business(
                INDEX_STATE_ENTITY,
                "indexed_space_id 不能为空：空值会让会话兼容性判断失去依据",
            ));
        }
        sqlx::query_as::<_, ImageSearchIndexState>(
            "INSERT INTO image_search_index_state (id, indexed_space_id) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET indexed_space_id = EXCLUDED.indexed_space_id \
             RETURNING *",
        )
        .bind(IMAGE_SEARCH_STATE_ID)
        .bind(space_id)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(INDEX_STATE_ENTITY))
    }

    /// **这个会话还能用吗？** —— 本文件最重要的一处。
    ///
    /// 三个条件全过才返回 `true`：
    ///
    /// 1. 会话未过期（`expires_at` 有索引，所以这个判断是廉价的）
    /// 2. 单例行存在 —— **不存在即不可用**
    /// 3. 查询向量维度与 `expected_dim` 相符
    ///
    /// # 为什么第 2 条是「不存在即不可用」
    ///
    /// 状态行缺失是一个**部署顺序**问题：先建会话、后写状态行（或者迁移
    /// 漏了这一行）。此时候会话的向量与当前索引的向量无法比较，而比较
    /// 不了就只能返回错误结果 —— 不报错，只是变差。
    ///
    /// 所以这里选「拒绝」而不是「放行」。放行的代价是静默的错误检索，
    /// 拒绝的代价是一次可解释的 409。
    ///
    /// # `expected_dim` 为 `None` 时不阻断
    ///
    /// 维度信息缺失（会话没存向量，或状态行没带维度）时**不**判false ——
    /// 那是「无法判断」，不是「不兼容」。模型的
    /// [`ImageSearchIndexState::accepts_session`] 也是同样的立场：真正的
    /// 「无法确认」不该被当成「不通过」。
    pub async fn session_is_usable(
        &self,
        session: &ImageSearchSession,
        expected_dim: Option<usize>,
    ) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        if session.is_expired(now) {
            return Ok(false);
        }
        if !session.is_ready() {
            return Ok(false);
        }
        let Some(state) = self.get().await? else {
            return Ok(false);
        };
        Ok(state.accepts_session(session.query_vector_dim(), expected_dim))
    }
}

// ================================================================ 热播女优新作

/// 「恰好只有一位女优」的影片 id —— 上游 `_history_actor_evidence`
/// （`hot_actress_release_service.py:53-66`）的 `single_female_history_ids`。
///
/// # 这条 SQL 是整个服务里最不能简化的一处
///
/// 上游用 peewee 写的：
///
/// ```python
/// .group_by(MovieActor.movie)
/// .having(fn.SUM(Case(None, [(Actor.gender == 1, 1)], 0)) == 1)
/// ```
///
/// 语义是「**这部影片总共只有一位女优**」——
/// `SUM(女优数) == 1` 而不是「至少有一位」。所以：
///
/// - 一部有 2 位女优的影片**不算**
/// - 一部有 1 位女优 + 3 位男优的影片**算**
///
/// # 为什么不写成子查询
///
/// 这段被 `history_rows` 复用（`IN (...)`），所以必须是独立的一步 —— 但它是
/// **同一份 SQL 里的子查询**，不是两次往返。
///
/// # `is_collection` / `is_blacklisted` 两个排除项
///
/// 集合片（`is_collection`）与黑名单影片不参与 —— 前者不是「某位女优的作品」，
/// 后者是用户明确排除的。
///
/// # `gender = 1` 是**硬编码**在 SQL 里的
///
/// 上游是 `FEMALE_GENDER = 1` 类常量。SQL 里没法绑常量（要用 `$1` 会多一个
/// 参数并让执行计划缓存变差），所以写死并在注释里标明来源。
const HOT_ACTRESS_ENTITY: &str = "HotActressRelease";

/// 女性性别值。对应上游 `FEMALE_GENDER = 1`（`:38`）。
pub const FEMALE_GENDER: i32 = 1;

/// 历史窗口里「只有一位女优」的影片，以及它们的女优与热度。
///
/// 一行 = 一部影片 × 它的**那位**女优（因为筛选过了，每部只有一行）。
///
/// 位置依次是 **`(movie_id, actor_id, heat, release_date)`**。
///
/// # 为什么是元组而不是结构体
///
/// 它是 `movie_actor` × `movie` × `actor` **三表 JOIN 的投影**（还带一个
/// `HAVING SUM(...) = 1` 的 CTE），不是任何一张表的镜像 —— 上游没有可对拍的
/// Peewee 模型。写成 `pub struct` 会被 `parity/compare_schema.py` 报
/// `UNCHECKED_STRUCT`，而豁免它是削弱那道门禁。取舍记录见
/// `repo/moment.rs::MomentSeedRow`。
///
/// 消费方用 `let (movie_id, actor_id, heat, release_date) = *row;` 解构，
/// 别用 `.0`/`.1` —— 前两位同类型，位置写错是静默的。
///
/// 各位置语义：
///
/// 1. `movie_id` 2. `actor_id`
/// 3. `heat` —— 影片热度。**库里可能为 NULL**（上游写 `float(heat or 0)`）
/// 4. `release_date` —— 发行日，**已 `CAST(... AS date)` 掉时分秒**（列本身是
///    `timestamp`，见 `movie.release_date` 的 DDL）。查询里没有这个 CAST 时
///    `NaiveDate` 会解码失败 —— `mismatched types ... DATE vs TIMESTAMP`
///    （2026-10-09 真库第一次执行时抓到的）。
pub type HistoryActorRow = (i32, i32, Option<i32>, chrono::NaiveDate);

/// 候选窗口里带女优的影片。
///
/// 一行 = 一部影片 × 它的**每一位**女优（候选窗口不做「只有一位」筛选）。
///
/// 位置依次是 **`(movie_id, actor_id, release_date)`**。
///
/// 元组而非结构体的理由同 [`HistoryActorRow`]（三表 JOIN 的投影）。
/// 前两位都是 `i32`，消费方一律解构取名。
pub type CandidateRow = (i32, i32, chrono::NaiveDate);

/// 热播女优新作的查询。
///
/// **刻意不叫 `DiscoveryRepository`** —— 它查的是 `movie` / `movie_actor` /
/// `actor` 三张表，不是 `discovery` 域自己的表。放在
/// `repo/discovery.rs` 是因为**服务层**属��� discovery 域，��表归属不同。
#[derive(Debug, Clone)]
pub struct HotActressReleaseRepository {
    pool: PgPool,
}

impl HotActressReleaseRepository {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 历史证据行：窗口 `[today - lookback_days, today - maturity_days)`。
    ///
    /// # `maturity_days` 是**下界**，不是「只算成熟的」
    ///
    /// 上游：`history_end = today - timedelta(days=cls.HISTORY_MATURITY_DAYS)`。
    /// 也就是说，**最近 60 天发行的片子不进历史证据** —— 它们「太新了，
    /// 还没有表现可评」。这与打分公式里的 `max(age_days, MATURITY_DAYS)`
    /// 是**两处不同的机制**，别合并成一个。
    ///
    /// # 两次查询、同一快照
    ///
    /// 先筛「只有一位女优」的影片 id，再取这些影片的行。用
    /// `in_snapshot_tx` 让两步看到**同一个快照** —— 否则并发写入时
    /// 「筛出的 id 集合」与「取到的行」可能不匹配。
    pub async fn history_actor_rows(
        &self,
        history_start: chrono::NaiveDate,
        history_end: chrono::NaiveDate,
    ) -> Result<Vec<HistoryActorRow>, DbError> {
        // ⚠️ `movie_actor` 的列是 `movie_id` / `actor_id`（`docker/schema.sql`
        // 的 DDL，也是仓储其它 20 处写入用的名字）。此处曾写成 `ma.movie` /
        // `ma.actor` —— 那是上游 Peewee 的**外键字段名**，SQL 列名不是它。这条
        // 查询在 2026-10-09 之前**从未被真库执行过**（`hot-actress-releases`
        // 的端点当时还是骨架），所以一直没暴露。
        let sql = r#"
           WITH single_female_movies AS (
               SELECT ma.movie_id AS movie_id
               FROM movie_actor ma
               JOIN movie m ON m.id = ma.movie_id
               JOIN actor a ON a.id = ma.actor_id
               WHERE m.is_collection = false
                 AND m.is_blacklisted = false
                 AND m.release_date >= $1
                 AND m.release_date <  $2
               GROUP BY ma.movie_id
               HAVING SUM(CASE WHEN a.gender = 1 THEN 1 ELSE 0 END) = 1
           )
           SELECT ma.movie_id AS movie_id,
                  ma.actor_id AS actor_id,
                  m.heat AS heat,
                  CAST(m.release_date AS date) AS release_date
           FROM movie_actor ma
           JOIN movie m ON m.id = ma.movie_id
           JOIN actor a ON a.id = ma.actor_id
           JOIN single_female_movies sfm ON sfm.movie_id = ma.movie_id
           WHERE a.gender = 1
           ORDER BY ma.movie_id, ma.actor_id
       "#;
        let rows = crate::common::page::in_snapshot_tx(&self.pool, |conn| {
            Box::pin(async move {
                sqlx::query_as::<_, HistoryActorRow>(sql)
                    .bind(history_start)
                    .bind(history_end)
                    .fetch_all(&mut *conn)
                    .await
                    // 闭包签名要求 `DbError`，而 `fetch_all` 给的是 `sqlx::Error`。
                    .map_err(DbError::from)
            })
        })
        .await?;
        Ok(rows)
    }

    /// 候选行：窗口 `[today - past_days, today + future_days)`。
    ///
    /// **候选窗口不做「只有一位女优」筛选** —— 那只用于历史证据。候选是
    /// 「窗口内带女优的新片」，一位或多位都要（打分时会挑得分最高的那位）。
    pub async fn candidate_rows(
        &self,
        candidate_start: chrono::NaiveDate,
        candidate_end: chrono::NaiveDate,
    ) -> Result<Vec<CandidateRow>, DbError> {
        // 列名同 `history_actor_rows` 上面的说明（`ma.movie_id` / `ma.actor_id`）。
        let sql = r#"
           SELECT ma.movie_id AS movie_id,
                  ma.actor_id AS actor_id,
                  CAST(m.release_date AS date) AS release_date
           FROM movie_actor ma
           JOIN movie m ON m.id = ma.movie_id
           JOIN actor a ON a.id = ma.actor_id
           WHERE m.is_collection = false
             AND m.is_blacklisted = false
             AND m.release_date >= $1
             AND m.release_date <  $2
             AND a.gender = 1
           ORDER BY m.id, ma.actor_id
       "#;
        let rows = crate::common::page::in_snapshot_tx(&self.pool, |conn| {
            Box::pin(async move {
                sqlx::query_as::<_, CandidateRow>(sql)
                    .bind(candidate_start)
                    .bind(candidate_end)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(DbError::from)
            })
        })
        .await?;
        Ok(rows)
    }
}
// ================================================================ 索引完成的判定

impl ImageSearchIndexStateRepository {
    /// 是否存在**已成功索引**的记录（缩略图或剧情图任一）。
    ///
    /// 用途：区分「从没索引过」与「索引过但空间变了」。上游
    /// `_has_completed_index_records`（`image_search_index_space_service.py:101-115`）。
    ///
    /// # 为什么这个查询是**状态机的一部分**，而不是可选的优化
    ///
    /// `image_search_index_state` 是**单例表**，可能还没有行。没有行时无法区分
    /// 「从没索引过」（该走 `uninitialized`）与「索引过但记录掉了行」
    /// （该走 `rebuild_required`）。**判据只能落在缩略图 / 剧情图的索引状态上。**
    ///
    /// 删掉这个查询的后果是：换 embedding 模型后，如果状态行恰好丢了，
    /// 系统会当成「没索引过」而从零开始 —— 而 Qdrant 里还留着旧空间的向量。
    /// 新查询会去比维度，**大概率被 `accepts_session` 拦住**（这是本仓库
    /// 已经钉住的不变量），但错误信息会变成「会话不兼容」而不是「需重建」，
    /// 指向错误的方向。
    ///
    /// # 用 `EXISTS` 而非 `COUNT(*)`
    ///
    /// 只需要「有没有」。`EXISTS` 命中第一行就停，而 `COUNT(*)` 要扫完 ——
    /// 索引完成的记录可能有几十万行。
    ///
    /// # 两张表的 `SUCCESS` 都是 2，但**值域不同**
    ///
    /// | 表 | 模块 | 值域 |
    /// |---|---|---|
    /// | `media_thumbnail` | `playback::media::image_search_index_status` | 0/1/2/3（含 `SKIPPED`）|
    /// | `movie_plot_image` | `catalog::asset::image_search_index_status` | 0/1/2（**无 SKIPPED**）|
    ///
    /// 「非 JAV 媒体的缩略图不参与检索」所以缩略图能有 `SKIPPED`，剧情图不能。
    /// `SUCCESS = 2` 相同，所以这里的 SQL 两边写同一个字面量。
    pub async fn has_completed_index_records(&self) -> Result<bool, DbError> {
        let sql = r#"
            SELECT EXISTS (
                SELECT 1 FROM media_thumbnail
                WHERE image_search_index_status = 2
            )
            OR EXISTS (
                SELECT 1 FROM movie_plot_image
                WHERE image_search_index_status = 2
            )
        "#;
        let found: bool = sqlx::query_scalar(sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(INDEX_STATE_ENTITY))?;
        Ok(found)
    }
}
// ================================================================ 待索引图片

impl ImageSearchSessionRepository {
    /// **删除全部会话**（重建索引时用）。
    ///
    /// # 与 `delete_expired` 的区别是**故意的**
    ///
    /// 重建时上游 `ImageSearchSession.delete().execute()`（`image_search_index_service.py:213`）
    /// —— **不按过期时间过滤，全删**。而 `delete_expired` 是日常清理。
    ///
    /// # 为什么必须全删
    ///
    /// 会话里存着**查询向量**。换 embedding 模型后维度变了，老会话的向量
    /// 无法与新索引里的向量比较 —— `accepts_session` 会拦住它们。
    ///
    /// **但那只在维度也变时有效。** 如果换了模型而维度恰好相同（例如两个
    /// 512 维模型），`accepts_session` **会放行**，于是老会话拿着旧空间的
    /// 向量去比新空间的索引 —— **返回语义完全无关的结果，且不报错**。
    ///
    /// 所以重建时不能依赖 `accepts_session` 兜底，**必须物理删掉**。
    pub async fn delete_all(&self) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM image_search_session")
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))?;
        Ok(result.rows_affected())
    }
}

/// 待索引的缩略图。
///
/// 位置依次是 **`(thumbnail_id, media_id, movie_id, movie_number,
/// offset_seconds, image_bytes)`**。
///
/// 元组而非结构体的理由同 [`HistoryActorRow`]：它是 `media_thumbnail` ×
/// `media` × `movie` × `image` 的 JOIN 投影。
///
/// 消费方（`sm-service` 的 `index_thumbnail_batch`）用
/// `let (thumbnail_id, media_id, movie_id, _movie_number, offset_seconds,
/// _image_bytes) = item;` 一次解构完，别在循环里散用 `.0`/`.4`。
///
/// 各位置语义：
///
/// 1. `thumbnail_id` 2. `media_id`
/// 3. `movie_id` —— 归属影片。**可能为 `None`**（候选查询只取
///    `Media.movie IS NOT NULL`，所以走这条路径的行一定有值）
/// 4. `movie_number`
/// 5. `offset_seconds` —— 该帧在视频里的秒偏移（`media_thumbnail."offset"`）。
///    **写向量库要用**：`ThumbnailVectorRecord.offset_seconds` 来自这里
///    （上游 `thumbnail.offset`，`:336`）。检索结果要把它回给客户端定位帧，
///    所以不能填 0 —— 那会让所有缩略图都指向第 0 秒。
/// 6. `image_bytes` —— 图片字节（`image.data`）。**推理客户端直接吃这个**。
pub type PendingThumbnail = (i32, i32, Option<i32>, Option<String>, Option<i32>, Vec<u8>);

/// 待索引的剧情图。
///
/// 位置依次是 **`(plot_image_id, movie_id, image_bytes)`**。
///
/// 元组而非结构体的理由同 [`HistoryActorRow`]（`movie_plot_image` × `image`
/// 的 JOIN 投影）。
pub type PendingPlotImage = (i32, Option<i32>, Vec<u8>);

/// 待索引图片的查询与状态回写。
///
/// # 缩略图与剧情图的候选条件**不对称**，这是上游的现状
///
/// | | 过滤条件 |
/// |---|---|
/// | 缩略图 | `status = PENDING` **且** `media.movie IS NOT NULL`（只覆盖归属 JAV 影片的）|
/// | 剧情图 | 只看 `status = PENDING`，**无额外过滤** |
///
/// 理由在上游注释里：「图像检索只覆盖归属 JAV 影片的缩略图」。非 JAV 媒体
/// （有 Movie 关联的那些）不参与图搜。
///
/// **重置时也不对称**（`image_search_index_service.py:214-224`）：缩略图重置
/// 带 `Media.movie IS NOT NULL` 过滤，剧情图**全表**重置。照抄。
#[derive(Debug, Clone)]
pub struct PendingImageRepository {
    pool: PgPool,
}

impl PendingImageRepository {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 一批待索引缩略图。
    ///
    /// **无 `ORDER BY`** —— 上游也没有。后果是分页顺序不保证稳定，靠
    /// `status` 从 PENDING 翻到终态来推进。**不要**自己加 `ORDER BY id`：
    /// 那会让「先到先处理」变成「按 id 顺序」，在失败重试时行为不同。
    pub async fn pending_thumbnails(&self, limit: i64) -> Result<Vec<PendingThumbnail>, DbError> {
        let sql = r#"
            SELECT t.id AS thumbnail_id,
                   t.media AS media_id,
                   m.movie AS movie_number,
                   m.id AS movie_id,
                   t."offset" AS offset_seconds,
                   i.data AS image_bytes
            FROM media_thumbnail t
            JOIN image i ON i.id = t.image
            JOIN media m ON m.id = t.media
            JOIN movie mv ON mv.movie_number = m.movie
            WHERE t.image_search_index_status = 0
              AND m.movie IS NOT NULL
            LIMIT $1
        "#;
        sqlx::query_as::<_, PendingThumbnail>(sql)
            .bind(limit.max(1))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))
    }

    /// 一批待索引剧情图。
    pub async fn pending_plot_images(&self, limit: i64) -> Result<Vec<PendingPlotImage>, DbError> {
        let sql = r#"
            SELECT p.id AS plot_image_id,
                   p.movie AS movie_id,
                   i.data AS image_bytes
            FROM movie_plot_image p
            JOIN image i ON i.id = p.image
            WHERE p.image_search_index_status = 0
            LIMIT $1
        "#;
        sqlx::query_as::<_, PendingPlotImage>(sql)
            .bind(limit.max(1))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))
    }

    /// 待处理总数（缩略图 + 剧情图）。
    ///
    /// 用两个 `COUNT(*)` 相加而不是一次 `UNION ALL` —— 上游就是两次 count
    /// 相加（`:262-266`）。**单条 `UNION ALL` 会更省往返**，但会让两个计数
    /// 不在同一快照里；这里照抄，边界那一行的差异不影响进度显示。
    pub async fn pending_count(&self) -> Result<i64, DbError> {
        let sql = r#"
            SELECT (
                SELECT COUNT(*) FROM media_thumbnail t
                JOIN media m ON m.id = t.media
                WHERE t.image_search_index_status = 0 AND m.movie IS NOT NULL
            ) + (
                SELECT COUNT(*) FROM movie_plot_image p
                WHERE p.image_search_index_status = 0
            ) AS total
        "#;
        let total: i64 = sqlx::query_scalar(sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))?;
        Ok(total)
    }

    /// 按索引状态统计**缩略图行**数（状态页的 `indexing` 摘要）。
    ///
    /// 上游 `StatusService._indexing_status`（`status_service.py:576-590`）：
    /// 两次 `MediaThumbnail.select().where(状态 == …).count()`，口径就是
    /// `media_thumbnail` 的**全部行** —— 不 join、不过滤 `movie`。
    ///
    /// # ⚠️ 别与 [`Self::pending_count`] 互换
    ///
    /// 那个是**索引任务的候选口径**（并了 `movie_plot_image`，还加了
    /// `m.movie IS NOT NULL`），回答的是「任务还要处理多少」；本方法是
    /// **状态展示口径**，回答的是「库里有多少行停在某个状态」。两者不相等是
    /// 正常的，把它们对齐反而会让状态页的数字不再反映任务队列。
    ///
    /// 状态取值见 [`crate::playback::media::image_search_index_status`]
    /// （0 待处理 / 1 失败 / 2 成功 / 3 跳过）。**不校验入参**：这是一次纯读，
    /// 传了未知状态只是数出 0，为此把状态页变成 500 不值得。
    pub async fn count_thumbnails_with_status(&self, status: i32) -> Result<i64, DbError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM media_thumbnail WHERE image_search_index_status = $1",
        )
        .bind(status)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))?;
        Ok(count)
    }
}
impl PendingImageRepository {
    /// 把缩略图的索引状态写成终态。
    ///
    /// # 为什么**必须**回写状态，而不是跳过失败的
    ///
    /// 上游 `_commit_statuses`（`:462`）/ `_set_status`（`:480`）。若失败不
    /// 标记，那行仍是 `PENDING`，下一轮批处理会**又取到它** —— 无限重试同一条
    /// 坏数据（编码失败的图片、维度不符的图），任务永远跑不完。
    ///
    /// 「宁可标记失败也不假装成功」是这里的核心。
    pub async fn set_thumbnail_status(
        &self,
        thumbnail_id: i32,
        status: i32,
    ) -> Result<u64, DbError> {
        // 0=PENDING 1=FAILED 2=SUCCESS 3=SKIPPED，见
        // `sm_db::playback::media::image_search_index_status`。
        if !crate::playback::media::image_search_index_status::is_valid(status) {
            return Err(DbError::business(
                HOT_ACTRESS_ENTITY,
                format!("未知的 image_search_index_status: {status}"),
            ));
        }
        let result =
            sqlx::query("UPDATE media_thumbnail SET image_search_index_status = $2 WHERE id = $1")
                .bind(thumbnail_id)
                .bind(status)
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 把剧情图的索引状态写成终态。
    ///
    /// 剧情图的**值域没有 `SKIPPED`**（`sm_db::catalog::asset::
    /// image_search_index_status` 只有 0/1/2），所以用**那套**校验而不是缩略图
    /// 那套 —— 用错会让「跳过」这个状态被写进剧情图，而列约束可能不接受。
    pub async fn set_plot_image_status(
        &self,
        plot_image_id: i32,
        status: i32,
    ) -> Result<u64, DbError> {
        if !crate::catalog::asset::image_search_index_status::is_valid(status) {
            return Err(DbError::business(
                HOT_ACTRESS_ENTITY,
                format!("未知的 image_search_index_status: {status}"),
            ));
        }
        let result =
            sqlx::query("UPDATE movie_plot_image SET image_search_index_status = $2 WHERE id = $1")
                .bind(plot_image_id)
                .bind(status)
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 重建时把缩略图状态**全部重置为 PENDING**。
    ///
    /// **带 `media.movie IS NOT NULL` 过滤** —— 只重置归属 JAV 影片的那些，
    /// 与候选查询的条件一致。剧情图是**全表**重置（`reset_all_plot_images`）。
    ///
    /// 两者不对称是上游现状（`:214-224`），照抄。
    pub async fn reset_all_thumbnails(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            r#"
            UPDATE media_thumbnail SET image_search_index_status = 0
            WHERE id IN (
                SELECT t.id FROM media_thumbnail t
                JOIN media m ON m.id = t.media
                WHERE m.movie IS NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 重建时把剧情图状态**全部重置为 PENDING** —— **无过滤**。
    pub async fn reset_all_plot_images(&self) -> Result<u64, DbError> {
        let result = sqlx::query("UPDATE movie_plot_image SET image_search_index_status = 0")
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(HOT_ACTRESS_ENTITY))?;
        Ok(result.rows_affected())
    }
}
impl ImageSearchSessionRepository {
    /// 写入会话的**两个**过滤字段。
    ///
    /// # 为什么需要这个方法（而不是复用 `set_exclusions`）
    ///
    /// `set_exclusions`（`:406`）只写 **`exclude_movie_ids`**，而
    /// `movie_ids`（包含范围）**没有任何写入方法** —— 但模型上有这一列，
    /// 上游的 `create` 也是两个一起写的。
    ///
    /// 所以图搜与剧情图搜建会话时都需要这个方法。**已有的 `set_exclusions`
    /// 保留**（它是只改排除条件的窄操作，语义清晰）。
    ///
    /// # `None` 与「写成空数组」不同
    ///
    /// - `None` -> 写 SQL `NULL` -> **不过滤**（服务层的 `normalize_ids` 把空
    ///   列表也归一成 `None`，所以两者等价）
    /// - `Some(&[])` -> 写 `[]` -> `parse_id_list` 读回来又归一成 `None`
    ///
    /// **两条路都通向「不过滤」**，与上游的 `_normalize_ids` 语义一致。
    /// 统一在服务层归一，这里不重复判断。
    pub async fn set_filters(
        &self,
        session_id: &str,
        movie_ids: Option<&[i32]>,
        exclude_movie_ids: Option<&[i32]>,
    ) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        let encode = |ids: Option<&[i32]>| -> Result<Option<String>, DbError> {
            match ids {
                None => Ok(None),
                // 上游存的是 JSON 数组文本（模型字段是 `Option<String>`）。
                Some(list) => serde_json::to_string(list).map(Some).map_err(|e| {
                    DbError::business(SESSION_ENTITY, format!("序列化过滤条件失败: {e}"))
                }),
            }
        };
        let movie_json = encode(movie_ids)?;
        let exclude_json = encode(exclude_movie_ids)?;
        let result = sqlx::query(
            "UPDATE image_search_session \
             SET movie_ids = $2, exclude_movie_ids = $3, updated_at = $4 \
             WHERE session_id = $1",
        )
        .bind(session_id.trim())
        .bind(movie_json)
        .bind(exclude_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(SESSION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
}
