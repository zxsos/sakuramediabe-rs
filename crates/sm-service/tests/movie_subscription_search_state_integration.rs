//! 订阅检索状态机的**真库**集成测试。
//!
//! # 为什么必须有这一份
//!
//! 这台状态机有三条「写错了不报错、只是悄悄不工作」的规则：
//!
//! | 规则 | 写错的后果 |
//! |---|---|
//! | `begin_attempt` **不做状态条件** | 加上 `AND state = 'pending'` 后，`failed_retryable` 的影片永远领不到 —— **重试功能整个没了** |
//! | 失败扣预算要看「是否新鲜」 | 漏掉后，刚上映的新片几次就耗尽预算、**永久不再搜索** |
//! | 崩溃恢复落 `failed_retryable` | 落 `pending` 会让「被打断」看起来像「从没搜过」 |
//!
//! 前两条在单元测试里只能验纯函数部分（`is_search_candidate` / `is_fresh`），
//! 「条件 UPDATE 到底改了哪几行」只有真库能给答案。
//!
//! 另一处**只有真库能验**的是「`reset(None)` 只动 `exhausted`」——
//! 那是一条 `WHERE` 子句，而不是 Rust 里的分支。

use std::path::PathBuf;

use chrono::{Duration, NaiveDateTime};
use sm_db::common::time::now_utc;
use sm_db::repo::{MovieRepository, NewMovie};
use sm_db::testing::TestDb;
use sm_service::catalog::movie_subscription_search_state::{
    MovieSubscriptionSearchStateService, SubscriptionSearchError, ERROR_CODE_TASK_INTERRUPTED,
    INTERRUPTED_ERROR_MESSAGE, STATE_EXHAUSTED, STATE_FAILED_RETRYABLE, STATE_PENDING,
    STATE_RUNNING, STATE_SUCCEEDED,
};
use sm_service::system::config::ConfigService;

/// 一份只写 `[downloads]` 的临时配置（schema 缺省会 overlay 上来）。
struct TempConfig {
    service: ConfigService,
    path: PathBuf,
}

impl TempConfig {
    fn new(stale_attempt_limit: i64, fresh_days: i64) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "sm-subsearch-{}-{}.toml",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &path,
            format!(
                "[downloads]\n\
                 subscription_search_stale_attempt_limit = {stale_attempt_limit}\n\
                 subscription_search_fresh_days = {fresh_days}\n"
            ),
        )
        .expect("写临时配置");
        Self {
            service: ConfigService::new(path.to_string_lossy().into_owned()),
            path,
        }
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// 建一部影片并把它摆成给定的检索状态。
async fn seed(
    db: &TestDb,
    number: &str,
    subscribed: bool,
    release_date: Option<NaiveDateTime>,
    state: &str,
    attempt_count: i32,
) -> i32 {
    let repo = MovieRepository::new(db.pool().clone());
    let movie = repo
        .insert(&NewMovie {
            movie_number: number.to_owned(),
            title: format!("{number} 标题"),
            ..NewMovie::default()
        })
        .await
        .expect("insert movie");

    sqlx::query(
        "UPDATE movie SET is_subscribed = $2, release_date = $3, \
         subscription_search_state = $4, subscription_search_attempt_count = $5 \
         WHERE id = $1",
    )
    .bind(movie.id)
    .bind(subscribed)
    .bind(release_date)
    .bind(state)
    .bind(attempt_count)
    .execute(db.pool())
    .await
    .expect("摆状态");

    movie.id
}

/// 两年前的发行日 = 「不新鲜」（缺省新鲜期 90 天）。
fn stale_release() -> NaiveDateTime {
    now_utc() - Duration::days(730)
}

/// 今天的发行日 = 「新鲜」。
fn fresh_release() -> NaiveDateTime {
    now_utc() - Duration::days(1)
}

#[derive(Debug, PartialEq)]
struct SearchState {
    state: String,
    attempt_count: i32,
    retry_round: i32,
    error_code: Option<String>,
    last_error: Option<String>,
    next_retry_at: Option<NaiveDateTime>,
    last_attempted_at: Option<NaiveDateTime>,
    last_error_at: Option<NaiveDateTime>,
}

async fn read_state(pool: &sqlx::PgPool, id: i32) -> SearchState {
    #[allow(clippy::type_complexity)]
    let row: (
        String,
        i32,
        i32,
        Option<String>,
        Option<String>,
        Option<NaiveDateTime>,
        Option<NaiveDateTime>,
        Option<NaiveDateTime>,
    ) = sqlx::query_as(
        "SELECT subscription_search_state, subscription_search_attempt_count, \
         subscription_search_retry_round, subscription_search_error_code, \
         subscription_search_last_error, subscription_search_next_retry_at, \
         subscription_search_last_attempted_at, subscription_search_last_error_at \
         FROM movie WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("读检索状态");
    SearchState {
        state: row.0,
        attempt_count: row.1,
        retry_round: row.2,
        error_code: row.3,
        last_error: row.4,
        next_retry_at: row.5,
        last_attempted_at: row.6,
        last_error_at: row.7,
    }
}

// --------------------------------------------------------------- begin_attempt

#[tokio::test]
async fn begin_attempt_moves_to_running_and_stamps_the_attempt() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-001",
        true,
        Some(stale_release()),
        STATE_PENDING,
        0,
    )
    .await;

    let claimed = service.begin_attempt(id).await.expect("领取");
    assert_eq!(claimed, Some(id));

    let state = read_state(db.pool(), id).await;
    assert_eq!(state.state, STATE_RUNNING);
    assert!(state.last_attempted_at.is_some(), "必须记下尝试时刻");
    assert_eq!(state.next_retry_at, None, "进了 running，退避时间就该清掉");
}

#[tokio::test]
async fn begin_attempt_returns_none_for_a_missing_movie() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);

    assert_eq!(
        service.begin_attempt(999_999).await.expect("不该报错"),
        None
    );
}

/// ★ ★ 本条是这个文件存在的首要理由：`begin_attempt` **不挑状态**。
///
/// 上游只按 id 更新。而候选查询放行的状态里**包含 `failed_retryable`**
/// （重试就是这么来的）—— 一旦有人「顺手」加上 `AND state = 'pending'`，
/// 重试链路会静默失效，且不报任何错。
#[tokio::test]
async fn begin_attempt_also_claims_a_retryable_failure() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-002",
        true,
        Some(stale_release()),
        STATE_FAILED_RETRYABLE,
        1,
    )
    .await;

    let claimed = service.begin_attempt(id).await.expect("领取");
    assert_eq!(
        claimed,
        Some(id),
        "failed_retryable 必须能被重新领取 —— 这就是「重试」本身"
    );
    assert_eq!(read_state(db.pool(), id).await.state, STATE_RUNNING);
}

// ------------------------------------------------------------ mark_succeeded

#[tokio::test]
async fn mark_succeeded_clears_the_budget_and_the_error() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-003",
        true,
        Some(stale_release()),
        STATE_RUNNING,
        2,
    )
    .await;

    service.mark_succeeded(id).await.expect("标记成功");

    let state = read_state(db.pool(), id).await;
    assert_eq!(state.state, STATE_SUCCEEDED);
    assert_eq!(state.attempt_count, 0, "终态要清预算");
    assert_eq!(state.error_code, None);
    assert_eq!(state.last_error, None);
    assert_eq!(state.next_retry_at, None);
}

// --------------------------------------------------------------- mark_failed

/// ★ 「没找到候选」**不扣**预算（`consumes_budget = false`）。
#[tokio::test]
async fn no_candidate_does_not_consume_the_budget() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-004",
        true,
        Some(stale_release()),
        STATE_RUNNING,
        2,
    )
    .await;

    let landed = service
        .mark_failed(id, &SubscriptionSearchError::no_candidate())
        .await
        .expect("标记失败");

    let state = read_state(db.pool(), id).await;
    assert_eq!(landed, STATE_FAILED_RETRYABLE);
    assert_eq!(state.attempt_count, 2, "计数不能涨");
    assert_eq!(state.error_code.as_deref(), Some("no_candidate_found"));
    assert_eq!(state.next_retry_at, None);
}

/// ★ ★ 第二重要的规则：**新鲜期内失败不扣预算**（哪怕 `consumes_budget = true`）。
///
/// 漏掉它，刚上映的新片会因为「还没人发片」几次耗尽预算 —— 而那部影片
/// 永久不再被搜索，用户毫不知情。
#[tokio::test]
async fn a_fresh_movie_failure_does_not_consume_the_budget() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-005",
        true,
        Some(fresh_release()),
        STATE_RUNNING,
        2,
    )
    .await;

    let landed = service
        .mark_failed(id, &SubscriptionSearchError::indexer_failed("索引器 500"))
        .await
        .expect("标记失败");

    let state = read_state(db.pool(), id).await;
    assert_eq!(landed, STATE_FAILED_RETRYABLE, "新鲜期内不判死");
    assert_eq!(state.attempt_count, 2, "计数不能涨");
    assert_eq!(state.error_code.as_deref(), Some("indexer_search_failed"));
    assert_eq!(state.last_error.as_deref(), Some("索引器 500"));
}

/// 旧片的可重试失败：计数 +1；连撞上限后落 `exhausted`（终态）。
#[tokio::test]
async fn a_stale_movie_burns_its_budget_and_then_gives_up() {
    let db = TestDb::require().await;
    // 上限 3 次
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-006",
        true,
        Some(stale_release()),
        STATE_RUNNING,
        0,
    )
    .await;

    let error = SubscriptionSearchError::indexer_failed("索引器超时");
    for expected_count in 1..=2 {
        let landed = service.mark_failed(id, &error).await.expect("标记失败");
        assert_eq!(landed, STATE_FAILED_RETRYABLE, "还没到上限");
        assert_eq!(
            read_state(db.pool(), id).await.attempt_count,
            expected_count
        );
    }

    let landed = service.mark_failed(id, &error).await.expect("第三次");
    let state = read_state(db.pool(), id).await;
    assert_eq!(landed, STATE_EXHAUSTED, "第 3 次到上限 → 放弃");
    assert_eq!(state.state, STATE_EXHAUSTED);
    assert_eq!(state.attempt_count, 3);
}

/// 没有发行日期 = **不新鲜**（按普通影片算，不给无限预算）。
#[tokio::test]
async fn a_movie_without_a_release_date_is_not_fresh() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(&db, "SUB-007", true, None, STATE_RUNNING, 0).await;

    service
        .mark_failed(id, &SubscriptionSearchError::indexer_failed("x"))
        .await
        .expect("标记失败");

    assert_eq!(read_state(db.pool(), id).await.attempt_count, 1, "该扣预算");
}

#[tokio::test]
async fn mark_failed_on_a_missing_movie_is_a_404() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);

    let error = service
        .mark_failed(999_999, &SubscriptionSearchError::no_candidate())
        .await
        .expect_err("影片不存在");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "movie_not_found");
}

/// ★ 上限**从配置读**：把它配成 1，第一次扣预算的失败就该落 `exhausted`。
///
/// 这条同时证明「配置真的被读了」——写死 3 的实现会在这里失败。
#[tokio::test]
async fn the_attempt_limit_comes_from_the_config() {
    let db = TestDb::require().await;
    let config = TempConfig::new(1, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    assert_eq!(service.stale_attempt_limit().expect("读配置"), 1);

    let id = seed(
        &db,
        "SUB-008",
        true,
        Some(stale_release()),
        STATE_RUNNING,
        0,
    )
    .await;

    let landed = service
        .mark_failed(id, &SubscriptionSearchError::indexer_failed("x"))
        .await
        .expect("标记失败");
    assert_eq!(landed, STATE_EXHAUSTED);
}

// --------------------------------------------------------------------- reset

/// 指定 id 的重置：回到 `pending`、清计数、`retry_round` **加一**。
#[tokio::test]
async fn reset_rewinds_the_listed_movies() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-009",
        true,
        Some(stale_release()),
        STATE_EXHAUSTED,
        3,
    )
    .await;

    let affected = service.reset(Some(&[id])).await.expect("重置");
    assert_eq!(affected, 1);

    let state = read_state(db.pool(), id).await;
    assert_eq!(state.state, STATE_PENDING);
    assert_eq!(state.attempt_count, 0);
    assert_eq!(state.retry_round, 1, "retry_round 是加一，不是清零");
    assert_eq!(state.error_code, None);
}

/// ★ `None` 只重置**已放弃**的，且**只动已订阅的**影片。
///
/// 两个口径都在 SQL 的 `WHERE` 里：`is_subscribed = TRUE`，且
/// `subscription_search_state = 'exhausted'`。漏掉后者会把正在等的影片
/// 也打回起点（重置计数、丢掉退避），用户会看到进度无故回退。
#[tokio::test]
async fn a_bare_reset_only_rewinds_exhausted_subscribed_movies() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);

    let exhausted = seed(
        &db,
        "SUB-010",
        true,
        Some(stale_release()),
        STATE_EXHAUSTED,
        3,
    )
    .await;
    let waiting = seed(
        &db,
        "SUB-011",
        true,
        Some(stale_release()),
        STATE_FAILED_RETRYABLE,
        1,
    )
    .await;
    // 已放弃但**没订阅**：不该被动。
    let unsubscribed = seed(
        &db,
        "SUB-012",
        false,
        Some(stale_release()),
        STATE_EXHAUSTED,
        3,
    )
    .await;

    let affected = service.reset(None).await.expect("重置");
    assert_eq!(affected, 1, "只有那一部已订阅且已放弃");

    assert_eq!(read_state(db.pool(), exhausted).await.state, STATE_PENDING);
    assert_eq!(
        read_state(db.pool(), waiting).await.state,
        STATE_FAILED_RETRYABLE,
        "还在等的不动"
    );
    assert_eq!(
        read_state(db.pool(), unsubscribed).await.state,
        STATE_EXHAUSTED,
        "没订阅的不管"
    );
}

/// **空数组与 `None` 等价**（上游 `if movie_ids:` 对空列表为假）——
/// 这不是「什么都不做」。
#[tokio::test]
async fn an_empty_id_list_behaves_like_no_filter() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);
    let id = seed(
        &db,
        "SUB-013",
        true,
        Some(stale_release()),
        STATE_EXHAUSTED,
        3,
    )
    .await;

    let affected = service.reset(Some(&[])).await.expect("重置");
    assert_eq!(affected, 1, "空数组 = 不过滤 = 重置所有已放弃的");
    assert_eq!(read_state(db.pool(), id).await.state, STATE_PENDING);
}

// ------------------------------------------------- recover_interrupted_running

/// ★ 崩溃恢复：`running` → `failed_retryable`（**不是** `pending`），
/// 并留下 `task_interrupted` 与固定文案。
#[tokio::test]
async fn recovery_turns_a_stuck_running_movie_into_a_retryable_failure() {
    let db = TestDb::require().await;
    let config = TempConfig::new(3, 90);
    let service = MovieSubscriptionSearchStateService::new(db.pool(), &config.service);

    let stuck = seed(
        &db,
        "SUB-014",
        true,
        Some(stale_release()),
        STATE_RUNNING,
        1,
    )
    .await;
    let idle = seed(
        &db,
        "SUB-015",
        true,
        Some(stale_release()),
        STATE_PENDING,
        0,
    )
    .await;

    let repaired = service.recover_interrupted_running().await.expect("恢复");
    assert_eq!(repaired, 1, "只有那一部卡在 running");

    let state = read_state(db.pool(), stuck).await;
    assert_eq!(
        state.state, STATE_FAILED_RETRYABLE,
        "「被打断」不是「从没搜过」"
    );
    assert_eq!(
        state.error_code.as_deref(),
        Some(ERROR_CODE_TASK_INTERRUPTED)
    );
    assert_eq!(state.last_error.as_deref(), Some(INTERRUPTED_ERROR_MESSAGE));
    assert!(
        state.last_error_at.is_some(),
        "last_error_at 必须写上（任务中心按它显示「最近一次失败」）"
    );
    assert_eq!(read_state(db.pool(), idle).await.state, STATE_PENDING);
}
