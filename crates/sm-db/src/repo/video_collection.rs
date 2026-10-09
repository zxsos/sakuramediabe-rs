//! `video_collection` 与 `video_collection_item` 的仓储。
//!
//! # 为什么这两个不是 `collection.rs` 的第四、第五个实例
//!
//! [`crate::repo::collection`] 里的宏假定父表实现 [`PluginOwned`](crate::collections::PluginOwned)
//! —— `owner_plugin_id` 与 `plugin_key` 两列，合起来构成唯一索引
//! `(owner_plugin_id, plugin_key)`。`video_collection` **没有**这两列：
//!
//! ```text
//! video_collection: id, created_at, updated_at, name, description
//! ```
//!
//! 所以它不实现 `PluginOwned`，也就没有 `list_plugin_owned` /
//! `find_by_plugin_key` / 半配置校验那套东西。硬套宏会写出一段引用不存在
//! 列的 SQL —— 那正是本仓库此前反复出现的缺陷形状。
//!
//! 成员表则**可以**复用 [`impl_ordered_member_repo!`](crate::impl_ordered_member_repo)：`position` 的语义、
//! 唯一索引 `(collection_id, video_item_id)`、`unlink` 不重排，三者与
//! `moment_collection_item` / `clip_collection_item` 完全一致。
//!
//! # `position` 有 DEFAULT 0，另两个合集没有
//!
//! ```text
//! video_collection_item.position integer NOT NULL DEFAULT 0
//! moment_collection_item.position integer NOT NULL
//! clip_collection_item.position integer NOT NULL
//! ```
//!
//! 后果是**省略 position 时行为不同**：本表的 `append` 若不显式给值，
//! 数据库会填 0，于是「追加第二个成员」会与第一个**并列**在位置 0。
//! 本仓储的 `append` 因此总是显式算 `max(position) + 1`，不依赖那个
//! DEFAULT —— 让「不写就并排」成为需要读 DDL 才发现的陷阱没有意义。
//!
//! # 索引够不够
//!
//! ```text
//! UNIQUE (collection_id, video_item_id)
//! INDEX  (position)
//! ```
//!
//! 而热查询是 `WHERE collection_id = $1 ORDER BY position, id`。两条索引都
//! **不**完全贴合：前者按 `video_item_id` 排，后者没有 `collection_id`。
//! 正确的索引是 `(collection_id, position)`。表很小（一个用户的合集成员
//! 通常几十行），所以现状不影响性能，但这是**上游 schema 的一个真实缺口**
//! —— 记在这里而不是私自改 DDL，因为 DDL 必须与上游逐字节一致。

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::repo::Ctx;
use crate::videos::{VideoCollection, VideoCollectionItem};

const VIDEO_COLLECTION_ENTITY: &str = "VideoCollection";

/// `video_collection_item` 的错误实体名。
///
/// 单独一个常量而不是在宏调用点再写一次字面量：本文件的 `impl` 块（成员分页）
/// 与宏展开都要用它，两处写死就会有一天对不上。
const VIDEO_COLLECTION_ITEM_ENTITY: &str = "VideoCollectionItem";

/// 新建一个视频合集。
#[derive(Debug, Clone)]
pub struct NewVideoCollection {
    /// 全局唯一。
    pub name: String,
    /// 简介。DDL 是 `text NOT NULL DEFAULT ''`，所以缺省空串而非 `None`。
    pub description: String,
}

impl NewVideoCollection {
    fn validate(&self) -> Result<(), DbError> {
        if VideoCollection::normalize_name(Some(&self.name)).is_empty() {
            return Err(DbError::business(VIDEO_COLLECTION_ENTITY, "name 不能为空"));
        }
        Ok(())
    }
}

/// `video_collection` 表仓储。
///
/// 方法比 [`crate::repo::collection::PlaylistRepository`] 少三组 ——
/// 全部是插件归属相关的，因为这张表**没有**那两列。
#[derive(Debug, Clone)]
pub struct VideoCollectionRepository {
    pool: PgPool,
}

impl VideoCollectionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 插入。`name` 全局唯一。
    pub async fn insert(&self, new: &NewVideoCollection) -> Result<VideoCollection, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, VideoCollection>(
            "INSERT INTO video_collection (name, description, created_at, updated_at) \
             VALUES ($1, $2, $3, $3) RETURNING *",
        )
        .bind(VideoCollection::normalize_name(Some(&new.name)))
        .bind(new.description.trim())
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))
    }

    /// 事务内变体，供 [`Ctx`] 编排时使用。
    pub async fn insert_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        new: &NewVideoCollection,
    ) -> Result<VideoCollection, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, VideoCollection>(
            "INSERT INTO video_collection (name, description, created_at, updated_at) \
             VALUES ($1, $2, $3, $3) RETURNING *",
        )
        .bind(VideoCollection::normalize_name(Some(&new.name)))
        .bind(new.description.trim())
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))
    }

    /// 按 id 查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<VideoCollection>, DbError> {
        Ok(
            sqlx::query_as::<_, VideoCollection>("SELECT * FROM video_collection WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按名查询。走 `name` 的唯一约束。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<VideoCollection>, DbError> {
        Ok(
            sqlx::query_as::<_, VideoCollection>("SELECT * FROM video_collection WHERE name = $1")
                .bind(VideoCollection::normalize_name(Some(name)))
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 列出全部合集，按 `updated_at DESC, id DESC`。**刻意不分页。**
    ///
    /// # 为什么不复用下面那个分页的 `list()`
    ///
    /// 排序键不同。分页的 `list()` 按 `name`（与唯一索引一致，利于分页稳定）；
    /// 上游 `GET /video-collections` 返回的是 `list[...]` 而非 `PageResponse`，
    /// 且顺序是「最近动过的在前」—— **增删成员都会 touch `updated_at`**。
    /// 按 `name` 排会让「刚加过成员的合集」停在字母原处，与上游可见的顺序不符。
    ///
    /// # `updated_at` 可空，而 DESC 在 PostgreSQL 里是 NULLS FIRST
    ///
    /// 从未被 touch 过的合集会排到最前面。看着违反直觉，但**与上游一致**
    /// （上游 `.updated_at.desc()` 落到 PG 上是同一串 SQL）。刻意不补
    /// `NULLS LAST`：合集顺序是客户端会缓存并做乐观更新的状态。
    ///
    /// `id DESC` 作次级键：同一毫秒内被 touch 的两个合集 `updated_at` 会并列，
    /// 只按它排会让列表在两次刷新间抖动。
    pub async fn list_ordered_by_recency(&self) -> Result<Vec<VideoCollection>, DbError> {
        Ok(sqlx::query_as::<_, VideoCollection>(
            "SELECT * FROM video_collection ORDER BY updated_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 列出全部合集。**分页。**
        ///
        /// 走 `name` 的唯一约束（`btree` 索引），排序 `name` 与之一致 ——
        /// 所以这里的 `ORDER BY` 不需要额外排序步骤。
        ///
        /// 上游另建了一条 `video_collection_name_idx`，而 `name` 已经有
        /// `UNIQUE` 约束自带 btree 索引 —— 那条索引是**冗余的**，写入时
        /// 每个新行都要多维护一份。记在这里，不私自改 DDL（必须与上游
        /// 逐字节一致）。
        pub async fn list(
            &self,
        ) -> Result<Page<VideoCollection>, DbError> {
            count = "SELECT COUNT(*) FROM video_collection",
            items = "SELECT * FROM video_collection ORDER BY name LIMIT $1 OFFSET $2",
        }
    }

    /// 合集计数：`(合集数, 成员行数)`。**给状态页用。**
    ///
    /// 两条标量查询而不是走 `Page` —— 状态页只要总数，不要内容。
    pub async fn collection_counts(&self) -> Result<(i64, i64), DbError> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM video_collection")
            .fetch_one(&self.pool)
            .await?;
        let items = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM video_collection_item")
            .fetch_one(&self.pool)
            .await?;
        Ok((count, items))
    }

    paged_list! {
        /// 按名字搜索（子串）。**分页。**
        ///
        /// 与 [`Self::list`] 分开而不是加一个可选参数：前者的排序键与索引
        /// 一致，后者是 `ILIKE` 子串匹配，两条查询的代价与索引使用完全不同。
        /// 合成一个带 `Option<String>` 的方法会让「传了 None 时走哪条路」
        /// 成为调用方要记住的隐含约定。
        ///
        /// `ILIKE` 子串匹配用不上 btree 索引，但 `video_collection` 的行数
        /// 与整库相比极小（一个用户通常几个到几十个），顺序扫描更快。
        pub async fn search_by_name(
            &self,
            keyword: &str,
        ) -> Result<Page<VideoCollection>, DbError> {
                count = "SELECT COUNT(*) FROM video_collection WHERE name ILIKE '%' || $1 || '%'",
                items = "SELECT * FROM video_collection WHERE name ILIKE '%' || $1 || '%' \
                         ORDER BY name LIMIT $2 OFFSET $3",
            }
    }

    /// 改名。返回是否真的改了。
    ///
    /// 空白名按业务错误拒绝 —— 与 `insert` 同一把尺子。
    pub async fn rename(&self, id: i32, name: &str) -> Result<bool, DbError> {
        let name = VideoCollection::normalize_name(Some(name));
        if name.is_empty() {
            return Err(DbError::business(VIDEO_COLLECTION_ENTITY, "name 不能为空"));
        }
        let result = sqlx::query(
            "UPDATE video_collection SET name = $2, updated_at = $3 \
                                  WHERE id = $1",
        )
        .bind(id)
        .bind(&name)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 一次性更新名称与/或简介。**一条 UPDATE，只推进一次 `updated_at`。**
    ///
    /// `name` / `description` 各自是「`None` = 不动这一列」：
    ///
    /// | 传入 | 结果 |
    /// |---|---|
    /// | `(None, None)` | 不发语句，返回 `Ok(false)` |
    /// | `(Some("新名"), None)` | 只改名字 |
    /// | `(Some("新名"), Some(""))` | 名字与简介都改（空串是合法的「清空简介」） |
    ///
    /// # 为什么不拆成 `rename` + `set_description` 两次调用
    ///
    /// 上游 `update_collection` 是「逐个字段赋值 → 一次 `save()`」，
    /// `updated_at` 只被赋值一次。拆成两次 UPDATE 会写两个时间戳，而
    /// `list_collections` 的默认排序正是 `updated_at DESC` —— 两次写入之间
    /// 读到的列表顺序会与上游不同。
    ///
    /// 空白名按业务错误拒绝，与 [`Self::insert`] 同一把尺子。
    pub async fn update(
        &self,
        id: i32,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, DbError> {
        if name.is_none() && description.is_none() {
            return Ok(false);
        }
        if let Some(name) = name {
            if VideoCollection::normalize_name(Some(name)).is_empty() {
                return Err(DbError::business(VIDEO_COLLECTION_ENTITY, "name 不能为空"));
            }
        }
        let result = sqlx::query(
            "UPDATE video_collection SET \
                 name = COALESCE($2, name), \
                 description = COALESCE($3, description), \
                 updated_at = $4 \
             WHERE id = $1",
        )
        .bind(id)
        .bind(name.map(|n| VideoCollection::normalize_name(Some(n))))
        .bind(description.map(str::trim))
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 只推进 `updated_at`，不动其它列。返回是否真的改了。
    ///
    /// 对应上游 `_touch_collection`：成员增删与重排都要让合集在
    /// 「最近活跃」里上浮，而那些操作本身不碰父表的其它列。
    pub async fn touch(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("UPDATE video_collection SET updated_at = $2 WHERE id = $1")
            .bind(id)
            .bind(crate::common::time::now_utc())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// [`Self::touch`] 的事务内变体。
    ///
    /// 「改成员 + touch 父表」必须原子：否则中间那一瞬父表的 `updated_at`
    /// 还是旧的，而成员已经变了 —— 合集列表的排序会与成员状态不一致。
    pub async fn touch_in(&self, ctx: &mut crate::repo::Ctx<'_>, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("UPDATE video_collection SET updated_at = $2 WHERE id = $1")
            .bind(id)
            .bind(crate::common::time::now_utc())
            .execute(ctx.conn().await?.as_conn())
            .await
            .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 删掉一个合集，返回是否真的删了。
    ///
    /// 成员行随外键 `CASCADE` 一并消失 —— 「保留合集、只清成员」用
    /// [`VideoCollectionItemRepository::clear`]。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM video_collection WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
}

crate::impl_ordered_member_repo!(
    VideoCollectionItemRepository,
    VideoCollectionItem,
    "video_collection_item",
    "video_item_id",
    VIDEO_COLLECTION_ITEM_ENTITY
);

/// 合集成员列表的排序键 → `ORDER BY` 片段。**白名单。**
///
/// 与 `sm_service::videos::VideoSort::as_str()` 逐字对应，且比
/// [`super::video_item::VIDEO_LIST_SORT_FIELD_MAP`] **多一个 `position`**
/// —— 那是成员表自己的列，只有这条路径有（上层的
/// `COLLECTION_ITEM_SORT_KEYS` 与 `ITEM_SORT_KEYS` 就是这个差别）。
///
/// `duration` / `file_size` 两个片段**直接复用**视频那边的常量：它们只看
/// `v.id` 与 `media` 表，与这边多出来的 `vci` 联结无关。抄一份会分叉。
pub const VIDEO_COLLECTION_ITEM_SORT_FIELD_MAP: [(&str, &str); 5] = [
    ("position", "vci.position"),
    ("created_at", "v.created_at"),
    ("title", "v.title"),
    (
        "duration",
        super::video_item::VIDEO_FIRST_MEDIA_DURATION_COLUMN,
    ),
    (
        "file_size",
        super::video_item::VIDEO_FIRST_MEDIA_FILE_SIZE_COLUMN,
    ),
];

/// 取排序片段。不在白名单 → `None`（调用方应报 422）。
pub fn video_collection_item_sort_column(key: &str) -> Option<&'static str> {
    VIDEO_COLLECTION_ITEM_SORT_FIELD_MAP
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, column)| *column)
}

impl VideoCollectionItemRepository {
    /// 合集成员分页，按给定排序键。返回 `(本页成员行, 总数)`。
    ///
    /// # 为什么要 JOIN `video_item`
    ///
    /// `title` / `created_at` / `duration` / `file_size` 四个排序键都在
    /// **条目**那张表上（或它的媒体上）。只按 `position` 排的话不 JOIN 也行，
    /// 但那样就得为两种排序写两条路径 —— 一条 JOIN 覆盖全部五种，更省。
    ///
    /// # 排序键必须来自 [`VIDEO_COLLECTION_ITEM_SORT_FIELD_MAP`]
    ///
    /// 不在白名单 → **运行时** `Err`，不是 `debug_assert!`（release 下会被
    /// 编译掉，那这份白名单就等于不存在，而它挡的是 SQL 注入）。
    ///
    /// 次序稳定项是 **`vci.id`**（不是 `v.id`）—— 上游 `_query_item_resources`
    /// 的 `tie_breaker=VideoCollectionItem.id` 就是这个。同一集可能被加入
    /// 多个合集，用 `v.id` 会在跨合集时不再唯一。
    pub async fn list_page_with_video(
        &self,
        collection_id: i32,
        sort_key: &str,
        descending: bool,
        request: &PageRequest,
    ) -> Result<(Vec<VideoCollectionItem>, i64), DbError> {
        let column = video_collection_item_sort_column(sort_key).ok_or_else(|| {
            DbError::business(
                VIDEO_COLLECTION_ITEM_ENTITY,
                format!("未知的排序键：{sort_key:?}"),
            )
        })?;
        let direction = if descending { "DESC" } else { "ASC" };

        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM video_collection_item WHERE collection_id = $1",
        )
        .bind(collection_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ITEM_ENTITY))?;

        let sql = format!(
            "SELECT vci.* FROM video_collection_item vci \
             JOIN video_item v ON v.id = vci.video_item_id \
             WHERE vci.collection_id = $1 \
             ORDER BY {column} {direction}, vci.id {direction} LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query_as::<_, VideoCollectionItem>(super::movie::safe_sql(sql))
            .bind(collection_id)
            .bind(request.limit())
            .bind(request.offset())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(VIDEO_COLLECTION_ITEM_ENTITY))?;
        Ok((rows, total))
    }
}

/// 只在 `video_collection_item` 上存在的三个成员操作。
///
/// # 为什么没有进 [`crate::impl_ordered_member_repo`]
///
/// 宏是给「moment / clip / video 三张同构表」共用的。逐条核对了上游：
///
/// | 操作 | moment | clip | video |
/// |---|---|---|---|
/// | 按**成员** id 移除（`unlink`） | ✅ `remove_point` | ✅ `remove_clip` | ✅ `remove_items_by_video_ids` |
/// | 按**关联行** id 移除 | ❌ | ❌ | ✅ `remove_item` |
/// | 批量按成员 id 移除 | ❌ | ❌ | ✅ `remove_items_by_video_ids` |
/// | 重排（改 `position`） | ❌ | ❌ | ✅ `reorder_items` |
///
/// 也就是说后三个是 videos 独有。放进宏会给另外两张表留下**永不调用的
/// 方法** —— 那正是本仓库文档里反复批评的「幽灵配置」，只是换成了方法。
///
/// # 「关联行 id」与「成员 id」是两个不同的东西
///
/// ```text
/// video_collection_item:  id（关联行） │ collection_id │ video_item_id（成员） │ position
///                                ▲                        ▲
///                        API 的 item_id            API 的 video_item_id
/// ```
///
/// 上游 `remove_item(collection_id, item_id)` 删的是**前者**。两者的取值
/// 空间都从 1 开始、外观完全一样，所以混淆不会报错、只会删掉错的行 ——
/// 这也是本文件要把两个方法命名成 `unlink_by_link_id` 与
/// `unlink_by_member_ids` 而不是两个 `unlink` 的原因。
impl VideoCollectionItemRepository {
    /// 按**关联行 id** 移除一个成员。返回是否真的删掉了一行。
    ///
    /// `collection_id` 一并进 `WHERE`：URL 里的合集 id 与成员 id 必须属于
    /// 同一个合集，否则「从 A 合集删 B 的成员」会静默成功。
    pub async fn unlink_by_link_id(
        &self,
        collection_id: i32,
        link_id: i32,
    ) -> Result<bool, DbError> {
        let result =
            sqlx::query("DELETE FROM video_collection_item WHERE collection_id = $1 AND id = $2")
                .bind(collection_id)
                .bind(link_id)
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity("VideoCollectionItem"))?;
        Ok(result.rows_affected() > 0)
    }

    /// 事务内按**成员 id** 批量移除。返回删掉了几行。
    ///
    /// `member_ids` 允许含重复：`= ANY($2)` 对重复值不敏感，调用方不必
    /// 先去重。上游用 `dict.fromkeys` 去重是为了让 IN 列表短一点，不是
    /// 为了语义。
    ///
    /// 空数组直接返回 0 而不发语句 —— PostgreSQL 的 `= ANY('{}')` 恒为
    /// false（不是 true），语义恰好也对，但发一条无意义的 DELETE 总归是
    /// 浪费一次往返。
    pub async fn unlink_by_member_ids_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        collection_id: i32,
        member_ids: &[i32],
    ) -> Result<u64, DbError> {
        if member_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "DELETE FROM video_collection_item \
             WHERE collection_id = $1 AND video_item_id = ANY($2)",
        )
        .bind(collection_id)
        .bind(member_ids)
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity("VideoCollectionItem"))?;
        Ok(result.rows_affected())
    }

    /// 事务内改一个成员的 `position`。返回是否真的改了。
    ///
    /// `collection_id` 进 `WHERE` 是防御性的：调用方（重排）已经校验过
    /// 「给出的 id 恰好覆盖本合集全部成员」，正常路径下这个条件恒真。
    /// 留着它是为了让「行不存在」与「行属于别的合集」返回同一个 `false`，
    /// 而不是把越权写当成成功。
    ///
    /// **不重排**其余成员的 `position` —— 重排是调用方一次性写全量的语义。
    pub async fn set_position_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        collection_id: i32,
        link_id: i32,
        position: i32,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE video_collection_item \
             SET position = $3, updated_at = $4 \
             WHERE collection_id = $1 AND id = $2",
        )
        .bind(collection_id)
        .bind(link_id)
        .bind(position)
        .bind(crate::common::time::now_utc())
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity("VideoCollectionItem"))?;
        Ok(result.rows_affected() > 0)
    }
}
