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

/// `video_item` 的局部更新。**每个 `None` 都表示「不动这一列」。**
///
/// # 为什么 `release_date` 是两层 `Option`
///
/// ```text
/// None      -> 不动（字段没给，或给了 null）
/// Some(None) -> 清空为 NULL
/// Some(Some(d)) -> 设为 d
/// ```
///
/// 上游 `update_video` 对三个字段的**显式 null** 语义并不一致：
///
/// | 字段 | 给了 null 时 |
/// |---|---|
/// | `title` / `summary` | **忽略**（`if ... is not None` 才赋值） |
/// | `release_date` | **清空**（`if "release_date" in update_data`） |
///
/// 两层 `Option` 就是这个差异的编码。合成一个 `Option<T>` 会让「清空」与
/// 「不动」无法区分，而两者的最终行状态不同。
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoItemFields<'a> {
    /// 标题。空白由 service 层拦成 422，这里不做判断。
    pub title: Option<&'a str>,
    /// 简介。空串是合法值（清空简介）。
    pub summary: Option<&'a str>,
    /// 发布日期。见类型文档的两层 `Option` 说明。
    pub release_date: Option<Option<chrono::NaiveDateTime>>,
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

    /// 按 id 批量取，返回 `{id: VideoItem}`。
    ///
    /// 合集列表要按成员行批量回填成员本体（封面取首个成员的条目封面）
    /// —— 逐个成员调 [`Self::find_by_id`] 就是 N+1。
    ///
    /// **空列表直接返回空**：空入参不该产生一次数据库往返。
    pub async fn find_by_ids(
        &self,
        ids: &[i32],
    ) -> Result<std::collections::HashMap<i32, VideoItem>, DbError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, VideoItem>("SELECT * FROM video_item WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
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

    /// 局部更新。一条 UPDATE，只推进一次 `updated_at`。
    ///
    /// 封面**不在**这个结构里：它由 [`Self::set_cover`] 单独写。原因是
    /// 上游换封面会连带「旧封面图不再被引用 → 清理磁盘文件」，那一步需要
    /// service 层知道旧值，混进这条 SQL 就看不见了。
    ///
    /// 返回 `None` 表示行不存在 —— 调用方已经做过 404 判定，这里是兜底。
    pub async fn update_fields(
        &self,
        id: i32,
        fields: &VideoItemFields<'_>,
    ) -> Result<Option<VideoItem>, DbError> {
        // `release_date` 用「标志位 + 值」两个参数表达「不动 / 清空 / 赋值」
        // 三态：`COALESCE` 做不到，因为 NULL 本身就是一个合法的目标值。
        let has_release_date = fields.release_date.is_some();
        let release_date = fields.release_date.flatten();
        sqlx::query_as::<_, VideoItem>(
            "UPDATE video_item SET \
                 title = COALESCE($2, title), \
                 summary = COALESCE($3, summary), \
                 release_date = CASE WHEN $4 THEN $5 ELSE release_date END, \
                 updated_at = $6 \
             WHERE id = $1 \
             RETURNING *",
        )
        .bind(id)
        .bind(fields.title)
        .bind(fields.summary)
        .bind(has_release_date)
        .bind(release_date)
        .bind(crate::common::time::now_utc())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))
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

// ================================================================ 列表项组装

/// 「首条有效媒体的时长」的 `ORDER BY` 片段。
///
/// **抽成常量是因为合集成员端点要一模一样的片段** —— 那边多联结了一张
/// `video_collection_item`（别名 `vci`），但这一条只看 `v.id` 与 `media`，
/// 所以可以逐字复用。抄两遍就会在某个时刻分叉。
pub(crate) const VIDEO_FIRST_MEDIA_DURATION_COLUMN: &str = "COALESCE((SELECT m.duration_seconds \
     FROM media m WHERE m.video_item_id = v.id AND m.valid ORDER BY m.id LIMIT 1), 0)";

/// 「首条有效媒体的文件大小」的 `ORDER BY` 片段。见上。
pub(crate) const VIDEO_FIRST_MEDIA_FILE_SIZE_COLUMN: &str = "COALESCE((SELECT m.file_size_bytes \
     FROM media m WHERE m.video_item_id = v.id AND m.valid ORDER BY m.id LIMIT 1), 0)";

/// 条目列表的排序键 → `ORDER BY` 片段。**白名单。**
///
/// 键与 `sm_service::videos::VideoSort::as_str()` **逐字对应** ——
/// 那边负责「用户给的 `field:direction` 合不合法」（含 `position` 这种只有
/// 合集成员才允许的键），这里负责「合法的键落成哪个 SQL 片段」。
/// 自由字符串会被拼进 SQL，所以片段只能从这张表里取。
///
/// # `duration` / `file_size` 排的不是本表列
///
/// 它们排的是**「首条有效媒体」**的两列（`Media.id` 最小且 `valid`）。
/// 上游写成 `MIN(Media.id)` 分组子查询 + 两次 `LEFT JOIN` +
/// `COALESCE(..., 0)`；这里用相关子查询表达**同一语义**：
///
/// - 取的是同一个值（`ORDER BY m.id LIMIT 1` 即 `MIN(id)`，且都限定 `valid`）；
/// - `COALESCE(..., 0)` 让「没有有效媒体」按 **0** 参与排序。少了它，`DESC`
///   会把 NULL 排到最前（PostgreSQL 的 `DESC` 是 NULLS FIRST），与上游不符。
pub const VIDEO_LIST_SORT_FIELD_MAP: [(&str, &str); 4] = [
    ("created_at", "v.created_at"),
    ("title", "v.title"),
    ("duration", VIDEO_FIRST_MEDIA_DURATION_COLUMN),
    ("file_size", VIDEO_FIRST_MEDIA_FILE_SIZE_COLUMN),
];

/// 取排序片段。不在白名单 → `None`（调用方应报 422，而不是拼一条可疑 SQL）。
pub fn video_list_sort_column(key: &str) -> Option<&'static str> {
    VIDEO_LIST_SORT_FIELD_MAP
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, column)| *column)
}

/// 每个条目的媒体统计：`(video_item_id, media_count, valid_count)`。
///
/// **投影行**（两列都是聚合值），不是任何单表的镜像 —— 所以是元组别名而不是
/// `pub struct`。取舍记录见 `repo/movie.rs::MovieResolutionLevelRow`。
pub type VideoMediaStats = (i32, i64, i64);

/// 每个条目的**首条有效媒体**：`(video_item_id, media_id, duration_seconds,
/// file_size_bytes, resolution)`。**投影行**，同上。
///
/// `media_id` 在里面是因为合集成员端点的 `first_media_id` 要它
/// （上游 `COALESCE(first_media.id, 0)` 那个哨兵值，归一为 `None`）。
pub type VideoFirstMedia = (i32, i32, i32, i64, Option<String>);

/// 条目归属的合集：`(video_item_id, collection_id, name)`。
///
/// **`video_collection_item ⋈ video_collection` 的 JOIN 投影**，同上。
pub type VideoCollectionRefRow = (i32, i32, String);

impl VideoItemRepository {
    /// 分页列出条目，可选标题子串过滤与排序。返回 `(本页, 总数)`。
    ///
    /// # 排序键必须来自 [`VIDEO_LIST_SORT_FIELD_MAP`]
    ///
    /// 不在白名单 → **运行时** `Err`，不是 `debug_assert!`：后者在 release 下
    /// 被编译掉，那这份白名单就等于不存在，而它挡的是 SQL 注入。
    ///
    /// # 过滤用 `LIKE`，与同文件的 `search_by_title` 的 `ILIKE` 不同
    ///
    /// 上游 `_filtered_query` 是 `VideoItem.title.contains(...)`，落到
    /// PostgreSQL 上是 **`LIKE`（区分大小写）**。这里照上游；`search_by_title`
    /// 的 `ILIKE` 是另一处（**它目前没有任何调用方**，所以两者暂时不会互相
    /// 矛盾）。按当前仓库口径若要统一，改这里一个词即可 —— 但那会偏离上游。
    pub async fn list_page(
        &self,
        query: Option<&str>,
        sort_key: &str,
        descending: bool,
        request: &PageRequest,
    ) -> Result<(Vec<VideoItem>, i64), DbError> {
        let column = video_list_sort_column(sort_key).ok_or_else(|| {
            DbError::business(VIDEO_ITEM_ENTITY, format!("未知的排序键：{sort_key:?}"))
        })?;
        // 方向只可能是这两个字面量之一，所以拼进去没有注入面。
        let direction = if descending { "DESC" } else { "ASC" };
        let pattern = query.map(|raw| format!("%{raw}%"));
        let filter = "WHERE ($1::text IS NULL OR v.title LIKE $1)";

        let total: i64 = sqlx::query_scalar(super::movie::safe_sql(format!(
            "SELECT COUNT(*) FROM video_item v {filter}"
        )))
        .bind(pattern.as_deref())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))?;

        let sql = format!(
            "SELECT v.* FROM video_item v {filter} \
             ORDER BY {column} {direction}, v.id {direction} LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query_as::<_, VideoItem>(super::movie::safe_sql(sql))
            .bind(pattern.as_deref())
            .bind(request.limit())
            .bind(request.offset())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(VIDEO_ITEM_ENTITY))?;
        Ok((rows, total))
    }

    /// 一批条目的媒体统计。返回 `{video_item_id: (media_count, valid_count)}`。
    ///
    /// `can_play` 的判据就是 `valid_count > 0`（上游 `bool(row.valid_count)`）。
    ///
    /// 用 `SUM(CASE WHEN valid THEN 1 ELSE 0 END)` 而不是 `SUM(valid)` ——
    /// PostgreSQL **不支持对 `boolean` 求和**（上游注释也是这么写的）。
    /// 没有媒体的条目不会出现在结果里，调用方按 `(0, false)` 兜底。
    pub async fn media_stats(
        &self,
        video_ids: &[i32],
    ) -> Result<std::collections::HashMap<i32, (i64, i64)>, DbError> {
        if video_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, VideoMediaStats>(
            "SELECT video_item_id, COUNT(*) AS media_count, \
                    SUM(CASE WHEN valid THEN 1 ELSE 0 END) AS valid_count \
             FROM media WHERE video_item_id = ANY($1) GROUP BY video_item_id",
        )
        .bind(video_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, count, valid)| (id, (count, valid)))
            .collect())
    }

    /// 一批条目的**首条有效媒体**。返回
    /// `{video_item_id: (media_id, duration_seconds, file_size_bytes, resolution)}`。
    ///
    /// `DISTINCT ON (video_item_id) … ORDER BY video_item_id, id` —— 每个条目
    /// 取 `Media.id` 最小的那条**有效**媒体，与上游那个
    /// `MIN(Media.id) WHERE valid GROUP BY video_item` 子查询同值。
    ///
    /// 没有有效媒体的条目**不出现在结果里**，调用方按
    /// `(0, 0, 0, None)` 兜底（上游 `COALESCE(..., 0)` 是同一件事）。
    pub async fn first_valid_media(
        &self,
        video_ids: &[i32],
    ) -> Result<std::collections::HashMap<i32, (i32, i32, i64, Option<String>)>, DbError> {
        if video_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, VideoFirstMedia>(
            "SELECT DISTINCT ON (video_item_id) video_item_id, id, duration_seconds, \
                    file_size_bytes, resolution \
             FROM media WHERE video_item_id = ANY($1) AND valid \
             ORDER BY video_item_id, id",
        )
        .bind(video_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, media_id, duration, size, resolution)| {
                (id, (media_id, duration, size, resolution))
            })
            .collect())
    }

    /// 一批条目归属的合集。返回 `{video_item_id: [(collection_id, name), …]}`。
    ///
    /// 按 `(name, id)` 升序 —— 上游 `_collections_map` 就是这么排的，客户端
    /// 按它渲染「所属合集」标签，**顺序是可见的**。
    pub async fn collections_map(
        &self,
        video_ids: &[i32],
    ) -> Result<std::collections::HashMap<i32, Vec<(i32, String)>>, DbError> {
        if video_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = sqlx::query_as::<_, VideoCollectionRefRow>(
            "SELECT vci.video_item_id, vc.id, vc.name \
             FROM video_collection_item vci \
             JOIN video_collection vc ON vc.id = vci.collection_id \
             WHERE vci.video_item_id = ANY($1) \
             ORDER BY vc.name, vc.id",
        )
        .bind(video_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut out: std::collections::HashMap<i32, Vec<(i32, String)>> =
            std::collections::HashMap::new();
        for (video_id, collection_id, name) in rows {
            out.entry(video_id).or_default().push((collection_id, name));
        }
        Ok(out)
    }
}
