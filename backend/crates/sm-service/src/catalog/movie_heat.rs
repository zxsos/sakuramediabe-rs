//! 影片热度重算（上游 `catalog/movie_heat_service.py`，70 行）。
//!
//! # 公式：固定 P99 参考值的**线性累计**
//!
//! ```text
//!   heat = ROUND(HEAT_SCALE * ( 7/34 * watched/WATCHED_REF
//!                             + 5/34 * want_watch/WANT_REF
//!                             +17/34 * comment/COMMENT_REF
//!                             + 5/34 * score_number/SCORE_REF ))
//! ```
//!
//! # ⚠️ **不设上限、不 clamp** —— 这是刻意的
//!
//! 上游注释（`:20`、`:27`）：
//!
//! > 按固定 P99 参考值做线性累计，**保留头部原始计数差异，不设置热度上限**。
//! > `HEAT_SCALE` 只是 P99 附近的**展示基准**，参考值以上继续线性增长。
//!
//! 我第一版骨架在这里写了 `clamp(0, 1)` 与「截到满值」—— **那是错的**，
//! 而且错得隐蔽：clamp 会让上百部爆款影片的热度**完全相同**，
//! 「保留头部差异」这个设计意图被抹掉，而头部差异正是热度排序的主要信息。
//!
//! 负数输入会算出负热度（上游也不防）—— `heat` 列没有 `CHECK (heat >= 0)`。
//! 照抄，别加 clamp。
//!
//! # 参考值是 **P99**，写死不再变
//!
//! 上游注释（`:11`）：「参考值取当前业务库各互动字段的 P99，**固定后**避免
//! 全库分布变化导致**历史热度漂移**」。
//!
//! 这与 `discovery::daily_recommendation` 里的 95 分位**不同**（那里每次现算
//! 分位）。两套做法别混用。
//!
//! # 更新是**增量**的：`WHERE heat != computed`
//!
//! 上游 `:33` / `:40` / `:48` 三处都有。所以：
//!
//! | 返回值 | 含义 |
//! |---|---|
//! | `candidate_count` | 热度与公式**不一致**的影片数（不是全库行数） |
//! | `updated_count` | 实际被 UPDATE 的行数 |
//!
//! 少了这个条件，每次都是 30 万行的全表 UPDATE。
//!
//! ⚠️ 顺带纠正一个容易想当然的地方：这次 UPDATE **不碰 `updated_at`**。
//! 上游用的是类级批量写 `Model.update(...)`，它**绕过**实例方法，而推进
//! `updated_at` 的覆写在 `TimestampedMixin.save()` 里 ——
//! `model/mixins.py:21-22` 自己就写明了「`Model.update(...)` / `insert_many(...)`
//! 这类批量写完全绕过实例方法，仍需调用方自己带上 updated_at」。上游这里
//! **没带**，所以热度重算不改变影片的「最后修改时刻」。照抄，别顺手补上。
//!
//! 两个计数**语义不同**：前者在事务内先数（用于日志），后者是执行结果，
//! 并发写入时两者会差。

use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::error::ServiceError;

/// 公式版本。**改公式必须改它**，否则无法区分新旧公式算出的热度。
pub const FORMULA_VERSION: &str = "v6";

/// 观看数 P99 参考值。
pub const WATCHED_COUNT_REFERENCE: f64 = 1308.0;
/// 想看数 P99 参考值。
pub const WANT_WATCH_COUNT_REFERENCE: f64 = 4991.0;
/// 评论数 P99 参考值。
pub const COMMENT_COUNT_REFERENCE: f64 = 41.0;
/// 评分人数 P99 参考值。
pub const SCORE_NUMBER_REFERENCE: f64 = 6291.0;
/// 展示基准刻度。**不是上限**（见模块文档）。
pub const HEAT_SCALE: f64 = 3100.0;

/// 四项权重。分母写 **34**（= 7+5+17+5），**不折成小数** —— 这样与上游
/// `:22-25` 的 `7.0 / 34.0` 逐字对应，改权重时一眼能看出动的是哪一项。
///
/// 四项之和**恰好**是 1（34/34），所以四项都达到 P99 时 `ROUND(...)` 正好落在
/// `HEAT_SCALE`。`COMMENT` 是 17/34 = 0.5，**绝对主导**。
pub mod weights {
    /// 观看数权重 `7/34`。
    ///
    /// ⚠️ 它**不是**最小的：`WANT_WATCH` / `SCORE_NUMBER` 是 5/34，比它小。
    /// 骨架期这里写过「观看数权重最低」，并被一条同名用例钉住了 —— 那个前提
    /// 与上游矛盾（见下面测试模块的 `the_weight_order_matches_upstream`）。
    pub const WATCHED: f64 = 7.0 / 34.0;
    /// 想看数权重 `5/34` —— 与 `SCORE_NUMBER` 同为最低。
    pub const WANT_WATCH: f64 = 5.0 / 34.0;
    /// 评论数权重 `17/34` —— **绝对主导**（占一半）。
    pub const COMMENT: f64 = 17.0 / 34.0;
    /// 评分人数权重 `5/34`。
    pub const SCORE_NUMBER: f64 = 5.0 / 34.0;
}

/// 期望热度的 **SQL 表达式**（可嵌进 `SELECT` / `UPDATE`）。
///
/// # 为什么是拼字符串，而不是 SQLx 的编译期宏
///
/// 上游用 Peewee 表达式在**运行时**拼 SQL（`movie_heat_service.py:19-28`），
/// 权重与参考值是常量。这里等价：表达式由本文件的常量拼出，仓储只负责把它
/// 塞进语句（`crates/sm-db/src/repo/movie.rs` 的 `count_stale_heat_in` /
/// `recompute_heat_in`）。
///
/// 用 `sqlx::query!` 反倒做不到 —— 那是编译期校验，读不到运行时常量。
/// 也不该把公式抄成 SQL 字符串字面量：那就成了**第二份**公式，改一处漏一处。
///
/// # 类型与运算次序**照抄**上游，别自行「改进」
///
/// | 上游 `movie_heat_service.py` | 这里 |
/// |---|---|
/// | `Movie.watched_count.cast("REAL")` | `CAST(movie.watched_count AS REAL)` |
/// | `(7.0 / 34.0) * ... / 1308` | 先把权重折成同一个 double，再 `* 计数 / 参考值` |
/// | `fn.ROUND(...)` | `ROUND(...)` |
/// | `.cast("INTEGER")` | `::INTEGER` |
///
/// `::float8` 不是装饰：Python 的 `7.0 / 34.0` 在导入时就折成一个 double，
/// Peewee 把它作为**参数**绑定（PostgreSQL 眼里是 `float8`）；而 SQL 里裸写
/// `0.20588235294117646` 会被解析成 `numeric`。不显式写成 `::float8`，就等于
/// 把「命中哪个除法运算符」交给 PostgreSQL 的类型提升规则去猜。
///
/// 计数那侧的 `CAST(... AS REAL)`（`float4`）也照抄：计数 < 2^24（16,777,216）
/// 时无损，超过时上游**同样**先丢精度 —— 别一个人在 Rust 侧「修好」它。
///
/// # 与 [`heat_of`] 的关系
///
/// 两个入口（全表 / 单部）都走这条 SQL，[`heat_of`] 是同一公式的 Rust 版本，
/// 用于单测与「不连库也要能算出期望值」的场合。改公式**三处一起改**：
/// 常量、这条表达式、[`heat_of`]。
pub fn heat_expression_sql() -> String {
    use weights::{COMMENT, SCORE_NUMBER, WANT_WATCH, WATCHED};
    format!(
        "ROUND(((({w:?}::float8 * CAST(movie.watched_count AS REAL)) / {wr:?}::float8) \
          + (({ww:?}::float8 * CAST(movie.want_watch_count AS REAL)) / {wwr:?}::float8) \
          + (({c:?}::float8 * CAST(movie.comment_count AS REAL)) / {cr:?}::float8) \
          + (({s:?}::float8 * CAST(movie.score_number AS REAL)) / {sr:?}::float8)) \
         * {scale:?}::float8)::INTEGER",
        w = WATCHED,
        wr = WATCHED_COUNT_REFERENCE,
        ww = WANT_WATCH,
        wwr = WANT_WATCH_COUNT_REFERENCE,
        c = COMMENT,
        cr = COMMENT_COUNT_REFERENCE,
        s = SCORE_NUMBER,
        sr = SCORE_NUMBER_REFERENCE,
        // `HEAT_SCALE` 在 `ROUND` 的**里面**：上游是
        // `fn.ROUND(normalized_heat * HEAT_SCALE)`（`:28`），先放大再取整。
        // 放到 `ROUND` 外面就成了「先取整再乘 3100」，量级差三个数量级。
        scale = HEAT_SCALE,
    )
}

/// 一部影片的重算输入。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HeatInput {
    pub watched_count: i64,
    pub want_watch_count: i64,
    pub comment_count: i64,
    pub score_number: i64,
}

/// ★ 单部影片的热度。**纯函数** —— 与 [`heat_expression_sql`] 同一条公式，
/// 用来在不连库的场合算出「期望值」。
///
/// # 运算次序必须与 SQL 一致，否则尾数会分叉
///
/// SQL 那边是 `(权重 * 计数) / 参考值`（先乘后除）。写成
/// `权重 * (计数 / 参考值)` 虽然数学上等价，浮点上却会在**末位**不同 ——
/// 而这里四舍五入到整数，末位差正好会在 `.5` 附近翻面。同一份公式的两处
/// 实现算出不同的整数，`WHERE heat != computed` 就会判定「永远不一致」。
///
/// `ROUND` 在 PostgreSQL 里是四舍五入（**远离零**），Rust 的 `.round()`
/// 语义相同。用 `.floor()` 会变成向零取整，与 SQL 不一致。
///
/// # 与 SQL 的**已知**差异：计数转 `REAL`
///
/// SQL 先把计数 `CAST(... AS REAL)`（`float4`），这里直接用 `f64`。计数小于
/// 2^24（16,777,216）时 `float4` 无损，两者逐位相同；再大就会先丢精度 ——
/// 那是**上游的行为**（`movie_heat_service.py:22-25` 的 `.cast("REAL")`），
/// 真有那种量级的计数时，以入库的 SQL 结果为准，不要来改这里。
pub fn heat_of(input: HeatInput) -> i64 {
    use weights::{COMMENT, SCORE_NUMBER, WANT_WATCH, WATCHED};
    // 先乘后除 —— 与 SQL 的 `(权重 * 计数) / 参考值` 同一个次序。
    let normalized = (WATCHED * input.watched_count as f64) / WATCHED_COUNT_REFERENCE
        + (WANT_WATCH * input.want_watch_count as f64) / WANT_WATCH_COUNT_REFERENCE
        + (COMMENT * input.comment_count as f64) / COMMENT_COUNT_REFERENCE
        + (SCORE_NUMBER * input.score_number as f64) / SCORE_NUMBER_REFERENCE;
    (normalized * HEAT_SCALE).round() as i64
}

/// 全表重算结果。**键名与上游逐字一致**（会进任务摘要）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MovieHeatUpdateResult {
    /// 热度与公式不一致的影片数（事务内先数）。
    pub candidate_count: i64,
    /// 实际更新的行数。
    pub updated_count: i64,
    /// 公式版本。
    pub formula_version: &'static str,
}

/// 热度服务。
///
/// # 为什么要有状态（骨架期是 `pub struct MovieHeatService;`）
///
/// 两个方法都要发 SQL，而全表重算的「先数后更」必须在**同一个事务**里 ——
/// 所以它得持有连接池与仓储。与
/// [`MediaService`](crate::playback::media::MediaService) 同一个取向：单元结构体 +
/// 关联函数拿不到任何仓储，于是两个方法都落不了地。
pub struct MovieHeatService {
    movies: MovieRepository,
    pool: Db,
}

impl MovieHeatService {
    /// 构造。
    pub fn new(db: &Db) -> Self {
        Self {
            movies: MovieRepository::new(db.clone()),
            pool: db.clone(),
        }
    }

    /// ★ 全表重算。返回「数出来多少条不一致」与「实际更新了多少行」。
    ///
    /// 上游 `movie_heat_service.py:55-70` 在 `database.atomic()` 里**先数后更**。
    /// 顺序不能反：反过来数的是「已经一致」的行，`candidate_count` 恒为 0 ——
    /// 那个字段正是用来判断「这次跑有没有东西可干」的。
    ///
    /// # 为什么是 `begin()`（READ COMMITTED）而不是 `in_snapshot_tx`
    ///
    /// `in_snapshot_tx` 会 `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ`，
    /// 那在并发重算同一批行时会抛 `could not serialize access due to concurrent
    /// update` —— 上游的 `atomic()` 只是 READ COMMITTED，并发时短暂阻塞即可。
    /// 而两个计数本来就不保证相等（并发写入会让 `updated_count` 小于
    /// `candidate_count`），没有理由用更强的隔离换一次中止。
    pub async fn update_movie_heat(&self) -> Result<MovieHeatUpdateResult, ServiceError> {
        let expression = heat_expression_sql();
        let mut tx = self.pool.begin().await?;

        // `ctx` 借用 `tx`，所以算出结果就出块结束借用，之后才能 `commit`。
        // 中途 `?` 直接返回时 `tx` 被 drop —— sqlx 自动回滚，不必手写 rollback。
        let (candidate_count, updated_count) = {
            let mut ctx = sm_db::repo::Ctx::in_tx(&mut tx, &self.pool);
            let candidate_count = self
                .movies
                .count_stale_heat_in(&mut ctx, &expression)
                .await?;
            // 上游在这里就记一条 info（`:60-64`）——「有多少部要重算」是运维
            // 判断「这次跑是不是空转」的唯一线索，别省。
            tracing::info!(
                formula_version = FORMULA_VERSION,
                candidate_movies = candidate_count,
                "影片热度重算开始"
            );
            let updated_count = self.movies.recompute_heat_in(&mut ctx, &expression).await?;
            (candidate_count, updated_count)
        };

        tx.commit().await?;

        Ok(MovieHeatUpdateResult {
            candidate_count,
            // `rows_affected` 是 u64，而任务摘要里是 i64。任何真实表的行长都远
            // 小于 i64::MAX（说是「不可能溢出」的那种断言通常靠不住，但这里
            // 上界是「一次 UPDATE 命中的行数」，而表本身就有行长上界）。
            updated_count: updated_count as i64,
            formula_version: FORMULA_VERSION,
        })
    }

    /// 单部影片重算。返回**实际更新行数**（`0` = 影片不存在**或**热度已对）。
    ///
    /// 两种情况都返回 0，上游**不区分** —— 照抄，别加「不存在就 404」。
    /// 手动重算的语义是「确保它是对的」，已经对时返回 0 是正确结果。
    ///
    /// [`crate::catalog::movie_interaction_sync`] 写回互动数后就是调它：
    /// 那里刚改了一部影片的计数，只该重算那一部。
    pub async fn update_single_movie_heat(&self, movie_id: i32) -> Result<u64, ServiceError> {
        let expression = heat_expression_sql();
        Ok(self
            .movies
            .recompute_heat_for(movie_id, &expression)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全零 → 0。
    #[test]
    fn a_movie_with_no_signals_has_no_heat() {
        assert_eq!(heat_of(HeatInput::default()), 0);
    }

    /// ★ 超出 P99 的信号**必须继续线性增长**，不得截断。
    ///
    /// 这是本文件最重要的一条：clamp 会让所有爆款影片热度相同，
    /// 「保留头部原始计数差异」的设计意图就没了。
    #[test]
    fn signals_beyond_p99_keep_growing() {
        let at_p99 = HeatInput {
            watched_count: 1308,
            want_watch_count: 4991,
            comment_count: 41,
            score_number: 6291,
        };
        let ten_x = HeatInput {
            watched_count: 13080,
            want_watch_count: 49910,
            comment_count: 410,
            score_number: 62910,
        };
        let base = heat_of(at_p99);
        let scaled = heat_of(ten_x);
        assert!(scaled > base, "10 倍信号应有更高热度：{base} -> {scaled}");
        assert!((scaled - base * 10).abs() <= 2, "应约为 10 倍");
    }

    /// ★ 评论数是**主导项**（17/34，占一半）。
    ///
    /// 搞反权重会完全改变热度排序，而排序直接决定所有列表的呈现。
    ///
    /// `assertions_on_constants` 在这里是**误报**：两侧都是 `const`，编译期
    /// 就能定值 —— 而这正是本条用例的目的（把权重比例钉死，谁改动谁红）。
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn comments_dominate() {
        use weights::*;
        assert!((COMMENT - 0.5).abs() < 1e-9, "评论权重就是 0.5");
        assert!(COMMENT > WATCHED && COMMENT > WANT_WATCH && COMMENT > SCORE_NUMBER);
    }

    /// ★ 四项权重的**真实**顺序与取值（上游 `movie_heat_service.py:22-25`）。
    ///
    /// # 这条用例此前是**错的**，值得写下来
    ///
    /// 它原来叫 `watched_count_has_the_smallest_weight`，断言
    /// `WATCHED < WANT_WATCH` 与 `WATCHED < SCORE_NUMBER` —— 注释还写着
    /// 「权重顺序照抄 7 < 5 < 17 < 5 里的相对关系」。但 **7 不小于 5**：
    /// 上游是 7/34（观看）· 5/34（想看）· 17/34（评论）· 5/34（评分人数），
    /// 也就是说 `WATCHED` 比另外两个 5/34 的项**大**。那两条断言必然失败，
    /// 而它们失败时看起来像「常量被人改坏了」，真实原因却是断言自己写反了。
    ///
    /// 所以这里不再只断言大小关系，而是**逐个断言等于 `n/34`** —— 大小关系
    /// 可以被「两边一起改错」骗过，具体到 `7/34` 不行。
    ///
    /// 理由同 [`comments_dominate`]：断言常量是刻意的。
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_weight_order_matches_upstream() {
        use weights::*;
        assert!((WATCHED - 7.0 / 34.0).abs() < 1e-12, "观看数 7/34");
        assert!((WANT_WATCH - 5.0 / 34.0).abs() < 1e-12, "想看数 5/34");
        assert!((COMMENT - 17.0 / 34.0).abs() < 1e-12, "评论数 17/34");
        assert!((SCORE_NUMBER - 5.0 / 34.0).abs() < 1e-12, "评分人数 5/34");

        // 大小关系：评论 > 观看 > 想看 == 评分人数。
        assert!(COMMENT > WATCHED);
        assert!(WATCHED > WANT_WATCH);
        assert!((WANT_WATCH - SCORE_NUMBER).abs() < 1e-12, "这两项同权重");
        // 四项之和恰好是 1 —— 所以四项都到 P99 时正好落在 HEAT_SCALE。
        assert!((WATCHED + WANT_WATCH + COMMENT + SCORE_NUMBER - 1.0).abs() < 1e-12);
    }

    /// ★ SQL 表达式与常量**同源**：权重/参考值改了，表达式跟着改。
    ///
    /// 把公式抄进 SQL 字符串最容易出的错是「改了一处漏了另一处」—— 那种漏
    /// 会让全表重算与单部重算给出不同的热度，且 `WHERE heat != computed`
    /// 会永远认为不一致。这里断言表达式里确实带着当前常量。
    #[test]
    fn the_sql_expression_is_built_from_the_same_constants() {
        let sql = heat_expression_sql();
        for expected in [
            format!("{:?}", weights::WATCHED),
            format!("{:?}", weights::WANT_WATCH),
            format!("{:?}", weights::COMMENT),
            format!("{:?}", weights::SCORE_NUMBER),
            format!("{:?}", WATCHED_COUNT_REFERENCE),
            format!("{:?}", WANT_WATCH_COUNT_REFERENCE),
            format!("{:?}", COMMENT_COUNT_REFERENCE),
            format!("{:?}", SCORE_NUMBER_REFERENCE),
            format!("{:?}", HEAT_SCALE),
        ] {
            assert!(
                sql.contains(&expected),
                "表达式里缺 {expected}：公式的第二份副本被漏改了\n{sql}"
            );
        }
        // 三个类型标注是**契约**：REAL 的计数转换、float8 的常量、INTEGER 的收口。
        assert!(sql.contains("CAST(movie.watched_count AS REAL)"));
        assert!(sql.contains("::float8"));
        assert!(sql.ends_with("::INTEGER"));
        // 不 clamp：表达式里不该出现 LEAST / GREATEST。
        assert!(!sql.contains("LEAST") && !sql.contains("GREATEST"));
    }

    /// 单一信号的热度可预测：只有评论数时约为 `0.5 * HEAT_SCALE`。
    #[test]
    fn a_single_signal_scales_by_its_weight() {
        let only_comments = HeatInput {
            comment_count: 41,
            ..HeatInput::default()
        };
        let expected = (0.5 * HEAT_SCALE).round() as i64;
        assert_eq!(heat_of(only_comments), expected);
    }

    /// 四项全 P99 时落在 `HEAT_SCALE` 附近。
    #[test]
    fn all_signals_at_p99_land_near_the_scale() {
        let at_p99 = HeatInput {
            watched_count: 1308,
            want_watch_count: 4991,
            comment_count: 41,
            score_number: 6291,
        };
        let heat = heat_of(at_p99);
        // 权重和 34/34 = 1 -> 恰好 HEAT_SCALE。
        assert_eq!(heat, HEAT_SCALE as i64);
    }

    /// 负数输入**不**被夹到 0（上游也不防，照抄）。
    ///
    /// 这里断言的是「不要偷偷加 clamp」—— 一旦有人为了「数据更干净」加上
    /// clamp，这条测试会失败并提醒他那是契约变更。
    #[test]
    fn negative_inputs_are_not_clamped_even_though_it_looks_dirty() {
        let dirty = HeatInput {
            watched_count: -100,
            ..HeatInput::default()
        };
        assert!(heat_of(dirty) < 0, "上游不防负数，别加 clamp");
    }

    /// 公式版本是**非空常量**。
    #[test]
    fn the_formula_version_is_pinned() {
        assert_eq!(FORMULA_VERSION, "v6");
    }
}
