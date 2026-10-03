//! `actor` 表仓储。
//!
//! # 这一批解锁了什么
//!
//! `actor` 是与 `Movie` **完全对称**的主数据：同样有 9 个受保护字段、
//! 同样有 `field_owners` / `mutation_revision`、同样有字段主权网关。
//! 上游有 `actor_ownership_gateway.py` 与 `actor_merge_service.py`，
//! 而我们此前只实现了 Movie 那一半。
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

use chrono::NaiveDateTime;
use sqlx::{FromRow, PgPool};

use crate::catalog::actor::Actor;
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

use super::ctx::Ctx;

const ENTITY: &str = "Actor";

/// 新建一位演员。
#[derive(Debug, Clone)]
pub struct NewActor {
    /// JavDB ID。空串在写入前归一为 `None`。
    pub javdb_id: Option<String>,
    pub name: String,
}

impl NewActor {
    fn validate(&self) -> Result<(), DbError> {
        if self.name.trim().is_empty() {
            return Err(DbError::business(ENTITY, "name 不能为空"));
        }
        Ok(())
    }

    /// 归一后的插入参数。
    fn normalized(&self) -> Result<(&str, Option<String>), DbError> {
        self.validate()?;
        // 空串会让 `WHERE javdb_id = ''` 命中一条「没有 JavDB 编号」的假记录。
        let javdb_id = self
            .javdb_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        Ok((self.name.trim(), javdb_id))
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
                    javdb_id: None,
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
    fn javdb_id_blank_normalises_to_none() {
        // 空串会让 `WHERE javdb_id = ''` 命中一条「没有编号」的假记录。
        let a = NewActor {
            javdb_id: Some("   ".to_owned()),
            name: "演员".to_owned(),
        };
        assert_eq!(a.normalized().unwrap().1, None);

        let b = NewActor {
            javdb_id: Some("  ABC  ".to_owned()),
            name: " 演员 ".to_owned(),
        };
        let (name, javdb) = b.normalized().unwrap();
        assert_eq!(name, "演员", "name 应被 trim");
        assert_eq!(javdb.as_deref(), Some("ABC"), "javdb_id 应被 trim");
    }
}
