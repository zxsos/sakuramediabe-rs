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

use std::collections::HashMap;

use sqlx::PgPool;

use crate::catalog::asset::Image;
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;

/// `image` 表的错误实体名。
const IMAGE_ENTITY: &str = "Image";

/// **全部**指向 `image.id` 的外键列，`表.列` 形式。
///
/// # 这份清单必须与 DDL 完全一致
///
/// 它是「这张图还有没有人用」那个判据的全部依据（见 [`ImageRepository::is_referenced`]）。
/// 漏一个的后果**不是报错**，是删掉一张正在被引用的图 —— 表现为「封面忽然
/// 裂了」，而且已经删掉的记录回滚不回来。
///
/// 所以 `sm-service` 里有一条集成测试直接读 `information_schema` 的外键约束、
/// 与这份清单对拍（`tests/image_reference_sites.rs`）。**加表加列时它会红，
/// 而不是等线上裂图。**
///
/// # 骨架期这份清单是错的（漏了三处，还写错了表名）
///
/// 原先只有五项，且第五项写成 `plot_image.image_id` —— **没有 `plot_image`
/// 这张表**，真名是 `movie_plot_image`（`movie_plot_image_image_id_fk`）。
/// 漏掉的三处是：`actor.profile_image_override_id`、
/// **`media_point.image_id`**、`video_item.cover_image_id`。
///
/// 中间那个尤其值得记：`media_point.image_id` 是 `NOT NULL` + `RESTRICT`，
/// 也就是「每个时刻点都钉着一张图」，漏掉它等于**每次删时刻点都会顺手删掉
/// 那张图**，而时刻点本身还在 —— 一行破外键。
pub const IMAGE_REFERENCE_SITES: [&str; 8] = [
    "movie.cover_image_id",
    "movie.thin_cover_image_id",
    "actor.profile_image_id",
    "actor.profile_image_override_id",
    "movie_plot_image.image_id",
    "media_thumbnail.image_id",
    "media_point.image_id",
    "video_item.cover_image_id",
];

/// 「这张图还被引用着吗」。**只此一份。**
///
/// [`ImageRepository::is_referenced`] 与
/// [`ImageRepository::delete_if_unreferenced`] 共用它 —— 那两处必须是同一个
/// 判据，否则「查着没人用」与「删时没人用」会分叉。
///
/// 与 [`IMAGE_REFERENCE_SITES`] 逐条对应（八个 `EXISTS`，两个双列的表各占一个）。
const IMAGE_REFERENCED_SQL: &str = "SELECT \
     EXISTS (SELECT 1 FROM movie WHERE cover_image_id = $1 OR thin_cover_image_id = $1) \
     OR EXISTS (SELECT 1 FROM actor WHERE profile_image_id = $1 OR profile_image_override_id = $1) \
     OR EXISTS (SELECT 1 FROM movie_plot_image WHERE image_id = $1) \
     OR EXISTS (SELECT 1 FROM media_thumbnail WHERE image_id = $1) \
     OR EXISTS (SELECT 1 FROM media_point WHERE image_id = $1) \
     OR EXISTS (SELECT 1 FROM video_item WHERE cover_image_id = $1)";

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

    /// 按 id **批量**取图，返回 `id -> Image` 映射。
    ///
    /// 存在的理由是片段列表的封面：一次要解析几十张缩略图对应的
    /// `image_id`，逐个 `find_by_id` 就是 N+1。
    ///
    /// # 缺项不出现在结果里
    ///
    /// 返回的是映射而不是 `Vec`，所以「没有这张图」与「有这张图」的区别由
    /// **键是否存在**表达，调用方不需要 `Option` 套 `Option`。
    /// `image_id` 是外键且 NOT NULL，所以理论上不该缺 —— 但外键约束可以被
    /// 关掉或延迟，而这里静默少一张封面比报错更容易排查。
    ///
    /// 空输入直接返回空映射，不发查询。
    pub async fn find_by_ids(&self, ids: &[i32]) -> Result<HashMap<i32, Image>, DbError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, Image>("SELECT * FROM image WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|image| (image.id, image)).collect())
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

    /// 按 `LIKE` 模式列出 `origin`。**不分页、只取这一列、按 `origin` 升序。**
    ///
    /// 上游 `movie_asset_pack_service.live_origins` 的查询部分：一次要拿到某个
    /// 影片目录下**全部**图片的 origin（几十个），分页在这里没有意义 ——
    /// 调用方要的是完整集合（拿它决定包里该有哪些条目）。
    ///
    /// 走 `image_origin_pattern`（`text_pattern_ops`），理由同
    /// [`Self::list_by_origin_pattern`]：**参数是完整的 LIKE 模式**，用
    /// [`Self::like_prefix`] 构造，目录名里的 `_` 不转义会匹配到别的影片。
    ///
    /// 排序是**语义要求**而不是好看：包的内容要能按字节稳定复现（重建后比对、
    /// 缓存校验），所以入包顺序必须确定。
    pub async fn list_origins_by_pattern(&self, pattern: &str) -> Result<Vec<String>, DbError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT origin FROM image WHERE origin LIKE $1 ESCAPE '\\' ORDER BY origin",
        )
        .bind(pattern)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(origin,)| origin).collect())
    }

    /// 这张图是否仍被任何一张表引用。上游 `image_record_is_still_used`。
    ///
    /// 一次往返问完全部 [`IMAGE_REFERENCE_SITES`]（不是逐个 `EXISTS` 发查询）
    /// —— 判据在删除路径上被调用，往返次数就是延迟。
    ///
    /// ⚠️ **存在过的 id 与不存在的 id 都返回 `false`。**「没人引用」与
    /// 「这张图根本不存在」在这层是同一件事，调用方若需要区分要先
    /// [`Self::find_by_id`]。
    pub async fn is_referenced(&self, image_id: i32) -> Result<bool, DbError> {
        let referenced: bool = sqlx::query_scalar(IMAGE_REFERENCED_SQL)
            .bind(image_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(referenced)
    }

    /// 已无人引用就删掉记录，返回被删的 `origin`；仍被引用或不存在时 `None`。
    ///
    /// 上游 `delete_image_record_if_unused` 的仓储部分。
    ///
    /// # 为什么「查」与「删」必须在同一个事务里
    ///
    /// 分两步（先 [`Self::is_referenced`] 再 `DELETE`）有一个窗口：查的时候
    /// 没人用，删之前另一处刚把这张图挂上。而外键里只有 `media_point` 那条是
    /// `RESTRICT`（会拦），其余是 `SET NULL` / `CASCADE`（**不拦**）——
    /// 于是要么删掉一张正在用的图，要么把别人的封面悄悄置空。
    ///
    /// `SELECT ... FOR UPDATE` 钉住这一行：新建外键引用需要对被引用行取
    /// `FOR KEY SHARE` 锁，两者**互斥**，所以并发的「把这张图挂上去」会等在
    /// 这里直到本事务结束。
    pub async fn delete_if_unreferenced(&self, image_id: i32) -> Result<Option<String>, DbError> {
        let mut tx = self.pool.begin().await?;

        // 1. 钉住这一行（不存在就直接返回，别把 DELETE 空跑一遍）。
        let existing: Option<(i32,)> =
            sqlx::query_as("SELECT id FROM image WHERE id = $1 FOR UPDATE")
                .bind(image_id)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_none() {
            return Ok(None);
        }

        // 2. 同一个事务里查引用方。
        let referenced: bool = sqlx::query_scalar(IMAGE_REFERENCED_SQL)
            .bind(image_id)
            .fetch_one(&mut *tx)
            .await?;
        if referenced {
            return Ok(None);
        }

        // 3. 删记录并取回 origin（调用方要拿它去删磁盘文件）。
        let deleted: Option<(String,)> =
            sqlx::query_as("DELETE FROM image WHERE id = $1 RETURNING origin")
                .bind(image_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(deleted.map(|(origin,)| origin))
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
