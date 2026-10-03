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

use sqlx::PgPool;

use crate::collections::{ClipCollection, MomentCollection, Playlist, PluginOwned};
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

use super::ctx::Ctx;

/// 三个合集共用一个错误实体名 —— 它们在 API 层是同一种资源，
/// 报错时说「Playlist」比说「ClipCollection」更贴近调用方的心智模型。
const COLLECTION_ENTITY: &str = "Collection";
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
        $kind_cols:literal, $kind_placeholder:literal, $kind_bind:expr
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
            pub async fn insert(&self, new: &$new) -> Result<$model, DbError> {
                let (owner, key) = new.owner_pair()?;
                let now = crate::common::time::now_utc();
                let kind = $kind_bind(new);
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (name, description, owner_plugin_id, plugin_key",
                    $kind_cols,
                    ", created_at, updated_at) ",
                    "VALUES ($1, $2, $3, $4",
                    $kind_placeholder,
                    ", $5, $5) RETURNING *",
                ))
                .bind(new.name.trim())
                .bind(new.description.trim())
                .bind(owner)
                .bind(key)
                .bind(kind)
                .bind(now)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))
            }

            /// 事务内变体，供 [`Ctx`] 编排多表写入时使用。
            pub async fn insert_in(
                &self,
                ctx: &mut Ctx<'_>,
                new: &$new,
            ) -> Result<$model, DbError> {
                let (owner, key) = new.owner_pair()?;
                let now = crate::common::time::now_utc();
                let kind = $kind_bind(new);
                sqlx::query_as::<_, $model>(concat!(
                    "INSERT INTO ",
                    $table,
                    " (name, description, owner_plugin_id, plugin_key",
                    $kind_cols,
                    ", created_at, updated_at) ",
                    "VALUES ($1, $2, $3, $4",
                    $kind_placeholder,
                    ", $5, $5) RETURNING *",
                ))
                .bind(new.name.trim())
                .bind(new.description.trim())
                .bind(owner)
                .bind(key)
                .bind(kind)
                .bind(now)
                .fetch_one(ctx.conn().await?.as_conn())
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))
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
/// `Playlist.kind` 在两个时刻/片段合集里不存在，所以那两个仓储传一个
/// 取值为 `Option<&str>` 的恒定 `None` 表达式 —— 占位符照常占位，绑的是
/// NULL，而那两列不存在、这段 SQL 根本不会被用到。
///
/// 写成 `""` 的话，SQL 会试图往一个**不存在的列**插值。
fn no_kind<C>(_new: &NewCollection<C>) -> Option<&'static str> {
    None
}

/// `Playlist` 的 `kind` 取值：未指定时留 `None`，让列的 DEFAULT 生效。
///
/// 空白串按「没指定」处理 —— `kind` 是 `varchar(64)` 而非枚举，写进空串
/// 会让 `Playlist::is_system()` 判 false，而那显然不是调用方的意图。
fn playlist_kind(new: &NewCollection<Playlist>) -> Option<&str> {
    new.kind.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

impl_collection_repo!(
    PlaylistRepository,
    Playlist,
    "playlist",
    NewCollection<Playlist>,
    ", kind",
    ", $5",
    playlist_kind
);

impl_collection_repo!(
    MomentCollectionRepository,
    MomentCollection,
    "moment_collection",
    NewCollection<MomentCollection>,
    "",
    "",
    no_kind::<MomentCollection>
);

impl_collection_repo!(
    ClipCollectionRepository,
    ClipCollection,
    "clip_collection",
    NewCollection<ClipCollection>,
    "",
    "",
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
    "SELECT COUNT(*) FROM playlist WHERE owner_plugin_id = $1",
    "SELECT * FROM playlist WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL \
     ORDER BY name LIMIT $2 OFFSET $3"
);

impl_paged!(
    MomentCollectionRepository,
    MomentCollection,
    "SELECT COUNT(*) FROM moment_collection",
    "SELECT * FROM moment_collection ORDER BY name LIMIT $1 OFFSET $2",
    "SELECT COUNT(*) FROM moment_collection WHERE owner_plugin_id = $1",
    "SELECT * FROM moment_collection WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL \
     ORDER BY name LIMIT $2 OFFSET $3"
);

impl_paged!(
    ClipCollectionRepository,
    ClipCollection,
    "SELECT COUNT(*) FROM clip_collection",
    "SELECT * FROM clip_collection ORDER BY name LIMIT $1 OFFSET $2",
    "SELECT COUNT(*) FROM clip_collection WHERE owner_plugin_id = $1",
    "SELECT * FROM clip_collection WHERE owner_plugin_id = $1 AND plugin_key IS NOT NULL \
     ORDER BY name LIMIT $2 OFFSET $3"
);
