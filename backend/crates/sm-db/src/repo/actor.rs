//! `actor` 表仓储。
//!
//! # 这一批解锁了什么
//!
//! `actor` 是与 `Movie` **完全对称**的主数据：同样有受保护字段白名单、
//! 同样有 `field_owners` / `mutation_revision`、同样有字段主权网关
//! （[`super::ActorOwnershipGateway`]）。上游那两个文件
//! （`actor_ownership_gateway.py` / `actor_merge_service.py`）现在都有对应物。
//!
//! # 墓碑指针与「不可删」
//!
//! `merged_into_id` 是墓碑指针，指向合并后的保留记录。演员**不被删除**
//! —— 合并是把来源行标记为 `merged_into_id = target` 并清掉订阅状态，
//! 而不是 `DELETE`。
//!
//! 所以本仓储**不提供** `delete`，只提供 [`ActorRepository::mark_merged`]
//! 与 [`ActorRepository::redirect_tombstones`]。
//!
//! # 墓碑链必须收敛到同一终点
//!
//! 上游合并时除了给来源行打墓碑，还会**把指向来源的墓碑一并重指向**：
//!
//! ```sql
//! UPDATE actor SET merged_into_id = %s WHERE merged_into_id IN (sources)
//! ```
//!
//! 没有这一步，A→B、B→C 合并后 A 仍指向 B，而 B 已是墓碑，
//! 解析链要多走一跳；更糟的是若 B 的行被清理，A 就成了**悬空指针**。
//! 一次合并把链压成一层，深度始终为 1。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Map as JsonMap, Value as Json};
use sqlx::postgres::PgArguments;
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{FromRow, PgConnection, PgPool, Postgres};

use crate::catalog::actor::{years_before, Actor};
use crate::common::page::{in_snapshot_tx, Page, PageRequest};
use crate::common::time::now_utc;
use crate::error::DbError;
use crate::paged_list;

use super::ctx::Ctx;
use super::movie::safe_sql;

const ENTITY: &str = "Actor";

/// 新建一位演员。
#[derive(Debug, Clone)]
pub struct NewActor {
    /// JavDB ID。**必填**。
    ///
    /// 类型是 `String` 而非 `Option<String>`，因为上游是
    /// `CaseSensitiveCharField(max_length=64, unique=True, index=True)` ——
    /// 没有 `null=True`，Peewee 的 Field 默认 `null=False`，
    /// 所以这一列是 `NOT NULL`。
    ///
    /// 此前这里是 `Option<String>`，空白被归一为 `None` 后绑进 INSERT，
    /// 必然违反 NOT NULL 约束。这个缺陷能活下来是因为对拍的可空性检查
    /// 从未真正生效（契约把「省略 `null=`」记成「解析不出来」，
    /// 而判定条件是 `py_nullable is False`），
    /// 同时集成测试在没有数据库时会静默跳过。
    ///
    /// 空白字符串按**业务错误**拒绝，而不是静默变 NULL：调用方没打算
    /// 给这个演员编号却传了空串，这是调用方的 bug，应该被告知。
    pub javdb_id: String,
    pub name: String,
}

impl NewActor {
    fn validate(&self) -> Result<(), DbError> {
        if self.name.trim().is_empty() {
            return Err(DbError::business(ENTITY, "name 不能为空"));
        }
        if self.javdb_id.trim().is_empty() {
            return Err(DbError::business(
                ENTITY,
                "javdb_id 不能为空：该列是 NOT NULL，写入空值会被数据库拒绝",
            ));
        }
        Ok(())
    }

    /// 归一后的插入参数。
    fn normalized(&self) -> Result<(&str, &str), DbError> {
        self.validate()?;
        // 保留 trim：唯一索引是大小写敏感的，而 JavDB 侧的 id 常被无意
        // 前后带空格。trim 后再存，可以让「同一部影片」的不同写入路径
        // 落到同一行上。
        Ok((self.name.trim(), self.javdb_id.trim()))
    }
}

/// `actor` 表仓储。
#[derive(Debug, Clone)]
pub struct ActorRepository {
    pool: PgPool,
}

impl ActorRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<Actor>, DbError> {
        Ok(
            sqlx::query_as::<_, Actor>("SELECT * FROM actor WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按主键查询，未命中返回 [`DbError::NotFound`]。
    ///
    /// 与 [`Self::find_by_id`] 的区别是错误类型：合并流程里「演员不存在」
    /// 是要报给调用方的 404，而不是让它去解 `Option`。
    pub async fn require_by_id(&self, id: i32) -> Result<Actor, DbError> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 按 `javdb_id` 查询。
    ///
    /// 这是演员与外部数据源之间的**唯一**稳定标识 —— `name` 会被合并
    /// 改写（`merge_alias_name` 把来源的名字并进 `alias_name`），
    /// 所以不能按名字定位外部记录。
    pub async fn find_by_javdb_id(&self, javdb_id: &str) -> Result<Option<Actor>, DbError> {
        Ok(
            sqlx::query_as::<_, Actor>("SELECT * FROM actor WHERE javdb_id = $1")
                .bind(javdb_id.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按 `id` 游标取一页，**升序**，且**排除墓碑**。
    ///
    /// 插件能力出口的 `ListActors` 用它（`crates/sm-server/src/plugin_host.rs`）。
    /// 排除墓碑与上游一致（`context.py:76` 的 `Actor.merged_into.is_null()`）：被合并
    /// 掉的演员不该再出现在「待补全」名单里 —— 它的资料已经并到保留记录上了，再抓一
    /// 遍只会把结果写到隧道记录上。
    ///
    /// `limit` 语义同 `MovieRepository::list_page_after_id`（`repo/movie.rs`）：调用方
    /// 多要一条来判「还有下一页」。
    pub async fn list_page_after_id(
        &self,
        after_id: i32,
        limit: i64,
    ) -> Result<Vec<Actor>, DbError> {
        Ok(sqlx::query_as::<_, Actor>(
            "SELECT * FROM actor WHERE id > $1 AND merged_into_id IS NULL ORDER BY id LIMIT $2",
        )
        .bind(after_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按一组主键批量取。**返回顺序不保证**，调用方自己按 `id` 排。
    ///
    /// 影片快照要带上每部影片的演员（`MovieSnapshot.actors`），而一次 `ListMovies`
    /// 可能涉及上千部影片 —— 逐部调 [`Self::find_by_id`] 就是 N+1。
    pub async fn find_by_ids(&self, ids: &[i32]) -> Result<Vec<Actor>, DbError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(
            sqlx::query_as::<_, Actor>("SELECT * FROM actor WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// 按名字或别名查。
    ///
    /// 同时匹配 `name` 与 `alias_name` —— 用户搜「苍井空」应该命中
    /// 主名是「空」但别名里有「苍井空」的那一条。这是**模糊**查询，
    /// 走不了索引，所以分页的 `total` 可能是全表扫出来的。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<Actor>, DbError> {
        let pattern = format!("%{}%", name.trim());
        Ok(sqlx::query_as::<_, Actor>(
            "SELECT * FROM actor WHERE name ILIKE $1 OR alias_name ILIKE $1 \
             ORDER BY id LIMIT 1",
        )
        .bind(&pattern)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 沿墓碑指针解析到最终保留记录。
    ///
    /// 用 `FOR UPDATE` 拿行锁：合并流程会同时改指针与订阅状态，
    /// 读到一半的链会让合并把订阅搬到即将成为墓碑的行上。
    ///
    /// 返回 `(最终记录, 途经的 id 列表)` —— 后者让调用方知道该把
    /// 指向中间节点的墓碑一并重指向终点。
    pub async fn resolve_canonical(
        &self,
        start_id: i32,
    ) -> Result<Option<(Actor, Vec<i32>)>, DbError> {
        let mut current_id = start_id;
        let mut chain = Vec::new();
        // 上限 8：链在正常维护下深度为 1（合并会压平），超过就说明
        // 有人在绕过合并流程直接写指针。
        for _ in 0..8 {
            let row = sqlx::query_as::<_, Actor>("SELECT * FROM actor WHERE id = $1 FOR UPDATE")
                .bind(current_id)
                .fetch_optional(&self.pool)
                .await?;
            let Some(actor) = row else {
                return if chain.is_empty() {
                    Ok(None)
                } else {
                    // 链中途断了：返回已知的最后一跳，不 panic。
                    Ok(None)
                };
            };
            match actor.merged_into_id {
                Some(next) if next != current_id => {
                    chain.push(actor.id);
                    current_id = next;
                }
                _ => return Ok(Some((actor, chain))),
            }
        }
        // 8 跳仍未终止：成环或链过长。返回 None 让调用方按「未解析」处理，
        // 绝不能返回一个任意的记录 —— 那会把资料写到错误的演员上。
        Ok(None)
    }

    /// 插入。
    pub async fn insert(&self, new: &NewActor) -> Result<Actor, DbError> {
        let mut ctx = Ctx::over_pool(&self.pool);
        self.insert_in(&mut ctx, new).await
    }

    /// [`Self::insert`] 的事务内变体。见 [`Ctx`]。
    pub async fn insert_in(&self, ctx: &mut Ctx<'_>, new: &NewActor) -> Result<Actor, DbError> {
        let (name, javdb_id) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Actor>(
            "INSERT INTO actor (javdb_id, name, alias_name, created_at, updated_at) \
             VALUES ($1, $2, $2, $3, $3) RETURNING *",
        )
        .bind(javdb_id)
        .bind(name)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    paged_list! {
        /// 列出演员。**分页。**
        ///
        /// 默认**排除墓碑**（`merged_into_id IS NOT NULL`）：它们已不是
        /// 独立实体，混进列表会让同一演算出多行。
        /// 传 `include_merged = true` 才能看到全部。
        pub async fn list(
            &self,
            include_merged: bool,
        ) -> Result<Page<Actor>, DbError> {
            count = "SELECT COUNT(*) FROM actor \
                     WHERE $1 OR merged_into_id IS NULL",
            items = "SELECT * FROM actor \
                     WHERE $1 OR merged_into_id IS NULL \
                     ORDER BY id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 按名字或别名搜索。**分页。**
        pub async fn search(
            &self,
            query: &str,
        ) -> Result<Page<Actor>, DbError> {
            count = "SELECT COUNT(*) FROM actor \
                     WHERE name ILIKE '%' || $1 || '%' OR alias_name ILIKE '%' || $1 || '%'",
            items = "SELECT * FROM actor \
                     WHERE name ILIKE '%' || $1 || '%' OR alias_name ILIKE '%' || $1 || '%' \
                     ORDER BY id LIMIT $2 OFFSET $3",
        }
    }

    /// 把来源演员标记为墓碑。
    ///
    /// 清掉 `is_subscribed` / `subscribed_at` 是刻意的：合并后**只有**
    /// 保留记录持有订阅状态，墓碑再带一份会让订阅列表出现重复，
    /// 而两个 `subscribed_at` 不一致时「取最早」的合并规则会在
    /// 下一轮合并里把时间戳往回改。
    pub async fn mark_merged(&self, ids: &[i32], target_id: i32) -> Result<u64, DbError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE actor SET is_subscribed = FALSE, subscribed_at = NULL, \
                 merged_into_id = $1, updated_at = $2 \
             WHERE id = ANY($3) AND merged_into_id IS NULL",
        )
        .bind(target_id)
        .bind(crate::common::time::now_utc())
        .bind(ids)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 把指向 `source_ids` 的墓碑一并重指向 `target_id`。
    ///
    /// 这是链能被压平的原因。见模块文档。
    pub async fn redirect_tombstones(
        &self,
        source_ids: &[i32],
        target_id: i32,
    ) -> Result<u64, DbError> {
        if source_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE actor SET merged_into_id = $1, updated_at = $2 \
             WHERE merged_into_id = ANY($3)",
        )
        .bind(target_id)
        .bind(crate::common::time::now_utc())
        .bind(source_ids)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 记录一次登录无关的更新（推进 `updated_at`）。
    pub async fn touch(&self, id: i32) -> Result<Actor, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Actor>("UPDATE actor SET updated_at = $2 WHERE id = $1 RETURNING *")
            .bind(id)
            .bind(now)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 标记订阅。`subscribed_at` 取最早，与上游合并规则一致。
    ///
    /// 单独成方法而不是走通用 `update`：订阅状态带**时间语义**，
    /// 重复订阅不应刷新时间。
    pub async fn set_subscribed(&self, id: i32, subscribed: bool) -> Result<Actor, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Actor>(
            "UPDATE actor SET is_subscribed = $2, \
                 subscribed_at = CASE WHEN $2 THEN COALESCE(subscribed_at, $3) ELSE NULL END, \
                 updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(subscribed)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 强制下一次全量同步。
    ///
    /// 上游在合并后把 `subscribed_movies_full_synced_at` 置 NULL，
    /// 因为「同步任务会覆盖墓碑的 javdb_id」，必须重新全量才能补齐
    /// 来源 ID 的历史影片。
    pub async fn invalidate_full_sync(&self, id: i32) -> Result<Actor, DbError> {
        sqlx::query_as::<_, Actor>(
            "UPDATE actor SET subscribed_movies_full_synced_at = NULL, updated_at = $2 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(crate::common::time::now_utc())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 按 JavDB 资源更新演员那几个**非受保护**字段。`None` = 这一列不动。
    ///
    /// 上游 `upsert_actor_from_javdb_resource`（`catalog_import_service.py:880-954`）
    /// 在建记录后 `save(only=[...])` 的那几列；`gender` **不在**这里 —— 它是
    /// 受保护字段，必须走
    /// [`ActorOwnershipGateway::update_host_source`](crate::repo::ActorOwnershipGateway::update_host_source)
    /// 并带上 `host:javdb` 这个 owner。
    ///
    /// # 为什么 `profile_image_id` 也只能「不动」而不能置空
    ///
    /// 置空只能写 SQL 字面量 `NULL`（本仓纪律），而这一列什么时候该清空上游
    /// 没有说（它只在拿不到头像时**不写**）。所以这里不提供清空路径 ——
    /// 需要时再加一个显式的方法，而不是让 `None` 承担两种含义。
    pub async fn update_javdb_profile(
        &self,
        id: i32,
        name: Option<&str>,
        alias_name: Option<&str>,
        javdb_type: Option<i32>,
        profile_image_id: Option<i32>,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE actor SET \
                name = COALESCE($2, name), \
                alias_name = COALESCE($3, alias_name), \
                javdb_type = COALESCE($4, javdb_type), \
                profile_image_id = COALESCE($5, profile_image_id), \
                updated_at = $6 \
              WHERE id = $1",
        )
        .bind(id)
        .bind(name.map(str::trim))
        .bind(alias_name)
        .bind(javdb_type)
        .bind(profile_image_id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 读取订阅同步时间。供同步任务判断是否需要全量。
    pub async fn load_sync_state(&self, id: i32) -> Result<Option<SyncState>, DbError> {
        Ok(sqlx::query_as::<_, SyncState>(
            "SELECT is_subscribed, subscribed_at, subscribed_movies_synced_at, \
                    subscribed_movies_full_synced_at FROM actor WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 本次同步要处理的**已订阅演员**，按 id 升序，**整行**。
    ///
    /// 对应上游 `SubscribedActorMovieSyncService.sync_subscribed_actor_movies`
    /// 的 `Actor.select().where(is_subscribed == True, merged_into.is_null())`
    /// （`subscribed_actor_movie_sync_service.py:22-26`）—— 连 `select()` 不带
    /// 字段列表这一点都照抄：调用方要读 `javdb_id` / `javdb_type` /
    /// 两个同步时刻，投影成另一套字段只会让「上游读了哪些列」看不出来。
    ///
    /// # 为什么还排除墓碑
    ///
    /// 合并在打墓碑时**顺手清掉订阅状态**（见 [`Self::mark_merged`]），
    /// 所以墓碑本来就不会带 `is_subscribed`。这个条件防的是历史数据里
    /// 「既订阅又是墓碑」的行：它们的作品会经保留记录的 `targets` 路径同步
    /// （见 [`Self::list_merged_source_targets`]），在这里再来一遍是重复劳动。
    pub async fn list_subscribed_for_sync(&self) -> Result<Vec<Actor>, DbError> {
        Ok(sqlx::query_as::<_, Actor>(
            "SELECT * FROM actor \
              WHERE is_subscribed = TRUE AND merged_into_id IS NULL \
              ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 保留记录的**墓碑来源**：`(javdb_id, javdb_type)`，按 id 升序。
    ///
    /// 上游 `_sync_actor` 的 `merged_sources`
    /// （`subscribed_actor_movie_sync_service.py:66-72`）：合并进来的演员
    /// 自己不再被同步，作品靠保留记录**代它抓一遍** —— 否则合并会让
    /// 「来源演员的那些作品」永远不再补录。
    ///
    /// # 一处**有意**差异：空串 `javdb_id` 的墓碑被过滤掉
    ///
    /// 上游不筛，直接把 `javdb_id` 交给 provider；空 id 查不出东西，
    /// provider 抛错，于是**整位演员**被计入 `failed_actors`、两个时间戳都不
    /// 推进 —— 一个空 id 的墓碑能让这位演员每晚都失败。这里跳过它：那种墓碑
    /// 没有可查的 id，而它的作品本来就已经经保留记录同步过了。
    ///
    /// （`javdb_id` 列是 **NOT NULL**，所以只需要判空串。）
    pub async fn list_merged_source_targets(
        &self,
        actor_id: i32,
    ) -> Result<Vec<(String, i32)>, DbError> {
        Ok(sqlx::query_as::<_, (String, i32)>(
            "SELECT javdb_id, javdb_type FROM actor \
              WHERE merged_into_id = $1 AND javdb_id <> '' \
              ORDER BY id",
        )
        .bind(actor_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 记下「订阅影片已同步到此刻」。返回受影响行数。
    ///
    /// 对应上游 `_sync_actor` 的结尾（`:136-140`）：
    ///
    /// ```python
    /// actor.subscribed_movies_synced_at = synced_at          # 总是推进
    /// if mode == "full" and actor.subscribed_movies_full_synced_at is None:
    ///     actor.subscribed_movies_full_synced_at = synced_at
    /// ```
    ///
    /// 第二行那个「只在原本为 `NULL` 时写」用 `COALESCE` 表达 ——
    /// **不要**把读到的值再传回来：那样会在并发下把别人刚写上的全量时刻
    /// 覆盖成 `NULL`（读→写之间隔着整个同步过程，可能几分钟）。
    ///
    /// # 不动 `updated_at`
    ///
    /// 上游 `save(only=[...])` 只写这两列。`updated_at` 的语义是「这行被改过」，
    /// 而每天一次的同步不该让整表看起来刚被改过。
    pub async fn mark_subscribed_movies_synced(
        &self,
        id: i32,
        synced_at: NaiveDateTime,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE actor \
                SET subscribed_movies_synced_at = $2, \
                    subscribed_movies_full_synced_at = \
                        COALESCE(subscribed_movies_full_synced_at, $2) \
              WHERE id = $1",
        )
        .bind(id)
        .bind(synced_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 这位演员名下是否已经有这部影片（按**影片的 `javdb_id`** 判重）。
    ///
    /// 对应上游 `_actor_movie_exists`（`:154-163`）：增量同步靠它决定
    /// 「翻到库里已有的那部就停」。用 `javdb_id` 而不是番号：番号会被人工改，
    /// 而 `javdb_id` 是外部数据源的稳定键。
    pub async fn has_actor_movie(
        &self,
        actor_id: i32,
        movie_javdb_id: &str,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS ( \
                 SELECT 1 FROM movie_actor ma \
                   JOIN movie m ON m.id = ma.movie_id \
                  WHERE ma.actor_id = $1 AND m.javdb_id = $2 \
             )",
        )
        .bind(actor_id)
        .bind(movie_javdb_id.trim())
        .fetch_one(&self.pool)
        .await?)
    }
}

/// 订阅同步状态。只读投影 —— 用它避免把整行 `Actor` 拉出来。
#[derive(Debug, Clone, FromRow)]
pub struct SyncState {
    pub is_subscribed: bool,
    pub subscribed_at: Option<NaiveDateTime>,
    pub subscribed_movies_synced_at: Option<NaiveDateTime>,
    /// `NULL` 表示从未全量同步过，或被显式作废。
    pub subscribed_movies_full_synced_at: Option<NaiveDateTime>,
}

// ============================================================================
// 演员列表 / 详情 / 筛选
//
// 以下全部对应上游 `actor_service.py`。三条硬约束贯穿这一节：
//
// 1. **占位符编号由 [`SqlBuilder::bind`] 统一分配。** WHERE 与 ORDER BY 里
//    的占位符必须共用一个计数器 —— 分开编号就会重号，而重号**不会报错**，
//    只会把值绑到别的条件上（见 `search_filters.rs` 的 FilterBuilder 注释）。
// 2. **COUNT 与 SELECT 的 WHERE 必须同一份字符串。** 上游用
//    `_filtered_actors()` 收口就是为了这个；这里靠「只渲染一次、
//    两处复用」保证。
// 3. **`a` 是表别名，全段 SQL 都靠它。** 拼错别名同样是运行期才发现。
// ============================================================================

/// 演员影片数（相关子查询）。
///
/// 上游 `ActorService._movie_count_expression`。放在 SELECT 里而不是先聚合成
/// 一张临时表，是因为演员列表的分页只取当页 N 行 —— 相关子查询只对那 N 行
/// 求值。
const MOVIE_COUNT_SQL: &str = "(SELECT COUNT(*) FROM movie_actor ma WHERE ma.actor_id = a.id)";

/// 演员**可播放**影片数。
///
/// 上游 `_playable_movie_count_expression`。两跳：`movie_actor` → `movie`，
/// 再用 `media` 里存在 `valid` 的行来判定「可播放」。
///
/// # `media.movie_number` 不是 `media.movie_id`
///
/// `media` 指向 `movie` 的**字符串主键** `movie_number`（Peewee 的
/// `ForeignKeyField("Movie", field="movie_number")`），所以列名就叫
/// `movie_number`。照抄 Peewee 的字段关系写成 `movie_id` 会得到
/// `column md.movie_id does not exist` —— 那一列在 `media` 上不存在。
const PLAYABLE_MOVIE_COUNT_SQL: &str = "(SELECT COUNT(*) FROM movie_actor ma \
     JOIN movie m ON m.id = ma.movie_id \
     WHERE ma.actor_id = a.id AND EXISTS (SELECT 1 FROM media md \
       WHERE md.valid = TRUE AND md.movie_number = m.movie_number))";

/// 是否存在可播放影片。对应上游 `_has_playable_movie_expression`。
const HAS_PLAYABLE_MOVIE_SQL: &str = "EXISTS (SELECT 1 FROM movie_actor ma \
     JOIN movie m ON m.id = ma.movie_id \
     WHERE ma.actor_id = a.id AND EXISTS (SELECT 1 FROM media md \
       WHERE md.valid = TRUE AND md.movie_number = m.movie_number))";

/// 归一化罩杯：`NULLIF(UPPER(BTRIM(cup)), '')`。
///
/// 对应上游 `_normalized_cup_expression`。**先 BTRIM 再 UPPER** 的顺序不能
/// 反：库里可能存 `" a "`（BTRIM 后 `"a"` → UPPER → `"A"`），
/// 反过来会得到 `" A "`，而筛选值是 `A`，`IN` 立刻失配。
const NORMALIZED_CUP_SQL: &str = "NULLIF(UPPER(BTRIM(a.cup)), '')";

/// 腰臀比。
///
/// 上游 `Actor.waist_cm.cast("REAL") / fn.NULLIF(Actor.hips_cm, 0)`。
/// 分子上的 `::REAL` 是**必需的**：PostgreSQL 的 `integer / integer` 是整数
/// 除法，35/90 会算成 0 而不是 0.3889 —— 而这个表达式是**排序键**，
/// 整数除法会让所有比值都变成 0，排序退化成全表顺序，且不报任何错。
const WAIST_HIP_RATIO_SQL: &str = "(a.waist_cm::REAL / NULLIF(a.hips_cm, 0))";

/// 列表 / 详情补头像用的 FROM 子句。
///
/// 上游 `_actor_query()` 用**两个** `LEFT OUTER JOIN` 补头像
/// （`profile_image` 与别名 `profile_image_override`），生效头像由应用侧的
/// `effective_profile_image` 按「覆盖优先」折叠。这里保持两次 join，
/// 再用 `COALESCE(oi.*, pi.*)` 在 SQL 里折叠成两列 —— 与上游
/// `profile_image_override` 优先、`profile_image` 兜底完全同序。
const ACTOR_FROM_SQL: &str = " FROM actor a \
     LEFT JOIN image pi ON pi.id = a.profile_image_id \
     LEFT JOIN image oi ON oi.id = a.profile_image_override_id";

/// 折叠后的生效头像两列。
const ACTOR_IMAGE_COLUMNS_SQL: &str = "COALESCE(oi.id, pi.id) AS image_id, \
     COALESCE(oi.origin, pi.origin) AS image_origin";

/// 演员列表 / 详情的一行。
///
/// **元组，不是 struct** —— 投影行不是表镜像，在 `sm-db` 里声明成
/// `pub struct` 会被 schema 对拍当成待验证的表模型（理由见
/// `sm-service/src/catalog/resolution.rs`）。具名类型放在 `sm-service`。
///
/// 元素依次为：演员本体、影片数、生效头像 id、生效头像 origin。
pub type ActorListRow = (Actor, i64, Option<i32>, Option<String>);

/// 列表筛选条件。
///
/// 每个字段对应上游 `_filtered_actors()` 的一个参数。默认值等价于上游的
/// 默认参数（`gender=ALL` / `subscription_status=ALL` / 其余 `None`），
/// 所以「全部演员」不是一个特殊分支，而是「所有条件都不加」。
#[derive(Debug, Clone, Default)]
pub struct ActorListFilter {
    /// `1` = 女、`2` = 男。`None` = 不限。
    pub gender: Option<i32>,
    /// `Some(true)` = 只看已订阅、`Some(false)` = 只看未订阅、`None` = 不限。
    pub subscribed: Option<bool>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub height_min: Option<i32>,
    pub height_max: Option<i32>,
    /// **已归一为大写**的罩杯值。非空即生效（上游 `if cups:`）。
    pub cups: Vec<String>,
    pub has_playable_movies: bool,
    /// 已拆分的检索词。词之间 AND、词内 name/alias OR。
    pub search_terms: Vec<String>,
}

/// 排序字段。
///
/// 取值与上游 `_actor_list_sort_field_map()` 的键**逐字相同** —— 键名会
/// 直接出现在错误回显里（`{"sort": "movieCounts:asc"}`），改名等于改契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorSortKey {
    SubscribedAt,
    Name,
    MovieCount,
    PlayableMovieCount,
    /// 特殊：**按生日反方向排**。见 [`ActorSort`]。
    Age,
    HeightCm,
    BustCm,
    WaistCm,
    HipsCm,
    WaistHipRatio,
    Cup,
}

impl ActorSortKey {
    /// 全部可排序字段，**按上游字段映射的书写顺序**。
    pub const ALL: [Self; 11] = [
        Self::SubscribedAt,
        Self::Name,
        Self::MovieCount,
        Self::PlayableMovieCount,
        Self::Age,
        Self::HeightCm,
        Self::BustCm,
        Self::WaistCm,
        Self::HipsCm,
        Self::WaistHipRatio,
        Self::Cup,
    ];

    /// 按名字取字段。`None` = 非法排序字段，由 service 层转成 422。
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }

    /// 契约里的字段名。
    pub fn name(self) -> &'static str {
        match self {
            Self::SubscribedAt => "subscribed_at",
            Self::Name => "name",
            Self::MovieCount => "movie_count",
            Self::PlayableMovieCount => "playable_movie_count",
            Self::Age => "age",
            Self::HeightCm => "height_cm",
            Self::BustCm => "bust_cm",
            Self::WaistCm => "waist_cm",
            Self::HipsCm => "hips_cm",
            Self::WaistHipRatio => "waist_hip_ratio",
            Self::Cup => "cup",
        }
    }

    /// 是否要 `NULLS LAST` 垫后空值。
    ///
    /// 对应上游 `ACTOR_LIST_NULLABLE_SORT_FIELDS`。**用原生 `NULLS LAST`
    /// 而不是 `CASE WHEN col IS NULL THEN 1 ELSE 0 END` 垫后**：后者会让
    /// 排序无法被复合索引服务，退化成全表扫 + 全量排序（上游注释给了实测
    /// 数字：30 万行影片列表 200ms+ vs 命中索引约 4ms）。
    pub fn nullable(self) -> bool {
        matches!(
            self,
            Self::SubscribedAt
                | Self::Age
                | Self::HeightCm
                | Self::BustCm
                | Self::WaistCm
                | Self::HipsCm
                | Self::WaistHipRatio
                | Self::Cup
        )
    }

    /// 排序列的 SQL 片段。
    ///
    /// `age` 与 `waist_hip_ratio` 是**算出来的**（前者来自 `birthday`，
    /// 见 [`ActorSort`]；后者是除法表达式），其余是裸列。
    pub fn sql(self) -> &'static str {
        match self {
            Self::SubscribedAt => "a.subscribed_at",
            Self::Name => "a.name",
            Self::MovieCount => MOVIE_COUNT_SQL,
            Self::PlayableMovieCount => PLAYABLE_MOVIE_COUNT_SQL,
            // 年龄本身不存在于表里；实际排的是生日，方向由 [`ActorSort`] 反转。
            Self::Age => "a.birthday",
            Self::HeightCm => "a.height_cm",
            Self::BustCm => "a.bust_cm",
            Self::WaistCm => "a.waist_cm",
            Self::HipsCm => "a.hips_cm",
            Self::WaistHipRatio => WAIST_HIP_RATIO_SQL,
            Self::Cup => NORMALIZED_CUP_SQL,
        }
    }
}

/// 演员列表的排序方式。
///
/// 三种，对应上游 `_build_actor_list_sort()` 的三条出口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorSort {
    /// 无排序表达式：按 `id ASC`。
    Default,
    /// `字段:方向`。
    Field { key: ActorSortKey, descending: bool },
    /// 传了检索词且**没**传排序：按检索相关度。
    ///
    /// 上游 `_actor_search_score_expression`：每个词一套 `CASE`
    /// （完全同名 0 / 姓名前缀 1 / 姓名包含 2 / 别名包含 3），
    /// 多个词**相加**。相关性越高越靠前，所以是 `ASC`。
    SearchRelevance { terms: Vec<String> },
}

/// 一个待绑定的值。
///
/// 枚举而不是 `serde_json::Value`：值类型决定了 PG 该按什么类型解码，
/// 混进 `Value` 就得在绑定时再猜一次。
#[derive(Debug, Clone)]
enum Bind {
    Text(String),
    Int(i32),
    Date(NaiveDate),
    Timestamp(NaiveDateTime),
    Bool(bool),
    TextArray(Vec<String>),
}

/// 让 [`Bind`] 能绑到两种查询形态上。
///
/// `sqlx::query` 与 `sqlx::query_as` 各自有 `bind`，但**没有**共同的超集
/// trait 可约束 —— 而本模块两条路都要走（COUNT 用 `query_scalar`、
/// 行数据用 `query_as`）。所以在这里声明一个只有 `bind_value` 的trait，
/// 把「按值类型分派」收在一处，而不是让每个调用点写一遍 `match`。
trait BindQuery<'q>: Sized {
    fn bind_value(self, value: &Bind) -> Self;
}

impl<'q> BindQuery<'q> for Query<'q, Postgres, PgArguments> {
    fn bind_value(self, value: &Bind) -> Self {
        match value {
            Bind::Text(v) => self.bind(v.clone()),
            Bind::Int(v) => self.bind(*v),
            Bind::Date(v) => self.bind(*v),
            Bind::Timestamp(v) => self.bind(*v),
            Bind::Bool(v) => self.bind(*v),
            Bind::TextArray(v) => self.bind(v.clone()),
        }
    }
}

impl<'q, O> BindQuery<'q> for QueryAs<'q, Postgres, O, PgArguments> {
    fn bind_value(self, value: &Bind) -> Self {
        match value {
            Bind::Text(v) => self.bind(v.clone()),
            Bind::Int(v) => self.bind(*v),
            Bind::Date(v) => self.bind(*v),
            Bind::Timestamp(v) => self.bind(*v),
            Bind::Bool(v) => self.bind(*v),
            Bind::TextArray(v) => self.bind(v.clone()),
        }
    }
}

impl<'q, O> BindQuery<'q> for QueryScalar<'q, Postgres, O, PgArguments> {
    fn bind_value(self, value: &Bind) -> Self {
        match value {
            Bind::Text(v) => self.bind(v.clone()),
            Bind::Int(v) => self.bind(*v),
            Bind::Date(v) => self.bind(*v),
            Bind::Timestamp(v) => self.bind(*v),
            Bind::Bool(v) => self.bind(*v),
            Bind::TextArray(v) => self.bind(v.clone()),
        }
    }
}

/// WHERE / ORDER BY 的片段与绑定值收集器。
///
/// **占位符编号的唯一来源。** [`Self::bind`] 是唯一能拿到 `$n` 的地方，
/// 所以编号与绑定顺序不可能对不上：谁先要编号谁先占，绑定时按 `binds` 的
/// 顺序来一遍。
#[derive(Debug, Default)]
struct SqlBuilder {
    next: usize,
    clauses: Vec<String>,
    order: Vec<String>,
    binds: Vec<Bind>,
}

impl SqlBuilder {
    fn new() -> Self {
        Self::default()
    }

    /// 分配下一个占位符并登记它的值，返回 `"$n"`。
    fn bind(&mut self, bind: Bind) -> String {
        self.next += 1;
        self.binds.push(bind);
        format!("${}", self.next)
    }

    /// 追加一个恒真条件。
    fn push(&mut self, clause: impl Into<String>) {
        self.clauses.push(clause.into());
    }

    /// 追加一个 `ORDER BY` 片段。
    fn push_order(&mut self, clause: impl Into<String>) {
        self.order.push(clause.into());
    }

    /// `WHERE ...`，无条件时恒为 `"1 = 1"`。
    ///
    /// 恒真条件不是占位：它让「COUNT 与 SELECT 共用同一份 WHERE 字符串」
    /// 在无筛选时也成立 —— 否则两条 SQL 的形状会分叉，将来加条件时只改一处
    /// 就又会出现 total 与 items 不一致。
    fn where_sql(&self) -> String {
        if self.clauses.is_empty() {
            "1 = 1".to_owned()
        } else {
            self.clauses.join(" AND ")
        }
    }

    fn order_sql(&self) -> String {
        self.order.join(", ")
    }
}

impl ActorRepository {
    /// 把筛选条件渲染成 WHERE 片段 + 绑定值。
    ///
    /// 抽成独立函数的原因只有一个：COUNT 与 SELECT 必须调用**同一个**它。
    /// 上游 `_filtered_actors()` 的 docstring 就是这个意思 ——
    /// 「筛选统一收口到这里，保证 count 和 items 逻辑一致」。
    fn build_filter(filter: &ActorListFilter, today: NaiveDate, sql: &mut SqlBuilder) {
        // 墓碑不是独立实体，混进列表会让同一演算出多行。
        sql.push("a.merged_into_id IS NULL");

        if let Some(gender) = filter.gender {
            let ph = sql.bind(Bind::Int(gender));
            sql.push(format!("a.gender = {ph}"));
        }
        if let Some(subscribed) = filter.subscribed {
            let ph = sql.bind(Bind::Bool(subscribed));
            sql.push(format!("a.is_subscribed = {ph}"));
        }

        // 年龄区间用「生日 ≤ 某个日期」表达，上界靠 age_max + 1 变成开区间。
        if let Some(age_min) = filter.age_min {
            let ph = sql.bind(Bind::Date(years_before(today, age_min)));
            sql.push(format!("a.birthday <= {ph}"));
        }
        if let Some(age_max) = filter.age_max {
            let ph = sql.bind(Bind::Date(years_before(today, age_max + 1)));
            sql.push(format!("a.birthday > {ph}"));
        }
        if let Some(height_min) = filter.height_min {
            let ph = sql.bind(Bind::Int(height_min));
            sql.push(format!("a.height_cm >= {ph}"));
        }
        if let Some(height_max) = filter.height_max {
            let ph = sql.bind(Bind::Int(height_max));
            sql.push(format!("a.height_cm <= {ph}"));
        }
        if !filter.cups.is_empty() {
            // `::text[]` 显式转型：PG 无法从 `= ANY($n)` 两侧推出参数类型，
            // 会报 `could not determine data type of parameter $n`。
            let ph = sql.bind(Bind::TextArray(filter.cups.clone()));
            sql.push(format!("{NORMALIZED_CUP_SQL} = ANY({ph}::text[])"));
        }
        if filter.has_playable_movies {
            sql.push(HAS_PLAYABLE_MOVIE_SQL);
        }
        // 词之间 AND、词内 name/alias OR。
        //
        // `LIKE` 而**不是** `ILIKE`：上游 Peewee 的 `contains` / `startswith`
        // 生成的是 `LIKE`，PostgreSQL 的 `LIKE` 大小写敏感。换成 `ILIKE`
        // 会让「搜得到」的范围变大，而那是对契约的静默放宽。
        for term in &filter.search_terms {
            let ph = sql.bind(Bind::Text(format!("%{term}%")));
            // 同一个占位符用两次是合法的（PG 的参数可以重复引用），
            // 也正是上游只传一个参数的做法。
            sql.push(format!("(a.name LIKE {ph} OR a.alias_name LIKE {ph})"));
        }
    }

    /// 渲染 ORDER BY，并把它的绑定追加到 `sql`。
    ///
    /// 必须在 [`Self::build_filter`] **之后**调用：占位符编号按出现顺序分配，
    /// 反过来会让 ORDER BY 的 `$1` 与 WHERE 的 `$1` 撞号 —— 而重号不报错，
    /// 只是把检索词绑到 `LIMIT` 上这类难以定位的错误。
    fn build_order(order: &ActorSort, sql: &mut SqlBuilder) {
        match order {
            ActorSort::Default => sql.push_order("a.id ASC"),
            ActorSort::Field { key, descending } => {
                // `age` 是个例外：年龄没有列，排的是生日，所以方向要**反过来**。
                //
                // 上游 `_age_order`：age asc = 年龄从小到大 = 生日从新到旧。
                // 漏掉这个取反会让「按年龄升序」变成「按年龄降序」，而结果看着
                // 完全合理 —— 只是每一行都反了。
                let descending = if *key == ActorSortKey::Age {
                    !*descending
                } else {
                    *descending
                };
                let dir = if descending { "DESC" } else { "ASC" };
                // 次级排序与主排序**同向**，且同样带 NULLS LAST ——
                // 上游 `build_ordered_expressions` 把 tie_breaker 交给同一个
                // 构造器，所以两者的方向与空值处理必然一致。
                let nulls = if key.nullable() { " NULLS LAST" } else { "" };
                sql.push_order(format!("{} {dir}{nulls}", key.sql()));
                sql.push_order(format!("a.id {dir}{nulls}"));
            }
            ActorSort::SearchRelevance { terms } => {
                let mut score = String::new();
                for term in terms {
                    let exact = sql.bind(Bind::Text(term.to_uppercase()));
                    let prefix = sql.bind(Bind::Text(format!("{term}%")));
                    let contains = sql.bind(Bind::Text(format!("%{term}%")));
                    if !score.is_empty() {
                        score.push_str(" + ");
                    }
                    // 无 ELSE：某词一个分支都不命中时整个表达式是 NULL。
                    // WHERE 已经保证每个词都能命中 name 或 alias，所以这里恒有值；
                    // 保留无 ELSE 是为了与上游 `Case(None, [...])` 一致。
                    score.push_str(&format!(
                        "(CASE WHEN UPPER(a.name) = {exact} THEN 0 \
                           WHEN a.name LIKE {prefix} THEN 1 \
                           WHEN a.name LIKE {contains} THEN 2 \
                           WHEN a.alias_name LIKE {contains} THEN 3 END)"
                    ));
                }
                sql.push_order(format!("{score} ASC"));
                sql.push_order("a.id ASC");
            }
        }
    }

    /// 按筛选 + 排序分页列出演员。**分页。**
    ///
    /// `total` 与 `items` 在同一个 `REPEATABLE READ` 快照里算（见
    /// [`in_snapshot_tx`]），否则并发写入下 `total=100` 而本页只有 10 条，
    /// 客户端按 total 拉页会漏数据。
    ///
    /// # `limit` / `offset` 是**未校验**的原始查询参数
    ///
    /// 上游 `list_actors` **没有** `validate_page`：
    ///
    /// ```python
    /// start = max(page - 1, 0) * page_size   # page<=0 夹到 0，page_size 不管
    /// ```
    ///
    /// 所以 `page=0` 返回第一页数据并把 `page: 0` 原样回显，`page_size=200`
    /// 也合法（返回 200 条）。而 [`PageRequest`] 会把这两个都变成 422 ——
    /// 那是**收窄**契约：宽表视图是真实用法，客户端传 200 拿到 422 而不是数据。
    /// 因此这里收裸 `i64`，由调用方（service 层）复刻上游的夹取规则。
    ///
    /// 代价是 `page_size` 为负会一路传到 PG（`LIMIT` 不接受负数）→ 500。
    /// 这与上游一致：上游同样把负数交给数据库并炸在同一条语句上。
    /// **刻意不**把它改成 422 —— 那会让「上游 500」变成「我们 422」，
    /// 而这类输入没有任何合法用途。
    ///
    /// # 一次取页 = 三条 SQL，而不是一条
    ///
    /// 影片数与生效头像用两次批量查询补，而不是 `SELECT a.*, 额外列`：
    ///
    /// - `Actor` 派生的是 `FromRow`，按**列名**解码，所以 `a.*` 能用；
    /// - 但 [`ActorListRow`] 是**元组**，sqlx 的元组 `FromRow` 按**列序**
    ///   解码（`from_row.rs` 的 `impl_from_row_for_tuple!` 用的是
    ///   `(0) -> T1; (1) -> T2; …`）。`a.*` 展开 27 列再接 3 列，
    ///   元组解到第 2 个元素时拿到的是 `javdb_id`，于是运行期报类型错。
    /// - 把 27 个列名抄进 SELECT 来凑元组位数，会让「加一列」变成静默腐烂：
    ///   少写一个列名只有运行期才炸。
    ///
    /// 所以：主查询按列名取 `Actor`，再用 `= ANY($1)` 批量补两列。
    /// 一页最多 100 行，两次批量查询的代价是两次往返，与 N+1 无关。
    pub async fn list_filtered(
        &self,
        filter: &ActorListFilter,
        order: &ActorSort,
        today: NaiveDate,
        limit: i64,
        offset: i64,
    ) -> Result<Page<ActorListRow>, DbError> {
        let mut sql = SqlBuilder::new();
        Self::build_filter(filter, today, &mut sql);
        let where_clause = sql.where_sql();
        let where_binds = sql.binds.len();
        Self::build_order(order, &mut sql);
        // ORDER BY 的绑定排在 WHERE 之后，且编号连续 —— items 查询按
        // `where_binds + order_binds` 一次绑完，COUNT 查询只绑 WHERE 那几个。
        let all_binds = sql.binds.clone();
        let order_binds = sql.binds.len() - where_binds;

        let count_sql = format!("SELECT COUNT(*) FROM actor a WHERE {where_clause}");
        // 排序键全部落在 `a` 或相关子查询上，主查询**不需要 join**。
        // **`$` 不能丢。** 少了它 `LIMIT {} OFFSET {}` 会渲染成字面量
        // `LIMIT 1 OFFSET 2`：查询照跑、总数照算，只是把首页跳掉了 ——
        // `total=1` 而 `items=0`，**一条错误都不报**。
        // 这个缺陷通过了 `cargo check`、`clippy -D warnings` 与全部单测
        // （单测里 limit/offset 恰好取 0/1 时字面量与占位符等价），
        // 只有真库上断言「刚建的演员出现在列表里」才暴露。
        let items_sql = format!(
            "SELECT a.* FROM actor a WHERE {where_clause} ORDER BY {} LIMIT ${} OFFSET ${}",
            sql.order_sql(),
            sql.next + 1,
            sql.next + 2
        );

        in_snapshot_tx(&self.pool, move |conn: &mut PgConnection| {
            let count_sql = count_sql.clone();
            let items_sql = items_sql.clone();
            let binds = all_binds.clone();
            Box::pin(async move {
                let mut count_query = sqlx::query_scalar::<_, i64>(safe_sql(count_sql));
                for bind in binds.iter().take(where_binds) {
                    count_query = count_query.bind_value(bind);
                }
                let total = count_query.fetch_one(&mut *conn).await?;

                let mut items_query = sqlx::query_as::<_, Actor>(safe_sql(items_sql));
                for bind in binds.iter().take(where_binds + order_binds) {
                    items_query = items_query.bind_value(bind);
                }
                let actors = items_query
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(&mut *conn)
                    .await?;

                let page = if actors.is_empty() {
                    Page::new(Vec::new(), total)
                } else {
                    let ids: Vec<i32> = actors.iter().map(|actor| actor.id).collect();
                    let counts = Self::movie_counts_of(&mut *conn, &ids).await?;
                    let images = Self::profile_images_of(&mut *conn, &ids).await?;
                    let rows = actors
                        .into_iter()
                        .map(|actor| {
                            let movie_count = counts
                                .iter()
                                .find(|(id, _)| *id == actor.id)
                                .map_or(0, |(_, count)| *count);
                            let (image_id, image_origin) = images
                                .iter()
                                .find(|(id, _, _)| *id == actor.id)
                                .map_or((None, None), |(_, image_id, origin)| {
                                    (*image_id, origin.clone())
                                });
                            (actor, movie_count, image_id, image_origin)
                        })
                        .collect();
                    Page::new(rows, total)
                };
                // 不用 `verify_page_shape`：它要一个 `PageRequest`，而本页
                // 的 limit/offset 来自**未校验**的查询参数（见方法文档）——
                // 硬造一个 `PageRequest` 会把上游允许的 `page_size=200` 变成 422。
                // 换成直接断言「返回条数不超过 limit」。
                if page.items.len() as i64 > limit {
                    return Err(DbError::business(
                        ENTITY,
                        format!(
                            "本页返回 {} 条，超过 LIMIT {limit} —— 查询漏了 LIMIT，\
                             total 与 items 会不一致",
                            page.items.len()
                        ),
                    ));
                }
                if page.total < page.items.len() as i64 {
                    return Err(DbError::business(
                        ENTITY,
                        format!(
                            "total({}) 小于本页条数({})，COUNT 与 SELECT 的条件不一致",
                            page.total,
                            page.items.len()
                        ),
                    ));
                }
                Ok(page)
            })
                as Pin<Box<dyn Future<Output = Result<Page<ActorListRow>, DbError>> + Send>>
        })
        .await
    }

    /// 批量取影片数：`(actor_id, 影片数)`。
    ///
    /// **只在给定的 id 集合里数**（`= ANY($1)`），所以本页有多少行就算多少行
    /// —— 不做全表分组。这是与 `count_by_playlists` 同一形状的批量计数，
    /// 而不是按演员逐个查。
    async fn movie_counts_of(
        conn: &mut PgConnection,
        ids: &[i32],
    ) -> Result<Vec<(i32, i64)>, sqlx::Error> {
        sqlx::query_as::<_, (i32, i64)>(
            "SELECT ma.actor_id, COUNT(*) FROM movie_actor ma \
             WHERE ma.actor_id = ANY($1) GROUP BY ma.actor_id",
        )
        .bind(ids.to_vec())
        .fetch_all(conn)
        .await
    }

    /// 批量取生效头像：`(actor_id, 头像 id, 头像 origin)`。
    async fn profile_images_of(
        conn: &mut PgConnection,
        ids: &[i32],
    ) -> Result<Vec<(i32, Option<i32>, Option<String>)>, sqlx::Error> {
        let sql = safe_sql(format!(
            "SELECT a.id, {ACTOR_IMAGE_COLUMNS_SQL}{ACTOR_FROM_SQL} WHERE a.id = ANY($1)"
        ));
        sqlx::query_as::<_, (i32, Option<i32>, Option<String>)>(sql)
            .bind(ids.to_vec())
            .fetch_all(conn)
            .await
    }

    /// 取一位演员（含生效头像与影片数）。
    pub async fn find_with_image(&self, id: i32) -> Result<Option<ActorListRow>, DbError> {
        let Some(actor) = self.find_by_id(id).await? else {
            return Ok(None);
        };
        let mut conn = self.pool.acquire().await?;
        let counts = Self::movie_counts_of(&mut conn, &[id]).await?;
        let images = Self::profile_images_of(&mut conn, &[id]).await?;
        let movie_count = counts.first().map_or(0, |(_, count)| *count);
        let (image_id, image_origin) = images
            .first()
            .map_or((None, None), |(_, image_id, origin)| {
                (*image_id, origin.clone())
            });
        Ok(Some((actor, movie_count, image_id, image_origin)))
    }

    /// 批量取演员（含生效头像与影片数）。`hot-actress-releases` 的装配用。
    ///
    /// 「生效头像」的那条缝与 [`Self::find_with_image`] 完全同一份实现
    /// （`profile_images_of` 里 override 优先）—— 两处不能各写一遍：只取
    /// `profile_image` 会让用户设的本地头像不生效。
    ///
    /// **返回顺序不保证**（`find_by_ids` 的 `WHERE id = ANY($1)` 不带 `ORDER BY`），
    /// 调用方按 `row.0.id` 建映射，别按下标对齐。
    pub async fn find_with_images(&self, ids: &[i32]) -> Result<Vec<ActorListRow>, DbError> {
        let actors = self.find_by_ids(ids).await?;
        if actors.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await?;
        let counts: HashMap<i32, i64> = Self::movie_counts_of(&mut conn, ids)
            .await?
            .into_iter()
            .collect();
        let images: HashMap<i32, (Option<i32>, Option<String>)> =
            Self::profile_images_of(&mut conn, ids)
                .await?
                .into_iter()
                .map(|(id, image_id, origin)| (id, (image_id, origin)))
                .collect();
        Ok(actors
            .into_iter()
            .map(|actor| {
                // 没有影片的左连接侧：`COUNT(*)` 的 `GROUP BY` 里就没有这一行，
                // 所以缺省是 0 而不是「没查到」（演员本身一定在）。
                let movie_count = counts.get(&actor.id).copied().unwrap_or(0);
                let (image_id, image_origin) =
                    images.get(&actor.id).cloned().unwrap_or((None, None));
                (actor, movie_count, image_id, image_origin)
            })
            .collect())
    }

    /// 演员列表的聚合：给筛选项用。
    ///
    /// 返回 `(演员数, 有生日的人数, 最早生日, 最晚生日, 有身高的人数,
    /// 最低身高, 最高身高)`。无匹配行时前两项是 0、其余是 `None` ——
    /// 上游用 `aggregate_query.get()` 拿聚合行，PG 的无 `GROUP BY` 聚合在零行
    /// 时仍返回一行（全是 NULL / 0），所以**这里必须返回一行而不是 `None`**。
    ///
    /// **元组而非 struct**：理由同 [`ActorListRow`]。
    #[allow(clippy::type_complexity)]
    pub async fn filter_aggregate(
        &self,
        scope: &ActorScope,
    ) -> Result<
        (
            i64,
            i64,
            Option<NaiveDate>,
            Option<NaiveDate>,
            i64,
            Option<i32>,
            Option<i32>,
        ),
        DbError,
    > {
        let mut sql = SqlBuilder::new();
        Self::build_filter(
            &scope.to_filter(),
            NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
            &mut sql,
        );
        // build_filter 无条件加 `a.merged_into_id IS NULL`，而聚合筛选项也要
        // 排除墓碑（上游 `get_filter_options` 显式 append 了同一条）。
        let statement = safe_sql(format!(
            "SELECT COUNT(a.id), COUNT(a.birthday), MIN(a.birthday), MAX(a.birthday), \
                    COUNT(a.height_cm), MIN(a.height_cm), MAX(a.height_cm) \
             FROM actor a WHERE {}",
            sql.where_sql()
        ));
        let mut query = sqlx::query_as::<
            _,
            (
                i64,
                i64,
                Option<NaiveDate>,
                Option<NaiveDate>,
                i64,
                Option<i32>,
                Option<i32>,
            ),
        >(statement);
        for bind in &sql.binds {
            query = query.bind_value(bind);
        }
        Ok(query.fetch_one(&self.pool).await?)
    }

    /// 罩杯筛选项：`(值, 该值的演员数)`，按值升序。
    pub async fn cup_options(&self, scope: &ActorScope) -> Result<Vec<(String, i64)>, DbError> {
        let mut sql = SqlBuilder::new();
        Self::build_filter(
            &scope.to_filter(),
            NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
            &mut sql,
        );
        sql.push(format!("{NORMALIZED_CUP_SQL} IS NOT NULL"));
        let statement = safe_sql(format!(
            "SELECT {NORMALIZED_CUP_SQL} AS value, COUNT(a.id) AS count \
             FROM actor a WHERE {} GROUP BY {NORMALIZED_CUP_SQL} \
             ORDER BY {NORMALIZED_CUP_SQL}",
            sql.where_sql()
        ));
        let mut query = sqlx::query_as::<_, (String, i64)>(statement);
        for bind in &sql.binds {
            query = query.bind_value(bind);
        }
        Ok(query.fetch_all(&self.pool).await?)
    }

    /// 演员关联的影片 id，按 id 升序。对应 `get_actor_movie_ids`。
    pub async fn movie_ids(&self, actor_id: i32) -> Result<Vec<i32>, DbError> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT m.id FROM movie m \
             JOIN movie_actor ma ON ma.movie_id = m.id \
             WHERE ma.actor_id = $1 ORDER BY m.id",
        )
        .bind(actor_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 演员关联影片的标签，按标签名升序。对应 `get_actor_tags`。
    pub async fn actor_tags(&self, actor_id: i32) -> Result<Vec<(i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, String)>(
            "SELECT t.id, t.name FROM tag t \
             JOIN movie_tag mt ON mt.tag_id = t.id \
             JOIN movie m ON m.id = mt.movie_id \
             JOIN movie_actor ma ON ma.movie_id = m.id \
             WHERE ma.actor_id = $1 \
             GROUP BY t.id, t.name ORDER BY t.name",
        )
        .bind(actor_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 演员关联影片的年份分布，按年份降序。对应 `get_actor_years`。
    ///
    /// `release_date` 为 `timestamp`，`EXTRACT(YEAR FROM ...)` 直接取年份；
    /// 上游用 `date_part('year', ...)`（返回 `double precision`）再在 Python
    /// 里 `int()`，这里在 SQL 里转 `int` —— 值恒为整数，转换无损。
    pub async fn actor_years(&self, actor_id: i32) -> Result<Vec<(i32, i64)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, i64)>(
            "SELECT EXTRACT(YEAR FROM m.release_date)::int AS year, COUNT(m.id) AS movie_count \
             FROM movie m \
             JOIN movie_actor ma ON ma.movie_id = m.id \
             WHERE ma.actor_id = $1 AND m.release_date IS NOT NULL \
             GROUP BY EXTRACT(YEAR FROM m.release_date)::int \
             ORDER BY EXTRACT(YEAR FROM m.release_date)::int DESC",
        )
        .bind(actor_id)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// 筛选项聚合的作用域（性别 + 订阅状态）。
///
/// 上游 `get_filter_options` 只接受这两个维度 —— 聚合里**不含**年龄/身高/
/// 罩杯筛选本身（否则筛选项会自我收窄：选了罩杯 C 就没有 D 的选项了）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ActorScope {
    /// `1` = 女、`2` = 男、`None` = 不限。
    pub gender: Option<i32>,
    /// `Some(true)` = 只算已订阅。
    pub subscribed: Option<bool>,
}

impl ActorScope {
    /// 降成列表筛选。聚合与列表共用同一组条件，避免两套语义。
    fn to_filter(self) -> ActorListFilter {
        ActorListFilter {
            gender: self.gender,
            subscribed: self.subscribed,
            ..ActorListFilter::default()
        }
    }
}

// ============================================================================
// 动态 SET：资料修改与合并共用
//
// 上游两处都用「拼 `SET` 子句 + 按字段数生成占位符」：
// `actor_service.update_profile` 与 `actor_merge_service._apply_merge`。
// 这里收敛成 [`ActorUpdate`]，两个用例共用一条渲染路径 ——
// 两处各写一遍的话，某天只改一处的 `field_owners` 逻辑，
// 「人工改过的字段在合并时保留」就会只在一条路径上成立。
// ============================================================================

/// 允许被写入的列。
///
/// **列名是代码里的字面量**，调用方只能通过 [`ActorUpdate`] 的具名 setter
/// 传入，没有任何路径能把外部输入拼进 SQL —— 这是 [`safe_sql`] 的依据。
///
/// 不含 `field_owners` / `mutation_revision` / `updated_at`：它们各有专门的
/// 语义（归属合并、版本推进、时间戳），走 [`ActorUpdate`] 的专用方法。
///
/// `alias_name` 是**唯一非资料类**的白名单列：`update_profile` 的 10 个
/// 可编辑字段不含它，只有**合并**会写它（`actor_merge_service._apply_merge`
/// 的 `alias_name = %s`）。身份列（`name` / `javdb_id`）仍然不可写。
const WRITABLE_ACTOR_COLUMNS: [&str; 17] = [
    "alias_name",
    "birthday",
    "blood_type",
    "bust_cm",
    "cup",
    "display_name_override",
    "gender",
    "height_cm",
    "hips_cm",
    "is_subscribed",
    "merged_into_id",
    "birthplace",
    "profile_image_id",
    "profile_image_override_id",
    "subscribed_at",
    "subscribed_movies_full_synced_at",
    "waist_cm",
];

/// 待写入的列值。
#[derive(Debug, Clone)]
pub enum ActorSetValue {
    Text(Option<String>),
    Int(Option<i32>),
    Date(Option<NaiveDate>),
    Timestamp(Option<NaiveDateTime>),
    Bool(bool),
}

impl ActorSetValue {
    /// 该值是否要写成 SQL 的 `NULL` 字面量。
    ///
    /// # 为什么 `None` 不能绑成空串或 0
    ///
    /// `col = NULL` 在 SQL 里是 UNKNOWN，**永远不成立**；而绑一个 Rust 的
    /// `None` 过去，PG 会按该列的类型把它解成 `NULL` —— 只有在我们显式
    /// 把它转成 `""` / `0` 时才会变成「清空 = 写空串」。
    ///
    /// 这个缺陷在真库上表现为：把罩杯清空（`{"cup": null}`）之后，
    /// `actor.cup` 变成 `""` 而不是 `NULL`，于是
    /// `NULLIF(UPPER(BTRIM(cup)), '')` 照样把它归一成空串 —— 列表里看不出
    /// 异常，但「非空罩杯」的聚合会多出一个空桶。
    fn is_null(&self) -> bool {
        matches!(
            self,
            Self::Text(None) | Self::Int(None) | Self::Date(None) | Self::Timestamp(None)
        )
    }

    fn bind(&self) -> Bind {
        match self {
            Self::Text(v) => Bind::Text(v.clone().unwrap_or_default()),
            Self::Int(v) => Bind::Int(v.unwrap_or_default()),
            Self::Date(v) => {
                Bind::Date(v.unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()))
            }
            Self::Timestamp(v) => Bind::Timestamp(v.unwrap_or_default()),
            Self::Bool(v) => Bind::Bool(*v),
        }
    }
}

/// 一次 `actor` 行更新。
///
/// 只描述「写什么」，不负责「什么时候写」—— 仓储的 `apply_update`（自动提交）
/// 与 `apply_update_in`（事务内）各渲染一次。
#[derive(Debug, Clone, Default)]
pub struct ActorUpdate {
    sets: Vec<(&'static str, ActorSetValue)>,
    /// 显式置 `NULL` 的列。
    nulls: Vec<&'static str>,
    /// `field_owners = field_owners || $n::jsonb` 的载荷。
    merge_owners: Option<JsonMap<String, Json>>,
    bump_revision: bool,
    touch: bool,
}

impl ActorUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写一个文本列。`None` 写 `NULL`。
    pub fn set_text(&mut self, column: &'static str, value: Option<String>) -> &mut Self {
        self.push(column, ActorSetValue::Text(value))
    }

    /// 写一个整数列。
    pub fn set_int(&mut self, column: &'static str, value: Option<i32>) -> &mut Self {
        self.push(column, ActorSetValue::Int(value))
    }

    /// 写一个日期列（`birthday` 是 `date`，不是 `timestamp`）。
    pub fn set_date(&mut self, column: &'static str, value: Option<NaiveDate>) -> &mut Self {
        self.push(column, ActorSetValue::Date(value))
    }

    /// 写一个时间戳列。
    pub fn set_timestamp(
        &mut self,
        column: &'static str,
        value: Option<NaiveDateTime>,
    ) -> &mut Self {
        self.push(column, ActorSetValue::Timestamp(value))
    }

    /// 写一个布尔列。
    pub fn set_bool(&mut self, column: &'static str, value: bool) -> &mut Self {
        self.push(column, ActorSetValue::Bool(value))
    }

    /// 把某列置 `NULL`。
    ///
    /// 与 `set_int(column, None)` 刻意分开：清空头像要写的是
    /// `profile_image_override_id = NULL`，而 `set_int` 传 `None` 会被
    /// 渲染成 `= NULL`（**永远不成立**）—— SQL 里 `col = NULL` 是 `UNKNOWN`，
    /// 不是赋值。所以 `None` 必须落成 `NULL` 字面量。
    pub fn set_null(&mut self, column: &'static str) -> &mut Self {
        self.nulls.push(column);
        self
    }

    /// 合并字段归属：`field_owners = field_owners || $n::jsonb`。
    pub fn merge_field_owners(&mut self, owners: JsonMap<String, Json>) -> &mut Self {
        self.merge_owners = Some(owners);
        self
    }

    /// `mutation_revision = mutation_revision + 1`。
    pub fn bump_mutation_revision(&mut self) -> &mut Self {
        self.bump_revision = true;
        self
    }

    /// `updated_at = $n`。
    pub fn touch(&mut self) -> &mut Self {
        self.touch = true;
        self
    }

    /// 是否一个列都没写。
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty() && self.nulls.is_empty() && self.merge_owners.is_none()
    }

    fn push(&mut self, column: &'static str, value: ActorSetValue) -> &mut Self {
        // 同名列后写覆盖先写：合并流程可能先填资料再决定要不要搬头像。
        match self.sets.iter_mut().find(|(name, _)| *name == column) {
            Some(slot) => slot.1 = value,
            None => self.sets.push((column, value)),
        }
        self
    }

    /// 渲染成 `SET` 子句与绑定值。列名不在白名单里就拒绝。
    fn render(&self) -> Result<(String, Vec<Bind>), DbError> {
        let mut next = 0usize;
        let mut binds = Vec::new();
        let mut assignments: Vec<String> = Vec::with_capacity(self.sets.len() + 3);
        for (column, value) in &self.sets {
            if !WRITABLE_ACTOR_COLUMNS.contains(column) {
                return Err(DbError::business(
                    ENTITY,
                    format!("不允许写入的列: {column}"),
                ));
            }
            // `None` 渲染成 `NULL` 字面量，见 [`ActorSetValue::is_null`]。
            if value.is_null() {
                assignments.push(format!("{column} = NULL"));
                continue;
            }
            next += 1;
            binds.push(value.bind());
            assignments.push(format!("{column} = ${next}"));
        }
        for column in &self.nulls {
            if !WRITABLE_ACTOR_COLUMNS.contains(column) {
                return Err(DbError::business(
                    ENTITY,
                    format!("不允许写入的列: {column}"),
                ));
            }
            assignments.push(format!("{column} = NULL"));
        }
        if let Some(owners) = &self.merge_owners {
            next += 1;
            binds.push(Bind::Text(Json::Object(owners.clone()).to_string()));
            assignments.push(format!("field_owners = field_owners || ${next}::jsonb"));
        }
        if self.bump_revision {
            assignments.push("mutation_revision = mutation_revision + 1".to_owned());
        }
        if self.touch {
            next += 1;
            binds.push(Bind::Timestamp(now_utc()));
            assignments.push(format!("updated_at = ${next}"));
        }
        Ok((assignments.join(", "), binds))
    }
}

impl ActorRepository {
    /// 应用一次更新（自动提交）。返回受影响行数。
    ///
    /// 受影响行数**必须**由调用方检查：上游 `update_profile` 在
    /// `rowcount != 1` 时报 404 `actor_not_found` —— 并发删除与
    /// 「更新成功」在这里是同一件事的两种结局，只有行数能区分。
    pub async fn apply_update(&self, id: i32, update: &ActorUpdate) -> Result<u64, DbError> {
        let mut ctx = Ctx::over_pool(&self.pool);
        self.apply_update_in(&mut ctx, id, update).await
    }

    /// [`Self::apply_update`] 的事务内变体。见 [`Ctx`]。
    pub async fn apply_update_in(
        &self,
        ctx: &mut Ctx<'_>,
        id: i32,
        update: &ActorUpdate,
    ) -> Result<u64, DbError> {
        let (assignments, binds) = update.render()?;
        let sql = format!(
            "UPDATE actor SET {assignments} WHERE id = ${}",
            binds.len() + 1
        );
        let mut query = sqlx::query(safe_sql(sql));
        for bind in &binds {
            query = query.bind_value(bind);
        }
        let result = query.bind(id).execute(ctx.conn().await?.as_conn()).await?;
        Ok(result.rows_affected())
    }

    /// 取一行并 `FOR UPDATE` 加行锁。
    ///
    /// 合并流程对目标与每个来源都要上锁：两个并发合并若都读到「来源未合并」
    /// 的旧快照，双方都会把各自的订阅时间搬过来，后写的那次覆盖前一次 ——
    /// 而 `subscribed_at` 的合并规则是「取最早」，覆盖就丢了信息。
    pub async fn lock_in(&self, ctx: &mut Ctx<'_>, id: i32) -> Result<Option<Actor>, DbError> {
        sqlx::query_as::<_, Actor>("SELECT * FROM actor WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(ctx.conn().await?.as_conn())
            .await
            .map_err(DbError::from)
    }

    /// 把来源演员的影片关联改挂到目标名下。
    ///
    /// `ON CONFLICT (movie_id, actor_id) DO NOTHING` 是必需的：同一部影片
    /// 两边都有时，唯一索引 `movie_actor_movie_id_actor_id_uniq` 会让
    /// 裸 INSERT 报冲突并**回滚整个合并**。
    pub async fn move_movie_links_in(
        &self,
        ctx: &mut Ctx<'_>,
        target_id: i32,
        source_ids: &[i32],
    ) -> Result<u64, DbError> {
        if source_ids.is_empty() {
            return Ok(0);
        }
        let sql = "INSERT INTO movie_actor (movie_id, actor_id) \
             SELECT movie_id, $1 FROM movie_actor WHERE actor_id = ANY($2) \
             ON CONFLICT (movie_id, actor_id) DO NOTHING";
        let result = sqlx::query(safe_sql(sql))
            .bind(target_id)
            .bind(source_ids.to_vec())
            .execute(ctx.conn().await?.as_conn())
            .await?;
        Ok(result.rows_affected())
    }

    /// 删掉来源演员的影片关联（关联已搬到目标名下）。
    pub async fn delete_movie_links_in(
        &self,
        ctx: &mut Ctx<'_>,
        source_ids: &[i32],
    ) -> Result<u64, DbError> {
        if source_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query("DELETE FROM movie_actor WHERE actor_id = ANY($1)")
            .bind(source_ids.to_vec())
            .execute(ctx.conn().await?.as_conn())
            .await?;
        Ok(result.rows_affected())
    }

    /// 把来源行打成墓碑：清订阅 + 指向目标。
    pub async fn tombstone_sources_in(
        &self,
        ctx: &mut Ctx<'_>,
        target_id: i32,
        source_ids: &[i32],
    ) -> Result<u64, DbError> {
        if source_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE actor SET is_subscribed = FALSE, subscribed_at = NULL, \
                 merged_into_id = $1, updated_at = $2 \
             WHERE id = ANY($3) AND merged_into_id IS NULL",
        )
        .bind(target_id)
        .bind(now_utc())
        .bind(source_ids.to_vec())
        .execute(ctx.conn().await?.as_conn())
        .await?;
        Ok(result.rows_affected())
    }

    /// 把指向来源的墓碑一并重指向目标（压平墓碑链）。
    pub async fn redirect_tombstones_in(
        &self,
        ctx: &mut Ctx<'_>,
        target_id: i32,
        source_ids: &[i32],
    ) -> Result<u64, DbError> {
        if source_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE actor SET merged_into_id = $1, updated_at = $2 \
             WHERE merged_into_id = ANY($3)",
        )
        .bind(target_id)
        .bind(now_utc())
        .bind(source_ids.to_vec())
        .execute(ctx.conn().await?.as_conn())
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blank_name_is_rejected_before_any_connection() {
        // 用一个连不上的 pool：若校验缺失，返回的会是连接错误。
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .expect("connect_lazy 不会真的去连");
        let repo = ActorRepository::new(pool);

        for blank in ["", "   ", "\t\n"] {
            let err = repo
                .insert(&NewActor {
                    javdb_id: "ABC".to_owned(),
                    name: blank.to_owned(),
                })
                .await
                .unwrap_err();
            assert!(
                matches!(err, DbError::Business { .. }),
                "空名应被 422 拒绝，实际 {err:?}"
            );
        }
    }

    #[test]
    fn blank_javdb_id_is_rejected_because_the_column_is_not_null() {
        // 上游：`CaseSensitiveCharField(max_length=64, unique=True, index=True)`，
        // 没有 `null=True` → DDL 是 `javdb_id varchar(64) NOT NULL UNIQUE`。
        //
        // 所以空白不能「归一为 NULL」——那会让 INSERT 违反 NOT NULL。
        // 按业务错误拒绝，调用方能立刻看到是哪个字段有问题。
        for blank in ["", "   ", "\t\n"] {
            let a = NewActor {
                javdb_id: blank.to_owned(),
                name: "演员".to_owned(),
            };
            let err = a
                .normalized()
                .expect_err("空白 javdb_id 应被拒绝：该列 NOT NULL");
            assert!(err.to_string().contains("javdb_id"), "{err}");
        }
    }

    #[test]
    fn both_fields_are_trimmed() {
        // 保留 trim 的理由：唯一索引大小写敏感，而 JavDB id 常被无意
        // 前后带空格。trim 后再存，「同一部影片」的不同写入路径才能落到
        // 同一行上，否则会插出两条 javdb_id 分别是 "ABC" 与 " ABC " 的行。
        let b = NewActor {
            javdb_id: "  ABC  ".to_owned(),
            name: " 演员 ".to_owned(),
        };
        let (name, javdb) = b.normalized().unwrap();
        assert_eq!(name, "演员", "name 应被 trim");
        assert_eq!(javdb, "ABC", "javdb_id 应被 trim");
    }
}
