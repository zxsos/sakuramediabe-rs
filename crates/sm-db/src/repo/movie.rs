//! `movie` 表仓储。
//!
//! # 为什么不用 `query!` 宏
//!
//! `sqlx` 开了 `macros` feature，但 `query!` 需要**编译期**数据库连接。
//! 本机有 PG、CI 没有，用宏会让 CI 直接编译失败。所以全部走
//! `sqlx::query_as()` + `#[derive(sqlx::FromRow)]`（运行时解析）。
//!
//! 代价是失去编译期列名校验，补偿手段是两道验证：
//!
//! - L1 `parity/compare_schema.py` 保证 Rust 结构体与 Peewee 模型一致
//! - L2 集成测试在真实 PG 上跑，任何不匹配会在运行时报出来

use std::collections::HashMap;

use sqlx::{PgPool, Postgres, QueryBuilder};

use super::collection::SortDirection;
use super::ctx::Ctx;
use crate::catalog::movie::{field_owner, Movie, MovieSeries, PROTECTED_MOVIE_FIELDS};
use crate::common::guard::{FieldGuard, WriteSource};
use crate::common::page::{Page, PageRequest};
use crate::common::update::UpdateSet;
use crate::error::DbError;
use crate::paged_list;

/// 实体名，用于错误分类。
const ENTITY: &str = "Movie";

/// 字段护栏：受保护字段白名单 + 宿主独占的状态列。
fn guard() -> FieldGuard {
    FieldGuard::new(
        ENTITY,
        &PROTECTED_MOVIE_FIELDS,
        &["field_owners", "mutation_revision"],
    )
}

/// 媒体分辨率 → 档位序号的 `CASE` 表达式。
///
/// 对应上游 `src/service/catalog/movie_resolution_service.py` 的
/// `resolution_level_expression()`。**列名硬编码为 `md.resolution`** ——
/// 每条用到它的查询都必须把 `media` 别名成 `md`。这是刻意的：表达式要能被
/// 拼进别的 SQL 字面量，所以它不能是带占位符的格式串。
///
/// # 为什么压成整数而不是直接比字符串
///
/// 档位筛选的语义是「影片的**最高**媒体落在 `[threshold, upper)` 区间」。
/// 要对一组媒体取最高再比阈值，必须有一个全序的比较键 —— 字符串
/// `"1080P"` / `"4K"` 做不到（字典序下 `"720P" > "1080P"`）。序号是
/// 唯一能让 `MAX()` 之后直接比较的形态。
///
/// # 分支顺序是契约本身，不能重排
///
/// `CASE` 从上往下第一个命中就返回，所以：
///
/// 1. `width <= 0 OR height <= 0 → 0` 必须在**所有**比较之前。否则
///    `0x0`（合法匹配那个正则！）会掉到 `ELSE 0`，而 `0x9999` 会命中
///    档位 5 —— 一个宽 0 的影片被判成 2K。
/// 2. 宽度分支（7680 / 3840）必须在高度分支之前。`7680x4320` 的高度
///    4320 ≥ 1440，若先判高度会落进档位 5（2K）而不是 7（8K）。
///
/// # `split_part` 按**第一个** `x` 切
///
/// `1920x1080` → `("1920", "1080")`。第二个参数写 2 时 PG 返回剩余部分，
/// 所以值里若还有 `x`（`1920x1080x60`）会被整体 cast 失败 —— 正则
/// `^\d+x\d+$` 已经把它排除了。
///
/// # 阈值与上游逐条一致
///
/// `8K=7680宽` / `4K=3840宽` / `2K=1440高` / `1080P=1080高` /
/// `720P=720高` / `480P=480高` / `360P=360高`。改任何一个数字都会让
/// 已入库媒体的档位归属变化，进而改变筛选结果。
///
/// # `pub(crate)` 而不是私有
///
/// `repo::collection` 的影片卡片查询要用同一份表达式做
/// 「最高档位落在 `[threshold, upper)`」的 `EXISTS` 筛选。**复制一份**会让
/// 两处的阈值各自漂移 —— 那是「筛 4K 筛出来的影片和档位计数对不上」这类
/// 只能靠用户发现的缺陷。所以只此一份，两处共用。
///
/// 共用的前提是**别名必须是 `md`**（见上），调用方的 SQL 也要自带这个别名。
pub(crate) const RESOLUTION_LEVEL_CASE: &str = "CASE \
     WHEN split_part(md.resolution, 'x', 1)::int <= 0 \
       OR split_part(md.resolution, 'x', 2)::int <= 0 THEN 0 \
     WHEN split_part(md.resolution, 'x', 1)::int >= 7680 THEN 7 \
     WHEN split_part(md.resolution, 'x', 1)::int >= 3840 THEN 6 \
     WHEN split_part(md.resolution, 'x', 2)::int >= 1440 THEN 5 \
     WHEN split_part(md.resolution, 'x', 2)::int >= 1080 THEN 4 \
     WHEN split_part(md.resolution, 'x', 2)::int >= 720 THEN 3 \
     WHEN split_part(md.resolution, 'x', 2)::int >= 480 THEN 2 \
     WHEN split_part(md.resolution, 'x', 2)::int >= 360 THEN 1 \
     ELSE 0 END";

/// 一部影片的聚合结果：**`(movie_id, max_level)`**。
///
/// # 为什么是元组而不是 `pub struct`
///
/// 这个行**不是任何表的镜像** —— 它是一次 `GROUP BY m.id` 聚合的投影。
/// 而 `pub struct` + `#[derive(FromRow)]` 在 `sm-db` 里的含义是
/// 「我映射一张表」，`parity/compare_schema.py` 也正是这么理解的：它把
/// 每一个这样的结构体都当成待验证的表模型，于是要求存在对应的 Peewee
/// 模型，找不到就报 `UNCHECKED_STRUCT` 并让门禁变红。
///
/// 投影行没有上游模型（上游用 Peewee 表达式树，那个「行」只是元组），
/// 所以两条路：
///
/// 1. 给对拍脚本加豁免 + 放宽它的判定条件 —— 那是**削弱门禁**，
///    而这道门禁存在的意义就是抓住「新增结构体却忘了对拍」。
/// 2. 让 `sm-db` 返回元组，把具名类型放到 `sm-service`（它不在对拍
///    扫描范围内，且那里才是消费方）。
///
/// 选 2。代价是这一层的可读性靠文档，收益是门禁一点没松。
/// `sm-service::catalog::resolution::MovieResolutionLevel` 是对应的具名类型。
///
/// # `max_level` 恒非空
///
/// `CASE` 的 `ELSE 0` 覆盖全部剩余情况，而内连接 + `WHERE` 保证每个分组
/// 至少一行，所以 `MAX(...)` 恒非空。解码成 `i32` 而不是 `Option<i32>`：
/// 若哪天这个前提破了，解码会**报 ColumnDecode**（可定位），而不是静默
/// 变成「无法解析」而少算一档。
pub type MovieResolutionLevelRow = (i32, i32);

/// 每日推荐的**全库候选**投影行。
///
/// 位置依次是 **`(id, heat, release_date, created_at, is_subscribed)`**。
/// 消费方用 `let (id, heat, ..) = row;` 解构，别用 `.0` / `.1` ——
/// 五个位置里有两个同类型的时间戳，写错位是**静默**的（编译通过、语义反了）。
///
/// # 为什么是元组而不是 `pub struct`
///
/// 它是 `movie` 表的**列子集**（5 列），不是表镜像：上游没有这样一张 Peewee
/// 模型可以逐列对照，写成 `pub struct` 会被门禁报 `UNCHECKED_STRUCT`。
/// 具名类型 `sm_service::discovery::daily_recommendation::CandidateMovie` 放在
/// 服务层，与 [`MovieResolutionLevelRow`] 同一选法（见
/// `parity/compare_schema.py` 的豁免说明：带 `FromRow` 的投影行不在豁免范围内）。
pub type CandidateMovieRow = (
    i32,
    i32,
    Option<chrono::NaiveDateTime>,
    Option<chrono::NaiveDateTime>,
    bool,
);

/// 影片列表的筛选条件（上游 `_filtered_movies` 的入参）。
///
/// 纯值对象：只作为参数传给 [`MovieRepository::list_movie_card_ids`]，没有
/// `FromRow`、不映射任何表 —— 与 `ClipFilter` 同一类（见对拍豁免名单）。
///
/// # 为什么用 `Option<bool>` / `Option<i32>` 而不是枚举
///
/// 上游的 `MovieListStatus` / `MovieNumberSource` 是 schema 层的枚举，服务层
/// 解析请求时已经校验过。这里用最朴素的三态表达，`sm-db` 就不必认识那些枚举
/// （也就少两个对拍豁免项）。
#[derive(Debug, Clone, Default)]
pub struct MovieListFilter {
    /// 演员 id。走 `COALESCE(merged_into_id, id)` —— **被合并的演员也要能筛出
    /// 它名下的影片**，否则用户点开一个已合并的演员会得到空列表。
    pub actor_id: Option<i32>,
    /// 标签 id。空 = 不筛。
    pub tag_ids: Vec<i32>,
    /// `true` = 必须同时含**全部**标签（AND）；`false` = 命中任一（OR）。
    pub tag_match_all: bool,
    /// 发行年份，按 `[year-01-01, year+1-01-01)` 半开区间。
    pub year: Option<i32>,
    /// `Some(true)` = 已订阅，`Some(false)` = 未订阅，`None` = 不限。
    pub subscribed: Option<bool>,
    /// 只看**能播**的（至少一条有效媒体）。
    ///
    /// 与 `subscribed` 同属上游的一个枚举，但语义正交，所以拆成两个字段。
    pub playable_only: bool,
    /// 只要**单片**。合集与单片互斥，所以没有对应的 `Some(false)`。
    pub single_only: bool,
    pub series_id: Option<i32>,
    /// **精确**匹配（上游 `parse_optional_exact_text` 已 strip）。
    pub director_name: Option<String>,
    pub maker_name: Option<String>,
    /// `Some(true)` = 仅 FC2，`Some(false)` = 排除 FC2。
    pub fc2: Option<bool>,
    pub heat_min: Option<i32>,
    pub heat_max: Option<i32>,
    /// 分辨率档位区间 `[threshold, upper)`，与影片卡片同一口径。
    pub resolution: Option<(i32, Option<i32>)>,
    /// 只要黑名单里的。默认 `false` = 只要**不在**黑名单的。
    pub blacklisted: bool,
    /// 检索词。空 = 不检索。
    pub search_terms: Vec<String>,
}

/// 影片列表的排序键。
///
/// 闭集枚举的理由与 [`super::collection::PlaylistMovieCardSort`] 相同：
/// `ORDER BY` 片段必须写死，否则 `added_at` 那段相关子查询就是注入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovieListSort {
    ReleaseDate,
    /// `status = playable` 时是「媒体入库时间」子查询，否则退化成 `movie.id`
    /// （上游 `extra_sort_builders` 只在 playable 时挂上去）。
    AddedAt,
    SubscribedAt,
    CommentCount,
    ScoreNumber,
    WantWatchCount,
    Heat,
}

/// 「最近一次媒体入库时间」相关子查询（`added_at` 在 playable 时的排序列）。
const MOVIE_LATEST_MEDIA_SUBQUERY: &str =
    "(SELECT MAX(md.created_at) FROM media md WHERE md.movie_number = m.movie_number)";

/// `^\d+[-_]\d+$`：纯数字番号。
///
/// 这类番号的分隔符是**片商标识**（一本道 `_` / 加勒比 `-`，同日番号是两部
/// 不同影片），所以检索时保留分隔符、**不折叠**。
fn is_pure_numeric_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == 0 || index >= bytes.len() || (bytes[index] != b'-' && bytes[index] != b'_') {
        return false;
    }
    index += 1;
    index < bytes.len() && bytes[index..].iter().all(u8::is_ascii_digit)
}

/// 番号检索键：去掉 `-`/`_`，并把 `FC2PPV` 折叠成 `FC2`。
///
/// 与查询里那段 `REPLACE(UPPER(TRANSLATE(movie_number, '-_', '')), 'FC2PPV', 'FC2')`
/// 逐条对应 —— 两处必须一起改，否则「存的值」与「比的值」落到不同形态，
/// 检索会安静地查不到（不报错，只是没有结果）。
fn number_search_key(normalized: &str) -> String {
    let key = normalized.replace(['-', '_'], "");
    match key.strip_prefix("FC2PPV") {
        Some(rest) => format!("FC2{rest}"),
        None => key,
    }
}

/// 插入一条影片。
///
/// 只暴露**有业务含义**的列；`heat` / `watched_count` / `comment_count`
/// 这些计数器由 service 层在业务流程里累加，不该在「创建影片」时被随手
/// 指定成任意值。其余列走数据库 DEFAULT。
#[derive(Debug, Clone, Default)]
pub struct NewMovie {
    /// 番号。**只去首尾空白、不做归一化改写** —— 分隔符与大小写都是有效信息。
    pub movie_number: String,
    pub title: String,
    /// 空串在写入前归一为 `None`（对应上游 `save()` 的 `or None`）。
    pub javdb_id: Option<String>,
    /// `summary text NOT NULL DEFAULT ''` —— **不是 `Option`**。
    ///
    /// 此前声明成 `Option<String>`，`None` 会绑成 NULL 并违反 NOT NULL。
    pub summary: String,
    pub maker_name: Option<String>,
    pub director_name: Option<String>,
    pub release_date: Option<chrono::NaiveDateTime>,
    /// `integer NOT NULL DEFAULT 0` —— **不是 `Option`**。
    pub duration_minutes: i32,
    /// `double precision NOT NULL DEFAULT 0` —— **不是 `Option`**。
    pub score: f64,
    /// `integer NOT NULL DEFAULT 0` —— **不是 `Option`**。
    pub score_number: i32,
    /// `series_id integer NULL` —— 宽度是 `i32`，与 [`Movie::series_id`] 一致。
    ///
    /// 此前这里是 `Option<i64>`，而同一张表的模型层是 `Option<i32>`。
    /// sqlx 把 i64 绑进 `integer` 列会失败，所以**每次**用它写外键都会报错。
    /// 这类漂移能活下来是因为 `NewMovie` 在对拍的豁免名单里（它是列的
    /// 子集，不是表镜像），而豁免顺带免掉了字段类型检查。
    pub series_id: Option<i32>,
    /// `integer NULL`，宽度同 [`Movie::cover_image_id`]。
    pub cover_image_id: Option<i32>,
    /// `integer NULL`，宽度同 [`Movie::thin_cover_image_id`]。
    pub thin_cover_image_id: Option<i32>,
    /// JSONB，默认 NULL。
    pub metadata_source: Option<serde_json::Value>,
}

/// `movie` 表仓储。
#[derive(Debug, Clone)]
pub struct MovieRepository {
    pool: PgPool,
}

impl MovieRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。事务场景需要它来保证同一连接。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<Movie>, DbError> {
        let row = sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 按主键查询，未命中返回 [`DbError::NotFound`]。
    pub async fn require_by_id(&self, id: i32) -> Result<Movie, DbError> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 按番号查询。番号是业务主键，外部调用方几乎总是用它。
    pub async fn find_by_number(&self, movie_number: &str) -> Result<Option<Movie>, DbError> {
        let row = sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE movie_number = $1")
            .bind(movie_number)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 远端 JavDB id 是否已被**别的**本地影片占用，占用则返回那条的番号。
    ///
    /// 上游 `_validate_remote_movie_metadata_javdb_id`（
    /// `movie_metadata_refresh_service.py:132-167`）的查询段：
    /// `(javdb_id == remote) & (Movie.id != movie.id)`。「刷新」必须先过这道闸
    /// —— 远端主键已被别的影片占用时直接拒绝，否则会把另一部影片的元数据
    /// 覆盖过来，而用户完全看不出来。
    pub async fn conflicting_number_by_javdb_id(
        &self,
        javdb_id: &str,
        exclude_movie_id: i32,
    ) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT movie_number FROM movie WHERE javdb_id = $1 AND id <> $2 LIMIT 1",
        )
        .bind(javdb_id)
        .bind(exclude_movie_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 按一批番号**批量**查询，返回 `番号 -> Movie`。
    ///
    /// 下载任务列表用它一次把当页涉及的影片卡片取回来（上游
    /// `_load_movies_for_tasks` 的 `Movie.movie_number.in_(numbers)`）；
    /// 逐行 [`Self::find_by_number`] 就是 N+1。
    ///
    /// 空入参直接返回空映射，不发查询。
    pub async fn find_by_numbers(
        &self,
        numbers: &[String],
    ) -> Result<HashMap<String, Movie>, DbError> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE movie_number = ANY($1)")
            .bind(numbers)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|movie| (movie.movie_number.clone(), movie))
            .collect())
    }

    /// 按主键**批量**取回，返回 `id → Movie`。**没命中的 id 不在结果里。**
    ///
    /// # 为什么列表页需要它
    ///
    /// 影片卡片的聚合查询里，影片本体与封面/系列是几条独立查询。先分页拿到
    /// `movie_id`，再**一次**取回这一页的影片，而不是每部影片查一次
    /// （那就是 N+1，而列表页一次渲染 20 部）。
    ///
    /// 「已订阅演员的最新影片」的一页 id。
    ///
    /// 对应上游 `_subscribed_actor_latest_movies_query`。三处口径：
    ///
    /// 1. **内连接** `movie_actor` + `actor`：只列至少关联一位**已订阅**演员的影片；
    /// 2. **排除合集番号**（`is_collection = FALSE`）—— 合集是合辑，不该出现在
    ///    这个流里；
    /// 3. 排序是 `release_date IS NULL, release_date DESC, id DESC`：用
    ///    `IS NULL` 把没有发行日期的垫到最后（等价于 `NULLS LAST`，但上游写的是
    ///    这个形式）。
    pub async fn list_subscribed_actor_movie_ids(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<i32>, DbError> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT m.id FROM movie m \
             JOIN movie_actor ma ON ma.movie_id = m.id \
             JOIN actor a ON a.id = ma.actor_id \
             WHERE a.is_subscribed = TRUE AND m.is_collection = FALSE \
             GROUP BY m.id \
             ORDER BY m.release_date IS NULL, m.release_date DESC, m.id DESC \
             LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 上面那条的总数（`COUNT(DISTINCT)`,与当页同一口径）。
    pub async fn count_subscribed_actor_movies(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT m.id) FROM movie m \
             JOIN movie_actor ma ON ma.movie_id = m.id \
             JOIN actor a ON a.id = ma.actor_id \
             WHERE a.is_subscribed = TRUE AND m.is_collection = FALSE",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// 「最近入库」影片的 id：**只含有本地媒体的影片**，按该影片**最近一次
    /// 媒体入库时间**倒序，其次 `id` 倒序。
    ///
    /// 对应上游 `_latest_movies_query`。两点与直觉不同：
    ///
    /// 1. 排序键是 **`MAX(media.created_at)`**，不是 `movie.created_at` ——
    ///    「最近入库」指的是本地文件到了，而不是影片记录被创建。
    /// 2. **内连接 media** —— 没有本地媒体的影片不出现在这个列表里（它不是一个
    ///    「最新影片」列表，而是「最新到货」列表）。
    pub async fn list_latest_with_media_ids(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<i32>, DbError> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT m.id FROM movie m JOIN media md ON md.movie_number = m.movie_number \
             WHERE m.is_blacklisted = FALSE \
             GROUP BY m.id ORDER BY MAX(md.created_at) DESC, m.id DESC \
             LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 上面那条查询的总数。
    ///
    /// # 上游的 `total` **不带黑名单过滤**
    ///
    /// 上游写的是 `Movie.select(Movie.id).join(Media).group_by(Movie.id).count()`，
    /// 而当页查询有 `is_blacklisted == False`。所以拉黑过一部有媒体的影片后，
    /// `total` 会比实际能翻到的条数多 —— 最后一页可能是空的。
    ///
    /// 这里**照抄**（与 `batch_set_subscription` 的 `updated_count` 同一处理）：
    /// 客户端在用这个数字渲染分页，改它就是静默的契约变更。
    pub async fn count_with_media(&self) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM (SELECT m.id FROM movie m \
             JOIN media md ON md.movie_number = m.movie_number \
             GROUP BY m.id) AS grouped",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// 互动数同步的**候选影片 id**，按 id 升序。
    ///
    /// 对应上游 `MovieInteractionSyncService._candidate_ids`
    /// （`catalog/movie_interaction_sync_service.py:39-63`）—— 那个四路 OR 的
    /// `due` 条件逐字搬到这里：
    ///
    /// | 支 | 条件 | 为什么 |
    /// |---|---|---|
    /// | 1 | `interaction_synced_at IS NULL` | 从没同步过 |
    /// | 2 | `is_subscribed AND subscribed_at > interaction_synced_at` | 刚订阅 —— 订阅是个强信号，值得立刻刷一次 |
    /// | 3 | 新片（发行 ≤ 60 天）且上次同步超过 **2 天** | 新片互动数变化快 |
    /// | 4 | 中段（60~180 天）且上次同步超过 **7 天** | 变化慢 |
    ///
    /// 再加上 `javdb_id IS NOT NULL`：没有 JavDB id 就没法查互动数，
    /// 放进候选只会让 `failed_movies` 白涨。
    ///
    /// # 三处时间参数由**调用方**给
    ///
    /// `recent_since` / `middle_since` / 两个「上次同步早于此时刻」的阈值
    /// 都与「现在」有关，而本层不取时间（测试要能固定时间）。
    /// 间隔常量（2 天 / 7 天 / 60 天 / 180 天）在 service 层，不在这里重复。
    ///
    /// # 单条 SQL，不是「拉全表再在 Rust 里筛」
    ///
    /// 30 万行的影片表全拉出来再筛会把内存和往返都拖垮，而**这条判据本来就
    /// 是 SQL 表达得清楚的**。第 4 支的 `release_date < recent_since` 不能省：
    /// 少了它，新片会被第 4 支**同时**命中，两支的间隔不同，语义就糊了。
    pub async fn list_interaction_sync_candidate_ids(
        &self,
        recent_since: chrono::NaiveDateTime,
        middle_since: chrono::NaiveDateTime,
        recent_cutoff: chrono::NaiveDateTime,
        middle_cutoff: chrono::NaiveDateTime,
    ) -> Result<Vec<i32>, DbError> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT id FROM movie \
              WHERE javdb_id IS NOT NULL \
                AND ( \
                     interaction_synced_at IS NULL \
                  OR (is_subscribed = TRUE AND subscribed_at IS NOT NULL \
                      AND subscribed_at > interaction_synced_at) \
                  OR (release_date >= $1 AND interaction_synced_at <= $2) \
                  OR (release_date >= $3 AND release_date < $1 AND interaction_synced_at <= $4) \
                ) \
              ORDER BY id",
        )
        .bind(recent_since)
        .bind(recent_cutoff)
        .bind(middle_since)
        .bind(middle_cutoff)
        .fetch_all(&self.pool)
        .await?)
    }

    /// **缺竖封面**的影片 `(id, movie_number)`，按 id 升序。
    ///
    /// 上游 `MovieThinCoverBackfillService.backfill_missing_thin_cover_images`
    /// （`movie_thin_cover_backfill_service.py:20`）：
    /// `Movie.select().where(Movie.thin_cover_image.is_null(True)).order_by(Movie.id)`。
    ///
    /// 带番号是为了**日志**：上游失败时打
    /// `movie_id={} movie_number={}`，只有 id 的话运维得再查一次库。
    pub async fn list_missing_thin_cover(&self) -> Result<Vec<(i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, String)>(
            "SELECT id, movie_number FROM movie \
              WHERE thin_cover_image_id IS NULL \
              ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 待接入 JavDB 的插件影片 `(id, movie_number)`，按 id 升序。
    ///
    /// 上游 `MovieJavdbBackfillService.pending()`
    /// （`movie_javdb_backfill_service.py:22-26`）：
    /// `javdb_id IS NULL AND metadata_source IS NOT NULL`。
    ///
    /// 第二条的语义是「**来源是插件**」—— 只有插件来源的影片才需要「过一阵子
    /// 再问 JavDB 收没收」。JavDB 自己来的影片本体就带着 `javdb_id`。
    pub async fn list_javdb_backfill_pending(
        &self,
        limit: i64,
    ) -> Result<Vec<(i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, String)>(
            "SELECT id, movie_number FROM movie \
              WHERE javdb_id IS NULL AND metadata_source IS NOT NULL \
              ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 本轮补录的候选 id：`pending()` 之外**再加**「检查时间已到」，按
    /// `javdb_next_check_at, id` 升序，取 `limit` 条。
    ///
    /// 上游 `run()` 是在 `pending()` 上再叠三个条件
    /// （`:29-35`）。**顺序照抄**：先按下次检查时间、再按 id ——
    /// 只按 id 会让同一批里「最久没查过的」排到后面。
    ///
    /// # `javdb_next_check_at IS NULL` 的影片**不会被选中**
    ///
    /// 上游是 `javdb_next_check_at <= now`，而 `NULL <= x` 在 SQL 里是
    /// **unknown** —— 那些行被排除。这是上游行为，不是 bug 漏写：
    /// 插件来源的影片在**入库时**就被写成 `now + 7 天`
    /// （`catalog_import_service.py:328`），所以不需要靠 NULL 兜底。
    /// 这里**不**加 `IS NULL` 分支 —— 加了就会把「还没排到时间的」一起拉进来，
    /// 一天 50 条的限量会被它们占满。
    pub async fn list_javdb_backfill_candidate_ids(
        &self,
        now: chrono::NaiveDateTime,
        limit: i64,
    ) -> Result<Vec<i32>, DbError> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT id FROM movie \
              WHERE javdb_id IS NULL AND metadata_source IS NOT NULL \
                AND javdb_next_check_at <= $1 \
              ORDER BY javdb_next_check_at, id \
              LIMIT $2",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按 id 取一条**仍符合** `pending()` 条件的影片。
    ///
    /// 上游 `run()` 里 `self.pending().where(Movie.id == movie_id).get_or_none()`
    /// （`movie_javdb_backfill_service.py:62`）：本轮 id 是**先批量取**的，
    /// 取 id 与处理之间这部片可能已被别的路径接入 JavDB —— 那时它不再是候选，
    /// 上游 `continue`（不计数、也不推后检查时间）。
    pub async fn find_javdb_backfill_pending(
        &self,
        movie_id: i32,
    ) -> Result<Option<(i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, String)>(
            "SELECT id, movie_number FROM movie \
              WHERE id = $1 AND javdb_id IS NULL AND metadata_source IS NOT NULL",
        )
        .bind(movie_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 把下次检查时间推后到 `at`。返回受影响行数。
    ///
    /// 上游 `run()` 的 `finally`（`:89-93`）：**无论成功失败都要推后** ——
    /// 失败的那条不该在下一轮立刻再占一个名额。
    ///
    /// `AND javdb_id IS NULL` 那条**不能省**：并发下这条影片可能刚被别的路径
    /// 接入 JavDB，此时再写检查时间等于把它「退回到待补录」。
    pub async fn postpone_javdb_check(
        &self,
        movie_id: i32,
        at: chrono::NaiveDateTime,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE movie SET javdb_next_check_at = $2 \
              WHERE id = $1 AND javdb_id IS NULL",
        )
        .bind(movie_id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// **有图片资产**（封面 / 薄封面 / 剧照）的影片番号，去重后按番号升序。
    ///
    /// 对应上游 `MovieAssetPackBackfillService._candidate_movie_numbers`
    /// （`movie_asset_pack_backfill_service.py:30-53`）：它查三次
    /// （`Movie.cover_image` 非空、`Movie.thin_cover_image` 非空、
    /// `MoviePlotImage` 里出现过的番号）再取并集 `sorted(...)`。
    /// 这里用一条 `UNION` 拿同一个集合 —— 三次往返换成一次，集合与顺序不变
    /// （`UNION` 自带去重，正对上 Python 的 set 并集）。
    ///
    /// # 为什么是「有图」而不是「有包」
    ///
    /// 「这一部要不要真的重建」由**磁盘状态**决定（包在不在、有没有残留散文件），
    /// 库看不出包的存在。所以这一步只回答「哪些影片有图可打」。
    ///
    /// `movie_plot_image.movie_id` 指向 `movie.id`（不是番号），所以剧照那一支
    /// 要连一次 `movie`。
    pub async fn list_numbers_with_asset_images(&self) -> Result<Vec<String>, DbError> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT movie_number FROM movie WHERE cover_image_id IS NOT NULL \
             UNION \
             SELECT movie_number FROM movie WHERE thin_cover_image_id IS NOT NULL \
             UNION \
             SELECT m.movie_number FROM movie_plot_image p \
                 JOIN movie m ON m.id = p.movie_id \
             ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 影片列表的一页 id（按 `filter` 筛、按 `sort` 排）。
    ///
    /// 用 [`QueryBuilder`] 而不是拼字符串：占位符编号由它维护，15 个可选筛选位
    /// 各带 0..n 个绑定值，手写编号迟早错位（而错位的失败方式是**静默**的：
    /// 两个相邻绑定值类型相同时 SQL 照样执行，只是筛出了别的东西）。
    ///
    /// `sort` 为 `None` 且 `filter.search_terms` 非空时按**相关度**排，见
    /// `push_movie_list_order`。
    pub async fn list_movie_card_ids(
        &self,
        filter: &MovieListFilter,
        sort: Option<(MovieListSort, SortDirection)>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<i32>, DbError> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT m.id FROM movie m WHERE ");
        Self::push_movie_list_filter(&mut builder, filter);
        builder.push(" ORDER BY ");
        Self::push_movie_list_order(&mut builder, filter, sort);
        builder
            .push(" LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        Ok(builder
            .build_query_scalar::<i32>()
            .fetch_all(&self.pool)
            .await?)
    }

    /// 同一筛选口径下的**总数**。
    ///
    /// 与当页用同一套条件（`count` 不带排序与分页）—— 口径不一致会让
    /// 「共 20 条」配上 5 条结果，客户端一直翻页。
    pub async fn count_movies(&self, filter: &MovieListFilter) -> Result<i64, DbError> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM movie m WHERE ");
        Self::push_movie_list_filter(&mut builder, filter);
        Ok(builder
            .build_query_scalar::<i64>()
            .fetch_one(&self.pool)
            .await?)
    }

    /// 把 15 个筛选位推到 builder 上（**从第一个条件开始**，不含 `WHERE`）。
    ///
    /// 每个 `AND` 都必须带上前导空格 —— `QueryBuilder` 不做拼接，少一个空格就是
    /// 语法错误，而那种错误只在运行时出现。
    fn push_movie_list_filter(builder: &mut QueryBuilder<Postgres>, filter: &MovieListFilter) {
        builder
            .push("m.is_blacklisted = ")
            .push_bind(filter.blacklisted);

        if let Some(actor_id) = filter.actor_id {
            // 合并链：`COALESCE(merged_into_id, id)` —— 点开一个**已合并**的
            // 演员也要能筛出影片。
            builder
                .push(
                    " AND m.id IN (SELECT ma.movie_id FROM movie_actor ma WHERE ma.actor_id IN \
                     (SELECT COALESCE(a.merged_into_id, a.id) FROM actor a WHERE a.id = ",
                )
                .push_bind(actor_id)
                .push("))");
        }

        if !filter.tag_ids.is_empty() {
            builder
                .push(" AND m.id IN (SELECT mt.movie_id FROM movie_tag mt WHERE mt.tag_id = ANY(")
                .push_bind(filter.tag_ids.clone())
                .push(")");
            if filter.tag_match_all {
                // AND：按影片分组后，命中的**去重**标签数等于请求数。
                builder
                    .push(" GROUP BY mt.movie_id HAVING COUNT(DISTINCT mt.tag_id) = ")
                    .push_bind(filter.tag_ids.len() as i64);
            }
            builder.push(")");
        }

        if let Some(year) = filter.year {
            // 半开区间：`[year-01-01, year+1-01-01)`。用闭区间会把次年 1/1 零点
            // 那部算进今年。
            if let (Some(start), Some(end)) = (
                chrono::NaiveDate::from_ymd_opt(year, 1, 1).and_then(|d| d.and_hms_opt(0, 0, 0)),
                chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
                    .and_then(|d| d.and_hms_opt(0, 0, 0)),
            ) {
                builder
                    .push(" AND m.release_date >= ")
                    .push_bind(start)
                    .push(" AND m.release_date < ")
                    .push_bind(end);
            }
        }

        match filter.subscribed {
            Some(true) => {
                builder.push(" AND m.is_subscribed = TRUE");
            }
            Some(false) => {
                builder.push(" AND m.is_subscribed = FALSE");
            }
            None => {}
        }
        if filter.playable_only {
            // 「能播」= 至少一条**有效**媒体（与影片卡片的 can_play 同一口径）。
            builder.push(
                " AND EXISTS (SELECT 1 FROM media md WHERE md.movie_number = m.movie_number \
                 AND md.valid = TRUE)",
            );
        }
        if filter.single_only {
            builder.push(" AND m.is_collection = FALSE");
        }
        if let Some(series_id) = filter.series_id {
            builder.push(" AND m.series_id = ").push_bind(series_id);
        }
        if let Some(name) = &filter.director_name {
            builder
                .push(" AND m.director_name = ")
                .push_bind(name.clone());
        }
        if let Some(name) = &filter.maker_name {
            builder.push(" AND m.maker_name = ").push_bind(name.clone());
        }
        match filter.fc2 {
            Some(true) => {
                builder.push(" AND m.movie_number LIKE 'FC2%'");
            }
            // `movie_number` 是 NOT NULL，所以 `NOT LIKE` 不会因 NULL 静默漏行。
            Some(false) => {
                builder.push(" AND m.movie_number NOT LIKE 'FC2%'");
            }
            None => {}
        }
        if let Some(min) = filter.heat_min {
            builder.push(" AND m.heat >= ").push_bind(min);
        }
        if let Some(max) = filter.heat_max {
            builder.push(" AND m.heat <= ").push_bind(max);
        }

        if let Some((threshold, upper)) = filter.resolution {
            // 与影片卡片同一套 EXISTS，共用 `RESOLUTION_LEVEL_CASE`（别名必须是
            // `md`）。**不能复用 `collection` 里的片段**：那个把占位符编号写死成
            // `$2`/`$3`，而这里的编号由 QueryBuilder 决定。
            builder
                .push(
                    " AND EXISTS (SELECT 1 FROM media md WHERE md.movie_number = m.movie_number \
                     AND md.valid = TRUE AND md.resolution ~ '^\\d+x\\d+$' \
                     GROUP BY md.movie_number HAVING MAX(",
                )
                .push(RESOLUTION_LEVEL_CASE)
                .push(") >= ")
                .push_bind(threshold);
            if let Some(upper) = upper {
                builder
                    .push(" AND MAX(")
                    .push(RESOLUTION_LEVEL_CASE)
                    .push(") < ")
                    .push_bind(upper);
            }
            builder.push(")");
        }

        // 多词：词之间 **AND**。
        for term in &filter.search_terms {
            builder.push(" AND (");
            Self::push_search_term(builder, term);
            builder.push(")");
        }
    }

    /// 单个检索词在各字段上的 **OR** 条件：片名、番号、演员、标签。
    ///
    /// `LIKE` 的 `%` **不转义** —— 上游 `.contains(term)` 直接拼 `%term%`，
    /// 所以用户输入里的 `%` 在上游也是通配符。转义会改变既有检索行为。
    fn push_search_term(builder: &mut QueryBuilder<Postgres>, term: &str) {
        builder.push("m.title LIKE ").push_bind(format!("%{term}%"));

        let normalized = term.trim().to_uppercase();
        let has_key = normalized.chars().any(|c| c.is_ascii_alphanumeric());
        if has_key {
            if is_pure_numeric_number(&normalized) {
                // 纯数字番号**保留分隔符**做子串匹配（一本道 `_` 与加勒比 `-`
                // 是两部不同影片，折叠会让检索互相串台）。
                builder
                    .push(" OR m.movie_number LIKE ")
                    .push_bind(format!("%{normalized}%"));
            } else {
                // 其余番号去掉 `-`/`_` 后再比，`FC2PPV` 折叠成 `FC2`。
                builder
                    .push(
                        " OR REPLACE(UPPER(TRANSLATE(m.movie_number, '-_', '')), 'FC2PPV', 'FC2') \
                         LIKE ",
                    )
                    .push_bind(format!("%{}%", number_search_key(&normalized)));
            }
        }

        // 演员命中（**只认未合并的**，与上游 `_matching_actor_ids` 一致）。
        builder
            .push(
                " OR m.id IN (SELECT ma.movie_id FROM movie_actor ma WHERE ma.actor_id IN \
                 (SELECT a.id FROM actor a WHERE a.merged_into_id IS NULL AND \
                 (a.name LIKE ",
            )
            .push_bind(format!("%{term}%"))
            .push(" OR a.alias_name LIKE ")
            .push_bind(format!("%{term}%"))
            .push(")))");

        // 标签命中。
        builder
            .push(
                " OR m.id IN (SELECT mt.movie_id FROM movie_tag mt WHERE mt.tag_id IN \
                 (SELECT t.id FROM tag t WHERE t.name LIKE ",
            )
            .push_bind(format!("%{term}%"))
            .push("))");
    }

    /// 相关度分数：**越小越靠前**，每个词取最高档命中，多词求和。
    ///
    /// 分档与上游逐条一致：完整番号 0、番号前缀 1、片名精确 2、片名前缀 3、
    /// 片名包含 4、仅演员或标签命中 5。`CASE` 从上往下第一个命中即返回。
    fn push_search_score(builder: &mut QueryBuilder<Postgres>, terms: &[String]) {
        for (index, term) in terms.iter().enumerate() {
            if index > 0 {
                builder.push(" + ");
            }
            builder.push("CASE");
            let normalized = term.trim().to_uppercase();
            let has_key = normalized.chars().any(|c| c.is_ascii_alphanumeric());
            if has_key {
                if is_pure_numeric_number(&normalized) {
                    builder
                        .push(" WHEN m.movie_number = ")
                        .push_bind(normalized.clone())
                        .push(" THEN 0 WHEN LEFT(m.movie_number, ")
                        .push_bind(normalized.chars().count() as i32)
                        .push(") = ")
                        .push_bind(normalized.clone())
                        .push(" THEN 1");
                } else {
                    let key = number_search_key(&normalized);
                    let expression = "REPLACE(UPPER(TRANSLATE(m.movie_number, '-_', '')), \
                                      'FC2PPV', 'FC2')";
                    builder
                        .push(" WHEN ")
                        .push(expression)
                        .push(" = ")
                        .push_bind(key.clone())
                        .push(" THEN 0 WHEN LEFT(")
                        .push(expression)
                        .push(", ")
                        .push_bind(key.chars().count() as i32)
                        .push(") = ")
                        .push_bind(key)
                        .push(" THEN 1");
                }
            }
            builder
                .push(" WHEN UPPER(m.title) = ")
                .push_bind(normalized)
                .push(" THEN 2 WHEN m.title LIKE ")
                .push_bind(format!("{term}%"))
                .push(" THEN 3 WHEN m.title LIKE ")
                .push_bind(format!("%{term}%"))
                .push(" THEN 4 ELSE 5 END");
        }
    }

    /// `ORDER BY` 的片段构造。
    ///
    /// 有检索词且没显式给排序时按**相关度**（上游 `_build_movie_search_sort`），
    /// 否则按 `sort`，都没给就是上游的默认值 `movie_number ASC`。
    fn push_movie_list_order(
        builder: &mut QueryBuilder<Postgres>,
        filter: &MovieListFilter,
        sort: Option<(MovieListSort, SortDirection)>,
    ) {
        if !filter.search_terms.is_empty() && sort.is_none() {
            Self::push_search_score(builder, &filter.search_terms);
            builder.push(" ASC, m.release_date DESC NULLS LAST, m.id DESC NULLS LAST");
            return;
        }

        let Some((sort, direction)) = sort else {
            builder.push("m.movie_number ASC");
            return;
        };
        let direction = match direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        // 可空的那两列在两个方向上都要显式 `NULLS LAST`（上游
        // `MOVIE_LIST_NULLABLE_SORT_FIELDS`），次级排序同款 —— 否则 `DESC` 下
        // 没有发行日期/订阅时间的影片会全部浮到最前。
        let (column, nullable) = match sort {
            MovieListSort::ReleaseDate => ("m.release_date".to_owned(), true),
            MovieListSort::SubscribedAt => ("m.subscribed_at".to_owned(), true),
            MovieListSort::CommentCount => ("m.comment_count".to_owned(), false),
            MovieListSort::ScoreNumber => ("m.score_number".to_owned(), false),
            MovieListSort::WantWatchCount => ("m.want_watch_count".to_owned(), false),
            MovieListSort::Heat => ("m.heat".to_owned(), false),
            // `added_at` 只有 playable 时才是「媒体入库时间」；其余情况上游把它
            // 映射成 `Movie.id`。
            MovieListSort::AddedAt => {
                if filter.playable_only {
                    (MOVIE_LATEST_MEDIA_SUBQUERY.to_owned(), false)
                } else {
                    ("m.id".to_owned(), false)
                }
            }
        };
        let nulls = if nullable { " NULLS LAST" } else { "" };
        builder
            .push(column)
            .push(" ")
            .push(direction)
            .push(nulls)
            .push(", m.id ")
            .push(direction)
            .push(nulls);
    }

    /// 影片订阅状态的**唯一定义**：一个七路 `CASE`，求值为状态字符串。
    ///
    /// 筛选 / 计数 / 列表展示共用这一个片段 —— 上游这么组织就是为了避免「SQL 一套
    /// 判定、Python 再抄一套」的漂移（此前正是两份实现，靠注释约束一致）。
    ///
    /// # 分支顺序即优先级
    ///
    /// `downloading` 必须在 `import_failed` **之前**：前者是后者的真子集，顺序反了
    /// 会被吞掉。两者并集恒等于「有活跃任务」。
    ///
    /// # 三处容易写反
    ///
    /// - `imported` **不判 `valid`** —— 判死的媒体也算已入库（上游 docstring 写明）；
    /// - `failed` 要**排除** `no_candidate_found` —— 那是「没找到资源」，不算失败；
    /// - `CASE` 是短路求值的，每行最多跑到三个 `EXISTS`，所以不要改成七个独立的
    ///   `SUM(CASE)`（上游记过这笔账：那样每行会展开 11 个相关子查询）。
    ///
    /// 互斥性由顺序保证，于是「各状态计数之和恒等于订阅总数」自动成立。
    const SUBSCRIPTION_STATUS_CASE: &str = "CASE \
     WHEN EXISTS (SELECT 1 FROM media md WHERE md.movie_number = m.movie_number) \
       THEN 'imported' \
     WHEN EXISTS (SELECT 1 FROM download_task dt WHERE dt.movie_number = m.movie_number \
       AND dt.state IN ('queued', 'downloading', 'completed') \
       AND dt.import_status IN ('pending', 'running')) \
       THEN 'downloading' \
     WHEN EXISTS (SELECT 1 FROM download_task dt WHERE dt.movie_number = m.movie_number \
       AND dt.state IN ('queued', 'downloading', 'completed')) \
       THEN 'import_failed' \
     WHEN m.subscription_search_state = 'exhausted' THEN 'exhausted' \
     WHEN m.subscription_search_state = 'failed_retryable' \
       AND (m.subscription_search_error_code IS NULL \
            OR m.subscription_search_error_code <> 'no_candidate_found') THEN 'failed' \
     WHEN m.subscription_search_last_attempted_at IS NOT NULL THEN 'missing' \
     ELSE 'pending' END";

    /// 按订阅状态分组计数（**一次** `GROUP BY` 算齐七项）。
    ///
    /// 上游注释：每个 tab 打一次 `COUNT` 会重复扫表，所以合成一条。
    pub async fn count_subscription_statuses(&self) -> Result<Vec<(String, i64)>, DbError> {
        let sql = format!(
            "SELECT {} AS status, COUNT(*) FROM movie m \
             WHERE m.is_subscribed = TRUE GROUP BY 1",
            Self::SUBSCRIPTION_STATUS_CASE
        );
        Ok(sqlx::query_as::<_, (String, i64)>(safe_sql(sql))
            .fetch_all(&self.pool)
            .await?)
    }

    /// 订阅影片的一页：`(movie_id, status)`，顺序由 `order_by` 片段决定。
    ///
    /// `# 状态在 WHERE 里要重复渲染一遍` —— PostgreSQL 的 `WHERE` 不能引用
    /// `SELECT` 别名（上游注释记着这条）。
    pub async fn list_subscription_ids(
        &self,
        status: Option<&str>,
        search: Option<&str>,
        order_by: &'static str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<(i32, String)>, DbError> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT m.id, ");
        builder
            .push(Self::SUBSCRIPTION_STATUS_CASE)
            .push(" AS status");
        Self::push_subscription_filter(&mut builder, status, search);
        builder
            .push(" ORDER BY ")
            .push(order_by)
            .push(" LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        Ok(builder
            .build_query_as::<(i32, String)>()
            .fetch_all(&self.pool)
            .await?)
    }

    /// 同一筛选口径下的**总数**（`total` 与当页必须一致）。
    pub async fn count_subscriptions(
        &self,
        status: Option<&str>,
        search: Option<&str>,
    ) -> Result<i64, DbError> {
        let mut builder = QueryBuilder::<Postgres>::new("SELECT COUNT(*) ");
        Self::push_subscription_filter(&mut builder, status, search);
        Ok(builder
            .build_query_scalar::<i64>()
            .fetch_one(&self.pool)
            .await?)
    }

    /// 把「已订阅 + 状态 + 检索词」这段 `WHERE` 推到 builder 上。
    ///
    /// 列表与计数共用，免得两处口径漂移（上游的 `_base_query` + `list_subscriptions`
    /// 也是同一段）。
    fn push_subscription_filter(
        builder: &mut QueryBuilder<Postgres>,
        status: Option<&str>,
        search: Option<&str>,
    ) {
        builder.push(" FROM movie m WHERE m.is_subscribed = TRUE");
        if let Some(status) = status {
            builder
                .push(" AND ")
                .push(Self::SUBSCRIPTION_STATUS_CASE)
                .push(" = ")
                .push_bind(status.to_owned());
        }
        let keyword = search.map(str::trim).filter(|value| !value.is_empty());
        if let Some(keyword) = keyword {
            builder
                .push(" AND (m.movie_number LIKE ")
                .push_bind(format!("%{keyword}%"))
                .push(" OR m.title LIKE ")
                .push_bind(format!("%{keyword}%"))
                .push(")");
        }
    }

    /// 每部影片的媒体数，按番号**精确**匹配。
    ///
    /// 上游注释强调：状态判定里 `media_exists` 用的是精确相等，所以这里也必须
    /// 精确匹配 —— 用 `LIKE` 会让「列表显示的媒体数」与「判成已入库」不一致。
    pub async fn count_media_by_numbers(
        &self,
        numbers: &[String],
    ) -> Result<HashMap<String, i64>, DbError> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT movie_number, COUNT(*) FROM media WHERE movie_number = ANY($1) \
             GROUP BY movie_number",
        )
        .bind(numbers)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// 每部影片**判死**的下载任务数（`state = 'failed'`）。
    ///
    /// 列表里叫 `dead_download_task_count` —— 「试过几个种子都失败了」。
    pub async fn count_failed_tasks_by_numbers(
        &self,
        numbers: &[String],
    ) -> Result<HashMap<String, i64>, DbError> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT movie_number, COUNT(*) FROM download_task WHERE movie_number = ANY($1) \
             AND state = 'failed' GROUP BY movie_number",
        )
        .bind(numbers)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// 每部影片**最新活跃任务**的导入状态。
    ///
    /// 活跃 = `state IN ('queued','downloading','completed')`；最新 = `created_at`
    /// 倒序、再 `id` 倒序（与上游 `_latest_import_status` 的排序一致）。
    pub async fn latest_import_status_by_numbers(
        &self,
        numbers: &[String],
    ) -> Result<HashMap<String, String>, DbError> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT DISTINCT ON (dt.movie_number) dt.movie_number, dt.import_status \
             FROM download_task dt \
             WHERE dt.movie_number = ANY($1) \
               AND dt.state IN ('queued', 'downloading', 'completed') \
             ORDER BY dt.movie_number, dt.created_at DESC, dt.id DESC",
        )
        .bind(numbers)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// 重开订阅检索：把九列检索状态重置回 `pending`，返回影响行数。
    ///
    /// 对应上游 `MovieSubscriptionSearchStateService.reset`。三个口径：
    ///
    /// 1. **只动已订阅的影片**（`is_subscribed = TRUE`）；
    /// 2. 给了 `movie_ids` 就只重开这些（去重后 `= ANY`）；`None` 或**空数组**时
    ///    只重开 `exhausted`（已放弃）的 —— 上游 `if movie_ids:` 对空列表为假，
    ///    所以「传空数组」与「省略」**等价**，不是「什么都不做」；
    /// 3. **不推进 `updated_at`**：上游用 `Model.update()`，它绕过 Peewee 的
    ///    `save()` 覆写，时间戳不动。这里照抄 —— 订阅状态没变，只是检索预算被
    ///    重开。
    ///
    /// `retry_round` 是**加一**而不是清零：它记「这部影片被重开过几次」，
    /// 清零会让无限重开看起来像第一次尝试。
    pub async fn reset_subscription_search(
        &self,
        movie_ids: Option<&[i32]>,
    ) -> Result<u64, DbError> {
        /// 九列状态的重置。置空只能用 SQL 字面量 `NULL`，理由见
        /// [`MovieRepository::mark_subscribed`]。
        const RESET: &str = "UPDATE movie SET \
             subscription_search_state = 'pending', \
             subscription_search_attempt_count = 0, \
             subscription_search_retry_round = subscription_search_retry_round + 1, \
             subscription_search_last_attempted_at = NULL, \
             subscription_search_last_succeeded_at = NULL, \
             subscription_search_next_retry_at = NULL, \
             subscription_search_error_code = NULL, \
             subscription_search_last_error = NULL, \
             subscription_search_last_error_at = NULL \
             WHERE is_subscribed = TRUE";

        let mut unique: Vec<i32> = Vec::new();
        if let Some(ids) = movie_ids {
            for id in ids {
                if !unique.contains(id) {
                    unique.push(*id);
                }
            }
        }

        let sql = if unique.is_empty() {
            format!("{RESET} AND subscription_search_state = 'exhausted'")
        } else {
            format!("{RESET} AND id = ANY($1)")
        };
        let mut stmt = sqlx::query(safe_sql(sql));
        if !unique.is_empty() {
            stmt = stmt.bind(&unique);
        }
        Ok(stmt.execute(&self.pool).await?.rows_affected())
    }

    /// **领取一次订阅检索**：`pending`/`failed_retryable` → `running`。
    ///
    /// 对应上游 `MovieSubscriptionSearchStateService.begin_attempt`。返回
    /// `true` 表示这一行被改动（`false` = 影片不存在）。
    ///
    /// # ★ `WHERE` 里**没有** `state` 条件 —— 别顺手加上
    ///
    /// 上游是 `Movie.update(...).where(Movie.id == movie_id).execute()`，
    /// 只按 id 定位。看着像漏了「只有 pending 才能领」，但调用方的候选查询
    /// 已经筛过状态（见 `candidate_condition`），而那里放行的状态**不止
    /// `pending`** —— 还有 `failed_retryable`。
    ///
    /// 加上 `AND subscription_search_state = 'pending'` 会让
    /// `failed_retryable` 的影片**永远领不到**：那正是「重试」这个功能本身，
    /// 而失败的样子是「重试再也跑不动」，不是报错。
    ///
    /// `next_retry_at` 在这里被清掉（上游同款）：它只用于
    /// `failed_retryable` 的退避判定，进了 `running` 就没有意义了。
    pub async fn begin_subscription_search_attempt(&self, movie_id: i32) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        let rows = sqlx::query(
            "UPDATE movie SET subscription_search_state = 'running', \
             subscription_search_last_attempted_at = $2, \
             subscription_search_next_retry_at = NULL \
             WHERE id = $1",
        )
        .bind(movie_id)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows > 0)
    }

    /// 标记订阅检索**成功**（终态）：清掉错误与重试预算。
    ///
    /// 对应上游 `mark_succeeded`。`attempt_count` **清零**（不是保留）——
    /// 下次因别的原因重开时它得从零开始算。
    pub async fn mark_subscription_search_succeeded(&self, movie_id: i32) -> Result<u64, DbError> {
        let now = crate::common::time::now_utc();
        let rows = sqlx::query(
            "UPDATE movie SET subscription_search_state = 'succeeded', \
             subscription_search_attempt_count = 0, \
             subscription_search_next_retry_at = NULL, \
             subscription_search_error_code = NULL, \
             subscription_search_last_error = NULL, \
             subscription_search_last_error_at = NULL, \
             subscription_search_last_succeeded_at = $2 \
             WHERE id = $1",
        )
        .bind(movie_id)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// 标记订阅检索**失败**：落 `state` 与**已经算好的** `attempt_count`。
    ///
    /// 对应上游 `mark_failed`。状态与计数由调用方算（它才知道
    /// `consumes_budget` 与影片是否「新鲜」），仓储只负责写 —— 那两条规则
    /// 在 `sm_service::catalog::movie_subscription_search_state` 里。
    ///
    /// `last_error` 收的是错误对象的 `Display`（上游写入的是 `str(error)`，
    /// 也就是异常消息本身）。
    pub async fn mark_subscription_search_failed(
        &self,
        movie_id: i32,
        state: &str,
        attempt_count: i32,
        error_code: &str,
        last_error: &str,
    ) -> Result<u64, DbError> {
        let now = crate::common::time::now_utc();
        let rows = sqlx::query(
            "UPDATE movie SET subscription_search_state = $2, \
             subscription_search_attempt_count = $3, \
             subscription_search_next_retry_at = NULL, \
             subscription_search_error_code = $4, \
             subscription_search_last_error = $5, \
             subscription_search_last_error_at = $6 \
             WHERE id = $1",
        )
        .bind(movie_id)
        .bind(state)
        .bind(attempt_count)
        .bind(error_code)
        .bind(last_error)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// ★ 进程启动时的兜底：把卡在 `running` 的检索改回 `failed_retryable`。
    ///
    /// 对应上游 `recover_interrupted_running_movies`。**不是改回 `pending`**：
    /// `pending` 意味着「从没搜过」，而这些影片是「搜到一半被打断」——
    /// 两者的 `last_attempted_at` 与用户看到的状态不同（那是「失败，待重试」）。
    ///
    /// 错误码固定 `task_interrupted`（上游写死）；文案由调用方传（它要跟
    /// 展示层一致，见 `INTERRUPTED_ERROR_MESSAGE`）。
    ///
    /// 不做这一步的后果：那些影片永远停在「正在搜索」，而候选查询把
    /// `running` 排除在外 —— 于是**再也不会被领取**，且没有任何报错。
    pub async fn recover_interrupted_subscription_searches(
        &self,
        error_message: &str,
    ) -> Result<u64, DbError> {
        let now = crate::common::time::now_utc();
        let rows = sqlx::query(
            "UPDATE movie SET subscription_search_state = 'failed_retryable', \
             subscription_search_next_retry_at = NULL, \
             subscription_search_error_code = 'task_interrupted', \
             subscription_search_last_error = $1, \
             subscription_search_last_error_at = $2 \
             WHERE subscription_search_state = 'running'",
        )
        .bind(error_message)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// 按「大写后的番号」点查。
    ///
    /// 人工输入（URL 路径、批量操作的请求体）不是库内规范形态，所以大小写要
    /// 靠 `UPPER(movie_number)` 抹平 —— 那正是函数索引 `movie_movie_number_upper`
    /// 的用途，用裸列比较会让这个索引失效。
    ///
    /// 分隔符**不在这里**互换：候选顺序由 `sm_service::movie_numbers` 的
    /// `movie_number_lookup_values` 决定，这里只管一次点查。
    pub async fn find_by_upper_number(&self, upper_number: &str) -> Result<Option<Movie>, DbError> {
        Ok(
            sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE UPPER(movie_number) = $1")
                .bind(upper_number)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按「大写后的番号」**批量**取回，`ORDER BY id`。
    ///
    /// 批量订阅/黑名单用它一次定位全部入参。排序是为了让「部分成功」的结果
    /// 稳定 —— 同一次请求两次调用的顺序不应不同。
    ///
    /// # 与 [`MovieRepository::find_by_ids`] 的空输入语义一致
    pub async fn list_by_upper_numbers(
        &self,
        upper_numbers: &[String],
    ) -> Result<Vec<Movie>, DbError> {
        if upper_numbers.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as::<_, Movie>(
            "SELECT * FROM movie WHERE UPPER(movie_number) = ANY($1) ORDER BY id",
        )
        .bind(upper_numbers)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 按 `id` 游标取一页，**升序**。语义：只取 `id > after_id` 的行。
    ///
    /// 插件能力出口的 `ListMovies` 用它（`crates/sm-server/src/plugin_host.rs`）。
    /// 用游标而不是 `OFFSET`：判定/回写类插件会**边扫边写**，而写会改
    /// `updated_at`（有的还会改排序键）—— `OFFSET` 在那种场景下会漏行或重扫。
    ///
    /// `limit` 是「最多几条」。调用方想判「还有下一页」，传 `limit + 1` 再看
    /// 回来的条数有没有超：比 `OFFSET` 稳，也不必额外跑一次 `COUNT`，更不会
    /// 因为恰好取满而多走一趟空查询。
    pub async fn list_page_after_id(
        &self,
        after_id: i32,
        limit: i64,
    ) -> Result<Vec<Movie>, DbError> {
        Ok(
            sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE id > $1 ORDER BY id LIMIT $2")
                .bind(after_id)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// 订阅：写订阅位，可选地同时把九列检索状态重置成「待抓取」。
    ///
    /// # 为什么手写 SQL 而不是 `UpdateSet`
    ///
    /// 重置要把**六列置 NULL**，而 `UpdateSet` 的 `ValueInner::Null` 在绑定层
    /// 是 `Option::<String>::None`（见本文件的绑定分支）—— 那是一个 **text
    /// 类型的 NULL**，对 `timestamp` 列直接报
    /// `column ... is of type timestamp without time zone but expression is of
    /// type text`。置空只能用 SQL 字面量 `NULL`，与 `ActorUpdate` 单独维护
    /// `nulls` 列表是同一个思路。
    ///
    /// 这几列都**不在** `PROTECTED_MOVIE_FIELDS` 里，所以绕开字段护栏不改变
    /// 任何可见行为。
    ///
    /// # `reset_search_state = false` 时**不碰** `subscribed_at`
    ///
    /// 对应上游 `if not was_subscribed or movie.subscribed_at is None:` ——
    /// 重复订阅一部已订阅的影片要保留原订阅时间，否则客户端的「最近订阅」
    /// 排序会被一次重复点击打乱。
    pub async fn mark_subscribed(&self, id: i32, reset_search_state: bool) -> Result<(), DbError> {
        let now = crate::common::time::now_utc();
        if reset_search_state {
            sqlx::query(
                "UPDATE movie SET is_subscribed = TRUE, subscribed_at = $2, \
                 subscription_search_state = 'pending', \
                 subscription_search_attempt_count = 0, \
                 subscription_search_retry_round = subscription_search_retry_round + 1, \
                 subscription_search_last_attempted_at = NULL, \
                 subscription_search_last_succeeded_at = NULL, \
                 subscription_search_next_retry_at = NULL, \
                 subscription_search_error_code = NULL, \
                 subscription_search_last_error = NULL, \
                 subscription_search_last_error_at = NULL, \
                 updated_at = $2 WHERE id = $1",
            )
            .bind(id)
            .bind(now)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query("UPDATE movie SET is_subscribed = TRUE, updated_at = $2 WHERE id = $1")
                .bind(id)
                .bind(now)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// 退订：清订阅位与订阅时间。
    ///
    /// `subscribed_at = NULL` 同样只能用字面量，理由见
    /// [`MovieRepository::mark_subscribed`]。
    pub async fn clear_subscription(&self, id: i32) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE movie SET is_subscribed = FALSE, subscribed_at = NULL, updated_at = $2 \
             WHERE id = $1",
        )
        .bind(id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// # 空输入直接返回空 map，不发查询
    ///
    /// `id = ANY('{}')` 返回零行而不是报错，两者结果相同 —— 提前返回省掉一次
    /// 往返，也让「筛选后这一页为空」这个常见情形不产生 DB 往返。
    pub async fn find_by_ids(&self, ids: &[i32]) -> Result<HashMap<i32, Movie>, DbError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }

    /// 播放列表内每部影片的**最高分辨率档位序号**。
    ///
    /// 对应上游 `PlaylistService.list_playlist_resolutions` 里那段
    /// `MAX(resolution_level_expression()) ... GROUP BY Movie.id`。
    /// 返回 `Vec<(movie_id, max_level)>` —— **每部影片一行**。
    ///
    /// # 档位序号：把 `WxH` 压成一个可比较的整数
    ///
    /// 核心是 **4K/8K 看宽度、其余看高度**（见文件里的
    /// `RESOLUTION_LEVEL_CASE`），分支顺序与上游 `Case` 逐条一致。
    ///
    /// # 只统计 `valid` 且形如 `WxH` 的媒体
    ///
    /// - `md.valid = TRUE`：判死的媒体不参与，与上游一致。
    /// - `md.resolution ~ '^\d+x\d+$'`：**只有 probe 写入的值**是这个形态。
    ///   脏值（空串、`1920*1080`、`HD`）一律排除，而不是让
    ///   `split_part(...)::int` 抛「invalid input syntax for type integer」。
    ///
    ///   这条正则是必需的，不是保险：没有它，一个脏值就会让整个查询
    ///   **报错**（而不是少算一部影片）—— 那是 500 级的故障。
    ///
    /// # 没有媒体的影片**不出现在结果里**
    ///
    /// `JOIN media` 是内连接，所以一部影片若没有任何合法媒体，整个分组
    /// 不存在。上游同样如此（`base.join(Media, ...)` + `where`），所以
    /// 「列表里有 3 部影片但只有 2 部计入档位」是**契约的一部分**，
    /// 不是缺陷。
    ///
    /// # 三个外键列名都**不是**上游 Peewee 的属性名
    ///
    /// 照抄上游表达式树会连撞三次，且每次都只在**运行时**才炸 ——
    /// `cargo check` / `clippy` / `cargo test --lib` 全绿，因为这条 SQL
    /// 从没被执行过：
    ///
    /// | 上游 Peewee | 真实 DDL 列 |
    /// |---|---|
    /// | `PlaylistMovie.playlist` | `playlist_movie.playlist_id` |
    /// | `PlaylistMovie.movie` | `playlist_movie.movie_id` |
    /// | `Media.movie` | `media.movie_number` |
    ///
    /// 外键列一律带 `_id` 后缀（`media` 那个例外：它指向
    /// `movie.movie_number` 这个**字符串**业务主键，所以列名也就叫
    /// `movie_number`）。
    ///
    /// 写错的后果都是 `column ... does not exist` 的 500，而不是静默少算 ——
    /// 这点是好事。集成测试见
    /// `crates/sm-service/tests/playlist_listing.rs`。
    pub async fn max_resolution_levels_by_playlist(
        &self,
        playlist_id: i32,
    ) -> Result<Vec<MovieResolutionLevelRow>, DbError> {
        let sql = format!(
            "SELECT m.id AS movie_id, MAX({RESOLUTION_LEVEL_CASE}) AS max_level \
             FROM movie m \
             JOIN playlist_movie pm ON pm.movie_id = m.id \
             JOIN media md ON md.movie_number = m.movie_number \
             WHERE pm.playlist_id = $1 AND md.valid = TRUE \
               AND md.resolution ~ '^\\d+x\\d+$' \
             GROUP BY m.id"
        );
        Ok(sqlx::query_as::<_, MovieResolutionLevelRow>(safe_sql(sql))
            .bind(playlist_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// 只写**互动数**那几个非受保护字段。`None` = 这一列不改。
    ///
    /// 对应上游 `update_movie_fields` 里"非受保护字段走窄更新"那一支
    /// （`catalog_import_service.py:518-526`）：受保护字段（title / summary /
    /// maker_name / director_name）必须走 `MovieOwnershipGateway`，而这几个
    /// 计数列**不在**受保护名单里 —— 网关也会拒它们。
    ///
    /// # 为什么要与热度分开
    ///
    /// `heat` 是推导列（上游公式），`recompute_heat_for` 专门管它。这里只碰
    /// 五个原始计数 —— 写热度会与公式版本打架。
    ///
    /// # 返回 0 行 = 影片不存在
    ///
    /// 调用方据此报 404，而不是"更新成功但什么都没改"。
    pub async fn update_interaction_counts(
        &self,
        movie_id: i32,
        score: Option<f64>,
        score_number: Option<i32>,
        watched_count: Option<i32>,
        want_watch_count: Option<i32>,
        comment_count: Option<i32>,
    ) -> Result<u64, DbError> {
        // `COALESCE($n, 列名)`：`None` 表示"这一列不动"，而不是"写成 NULL"。
        // 置空只能走 SQL 字面量 `NULL`（本仓纪律），不能靠 Option 混进来。
        let result = sqlx::query(
            "UPDATE movie SET \
                score = COALESCE($2, score), \
                score_number = COALESCE($3, score_number), \
                watched_count = COALESCE($4, watched_count), \
                want_watch_count = COALESCE($5, want_watch_count), \
                comment_count = COALESCE($6, comment_count), \
                updated_at = $7 \
              WHERE id = $1",
        )
        .bind(movie_id)
        .bind(score)
        .bind(score_number)
        .bind(watched_count)
        .bind(want_watch_count)
        .bind(comment_count)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// JavDB 补录：把 JavDB 那一份写进影片，并**清空**下次检查时间。
    ///
    /// 上游 `backfill_plugin_movie`（`catalog_import_service.py:355-481`）结尾
    /// 那两件事：`javdb_id` 落上、`javdb_next_check_at = NULL`（「不用再问了」）。
    ///
    /// # 置空只能写 SQL 字面量 `NULL`
    ///
    /// 本仓纪律：把 `Option::None` 绑进参数会与「这一列不动」混淆，所以清空
    /// 一律写成字面量。其余列的 `None` 是 **COALESCE 语义**（不动这一列）。
    ///
    /// # `AND javdb_id IS NULL` 不能省
    ///
    /// 并发下这部片可能刚被别的路径接入 JavDB。少了这一条会**覆盖**已有的
    /// `javdb_id`；返回 0 行让调用方据此判冲突（上游是显式抛 `ValueError`）。
    pub async fn apply_javdb_backfill(
        &self,
        movie_id: i32,
        javdb_id: Option<&str>,
        release_date: Option<chrono::NaiveDateTime>,
        duration_minutes: Option<i32>,
        series_id: Option<i32>,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE movie SET \
                javdb_id = COALESCE($2, javdb_id), \
                javdb_next_check_at = NULL, \
                release_date = COALESCE($3, release_date), \
                duration_minutes = COALESCE($4, duration_minutes), \
                series_id = COALESCE($5, series_id), \
                updated_at = $6 \
              WHERE id = $1 AND javdb_id IS NULL",
        )
        .bind(movie_id)
        .bind(javdb_id.map(str::trim))
        .bind(release_date)
        .bind(duration_minutes)
        .bind(series_id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 插件来源影片入库后补写 `metadata_source` 与下次检查时间。
    ///
    /// 上游 `import_plugin_movie`（`:295-352`）在建记录时就带上这两个值，而
    /// 本仓的 `NewMovie` 是「列的固定子集」（在对拍的豁免名单里），为避免把它
    /// 撑成第二份表镜像，这两列在建完之后再补写一次。
    ///
    /// `source` **不解释、不校验** —— 上游也是整个 dict 透传进 JSONB 列。
    pub async fn set_plugin_metadata_source(
        &self,
        movie_id: i32,
        source: &serde_json::Value,
        next_check_at: chrono::NaiveDateTime,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE movie SET metadata_source = $2, javdb_next_check_at = $3, updated_at = $4 \
              WHERE id = $1",
        )
        .bind(movie_id)
        .bind(source)
        .bind(next_check_at)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 设置竖封面。`image_id = None` = 清除。
    ///
    /// 供 `CatalogImportService::backfill_movie_thin_cover`（竖封面回填）。
    pub async fn set_thin_cover_image_id(
        &self,
        movie_id: i32,
        image_id: Option<i32>,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE movie SET thin_cover_image_id = $2, updated_at = $3 \
              WHERE id = $1",
        )
        .bind(movie_id)
        .bind(image_id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 插入。
    ///
    /// `javdb_id` 的空串在此归一为 `None` —— 库里出现空串会让
    /// `WHERE javdb_id = ''` 命中一条「没有 JavDB 编号」的假记录，
    /// 而唯一索引把第二条例外也挡掉了。
    ///
    /// DDL 里 `duration_minutes` / `score` / `heat` 等是 `NOT NULL DEFAULT 0`。
    /// **只有不写该列才会取默认值**；写 `NULL` 就是 NULL，直接违反 NOT NULL。
    /// 所以未提供的列在 SQL 里字面写 `DEFAULT`，由 PostgreSQL 填值 ——
    /// 这样默认值住在 schema 里，裸 SQL 插入与仓储插入的初始状态必然一致。
    /// 插入。
    ///
    /// `javdb_id` 的空串在此归一为 `None` —— 库里出现空串会让
    /// `WHERE javdb_id = ''` 命中一条「没有 JavDB 编号」的假记录。
    ///
    /// # NOT NULL DEFAULT 列必须发值，不能发 NULL
    ///
    /// DDL 里 `duration_minutes` / `score` 等是 `NOT NULL DEFAULT 0`。
    /// 发 `NULL` 会违反 NOT NULL —— 第一版就是这么写的，31 个集成测试全挂在
    /// 这里（SQLSTATE 23502 not_null_violation）。
    ///
    /// 另一种做法是在 SQL 里写 `DEFAULT` 关键字让 PostgreSQL 自己填，但那
    /// 会让绑定顺序与占位符编号错位：sqlx 的 `bind` 是顺序追加，没法跳过
    /// DEFAULT 那一项，于是 $5 类型无法推断（SQLSTATE 42P18）。
    ///
    /// 所以用固定 SQL + Rust 侧填默认值。`insert_defaults_match_ddl` 测试
    /// 锁定了这些默认值与 DDL 的一致性。
    pub async fn insert(&self, new: &NewMovie) -> Result<Movie, DbError> {
        let mut ctx = Ctx::over_pool(&self.pool);
        self.insert_in(&mut ctx, new).await
    }

    /// [`Self::insert`] 的事务内变体。见 [`Ctx`]。
    ///
    /// 「导入一部影片」用例需要它与标签 upsert、演员关联共享一个事务。
    pub async fn insert_in(&self, ctx: &mut Ctx<'_>, new: &NewMovie) -> Result<Movie, DbError> {
        let javdb_id = new
            .javdb_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let movie_number = new.movie_number.trim();
        if movie_number.is_empty() {
            return Err(DbError::business(ENTITY, "movie_number 不能为空"));
        }

        // 固定 15 个占位符 + 1 个复用的时间戳，顺序不可调换。
        let sql = "\
            INSERT INTO movie (
                movie_number, title, javdb_id, summary, maker_name, director_name,
                release_date, duration_minutes, score, score_number, series_id,
                cover_image_id, thin_cover_image_id, metadata_source,
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $15
            ) RETURNING *";

        let now = crate::common::time::now_utc();
        let row = sqlx::query_as::<_, Movie>(sql)
            .bind(movie_number)
            .bind(new.title.trim())
            .bind(javdb_id)
            // summary 是 NOT NULL DEFAULT ''：类型已是 String，绑 trimmed 值。
            .bind(new.summary.trim())
            .bind(new.maker_name.as_deref().map(str::trim))
            .bind(new.director_name.as_deref().map(str::trim))
            .bind(new.release_date)
            .bind(new.duration_minutes)
            .bind(new.score)
            .bind(new.score_number)
            .bind(new.series_id)
            .bind(new.cover_image_id)
            .bind(new.thin_cover_image_id)
            .bind(new.metadata_source.as_ref())
            .bind(now)
            .fetch_one(ctx.conn().await?.as_conn())
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        Ok(row)
    }
    /// 冲突（用户同时点了订阅和屏蔽），应该返回 422 并说清原因。
    pub async fn update(
        &self,
        id: i32,
        mut set: UpdateSet<'_>,
        source: WriteSource,
    ) -> Result<Movie, DbError> {
        // 1. 护栏。先只看**调用方**要写的字段 —— 此时还没 touch，
        //    updated_at 不该出现在护栏检查里（它是宿主内部行为）。
        let names: Vec<&str> = set.fields().iter().map(|(name, _)| *name).collect();
        guard().check_all(names.iter().copied(), source)?;

        // 2. CHECK 预判：只有当本次写入会碰到这两个位时才需要查当前值。
        if names
            .iter()
            .any(|n| *n == "is_subscribed" || *n == "is_blacklisted")
        {
            self.precheck_blacklist(id, set.fields()).await?;
        }

        // 3. 强制推进时间戳 —— 调用方无法绕过
        //
        // **空检查必须在 touch() 之前**：否则空 UpdateSet 会被 touch 填成
        // 一个只含 updated_at 的 patch，finish() 的空检查就失效了，
        // 最终执行 `SET updated_at = now()` —— 它合法但影响 0 行，
        // 会被误报成 NotFound（「行不存在」），而真实原因是「没东西可改」。
        if set.is_empty() {
            return Err(DbError::business(ENTITY, "没有要更新的字段"));
        }
        set.touch();

        // 占位符编号：字段从 $1 起（与 SET 子句书写顺序一致），id 放最后。
        let assignments = set.assignments(1);
        let fields = set.finish(ENTITY)?;
        let sql = format!(
            "UPDATE movie SET {assignments} WHERE id = ${}",
            fields.len() + 1
        );

        // 分两步：先 UPDATE，按 rows_affected 判定命中；再单独 SELECT 读回。
        //
        // 不用 `UPDATE ... RETURNING *`：那种写法下 0 行命中与「解码失败」
        // 都表现为同一个 Err，排查时看不出到底是哪个。拆开后命中判定是
        // 一个确定的数字，NotFound 也就能和「真的没这行」区分开。
        let query = fields
            .iter()
            .fold(sqlx::query(safe_sql(sql)), |query, (_, value)| {
                bind_value_exec(query, value)
            });
        let result = query.bind(id).execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::not_found(ENTITY, id));
        }
        self.require_by_id(id).await
    }

    /// CHECK 预判：合并当前值与本次写入后，`is_subscribed` 与
    /// `is_blacklisted` 是否会同时为真。
    async fn precheck_blacklist(
        &self,
        id: i32,
        fields: &[(&str, crate::common::update::Value<'_>)],
    ) -> Result<(), DbError> {
        let current = self.require_by_id(id).await?;

        let mut subscribed = current.is_subscribed;
        let mut blacklisted = current.is_blacklisted;
        for (name, value) in fields {
            match *name {
                "is_subscribed" => subscribed = as_bool(value),
                "is_blacklisted" => blacklisted = as_bool(value),
                _ => {}
            }
        }

        if subscribed && blacklisted {
            return Err(DbError::business(
                ENTITY,
                "is_subscribed 与 is_blacklisted 不能同时为真（数据库 CHECK 约束 \
                 movie_subscription_blacklist_exclusive 也会拒绝）",
            ));
        }
        Ok(())
    }

    paged_list! {
        /// 按订阅布尔值列出。**分页。**
        ///
        /// `total` 是该状态下的**全部**影片数，不是本页条数 —— 订阅列表页
        /// 要显示「共 N 部」，客户端 `fetch_all_pages` 也要靠它决定拉几页。
        ///
        /// 两条查询跑在同一个 REPEATABLE READ 快照里，否则并发订阅/退订时
        /// 两者会看到不同的世界，见 [`crate::common::page`] 模块文档。
        ///
        /// `NULLS LAST`：`subscribed_at` 对未订阅的影片是 NULL，按它倒序时
        /// 不写这个子句，PostgreSQL 会把 NULL 排在**最前** —— 未订阅的会
        /// 出现在列表顶部。
        pub async fn list_by_subscription(
            &self,
            subscribed: bool,
        ) -> Result<Page<Movie>, DbError> {
            count = "SELECT COUNT(*) FROM movie WHERE is_subscribed = $1",
            items = "SELECT * FROM movie WHERE is_subscribed = $1 \
                     ORDER BY subscribed_at DESC NULLS LAST, id \
                     LIMIT $2 OFFSET $3",
        }
    }

    /// 按订阅状态列出。**分页。**
    ///
    /// 薄包装：把 [`SubscriptionState`] 翻成布尔再交给
    /// [`list_by_subscription`](Self::list_by_subscription)。宏只能生成
    /// 「参数原样 bind」的方法，所以类型转换必须留在外面 —— 否则调用方
    /// 可能传一个裸 `bool` 而绕过这个枚举。
    pub async fn list_by_subscription_state(
        &self,
        state: SubscriptionState,
        page: PageRequest,
    ) -> Result<Page<Movie>, DbError> {
        self.list_by_subscription(state.as_bool(), page).await
    }

    /// 认领一条待刮削的影片。
    ///
    /// 用 `UPDATE ... WHERE id = $1 AND is_subscribed = false` 的单语句
    /// 写法而不是「先 SELECT 再 UPDATE」：前者靠行锁天然排他，
    /// 后者在两个 worker 之间会重复领取同一条。
    pub async fn claim_for_scrape(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE movie SET updated_at = $2 \
             WHERE id = $1 AND is_subscribed = false AND javdb_id IS NULL",
        )
        .bind(id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// 标记某字段的归属。宿主独占 —— 插件能写就能伪造所有权。
    ///
    /// 整个 `field_owners` 被**替换**而非合并，所以调用方要先读出当前值。
    /// 归属变更同时递增 `mutation_revision`：两个插件并发登记不同字段时，
    /// 后写的那次会靠版本号被发现覆盖了先前那次。
    pub async fn set_field_owner(
        &self,
        id: i32,
        field: &str,
        owner: &str,
    ) -> Result<Movie, DbError> {
        if !Movie::is_protected(field) {
            return Err(DbError::business(
                ENTITY,
                format!("{field} 不是受保护字段，无需登记归属"),
            ));
        }
        if owner != field_owner::HOST_MANUAL && !owner.starts_with("plugin:") {
            return Err(DbError::business(
                ENTITY,
                format!("owner 必须是 host:manual 或 plugin:<id>，收到 {owner}"),
            ));
        }

        let current = self.require_by_id(id).await?;
        let mut map = current
            .field_owners
            .as_object()
            .cloned()
            .unwrap_or_default();
        map.insert(
            field.to_owned(),
            serde_json::Value::String(owner.to_owned()),
        );

        let mut set = UpdateSet::new();
        set.set("field_owners", serde_json::Value::Object(map));
        set.set("mutation_revision", current.mutation_revision + 1);

        self.update(id, set, WriteSource::Host).await
    }

    // ------------------------------------------------------------ 热度重算

    /// 热度与公式**不一致**的影片数。
    ///
    /// `expression` 是「期望热度」的 SQL 表达式，由
    /// `sm_service::catalog::movie_heat::heat_expression_sql()` 生成 ——
    /// **公式留在 service 层**：上游也是这个分层（`movie_heat_service.py:19-28`
    /// 用 Peewee 表达式拼 SQL，ORM 只负责执行），仓储不该知道权重与参考值。
    ///
    /// 让仓储收一个 SQL 片段看着别扭，但替代方案更差：把公式抄成这里的
    /// 字面量就是**第二份**公式，改一处漏一处，而漏了的后果是两个入口
    /// （全表 / 单部）算出不同的热度且 `WHERE heat != computed` 永远判定不一致。
    ///
    /// 对应上游 `build_candidate_count_query`（`:31-33`）：`COUNT(movie.id)`，
    /// 连不是 `COUNT(*)` 这个写法都照抄（结果一样，但保持可对拍）。
    pub async fn count_stale_heat_in(
        &self,
        ctx: &mut Ctx<'_>,
        expression: &str,
    ) -> Result<i64, DbError> {
        // 动态 SQL 的**审计依据**：`expression` 只可能来自
        // `movie_heat::heat_expression_sql()` —— 它由本仓的 f64 常量经 `{:?}`
        // 拼出，没有任何路径能把用户输入或数据库内容喂进去。见 [`safe_sql`]。
        let sql = format!("SELECT COUNT(movie.id) FROM movie WHERE movie.heat != ({expression})");
        sqlx::query_scalar::<_, i64>(safe_sql(sql))
            .fetch_one(ctx.conn().await?.as_conn())
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 全表重算：`UPDATE movie SET heat = <公式> WHERE heat != <公式>`。
    /// 返回**实际更新行数**。
    ///
    /// 对应上游 `build_update_query`（`:35-41`）。那个 `!=` 不是优化而是语义：
    /// 少了它就是 30 万行的全表写。
    ///
    /// ⚠️ **不碰 `updated_at`** —— 上游用的是类级 `Model.update(...)`，它绕过
    /// 推进 `updated_at` 的实例方法覆写（`model/mixins.py:21-22`），所以热度
    /// 重算不改变影片的「最后修改时刻」。别顺手补 `updated_at = now()`。
    pub async fn recompute_heat_in(
        &self,
        ctx: &mut Ctx<'_>,
        expression: &str,
    ) -> Result<u64, DbError> {
        // 审计依据同 `count_stale_heat_in`：`expression` 来自常量，无外部输入。
        let sql =
            format!("UPDATE movie SET heat = ({expression}) WHERE movie.heat != ({expression})");
        let outcome = sqlx::query(safe_sql(sql))
            .execute(ctx.conn().await?.as_conn())
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(outcome.rows_affected())
    }

    /// 单部影片的热度重算，对应上游 `build_single_movie_update_query`（`:43-49`）：
    /// 在同一个 `!=` 条件之外再加 `id = ?`。
    ///
    /// 返回**实际更新行数**，所以 `0` 同时含义「影片不存在」与「热度已经是对的」
    /// —— 上游不区分（`:52-53` 只回 `execute()` 的结果），调用方也别去补一个
    /// 404：手动重算的语义是「确保它是对的」，已经对时 0 是正确结果。
    pub async fn recompute_heat_for(
        &self,
        movie_id: i32,
        expression: &str,
    ) -> Result<u64, DbError> {
        let sql = format!(
            "UPDATE movie SET heat = ({expression}) \
             WHERE movie.id = $1 AND movie.heat != ({expression})"
        );
        // 审计依据同上：唯一的变量部分还是那条常量表达式（`movie_id` 走绑定参数）。
        let outcome = sqlx::query(safe_sql(sql))
            .bind(movie_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(outcome.rows_affected())
    }
}

/// 订阅状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionState {
    Subscribed,
    NotSubscribed,
}

impl SubscriptionState {
    /// 该状态对应的 `is_subscribed` 布尔值。
    ///
    /// 列本身是 boolean，所以只保留这一个转换。
    /// 曾经还有一个 `as_str() -> "true" / "false"`，用来把状态转成字符串
    /// 再跟 `"true"` 比较 —— 那绕了一圈布尔，而列要的是 `bool`。
    /// 现在查询直接绑 `as_bool()`。
    pub fn as_bool(&self) -> bool {
        matches!(self, Self::Subscribed)
    }
}

/// 从 UpdateSet 的值里取布尔。
fn as_bool(value: &crate::common::update::Value<'_>) -> bool {
    matches!(&*value.0, crate::common::update::ValueInner::Bool(true))
}

/// 生成「把 [`UpdateSet`](crate::common::update::UpdateSet) 的值绑到
/// 查询上」的两个函数。
///
/// sqlx 的 `bind` 是泛型方法且**按类型静态分发**，所以无返回行的 `Query`
/// 与有返回行的 `QueryAs` 各要一份签名。它们的 `match` 逻辑完全相同 ——
/// 写两遍的话，加一个 [`ValueInner`] 变体就可能只改一处，而漏掉的那处
/// 会在编译期不报错（因为 match 仍然穷尽）、运行时才崩。
///
/// 用宏生成保证两份永远同步。编译器会在变体不匹配时报错。
///
/// `$generics` 给的是额外的类型参数：`Query` 没有输出类型，所以传 `[]`；
/// 而 `QueryAs` 需要一个 `O` 表示返回的行类型。
///
/// 目前只有 `Query` 那一个实例存活 —— `task.rs` 的 `update_metadata`
/// 曾经用 `QueryAs` 版本，但那条路径因为 `RETURNING *` 会把「0 行命中」
/// 与「解码失败」压成同一个 Err 而改用了 `execute` + `rows_affected`。
/// 宏保留着，因为下一个需要动态 SET 子句并读回结果的地方会立刻用上；
/// 只有一个实例时它看起来多余，但删掉之后 `ValueInner` 一旦新增变体，
/// 两处（这里 + 未来的那处）就要各改一次，而漏掉的那处编译期不报错。
macro_rules! impl_bind_value {
    ($name:ident, [$($gen:ident),*], $query:ty, $doc:literal) => {
        #[doc = $doc]
        pub(crate) fn $name<'q, $($gen),*>(
            query: $query,
            value: &crate::common::update::Value<'_>,
        ) -> $query {
            use crate::common::update::ValueInner;
            match &*value.0 {
                ValueInner::Null => query.bind(Option::<String>::None),
                ValueInner::Bool(v) => query.bind(*v),
                ValueInner::Int(v) => query.bind(*v),
                ValueInner::Float(v) => query.bind(*v),
                ValueInner::Text(v) => query.bind(v.clone()),
                ValueInner::Timestamp(v) => query.bind(*v),
                ValueInner::Json(v) => query.bind(v.clone()),
            }
        }
    };
}

impl_bind_value!(
    bind_value_exec,
    [],
    sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    "把值绑到无返回行的 `query` 上。"
);

/// 把受控的动态 SQL 交给 sqlx 0.9。
///
/// sqlx 0.9 新增了编译期注入防护：`query` / `query_as` 的 SQL 参数必须实现
/// [`sqlx::SqlSafeStr`]，而 `&String` **不实现**它 —— 想用动态 SQL 就
/// 必须显式声明「我已确认这段 SQL 安全」。
///
/// 目前有**两处**调用，审计依据各自不同，写在这里免得后来人只看见一处：
///
/// 1. [`bind_value_exec`] 那条路径（`task.rs` 的 `update_metadata` 等）：列名
///    全部来自 [`UpdateSet`]，而调用方只能通过 `set()` 传入**字面量列名**，
///    没有任何路径能把用户输入拼进 SQL。占位符数量由 `assignments()` 按字段数
///    生成，与 bind 数量严格一致。
/// 2. 热度重算（[`MovieRepository::count_stale_heat_in`] 等）：被拼进去的只有
///    `movie_heat::heat_expression_sql()` —— 它由本仓的 f64 常量经 `{:?}` 生成，
///    不含任何外部数据；影片 id 走绑定参数。
///
/// **加新调用点前先想清楚依据**：这个包装是「我已审计」的声明，不是消音器。
pub(crate) fn safe_sql(sql: impl Into<String>) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(sql.into())
}

/// `movie_series` 表仓储。
#[derive(Debug, Clone)]
pub struct MovieSeriesRepository {
    pool: PgPool,
}

impl MovieSeriesRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 按名称查询。系列名在 save 前被 strip，避免「同一系列两个实体」。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<MovieSeries>, DbError> {
        let row = sqlx::query_as::<_, MovieSeries>("SELECT * FROM movie_series WHERE name = $1")
            .bind(name.trim())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 按主键**批量**取回，返回 `id → MovieSeries`。**没命中的 id 不在结果里。**
    ///
    /// 影片卡片只用到系列**名**，而它是 `movie.series_id` 指向的另一张表 ——
    /// 逐部影片查一次就是 N+1。见 [`MovieRepository::find_by_ids`] 的同款说明。
    pub async fn find_by_ids(&self, ids: &[i32]) -> Result<HashMap<i32, MovieSeries>, DbError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows =
            sqlx::query_as::<_, MovieSeries>("SELECT * FROM movie_series WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }
}

impl MovieRepository {
    /// ★ 演员 id → 番号，**带墓碑解析**（`merged_into_id`）。
    ///
    /// 上游 `list_media`（`media_service.py:281-290`）先把 `actor_ids` 解析成正规
    /// id（`COALESCE(merged_into, id)`）再换成番号。这里一次性做完，避免每个
    /// actor_id 一次额外查询。
    pub async fn numbers_for_actor_ids(&self, actor_ids: &[i32]) -> Result<Vec<String>, DbError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT m.movie_number FROM movie m \
               JOIN movie_actor ma ON ma.movie_id = m.id \
              WHERE ma.actor_id IN (SELECT COALESCE(a.merged_into_id, a.id) \
                                      FROM actor a WHERE a.id = ANY($1))",
        )
        .bind(actor_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(number,)| number).collect())
    }

    /// 每日推荐的**全库候选**：全部「非集合、未拉黑」影片的 5 列投影。
    ///
    /// 对应上游 `_load_candidate_movies`（`daily_recommendation_service.py:130-154`）：
    ///
    /// ```python
    /// Movie.select(Movie.id, Movie.heat, Movie.release_date, Movie.created_at,
    ///              Movie.is_subscribed)
    ///      .where(Movie.is_collection == False, Movie.is_blacklisted == False)
    ///      .order_by(Movie.id.asc())
    /// ```
    ///
    /// # 只投影 5 列，是**内存**问题不是风格问题
    ///
    /// 上游注释（`:132`）：全库 30 万行若加载完整模型实例，「**实测峰值
    /// 3.9GB**」。这个是每日推荐任务能在小内存机器上跑起来的前提。
    /// `is_collection` 只进 `WHERE`，不进投影。
    ///
    /// # `ORDER BY id` 不能省
    ///
    /// 打分是纯函数、结果由 `sort_scored` 的 tie-breaker 决定 —— 但**同分且
    /// 同日**的影片最终次序取决于候选取回顺序（`sort_by` 是稳定的）。少了
    /// 排序，两次生成之间同分影片的相对位置会漂，而快照每天重写一次，
    /// 那种漂移会被用户看见。
    ///
    /// # 拉黑影片**不进入候选**，而不是「进候选后得 0 分」
    ///
    /// 上游把过滤放在 SQL 里。放在打分侧会让它们在 `freshness` 里占排名位次
    /// （拉黑的影片越多，真实候选的 freshness 分被压得越低）。
    pub async fn list_daily_candidates(&self) -> Result<Vec<CandidateMovieRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT id, heat, release_date, created_at, is_subscribed FROM movie \
             WHERE is_collection = false AND is_blacklisted = false \
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
