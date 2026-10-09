//! `media_point`、`media_thumbnail`、`moment_recommendation` 三张表的仓储。
//!
//! 对应上游 `src/service/discovery/moment_recommendation_service.py`（580 行）。
//! 单独成文件而不并进 [`discovery`](super::discovery)：那三张表里
//! `media_point` / `media_thumbnail` 属 **playback** 域，只有
//! `moment_recommendation` 属 discovery。放一起会让「域」的边界失真。
//!
//! # 一律用自定义投影，不加载完整模型
//!
//! 与 `discovery::hot_actress_release` 同一个理由：选图阶段候选可能有上千条
//! 缩略图，带上完整 `Media` / `Movie` / `Image` 会把内存拖垮。这里每个查询
//! 只取打分与选图真正用到的列。
//!
//! # `media_thumbnail."offset"` 必须加引号
//!
//! `offset` 是 SQL 保留字，DDL 里写的是 `"offset"`（`schema.sql:265`）。
//! 漏引号不会报错于编译期，只会在运行期报语法错 —— 这类错误在重构期最容易漏。
//!
//! # 三处「有效」过滤**不一样**，照抄不统一
//!
//! | 查询 | 过滤 |
//! |---|---|
//! | [`MomentSeedRepository::load_seeds`] | `media.valid`（**无**黑名单过滤） |
//! | [`MediaThumbnailRepository::by_ids`] | `media.valid`（**无**黑名单过滤） |
//! | [`MomentRecommendationRepository::list_valid`] | `media.valid` **且** `movie.is_blacklisted = false` |
//!
//! 前两者没有黑名单过滤是上游原样：`Movie.is_blacklisted` 只出现在
//! `_thumbnail_query`（`:125`）与 `list_items`（`:517`）里。
//! **「统一」成都有过滤会悄悄改变种子池** —— 黑名单影片的打点会消失，
//! 而它是用户明确标记不该出现的片子。
//!
//! # `moment_recommendation` 是**整表替换**，不是增量
//!
//! 上游 `:494-498`：先 `MomentRecommendation.delete().execute()` 再
//! `insert_many`。所以 `rank` 与 `thumbnail_id` 上的 UNIQUE 约束不会撞车
//! —— 表在插入那一刻是空的。
//!
//! 换句话说**没有「部分失败」**：要么整批新池子生效，要么（事务回滚）旧池子
//! 原样保留。绝不能写成「按 generated_at 分区只删旧的」—— 那会留下一批
//! 上次没生成过的孤儿行。

use sqlx::PgPool;

use crate::error::DbError;

const SEED_ENTITY: &str = "MediaPoint";
const THUMBNAIL_ENTITY: &str = "MediaThumbnail";
const RECOMMENDATION_ENTITY: &str = "MomentRecommendation";

/// 种子投影：一条待取向量的打点。
///
/// 对应上游 `_MomentSeed`（`:66-73`）里用到的字段。`recency_score` **不在
/// 这里** —— 它由位置算出，放在 service 层（`recency_score(index, total)`）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MomentSeedRow {
    pub point_id: i32,
    pub media_id: i32,
    pub thumbnail_id: i32,
    /// 该媒体所属影片。**经 `movie_number` 关联**，不是直连外键。
    pub movie_id: i32,
    pub offset_seconds: i32,
    /// 该媒体时长。
    ///
    /// **DDL 是 `integer NOT NULL DEFAULT 0`**，不是可空 —— 「时长未知」在
    /// 库里表现为 **0**。所以调用方要按「`<= 0` 即未知」处理，而不是靠
    /// `Option` 判断。上游 `media.duration_seconds or 0` 兜的是同一件事。
    pub duration_seconds: i32,
}

/// 缩略图投影：选图与打分需要的全部列。
///
/// `is_collection` 带上是因为三个采集源都要跳过合集条目
/// （`:232` / `:333`）—— 那是「推荐时刻」不面向合集的理由。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MediaThumbnailRow {
    pub thumbnail_id: i32,
    pub media_id: i32,
    pub movie_id: i32,
    /// 该缩略图在影片里的偏移（秒）。列名是 `"offset"`。
    pub offset: i32,
    /// 图片相对路径。**未签名** —— 签名在 API 层做（要密钥）。
    pub image_origin: String,
    pub duration_seconds: i32,
    /// `movie.heat` 是 `NOT NULL DEFAULT 0` —— 「没热度」是 0，不是 `NULL`。
    pub movie_heat: i32,
    pub movie_is_collection: bool,
}

/// 热门候选投影（源 C）。对应上游 `:362-367`。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PopularMovieRow {
    pub movie_id: i32,
    /// `NOT NULL DEFAULT 0`。
    pub heat: i32,
}

/// 落库的一行时刻推荐。对应上游 `:473-493` 的 `rows` 字典。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NewMomentRecommendation {
    /// 从 1 开始的连续名次。**`rank` 上有 UNIQUE 约束**。
    pub rank: i32,
    pub score: f64,
    pub strategy: String,
    pub reason: String,
    pub movie_id: i32,
    pub media_id: i32,
    /// **`thumbnail_id` 上也有 UNIQUE 约束** —— 一张缩略图只能入选一次。
    pub thumbnail_id: i32,
    pub offset_seconds: i32,
    pub seed_point_id: Option<i32>,
    pub seed_thumbnail_id: Option<i32>,
    pub source_movie_id: Option<i32>,
    pub visual_score: Option<f64>,
    pub movie_similarity_score: Option<f64>,
}

/// 读侧的一行。对应上游 `MomentRecommendation` 模型 + 两次 JOIN。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MomentRecommendationRow {
    pub recommendation_id: i32,
    pub rank: i32,
    pub score: f64,
    pub strategy: String,
    pub reason: String,
    pub media_id: i32,
    pub thumbnail_id: i32,
    pub offset_seconds: i32,
    pub movie_id: i32,
}

/// 取种子打点。
#[derive(Debug, Clone)]
pub struct MomentSeedRepository {
    pool: PgPool,
}

impl MomentSeedRepository {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 最近的打点，按**创建时间倒序**取前 `limit` 条。
    ///
    /// 上游 `_load_seeds`（`:136-163`）：
    ///
    /// ```python
    /// .order_by(MediaPoint.created_at.desc(), MediaPoint.id.desc())
    /// .limit(max(int(limit), 0))
    /// ```
    ///
    /// # 两级排序不是冗余
    ///
    /// `created_at` 是**可空**的（DDL `timestamp NULL`）。一批同批写入的行
    /// 时间戳可能相同，此时顺序由 `id` 定 —— 只按 `created_at` 排的话，
    /// 并列行的顺序由数据库随意给出，而它会经 `recency_score` 直接变成得分。
    /// 也就是说**不稳定排序会变成不稳定打分**。
    ///
    /// # `movie_number` 而非 `movie_id`
    ///
    /// `media.movie` 存的是**番号字符串**，要经 `movie.movie_number` 才能
    /// 拿到 `movie.id`。`media_point.movie_id` 那列（DDL 存在）**不能用**：
    /// 上游不读它，它由另一个写入路径维护。
    pub async fn load_seeds(&self, limit: i64) -> Result<Vec<MomentSeedRow>, DbError> {
        let sql = r#"
            SELECT mp.id            AS point_id,
                   mp.media_id     AS media_id,
                   mp.thumbnail_id AS thumbnail_id,
                   m.id            AS movie_id,
                   mp.offset_seconds AS offset_seconds,
                   med.duration_seconds AS duration_seconds
            FROM media_point mp
            JOIN media med ON med.id = mp.media_id
            JOIN movie m ON m.movie_number = med.movie_number
            JOIN media_thumbnail mt ON mt.id = mp.thumbnail_id
            WHERE med.valid = true
            ORDER BY mp.created_at DESC, mp.id DESC
            LIMIT $1
        "#;
        let rows = sqlx::query_as::<_, MomentSeedRow>(sql)
            .bind(limit.max(0))
            .fetch_all(&self.pool)
            .await
            .map_err(|error| DbError::business(SEED_ENTITY, format!("取种子失败：{error}")))?;
        Ok(rows)
    }
}

/// 缩略图读取。
#[derive(Debug, Clone)]
pub struct MediaThumbnailRepository {
    pool: PgPool,
}

impl MediaThumbnailRepository {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按 id 批量取。
    ///
    /// 上游 `_get_thumbnails_by_ids`（`:128-134`）。**空列表直接返回空** ——
    /// 上游 `if not thumbnail_ids: return {}`，而 `IN ()` 在 SQL 里是语法错。
    ///
    /// 调用方是 Qdrant 命中之后（可能有几十个 id），所以走 `= ANY($1)` 而不是
    /// 拼 `IN (...)`。
    pub async fn by_ids(&self, thumbnail_ids: &[i32]) -> Result<Vec<MediaThumbnailRow>, DbError> {
        if thumbnail_ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = r#"
            SELECT mt.id            AS thumbnail_id,
                   mt.media_id     AS media_id,
                   m.id            AS movie_id,
                   mt."offset"     AS offset,
                   i.origin        AS image_origin,
                   med.duration_seconds AS duration_seconds,
                   m.heat          AS movie_heat,
                   m.is_collection AS movie_is_collection
            FROM media_thumbnail mt
            JOIN image i ON i.id = mt.image_id
            JOIN media med ON med.id = mt.media_id
            JOIN movie m ON m.movie_number = med.movie_number
            WHERE mt.id = ANY($1)
              AND med.valid = true
        "#;
        let rows = sqlx::query_as::<_, MediaThumbnailRow>(sql)
            .bind(thumbnail_ids)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| DbError::business(THUMBNAIL_ENTITY, format!("取缩略图失败：{error}")))?;
        Ok(rows)
    }

    /// 某部影片**全部**媒体的缩略图，按 `(media_id, offset, thumbnail_id)` 升序。
    ///
    /// 上游 `_choose_thumbnail_for_movie`（`:281-307`）。调用方在内存里按
    /// `media_id` 分组，每组用 [`MediaThumbnailRepository::pick_closest`] 选一张。
    ///
    /// **不分页** —— 一部影片的缩略图是有限的几百张。
    pub async fn by_movie(&self, movie_id: i32) -> Result<Vec<MediaThumbnailRow>, DbError> {
        let sql = r#"
            SELECT mt.id            AS thumbnail_id,
                   mt.media_id     AS media_id,
                   m.id            AS movie_id,
                   mt."offset"     AS offset,
                   i.origin        AS image_origin,
                   med.duration_seconds AS duration_seconds,
                   m.heat          AS movie_heat,
                   m.is_collection AS movie_is_collection
            FROM media_thumbnail mt
            JOIN image i ON i.id = mt.image_id
            JOIN media med ON med.id = mt.media_id
            JOIN movie m ON m.movie_number = med.movie_number
            WHERE m.id = $1
              AND med.valid = true
            ORDER BY med.id ASC, mt."offset" ASC, mt.id ASC
        "#;
        let rows = sqlx::query_as::<_, MediaThumbnailRow>(sql)
            .bind(movie_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| DbError::business(THUMBNAIL_ENTITY, format!("取缩略图失败：{error}")))?;
        Ok(rows)
    }

    /// 从**已按 offset 升序**的列表里挑最接近 `desired_offset` 的那张。
    ///
    /// 上游 `_choose_thumbnail_from_media_thumbnails`（`:258-270`）：
    ///
    /// ```python
    /// return min(thumbnails, key=lambda item: (abs(int(item.offset) - desired_offset), item.id))
    /// ```
    ///
    /// **二级排序键 `id` 不能省** —— 多张缩略图可能同偏移（重复抓帧），
    /// 少了它每次选出的都可能是另一张，推荐图会随机变。
    pub fn pick_closest(
        thumbnails: &[MediaThumbnailRow],
        desired_offset: i64,
    ) -> Option<&MediaThumbnailRow> {
        thumbnails
            .iter()
            .min_by_key(|item| ((item.offset as i64 - desired_offset).abs(), item.thumbnail_id))
    }
}

/// 时刻推荐的读写。
#[derive(Debug, Clone)]
pub struct MomentRecommendationRepository {
    pool: PgPool,
}

impl MomentRecommendationRepository {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 热门候选（源 C 的输入）。上游 `:362-367`。
    ///
    /// # `limit * 5` 是「多取点再筛掉选不出图的」
    ///
    /// 上游 `.limit(max(limit * 5, limit, 1))`。取 5 倍是因为一部热门影片
    /// 可能**一张缩略图都选不出来**（无图 / 全部无效），而热门源是**兜底**
    /// —— 收集够 `limit` 个候选就停（`:388-389`）。取少了会让热门源白跑一趟
    /// 却仍然凑不满。
    ///
    /// 排序三键：`heat DESC, created_at DESC, id DESC`。后两段是 tie-break ——
    /// 同热度时不靠数据库随意返回。
    ///
    /// **只排除合集，不排除黑名单** —— 与上游一致（`:364` 只有
    /// `is_collection == False`）。
    pub async fn popular_movies(&self, limit: i64) -> Result<Vec<PopularMovieRow>, DbError> {
        let sql = r#"
            SELECT m.id   AS movie_id,
                   m.heat AS heat
            FROM movie m
            WHERE m.is_collection = false
            ORDER BY m.heat DESC, m.created_at DESC, m.id DESC
            LIMIT $1
        "#;
        let rows = sqlx::query_as::<_, PopularMovieRow>(sql)
            .bind((limit * 5).max(limit).max(1))
            .fetch_all(&self.pool)
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        Ok(rows)
    }

    /// **整表替换**：先清空，再插入新池子。
    ///
    /// 上游 `:494-498`。两步在**同一个事务**里 —— 中间态（旧池已删、新池
    /// 未插）会让并发读侧看到空列表。
    ///
    /// # 空候选也要执行删除
    ///
    /// 上游注释（`:495`）：「空候选表示清空旧池，而不是继续展示过期数据」。
    /// 所以 `rows` 为空时**只删不插**，且这不算错误 —— 返回 `Ok(0)`。
    /// 写成「没候选就跳过」会让过期推荐一直挂在界面上。
    pub async fn replace_all(
        &self,
        rows: &[NewMomentRecommendation],
        generated_at: chrono::NaiveDateTime,
    ) -> Result<u64, DbError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        sqlx::query("DELETE FROM moment_recommendation")
            .execute(&mut *tx)
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        let mut inserted = 0u64;
        for row in rows {
            let sql = r#"
                INSERT INTO moment_recommendation (
                    rank, score, strategy, reason, movie_id, media_id, thumbnail_id,
                    offset_seconds, seed_point_id, seed_thumbnail_id, source_movie_id,
                    visual_score, movie_similarity_score, generated_at,
                    created_at, updated_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $14, $14)
            "#;
            let result = sqlx::query(sql)
                .bind(row.rank)
                .bind(row.score)
                .bind(&row.strategy)
                .bind(&row.reason)
                .bind(row.movie_id)
                .bind(row.media_id)
                .bind(row.thumbnail_id)
                .bind(row.offset_seconds)
                .bind(row.seed_point_id)
                .bind(row.seed_thumbnail_id)
                .bind(row.source_movie_id)
                .bind(row.visual_score)
                .bind(row.movie_similarity_score)
                .bind(generated_at)
                .execute(&mut *tx)
                .await
                .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
            inserted += result.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        Ok(inserted)
    }

    /// 仍然有效的推荐，按 `rank` 升序，分页。
    ///
    /// 上游 `:513-532`。**分页与 [`Self::count_valid`] 用同一套过滤** ——
    /// 否则失效的行会占掉分页槽位，表现为最后一页条目不足而总数偏大。
    pub async fn list_valid(
        &self,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<MomentRecommendationRow>, DbError> {
        let sql = r#"
            SELECT mr.id         AS recommendation_id,
                   mr.rank       AS rank,
                   mr.score      AS score,
                   mr.strategy   AS strategy,
                   mr.reason     AS reason,
                   mr.media_id   AS media_id,
                   mr.thumbnail_id AS thumbnail_id,
                   mr.offset_seconds AS offset_seconds,
                   mr.movie_id   AS movie_id
            FROM moment_recommendation mr
            JOIN media med ON med.id = mr.media_id
            JOIN movie m ON m.id = mr.movie_id
            WHERE med.valid = true
              AND m.is_blacklisted = false
            ORDER BY mr.rank ASC
            OFFSET $1 LIMIT $2
        "#;
        let rows = sqlx::query_as::<_, MomentRecommendationRow>(sql)
            .bind(offset.max(0))
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        Ok(rows)
    }

    /// 有效推荐总数。**不是 `COUNT(*)`**（见模块文档）。
    pub async fn count_valid(&self) -> Result<i64, DbError> {
        let sql = r#"
            SELECT COUNT(*)
            FROM moment_recommendation mr
            JOIN media med ON med.id = mr.media_id
            JOIN movie m ON m.id = mr.movie_id
            WHERE med.valid = true
              AND m.is_blacklisted = false
        "#;
        let total: (i64,) = sqlx::query_as(sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        Ok(total.0)
    }

    /// 最近一次生成时间。**刻意不带有效过滤**（上游 `:521-528`）。
    ///
    /// 上游那个查询只 `ORDER BY generated_at DESC LIMIT 1`，没有
    /// `Media.valid` / 黑名单条件。所以池子里最新那批若全失效，
    /// `generated_at` 仍显示那个时间而 `items` 是空的。
    /// **照抄**：「修正」成一致会让客户端在「换了池子但都是失效行」时
    /// 看不到时间戳。
    pub async fn latest_generated_at(&self) -> Result<Option<chrono::NaiveDateTime>, DbError> {
        let sql = "SELECT generated_at FROM moment_recommendation ORDER BY generated_at DESC LIMIT 1";
        let row: Option<(chrono::NaiveDateTime,)> = sqlx::query_as(sql)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| DbError::business(RECOMMENDATION_ENTITY, format!("时刻推荐表操作失败：{error}")))?;
        Ok(row.map(|tuple| tuple.0))
    }
}
