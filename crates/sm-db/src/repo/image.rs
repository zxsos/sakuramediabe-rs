//! `image` 表仓储。
//!
//! # 为什么它排在 `video_item` 之前
//!
//! 两处都硬性依赖这张表：
//!
//! - `media_point.image_id` 是 `NOT NULL` 且**无 DEFAULT**，所以「给时刻配一张
//!   图」在有仓储之前只能手写 SQL。
//! - `video_item.cover_image_id` 指向 `image.id`。
//!
//! # `origin` 是 `varchar(255) NOT NULL UNIQUE`，所以插入是 upsert
//!
//! 同一张原图会被反复登记：刮削任务重跑、同一部影片入库两次、缩略图重新
//! 生成。那不是错误，是**同一张图**。`ON CONFLICT DO UPDATE` 让「按 origin
//! 拿 id」成为幂等操作 —— 调用方不必先查再决定插还是更新。
//!
//! # 前缀查询：`text_pattern_ops` 索引的用途
//!
//! 上游给 `origin` 建了一个 `text_pattern_ops` 索引：
//!
//! ```sql
//! CREATE INDEX image_origin_pattern ON image (origin text_pattern_ops);
//! ```
//!
//! 那是给 `LIKE 'prefix%'` 用的 —— 树是按**完整字符串**排序的，所以
//! `WHERE origin = $1` 走那条唯一索引就够，而前缀匹配只能靠这条。演员的
//! 所有图片、影片的所有剧照，都按「同目录 + 同前缀」成组出现
//! （`actors/ABD-001/1.jpg`、`ABD-001/2.jpg`……），所以这是热路径。
//!
//! **这条索引此前是为一个不存在的查询建的** —— 没有任何仓储做前缀查询。

use sqlx::PgPool;

use crate::catalog::asset::Image;
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

/// `image` 表的错误实体名。
const IMAGE_ENTITY: &str = "Image";

/// 新登记一张图片。
#[derive(Debug, Clone)]
pub struct NewImage {
    /// 原图路径（相对 media 根目录）。**必填且唯一。**
    pub origin: String,
}

impl NewImage {
    fn validate(&self) -> Result<(), DbError> {
        if self.origin.trim().is_empty() {
            return Err(DbError::business(IMAGE_ENTITY, "origin 不能为空"));
        }
        Ok(())
    }
}

/// `image` 表仓储。
///
/// 全表只有三列（`id` / `origin` / 时间戳），所以方法很少 —— 这不是
/// 遗漏，是表的形状就那样。
#[derive(Debug, Clone)]
pub struct ImageRepository {
    pool: PgPool,
}

impl ImageRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按 `origin` 登记图片，返回 `(id, 本次是否新建)`。
    ///
    /// **幂等。** `origin` 唯一，所以重复登记同一张图走
    /// `ON CONFLICT DO UPDATE` 而不是报错。
    ///
    /// 冲突时更新 `updated_at` 而**不**碰 `origin`（它是冲突键，改了就不是
    /// 「同一张图」了）。返回值里的 `false` 让调用方能区分「刚建的」与
    /// 「早就有的」—— 后者意味着磁盘上的文件可能已经不存在，需要校验。
    pub async fn upsert(&self, new: &NewImage) -> Result<(i32, bool), DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        let origin = new.origin.trim();
        // `xmax = 0` 是 PostgreSQL 判断「这行是本次插入的」的惯用技巧。
        // 比先查再写少一次往返，也不会在并发下把两次插入都报成「新建」。
        let row: (i32, bool) = sqlx::query_as(
            "INSERT INTO image (origin, created_at, updated_at) VALUES ($1, $2, $2) \
             ON CONFLICT (origin) DO UPDATE SET updated_at = EXCLUDED.updated_at \
             RETURNING id, xmax = 0",
        )
        .bind(origin)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(IMAGE_ENTITY))?;
        Ok(row)
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn upsert_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        new: &NewImage,
    ) -> Result<(i32, bool), DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        let origin = new.origin.trim();
        let row: (i32, bool) = sqlx::query_as(
            "INSERT INTO image (origin, created_at, updated_at) VALUES ($1, $2, $2) \
             ON CONFLICT (origin) DO UPDATE SET updated_at = EXCLUDED.updated_at \
             RETURNING id, xmax = 0",
        )
        .bind(origin)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(IMAGE_ENTITY))?;
        Ok(row)
    }

    /// 按 `origin` 取一张图。走 `origin` 的唯一索引。
    pub async fn find_by_origin(&self, origin: &str) -> Result<Option<Image>, DbError> {
        Ok(
            sqlx::query_as::<_, Image>("SELECT * FROM image WHERE origin = $1")
                .bind(origin.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按 id 取一张图。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<Image>, DbError> {
        Ok(
            sqlx::query_as::<_, Image>("SELECT * FROM image WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 把前缀转成 `LIKE` 模式里**按字面匹配**的形式。
    ///
    /// `_` 与 `%` 是 LIKE 的通配符，而 `origin` 是文件路径 —— 目录名里出现
    /// `_` 极其常见（`ABD-001_1080p`、`scene_01`）。不转义的话
    /// `LIKE 'actors/ABD-001_%'` 会把 `ABD-001X` 也匹配进来，而那是个
    /// **不同的**演员。
    ///
    /// 转义顺序有讲究：先 `\` 自己，再 `%` 与 `_`。反过来会把自己刚加的
    /// 转义符再转义一次。
    ///
    /// ```ignore
    /// let pattern = ImageRepository::like_prefix("actors/ABD-001_");
    /// let page = repo.list_by_origin_pattern(&pattern).await?;
    /// ```
    pub fn like_prefix(prefix: &str) -> String {
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("{escaped}%")
    }

    paged_list! {
        /// 按 `LIKE` 模式列出图片。**分页。**
        ///
        /// 走 `image_origin_pattern`（`text_pattern_ops`）—— 那条索引只对
        /// 前缀模式有用，所以这里的条件必须是 `LIKE '...%'` 而不是
        /// `WHERE origin = $1`（后者走唯一索引即可，用不上这条）。
        ///
        /// **参数是完整的 LIKE 模式，不是裸前缀。** 用
        /// [`Self::like_prefix`] 构造它 —— 目录名里的 `_` 是通配符，
        /// 不转义会匹配到别的演员/影片。
        ///
        /// 需要按完整路径查单张图，用 [`Self::find_by_origin`]，那条走唯一
        /// 索引，与前缀扫描是两回事。
        pub async fn list_by_origin_pattern(
            &self,
            pattern: &str,
        ) -> Result<Page<Image>, DbError> {
            count = "SELECT COUNT(*) FROM image WHERE origin LIKE $1 ESCAPE '\\'",
            items = "SELECT * FROM image WHERE origin LIKE $1 ESCAPE '\\' \
                     ORDER BY origin LIMIT $2 OFFSET $3",
        }
    }

    /// 按 `origin` 升序列出全部图片。**刻意不分页。**
    ///
    /// 全表导出用。分页在这里没有意义 —— 唯一会想要这个方法的场景是备份，
    /// 而备份不该一页一页地取。
    pub async fn list_all(&self) -> Result<Vec<Image>, DbError> {
        Ok(
            sqlx::query_as::<_, Image>("SELECT * FROM image ORDER BY origin")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// 删掉一张图，返回是否真的删了。**调用方需自行处理引用。**
    ///
    /// 「需自行处理」不是推卸：`movie.cover_image_id`、
    /// `video_item.cover_image_id`、`media_point.image_id`、
    /// `image_search_session.thumbnail_id` 都指向这张表，而外键的
    /// `on_delete` 各不相同（有的 SET NULL，有的 CASCADE，有的 RESTRICT）。
    /// 仓储猜不出调用方想保留哪一个 —— 要级联就把整组写入包进
    /// [`Ctx`](crate::repo::Ctx) 一个事务。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM image WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(IMAGE_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
}
