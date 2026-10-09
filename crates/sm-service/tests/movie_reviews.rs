//! `MovieReviewService` 的**服务层**分支测试。
//!
//! # 分层与覆盖面
//!
//! | 层 | 覆盖在哪 |
//! |---|---|
//! | JavDB 载荷解析 / 键名换算 | `catalog::javdb` 单测 + `tests/javdb_provider.rs`（打桩 HTTP）|
//! | 本文件的三个分支 | 影片不存在 → 404；没绑 `javdb_id` → **空列表**；来源挂了 → 502 |
//!
//! 远端 404 统一映射成本地 404 那条分支也在这里 —— 它是上游注释明写的
//! 「仍统一映射为影片不存在」，details 里多出的 `javdb_id` 是排查入口。
//!
//! # 串行锁
//!
//! 与 `media_file_hash_backfill` 同一个夹具级事实：advisory lock 键
//! `(namespace, media_id)` 不区分 schema，每个测试 schema 的 id 又都从 1 开始
//! —— 并行跑会互相占锁。豁免 `await_holding_lock` 是**故意的**：锁的目的
//! 就是跨整个用例（含 await 点）互斥。
mod support;

use std::pin::Pin;
use std::sync::Arc;

use sm_db::testing::TestDb;
use sm_service::catalog::javdb::JavdbMovieReview;
use sm_service::catalog::metadata_source::MetadataSourceError;
use sm_service::catalog::movie_reviews::{JavdbReviewSource, MovieReviewService};

/// trait 方法返回类型的别名（显式生命周期使签名冗长，这里收一份）。
type ReviewFuture<'a> = Pin<
    Box<
        dyn std::future::Future<Output = Result<Vec<JavdbMovieReview>, MetadataSourceError>>
            + Send
            + 'a,
    >,
>;

/// 一次调用收到的参数：`(javdb_id, page, limit, sort_by)`。
type RecordedCall = (String, i64, i64, Option<String>);

/// 全绿剧本（这条链路里 provider 不该失败）。
struct ScriptedSource {
    outcome: Result<Vec<JavdbMovieReview>, MetadataSourceError>,
    /// 记下收到的调用参数 ——「没绑 id 就不该问 provider」这类断言靠它。
    calls: std::sync::Mutex<Vec<RecordedCall>>,
}

impl ScriptedSource {
    fn returning(outcome: Result<Vec<JavdbMovieReview>, MetadataSourceError>) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl JavdbReviewSource for ScriptedSource {
    fn movie_reviews<'a>(
        &'a self,
        javdb_id: &'a str,
        page: i64,
        limit: i64,
        sort_by: Option<&'a str>,
    ) -> ReviewFuture<'a> {
        self.calls.lock().expect("锁").push((
            javdb_id.to_owned(),
            page,
            limit,
            sort_by.map(str::to_owned),
        ));
        Box::pin(std::future::ready(self.outcome.clone()))
    }
}

fn review(id: i64) -> JavdbMovieReview {
    JavdbMovieReview {
        id,
        score: 8,
        content: "好".to_owned(),
        created_at: None,
        username: "alice".to_owned(),
        like_count: 1,
        watch_count: 2,
        movie: None,
    }
}

/// 把共享的桩装进 trait 对象 —— 断言要用原来的 `Arc` 句柄，provider 拿的
/// 得是同一个实例（用例里记的 `calls` 都要在）。
struct Shared(Arc<ScriptedSource>);

impl JavdbReviewSource for Shared {
    fn movie_reviews<'a>(
        &'a self,
        javdb_id: &'a str,
        page: i64,
        limit: i64,
        sort_by: Option<&'a str>,
    ) -> ReviewFuture<'a> {
        self.0.movie_reviews(javdb_id, page, limit, sort_by)
    }
}

fn service(pool: &sqlx::PgPool, source: Arc<ScriptedSource>) -> MovieReviewService {
    MovieReviewService::with_provider(pool, Box::new(Shared(source)))
}

/// 本地片没绑 `javdb_id` → **200 空列表**，且 provider 一次都不被问。
///
/// 把「没有评论」报成 404 会诱导客户端把这部片标成「JavDB 没收录」——
/// 而真实情况只是导入时没拿到远端 id。上游明确是 `return []`。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_movie_without_a_javdb_id_returns_an_empty_list() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    support::seed_movie_if_missing(&db, "NO-ID-001").await;

    let source = ScriptedSource::returning(Ok(vec![review(1)]));
    let error_or_reviews = service(db.pool(), source.clone())
        .get_movie_reviews("NO-ID-001", 1, 20, Some("recently"))
        .await;

    let reviews = error_or_reviews.expect("不该报错");
    assert!(reviews.is_empty(), "没绑 id 就是「没有评论」");
    assert!(
        source.calls.lock().expect("锁").is_empty(),
        "provider 不该被问 —— 没有远端 id 可问"
    );
}

/// 影片不存在 → 404 `movie_not_found`（provider 同样不被问）。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_unknown_movie_is_a_404() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;

    let source = ScriptedSource::returning(Ok(vec![review(1)]));
    let error = service(db.pool(), source.clone())
        .get_movie_reviews("GHOST-001", 1, 20, None)
        .await
        .expect_err("该 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "movie_not_found");
    assert!(source.calls.lock().expect("锁").is_empty());
}

/// 正常链路：参数原样透传给 provider，映射结果原样返回。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_bound_movie_forwards_the_query_to_the_source() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    support::seed_movie_if_missing(&db, "BOUND-001").await;
    sqlx::query("UPDATE movie SET javdb_id = 'J-99' WHERE movie_number = 'BOUND-001'")
        .execute(db.pool())
        .await
        .expect("绑 javdb_id");

    let source = ScriptedSource::returning(Ok(vec![review(7)]));
    let reviews = service(db.pool(), source.clone())
        .get_movie_reviews("BOUND-001", 3, 15, Some("hotly"))
        .await
        .expect("取评论");
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].id, 7);
    let calls = source.calls.lock().expect("锁");
    assert_eq!(calls.len(), 1, "只问一次");
    let (javdb_id, page, limit, sort) = &calls[0];
    assert_eq!(javdb_id, "J-99");
    assert_eq!((*page, *limit), (3, 15), "分页参数原样透传");
    assert_eq!(sort.as_deref(), Some("hotly"), "排序原样透传");
}

/// 远端 404 统一映射成本地 404，details **多带 `javdb_id`** ——
/// 「本地缺片」与「远端缺评论」是两种不同的 404，用户看到的文案相同，
/// 排查时全靠 details 分。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_remote_not_found_maps_to_a_local_404_with_the_javdb_id() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    support::seed_movie_if_missing(&db, "BOUND-404").await;
    sqlx::query("UPDATE movie SET javdb_id = 'J-GONE' WHERE movie_number = 'BOUND-404'")
        .execute(db.pool())
        .await
        .expect("绑 javdb_id");

    let source = ScriptedSource::returning(Err(MetadataSourceError::NotFound));
    let error = service(db.pool(), source)
        .get_movie_reviews("BOUND-404", 1, 20, None)
        .await
        .expect_err("该 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "movie_not_found");
    let message = format!("{:?}", error.api);
    assert!(
        message.contains("J-GONE"),
        "details 要带 javdb_id：{message}"
    );
}

/// 来源失败 → 502 `movie_review_fetch_failed`（**不是** 404）：
/// 网络抖动和「没有评论」是两件事，混掉会让客户端把远端故障当成空数据
/// 缓存下来。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_source_failure_is_a_502() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    support::seed_movie_if_missing(&db, "BOUND-502").await;
    sqlx::query("UPDATE movie SET javdb_id = 'J-ERR' WHERE movie_number = 'BOUND-502'")
        .execute(db.pool())
        .await
        .expect("绑 javdb_id");

    let source =
        ScriptedSource::returning(Err(MetadataSourceError::RequestFailed("超时".to_owned())));
    let error = service(db.pool(), source)
        .get_movie_reviews("BOUND-502", 1, 20, None)
        .await
        .expect_err("该 502");
    assert_eq!(error.status, 502);
    assert_eq!(error.code(), "movie_review_fetch_failed");
}
