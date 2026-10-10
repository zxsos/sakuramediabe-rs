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

// `Arguments` 是 trait 而非类型 —— sqlx 0.9 里 `sqlx::Arguments` 只是契约，
// Postgres 的具体类型是 `sqlx::postgres::PgArguments`。它的 `add`/`len` 都只在
// trait 里可见，所以这个导入是必需的，不是冗余。
use sqlx::{Arguments as _, PgPool};

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

    /// 按 id 查询一条缩略图。
    ///
    /// # 为什么需要它
    ///
    /// 唯一索引是 `(media_id, offset)`，所以已有的查询入口全部是「按
    /// `(media_id, offset)` 定位」或「按 media 列举」。而
    /// `VideoItemService` 的封面规则拿到的是**缩略图 id**
    /// （`VideoItemUpdateRequest.cover_thumbnail_id`），方向正好相反 ——
    /// 没有这个方法，那条规则只能退回 service 层写 SQL。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<MediaThumbnail>, DbError> {
        Ok(
            sqlx::query_as::<_, MediaThumbnail>("SELECT * FROM media_thumbnail WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
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

    /// 某条 Media 的**全部**缩略图，按 `(offset, id)` 升序。**刻意不分页。**
    ///
    /// # 为什么与下面分页的 `list_by_media` 是两个方法
    ///
    /// 上游是**两个用途**，排序也不同：
    ///
    /// | 用途 | 排序 |
    /// |---|---|
    /// | `GET /media/{id}/thumbnails`（本方法） | `offset ASC, id ASC` |
    /// | 运维/巡检翻页看缩略图台账 | `offset` 单键 |
    ///
    /// `id` 这个**次级键不能省**：同一秒上可能有两条（重新生成过就撞上
    /// `(media_id, offset)` 唯一约束之外的脏数据），只按 `offset` 排的话
    /// PostgreSQL 不保证稳定顺序，于是「同一媒体两次请求拿到不同次序」——
    /// 而选图逻辑（`discovery::moment_recommendation` 取中位数）依赖位置语义。
    pub async fn list_all_by_media(&self, media_id: i32) -> Result<Vec<MediaThumbnail>, DbError> {
        Ok(sqlx::query_as::<_, MediaThumbnail>(
            "SELECT * FROM media_thumbnail WHERE media_id = $1 ORDER BY \"offset\", id",
        )
        .bind(media_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 批量取「若干条 Media 在若干个时刻」的缩略图。
    ///
    /// 对应上游 `MediaClipService.load_cover_map` 的两次 `in_` 查询。片段封面
    /// 是**区间首帧**的缩略图，即 `(media_id, start_offset_seconds)` 那一条。
    ///
    /// # 会多取，调用方必须按精确 `(media_id, offset)` 回填
    ///
    /// `media_id = ANY(..) AND offset = ANY(..)` 是**笛卡尔积**上的筛选：
    /// 要 `(1, 100)` 与 `(2, 200)` 两张封面时，只要库里存在 `(1, 200)`，
    /// 它也会被取出来。上游注释写了同样的话（「`in_` 组合可能多取」）。
    ///
    /// 所以调用方要按 `(media_id, offset)` 精确配对，不能按行序。多取的行
    /// 数量是 `|media_ids| x |offsets|` 里实际存在的部分，实践中远小于全表。
    ///
    /// `"offset"` 加引号：它是 PostgreSQL 的保留字。
    ///
    /// # 空输入直接返回空，不发查询
    ///
    /// 片段可能全部是孤立片段（`media_id` 为空），此时两个数组都空。提前
    /// 返回省掉一次往返，也避免 `= ANY('{}')` 的空数组语义被误读。
    pub async fn covers_by_media_offsets(
        &self,
        media_ids: &[i32],
        offsets: &[i32],
    ) -> Result<Vec<MediaThumbnail>, DbError> {
        if media_ids.is_empty() || offsets.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<_, MediaThumbnail>(
            "SELECT * FROM media_thumbnail \
             WHERE media_id = ANY($1) AND \"offset\" = ANY($2)",
        )
        .bind(media_ids)
        .bind(offsets)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 列出某条 Media 在 `[start, end]` **闭区间**内的缩略图，按 offset 升序。
    ///
    /// 对应上游 `_clip_thumbnail_rows`。两端都**含** —— 区间是用两张缩略图
    /// 圈出来的，首尾两张图本身就在区间内；若用开区间，片段的第一帧与最后一帧
    /// 会各丢一张，而那正是前端预览要用的两帧。
    ///
    /// `"offset"` 加引号：PostgreSQL 保留字。
    ///
    /// 走 `media_id` 前缀，区间过滤在内存索引上做 —— 一条 Media 的缩略图数量
    /// 是几十量级（按 N 秒一张），拉全量再过滤比走索引范围更省。
    pub async fn list_in_offset_range(
        &self,
        media_id: i32,
        start: i32,
        end: i32,
    ) -> Result<Vec<MediaThumbnail>, DbError> {
        Ok(sqlx::query_as::<_, MediaThumbnail>(
            "SELECT * FROM media_thumbnail \
             WHERE media_id = $1 AND \"offset\" >= $2 AND \"offset\" <= $3 \
             ORDER BY \"offset\"",
        )
        .bind(media_id)
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await?)
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

    /// 按 id 批量取，返回 `{id: MediaPoint}`。
    ///
    /// 时刻合集点位列表要按成员行批量回填点位本体 —— 逐个合集/逐个成员
    /// 调 [`Self::find_by_id`] 就是 N+1。
    ///
    /// **空列表直接返回空**：`= ANY('{}')` 本身合法，但空入参不该产生一次
    /// 数据库往返。
    ///
    /// 走主键索引。
    pub async fn find_by_ids(
        &self,
        ids: &[i32],
    ) -> Result<std::collections::HashMap<i32, MediaPoint>, DbError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, MediaPoint>("SELECT * FROM media_point WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }

    /// 插入时刻点。参数顺序与上游 `MediaPoint.create(...)` 的关键字一一对应。
    ///
    /// `movie_number` / `video_item_id` 是**快照**（不建外键），所以
    /// `media_id` 可空 —— 来源 Media 被删后时刻点仍然存在并保留展示，
    /// 这是 `on_delete = SET NULL` 的设计意图。
    ///
    /// # ⚠️ 这里此前漏了 `thumbnail_id`（本轮补上）
    ///
    /// DDL 有 `thumbnail_id integer NULL`，模型也声明了该字段，但 INSERT 的列
    /// 清单里**没有它** —— 于是每个时刻点的 `thumbnail_id` 恒为 NULL。
    /// 后果不止「少一列」：上游 `create_point` 的**幂等判据**正是
    /// `WHERE media = ? AND thumbnail = ?`（见 [`Self::find_by_media_and_thumbnail`]），
    /// `thumbnail_id` 恒 NULL 会让那条查询永远命中不了，重复建点变成必然。
    ///
    /// 不提供 upsert：`media_point` 上**没有**唯一索引，同一时刻点可以
    /// 有多行（来源不同就是不同记录）。幂等由 service 层「先查后插」保证。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        &self,
        media_id: Option<i32>,
        thumbnail_id: Option<i32>,
        image_id: i32,
        movie_number: Option<&str>,
        video_item_id: Option<i32>,
        offset_seconds: i32,
    ) -> Result<MediaPoint, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaPoint>(
            "INSERT INTO media_point ( \
                 media_id, thumbnail_id, image_id, movie_number, video_item_id, \
                 offset_seconds, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $7) RETURNING *",
        )
        .bind(media_id)
        .bind(thumbnail_id)
        .bind(image_id)
        .bind(movie_number.map(str::trim).filter(|s| !s.is_empty()))
        .bind(video_item_id)
        .bind(offset_seconds)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(POINT_ENTITY))
    }

    /// 某条 Media 上「指向某个缩略图」的时刻点。**幂等判据。**
    ///
    /// 上游 `create_point` 的第一件事就是这条查询：命中说明同一个
    /// `(media, thumbnail)` 已经建过点，直接返回它并告诉调用方「没新建」。
    ///
    /// `ORDER BY id LIMIT 1`：表上没有唯一索引，理论上可能有多行（历史数据
    /// 或被别处写坏），取最早的一条，与上游 `.order_by(MediaPoint.id).first()`
    /// 一致。
    pub async fn find_by_media_and_thumbnail(
        &self,
        media_id: i32,
        thumbnail_id: i32,
    ) -> Result<Option<MediaPoint>, DbError> {
        Ok(sqlx::query_as::<_, MediaPoint>(
            "SELECT * FROM media_point \
             WHERE media_id = $1 AND thumbnail_id = $2 ORDER BY id LIMIT 1",
        )
        .bind(media_id)
        .bind(thumbnail_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 某条 Media 的**全部**时刻点，按 `id` 升序。**刻意不分页。**
    ///
    /// # 为什么与下面分页的 `list_by_media` 排序不同
    ///
    /// 上游这是**两个端点各自的排序**，不是随手写的：
    ///
    /// | 端点 | 排序 |
    /// |---|---|
    /// | `GET /media/{id}/points`（本方法） | `MediaPoint.id` |
    /// | `GET /media-points`（全局列表） | `created_at` 降序（可传 sort 覆盖）|
    ///
    /// 本方法服务前者。**不要**为了「统一」把它改成按 `offset_seconds` ——
    /// 那会让客户端的点位顺序与上游不一致（骨架文档里就写错过这一条）。
    pub async fn list_all_by_media(&self, media_id: i32) -> Result<Vec<MediaPoint>, DbError> {
        Ok(sqlx::query_as::<_, MediaPoint>(
            "SELECT * FROM media_point WHERE media_id = $1 ORDER BY id",
        )
        .bind(media_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 一批 Media 的**全部**时刻点，按 `(media_id, id)` 升序。**批量版**
    /// [`Self::list_all_by_media`] —— 视频详情的 `media_items` 要一次性给每条媒体
    /// 挂上时刻点，逐条调就是 N+1。
    ///
    /// 排序与上游 `_media_items` 的 `ORDER BY MediaPoint.media, MediaPoint.id`
    /// 逐字一致：先按媒体分组，组内按 id。调用方按 `media_id` 归组即可。
    ///
    /// 空入参直接返回空：`= ANY('{}')` 合法但没必要跑一趟。
    pub async fn list_all_by_media_ids(
        &self,
        media_ids: &[i32],
    ) -> Result<Vec<MediaPoint>, DbError> {
        if media_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<_, MediaPoint>(
            "SELECT * FROM media_point WHERE media_id = ANY($1) ORDER BY media_id, id",
        )
        .bind(media_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 跨媒体的时刻列表（上游 `_point_query_with_image` + `list_media_points`）。
    ///
    /// `kind`：`jav` = 有番号（`movie_number IS NOT NULL`）；`video` = 有视频条目
    /// （`video_item_id IS NOT NULL`）；`None` = 不限。**归属性由快照列判断**，
    /// 因为媒体可能已被删除而时刻还在（两列都是无外键的快照）。
    ///
    /// `exclude_collection_id`：排除**已经在那个合集里**的时刻
    /// （上游 `:504-511`），用于「把还没归档的时刻挑出来」。
    pub async fn list_filtered(
        &self,
        kind: Option<&str>,
        keyword: Option<&str>,
        exclude_collection_id: Option<i32>,
        order_sql: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MediaPoint>, DbError> {
        let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "SELECT p.* FROM media_point p \
               LEFT JOIN video_item vi ON vi.id = p.video_item_id",
        );
        Self::push_point_where(&mut builder, kind, keyword, exclude_collection_id);
        builder.push(" ORDER BY ");
        builder.push(order_sql);
        builder.push(" LIMIT ");
        builder.push_bind(limit);
        builder.push(" OFFSET ");
        builder.push_bind(offset);
        Ok(builder
            .build_query_as::<MediaPoint>()
            .fetch_all(&self.pool)
            .await?)
    }

    /// 与 [`Self::list_filtered`] **同一份**条件的计数。
    pub async fn count_filtered(
        &self,
        kind: Option<&str>,
        keyword: Option<&str>,
        exclude_collection_id: Option<i32>,
    ) -> Result<i64, DbError> {
        let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "SELECT COUNT(*) FROM media_point p \
               LEFT JOIN video_item vi ON vi.id = p.video_item_id",
        );
        Self::push_point_where(&mut builder, kind, keyword, exclude_collection_id);
        let row: (i64,) = builder.build_query_as().fetch_one(&self.pool).await?;
        Ok(row.0)
    }

    /// 条件拼接的**唯一**处（两个查询共用）。
    fn push_point_where(
        builder: &mut sqlx::QueryBuilder<sqlx::Postgres>,
        kind: Option<&str>,
        keyword: Option<&str>,
        exclude_collection_id: Option<i32>,
    ) {
        builder.push(" WHERE 1 = 1");
        let kind_fragment = match kind {
            Some("jav") => Some(" AND p.movie_number IS NOT NULL"),
            Some("video") => Some(" AND p.video_item_id IS NOT NULL"),
            _ => None,
        };
        if let Some(fragment) = kind_fragment {
            builder.push(fragment);
        }
        if let Some(keyword) = keyword.map(str::trim).filter(|raw| !raw.is_empty()) {
            // JAV 匹配番号、video 匹配条目标题 —— 两条都要，因为同一个列表
            // 可以同时装两类时刻。
            let pattern = format!("%{keyword}%");
            builder.push(" AND (p.movie_number ILIKE ");
            builder.push_bind(pattern.clone());
            builder.push(" OR vi.title ILIKE ");
            builder.push_bind(pattern);
            builder.push(")");
        }
        if let Some(collection_id) = exclude_collection_id {
            builder.push(
                " AND NOT EXISTS (SELECT 1 FROM moment_collection_item mci \
                   WHERE mci.collection_id = ",
            );
            builder.push_bind(collection_id);
            builder.push(" AND mci.point_id = p.id)");
        }
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

    /// 删片段。返回是否真的删掉了一行。
    ///
    /// `clip_collection_item.clip_id` 的外键是 `ON DELETE CASCADE`，所以删除
    /// 会把该片段从所有合集里移出，无需显式清理关联行 —— 与上游
    /// `clip.delete_instance()` 一致。
    ///
    /// 存在的理由是**回收**：列表端点判定产物无效时会删行（见
    /// `sm_service::playback::clip_artifact`），那是这条路径的调用方。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM media_clip WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 改片段标题。返回改后的行。
    ///
    /// 对应上游 `update_clip` 的 `save(only=[title, updated_at])`。
    ///
    /// # `updated_at` 必须显式写
    ///
    /// 上游的 `TimestampedMixin` **不自动维护** `updated_at`（上游注释专门
    /// 提醒了这点），所以那里手动赋值。这里同理 —— 忘了写，列表的
    /// `created_at DESC` 排序不会受影响，但任何按「最近修改」展示的地方都会
    /// 读到旧值，而这种缺失不报错。
    ///
    /// `title` 存**裁剪后**的值（上游 `field_validator` 做的 strip），
    /// 裁剪由 service 层负责。
    pub async fn update_title(&self, id: i32, title: &str) -> Result<MediaClip, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaClip>(
            "UPDATE media_clip SET title = $2, updated_at = $3 WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(title.trim())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::business(CLIP_ENTITY, format!("片段 {id} 不存在，无法改标题")))
    }

    /// 该片段所属的合集，按 `name ASC, id ASC`。
    ///
    /// 对应上游 `_load_clip_collections`。返回 `(id, name)` **元组**而不是
    /// 结构体，理由同
    /// [`MovieResolutionLevelRow`](crate::repo::movie::MovieResolutionLevelRow)：
    /// 这是 join 出来的投影行，不是任何表的镜像，而 `pub struct` + `FromRow`
    /// 在本 crate 里的含义是「我映射一张表」，schema 对拍会因此要求一个不存在的
    /// 上游模型。具名类型由消费方（service）定义。
    ///
    /// 排序两级：`name` 让选择器里的合集按名称排列，`id` 兜底。同名不会发生
    /// （`clip_collection.name` 唯一），但次级键让排序在索引变动后仍然确定。
    pub async fn list_collections_for_clip(
        &self,
        clip_id: i32,
    ) -> Result<Vec<(i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, String)>(
            "SELECT c.id, c.name FROM clip_collection c \
             JOIN clip_collection_item i ON i.collection_id = c.id \
             WHERE i.clip_id = $1 \
             ORDER BY c.name ASC, c.id ASC",
        )
        .bind(clip_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 列出某条 Media 的**全部**片段，按 `created_at DESC, id DESC`。**不分页。**
    ///
    /// 对应上游 `list_clips` 的取数那一步（`.order_by(created_at.desc(),
    /// id.desc())`，无 `LIMIT`）。刻意不用上面的 `list_by_media`：那个按
    /// `start_offset_seconds, id` 排序并分页，而这里要的是创建时间倒序的全量 ——
    /// 混用会让「按创建时间」悄悄变成「按区间起点」。
    ///
    /// 两级排序的理由与播放列表那条一致：同一时刻插入的多个片段
    /// `created_at` 会并列，只按它排会让列表在两次刷新之间抖动。
    pub async fn list_all_for_media(&self, media_id: i32) -> Result<Vec<MediaClip>, DbError> {
        Ok(sqlx::query_as::<_, MediaClip>(
            "SELECT * FROM media_clip WHERE media_id = $1 \
             ORDER BY created_at DESC, id DESC",
        )
        .bind(media_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按筛选条件列出片段。**刻意不分页，也不返回总数。**
    ///
    /// 对应上游 `list_media_clips` 里的 `list(query.order_by(*order_by))` ——
    /// 那一行**取回全部匹配行**，`total` 与切片都在 Python 侧做。
    ///
    /// # 为什么这里不能有 `LIMIT` / `OFFSET`
    ///
    /// 上游在过滤**之后**才切页，而过滤（`valid_clips`）要看文件系统 ——
    /// 产物文件是否还在、字节数是否对得上。数据库不知道这些，所以无法把
    /// 分页下推。
    ///
    /// 若这里加了 `LIMIT/OFFSET`，会得到两个**各自成立但互相矛盾**的数：
    /// 页里可能全是即将被回收的无效行，而基于全量算出的 `total` 与页内容
    /// 对不上（页里 5 条、`total` 3 条那种）。所以本方法只负责「取回候选集」，
    /// 由 service 过滤后切片。
    ///
    /// # 动态 SQL 的边界
    ///
    /// 条件个数随关键词数量变化，占位符个数也随之变化，所以这段要走
    /// `safe_sql`（`repo::movie` 里的 crate 内部出口；值仍然是绑定的，只有
    /// 条件**文本**是拼出来的）。排序只有两个取值，因此用两条字面量而
    /// 不是把排序键拼进 SQL —— 排序键是客户端可控的，绝不能进字符串拼接。
    ///
    /// # 番号是**精确**匹配，不是子串
    ///
    /// 上游是 `MediaClip.movie_number == normalized`。子串匹配是
    /// `keyword` 那条路径的事，两者不要混。
    ///
    /// # `NOT IN` 与 NULL
    ///
    /// `exclude_collection_id` 编译成 `id NOT IN (SELECT clip_id FROM ...)`。
    /// 若子查询可能返回 NULL，`NOT IN` 的结果是 NULL（而非 TRUE），那些行会被
    /// **静默漏掉**。`clip_collection_item.clip_id` 是 NOT NULL，所以这里是
    /// 安全的 —— **但该列若改成可空，必须换成 `NOT EXISTS`**，否则排除会静默
    /// 失效。
    pub async fn list_filtered(&self, filter: &ClipFilter) -> Result<Vec<MediaClip>, DbError> {
        let mut conditions: Vec<String> = Vec::new();
        // sqlx 0.9 里 `Arguments` 是 trait，Postgres 的具体类型是 `PgArguments`。
        // 用它而不是逐个 `.bind()`：条件个数随关键词数量变化，而 `.bind()`
        // 的调用次数必须在编译期固定。
        let mut args = sqlx::postgres::PgArguments::default();
        let mut next = 1usize;

        // 1) 番号精确匹配。空串与 None 都不加条件（上游 `if normalized_movie_number:`）。
        if let Some(number) = filter
            .movie_number
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            conditions.push(format!("movie_number = ${next}"));
            add_bind(&mut args, number)?;
            next += 1;
        }

        // 2) 关键词条件。`FilterBuilder` 交出的 SQL 自带 `$1..$n`，且它的
        //    编号从 1 起 —— 所以这里必须把它**重编号**到已绑定值之后。
        //
        //    位移量是 `next - 1`（= 已绑定的值的个数），**不是 `next`**。
        //    `next` 是「下一个编号」，当番号条件缺席时它就是 1，而此时关键词
        //    条件本来就该从 `$1` 起 —— 位移 1 会把整段推到 `$2` 起，留下一个
        //    无人提供的 `$1`，PostgreSQL 报 `could not determine data type of
        //    parameter $1`。
        //
        //    `TRUE` 意味着「没有词」，等价于无过滤，丢掉即可；`FALSE` 意味着
        //    「某个词匹配不到任何字段」，必须保留 —— 它让整条查询返回空，
        //    这正是「不静默丢弃这个词」的效果。
        if let Some(keyword_sql) = filter
            .keyword_sql
            .as_deref()
            .map(str::trim)
            .filter(|sql| !sql.is_empty() && *sql != "TRUE")
        {
            conditions.push(format!("({})", shift_placeholders(keyword_sql, next - 1)));
            for bind in &filter.keyword_binds {
                add_bind(&mut args, bind)?;
            }
            next += filter.keyword_binds.len();
        }

        // 3) 排除某合集内的片段。
        if let Some(collection_id) = filter.exclude_collection_id {
            conditions.push(format!(
                "id NOT IN (SELECT clip_id FROM clip_collection_item \
                 WHERE collection_id = ${next})"
            ));
            add_bind(&mut args, collection_id)?;
            next += 1;
        }

        debug_assert_eq!(
            next - 1,
            args.len(),
            "占位符个数必须与绑定值个数一致，否则值会绑到错的条件上"
        );

        // 排序键是两个**字面量**之一，不接受任何客户端输入参与拼接。
        let order = if filter.created_at_asc {
            "created_at ASC, id ASC"
        } else {
            "created_at DESC, id DESC"
        };
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let sql = format!("SELECT * FROM media_clip {where_clause} ORDER BY {order}");

        Ok(
            sqlx::query_as_with::<_, MediaClip, _>(crate::repo::movie::safe_sql(sql), args)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

/// 往参数列表尾部加一个值。
///
/// `Arguments::add` 返回 `Result`，因为编码可能失败。错误里没有上下文（它
/// 只知道类型不知道列），所以这里补上位置 —— 排查时能立刻知道是第几个
/// 占位符出的问题，而那正是本方法最容易错的地方。
fn add_bind<'q, T>(args: &mut sqlx::postgres::PgArguments, value: T) -> Result<(), DbError>
where
    T: sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    let position = args.len() + 1;
    args.add(value).map_err(|err| {
        DbError::business("MediaClip", format!("绑定第 {position} 个参数失败: {err}"))
    })
}

/// 把一段 SQL 里的 `$n` 占位符整体后移 `by` 个。
///
/// `sm_service::playback::search_filters::FilterBuilder` 生成的关键词条件自带
/// `$1..$k` 编号，而外层查询可能已经绑了番号（`$1`）。直接拼起来会让两段
/// 编号**重叠** —— 而重叠不会报错，PostgreSQL 只是把后绑的值交给前一个位置，
/// 于是「按番号筛选」悄悄变成「按关键词的第一个词筛选」。
///
/// # 只认「`$` + 数字」，其余原样保留
///
/// - `$` 后不接数字：不是占位符，原样输出；
/// - `$$`：PostgreSQL 的美元引用起始标记，成对出现时整体跳过，
///   否则 `$$1` 会被误读成 `$$` + 占位符 `$1`。
///
/// 绑定值里的 `$` 不受影响 —— 值是**绑定的**，从不拼进 SQL 文本，所以要防的
/// 只有这段拼接文本，而它的内容全部由本仓库的代码生成。
fn shift_placeholders(sql: &str, by: usize) -> String {
    if by == 0 {
        return sql.to_owned();
    }
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            // 美元引用：两个 `$` 一起跳过，后面的内容不是编号。
            if bytes.get(i + 1) == Some(&b'$') {
                out.push_str("$$");
                i += 2;
                continue;
            }
            let digits_start = i + 1;
            let mut j = digits_start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start {
                // `$` + 数字：整体后移。
                let value: usize = sql[digits_start..j].parse().expect("全是 ASCII 数字");
                out.push('$');
                out.push_str(&(value + by).to_string());
                i = j;
                continue;
            }
        }
        // 非占位符字节：按 UTF-8 字符推进，避免把多字节字符截断。
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&sql[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// 一个 ASCII 字符的 UTF-8 长度。
fn utf8_len(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte < 0xE0 {
        2
    } else if byte < 0xF0 {
        3
    } else {
        4
    }
}

/// 片段列表的筛选条件。
///
/// 由 service 层组装：关键词条件来自
/// `sm_service::playback::search_filters::FilterBuilder`，其余是标量筛选。
#[derive(Debug, Clone, Default)]
pub struct ClipFilter {
    /// 精确匹配来源番号快照。`None` 或空串 = 不限。
    pub movie_number: Option<String>,
    /// 关键词条件 SQL（自带 `$1..$n`）。`None`、`""` 或 `"TRUE"` = 不限。
    pub keyword_sql: Option<String>,
    /// 与 `keyword_sql` 占位符一一对应，顺序即编号升序。
    pub keyword_binds: Vec<String>,
    /// 排除该合集内的片段。
    pub exclude_collection_id: Option<i32>,
    /// `true` = `created_at:asc`，`false` = `created_at:desc`（默认）。
    pub created_at_asc: bool,
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

    // -------------------------------------------------- 占位符重编号

    /// 重编号是这个函数存在的全部理由，所以它的行为要逐条钉住。
    #[test]
    fn placeholders_shift_by_the_offset() {
        assert_eq!(shift_placeholders("a = $1", 1), "a = $2");
        assert_eq!(
            shift_placeholders("a = $1 AND b = $2", 3),
            "a = $4 AND b = $5"
        );
        assert_eq!(
            shift_placeholders("($1) AND ($2 OR $3)", 10),
            "($11) AND ($12 OR $13)"
        );
    }

    #[test]
    fn a_zero_offset_is_the_identity() {
        assert_eq!(shift_placeholders("a = $1 AND $2", 0), "a = $1 AND $2");
    }

    /// `$` 后面没有数字时**不是**占位符，必须原样保留。
    ///
    /// 误改的后果是 SQL 语法错误或语义变化 —— 比如字面量里的 `$$`。
    #[test]
    fn a_dollar_without_digits_is_left_alone() {
        assert_eq!(shift_placeholders("a = $$1", 5), "a = $$1");
        assert_eq!(shift_placeholders("cost $ 5", 5), "cost $ 5");
        assert_eq!(
            shift_placeholders("no placeholder here", 5),
            "no placeholder here"
        );
    }

    /// 多位数编号要整体平移，不能只改个位。
    ///
    /// `$9` 加 1 应得 `$10`。若按字节处理就会得到 `$10` 但把 `1`、`0` 拆错。
    #[test]
    fn multi_digit_placeholders_shift_as_a_whole() {
        assert_eq!(shift_placeholders("$9", 1), "$10");
        assert_eq!(shift_placeholders("$99", 1), "$100");
    }

    /// 关键词条件里含中文（`ILIKE` 的字面量、列名旁的说明）时不能截断字符。
    #[test]
    fn multibyte_text_survives_the_rewrite() {
        assert_eq!(
            shift_placeholders("t ILIKE '%' || $1 || '%' AND 番号 IS NOT NULL", 2),
            "t ILIKE '%' || $3 || '%' AND 番号 IS NOT NULL"
        );
        assert_eq!(shift_placeholders("编号 = $1", 1), "编号 = $2");
    }

    /// 真实形状：番号已绑 `$1`，关键词条件从 `$1` 起，必须让开。
    ///
    /// 这就是那个**不报错**的 bug 的原型 —— 不重编号的话两段都指向 `$1`。
    #[test]
    fn a_keyword_filter_never_collides_with_an_earlier_bind() {
        let keyword = shift_placeholders(
            "(c ILIKE '%' || $1 || '%' OR c ILIKE '%' || $2 || '%') AND (title ILIKE '%' || $3 || '%')",
            1,
        );
        assert_eq!(
            keyword,
            "(c ILIKE '%' || $2 || '%' OR c ILIKE '%' || $3 || '%') AND (title ILIKE '%' || $4 || '%')"
        );
        // 编号必须严格递增且无重复
        let numbers: Vec<usize> = keyword
            .split('$')
            .skip(1)
            .filter_map(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .collect();
        let mut sorted = numbers.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(numbers, sorted, "占位符必须唯一：{numbers:?}");
    }

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
