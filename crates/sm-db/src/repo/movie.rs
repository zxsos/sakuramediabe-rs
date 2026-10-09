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

use sqlx::{PgPool, Postgres};

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
const RESOLUTION_LEVEL_CASE: &str = "CASE \
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

    /// 插入。
    ///
    /// `javdb_id` 的空串在此归一为 `None` —— 库里出现空串会让
    /// `WHERE javdb_id = ''` 命中一条「没有 JavDB 编号」的假记录，
    /// 而唯一索引把第二条例外也挡掉了。
    ///
    /// # 可选列必须发 DEFAULT 而不是 NULL
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
/// 确认依据：列名全部来自 [`UpdateSet`]，而调用方只能通过 `set()` 传入
/// 字面量列名，没有任何路径能把用户输入拼进 SQL。占位符数量由
/// `assignments()` 按字段数生成，与 bind 数量严格一致。
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
}
