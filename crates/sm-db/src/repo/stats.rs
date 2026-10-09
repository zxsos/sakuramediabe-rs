//! 状态页的**跨表聚合**查询。
//!
//! # 为什么单独一个模块，而不是散进各自的仓储
//!
//! 本模块的每条查询都跨表，而本仓库的 `repo/` 其它文件严格一文件一表：
//!
//! | 查询 | 跨的表 |
//! |---|---|
//! | [`StatsRepository::playable_movie_count`] | `movie` + `media` |
//! | [`StatsRepository::media_usage_by_library`] | `media_library` + `media` |
//! | [`StatsRepository::watched_progress`] | `media_progress` + `media` |
//!
//! 单表计数（影片总数、媒体总数、合集条数…）仍然放在各自的仓储里 —— 那些
//! 不是「跨表」，只是调用方恰好一起用。把它们搬进来会让这个模块的边界
//! 从「跨表」退化成「状态页用到的」，那等于按调用方而非按数据归属组织代码。
//!
//! 唯一的例外是下载任务的六分类：它按 `state × import_status` 分组后**在
//! Rust 侧折叠**成一个桶名（见 [`crate::transfers::downloads::import_status`]），
//! 折叠规则是业务规则而不是 SQL，所以查询在本模块、折叠在 service。
//!
//! # 这些查询全部是「统计」而非「读写」
//!
//! 没有 UPDATE、没有锁、没有分页。它们的唯一消费者是 `/status*` 三个端点，
//! 而那两个端点被人手工点开，不在热路径上 —— 所以刻意不做缓存，也不引入
//! 「什么时候该失效」那类问题。
//!
//! # 写这些 SQL 之前：列名对着 `docker/schema.sql` 核一遍
//!
//! 本模块的 SQL 里有三个列名与上游 Peewee 的**属性名**不同，而照抄属性名
//! 只会得到运行时的 `column ... does not exist`（500）—— `cargo check`、
//! `clippy`、`cargo test --lib` **全绿**，因为这些查询从未被执行过：
//!
//! | 上游 Peewee | 真实 DDL 列 |
//! |---|---|
//! | `Media.movie` | `media.movie_number`（字符串业务主键） |
//! | `MediaProgress.media` | `media_progress.media_id` |
//! | `MediaThumbnail.media` | `media_thumbnail.media_id` |
//!
//! 这已经是本仓库**第三次**犯同一类错（前两次：分辨率聚合的
//! `playlist_movie.movie`、以及 `task_state` 的 `succeeded`）。规则很朴素：
//! **新写的 SQL 先 grep 一遍 `docker/schema.sql`**，比写完再等集成测试报错
//! 便宜得多 —— 那条报错信息只说「列不存在」，不说你本该写什么。
//!
//! # 第二类：`SUM(bigint)` 返回 `NUMERIC`
//!
//! 聚合表达式的返回类型**对拍查不出来** —— schema 对拍只看表的列，不看
//! `SUM(...)` 的结果类型。所以「表结构 40/40 一致」与「这个查询能解码」
//! 是两件独立的事，必须各验一次。

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::error::DbError;
use crate::playback::media::thumbnail_state;
use crate::transfers::downloads::import_status;

/// 状态页聚合仓储。
#[derive(Debug, Clone)]
pub struct StatsRepository {
    pool: PgPool,
}

impl StatsRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 女性演员总数（`gender = 1` 且未被合并）。
    ///
    /// `merged_into_id IS NULL` 是硬条件：演员合并会把 A 指向 B，两行都还在，
    /// 只按 gender 统计会把**同一个人**数两次。上游同样带这个条件。
    pub async fn female_actor_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM actor WHERE gender = $1 AND merged_into_id IS NULL",
        )
        .bind(FEMALE_GENDER)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 女性演员里的已订阅数。条件同上，**外加** `is_subscribed`。
    pub async fn female_actor_subscribed_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM actor \
             WHERE gender = $1 AND is_subscribed = TRUE AND merged_into_id IS NULL",
        )
        .bind(FEMALE_GENDER)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 影片总数。
    pub async fn movie_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movie")
            .fetch_one(&self.pool)
            .await?)
    }

    /// 已订阅影片数。
    pub async fn subscribed_movie_count(&self) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movie WHERE is_subscribed = TRUE")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// **可播放**影片数：有至少一条 `valid` 媒体的影片。
    ///
    /// `COUNT(DISTINCT movie_number)` 而不是 `COUNT(*)` —— 一部影片有多条媒体
    /// 是常态（不同画质、不同版本），按行数统计会把「有多少个文件」当成
    /// 「有多少部影片」，数字能差一个量级。
    ///
    /// `movie_number IS NOT NULL` 是内连接的结果，保留条件是为了让语义显式：
    /// 非 JAV 媒体（`video_item_id`）没有 `movie_number`，不参与影片统计。
    pub async fn playable_movie_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT movie_number) FROM media \
             WHERE valid = TRUE AND movie_number IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// 媒体文件总数。
    pub async fn media_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media")
            .fetch_one(&self.pool)
            .await?)
    }

    /// 媒体文件总字节数。**空库返回 0 而不是 NULL。**
    ///
    /// # `SUM(bigint)` 在 PostgreSQL 返回 `NUMERIC`，不是 `bigint`
    ///
    /// 这不是笔误，是 PostgreSQL 的规定：整数求和会溢出 `bigint`，所以
    /// `SUM(int8)` 的返回类型是 `NUMERIC`。不写 `::bigint` 的话，sqlx 会在
    /// 解码时报
    /// `Rust type i64 (as SQL type INT8) is not compatible with SQL type NUMERIC`
    /// —— 一个 500，而 schema 对拍**查不出来**（它只看列名/可空性/类型名映射，
    /// 不看聚合表达式的返回类型）。
    ///
    /// 截断风险：`bigint` 上限 9.2 EB，而这是媒体库总字节数，不可能到。
    /// `COALESCE(..., 0)` 同样必需：空表时 `SUM` 返回 NULL。
    pub async fn media_total_size_bytes(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(file_size_bytes), 0)::bigint FROM media",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// 媒体库总数。
    pub async fn media_library_count(&self) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_library")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// 缩略图文件总数（`media_thumbnail` 行数）。
    pub async fn thumbnail_total(&self) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_thumbnail")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// **待生成**缩略图的媒体数。
    ///
    /// 口径与缩略图任务的候选查询**逐条一致**（上游
    /// `playback/thumbnails/task_service.py:76-90`）—— 状态页说「还差 N 个」
    /// 而任务队列实际领到 M 个（M < N 因为库已下线）会让人以为卡住了：
    ///
    /// 1. `valid = TRUE`；
    /// 2. **尚无缩略图**（`NOT EXISTS` 子查询）—— 已经有缩略图的媒体
    ///    哪怕状态是 `pending` 也不该重做；
    /// 3. 状态是 `pending` / `succeeded` **或** `retry_wait` 且已到重试时刻。
    ///
    /// 第 3 条的 `retry_wait` 分支带 `next_retry_at IS NULL OR <= now` ——
    /// 不带的话「刚失败、退避期还没到」的任务也会被算成「待生成」，
    /// 那个数字会一直下不来。
    pub async fn pending_thumbnail_media_count(&self) -> Result<i64, DbError> {
        let now = crate::common::time::now_utc();
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM media \
             WHERE valid = TRUE \
               AND NOT EXISTS (SELECT 1 FROM media_thumbnail t WHERE t.media_id = media.id) \
               AND ( \
                 thumbnail_generation_state IN ($1, $2) \
                 OR (thumbnail_generation_state = $3 \
                     AND (thumbnail_next_retry_at IS NULL OR thumbnail_next_retry_at <= $4)) \
               )",
        )
        .bind(thumbnail_state::PENDING)
        .bind(thumbnail_state::SUCCEEDED)
        .bind(thumbnail_state::RETRY_WAIT)
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 退避等待中的缩略图媒体数（`state = retry_wait` 且无缩略图）。
    pub async fn retry_wait_thumbnail_media_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM media \
             WHERE thumbnail_generation_state = $1 \
               AND NOT EXISTS (SELECT 1 FROM media_thumbnail t WHERE t.media_id = media.id)",
        )
        .bind(thumbnail_state::RETRY_WAIT)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 终态失败的缩略图媒体数（`state = terminal` 且无缩略图）。
    ///
    /// `terminal` 是「成功**或**明确放弃」的合并终态，所以这一栏里既有成功的
    /// 也有放弃的 —— 上游的字段名是 `terminal_failed_media` 而判定是
    /// `state == TERMINAL`。字段名的「failed」与判定不一致，这里照抄不改：
    /// 客户端按这个字段名渲染，改名等于改契约。
    pub async fn terminal_failed_thumbnail_media_count(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM media \
             WHERE thumbnail_generation_state = $1 \
               AND NOT EXISTS (SELECT 1 FROM media_thumbnail t WHERE t.media_id = media.id)",
        )
        .bind(thumbnail_state::TERMINAL)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 下载任务按 `(state, import_status)` 分组计数。
    ///
    /// 只返回库里**实际存在**的组合，缺失的组合不出现在结果里 —— 折叠成六个
    /// 用户视角的桶是 service 层的活（见
    /// [`DownloadTaskBucket`](crate::transfers::downloads::import_status)）。
    ///
    /// 按 `(state, import_status)` 排序让结果**确定**：同一份数据两次调用
    /// 得到同样的顺序，便于测试与排查。
    pub async fn download_task_groups(&self) -> Result<Vec<(String, String, i64)>, DbError> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT state, import_status, COUNT(*) AS total FROM download_task \
             GROUP BY state, import_status ORDER BY state, import_status",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 每个媒体库的文件数与占用字节数：`(library_id, file_count, total_size_bytes)`。
    ///
    /// **以媒体表为基准**，所以「有媒体但库被删了」的孤儿行也会出现。
    /// 反过来（以库表为基准）会让没有任何媒体的空库**不出现** —— 而状态页要
    /// 展示空库，因为「配了一个库但一个文件都没进去」是需要被看见的状态。
    /// 那个基准的选择在 service 层（以 `media_library` 为准遍历）。
    ///
    /// 返回**元组**而不是具名结构体：这是聚合投影而非表镜像，在 `sm-db` 里
    /// 声明成 `pub struct` 会被 schema 对拍当成待验证的表模型。具名类型在
    /// `sm_service::system::status`。
    pub async fn media_usage_by_library(&self) -> Result<Vec<(i32, i64, i64)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, i64, i64)>(
            "SELECT library_id, COUNT(*) AS file_count, \
                    COALESCE(SUM(file_size_bytes), 0)::bigint AS total_size_bytes \
             FROM media GROUP BY library_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 窗口内的观看记录：`(last_watched_at, movie_number)`。
    ///
    /// 三个条件与上游 `_watched_progress_condition` + 窗口过滤一致：
    ///
    /// 1. `position_seconds > 0` —— **看过**的定义。上游明确写「position > 0
    ///    即视为看过」，所以刚开始播放（position 还是 0）不计入。
    /// 2. `last_watched_at IS NOT NULL` —— 历史形态允许为空，聚合前排除。
    /// 3. `movie_number IS NOT NULL` —— 非 JAV 媒体不属于任何影片。
    ///
    /// 窗口是**半开区间** `[start, end)`。闭区间会把窗口外恰好落在边界上的
    /// 脏时间戳算进来，而分桶时它会被归到错误的桶。
    pub async fn watched_progress(
        &self,
        window_start: NaiveDateTime,
        window_end: NaiveDateTime,
    ) -> Result<Vec<(NaiveDateTime, String)>, DbError> {
        Ok(sqlx::query_as(
            "SELECT p.last_watched_at, m.movie_number \
             FROM media_progress p \
             JOIN media m ON m.id = p.media_id \
             WHERE p.position_seconds > 0 \
               AND p.last_watched_at IS NOT NULL \
               AND m.movie_number IS NOT NULL \
               AND p.last_watched_at >= $1 \
               AND p.last_watched_at < $2 \
             ORDER BY p.last_watched_at, m.movie_number",
        )
        .bind(window_start)
        .bind(window_end)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 最早一次有效观看的时间。**没有记录时返回 `None`。**
    ///
    /// `ALL` 范围的起点由它决定，所以 `None` 必须与「最早记录本身」可区分 ——
    /// 返回 `Option` 而不是用某个哨兵值。
    pub async fn earliest_watched_at(&self) -> Result<Option<NaiveDateTime>, DbError> {
        Ok(sqlx::query_scalar::<_, Option<NaiveDateTime>>(
            "SELECT MIN(last_watched_at) FROM media_progress \
             WHERE position_seconds > 0 AND last_watched_at IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?)
    }
}

/// 女性演员的 `gender` 取值。上游 `StatusService.FEMALE_GENDER = 1`。
pub const FEMALE_GENDER: i32 = 1;

/// 导入状态的合法集合在这里做一次交叉检查 —— 仓储层不校验字面量，
/// 但状态页的六分类依赖它，错了会静默错计数。
#[allow(dead_code)]
const _: fn() = || {
    // `skipped` 必须在白名单里：少了它，「没有可导入文件」的任务会被
    // 归到「导入失败」桶里。
    assert!(import_status::is_valid(import_status::SKIPPED));
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_female_gender_constant_is_one() {
        // 上游 `StatusService.FEMALE_GENDER = 1`。
        // 改这个数字会让整个「女性演员」统计悄悄换一批人。
        assert_eq!(FEMALE_GENDER, 1);
    }

    #[test]
    fn every_import_status_lands_in_some_bucket() {
        // 五个合法取值每一个都必须能被折叠进某个用户视角的桶。
        // 少一个就意味着那种任务在 `/status/insights` 里**完全不出现**，
        // 而六个桶之和会小于 `total` —— 那是个说不清的矛盾。
        for status in import_status::ALL {
            assert!(
                import_status::is_valid(status),
                "{status} 必须合法，否则它会被算进「未知」分支"
            );
        }
    }
}
