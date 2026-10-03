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
    "VideoCollectionItem"
);
