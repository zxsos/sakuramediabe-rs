//! `GET /daily-recommendations` 的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 这层测什么
//!
//! 读侧只做四件事，但每一件都容易「看起来对」：
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 排序 | 严格按 `rank`，**不是**按 `score` / 入库序 | 生成侧已按 `score` 排好并写进 `rank` |
//! | 响应元素 | **完整影片卡片** + 8 个推荐字段 | 上游 `DailyRecommendationMovieResource` 继承 `MovieListItemResource` |
//! | 拉黑影片 | **不占分页槽位**（`items` 与 `total` 一起少） | `COUNT` 与 `SELECT` 必须带同一个 `is_blacklisted = false` |
//! | `is_stale` | **元素级**：`snapshot_date < 今天` | 不是页级字段，跨天翻页要靠它 |
//!
//! 还有一条**空快照**路径：库里没有推荐时返回 `items: []` / `total: 0` ——
//! 生成侧还没接（见 `routes/recommendations.rs` 的 ⚠️），这是当前的真实行为。
//!
//! 每个用例的 `TestDb` 是独立 schema。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, NaiveDate, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    DailyRecommendationItemRepository, MovieRepository, NewDailyRecommendation, NewMovie, NewUser,
    UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "daily-rec-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-daily-{tag}-{}", unique()));
        std::fs::create_dir_all(&base).expect("建临时目录");
        let config_path = base.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[auth]\nfile_signature_secret = \"{SECRET}\"\n"),
        )
        .expect("写测试配置");
        Self { config_path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.config_path.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

async fn seed_token(db: &Db) -> String {
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: format!("dr{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

fn app(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(fixture.config_path.clone()),
    ))
}

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "响应体不是 JSON: {err}; 原始: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

// ================================================================ 造数

/// 造一部影片，返回 `(id, 番号)`。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("DR-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "每日推荐测试影片".to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: None,
            duration_minutes: 0,
            score: 0.0,
            score_number: 0,
            series_id: None,
            cover_image_id: None,
            thin_cover_image_id: None,
            metadata_source: None,
        })
        .await
        .expect("insert movie");
    (movie.id, number)
}

async fn set_blacklisted(db: &TestDb, movie_id: i32, blacklisted: bool) {
    sqlx::query("UPDATE movie SET is_blacklisted = $2 WHERE id = $1")
        .bind(movie_id)
        .bind(blacklisted)
        .execute(db.pool())
        .await
        .expect("set is_blacklisted");
}

/// 整批写一张快照。`replace_all` 是本表唯一正确的写入方式（`rank` 全表唯一）。
async fn seed_snapshot(db: &TestDb, items: &[(i32, i32, f64, NaiveDate)]) {
    let rows: Vec<NewDailyRecommendation> = items
        .iter()
        .map(|(movie_id, rank, score, day)| NewDailyRecommendation {
            snapshot_date: *day,
            movie_id: *movie_id,
            rank: *rank,
            score: *score,
            reason_codes: Some(r#"["popular_movie"]"#.to_owned()),
            reason_texts: Some(r#"["近期热度较高"]"#.to_owned()),
            signal_scores: Some(r#"{"heat":0.5}"#.to_owned()),
            generated_at: sm_db::common::time::now_utc(),
        })
        .collect();
    DailyRecommendationItemRepository::new(db.pool().clone())
        .replace_all(&rows)
        .await
        .expect("replace_all 快照");
}

fn days_ago(days: i64) -> NaiveDate {
    chrono::Local::now().date_naive() - Duration::days(days)
}

// ================================================================ 用例

#[tokio::test]
async fn empty_snapshot_returns_an_empty_page() {
    // 生成侧还没接，库里没有快照是**正常状态** —— 不是 500，也不是 404。
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(app(&db, &fixture), get("/daily-recommendations", &token)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["total"], 0);
    // 回显**请求的**分页参数（缺省 page=1 / page_size=20）。
    assert_eq!(body["page"], 1);
    assert_eq!(body["page_size"], 20);
}

#[tokio::test]
async fn items_are_rank_ordered_and_carry_the_full_card_plus_recommendation_fields() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;

    let (a_id, a_number) = seed_movie(&db).await;
    let (b_id, b_number) = seed_movie(&db).await;
    let day = days_ago(0);
    // 故意让 rank 与「入库顺序」相反：rank=1 是后写的 b。
    seed_snapshot(&db, &[(a_id, 2, 0.4, day), (b_id, 1, 0.9, day)]).await;

    let (status, body) = send(app(&db, &fixture), get("/daily-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 2);

    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 2);
    // 排序按 rank（= 生成侧的 score 序），不是入库序。
    assert_eq!(items[0]["rank"], 1);
    assert_eq!(items[0]["id"], b_id);
    assert_eq!(items[1]["rank"], 2);
    assert_eq!(items[1]["id"], a_id);

    let first = &items[0];
    // 8 个推荐字段（快照日期是**元素级**，不是页级）。
    assert_eq!(first["snapshot_date"], day.format("%Y-%m-%d").to_string());
    assert!(first["generated_at"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    assert_eq!(first["recommendation_score"], 0.9);
    assert_eq!(first["reason_codes"], json!(["popular_movie"]));
    assert_eq!(first["reason_texts"], json!(["近期热度较高"]));
    assert_eq!(first["signal_scores"], json!({"heat": 0.5}));
    assert_eq!(first["is_stale"], false);
    // 页级**没有** snapshot_date。
    assert!(body.get("snapshot_date").is_none());

    // 完整影片卡片（不是「缩过的几个字段」）。
    assert_eq!(first["movie_number"], b_number);
    assert_eq!(first["title"], "每日推荐测试影片");
    assert_eq!(first["can_play"], false);
    assert_eq!(first["media_count"], 0);
    assert_eq!(first["media_items"], json!([]));
    assert_eq!(first["is_blacklisted"], false);
    // 第二部影片也在（同一批）。
    assert_eq!(items[1]["movie_number"], a_number);
}

#[tokio::test]
async fn blacklisted_movies_leave_no_slot_in_pagination() {
    // 拉黑的影片要被 JOIN 过滤掉：`items` 与 `total` **一起**少，而不是
    // 「取回两条再丢掉一条」（那会让 total 偏大、最后一页变短）。
    let db = TestDb::require().await;
    let fixture = Fixture::new("blacklist");
    let token = seed_token(db.pool()).await;

    let (hidden_id, _) = seed_movie(&db).await;
    let (shown_id, shown_number) = seed_movie(&db).await;
    let day = days_ago(0);
    seed_snapshot(&db, &[(hidden_id, 1, 0.9, day), (shown_id, 2, 0.8, day)]).await;
    set_blacklisted(&db, hidden_id, true).await;

    let (status, body) = send(app(&db, &fixture), get("/daily-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "拉黑的那条不占总数");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["movie_number"], shown_number);
    assert_eq!(items[0]["rank"], 2);
}

#[tokio::test]
async fn is_stale_marks_snapshots_before_today() {
    // `is_stale` 是**元素级**：昨天的快照标 stale，今天的标 false。
    // 生成侧每天重写，正常情况下不会出现两天混在一批 —— 但读侧必须能表达它。
    let db = TestDb::require().await;
    let fixture = Fixture::new("stale");
    let token = seed_token(db.pool()).await;

    let (movie_id, _) = seed_movie(&db).await;
    seed_snapshot(&db, &[(movie_id, 1, 0.5, days_ago(1))]).await;

    let (status, body) = send(app(&db, &fixture), get("/daily-recommendations", &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"][0]["is_stale"], true, "昨天的快照应标 stale");
    assert_eq!(
        body["items"][0]["snapshot_date"],
        days_ago(1).format("%Y-%m-%d").to_string()
    );
}

#[tokio::test]
async fn pagination_walks_rank_order() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("page");
    let token = seed_token(db.pool()).await;

    let (m1, _) = seed_movie(&db).await;
    let (m2, _) = seed_movie(&db).await;
    let (m3, _) = seed_movie(&db).await;
    let day = days_ago(0);
    seed_snapshot(
        &db,
        &[(m1, 1, 0.9, day), (m2, 2, 0.7, day), (m3, 3, 0.5, day)],
    )
    .await;

    let (status, first) = send(
        app(&db, &fixture),
        get("/daily-recommendations?page=1&page_size=2", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["total"], 3);
    assert_eq!(first["page"], 1);
    assert_eq!(first["page_size"], 2);
    let first_items = first["items"].as_array().expect("items 是数组");
    assert_eq!(first_items.len(), 2);
    assert_eq!(first_items[0]["rank"], 1);
    assert_eq!(first_items[1]["rank"], 2);

    let (status, second) = send(
        app(&db, &fixture),
        get("/daily-recommendations?page=2&page_size=2", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["page"], 2);
    let second_items = second["items"].as_array().expect("items 是数组");
    assert_eq!(second_items.len(), 1);
    assert_eq!(second_items[0]["rank"], 3);
}

#[tokio::test]
async fn invalid_pagination_is_422_with_the_dedicated_code() {
    // 越界要 422，**不是夹到边界**。错误码是 daily 专用的那个：
    // 客户端要靠它区分「分页参数错了」与别的筛选错误。
    let db = TestDb::require().await;
    let fixture = Fixture::new("badpage");
    let token = seed_token(db.pool()).await;

    for uri in [
        "/daily-recommendations?page=0",
        "/daily-recommendations?page_size=0",
        "/daily-recommendations?page_size=101",
    ] {
        let (status, body) = send(app(&db, &fixture), get(uri, &token)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
        assert_eq!(
            body["error"]["code"], "invalid_daily_recommendation_filter",
            "{uri}: {body}"
        );
    }
}
