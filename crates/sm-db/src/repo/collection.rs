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

use crate::collections::{
    ClipCollection, ClipCollectionItem, MomentCollection, MomentCollectionItem, Playlist,
    PlaylistMovie, PluginOwned,
};
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

use super::ctx::Ctx;

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
        $repo:ident, $model:ty, $table:literal, $member:literal, $entity:literal
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

    /// 改描述，返回是否真的改了。
    ///
    /// 与 [`Self::rename`] 分开而不是一个 `update` —— 上游
    /// `update_playlist` 是「局部可更新，未传的字段保持原值」。一个全字段
    /// 写入口会把「只改描述」变成「连名字一起重写」，在并发下覆盖别人的
    /// 改名。
    pub async fn set_description(&self, id: i32, description: &str) -> Result<bool, DbError> {
        let result =
            sqlx::query("UPDATE playlist SET description = $2, updated_at = $3 WHERE id = $1")
                .bind(id)
                .bind(description.trim())
                .bind(crate::common::time::now_utc())
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 改名，返回是否真的改了。空白名按业务错误拒绝。
    ///
    /// `name` 是唯一索引，所以重名会撞约束 —— 唯一性判断留给 service 层
    /// （它能区分「保留名」与「已被占用」，而仓储不能）。
    pub async fn rename(&self, id: i32, name: &str) -> Result<bool, DbError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DbError::business(COLLECTION_ENTITY, "name 不能为空"));
        }
        let result = sqlx::query("UPDATE playlist SET name = $2, updated_at = $3 WHERE id = $1")
            .bind(id)
            .bind(name)
            .bind(crate::common::time::now_utc())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
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
}
