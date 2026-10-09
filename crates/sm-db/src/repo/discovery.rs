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
    /// 「重抓一个榜单」的正确做法是 `upsert` 逐条写入 —— 名次没变的条目
    /// 保留 `created_at`，历史因此可追。这个方法给的是「这个榜单作废了」
    /// 那种情况。
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
