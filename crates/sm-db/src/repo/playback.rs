//! Media 族仓储：`media_thumbnail` / `media_progress` / `media_point` /
//! `media_clip`。
//!
//! # 为什么 `media_thumbnail` 排在最前面
//!
//! [`crate::repo::media::MediaRepository::record_thumbnail_success`] 已经
//! 能把一条 Media 标记成「缩略图生成成功」，但**产物无处可存** ——
//! `media_thumbnail` 表没有仓储。这是现有代码内部就已经断裂的地方，
//! 本文件把它接上。
//!
//! # 三张表的唯一索引决定了写入方式
//!
//! | 表 | 唯一索引 | 后果 |
//! |---|---|---|
//! | `media_thumbnail` | `(media_id, offset)` | 同一时刻点不重复产出 → 必须 **upsert** |
//! | `media_progress` | `(media_id)` | 一条 Media 至多一条进度 → 必须 **upsert** |
//! | `media_clip` | `(media_id, start, end)`，而 `media_id` **可空** | 来源被删后 `media_id` 置 NULL，**多个 NULL 不参与唯一约束** → 正是期望行为 |
//!
//! 所以本文件**不提供** `media_thumbnail` 与 `media_progress` 的裸
//! `insert`：那会撞唯一约束，而「撞了再改」比 upsert 更难推理。

use sqlx::PgPool;

use super::ctx::Ctx;
use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::playback::media::{
    image_search_index_status, MediaClip, MediaPoint, MediaProgress, MediaThumbnail,
};

const THUMBNAIL_ENTITY: &str = "MediaThumbnail";
const PROGRESS_ENTITY: &str = "MediaProgress";
const POINT_ENTITY: &str = "MediaPoint";
const CLIP_ENTITY: &str = "MediaClip";

/// `media_thumbnail` 表仓储。
#[derive(Debug, Clone)]
pub struct MediaThumbnailRepository {
    pool: PgPool,
}

impl MediaThumbnailRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 写入（或覆盖）某个时刻点的缩略图。
    ///
    /// 用 `ON CONFLICT (media_id, "offset") DO UPDATE` 而不是 `insert`：
    /// 唯一索引保证同一时刻点不重复产出，而「重复产出」在重试场景下
    /// 是常态（第一次生成的图质量不达标，重来一次）。裸 insert 会撞
    /// 约束，把「重试」变成「失败」。
    ///
    /// 覆盖时**只更新 image 与索引状态**，`created_at` 保持首次产出的
    /// 时刻 —— 那个时刻是「这个时刻点被首次识别出来」的时间，重试
    /// 不该改写它。
    pub async fn upsert(
        &self,
        media_id: i32,
        offset_seconds: i32,
        image_id: i32,
        index_status: i32,
    ) -> Result<MediaThumbnail, DbError> {
        let mut ctx = Ctx::over_pool(&self.pool);
        self.upsert_in(&mut ctx, media_id, offset_seconds, image_id, index_status)
            .await
    }

    /// [`Self::upsert`] 的事务内变体。见 [`Ctx`]。
    pub async fn upsert_in(
        &self,
        ctx: &mut Ctx<'_>,
        media_id: i32,
        offset_seconds: i32,
        image_id: i32,
        index_status: i32,
    ) -> Result<MediaThumbnail, DbError> {
        if !image_search_index_status::is_valid(index_status) {
            return Err(DbError::business(
                THUMBNAIL_ENTITY,
                format!("未知的 image_search_index_status: {index_status}"),
            ));
        }
        let now = crate::common::time::now_utc();
        let mut conn = ctx.conn().await?;
        sqlx::query_as::<_, MediaThumbnail>(
            "INSERT INTO media_thumbnail ( \
                 media_id, image_id, \"offset\", image_search_index_status, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $5) \
             ON CONFLICT (media_id, \"offset\") DO UPDATE \
             SET image_id = EXCLUDED.image_id, \
                 image_search_index_status = EXCLUDED.image_search_index_status, \
                 updated_at = EXCLUDED.updated_at \
             RETURNING *",
        )
        .bind(media_id)
        .bind(image_id)
        .bind(offset_seconds)
        .bind(index_status)
        .bind(now)
        .fetch_one(conn.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(THUMBNAIL_ENTITY))
    }

    paged_list! {
        /// 列出某条 Media 的全部缩略图，按时刻点升序。**分页。**
        ///
        /// 一部长片可能有几十个时刻点，所以分页。
        pub async fn list_by_media(
            &self,
            media_id: i32,
        ) -> Result<Page<MediaThumbnail>, DbError> {
            count = "SELECT COUNT(*) FROM media_thumbnail WHERE media_id = $1",
            items = "SELECT * FROM media_thumbnail WHERE media_id = $1 \
                     ORDER BY \"offset\" LIMIT $2 OFFSET $3",
        }
    }

    /// 按 `image_search_index_status` 取出待处理的缩略图。
    ///
    /// 索引是 `(image_search_index_status, id)`，正好服务这个查询。
    /// 队列扫描应当**只按状态取**，不 join 回 `media` —— 状态列已经
    /// 把「谁是 PENDING」这个答案缓存好了，join 只会让它变慢。
    pub async fn list_by_index_status(
        &self,
        status: i32,
        limit: i64,
    ) -> Result<Vec<MediaThumbnail>, DbError> {
        Ok(sqlx::query_as::<_, MediaThumbnail>(
            "SELECT * FROM media_thumbnail WHERE image_search_index_status = $1 ORDER BY id LIMIT $2",
        )
        .bind(status)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 标记缩略图已进入图像检索终态。
    ///
    /// 与 `media` 上的缩略图状态机**刻意分开**：那台状态机管的是
    /// 「有没有生成出来」，这台管的是「生成的那张有没有进检索索引」。
    /// 两者会先后经历 PENDING，中间还能分叉（SKIPPED 表示非 JAV 媒体
    /// 不参与检索）。合成一台会丢掉「图生成了但没进索引」这个状态，
    /// 而那恰好是需要人工排查的组合。
    pub async fn mark_indexed(&self, id: i32) -> Result<MediaThumbnail, DbError> {
        self.set_index_status(id, image_search_index_status::SUCCESS)
            .await
    }

    /// 标记该缩略图**不参与**图像检索。
    ///
    /// 用于非 JAV 媒体：图照常生成，但进检索索引没有意义，落明确终态
    /// 避免长期滞留 PENDING（模型注释的原话）。
    pub async fn mark_skipped(&self, id: i32) -> Result<MediaThumbnail, DbError> {
        self.set_index_status(id, image_search_index_status::SKIPPED)
            .await
    }

    /// 记录一次索引失败。
    pub async fn mark_index_failed(&self, id: i32) -> Result<MediaThumbnail, DbError> {
        self.set_index_status(id, image_search_index_status::FAILED)
            .await
    }

    /// 三个 `mark_*` 的共同实现。它们只差一个状态字面量。
    async fn set_index_status(&self, id: i32, status: i32) -> Result<MediaThumbnail, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaThumbnail>(
            "UPDATE media_thumbnail \
             SET image_search_index_status = $2, updated_at = $3 WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(status)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(THUMBNAIL_ENTITY, id))
    }
}

/// `media_progress` 表仓储。
#[derive(Debug, Clone)]
pub struct MediaProgressRepository {
    pool: PgPool,
}

impl MediaProgressRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 记录播放位置。**upsert**。
    ///
    /// `media_id` 上有单列唯一索引，一条 Media 至多一条进度，所以这是
    /// 典型的 upsert 而非 insert。
    ///
    /// # 为什么允许进度倒退
    ///
    /// 真实场景里倒退是合法的：用户拖回去重看、把上一集的位置同步过来。
    /// 「只许前进」听起来更安全，但会让这些操作**看起来成功却没生效** ——
    /// 比倒退本身更糟。倒退与乱序的区分交给 service 层（它知道
    /// 「刚看完这集」这种上下文），仓储层不做这个判断。
    pub async fn save(
        &self,
        media_id: i32,
        position_seconds: i32,
    ) -> Result<MediaProgress, DbError> {
        if position_seconds < 0 {
            return Err(DbError::business(
                PROGRESS_ENTITY,
                format!("position_seconds 不能为负，收到 {position_seconds}"),
            ));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaProgress>(
            "INSERT INTO media_progress ( \
                 media_id, position_seconds, last_watched_at, created_at, updated_at \
             ) VALUES ($1, $2, $3, $3, $3) \
             ON CONFLICT (media_id) DO UPDATE \
             SET position_seconds = EXCLUDED.position_seconds, \
                 last_watched_at = EXCLUDED.last_watched_at, \
                 updated_at = EXCLUDED.updated_at \
             RETURNING *",
        )
        .bind(media_id)
        .bind(position_seconds)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(PROGRESS_ENTITY))
    }

    /// 读取进度。未看过返回 `None`。
    pub async fn find_by_media(&self, media_id: i32) -> Result<Option<MediaProgress>, DbError> {
        Ok(
            sqlx::query_as::<_, MediaProgress>("SELECT * FROM media_progress WHERE media_id = $1")
                .bind(media_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 批量读取多条 Media 的进度。
    ///
    /// 列表页要显示「已看到 42%」时，N+1 次查询是明显的浪费。
    /// 返回 HashMap 让调用方一次查完；未看过的 media 不会出现在结果里。
    pub async fn load_many(
        &self,
        media_ids: &[i32],
    ) -> Result<std::collections::HashMap<i32, MediaProgress>, DbError> {
        if media_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, MediaProgress>(
            "SELECT * FROM media_progress WHERE media_id = ANY($1)",
        )
        .bind(media_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|row| (row.media_id, row)).collect())
    }

    /// 清除进度（标记为未看）。返回是否真的删掉了一行。
    ///
    /// 删行而不是把 `position_seconds` 归零：归零会让「已看到片尾」
    /// 和「刚开始看」变成同一个值，而 `last_watched_at` 还在，
    /// 语义上说不清。删掉最干净。
    pub async fn clear(&self, media_id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM media_progress WHERE media_id = $1")
            .bind(media_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// `media_point` 表仓储（时刻点 / 剧情点）。
#[derive(Debug, Clone)]
pub struct MediaPointRepository {
    pool: PgPool,
}

impl MediaPointRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按 id 查询一个时刻点。
    ///
    /// service 层校验「这个 point 存在吗」时需要它 —— 那是对
    /// `media_point` 行的检查，而**不是**对 `moment_collection_item`
    /// 行的检查（后者是关联表，它的行存在只说明关联存在）。
    ///
    /// 走主键索引。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<MediaPoint>, DbError> {
        Ok(
            sqlx::query_as::<_, MediaPoint>("SELECT * FROM media_point WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 插入时刻点。
    ///
    /// `movie_number` / `video_item_id` 是**快照**（不建外键），所以
    /// `media_id` 可空 —— 来源 Media 被删后时刻点仍然存在并保留展示，
    /// 这是 `on_delete = SET NULL` 的设计意图。
    ///
    /// 不提供 upsert：`media_point` 上**没有**唯一索引，同一时刻点可以
    /// 有多行（来源不同就是不同记录）。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        &self,
        image_id: i32,
        offset_seconds: i32,
        media_id: Option<i32>,
        movie_number: Option<&str>,
        video_item_id: Option<i32>,
    ) -> Result<MediaPoint, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaPoint>(
            "INSERT INTO media_point ( \
                 media_id, image_id, movie_number, video_item_id, offset_seconds, \
                 created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING *",
        )
        .bind(media_id)
        .bind(image_id)
        .bind(movie_number.map(str::trim).filter(|s| !s.is_empty()))
        .bind(video_item_id)
        .bind(offset_seconds)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(POINT_ENTITY))
    }

    paged_list! {
        /// 列出某条 Media 的时刻点，按时刻升序。**分页。**
        pub async fn list_by_media(
            &self,
            media_id: i32,
        ) -> Result<Page<MediaPoint>, DbError> {
            count = "SELECT COUNT(*) FROM media_point WHERE media_id = $1",
            items = "SELECT * FROM media_point WHERE media_id = $1 \
                     ORDER BY offset_seconds, id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 列出**没有来源 Media** 的时刻点（孤儿）。**分页。**
        pub async fn list_orphaned(&self) -> Result<Page<MediaPoint>, DbError> {
            count = "SELECT COUNT(*) FROM media_point WHERE media_id IS NULL",
            items = "SELECT * FROM media_point WHERE media_id IS NULL \
                     ORDER BY id LIMIT $1 OFFSET $2",
        }
    }

    /// 删时刻点。返回是否真的删掉了一行。
    ///
    /// 删时刻点本身不受 RESTRICT 约束 —— 那个约束保护的是 `image`，
    /// 方向相反。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM media_point WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// `media_clip` 表仓储（片段）。
#[derive(Debug, Clone)]
pub struct MediaClipRepository {
    pool: PgPool,
}

impl MediaClipRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按 id 查询一个片段。
    ///
    /// service 层校验「这个 clip 存在吗」时需要它 —— 那是对 `media_clip`
    /// 行的检查，而**不是**对 `clip_collection_item` 行的检查（后者是
    /// 关联表，它的行存在只说明关联存在）。
    ///
    /// 走主键索引。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<MediaClip>, DbError> {
        Ok(
            sqlx::query_as::<_, MediaClip>("SELECT * FROM media_clip WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 插入片段。
    ///
    /// 唯一索引 `(media_id, start, end)` 里 `media_id` **可空**，而
    /// NULL 不参与唯一约束 —— 所以多个「来源已删除」的片段可以共存。
    /// 这是期望行为，模型注释写明了「唯一索引只在来源存活期间有效」。
    pub async fn insert(&self, new: &NewMediaClip) -> Result<MediaClip, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaClip>(
            "INSERT INTO media_clip ( \
                 media_id, movie_number, start_offset_seconds, end_offset_seconds, \
                 title, file_path, file_size_bytes, duration_seconds, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) RETURNING *",
        )
        .bind(new.media_id)
        .bind(
            new.movie_number
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .bind(new.start_offset_seconds)
        .bind(new.end_offset_seconds)
        .bind(new.title.trim())
        .bind(new.file_path.trim())
        .bind(new.file_size_bytes)
        .bind(new.duration_seconds)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(CLIP_ENTITY))
    }

    paged_list! {
        /// 列出某条 Media 的片段。**分页。**
        pub async fn list_by_media(
            &self,
            media_id: i32,
        ) -> Result<Page<MediaClip>, DbError> {
            count = "SELECT COUNT(*) FROM media_clip WHERE media_id = $1",
            items = "SELECT * FROM media_clip WHERE media_id = $1 \
                     ORDER BY start_offset_seconds, id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 按影片番号列出片段。**分页。**
        ///
        /// 走 `media_clip_movie_number_idx`。**来源被删的片段也会出现** ——
        /// 快照列的意义就是让它们仍能归属到某部影片。
        pub async fn list_by_movie_number(
            &self,
            movie_number: &str,
        ) -> Result<Page<MediaClip>, DbError> {
            count = "SELECT COUNT(*) FROM media_clip WHERE movie_number = $1",
            items = "SELECT * FROM media_clip WHERE movie_number = $1 \
                     ORDER BY start_offset_seconds, id LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 列出仍然挂在来源上的片段。**分页。**
        pub async fn list_attached(&self) -> Result<Page<MediaClip>, DbError> {
            count = "SELECT COUNT(*) FROM media_clip WHERE media_id IS NOT NULL",
            items = "SELECT * FROM media_clip WHERE media_id IS NOT NULL \
                     ORDER BY id LIMIT $1 OFFSET $2",
        }
    }

    paged_list! {
        /// 列出「独立资产」片段（来源已删除，文件与记录都保留）。**分页。**
        pub async fn list_detached(&self) -> Result<Page<MediaClip>, DbError> {
            count = "SELECT COUNT(*) FROM media_clip WHERE media_id IS NULL",
            items = "SELECT * FROM media_clip WHERE media_id IS NULL \
                     ORDER BY id LIMIT $1 OFFSET $2",
        }
    }
}

/// 新建一个片段。
#[derive(Debug, Clone)]
pub struct NewMediaClip {
    /// 来源 Media。`None` 表示直接创建独立片段。
    pub media_id: Option<i32>,
    /// 来源快照，便于来源删除后仍可归属与展示。
    pub movie_number: Option<String>,
    pub start_offset_seconds: i32,
    pub end_offset_seconds: i32,
    /// `title text NOT NULL DEFAULT ''` —— **不是 `Option`**。
    ///
    /// 注释此前写着「可空 —— DDL 有 `DEFAULT ''`」，把「有默认值」误当成
    /// 「可以为空」。DEFAULT 只决定省略时写什么，不改变列的可空性；
    /// 声明成 `Option` 时 `None` 会绑成 NULL 并违反 NOT NULL。
    /// 同一结构里的 `duration_seconds` 已经是 `i32`，这里应当一致。
    pub title: String,
    /// 产物 mp4 相对 `media_clip_root_path` 的路径。
    pub file_path: String,
    pub file_size_bytes: i64,
    pub duration_seconds: i32,
}

impl NewMediaClip {
    fn validate(&self) -> Result<(), DbError> {
        if self.file_path.trim().is_empty() {
            return Err(DbError::business(CLIP_ENTITY, "file_path 不能为空"));
        }
        if self.end_offset_seconds < self.start_offset_seconds {
            return Err(DbError::business(
                CLIP_ENTITY,
                format!(
                    "end_offset_seconds({}) 不能小于 start_offset_seconds({})",
                    self.end_offset_seconds, self.start_offset_seconds
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(start: i32, end: i32) -> NewMediaClip {
        NewMediaClip {
            media_id: Some(1),
            movie_number: Some("ABC-001".to_owned()),
            start_offset_seconds: start,
            end_offset_seconds: end,
            title: String::new(),
            file_path: "clip.mp4".to_owned(),
            file_size_bytes: 1024,
            duration_seconds: end - start,
        }
    }

    #[test]
    fn clip_requires_a_path_and_a_sane_range() {
        assert!(clip(0, 10).validate().is_ok());

        let mut no_path = clip(0, 10);
        no_path.file_path = "  ".to_owned();
        assert!(no_path.validate().is_err(), "空路径");

        // 倒置区间被拒 —— 负长度片段在播放器里没有意义
        assert!(clip(10, 5).validate().is_err(), "end < start");
        // 相等区间合法（长度为 0），模型里 length_seconds 返回 0
        assert!(clip(5, 5).validate().is_ok());
    }

    #[test]
    fn index_status_terminality_drives_the_scanner() {
        // 只有 SUCCESS / SKIPPED 是终态。SKIPPED 是非 JAV 媒体的正常结局，
        // 把它也算进终态才能避免它们永久滞留 PENDING。
        assert!(image_search_index_status::is_terminal(
            image_search_index_status::SUCCESS
        ));
        assert!(image_search_index_status::is_terminal(
            image_search_index_status::SKIPPED
        ));
        assert!(!image_search_index_status::is_terminal(
            image_search_index_status::PENDING
        ));
        assert!(!image_search_index_status::is_terminal(
            image_search_index_status::FAILED
        ));
    }
}
