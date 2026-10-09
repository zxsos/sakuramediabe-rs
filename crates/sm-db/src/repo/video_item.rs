//! `video_item` 表仓储。
//!
//! # 为什么它曾经是整个库最尴尬的一张缺口
//!
//! `media` 表上有一条 CHECK 约束：
//!
//! ```sql
//! CHECK ((movie_number IS NULL) <> (video_item_id IS NULL))
//! ```
//!
//! 一条媒体**必须**恰好归属 `movie.movie_number`（JAV 影片）或
//! `video_item.id`（非 JAV 影片）之一，两者都空或都非空都拒。
//! `MediaRepository::insert` 在 Rust 侧强制同一条规则。
//!
//! 所以在有本仓储之前，**非 JAV 媒体根本写不进去** —— JAV 影片能写是因为
//! `movie` 有仓储。这是一张叶子表，却卡住了一整类数据的入口。
//!
//! # 与 `movie` 的差异
//!
//! | | `movie` | `video_item` |
//! |---|---|---|
//! | 标题 | NOT NULL | NOT NULL，同样拒绝空白 |
//! | 唯一键 | `movie_number` | **无** |
//! | 索引 | 多个 | `title` 与 `release_date` 两条普通索引 |
//!
//! `video_item` **没有唯一约束**，所以「同一个非 JAV 影片被登记两次」数据库
//! 拦不住。调用方要么先按 `title` + `release_date` 查，要么接受重复。
//! 本仓储因此**不提供** `upsert` —— 拿不出一个正确的冲突键，提供了就是
//! 骗人。`insert` 之后想要幂等，调用方用 `find_by_title_and_date`。

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::videos::VideoItem;

/// `video_item` 的错误实体名。
const VIDEO_ITEM_ENTITY: &str = "VideoItem";

/// 新登记一个非 JAV 影片条目。
#[derive(Debug, Clone)]
pub struct NewVideoItem {
    /// 标题。空白按业务错误拒绝 —— 归一逻辑与上游
    /// `(title or "").strip()` 一致，但空标题本身没有意义。
    pub title: String,
    /// 简介。缺省空串（DDL 有 `DEFAULT ''`，且列是 `NOT NULL`）。
    pub summary: String,
    /// 封面图。指向 `image.id`。
    pub cover_image_id: Option<i32>,
    pub release_date: Option<chrono::NaiveDateTime>,
    /// `JsonTextField`，**可空**（与 `summary` 不同）。
    pub extra: Option<String>,
}

impl NewVideoItem {
    fn validate(&self) -> Result<(), DbError> {
        if VideoItem::normalize_title(Some(&self.title)).is_empty() {
            return Err(DbError::business(VIDEO_ITEM_ENTITY, "title 不能为空"));
        }
        Ok(())
    }
}

/// `video_item` 表仓储。
#[derive(Debug, Clone)]
pub struct VideoItemRepository {
    pool: PgPool,
}

impl VideoItemRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 插入。
    ///
    /// 归一规则全部来自模型上的 [`VideoItem::normalize_title`]，不重复实现
    /// —— 上游 `VideoItem.save` 的行为是 `.strip()` 后把 `None` 折叠成空串，
    /// 仓储若自己再写一遍 `trim`，两处就会在某个边界上分叉。
    pub async fn insert(&self, new: &NewVideoItem) -> Result<VideoItem, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, VideoItem>(
            "INSERT INTO video_item (title, summary, cover_image_id, release_date, \
                                     extra, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING *",
        )
        .bind(VideoItem::normalize_title(Some(&new.title)))
        .bind(new.summary.trim())
        .bind(new.cover_image_id)
        .bind(new.release_date)
        .bind(new.extra.as_deref())
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn insert_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        new: &NewVideoItem,
    ) -> Result<VideoItem, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, VideoItem>(
            "INSERT INTO video_item (title, summary, cover_image_id, release_date, \
                                     extra, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING *",
        )
        .bind(VideoItem::normalize_title(Some(&new.title)))
        .bind(new.summary.trim())
        .bind(new.cover_image_id)
        .bind(new.release_date)
        .bind(new.extra.as_deref())
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))
    }

    /// 按 id 查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<VideoItem>, DbError> {
        Ok(
            sqlx::query_as::<_, VideoItem>("SELECT * FROM video_item WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按标题与发布日期查一个条目。**这是本表的「幂等键」。**
    ///
    /// 表上**没有**唯一约束，所以「同一个非 JAV 影片被登记两次」数据库拦不住
    /// —— 上面那句「不提供 upsert」指的就是这个。调用方要幂等写入，就得
    /// 自己在应用层用这个查询先查一次。
    ///
    /// **刻意不保证唯一**：同一标题、同一天上映的两部不同影片是可能的
    /// （重拍、翻拍）。把 (title, release_date) 声明成唯一索引会让第二部
    /// 写不进去，而那比重复更糟。
    pub async fn find_by_title_and_date(
        &self,
        title: &str,
        release_date: Option<chrono::NaiveDateTime>,
    ) -> Result<Option<VideoItem>, DbError> {
        Ok(sqlx::query_as::<_, VideoItem>(
            "SELECT * FROM video_item \
             WHERE title = $1 AND release_date IS NOT DISTINCT FROM $2 \
             ORDER BY id LIMIT 1",
        )
        .bind(VideoItem::normalize_title(Some(title)))
        .bind(release_date)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 设置封面图。返回是否真的改了。
    ///
    /// **与「删图」不同**：删 `image` 不会级联到 `video_item.cover_image_id`，
    /// 所以这里显式提供一个「换封面」入口，而不是让调用方拼 UPDATE。
    pub async fn set_cover(&self, id: i32, cover_image_id: Option<i32>) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE video_item SET cover_image_id = $2, updated_at = $3 \
                                  WHERE id = $1",
        )
        .bind(id)
        .bind(cover_image_id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    paged_list! {
        /// 按标题搜索。**分页。**
        ///
        /// 走 `video_item_title_idx`。匹配语义是**子串**而不是前缀 ——
        /// 那是 `ILIKE` 的行为，普通 btree 索引用不上，但 `title` 的行数
        /// 与整库相比极小（几十到几千行），顺序扫描比走索引更快。
        pub async fn search_by_title(
            &self,
            title: &str,
        ) -> Result<Page<VideoItem>, DbError> {
            count = "SELECT COUNT(*) FROM video_item WHERE title ILIKE '%' || $1 || '%'",
            items = "SELECT * FROM video_item WHERE title ILIKE '%' || $1 || '%' \
                     ORDER BY title, id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 按发布日期倒序列出。**分页。**
        ///
        /// 走 `video_item_release_date_idx`。可传一个日期下界。
        ///
        /// **无日期的条目排在最后**，而不是被丢掉 ——
        /// 排序里显式写 `(release_date IS NULL), release_date DESC`：
        /// PostgreSQL 的 `DESC` 默认把 NULL 排在**前**面，那会让
        /// 「最近上映」列表的第一页被一堆无日期条目占满。
        pub async fn list_recent(
            &self,
            since: Option<chrono::NaiveDateTime>,
        ) -> Result<Page<VideoItem>, DbError> {
            count = "SELECT COUNT(*) FROM video_item \
                     WHERE ($1::timestamp IS NULL OR release_date >= $1)",
            items = "SELECT * FROM video_item \
                     WHERE ($1::timestamp IS NULL OR release_date >= $1) \
                     ORDER BY (release_date IS NULL), release_date DESC, id \
                     LIMIT $2 OFFSET $3",
        }
    }

    /// 列出某个非 JAV 影片下的全部媒体。**刻意不分页。**
    ///
    /// 「这个条目下有哪些文件」—— 详情页用。媒体数与整库相比极小，且调用方
    /// 几乎总是要全部（拼播放列表、算总时长），分页只会让每个调用方自己写
    /// 取完所有页的循环。
    pub async fn list_media(
        &self,
        video_item_id: i32,
    ) -> Result<Vec<crate::playback::media::Media>, DbError> {
        Ok(sqlx::query_as::<_, crate::playback::media::Media>(
            "SELECT * FROM media WHERE video_item_id = $1 ORDER BY id",
        )
        .bind(video_item_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 删掉一个条目，返回是否真的删了。
    ///
    /// 媒体的外键是 `CASCADE`（见 `media` 的 CHECK 与建表约束），所以条目
    /// 一删，它下面的媒体**也一并消失**。调用方若只想下架不想丢数据，得
    /// 自己先把媒体改挂到别的条目上。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM video_item WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
}
