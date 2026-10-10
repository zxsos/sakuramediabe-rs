//! `tag` / `movie_tag` / `movie_actor` 仓储。
//!
//! # 这一批解锁了什么
//!
//! 之前缺这些表，导致「插 Movie + 3 条 MovieActor + upsert Tag」这个
//! 用例**无法表达** —— 三个仓储里有三个不存在，于是它只能被拆成三次
//! 各自提交的写入，中间失败会留下「Movie 有了但演员没了」的半成品。
//!
//! 现在 [`UnitOfWork`](super::UnitOfWork) 可以编排它们了。
//!
//! # 两张关联表的形状相同，行为不同
//!
//! | 表 | 唯一索引 | `replace_all` 的语义 |
//! |---|---|---|
//! | `movie_actor` | `(movie_id, actor_id)` | 「这部影片的演员就是这几位」 |
//! | `movie_tag` | `(movie_id, tag_id)` | 「这部影片的标签就是这几个」 |
//!
//! 两者的外键都是 `CASCADE`（上游 46 个外键里 30 个是 CASCADE，
//! 零个依赖数据库默认），所以删 `movie` 会自动带走这些行 —— 仓储层
//! **不提供** `delete_by_movie`，那个动作由数据库完成。
//!
//! # 标签按名字 upsert
//!
//! `tag.name` 上有唯一索引，而导入流程要处理「一部影片带 20 个标签」
//! 这种场景。先查后插在并发下会撞唯一约束，所以用
//! `ON CONFLICT (name) DO UPDATE ... RETURNING *` —— 冲突时**返回既有行**
//! 而不是报错。

use sqlx::PgPool;

use crate::catalog::asset::{MovieActor, MovieTag, Tag};
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

use super::ctx::Ctx;

const TAG_ENTITY: &str = "Tag";
const MOVIE_ACTOR_ENTITY: &str = "MovieActor";
const MOVIE_TAG_ENTITY: &str = "MovieTag";

/// `tag` 表仓储。
#[derive(Debug, Clone)]
pub struct TagRepository {
    pool: PgPool,
}

impl TagRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按名字查找。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<Tag>, DbError> {
        Ok(
            sqlx::query_as::<_, Tag>("SELECT * FROM tag WHERE name = $1")
                .bind(name.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按名字 upsert。
    ///
    /// 冲突时**不更新任何列** —— `DO UPDATE SET name = EXCLUDED.name` 写成
    /// 恒等赋值，是为了走 `RETURNING *` 拿到既有行。标签没有需要更新的
    /// 属性，重命名是另一个操作（上游也没有）。
    ///
    /// 空白名字被拒：它会占用一个唯一键，而语义上不表示任何标签。
    pub async fn upsert_by_name(&self, name: &str) -> Result<Tag, DbError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DbError::business(TAG_ENTITY, "tag name 不能为空"));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Tag>(
            "INSERT INTO tag (name, created_at, updated_at) VALUES ($1, $2, $2) \
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
             RETURNING *",
        )
        .bind(name)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(TAG_ENTITY))
    }

    /// [`Self::upsert_by_name`] 的事务内变体。见 [`Ctx`]。
    pub async fn upsert_by_name_in(&self, ctx: &mut Ctx<'_>, name: &str) -> Result<Tag, DbError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DbError::business(TAG_ENTITY, "tag name 不能为空"));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Tag>(
            "INSERT INTO tag (name, created_at, updated_at) VALUES ($1, $2, $2) \
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
             RETURNING *",
        )
        .bind(name)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(TAG_ENTITY))
    }

    paged_list! {
        /// 按名字模糊查出标签。**分页。**
        ///
        /// 走不了 `tag_name_idx`：`ILIKE '%q%'` 是前后通配，而该索引只服务
        /// 前缀匹配。标签表很小（几十行），全表扫可以接受；上游
        /// `tag_service` 走的也是这条路。
        ///
        /// `query` 传空串会列出全部标签 —— 过滤条件退化为 `ILIKE '%%'`。
        pub async fn list_by_name(&self, query: &str) -> Result<Page<Tag>, DbError> {
            count = "SELECT COUNT(*) FROM tag WHERE name ILIKE '%' || $1 || '%'",
            items = "SELECT * FROM tag WHERE name ILIKE '%' || $1 || '%' \
                     ORDER BY name LIMIT $2 OFFSET $3",
        }
    }

    /// 一组影片的标签，`(movie_id, tag_id, name)`，**按 `(movie_id, tag_id)` 升序**。
    ///
    /// 影片快照要带上标签（`MovieSnapshot.tags`），而一次 `ListMovies` 可能涉及
    /// 上千部影片 —— 逐部查 `movie_tag` 就是 N+1。形状与
    /// [`MovieActorRepository::actor_ids_for_movies`] 同构（一次 join 出目标行），
    /// 归组由调用方做。
    ///
    /// 元组而不是结构体：投影行没有上游 Peewee 模型，声明成结构体会让对拍门禁
    /// 报 `UNCHECKED_STRUCT`（同 `TagCountRow` 的理由）。
    pub async fn names_for_movies(
        &self,
        movie_ids: &[i32],
    ) -> Result<Vec<(i32, i32, String)>, DbError> {
        if movie_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<_, (i32, i32, String)>(
            "SELECT mt.movie_id, t.id, t.name FROM movie_tag mt \
             JOIN tag t ON t.id = mt.tag_id \
             WHERE mt.movie_id = ANY($1) \
             ORDER BY mt.movie_id, t.id",
        )
        .bind(movie_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 删标签。返回是否真的删掉了一行。
    ///
    /// 已被 `movie_tag` 引用的标签**删不掉** —— 唯一约束之外没有
    /// `ON DELETE CASCADE`，所以数据库会拒绝（`DbError::ConstraintViolation`）。
    /// 这是刻意的：删了标签而 `movie_tag` 还在，会留下指向不存在标签的行。
    ///
    /// 调用方需要先解绑。返回 `ConstraintViolation` 而不是静默跳过，
    /// 因为「这个标签正在被 3 部电影使用」是调用方该知道的。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM tag WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// `movie_actor` 关联表仓储。
#[derive(Debug, Clone)]
pub struct MovieActorRepository {
    pool: PgPool,
}

impl MovieActorRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    paged_list! {
        /// 列出某部影片的演员。**分页。**
        pub async fn list_by_movie(
            &self,
            movie_id: i32,
        ) -> Result<Page<MovieActor>, DbError> {
            count = "SELECT COUNT(*) FROM movie_actor WHERE movie_id = $1",
            items = "SELECT * FROM movie_actor WHERE movie_id = $1 ORDER BY id LIMIT $2 OFFSET $3",
        }
    }

    /// 某部影片的全部演员 id（**不分页**，按 `movie_actor.id` 升序）。
    ///
    /// 上游详情页的 `_actors(movie)` 是 `MovieActor.select().where(movie=...)`，
    /// **不分页** —— 一部片的演员是几位到十几位，分页只会让调用方被迫翻页。
    /// 复用上面那个分页方法去翻页也能拿到，但那要把「详情页」这个只读场景
    /// 变成多次查询；详情是一次性读全部的场景。
    pub async fn actor_ids_for_movie(&self, movie_id: i32) -> Result<Vec<i32>, DbError> {
        Ok(
            sqlx::query_scalar("SELECT actor_id FROM movie_actor WHERE movie_id = $1 ORDER BY id")
                .bind(movie_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// 一组影片的演员关联，`(movie_id, actor_id)`，**按 `(movie_id, actor_id)` 升序**。
    ///
    /// 影片快照要带上演员，而一次 `ListMovies` 可能涉及上千部影片 —— 逐部调
    /// [`Self::actor_ids_for_movie`] 就是 N+1。排序与上游一致
    /// （`context.py:122-127` 的 `.order_by(MovieActor.movie, MovieActor.actor)`），
    /// 于是同一部影片的演员顺序在两条路径下相同。
    pub async fn actor_ids_for_movies(
        &self,
        movie_ids: &[i32],
    ) -> Result<Vec<(i32, i32)>, DbError> {
        if movie_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<_, (i32, i32)>(
            "SELECT movie_id, actor_id FROM movie_actor WHERE movie_id = ANY($1) \
             ORDER BY movie_id, actor_id",
        )
        .bind(movie_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 列出某位演员出演的全部影片。**分页。**
        ///
        /// `(movie_id, actor_id)` 唯一索引的第二列在 `actor_id` 定值后
        /// 仍可定位，所以这个查询不需要额外索引。
        pub async fn list_by_actor(
            &self,
            actor_id: i32,
        ) -> Result<Page<MovieActor>, DbError> {
            count = "SELECT COUNT(*) FROM movie_actor WHERE actor_id = $1",
            items = "SELECT * FROM movie_actor WHERE actor_id = $1 \
                     ORDER BY id LIMIT $2 OFFSET $3",
        }
    }

    /// 候选集里、**关联了已订阅演员**的影片 id（去重，顺序不定）。
    ///
    /// 对应上游 `_load_subscribed_actor_movie_ids`
    /// （`daily_recommendation_service.py:171-181`）：
    ///
    /// ```python
    /// MovieActor.select(MovieActor.movie)
    ///     .join(Actor, JOIN.INNER, on=(MovieActor.actor == Actor.id))
    ///     .where(Actor.is_subscribed == True, MovieActor.movie.in_(candidate_ids))
    /// ```
    ///
    /// # `DISTINCT` 不是优化，是语义
    ///
    /// 一部影片常有多个演员，其中两个都被订阅 —— 不去重会返回同一个
    /// `movie_id` 两次。上游用 `{movie_id for (movie_id,) in rows}` 去重。
    ///
    /// # **不解析 `merged_into_id`**（与 `MovieRepository::numbers_for_actor_ids` 不同）
    ///
    /// 上游这一处没有走演员合并链，照抄：被合并演员名下的订阅影片在这里
    /// **算不进去**。改成解析合并链会让推荐结果与上游不同，而那种差异
    /// 表现为「某几部影片多/少了一点订阅演员分」，很难归因。
    pub async fn list_with_subscribed_actor_in(
        &self,
        candidate_ids: &[i32],
    ) -> Result<Vec<i32>, DbError> {
        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT ma.movie_id FROM movie_actor ma \
             JOIN actor a ON a.id = ma.actor_id \
             WHERE a.is_subscribed = true AND ma.movie_id = ANY($1)",
        )
        .bind(candidate_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 关联一位演员。重复关联返回既有行。
    pub async fn link(&self, movie_id: i32, actor_id: i32) -> Result<MovieActor, DbError> {
        let mut ctx = Ctx::over_pool(&self.pool);
        self.link_in(&mut ctx, movie_id, actor_id).await
    }

    /// [`Self::link`] 的事务内变体。见 [`Ctx`]。
    pub async fn link_in(
        &self,
        ctx: &mut Ctx<'_>,
        movie_id: i32,
        actor_id: i32,
    ) -> Result<MovieActor, DbError> {
        let row = sqlx::query_as::<_, MovieActor>(
            "INSERT INTO movie_actor (movie_id, actor_id) VALUES ($1, $2) \
             ON CONFLICT (movie_id, actor_id) DO UPDATE SET movie_id = EXCLUDED.movie_id \
             RETURNING *",
        )
        .bind(movie_id)
        .bind(actor_id)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(MOVIE_ACTOR_ENTITY))?;
        Ok(row)
    }

    /// 解除关联。返回是否真的删掉了一行。
    pub async fn unlink(&self, movie_id: i32, actor_id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM movie_actor WHERE movie_id = $1 AND actor_id = $2")
            .bind(movie_id)
            .bind(actor_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 解绑某位演员的全部影片。返回删除行数。
    ///
    /// 上游 `actor_merge_service.py:95` 走的就是这条路：合并演员时
    /// 把来源演员的所有关联行清掉，再把影片指向保留的那位。
    pub async fn unlink_actor_everywhere(&self, actor_id: i32) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM movie_actor WHERE actor_id = $1")
            .bind(actor_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// 删掉某部影片的全部演员关联。返回删除行数。
    ///
    /// 外键是 `CASCADE`，所以删 `movie` 时数据库会做这件事。这个方法
    /// 存在的理由是**替换**：先清空再重新插入，中间需要一个明确的边界。
    /// 不提供 `delete_by_movie` 之外的形式，因为 CASCADE 已经覆盖了删除场景。
    pub async fn clear_movie(&self, movie_id: i32) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM movie_actor WHERE movie_id = $1")
            .bind(movie_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

/// `movie_tag` 关联表仓储。
#[derive(Debug, Clone)]
pub struct MovieTagRepository {
    pool: PgPool,
}

impl MovieTagRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    paged_list! {
        /// 列出某部影片的标签关联。**分页。**
        pub async fn list_by_movie(
            &self,
            movie_id: i32,
        ) -> Result<Page<MovieTag>, DbError> {
            count = "SELECT COUNT(*) FROM movie_tag WHERE movie_id = $1",
            items = "SELECT * FROM movie_tag WHERE movie_id = $1 ORDER BY id LIMIT $2 OFFSET $3",
        }
    }

    /// 某部影片的全部标签 `(tag_id, name)`（**不分页**，按 `tag.id` 升序）。
    ///
    /// 上游详情页：`Tag.select(Tag).join(MovieTag).where(MovieTag.movie == movie)
    /// .order_by(Tag.id)` —— 显式按 `Tag.id` 排序，不是按关联表顺序。
    ///
    /// # 为什么直接 join 出 name，而不是先取关联行再逐个查标签
    ///
    /// 详情页要的是**名字**，一条 join 就够；逐个查会变成 N+1 次查询。
    pub async fn tags_for_movie(&self, movie_id: i32) -> Result<Vec<(i32, String)>, DbError> {
        Ok(sqlx::query_as(
            "SELECT t.id, t.name FROM movie_tag mt \
             JOIN tag t ON t.id = mt.tag_id \
             WHERE mt.movie_id = $1 ORDER BY t.id",
        )
        .bind(movie_id)
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 列出某个标签关联的全部影片。**分页。**
        pub async fn list_by_tag(&self, tag_id: i32) -> Result<Page<MovieTag>, DbError> {
            count = "SELECT COUNT(*) FROM movie_tag WHERE tag_id = $1",
            items = "SELECT * FROM movie_tag WHERE tag_id = $1 ORDER BY id LIMIT $2 OFFSET $3",
        }
    }

    /// 关联一个标签。重复关联返回既有行。
    pub async fn link(&self, movie_id: i32, tag_id: i32) -> Result<MovieTag, DbError> {
        let mut ctx = Ctx::over_pool(&self.pool);
        self.link_in(&mut ctx, movie_id, tag_id).await
    }

    /// [`Self::link`] 的事务内变体。见 [`Ctx`]。
    pub async fn link_in(
        &self,
        ctx: &mut Ctx<'_>,
        movie_id: i32,
        tag_id: i32,
    ) -> Result<MovieTag, DbError> {
        let row = sqlx::query_as::<_, MovieTag>(
            "INSERT INTO movie_tag (movie_id, tag_id) VALUES ($1, $2) \
             ON CONFLICT (movie_id, tag_id) DO UPDATE SET movie_id = EXCLUDED.movie_id \
             RETURNING *",
        )
        .bind(movie_id)
        .bind(tag_id)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(MOVIE_TAG_ENTITY))?;
        Ok(row)
    }

    /// 解除关联。返回是否真的删掉了一行。
    pub async fn unlink(&self, movie_id: i32, tag_id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM movie_tag WHERE movie_id = $1 AND tag_id = $2")
            .bind(movie_id)
            .bind(tag_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 删掉某部影片的全部标签关联。返回删除行数。
    pub async fn clear_movie(&self, movie_id: i32) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM movie_tag WHERE movie_id = $1")
            .bind(movie_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 校验发生在建立连接之前，所以可以用一个**连不上的 pool** 来证明
    /// 「它根本没走到数据库」—— 如果校验缺失，这个调用会返回连接错误
    /// 而不是 `Business`。
    fn unreachable_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            // 端口 1 上不会有 PostgreSQL。连接会在 acquire_timeout 内失败。
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .expect("connect_lazy 不会真的去连")
    }

    #[tokio::test]
    async fn blank_tag_name_is_rejected_without_touching_the_database() {
        let repo = TagRepository::new(unreachable_pool());
        for blank in ["", "   ", "\t\n"] {
            let err = repo.upsert_by_name(blank).await.unwrap_err();
            match err {
                DbError::Business { entity, reason } => {
                    assert_eq!(entity, TAG_ENTITY);
                    assert!(reason.contains("tag name"), "{reason}");
                }
                other => panic!(
                    "空白标签名应在连库之前被拒，实际 {other:?} —— \
                     若这是连接错误，说明校验被移到了连接之后"
                ),
            }
        }
    }
}
