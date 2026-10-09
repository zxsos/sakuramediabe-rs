//! 合集族 6 张表：`playlist` / `moment_collection` / `clip_collection`
//! 与它们的成员表。
//!
//! # 三个父表结构相同，所以仓储也是三个
//!
//! 模型层已经用 `owned_collection!` 宏把三个结构体生成好了（差异只有
//! `Playlist.kind` 一列）。这里用同样的手法生成三个仓储 —— 不是为了少写
//! 三遍，而是为了让「父表的六个方法」在三处**必然**保持一致：插件归属的
//! 三种查询、名称查询、插入、删除。
//!
//! 若哪天三个父表真的分家了，宏会一起变 —— 那时它就该被拆开。
//!
//! # 插件归属索引的语义
//!
//! 三张表都有 `UNIQUE (owner_plugin_id, plugin_key)`。注意它**不是**
//! 「name 全局唯一」的同义词：`name` 自己带 unique，而这一条只防止**同一个
//! 插件**重复注册同一个 key。宿主或用户创建的列表两列都是 NULL，而 NULL
//! 不参与唯一约束（SQL 标准）—— 所以插件之间、宿主与插件之间互不冲突。
//!
//! 混合状态（一列有值一列为空）是**半配置**，不算插件所有：
//! [`PluginOwned::is_plugin_owned`] 要求两列都非空。`list_plugin_owned`
//! 因此用 `(owner_plugin_id, plugin_key) IS NOT NULL` 而不是
//! `owner_plugin_id = $1`。
//!
//! # 成员表的 `position` 由应用维护
//!
//! 三张成员表的唯一索引都是 `(collection_id, <成员>_id)`，**`position`
//! 不参与**。所以：
//!
//! - 同一个成员在同一个合集里**不能出现两次**（重复插入撞唯一约束）；
//! - 但**两个不同成员可以有相同 position** —— 数据库不管这个。
//!
//! 换句话说「合集是有序的」这件事完全靠应用保证。`replace_items` 与
//! `append_*` 因此都按调用方给的顺序写 position，而不去校验「这个位置上
//! 已经有别的东西了」。
//!
//! # 全部外键都是 CASCADE
//!
//! 删合集 → 成员行消失；删成员（影片/时刻/片段）→ 所属合集里的对应行消失。
//! 两种方向都不会留下孤儿行，仓储层因此不需要「清理残留」的方法。

use std::collections::HashMap;

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::collections::{
    ClipCollection, ClipCollectionItem, MomentCollection, MomentCollectionItem, Playlist,
    PlaylistMovie, PluginOwned, PLAYLIST_KIND_RECENTLY_PLAYED,
};
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

use super::ctx::Ctx;
use super::movie::{safe_sql, RESOLUTION_LEVEL_CASE};

/// 三个合集共用一个错误实体名 —— 它们在 API 层是同一种资源，
/// 报错时说「Playlist」比说「ClipCollection」更贴近调用方的心智模型。
const COLLECTION_ENTITY: &str = "Collection";

/// `playlist_movie` 单独一个错误实体名 —— 它是「列表 × 影片」的关联行，
/// 与三个合集本体不是同一种资源，混用会让排障时分不清是哪张表出错。
const PLAYLIST_MOVIE_ENTITY: &str = "PlaylistMovie";
/// 新建一个合集。
#[derive(Debug, Clone)]
pub struct NewCollection<C> {
    /// 全局唯一。
    pub name: String,
    pub description: String,
    /// 创建该合集的插件；宿主或用户创建时为 `None`。
    pub owner_plugin_id: Option<String>,
    /// 插件自定义的稳定 key。与 `owner_plugin_id` 同时为 `None`
    /// 表示宿主所有。
    pub plugin_key: Option<String>,
    /// 只有 `Playlist` 用得上：`kind` 的初值。其他两个合集忽略它。
    pub kind: Option<String>,
    /// 让调用方在类型上区分「要 kind」与「不要 kind」。
    pub _marker: std::marker::PhantomData<C>,
}

impl<C> NewCollection<C> {
    /// 宿主或用户创建的合集。
    pub fn host_owned(name: &str, description: &str) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            owner_plugin_id: None,
            plugin_key: None,
            kind: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// 插件创建的合集。
    pub fn plugin_owned(
        owner_plugin_id: &str,
        plugin_key: &str,
        name: &str,
        description: &str,
    ) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            owner_plugin_id: Some(owner_plugin_id.to_owned()),
            plugin_key: Some(plugin_key.to_owned()),
            kind: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// 指定 `kind`。只对 `NewCollection<Playlist>` 有意义。
    pub fn with_kind(mut self, kind: &str) -> Self {
        self.kind = Some(kind.to_owned());
        self
    }

    fn owner_pair(&self) -> Result<(Option<&str>, Option<&str>), DbError> {
        // 半配置状态在这里被拒，而不是靠 `is_plugin_owned` 事后判断：
        // 「只有 plugin_key 没有 owner」意味着「哪个插件的这份资源」无从
        // 归属，写进库会让唯一索引 `(owner, key)` 变成全局唯一——
        // 于是**另一个**插件用同一个 key 就会撞约束，而那本该是两件事。
        match (&self.owner_plugin_id, &self.plugin_key) {
            (Some(_), None) | (None, Some(_)) => Err(DbError::business(
                COLLECTION_ENTITY,
                "owner_plugin_id 与 plugin_key 必须同时有值或同时为空：\
                 只有其一时无法判断这份合集属于哪个插件",
            )),
            _ => {
                if self
                    .owner_plugin_id
                    .as_deref()
                    .is_some_and(|s| s.trim().is_empty())
                    || self
                        .plugin_key
                        .as_deref()
                        .is_some_and(|s| s.trim().is_empty())
                {
                    return Err(DbError::business(
                        COLLECTION_ENTITY,
                        "owner_plugin_id / plugin_key 空白应传 None，不是空串",
                    ));
                }
                if self.name.trim().is_empty() {
                    return Err(DbError::business(COLLECTION_ENTITY, "name 不能为空"));
                }
                Ok((
                    self.owner_plugin_id.as_deref().map(str::trim),
                    self.plugin_key.as_deref().map(str::trim),
                ))
            }
        }
    }
}
/// 生成三个结构相同的合集父表仓储。
///
/// 六个方法在三个合集上**必然**一致 —— 插件归属的三种查询、按名查询、
/// `$kind_cols` 与 `$kind_placeholder` 是**片段**而不是完整子句，为的是让
/// 「这张表没有 kind 列」这件事自然地表达成两个空串：
///
/// ```text
/// 有 kind: cols = ", kind"          placeholder = ", $5"
/// 无 kind: cols = ""                placeholder = ""
/// ```
///
/// 两者占位符编号一致（都是 `$1..$5`），所以绑值代码不需要分支。
///
/// 早先的写法给无 kind 的表传空列名，而 SQL 是
/// `"..., plugin_key, ", $kind_column, ", created_at..."` —— 拼出来是
/// `plugin_key, , created_at`，多一个逗号，语法错误。
macro_rules! impl_collection_repo {
    (
        $repo:ident, $model:ty, $table:literal, $new:ty,
        $kind_cols:literal, $kind_placeholder:literal, $values_tail:literal,
        $kind_bind:expr
    ) => {
        #[doc = concat!("`", $table, "` 表仓储。")]
        #[derive(Debug, Clone)]
        pub struct $repo {
            pool: PgPool,
        }

        impl $repo {
            pub fn new(pool: PgPool) -> Self {
                Self { pool }
            }

            pub fn pool(&self) -> &PgPool {
                &self.pool
            }

            /// 按主键查询。
            pub async fn find_by_id(&self, id: i32) -> Result<Option<$model>, DbError> {
                Ok(
                    sqlx::query_as::<_, $model>(concat!(
                        "SELECT * FROM ",
                        $table,
                        " WHERE id = $1"
                    ))
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?,
                )
            }

            /// 列出全部合集，**只保留插件所有的**。
            ///
            /// 与 [`Self::list_plugin_owned`] 的区别：那个按 `owner_plugin_id`
            /// 过滤（SQL 条件），这个在**应用侧**用
            /// [`PluginOwned::is_plugin_owned`] 逐行判断，因此连半配置状态
            /// （只有 owner 没有 key）也会被排除。
            ///
            /// 什么时候需要这个而不是 SQL 过滤：调用方拿到的是**混合来源**的
            /// 合集，要按归属分组 —— 判定规则就必须与模型上的定义一致，
            /// 而模型的定义是「两列都非空」。
            pub async fn list_plugin_owned_rows(&self) -> Result<Vec<$model>, DbError> {
                let all = sqlx::query_as::<_, $model>(concat!(
                    "SELECT * FROM ",
                    $table,
                    " ORDER BY name"
                ))
                .fetch_all(&self.pool)
                .await?;
                Ok(all
                    .into_iter()
                    .filter(|row| row.is_plugin_owned())
                    .collect())
            }

            /// 按名查询。走 `name` 的唯一索引。
            pub async fn find_by_name(&self, name: &str) -> Result<Option<$model>, DbError> {
                Ok(sqlx::query_as::<_, $model>(concat!(
                    "SELECT * FROM ",
                    $table,
                    " WHERE name = $1"
                ))
                .bind(name.trim())
                .fetch_optional(&self.pool)
                .await?)
            }

            /// 按插件的稳定 key 查询。**走 `(owner_plugin_id, plugin_key)`
            /// 唯一索引。** 宿主与用户创建的合集不会出现在这里 ——
            /// 它们的这两列都是 NULL，而 `NULL = $1` 不成立。
            ///
            /// 判定「这份合集归插件所有」的依据是
            /// [`PluginOwned::is_plugin_owned`]：两列**都**非空才算。
            /// 本方法只查两列都非空的行，半配置状态（只有 owner 没有 key）
            /// 查不到 —— 那正是 `list_plugin_owned` 用
            /// `plugin_key IS NOT NULL` 把它们排除掉的原因。
            pub async fn find_by_plugin_key(
                &self,
                owner_plugin_id: &str,
                plugin_key: &str,
            ) -> Result<Option<$model>, DbError> {
                Ok(sqlx::query_as::<_, $model>(concat!(
                    "SELECT * FROM ",
                    $table,
                    " WHERE owner_plugin_id = $1 AND plugin_key = $2",
                ))
                .bind(owner_plugin_id.trim())
                .bind(plugin_key.trim())
                .fetch_optional(&self.pool)
                .await?)
            }

            /// 插入。
            ///
            /// `owner_plugin_id` / `plugin_key` 由 `NewCollection` 的内部
            /// 校验保证「要么都空，要么都非空」。那条规则是必需的 ——
            /// 唯一索引 `(owner_plugin_id, plugin_key)` 在 `(NULL, NULL)`
            /// 之间不冲突，而 `(A, NULL)` 与 `(B, NULL)` **也不**冲突，
            /// 于是两个不同的插件可以用同一个 key 而互不报错。那是半配置
            /// 状态造出的真实漏洞，入口处拒掉。
            ///
            /// # `$values_tail` 为什么必须随 `kind` 而变
            ///
            /// 两种形态都绑**六个**值，第五个是 `$kind_bind`：
            ///
            /// | 形态 | 第五个绑定 | `VALUES` 尾部 |
            /// |---|---|---|
            /// | 有 `kind` 列 | `kind`（`Option<&str>`） | `, $6, $6)` |
            /// | 无 `kind` 列 | `now`（`NaiveDateTime`） | `, $5, $6)` |
            ///
            /// 早先的版本让第五个绑定恒为 `kind`，并统一写 `, $5, $5)`。
            /// 对没有 `kind` 列的两个合集，`$5` 就落进了 `created_at`
            /// —— 而它绑的是 `Option<&str>`，于是 PostgreSQL 报：
            ///
            /// ```text
            /// column "created_at" is of type timestamp without time zone
            /// but expression is of type text
            /// ```
            ///
            /// 这个缺陷从父表仓储落地起就存在，且从未被触发过 ——
            /// 合集族此前**没有任何测试**。
            pub async fn insert(&self, new: &$new) -> Result<$model, DbError> {
                let (owner, key) = new.owner_pair()?;
                let now = crate::common::time::now_utc();
                let fifth = $kind_bind(new);
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (name, description, owner_plugin_id, plugin_key",
                    $kind_cols,
                    ", created_at, updated_at) ",
                    "VALUES ($1, $2, $3, $4",
                    $kind_placeholder,
                    $values_tail,
                ))
                .bind(new.name.trim())
                .bind(new.description.trim())
                .bind(owner)
                .bind(key)
                .bind(fifth)
                .bind(now)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))
            }

            /// 事务内变体，供 [`Ctx`] 编排多表写入时使用。
            ///
            /// 与 [`Self::insert`] 共用同一套占位符编号 —— 两条路径各自
            /// 抄一份 SQL 是最容易让它们悄悄分叉的地方，所以编号规则写在
            /// 宏参数里。
            pub async fn insert_in(
                &self,
                ctx: &mut Ctx<'_>,
                new: &$new,
            ) -> Result<$model, DbError> {
                let (owner, key) = new.owner_pair()?;
                let now = crate::common::time::now_utc();
                let fifth = $kind_bind(new);
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (name, description, owner_plugin_id, plugin_key",
                    $kind_cols,
                    ", created_at, updated_at) ",
                    "VALUES ($1, $2, $3, $4",
                    $kind_placeholder,
                    $values_tail,
                ))
                .bind(new.name.trim())
                .bind(new.description.trim())
                .bind(owner)
                .bind(key)
                .bind(fifth)
                .bind(now)
                .fetch_one(ctx.conn().await?.as_conn())
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))
            }

            /// 推进 `updated_at`，返回是否真的改了。
            ///
            /// 合集的「最近活跃」靠这一列 —— 列表页按它排序。上游在增删
            /// 成员时都会 `_touch_playlist`，所以这个入口是必需的。
            ///
            /// 刻意**只**更新 `updated_at`：连 name / description 一起写
            /// 会在并发下把别人的改动覆盖掉。
            pub async fn touch(&self, id: i32) -> Result<bool, DbError> {
                let result = sqlx::query(concat!(
                    "UPDATE ",
                    $table,
                    " SET updated_at = $2 WHERE id = $1",
                ))
                .bind(id)
                .bind(crate::common::time::now_utc())
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
                Ok(result.rows_affected() > 0)
            }

            /// 改名，返回是否真的改了。空白名按业务错误拒绝。
            ///
            /// `name` 是唯一索引，所以重名会撞约束 —— 唯一性判断留给
            /// service 层（它能区分「系统保留名」与「已被占用」，仓储不能）。
            pub async fn rename(&self, id: i32, name: &str) -> Result<bool, DbError> {
                let name = name.trim();
                if name.is_empty() {
                    return Err(DbError::business(COLLECTION_ENTITY, "name 不能为空"));
                }
                let result = sqlx::query(concat!(
                    "UPDATE ",
                    $table,
                    " SET name = $2, updated_at = $3 WHERE id = $1",
                ))
                .bind(id)
                .bind(name)
                .bind(crate::common::time::now_utc())
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
                Ok(result.rows_affected() > 0)
            }

            /// 改描述，返回是否真的改了。
            ///
            /// 与 [`Self::rename`] 分开而不是一个 `update` 入口 —— 上游的
            /// `update_playlist` 是「局部可更新，未传的字段保持原值」。一个
            /// 全字段写入口会把「只改描述」变成「连名字一起重写」，在并发下
            /// 覆盖别人的改名。
            pub async fn set_description(
                &self,
                id: i32,
                description: &str,
            ) -> Result<bool, DbError> {
                let result = sqlx::query(concat!(
                    "UPDATE ",
                    $table,
                    " SET description = $2, updated_at = $3 WHERE id = $1",
                ))
                .bind(id)
                .bind(description.trim())
                .bind(crate::common::time::now_utc())
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
                Ok(result.rows_affected() > 0)
            }

            /// 删合集。**连带删除全部成员行**（`ON DELETE CASCADE`）。
            pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
                let result = sqlx::query(concat!("DELETE FROM ", $table, " WHERE id = $1"))
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                Ok(result.rows_affected() > 0)
            }
        }
    };
}
/// 时刻/片段合集**没有** `kind` 列，所以第五个绑定是 `now` 而不是 `kind`。
///
/// 返回 `NaiveDateTime` 而**不是** `None` —— 第五个绑定在两种形态下占据
/// 的位置不同：
///
/// | 形态 | `$5` 是 | `$6` 是 |
/// |---|---|---|
/// | 有 `kind` 列 | `kind` | `now`（created_at 与 updated_at 共用） |
/// | 无 `kind` 列 | `now`（created_at） | `now`（updated_at） |
///
/// 早先的版本让这个函数返回 `None`，而 `VALUES` 尾部恒为 `, $5, $5)` ——
/// 于是 `None` 落进了 `created_at`（timestamp），PostgreSQL 报
/// `column "created_at" is of type timestamp without time zone but
/// expression is of type text`。
///
/// 合集族的父表仓储在此之前从未被执行过 —— 这就是它藏了多久的原因。
fn no_kind<C>(_new: &NewCollection<C>) -> chrono::NaiveDateTime {
    crate::common::time::now_utc()
}

/// `Playlist` 的 `kind` 取值。
///
/// **未指定时回落成 [`Playlist::default_kind`]，而不是 `None`。**
///
/// `playlist.kind` 是 `varchar(64) NOT NULL` 且**没有 DEFAULT**，所以绑
/// `None` 就是绑 NULL，直接违反约束：
///
/// ```text
/// ConstraintViolation { entity: "Collection", constraint: "not_null_violation" }
/// ```
///
/// 模型上早就写着 `Playlist::default_kind()`，而仓储没有调用它 ——
/// 注释里反而写着「留 `None`，让列的 DEFAULT 生效」，而那个 DEFAULT
/// 并不存在。合集族此前没有任何测试，所以这条路径从未被执行。
fn playlist_kind(new: &NewCollection<Playlist>) -> String {
    new.kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| Playlist::default_kind())
        .to_owned()
}

impl_collection_repo!(
    PlaylistRepository,
    Playlist,
    "playlist",
    NewCollection<Playlist>,
    ", kind",
    ", $5",
    ", $6, $6) RETURNING *",
    playlist_kind
);

impl_collection_repo!(
    MomentCollectionRepository,
    MomentCollection,
    "moment_collection",
    NewCollection<MomentCollection>,
    "",
    "",
    ", $5, $6) RETURNING *",
    no_kind::<MomentCollection>
);

impl_collection_repo!(
    ClipCollectionRepository,
    ClipCollection,
    "clip_collection",
    NewCollection<ClipCollection>,
    "",
    "",
    ", $5, $6) RETURNING *",
    no_kind::<ClipCollection>
);
/// 三个合集的分页方法**手写**而不放进宏。
///
/// `paged_list!` 的 `count` / `items` 捕获的是 `:literal`，而宏里的
/// `concat!("...", $table, "...")` 是一次宏调用，不是单个字面量 token ——
/// 直接放进去编译不过（`no rules expected concat`）。
///
/// 放宽 `paged_list!` 去接受 `:expr` 是可行的，但那样生成的 SQL 就不再是
/// 编译期常量，而 `safe_sql` 的全部意义就是「这段 SQL 是审过的常量」。
/// 所以宁可手写六遍。
macro_rules! impl_paged {
    (
        $repo:ident, $model:ty,
        $count_all:literal, $items_all:literal,
        $count_owned:literal, $items_owned:literal
    ) => {
        impl $repo {
            paged_list! {
                /// 列出某个插件名下的全部合集。**分页。**
                ///
                /// `plugin_key IS NOT NULL` 那一半不是冗余：半配置状态
                /// （只有 owner 没有 key）在 `find_by_plugin_key` 下查不到，
                /// 但确实属于这个插件。只按 `owner_plugin_id` 过滤会把它
                /// 一起列出来 —— 那不是错，但会让人困惑「为什么这条没有 key」。
                ///
                /// 刻意**不**加 `plugin_key = $2`：`plugin_key` 在插件内部
                /// 也不保证唯一，列出全部才是有用的语义。
                pub async fn list_plugin_owned(
                    &self,
                    owner_plugin_id: &str,
                ) -> Result<Page<$model>, DbError> {
                    count = $count_owned,
                    items = $items_owned,
                }
            }

            paged_list! {
                /// 列出全部合集。**分页。** 按 `name` 排序，与唯一索引一致。
                pub async fn list(&self) -> Result<Page<$model>, DbError> {
                    count = $count_all,
                    items = $items_all,
                }
            }
        }
    };
}

impl_paged!(
    PlaylistRepository,
    Playlist,
    "SELECT COUNT(*) FROM playlist",
    "SELECT * FROM playlist ORDER BY name LIMIT $1 OFFSET $2",
    "SELECT COUNT(*) FROM playlist WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL",
    "SELECT * FROM playlist WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL \
     ORDER BY name LIMIT $2 OFFSET $3"
);

impl_paged!(
    MomentCollectionRepository,
    MomentCollection,
    "SELECT COUNT(*) FROM moment_collection",
    "SELECT * FROM moment_collection ORDER BY name LIMIT $1 OFFSET $2",
    "SELECT COUNT(*) FROM moment_collection WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL",
    "SELECT * FROM moment_collection WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL \
     ORDER BY name LIMIT $2 OFFSET $3"
);

impl_paged!(
    ClipCollectionRepository,
    ClipCollection,
    "SELECT COUNT(*) FROM clip_collection",
    "SELECT * FROM clip_collection ORDER BY name LIMIT $1 OFFSET $2",
    "SELECT COUNT(*) FROM clip_collection WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL",
    "SELECT * FROM clip_collection WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL \
     ORDER BY name LIMIT $2 OFFSET $3"
);

impl ClipCollectionRepository {
    /// 列出全部片段合集，按 `updated_at DESC, id DESC`。**刻意不分页。**
    ///
    /// # 为什么不复用宏给的 `list()`
    ///
    /// 两条都不可省：
    ///
    /// 1. **排序键不同。** 宏里的 `list()` 按 `name`（与唯一索引一致，利于分页
    ///    稳定）；这里按 `updated_at DESC` —— 上游合集列表的顺序是「最近动过
    ///    的在前」，而**增删成员都会 touch `updated_at`**。按 `name` 排会让
    ///    「刚加过片段的合集」停在字母原处，与上游可见的顺序不符。
    /// 2. **不分页。** 上游返回 `list[...]` 而非 `PageResponse` —— 合集数量是
    ///    用户级的小数字，分页没有意义。
    ///
    /// # 为什么是独立 impl 而不是进宏
    ///
    /// `impl_paged!` 收的是**字面量 SQL 字符串**，没有表名元变量，所以这段
    /// 没法用 `concat!` 拼表名。而 `MomentCollectionRepository` 不需要它
    /// （时刻点合集没有列表端点），`PlaylistRepository` 也 already有自己的
    /// `list_ordered`。
    ///
    /// # `updated_at` 可空，而 DESC 在 PostgreSQL 里是 NULLS FIRST
    ///
    /// 从未被 touch 过的合集会排到最前面。看着违反直觉，但**与上游一致**
    /// —— 上游 `ClipCollection.updated_at.desc()` 落到 PG 上是同一串 SQL。
    /// 刻意不补 `NULLS LAST`：合集顺序是客户端会缓存并做乐观更新的状态。
    ///
    /// `id DESC` 作次级键：同一毫秒内被 touch 的两个合集（`add_clip` 会连带
    /// touch 父合集）`updated_at` 会并列，只按它排会让列表在两次刷新间抖动。
    pub async fn list_ordered_by_recency(&self) -> Result<Vec<ClipCollection>, DbError> {
        Ok(sqlx::query_as::<_, ClipCollection>(
            "SELECT * FROM clip_collection ORDER BY updated_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

impl MomentCollectionRepository {
    /// 列出全部时刻合集，按 `updated_at DESC, id DESC`。**刻意不分页。**
    ///
    /// 与 [`ClipCollectionRepository::list_ordered_by_recency`] 逐条同理，
    /// 包括 `updated_at` 可空导致「从未 touch 过的排最前」这个与上游一致的
    /// 行为。
    ///
    /// # 更正一处此前的判断
    ///
    /// 上面 clip 那份的文档写着「`MomentCollectionRepository` 不需要它
    /// （时刻点合集没有列表端点）」。**那是错的** —— 上游
    /// `moment_collections.py:21` 有 `GET ""` → `list[MomentCollectionResource]`，
    /// 且 `moment_collection_service.py:146-150` 的排序与 clip 完全相同
    /// （`updated_at.desc(), id.desc()`）。它此前缺，是因为时刻合集那 9 个
    /// 端点还停在 `todo!()`。
    pub async fn list_ordered_by_recency(&self) -> Result<Vec<MomentCollection>, DbError> {
        Ok(sqlx::query_as::<_, MomentCollection>(
            "SELECT * FROM moment_collection ORDER BY updated_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

// ================================================================ 成员表

/// 生成两张「有序合集成员表」的仓储。
///
/// `moment_collection_item` 与 `clip_collection_item` 只差两样：表名，与
/// 成员外键的列名。七个方法因此**必然**一致 —— 追加、按序列出、解绑、
/// 清空、插入到指定位置、替换全部。
///
/// # `position` 不在唯一索引里
///
/// 唯一索引是 `(collection_id, <成员>)`，**不含 `position`**。所以数据库
/// 保证的是「同一成员不会在同一个合集里出现两次」，而**不**保证
/// 「不同成员的 position 互不相同」。两个成员完全可以同处一个 position。
///
/// 这一点决定下面两个方法的行为：
///
/// - `append` 用 `max(position) + 1` 算新位置。并发调用会算出同一个值
///   —— 结果仍确定（并列按 `id` 排），但不是「一个接一个」。要严格有序
///   就用 `replace_all` 或 `insert_at`，它们写调用方给的确定值。
/// - `unlink` **不重排**留下的空位。删中间一个就为补洞而重写全部
///   `position`，代价是 O(n) 次写，且会让并发读者看到中间状态。缺口在
///   播放时不可见（`ORDER BY position, id` 仍给出确定顺序）。
/// # 为什么用 `#[macro_export]` 而不是 `pub(crate) use`
///
/// `macro_rules!` 默认只在**定义点之后**的文本作用域内可见，所以
/// `video_collection.rs` 里想复用它就得按路径引用。两条路都试过：
///
/// - `pub(crate) use impl_ordered_member_repo;` 放在宏**前面** ——
///   报 `unresolved import`，因为重导出的目标还不存在。
/// - 放在宏**后面** —— 报 `E0252: defined multiple times`，
///   `macro_rules!` 已经用这个名字引入了一个绑定。
///
/// `#[macro_export]` 把它导出到 crate 根，于是
/// `crate::impl_ordered_member_repo!(...)` 干净可用。它同时让这个宏出现在
/// `cargo doc` 里 —— 对一个「给三张同构表生成七个方法」的宏来说，
/// 那反而是有用的。
#[macro_export]
macro_rules! impl_ordered_member_repo {
    (
        // `$entity` 收 `expr` 而不是 `literal`：宏体里只把它当表达式用
        // （`with_entity($entity)`，没有 `concat!`），所以允许调用方传一个
        // `const` —— 否则「`VideoCollectionItem`」这个实体名会在调用点再抄
        // 一遍字面量，与文件里的常量分叉。
        $repo:ident, $model:ty, $table:literal, $member:literal, $entity:expr
    ) => {
        #[doc = concat!("`", $table, "` 表仓储：有序合集成员。")]
        #[derive(Debug, Clone)]
        pub struct $repo {
            pool: PgPool,
        }

        impl $repo {
            pub fn new(pool: PgPool) -> Self {
                Self { pool }
            }

            pub fn pool(&self) -> &PgPool {
                &self.pool
            }

            /// 追加到末尾。位置为当前最大值加一。
            ///
            /// **不**声称并发安全：两个并发 `append` 会算出同一个位置。
            /// 结果仍确定（并列按 `id` 排），但不是「一个接一个」。
            /// 需要严格有序时用 `replace_all`。
            pub async fn append(
                &self,
                collection_id: i32,
                member_id: i32,
            ) -> Result<$model, DbError> {
                let now = $crate::common::time::now_utc();
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (collection_id, ",
                    $member,
                    ", position, created_at, updated_at) ",
                    "VALUES ($1, $2, ",
                    "  COALESCE((SELECT max(position) FROM ",
                    $table,
                    "              WHERE collection_id = $1), -1) + 1, ",
                    "  $3, $3) ",
                    "RETURNING *",
                ))
                .bind(collection_id)
                .bind(member_id)
                .bind(now)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity($entity))
            }
            /// 查某个合集里是否已有这个成员。**至多一行。**
            ///
            /// 存在的唯一索引 `(collection_id, <成员外键>)` 就是这条查询的
            /// 索引，所以它是 O(1) 的点查。
            ///
            /// # 为什么值得单独一个方法
            ///
            /// 「重复加入要幂等」这条规则要求**先查后插**，而
            /// `list_by_collection` 是把整个合集读出来在内存里扫 —— 那个
            /// 复杂度是 O(合集规模)，而万级成员合集在上游是真实存在的
            /// （`list_collection_items` 的分页就是为它加的）。
            pub async fn find_by_member(
                &self,
                collection_id: i32,
                member_id: i32,
            ) -> Result<Option<$model>, DbError> {
                Ok(sqlx::query_as::<_, $model>(concat!(
                    "SELECT * FROM ",
                    $table,
                    " WHERE collection_id = $1 AND ",
                    $member,
                    " = $2",
                ))
                .bind(collection_id)
                .bind(member_id)
                .fetch_optional(&self.pool)
                .await?)
            }
            /// 追加到末尾的事务内变体，位置同样算 `max(position) + 1`。
            ///
            /// 与 [`Self::append`] 的差别是「算 MAX」与「插入」落在**同一个
            /// 事务**里：并发追加时不会读到陈旧 MAX。
            pub async fn append_in(
                &self,
                ctx: &mut Ctx<'_>,
                collection_id: i32,
                member_id: i32,
            ) -> Result<$model, DbError> {
                let now = $crate::common::time::now_utc();
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (collection_id, ",
                    $member,
                    ", position, created_at, updated_at) ",
                    "VALUES ($1, $2, ",
                    "  COALESCE((SELECT max(position) FROM ",
                    $table,
                    "              WHERE collection_id = $1), -1) + 1, ",
                    "  $3, $3) ",
                    "RETURNING *",
                ))
                .bind(collection_id)
                .bind(member_id)
                .bind(now)
                .fetch_one(ctx.conn().await?.as_conn())
                .await
                .map_err(|e| DbError::from(e).with_entity($entity))
            }
            /// 插到指定位置。**不**检查该位置是否已被占用。
            ///
            /// 唯一索引不含 `position`，所以数据库不会拦重复位置 ——
            /// 那是**调用方**要保证的事（拖拽排序会算出目标位置）。
            pub async fn insert_at(
                &self,
                collection_id: i32,
                member_id: i32,
                position: i32,
            ) -> Result<$model, DbError> {
                let now = $crate::common::time::now_utc();
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (collection_id, ",
                    $member,
                    ", position, created_at, updated_at) ",
                    "VALUES ($1, $2, $3, $4, $4) RETURNING *",
                ))
                .bind(collection_id)
                .bind(member_id)
                .bind(position)
                .bind(now)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity($entity))
            }

            /// 事务内变体，供 [`Ctx`] 编排多表写入时使用。
            pub async fn insert_at_in(
                &self,
                ctx: &mut Ctx<'_>,
                collection_id: i32,
                member_id: i32,
                position: i32,
            ) -> Result<$model, DbError> {
                let now = $crate::common::time::now_utc();
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (collection_id, ",
                    $member,
                    ", position, created_at, updated_at) ",
                    "VALUES ($1, $2, $3, $4, $4) RETURNING *",
                ))
                .bind(collection_id)
                .bind(member_id)
                .bind(position)
                .bind(now)
                .fetch_one(ctx.conn().await?.as_conn())
                .await
                .map_err(|e| DbError::from(e).with_entity($entity))
            }

            /// 按播放顺序列出某个合集的全部成员。**刻意不分页。**
            ///
            /// 一次「读出整个合集去播」的查询，分页只会让 worker 反复取
            /// 第 1 页。`ORDER BY position, id` —— `id` 作次级键是必要的：
            /// 位置可以并列（唯一索引不含它），只按 `position` 排序时并列
            /// 之间的顺序不确定，会导致播放列表抖动。
            ///
            /// 与 `PlaylistMovieRepository::list_by_playlist` 的区别：
            /// 那张表没有 `position`，顺序只能靠加入先后。
            pub async fn list_by_collection(
                &self,
                collection_id: i32,
            ) -> Result<Vec<$model>, DbError> {
                Ok(sqlx::query_as::<_, $model>(concat!(
                    "SELECT * FROM ",
                    $table,
                    " WHERE collection_id = $1 ORDER BY position, id",
                ))
                .bind(collection_id)
                .fetch_all(&self.pool)
                .await?)
            }
            /// 按播放顺序列出**若干个**合集的全部成员。**刻意不分页。**
            ///
            /// 与 [`Self::list_by_collection`] 的区别是**入参是复数**，因此
            /// 上游那个「列表页要显示每个合集的成员数」可以一次查完 ——
            /// 逐个合集调 `list_by_collection` 就是 N+1，而列表页要渲染
            /// 全部合集。
            ///
            /// 不按 `position` 排序：跨合集时那个顺序没有意义，调用方要按
            /// 合集分组后自己排。只按 `collection_id` 排，让分组是连续的。
            pub async fn list_by_collections(
                &self,
                collection_ids: &[i32],
            ) -> Result<Vec<$model>, DbError> {
                // 与 `count_by_playlists` 同理：空输入直接返回，不发查询。
                if collection_ids.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(sqlx::query_as::<_, $model>(concat!(
                    "SELECT * FROM ",
                    $table,
                    " WHERE collection_id = ANY($1) ORDER BY collection_id",
                ))
                .bind(collection_ids)
                .fetch_all(&self.pool)
                .await?)
            }

            /// 下一个可用位置：`COALESCE(MAX(position), -1) + 1`。
            ///
            /// **空合集返回 0** —— 那个 `-1` 就是为了让首项从 0 开始。
            ///
            /// # 追加成员请用 [`Self::append`]，不要用这个
            ///
            /// `append` 把「算位置」与「插入」合成**一条** `INSERT ... SELECT`，
            /// 所以两个并发 `append` 不会算出同一个位置。这里是两条语句，
            /// 中间有窗口 —— 上游 `add_clip` 正是那个两步式（先查
            /// `MAX(position)` 再插，包在事务里），而单条语句在并发下更紧。
            ///
            /// 那为什么还留着它：`position` 语义（空位不重排、因此不能用
            /// `COUNT(*)`）需要一处可执行的说明，而 [`Self::append`] 的文档
            /// 已经在讲它。
            pub async fn next_position(&self, collection_id: i32) -> Result<i32, DbError> {
                let next: Option<i32> = sqlx::query_scalar(concat!(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM ",
                    $table,
                    " WHERE collection_id = $1",
                ))
                .bind(collection_id)
                .fetch_one(&self.pool)
                .await?;
                Ok(next.unwrap_or(0))
            }

            /// 解绑一个成员。返回是否真的删掉了一行。
            ///
            /// **不重排**留下的 `position` 空位 —— 见宏的文档。
            pub async fn unlink(
                &self,
                collection_id: i32,
                member_id: i32,
            ) -> Result<bool, DbError> {
                let result = sqlx::query(concat!(
                    "DELETE FROM ",
                    $table,
                    " WHERE collection_id = $1 AND ",
                    $member,
                    " = $2",
                ))
                .bind(collection_id)
                .bind(member_id)
                .execute(&self.pool)
                .await?;
                Ok(result.rows_affected() > 0)
            }

            /// 清空某个合集。返回删掉了几行。
            ///
            /// 删合集本身会 CASCADE 掉这些行；这个方法给的是「保留合集、
            /// 只清成员」—— 那是编辑页「全选取消」的操作。
            pub async fn clear(&self, collection_id: i32) -> Result<u64, DbError> {
                let result =
                    sqlx::query(concat!("DELETE FROM ", $table, " WHERE collection_id = $1"))
                        .bind(collection_id)
                        .execute(&self.pool)
                        .await?;
                Ok(result.rows_affected())
            }

            /// 事务内清空，供 [`Ctx`] 编排「清空 + 按序插入」时使用。
            pub async fn clear_in(
                &self,
                ctx: &mut Ctx<'_>,
                collection_id: i32,
            ) -> Result<u64, DbError> {
                let result =
                    sqlx::query(concat!("DELETE FROM ", $table, " WHERE collection_id = $1",))
                        .bind(collection_id)
                        .execute(ctx.conn().await?.as_conn())
                        .await?;
                Ok(result.rows_affected())
            }

            /// 把某个合集的成员**替换**为给定顺序的一组。
            ///
            /// 语义是「这个合集的成员就是这些，顺序如此」—— 适合拖拽排序
            /// 与编辑页保存。返回本次写入的行数。
            ///
            /// 先清后插，**不**逐条 diff：diff 能省下未变动行的写，但要让
            /// 「哪些行变了」这个判断正确，得先把当前集合完整读出比较，
            /// 而那与「清后插」的成本同量级。换来的是顺序**一定**等于
            /// 入参 —— diff 版本在并发插入下会漏掉新行。
            ///
            /// **不是**一个事务内的操作。调用方要原子性就用 `clear_in` 与
            /// `insert_at_in` 自己编排 —— 两者共享同一个 `Ctx`，所以
            /// 「清空 + 按序插入」能包在一个事务里。
            pub async fn replace_all(
                &self,
                collection_id: i32,
                member_ids: &[i32],
            ) -> Result<u64, DbError> {
                let removed = self.clear(collection_id).await?;
                for (index, member_id) in member_ids.iter().enumerate() {
                    self.insert_at(collection_id, *member_id, index as i32)
                        .await?;
                }
                Ok(removed + member_ids.len() as u64)
            }
        }
    };
}

impl_ordered_member_repo!(
    MomentCollectionItemRepository,
    MomentCollectionItem,
    "moment_collection_item",
    "point_id",
    "MomentCollectionItem"
);

impl_ordered_member_repo!(
    ClipCollectionItemRepository,
    ClipCollectionItem,
    "clip_collection_item",
    "clip_id",
    "ClipCollectionItem"
);

impl MomentCollectionItemRepository {
    /// ★ 「这个时刻属于哪些合集」—— 该点所属合集的 `(id, name)`。
    ///
    /// 上游 `MomentCollectionService.list_point_collections`
    /// （`moment_collection_service.py`）：按成员表反查，**顺序是合集的
    /// `updated_at DESC, id DESC`** —— 也就是「最近动过的合集排前面」，
    /// 与合集列表的排序一致。
    ///
    /// 一次三表 JOIN 而不是「取成员再逐个查合集」：成员表在 `point_id` 上
    /// 有索引，一个点通常只属于极少几个合集，但逐个查是 N+1。
    ///
    /// 返回元组而不是具名结构体 —— 投影行（见本文件其它查询的约定）。
    pub async fn list_collections_for_point(
        &self,
        point_id: i32,
    ) -> Result<Vec<(i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, String)>(
            "SELECT c.id, c.name FROM moment_collection_item i \
               JOIN moment_collection c ON c.id = i.collection_id \
              WHERE i.point_id = $1 \
              ORDER BY c.updated_at DESC, c.id DESC",
        )
        .bind(point_id)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// `playlist_movie` 表仓储：JAV 播放列表的成员。
///
/// **本表没有 `position`** —— 唯一索引是 `(playlist_id, movie_id)`。
/// 播放顺序只能靠 `id`，即加入播放列表的先后。模型上的
/// [`PlaylistMovie::playback_order_key`] 直接返回 `id` 就是这个意思。
///
/// 所以这里**没有** `append` / `insert_at` / `replace_all` 那套位置语义，
/// 只有「加进去」与「移出来」。要改播放顺序，只能重建整个成员列表 ——
/// 而那正是 `replace_all` 在做的事，只是不带位置列。
#[derive(Debug, Clone)]
pub struct PlaylistMovieRepository {
    pool: PgPool,
}

/// `moment_collection` / `clip_collection` 的合集计数。**给状态页用。**
///
/// 这两张表**没有 `kind` 列**，所以不需要 `include_system` 参数 ——
/// 「系统托管的合集」这个概念只存在于 `playlist`。
///
/// 刻意不写进 `impl_collection_repo!` 宏：那个宏给三张父表生成方法，而
/// `playlist` 那一侧已有自己的 `collection_counts(include_system)`（要排除
/// 系统列表）。宏里再生成一个同义方法会让调用方有机会挑错。
macro_rules! impl_collection_totals {
    ($repo:ident, $table:literal, $item_table:literal) => {
        impl $repo {
            /// 合集计数：`(合集数, 成员行数)`。**给状态页用。**
            pub async fn collection_counts(&self) -> Result<(i64, i64), DbError> {
                let count = sqlx::query_scalar::<_, i64>(concat!("SELECT COUNT(*) FROM ", $table))
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
                let items =
                    sqlx::query_scalar::<_, i64>(concat!("SELECT COUNT(*) FROM ", $item_table))
                        .fetch_one(&self.pool)
                        .await
                        .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
                Ok((count, items))
            }
        }
    };
}

impl_collection_totals!(
    MomentCollectionRepository,
    "moment_collection",
    "moment_collection_item"
);
impl_collection_totals!(
    ClipCollectionRepository,
    "clip_collection",
    "clip_collection_item"
);

/// `Playlist` 专属的仓储方法。
///
/// **不**放进 `impl_collection_repo!` —— 宏生成的三个父表里只有 `playlist`
/// 有 `kind` 列，另外两张表（`moment_collection` / `clip_collection`）
/// 没有。把引用 `kind` 的 SQL 放进宏会为它们生成一段引用不存在列的查询
/// —— 那正是本仓库此前反复出现的缺陷形状。
impl PlaylistRepository {
    /// 按 `kind` 查一个系统列表。**至多一行。**
    ///
    /// 「最近播放」是系统单例：按 kind 查，不存在才建。所以这个查询返回
    /// `Option` 而不是分页列表 —— 出现两行就是数据损坏，而不是正常情况。
    pub async fn find_by_system_kind(&self, kind: &str) -> Result<Option<Playlist>, DbError> {
        Ok(
            sqlx::query_as::<_, Playlist>("SELECT * FROM playlist WHERE kind = $1 LIMIT 1")
                .bind(kind.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 列出播放列表，**系统列表固定在前**。**刻意不分页。**
    ///
    /// # 排序对齐上游 `_playlist_system_order`
    ///
    /// ```sql
    /// ORDER BY CASE kind WHEN 'recently_played' THEN 0 ELSE 1 END ASC,
    ///          updated_at DESC, id DESC
    /// ```
    ///
    /// 上游是 `Case(Playlist.kind, _SYSTEM_KIND_ORDER, len(_SYSTEM_KIND_ORDER))`
    /// —— 那个 `len(...)` 就是 `ELSE` 分支的值，即「不在系统 kind 集合里
    /// 的一切」。写成常量 `1` 是因为 `SYSTEM_PLAYLIST_KINDS` 目前只有一项；
    /// **加第二个系统 kind 时这里要改成 2**，否则两类系统列表之间没有稳定
    /// 次序（并列后靠 `updated_at` 决定，而它们通常同时被写）。
    ///
    /// `updated_at DESC, id DESC` 两级排序是为了**稳定**：同一毫秒内被写过的
    /// 两个列表（`add_movie` 会连带 touch 父列表）`updated_at` 会并列，
    /// 只按它排会让列表页在两次刷新之间抖动。
    ///
    /// # `updated_at` 可空，而 DESC 在 PostgreSQL 里是 NULLS FIRST
    ///
    /// 所以从未被写过的列表（`updated_at IS NULL`）会排到最前面。这看着
    /// 违反直觉，但**与上游一致** —— 上游 `Playlist.updated_at.desc()` 落到
    /// PG 上是同一串 SQL。这里刻意不补 `NULLS LAST`：那会让新旧列表的
    /// 相对顺序与上游不同，而列表顺序是客户端会缓存并做乐观更新的状态。
    pub async fn list_ordered(&self, include_system: bool) -> Result<Vec<Playlist>, DbError> {
        // `kind` 列是 NOT NULL（DDL 有默认值 custom，Rust 侧也是 `String`
        // 而非 `Option<String>`），所以 `NOT IN` 不会因 NULL 产生三值逻辑。
        let sql = if include_system {
            "SELECT * FROM playlist \
             ORDER BY CASE kind WHEN 'recently_played' THEN 0 ELSE 1 END ASC, \
                      updated_at DESC, id DESC"
        } else {
            "SELECT * FROM playlist WHERE kind <> 'recently_played' \
             ORDER BY CASE kind WHEN 'recently_played' THEN 0 ELSE 1 END ASC, \
                      updated_at DESC, id DESC"
        };
        Ok(sqlx::query_as::<_, Playlist>(sql)
            .fetch_all(&self.pool)
            .await?)
    }

    /// 合集计数：`(合集数, 成员行数)`。**给状态页用。**
    ///
    /// `include_system = false` 时只数自定义列表 —— 与 `list_ordered(false)`
    /// 的口径一致，所以状态页显示的「N 个播放列表」与用户在
    /// `GET /playlists?include_system=false` 里看到的条数相同。
    ///
    /// 成员数**必须跟着同一个过滤走**（子查询里也判 `kind`）：直接
    /// `COUNT(*) FROM playlist_movie` 会把系统列表（最近播放）的成员算进去，
    /// 于是「4 个列表 / 30 部电影」里那 30 部包含了系统列表的，而用户看到的
    /// 只有 4 个自定义列表 —— 两个数字对不上，且没人知道该信哪个。
    ///
    /// 两个标量各一条查询（而不是 N+1 逐列表 `COUNT`）。
    pub async fn collection_counts(&self, include_system: bool) -> Result<(i64, i64), DbError> {
        let (count_sql, items_sql) = if include_system {
            (
                "SELECT COUNT(*) FROM playlist",
                "SELECT COUNT(*) FROM playlist_movie",
            )
        } else {
            (
                "SELECT COUNT(*) FROM playlist WHERE kind <> $1",
                "SELECT COUNT(*) FROM playlist_movie pm \
                 JOIN playlist p ON p.id = pm.playlist_id WHERE p.kind <> $1",
            )
        };
        let count = if include_system {
            sqlx::query_scalar::<_, i64>(count_sql)
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_scalar::<_, i64>(count_sql)
                .bind(PLAYLIST_KIND_RECENTLY_PLAYED)
                .fetch_one(&self.pool)
                .await?
        };
        let items = if include_system {
            sqlx::query_scalar::<_, i64>(items_sql)
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_scalar::<_, i64>(items_sql)
                .bind(PLAYLIST_KIND_RECENTLY_PLAYED)
                .fetch_one(&self.pool)
                .await?
        };
        Ok((count, items))
    }
}

// ================================================================ 影片卡片

/// 列表影片卡片的排序键。
///
/// # 为什么是闭集枚举，而不是让调用方递字符串进来
///
/// 上游 `_build_playlist_sort` 把 `field:direction` 解析成 Peewee 表达式树，
/// 其中两个字段是**相关子查询**。Rust 侧没有表达式树，`ORDER BY` 只能以片段
/// 形式写死 —— 而一旦允许调用方递片段进来，那两段子查询就成了注入口。
/// 枚举让「允许的排序键」与「能拼进 SQL 的东西」是同一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistMovieCardSort {
    /// `movie.heat`。**非空**，两个方向都不带 `NULLS` 修饰。
    Heat,
    /// `movie.release_date`。**可空**：上游把它列在
    /// `PLAYLIST_NULLABLE_SORT_FIELDS` 里，所以排序列与次级排序都带
    /// `NULLS LAST`。
    ReleaseDate,
    /// 相关子查询：该影片最近一次媒体入库时间。
    ///
    /// **不加 `NULLS LAST`**：上游没加，加了自己改口径（`ASC` 下 PG 默认空值
    /// 在后、`DESC` 下空值在前，与上游一致）。所以没有媒体的影片在 `DESC`
    /// 排序下会浮到最前，那是**契约的一部分**。
    AddedAt,
    /// 相关子查询：该影片**有效**媒体的最高码率，没有媒体或解析不出按 `0`。
    ///
    /// `0` 兜底而不是 NULL：上游刻意 `COALESCE(..., 0)`。改成 NULL 后
    /// 「没有媒体」与「码率真的是 0」会分成两队，`DESC` 下前者全部浮到最前。
    Bitrate,
}

/// 排序方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// 最近一次媒体入库时间（相关子查询）。
const LATEST_MEDIA_SUBQUERY: &str = "(SELECT MAX(md.created_at) FROM media md \
     WHERE md.movie_number = m.movie_number)";

/// 有效媒体的最高码率（相关子查询）。
///
/// # `video_info` 是 TEXT 存 JSON
///
/// `::json` 遇到脏文本会**抛错**（`invalid input syntax for type json`），
/// 这一点上游一样（`Media.video_info.cast("json")`）—— 空串被 `NULLIF` 摘掉，
/// 但 `not json at all` 会让整个列表 500。刻意不对齐掉这个行为：脏值是
/// probe 写坏的数据，应该被发现而不是被静默当成码率 0。
const MAX_BITRATE_SUBQUERY: &str =
    "(SELECT COALESCE(MAX(NULLIF(md.video_info::json->'video'->>'bit_rate', '')::bigint), 0) \
     FROM media md WHERE md.movie_number = m.movie_number AND md.valid = TRUE)";

/// 分辨率筛选片段：影片的**最高**档位落在 `[threshold, upper)`。
///
/// 对应上游 `movie_resolution_service.resolution_exists_expression`。
/// 除 `$2` / `$3` 两个占位符外全是常量，所以进 SQL 是安全的。
///
/// # `md.resolution ~ '^\d+x\d+$'` 是必需的，不是保险
///
/// [`RESOLUTION_LEVEL_CASE`] 对它做 `split_part(...)::int`。没有这条正则，
/// 一个脏值（空串、`1920*1080`、`HD`）就会让 `::int` 抛错 —— 整个列表 500，
/// 而不是少算一部影片。
fn resolution_exists_fragment(has_upper: bool) -> String {
    let upper = if has_upper {
        format!(" AND MAX({RESOLUTION_LEVEL_CASE}) < $3")
    } else {
        String::new()
    };
    format!(
        " AND EXISTS (SELECT 1 FROM media md \
          WHERE md.movie_number = m.movie_number AND md.valid = TRUE \
            AND md.resolution ~ '^\\d+x\\d+$' \
          GROUP BY md.movie_number \
          HAVING MAX({RESOLUTION_LEVEL_CASE}) >= $2{upper})"
    )
}

/// `ORDER BY` 片段。列名全部来自枚举，**没有用户输入**。
///
/// 次级排序一律 `movie.id` **同向**，对应上游 `build_ordered_expressions` 的
/// `tie_breaker=Movie.id` —— 少了它，同一热度/同一发布日的影片在两次刷新之间
/// 会抖动。`nullable` 字段的次级排序也带 `NULLS LAST`，与上游一致。
///
/// 缺省（`None`）走「列表关系最近触达倒序」，对应上游
/// `[PlaylistMovie.updated_at.desc(), PlaylistMovie.id.desc()]`。
fn card_order_by(sort: Option<(PlaylistMovieCardSort, SortDirection)>) -> String {
    let (column, direction, nullable) = match sort {
        None => return "ORDER BY pm.updated_at DESC, pm.id DESC".to_owned(),
        Some((PlaylistMovieCardSort::Heat, direction)) => ("m.heat".to_owned(), direction, false),
        Some((PlaylistMovieCardSort::ReleaseDate, direction)) => {
            ("m.release_date".to_owned(), direction, true)
        }
        Some((PlaylistMovieCardSort::AddedAt, direction)) => {
            (LATEST_MEDIA_SUBQUERY.to_owned(), direction, false)
        }
        Some((PlaylistMovieCardSort::Bitrate, direction)) => {
            (MAX_BITRATE_SUBQUERY.to_owned(), direction, false)
        }
    };
    let direction = match direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    let nulls = if nullable { " NULLS LAST" } else { "" };
    format!("ORDER BY {column} {direction}{nulls}, m.id {direction}{nulls}")
}

impl PlaylistMovieRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 加入播放列表。**幂等。**
    ///
    /// 唯一索引 `(playlist_id, movie_id)` 让重复加入可以走
    /// `ON CONFLICT DO NOTHING`，返回「本次是否真的新增了一行」。
    ///
    /// 幂等而不是报错，是因为调用方天然会重复提交：UI 上连点两下「加入」、
    /// 播放列表页面重复挂载、导入脚本重跑。拿约束违例当业务结果没有意义。
    pub async fn add(&self, playlist_id: i32, movie_id: i32) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        let result = sqlx::query(
            "INSERT INTO playlist_movie (playlist_id, movie_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $3) ON CONFLICT (playlist_id, movie_id) DO NOTHING",
        )
        .bind(playlist_id)
        .bind(movie_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(PLAYLIST_MOVIE_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 移出播放列表。返回是否真的删掉了一行。
    pub async fn remove(&self, playlist_id: i32, movie_id: i32) -> Result<bool, DbError> {
        let result =
            sqlx::query("DELETE FROM playlist_movie WHERE playlist_id = $1 AND movie_id = $2")
                .bind(playlist_id)
                .bind(movie_id)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 按加入顺序列出某个播放列表的全部影片。**刻意不分页。**
    ///
    /// `ORDER BY id` 而不是 `created_at`：本表的「顺序」就是加入先后，
    /// 而 `id` 是它的精确表达。同一毫秒内加入的两部影片，
    /// `created_at` 会并列（排序不确定，播放列表抖动），`id` 不会。
    pub async fn list_by_playlist(&self, playlist_id: i32) -> Result<Vec<PlaylistMovie>, DbError> {
        Ok(sqlx::query_as::<_, PlaylistMovie>(
            "SELECT * FROM playlist_movie WHERE playlist_id = $1 ORDER BY id",
        )
        .bind(playlist_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 「最近播放」列表里、属于**候选集**的影片，取最近的 N 部。**只返回 id。**
    ///
    /// 对应上游 `_load_recent_seed_ids`（`daily_recommendation_service.py:156-169`）：
    ///
    /// ```python
    /// rows = (PlaylistMovie.select(PlaylistMovie.movie)
    ///         .where(PlaylistMovie.playlist == playlist,
    ///                PlaylistMovie.movie.in_(candidate_ids))
    ///         .order_by(PlaylistMovie.updated_at.desc(), PlaylistMovie.id.desc())
    ///         .limit(RECENT_SEED_LIMIT))
    /// ```
    ///
    /// # `candidate_ids` 必须在 SQL 里过滤，不能取回来再筛
    ///
    /// 「最近播放」里可能有已拉黑或已删除的影片。先取最近 30 条再在内存里
    /// 剔除，会让**被剔除的名额不被补齐** —— 最近 30 条里恰有 5 条是拉黑的，
    /// 就只拿到 25 个种子。上游把过滤放进 `WHERE`，`LIMIT` 数的是候选。
    ///
    /// # 排序两级，且**不补 `NULLS LAST`**
    ///
    /// `updated_at DESC, id DESC`。`updated_at` 可空（老行），在 PG 的 `DESC`
    /// 下按默认排最前；补 `NULLS LAST` 会让顺序与上游不同。第二级 `id DESC`
    /// 给同一毫秒写入的多行一个确定次序（否则两次生成之间种子顺序会抖，
    /// 而种子位置决定 `seed_weight` 的线性衰减值）。
    pub async fn list_recent_played_in(
        &self,
        playlist_id: i32,
        candidate_ids: &[i32],
        limit: i64,
    ) -> Result<Vec<i32>, DbError> {
        // 空候选集提前返回：`movie_id = ANY('{}')` 本就零行，省一次往返。
        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT movie_id FROM playlist_movie \
             WHERE playlist_id = $1 AND movie_id = ANY($2) \
             ORDER BY updated_at DESC, id DESC LIMIT $3",
        )
        .bind(playlist_id)
        .bind(candidate_ids)
        // `max(1)`：`LIMIT 0` 会让「最近的种子」这个查询静默返回空表，
        // 而那看起来像「最近播放是空的」。上游的 `RECENT_SEED_LIMIT` 是常量 30。
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 列出某个影片出现在哪些播放列表里。**分页。**
        ///
        /// 「这部影片被哪些列表收录了」——影片详情页用。走唯一索引
        /// `(playlist_id, movie_id)` 的**后缀**。
        pub async fn list_by_movie(
            &self,
            movie_id: i32,
        ) -> Result<Page<PlaylistMovie>, DbError> {
            count = "SELECT COUNT(*) FROM playlist_movie WHERE movie_id = $1",
            items = "SELECT * FROM playlist_movie WHERE movie_id = $1 \
                     ORDER BY playlist_id LIMIT $2 OFFSET $3",
        }
    }

    /// 批量统计各播放列表的成员数。
    ///
    /// 对应上游 `count_by_owner(PlaylistMovie, PlaylistMovie.playlist, ids)`。
    /// **一次查询取回所有计数** —— 逐个列表 `COUNT(*)` 是 N+1，而列表页
    /// 一次要渲染全部列表。
    ///
    /// # 空输入直接返回空 map，不发查询
    ///
    /// 上游 `count_by_owner` 开头就是 `if not owner_ids: return {}`，而
    /// `WHERE playlist_id = ANY('{}')` 会返回零行而不是报错 —— 两者结果
    /// 相同，但提前返回省掉一次往返，也让「无系统播放列表的新装实例」
    /// 这个常见首屏不产生任何 DB 往返。
    ///
    /// # 用 `= ANY($1)` 而不是 `IN (...)`
    ///
    /// 变长列表拼进 SQL 字面量需要在 `format!` 里做占位符拼接，那正是
    /// 本仓库明确排除的做法（注入面 + 破坏 `query_as` 的字面量约定）。
    /// 数组绑定让 SQL 保持字面量。
    pub async fn count_by_playlists(
        &self,
        playlist_ids: &[i32],
    ) -> Result<HashMap<i32, i64>, DbError> {
        if playlist_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, PlaylistMemberCount>(
            "SELECT playlist_id, COUNT(*) AS member_count FROM playlist_movie \
             WHERE playlist_id = ANY($1) GROUP BY playlist_id",
        )
        .bind(playlist_ids)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| (r.playlist_id, r.member_count))
            .collect())
    }

    /// 列表内影片卡片的**总数**（已应用分辨率筛选）。
    ///
    /// 对应上游 `list_playlist_movies` 里那条 `total_query.count()`。
    /// **总数与当页用同一个筛选口径** —— 否则「共 20 条」配上 5 条结果，
    /// 客户端会一直翻页。
    pub async fn count_movie_cards(
        &self,
        playlist_id: i32,
        resolution: Option<(i32, Option<i32>)>,
    ) -> Result<i64, DbError> {
        let mut sql = String::from(
            "SELECT COUNT(*) FROM playlist_movie pm JOIN movie m ON m.id = pm.movie_id \
             WHERE pm.playlist_id = $1",
        );
        if let Some((_, upper)) = resolution {
            sql.push_str(&resolution_exists_fragment(upper.is_some()));
        }
        let mut query = sqlx::query_scalar::<_, i64>(safe_sql(sql)).bind(playlist_id);
        if let Some((threshold, upper)) = resolution {
            query = query.bind(threshold);
            if let Some(upper) = upper {
                query = query.bind(upper);
            }
        }
        Ok(query.fetch_one(&self.pool).await?)
    }

    /// 列表内影片卡片的一页：`(playlist_movie.id, playlist_movie.updated_at, movie.id)`。
    ///
    /// # 为什么返回三元组，而不是把整张卡片查出来
    ///
    /// 卡片要 20+ 个列，横跨 `movie` / `image` / `movie_series` / `media` 四张表。
    /// 一次 JOIN 展开就只能用 20+ 个位置元组（`sm-db` 不新增投影结构体，理由见
    /// `MovieResolutionLevelRow` 的文档），而**位置元组错位是静默的** ——
    /// 相邻两个字段类型相同就换得过来，只有断言到具体值时才暴露。
    ///
    /// 所以这一层只回答「这一页是哪些影片、按什么顺序」，影片本体与图片、
    /// 系列名由调用方用各自的 `find_by_ids` **批量**补齐：每页固定条数，
    /// 不随页大小增长，也不随影片数增长。
    ///
    /// # `resolution` 是 `(threshold, upper)`
    ///
    /// `upper == None` 只有最高档（8K）会出现，此时 `EXISTS` 只有一个下界，
    /// 占位符编号随之少一个 —— 所以 `LIMIT/OFFSET` 的编号是**按分支算出来的**，
    /// 写死会在筛 8K 时把 `limit` 绑到 `upper` 的位子上（而两者都是整数，
    /// 不会报类型错，只会筛出一个安静的错误结果）。
    ///
    /// # `pm.updated_at` 回到 `Option`
    ///
    /// DDL 里该列可空，而上游 DTO 把它声明成非空 `datetime`。解码成 `Option`
    /// 让调用方决定怎么呈现（本仓库对同类情况的约定是空串），而不是在这里
    /// 因为一行 NULL 让整页 500。
    pub async fn list_movie_cards(
        &self,
        playlist_id: i32,
        resolution: Option<(i32, Option<i32>)>,
        sort: Option<(PlaylistMovieCardSort, SortDirection)>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<(i32, Option<NaiveDateTime>, i32)>, DbError> {
        let has_upper = matches!(resolution, Some((_, Some(_))));
        let mut sql = String::from(
            "SELECT pm.id AS link_id, pm.updated_at AS link_updated_at, m.id AS movie_id \
             FROM playlist_movie pm JOIN movie m ON m.id = pm.movie_id \
             WHERE pm.playlist_id = $1",
        );
        if resolution.is_some() {
            sql.push_str(&resolution_exists_fragment(has_upper));
        }
        sql.push(' ');
        sql.push_str(&card_order_by(sort));
        sql.push_str(if has_upper {
            " LIMIT $4 OFFSET $5"
        } else if resolution.is_some() {
            " LIMIT $3 OFFSET $4"
        } else {
            " LIMIT $2 OFFSET $3"
        });

        let mut query =
            sqlx::query_as::<_, (i32, Option<NaiveDateTime>, i32)>(safe_sql(sql)).bind(playlist_id);
        if let Some((threshold, upper)) = resolution {
            query = query.bind(threshold);
            if let Some(upper) = upper {
                query = query.bind(upper);
            }
        }
        Ok(query.bind(limit).bind(offset).fetch_all(&self.pool).await?)
    }
}

/// `count_by_playlists` 的一行。
///
/// 单独一个 `FromRow` 结构体而不是 `query!` 宏：宏需要编译期连接，
/// CI 上没有（见 `movie.rs` 的模块文档）。
#[derive(Debug, Clone, sqlx::FromRow)]
struct PlaylistMemberCount {
    playlist_id: i32,
    member_count: i64,
}
