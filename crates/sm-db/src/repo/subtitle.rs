//! `subtitle` 与 `movie_plot_image` 两张资产表的仓储。
//!
//! 两者都是「产物的落点」：字幕是播放页的必需输入，剧情图是详情页的配图。
//! 此前都无处可存 —— 与此前 `media_thumbnail` 的情况同类（`playback.rs` 的
//! 模块文档记过一次）。
//!
//! # `movie_plot_image` 没有时间戳
//!
//! ```text
//! movie_plot_image: id, movie_id, image_id, image_search_index_status
//! ```
//!
//! 四列，**没有 `created_at` / `updated_at`** —— 全库第三张不继承
//! `TimestampedMixin` 的表（前两张是 `image_search_index_state` 与
//! `schema_migration`）。它是关联表，不是事件记录。
//!
//! # `movie_plot_image` 的索引透露了它该怎么被查
//!
//! ```text
//! UNIQUE (movie_id, image_id)
//! INDEX  (image_search_index_status, id)
//! ```
//!
//! 第二条索引的存在说明**有个按索引状态批量取剧照的 worker** —— 它扫
//! `PENDING` 的剧照去做向量化。所以在
//! [`list_by_index_status`](MoviePlotImageRepository::list_by_index_status)
//! 里给出它，而不是只留一个按影片查的接口。索引是为某个查询建的，而那个
//! 查询此前不存在。

use sqlx::PgPool;

use crate::catalog::asset::{MoviePlotImage, Subtitle};
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::system::activity::SystemNotification;
use crate::system::migration::SchemaMigration;

const SUBTITLE_ENTITY: &str = "Subtitle";
const PLOT_ENTITY: &str = "MoviePlotImage";
const NOTIFICATION_ENTITY: &str = "SystemNotification";
const MIGRATION_ENTITY: &str = "SchemaMigration";

// ================================================================ subtitle

/// 新增一条字幕。
#[derive(Debug, Clone)]
pub struct NewSubtitle {
    pub movie_id: i32,
    /// 字幕文件路径。唯一索引是 `(movie_id, file_path)`，所以同一部影片
    /// 可以有多个字幕文件，而同一个路径只登记一次。
    pub file_path: String,
}

impl NewSubtitle {
    fn validate(&self) -> Result<(), DbError> {
        if self.file_path.trim().is_empty() {
            return Err(DbError::business(SUBTITLE_ENTITY, "file_path 不能为空"));
        }
        Ok(())
    }
}

/// `subtitle` 表仓储。
#[derive(Debug, Clone)]
pub struct SubtitleRepository {
    pool: PgPool,
}

impl SubtitleRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 登记一条字幕，**按 `(movie_id, file_path)` 幂等**。
    ///
    /// 刮削任务重跑是常态：同一部影片会反复发现同一个字幕文件。`ON CONFLICT
    /// DO NOTHING` 让重复登记成为无操作，而不是把「重试」变成「失败」。
    ///
    /// 返回是否真的新增了一行 —— `false` 意味着这个字幕早就登记过了。
    pub async fn upsert(&self, new: &NewSubtitle) -> Result<bool, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        let result = sqlx::query(
            "INSERT INTO subtitle (movie_id, file_path, created_at, updated_at) \
             VALUES ($1, $2, $3, $3) \
             ON CONFLICT (movie_id, file_path) DO NOTHING",
        )
        .bind(new.movie_id)
        .bind(new.file_path.trim())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(SUBTITLE_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn upsert_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        new: &NewSubtitle,
    ) -> Result<bool, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        let result = sqlx::query(
            "INSERT INTO subtitle (movie_id, file_path, created_at, updated_at) \
             VALUES ($1, $2, $3, $3) \
             ON CONFLICT (movie_id, file_path) DO NOTHING",
        )
        .bind(new.movie_id)
        .bind(new.file_path.trim())
        .bind(now)
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(SUBTITLE_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 列出某部影片的全部字幕。**刻意不分页。**
    ///
    /// 「这部影片有哪些字幕轨」是播放页一次读全的查询，分页只会让调用方
    /// 自己写取完所有页的循环。一部影片的字幕通常是零到几条。
    pub async fn list_by_movie(&self, movie_id: i32) -> Result<Vec<Subtitle>, DbError> {
        Ok(sqlx::query_as::<_, Subtitle>(
            "SELECT * FROM subtitle WHERE movie_id = $1 ORDER BY file_path, id",
        )
        .bind(movie_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 删掉一条字幕，返回是否真的删了。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM subtitle WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(SUBTITLE_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 删掉某部影片的全部字幕，返回删了几行。
    ///
    /// 删影片会 `CASCADE` 掉这些行；这个方法给的是「字幕源换了，整批重刮」
    /// 那种情况。
    pub async fn clear_movie(&self, movie_id: i32) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM subtitle WHERE movie_id = $1")
            .bind(movie_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(SUBTITLE_ENTITY))?;
        Ok(result.rows_affected())
    }
}

// ================================================================ movie_plot_image

/// `movie_plot_image` 表仓储。
///
/// 只有四列、无时间戳，所以方法很少 —— 那是表的形状，不是遗漏。
#[derive(Debug, Clone)]
pub struct MoviePlotImageRepository {
    pool: PgPool,
}

impl MoviePlotImageRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 关联一张剧照，**按 `(movie_id, image_id)` 幂等**。
    ///
    /// 唯一索引就是这两列，所以重复关联是 `DO NOTHING` 而不是报错 ——
    /// 刮削重跑会发现同一张图。返回是否真的新增了一行。
    ///
    /// 初始索引状态取
    /// [`PENDING`](crate::catalog::asset::image_search_index_status::PENDING)：
    /// 新关联的剧照还没做向量化，worker 扫 PENDING 时会拿到它。
    ///
    /// 本表**没有时间戳**，所以 `DO NOTHING` 而不是 `DO UPDATE SET updated_at`
    /// —— 没有可以更新的时刻字段。
    pub async fn link(&self, movie_id: i32, image_id: i32) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO movie_plot_image (movie_id, image_id, image_search_index_status) \
             VALUES ($1, $2, $3) ON CONFLICT (movie_id, image_id) DO NOTHING",
        )
        .bind(movie_id)
        .bind(image_id)
        .bind(crate::catalog::asset::image_search_index_status::PENDING)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(PLOT_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn link_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        movie_id: i32,
        image_id: i32,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO movie_plot_image (movie_id, image_id, image_search_index_status) \
             VALUES ($1, $2, $3) ON CONFLICT (movie_id, image_id) DO NOTHING",
        )
        .bind(movie_id)
        .bind(image_id)
        .bind(crate::catalog::asset::image_search_index_status::PENDING)
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(PLOT_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 列出某部影片的全部剧照。**刻意不分页。**
    ///
    /// 详情页一次读全，分页只会让调用方自己写取完所有页的循环。
    pub async fn list_by_movie(&self, movie_id: i32) -> Result<Vec<MoviePlotImage>, DbError> {
        Ok(sqlx::query_as::<_, MoviePlotImage>(
            "SELECT * FROM movie_plot_image WHERE movie_id = $1 ORDER BY id",
        )
        .bind(movie_id)
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 列出处于某个索引状态的全部剧照。**分页。**
        ///
        /// 走 `movie_plot_image_image_search_index_status_id_idx` —— 那条索引
        /// **存在就是为了**这个查询：worker 扫 PENDING 去做向量化。
        ///
        /// 排序是 `(image_search_index_status, id)`，与索引一致，所以不需要
        /// 额外排序步骤。`id` 作次级键让多轮扫描的顺序稳定 —— 否则同一批
        /// PENDING 每次扫出来的顺序可能不同，重试会挑到不同的图。
        pub async fn list_by_index_status(
            &self,
            status: i32,
        ) -> Result<Page<MoviePlotImage>, DbError> {
            count = "SELECT COUNT(*) FROM movie_plot_image \
                     WHERE image_search_index_status = $1",
            items = "SELECT * FROM movie_plot_image WHERE image_search_index_status = $1 \
                     ORDER BY image_search_index_status, id LIMIT $2 OFFSET $3",
        }
    }

    /// 推进一张剧照的向量化状态。返回是否真的改了。
    ///
    /// 这是 worker 的另一个动作：做完向量化后把它标成 SUCCESS 或 FAILED。
    pub async fn set_index_status(
        &self,
        movie_id: i32,
        image_id: i32,
        status: i32,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE movie_plot_image SET image_search_index_status = $3 \
             WHERE movie_id = $1 AND image_id = $2",
        )
        .bind(movie_id)
        .bind(image_id)
        .bind(status)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(PLOT_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 解除关联，返回是否真的删了。
    pub async fn unlink(&self, movie_id: i32, image_id: i32) -> Result<bool, DbError> {
        let result =
            sqlx::query("DELETE FROM movie_plot_image WHERE movie_id = $1 AND image_id = $2")
                .bind(movie_id)
                .bind(image_id)
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::from(e).with_entity(PLOT_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }
}

// ================================================================ system_notification

/// 新建一条通知。
#[derive(Debug, Clone)]
pub struct NewNotification {
    /// 分类，有索引。取值必须是 [`notification_category`] 白名单之一。
    ///
    /// 此前这里是空的（上游枚举没被搬过来），所以只校验非空；白名单补全后
    /// 校验也跟着收紧 —— 未知分类会让客户端的分类筛选渲染不出对应分支。
    pub category: String,
    pub title: String,
    pub content: String,
    /// 事件类型。新身份字段，与 `resource_type` / `resource_id` 一套。
    pub event_type: Option<String>,
    /// 去重键。**唯一** —— 同一事件只产生一条通知。
    ///
    /// NULL 不参与唯一约束，所以没去重需求的普通通知不受影响。
    pub dedupe_key: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<i32>,
    /// 关联的后台任务。删任务会 SET NULL。
    pub related_task_run_id: Option<i32>,
    /// 遗留的展示关联字段。与上面那套并存，迁移时不要合并。
    pub related_resource_type: Option<String>,
    pub related_resource_id: Option<i32>,
}

impl NewNotification {
    /// 归一化后的分类。空白与非白名单值都回落 `info`。
    ///
    /// 与上游 `normalize_allowed_filter` 的差别：上游对非法值抛 422，而这里
    /// 回落。理由是本方法是**仓储层**，它不知道调用方是 HTTP 层（该报 422）
    /// 还是 worker 内部调用（只该记日志）。要区分请在 service 层用
    /// `sm_service::system::activity::filters::normalize_allowed_filter`。
    pub fn normalized_category(&self) -> &str {
        let trimmed = self.category.trim();
        if crate::system::activity::notification_category::is_valid(trimmed) {
            trimmed
        } else {
            crate::system::activity::notification_category::DEFAULT
        }
    }

    /// 校验并归一化前置条件。**公开是为了让规则能被直接断言** ——
    /// 它决定「什么会被写进库」，出错时用户看到的是通知丢失而不是报错。
    pub fn validate(&self) -> Result<(), DbError> {
        if self.title.trim().is_empty() {
            return Err(DbError::business(NOTIFICATION_ENTITY, "title 不能为空"));
        }
        if !crate::system::activity::notification_category::is_valid(self.category.trim()) {
            return Err(DbError::business(
                NOTIFICATION_ENTITY,
                format!(
                    "category 非法：{:?}，允许值 {:?}",
                    self.category.trim(),
                    crate::system::activity::notification_category::ALL
                ),
            ));
        }
        // `resource_type` 与 `resource_id` 必须配对 —— 单有一个无法定位
        // 资源。数据库不校验这个（两列都是独立的可空列）。
        if self.resource_type.is_some() != self.resource_id.is_some() {
            return Err(DbError::business(
                NOTIFICATION_ENTITY,
                "resource_type 与 resource_id 必须同时给出或同时留空",
            ));
        }
        Ok(())
    }
}

/// `system_notification` 表仓储。
///
/// 这是后台任务失败的**面向用户的出口** —— 没有它，任务的失败只存在于
/// `background_task_run` 里，用户永远看不到。
#[derive(Debug, Clone)]
pub struct SystemNotificationRepository {
    pool: PgPool,
}

impl SystemNotificationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 发一条通知，**带去重**。
    ///
    /// `dedupe_key` 有唯一约束，所以走 `ON CONFLICT DO NOTHING`：同一事件
    /// 触发两次（任务重试、worker 重启）只产生一条通知。
    ///
    /// 返回 `None` 表示已被去重 —— 已有的那条没被改动。调用方拿到 `None`
    /// 就该知道「用户已经看到过了」，而不是再提示一次。
    ///
    /// `dedupe_key` 为 `None` 时不受此约束：NULL 不参与唯一约束，所以普通
    /// 通知照常产生多条。
    pub async fn notify(
        &self,
        new: &NewNotification,
    ) -> Result<Option<SystemNotification>, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, SystemNotification>(
            "INSERT INTO system_notification (category, title, content, event_type, \
                    dedupe_key, resource_type, resource_id, related_task_run_id, \
                    related_resource_type, related_resource_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11) \
             ON CONFLICT (dedupe_key) DO NOTHING RETURNING *",
        )
        .bind(new.category.trim())
        .bind(new.title.trim())
        .bind(new.content.trim())
        .bind(new.event_type.as_deref().map(str::trim))
        .bind(
            new.dedupe_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .bind(new.resource_type.as_deref().map(str::trim))
        .bind(new.resource_id)
        .bind(new.related_task_run_id)
        .bind(new.related_resource_type.as_deref().map(str::trim))
        .bind(new.related_resource_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))
    }
    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn notify_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        new: &NewNotification,
    ) -> Result<Option<SystemNotification>, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, SystemNotification>(
            "INSERT INTO system_notification (category, title, content, event_type, \
                    dedupe_key, resource_type, resource_id, related_task_run_id, \
                    related_resource_type, related_resource_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11) \
             ON CONFLICT (dedupe_key) DO NOTHING RETURNING *",
        )
        .bind(new.category.trim())
        .bind(new.title.trim())
        .bind(new.content.trim())
        .bind(new.event_type.as_deref().map(str::trim))
        .bind(
            new.dedupe_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .bind(new.resource_type.as_deref().map(str::trim))
        .bind(new.resource_id)
        .bind(new.related_task_run_id)
        .bind(new.related_resource_type.as_deref().map(str::trim))
        .bind(new.related_resource_id)
        .bind(now)
        .fetch_optional(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))
    }

    /// 按 `dedupe_key` 原子创建，**冲突时回读并返回既有行**。
    ///
    /// 与 [`Self::notify`] 的区别只在返回值：`notify` 用 `None` 表达
    /// 「已被去重」，而本方法给出那条既有记录。上游 `create_once` 是后者
    /// （`activity/notifications.py:112-140`）—— 调用方要拿它填资源字段，
    /// 拿到 `None` 反而要再查一次。
    ///
    /// `dedupe_key` 为空是**调用错误**：上游直接
    /// `raise ValueError("notification_dedupe_key_required")`。想要不带
    /// 去重的普通通知请用 [`Self::notify`]（`NULL` 不参与唯一约束）。
    pub async fn create_once(
        &self,
        new: &NewNotification,
    ) -> Result<SystemNotification, DbError> {
        new.validate()?;
        let dedupe_key = new
            .dedupe_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(dedupe_key) = dedupe_key else {
            return Err(DbError::business(
                NOTIFICATION_ENTITY,
                "notification_dedupe_key_required：create_once 要求非空 dedupe_key",
            ));
        };

        if let Some(row) = self.notify(new).await? {
            return Ok(row);
        }

        // 冲突落败：回读既有行。唯一约束保证它此刻一定存在。
        self.find_by_dedupe_key(dedupe_key)
            .await?
            .ok_or_else(|| {
                DbError::business(
                    NOTIFICATION_ENTITY,
                    "create_once 冲突后回读不到既有行",
                )
            })
    }

    /// 按去重键取一行。
    pub async fn find_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<SystemNotification>, DbError> {
        sqlx::query_as::<_, SystemNotification>(
            "SELECT * FROM system_notification WHERE dedupe_key = $1",
        )
        .bind(dedupe_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))
    }

    /// 释放已解决事件的幂等键，返回受影响的行数。
    ///
    /// 对应上游 `release_notification_dedupe_key`
    /// （`activity/notifications.py:103`）。**保留通知历史**以便下次同类事件
    /// 重新提醒 —— 置空键而不是删行正是这个用意。
    pub async fn release_dedupe_key(&self, dedupe_key: &str) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE system_notification SET dedupe_key = NULL WHERE dedupe_key = $1",
        )
        .bind(dedupe_key)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 未读计数。
    pub async fn count_unread(&self) -> Result<i64, DbError> {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM system_notification WHERE is_read = false",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))
    }

    paged_list! {
        /// 通知的分页查询，带分类与已读状态两个可选筛选。
        ///
        /// 对应上游 `NotificationService.list_notifications`
        /// （`activity/notifications.py:205`）。排序 `id DESC` —— 通知列表要
        /// 「最新的在前」，升序会把最老的顶在第一页，那是通知里最没用的位置。
        ///
        /// 两个筛选都可选，所以用 `($n IS NULL OR …)` 让语句恒定；理由见
        /// `BackgroundTaskRunRepository::list_runs` 的同款说明（索引在
        /// 「不筛选」时用不上，对一个通知列表可接受）。
        pub async fn list_all(
            &self,
            category: Option<String>,
            is_read: Option<bool>,
        ) -> Result<Page<SystemNotification>, DbError> {
            count = "SELECT COUNT(*) FROM system_notification \
                     WHERE ($1::text IS NULL OR category = $1) \
                       AND ($2::bool IS NULL OR is_read = $2)",
            items = "SELECT * FROM system_notification \
                     WHERE ($1::text IS NULL OR category = $1) \
                       AND ($2::bool IS NULL OR is_read = $2) \
                     ORDER BY id DESC LIMIT $3 OFFSET $4",
        }
    }

    /// 批量标记已读，返回**真正改动了几行**。
    ///
    /// 对应上游 `mark_notifications_read`（`notifications.py:246`）。
    /// `WHERE id = ANY($1) AND is_read = false` 有两个作用：
    ///
    /// - 重复标记返回 0 而不是再写一次 `read_at` —— `read_at` 是「第一次被
    ///   读到的时刻」，不该被后续点击改写（同 [`Self::mark_read`] 的理由）。
    /// - **不存在的 id 被静默忽略**。上游也是这个行为：客户端传回来一串 id，
    ///   其中一个已经被清理掉，不该让整批失败。
    ///
    /// 空列表直接返回 0，不发 SQL —— 那是「匹配不到任何行」的空查询，
    /// 白跑一趟往返。
    pub async fn mark_notifications_read(&self, ids: &[i32]) -> Result<u64, DbError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE system_notification SET is_read = true, read_at = $2, updated_at = $2 \
             WHERE id = ANY($1) AND is_read = false",
        )
        .bind(ids)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 标记已读，写 `is_read` 与 `read_at` **两个字段**。
    ///
    /// 只写 `is_read` 会留下一个不一致的状态 —— 模型的
    /// [`SystemNotification::read_state_inconsistent`] 正是为检测它而写的。
    /// 所以这里一起写，而不是让调用方分两步。
    ///
    /// `WHERE is_read = false` 让重复标记返回 `false`，且不会覆盖首次读
    /// 的时刻 —— `read_at` 是「第一次被读到的时刻」，不该被后续点击改写。
    pub async fn mark_read(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE system_notification SET is_read = true, read_at = $2, updated_at = $2 \
             WHERE id = $1 AND is_read = false",
        )
        .bind(id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 批量标记已读，返回改了几行。
    pub async fn mark_all_read(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE system_notification SET is_read = true, read_at = $1, updated_at = $1 \
             WHERE is_read = false",
        )
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected())
    }

    paged_list! {
        /// 列出未读。**分页。** 走 `system_notification_is_read_idx`。
        ///
        /// 排序 `id DESC` —— 未读列表要「最新的在前」。升序会把最老的
        /// 顶在第一页，那是通知里最没用的位置。
        pub async fn list_unread(&self) -> Result<Page<SystemNotification>, DbError> {
            count = "SELECT COUNT(*) FROM system_notification WHERE is_read = false",
            items = "SELECT * FROM system_notification WHERE is_read = false \
                     ORDER BY id DESC LIMIT $1 OFFSET $2",
        }
    }

    paged_list! {
        /// 按分类列出。**分页。** 走 `system_notification_category_idx`。
        ///
        /// 排序同样是 `id DESC`：分类视图里最新的也该在前。
        pub async fn list_by_category(
            &self,
            category: &str,
        ) -> Result<Page<SystemNotification>, DbError> {
            count = "SELECT COUNT(*) FROM system_notification WHERE category = $1",
            items = "SELECT * FROM system_notification WHERE category = $1 \
                     ORDER BY id DESC LIMIT $2 OFFSET $3",
        }
    }

    /// 删掉一条通知。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM system_notification WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 把指向一批任务台账的 `related_task_run_id` **置空**。返回改了几行。
    ///
    /// # 置空而不是让外键级联删通知
    ///
    /// 通知是**独立实体**，它自己的保留期由 [`Self::delete_read_before_in`]
    /// 管。而 `related_task_run_id` 只是「这条通知与那次运行有关」的展示关联 ——
    /// 台账记录被保留期清理掉后，通知（「某某任务失败了」）仍然是用户要看的
    /// 事实，不该跟着消失。上游显式做了同一件事，注释写明「避免悬挂引用，
    /// 不依赖数据库级联行为」。
    ///
    /// 事务内变体：必须与删台账放在同一个事务里。
    pub async fn detach_task_runs_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        task_run_ids: &[i32],
    ) -> Result<u64, DbError> {
        if task_run_ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE system_notification SET related_task_run_id = NULL, updated_at = $2 \
             WHERE related_task_run_id = ANY($1)",
        )
        .bind(task_run_ids)
        .bind(crate::common::time::now_utc())
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected())
    }

    /// 删掉「已读且 `read_at` 早于 `cutoff`」的通知。返回删了几行。
    ///
    /// **未读通知一律保留** —— 用户还没看过的内容不该被清理掉。
    ///
    /// 用 `read_at` 而不是 `created_at` 作为窗口基准：一条 30 天前创建、
    /// 昨天才读的通知，按 `read_at` 算还有保留期，按 `created_at` 算已经过期。
    /// 上游用的是 `read_at`（`SystemNotification.read_at < cutoff`）。
    ///
    /// 事务内变体。
    pub async fn delete_read_before_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        cutoff: chrono::NaiveDateTime,
    ) -> Result<u64, DbError> {
        let result =
            sqlx::query("DELETE FROM system_notification WHERE is_read = true AND read_at < $1")
                .bind(cutoff)
                .execute(ctx.conn().await?.as_conn())
                .await
                .map_err(|e| DbError::from(e).with_entity(NOTIFICATION_ENTITY))?;
        Ok(result.rows_affected())
    }
}
// ================================================================ schema_migration

/// `schema_migration` 表仓储：DDL 版本记录。
///
/// 三列，**没有 `created_at` / `updated_at`** —— 全库第二张不继承
/// `TimestampedMixin` 的表。它记的是「某个迁移被应用的时刻」，那个时刻就
/// 是 `applied_at` 本身，再加一列「这条记录何时写入」没有意义。
///
/// 这张表是唯一一个「仓储的主要价值是让别人能**查**」的表：写入只在启动时
/// 发生一次，而「哪个迁移已经应用过了」要被反复问。
#[derive(Debug, Clone)]
pub struct SchemaMigrationRepository {
    pool: PgPool,
}

impl SchemaMigrationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 记录一次迁移已应用，**按 `name` 幂等**。
    ///
    /// `name` 有唯一约束，而启动流程**必须**能重入 —— 进程重启、并发启动
    /// 多个实例，都会走到「再记一次」。`ON CONFLICT DO NOTHING` 让它是无
    /// 操作而不是报错：报错会把「第二次启动」变成启动失败。
    ///
    /// 返回是否真的新增了一行。`false` 意味着这个迁移早就应用过了，调用方
    /// 据此决定「这次要执行它」还是「跳过」。
    ///
    /// **不**更新 `applied_at`：那是「首次被应用的时刻」，重入不该改写它。
    pub async fn record(&self, name: &str) -> Result<bool, DbError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DbError::business(MIGRATION_ENTITY, "name 不能为空"));
        }
        let result = sqlx::query(
            "INSERT INTO schema_migration (name, applied_at) VALUES ($1, $2) \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(name)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(MIGRATION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 事务内变体，供 [`Ctx`](crate::repo::Ctx) 编排时使用。
    pub async fn record_in(
        &self,
        ctx: &mut crate::repo::Ctx<'_>,
        name: &str,
    ) -> Result<bool, DbError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DbError::business(MIGRATION_ENTITY, "name 不能为空"));
        }
        let result = sqlx::query(
            "INSERT INTO schema_migration (name, applied_at) VALUES ($1, $2) \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(name)
        .bind(crate::common::time::now_utc())
        .execute(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(MIGRATION_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 某个迁移是否已应用。走 `name` 的唯一约束。
    pub async fn is_applied(&self, name: &str) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM schema_migration WHERE name = $1)",
        )
        .bind(name.trim())
        .fetch_one(&self.pool)
        .await?)
    }

    /// 列出已应用的迁移名，**按应用时刻升序**。**刻意不分页。**
    ///
    /// 与 `SchemaMigration::is_applied` 分开：那是「某一个应用过吗」，这是
    /// 「都应用了哪些、按什么顺序」—— 后者用于启动日志与排障，量很小
    /// （几十条），分页只会添麻烦。
    pub async fn applied_names(&self) -> Result<Vec<String>, DbError> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT name FROM schema_migration ORDER BY applied_at, id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    paged_list! {
        /// 按名字列出全部记录。**分页。**
        ///
        /// 走 `schema_migration_name_idx`。与 [`Self::applied_names`] 的区别
        /// 是这里返回整行（含 `applied_at`）而不是只有名字。
        pub async fn list(&self) -> Result<Page<SchemaMigration>, DbError> {
            count = "SELECT COUNT(*) FROM schema_migration",
            items = "SELECT * FROM schema_migration ORDER BY name LIMIT $1 OFFSET $2",
        }
    }
}
