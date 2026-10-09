//! 热度重算的**真库对拍**：SQL 表达式 ↔ 纯函数 [`heat_of`]。
//!
//! # 为什么必须有这一份
//!
//! 热度有**两处实现**（`heat_expression_sql()` 拼的 SQL 与 `heat_of()` 的
//! Rust 版本），而更新条件是 `WHERE heat != <表达式>`。两处差一个尾数，
//! 症状不是报错，而是**每轮全表重算都判定「不一致」**：`updated_count`
//! 永远等于候选数、`heat` 在两次实现之间来回跳。
//!
//! 单元测试只能证明「表达式里确实带着当前常量」这类文本性质 ——
//! 它证明不了 PostgreSQL 算出来的整数与 Rust 算出来的整数相同。
//! 本文件用真库把两者逐行比一次。
//!
//! 顺带钉住三条只有真库能验的性质：
//!
//! | 性质 | 为什么重要 |
//! |---|---|
//! | `WHERE heat != computed` 的增量语义 | 第二次跑必须 `updated_count = 0` |
//! | 单部重算只动那一部 | 否则每天 5 点的任务退化成全表写 |
//! | **不 clamp** | 计数为负时热度为负；clamp 会让爆款之间没有差别 |

use sm_db::repo::{MovieRepository, NewMovie};
use sm_db::testing::TestDb;
use sm_service::catalog::movie_heat::{heat_of, HeatInput, MovieHeatService};

/// 建一部影片，并把四个互动计数写成给定值（`NewMovie` 不含这三个计数列）。
async fn seed_movie(db: &TestDb, number: &str, input: HeatInput) -> i32 {
    let repo = MovieRepository::new(db.pool().clone());
    let movie = repo
        .insert(&NewMovie {
            movie_number: number.to_owned(),
            title: format!("{number} 标题"),
            ..NewMovie::default()
        })
        .await
        .expect("insert movie");

    // `heat` 故意先写成一个**明显不一致**的值：列默认是 0，而「全零计数」那部
    // 的期望热度**恰好也是 0** —— 不摆一个错值，它根本进不了候选集，
    // `candidate_count` 会少 1，而这是用例自己的错，不是实现的错。
    sqlx::query(
        "UPDATE movie SET watched_count = $2, want_watch_count = $3, \
         comment_count = $4, score_number = $5, heat = $6 \
         WHERE id = $1",
    )
    .bind(movie.id)
    .bind(input.watched_count as i32)
    .bind(input.want_watch_count as i32)
    .bind(input.comment_count as i32)
    .bind(input.score_number as i32)
    .bind(SEEDED_WRONG_HEAT)
    .execute(db.pool())
    .await
    .expect("写入互动计数");

    movie.id
}

/// 种子里那个「明显不一致」的热度初值。见 [`seed_movie`] 的注释。
const SEEDED_WRONG_HEAT: i32 = 999_999;

async fn raw_heat(pool: &sqlx::PgPool, id: i32) -> i32 {
    sqlx::query_scalar("SELECT heat FROM movie WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("heat read")
}

/// 一批覆盖各种形状的计数：
///
/// - 全零（新片）；
/// - 四项都到 P99（应落在 `HEAT_SCALE` 附近）；
/// - 只有一项有值（验单项权重）；
/// - 大字计数（验 `CAST(... AS REAL)` 那一步不会因为 `int4` 溢出）；
/// - 负计数（验**不 clamp**）。
fn fixtures() -> [(&'static str, HeatInput); 5] {
    [
        (
            "HEAT-ZERO",
            HeatInput {
                watched_count: 0,
                want_watch_count: 0,
                comment_count: 0,
                score_number: 0,
            },
        ),
        (
            "HEAT-P99",
            HeatInput {
                watched_count: 1308,
                want_watch_count: 4991,
                comment_count: 41,
                score_number: 6291,
            },
        ),
        (
            "HEAT-COMMENTS",
            HeatInput {
                watched_count: 0,
                want_watch_count: 0,
                comment_count: 41,
                score_number: 0,
            },
        ),
        (
            "HEAT-BIG",
            HeatInput {
                watched_count: 2_000_000,
                want_watch_count: 9_000_000,
                comment_count: 3_000_000,
                score_number: 7_000_000,
            },
        ),
        (
            "HEAT-NEGATIVE",
            HeatInput {
                watched_count: -100,
                want_watch_count: -200,
                comment_count: -50,
                score_number: 0,
            },
        ),
    ]
}

/// ★ SQL 与纯函数必须给出**逐行相同**的整数。
#[tokio::test]
async fn the_sql_expression_agrees_with_the_pure_function() {
    let db = TestDb::require().await;
    let service = MovieHeatService::new(db.pool());

    let mut expected = Vec::new();
    for (number, input) in fixtures() {
        let id = seed_movie(&db, number, input).await;
        expected.push((number, id, heat_of(input)));
    }

    let result = service.update_movie_heat().await.expect("全表重算");

    assert_eq!(
        result.candidate_count,
        expected.len() as i64,
        "五部影片的 heat 初始都是 0，都该进候选"
    );
    assert_eq!(result.updated_count, expected.len() as i64);

    for (number, id, want) in expected {
        let got = raw_heat(db.pool(), id).await;
        assert_eq!(
            got, want as i32,
            "{number}: SQL 算出 {got}，纯函数算出 {want} —— \
             两处实现分叉了，而 `WHERE heat != computed` 会因此永远判定不一致"
        );
    }
}

/// ★ 已经算对的行**不再被写**：第二次跑必须零更新。
///
/// 这条是 `WHERE heat != computed` 那个条件的存在理由。少了它，每天 5 点
/// 那次任务会把 30 万行全部重写一遍（并刷新 `updated_at`）。
#[tokio::test]
async fn a_second_pass_updates_nothing() {
    let db = TestDb::require().await;
    let service = MovieHeatService::new(db.pool());

    for (number, input) in fixtures() {
        seed_movie(&db, number, input).await;
    }

    let first = service.update_movie_heat().await.expect("第一遍");
    assert_eq!(first.updated_count, 5);

    let second = service.update_movie_heat().await.expect("第二遍");
    assert_eq!(
        second.candidate_count, 0,
        "第一遍之后不该还有「不一致」的行"
    );
    assert_eq!(second.updated_count, 0);
}

/// ★ 单部重算只动那一部；已经对的那部返回 0（**不是** 404）。
#[tokio::test]
async fn a_single_recompute_does_not_touch_its_neighbours() {
    let db = TestDb::require().await;
    let service = MovieHeatService::new(db.pool());

    let target = HeatInput {
        watched_count: 1308,
        want_watch_count: 0,
        comment_count: 0,
        score_number: 0,
    };
    let neighbour = HeatInput {
        watched_count: 0,
        want_watch_count: 4991,
        comment_count: 0,
        score_number: 0,
    };
    let target_id = seed_movie(&db, "HEAT-ONE", target).await;
    let neighbour_id = seed_movie(&db, "HEAT-TWO", neighbour).await;

    let updated = service
        .update_single_movie_heat(target_id)
        .await
        .expect("单部重算");
    assert_eq!(updated, 1, "那一部的 heat 从 0 变成期望值");

    assert_eq!(raw_heat(db.pool(), target_id).await, heat_of(target) as i32);
    assert_eq!(
        raw_heat(db.pool(), neighbour_id).await,
        SEEDED_WRONG_HEAT,
        "邻居不该被动过（它还留着种子里那个错值）"
    );

    // 再来一次：已经对了，返回 0（上游不区分「不存在」与「已经对」）。
    let again = service
        .update_single_movie_heat(target_id)
        .await
        .expect("再算一次");
    assert_eq!(again, 0);
}

/// ★ **不 clamp**：计数为负时热度为负，不会被压到 0/某个下限。
///
/// clamp 是「看起来更安全」的那种改动，代价是所有爆款的热度变成同一个数
/// —— 排序失去区分度，而且没有任何报错。
#[tokio::test]
async fn negative_counters_produce_negative_heat_without_clamping() {
    let db = TestDb::require().await;
    let service = MovieHeatService::new(db.pool());

    let input = HeatInput {
        watched_count: -100,
        want_watch_count: -200,
        comment_count: -50,
        score_number: 0,
    };
    let id = seed_movie(&db, "HEAT-NEG", input).await;

    service.update_movie_heat().await.expect("全表重算");

    let heat = raw_heat(db.pool(), id).await;
    assert_eq!(heat, heat_of(input) as i32);
    assert!(heat < 0, "上游不 clamp，负数就是负数：{heat}");
}
