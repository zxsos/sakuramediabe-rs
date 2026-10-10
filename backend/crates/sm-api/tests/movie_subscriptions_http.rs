//! `/movie-subscriptions` 的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! 目前只有 `POST /movie-subscriptions/search-resets` 一条。它看着简单，但
//! 「传空数组」与「省略」**等价**、而「完全不带 body」也是合法请求 —— 三态
//! 都得钉住，否则客户端会拿到与预期不同的重开范围。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{MovieRepository, NewMovie, NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "movie-subs-secret";

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
        let base = std::env::temp_dir().join(format!("sm-msubs-{tag}-{}", unique()));
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
            username: format!("msr{}", unique()),
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

/// 无请求体的 GET。
fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

/// 带 body 的 POST。
fn post(uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).expect("序列化")))
        .expect("构造请求")
}

/// **完全不带 body** 的 POST（上游允许的可选请求体）。
fn post_empty(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
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

/// 造一部**已订阅**的影片，返回 id。
async fn seed_subscribed_movie(db: &TestDb) -> i32 {
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: format!("MSR{:06}", n()),
            title: "订阅台账影片".to_owned(),
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
    sqlx::query(
        "UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW(), \
         subscription_search_state = $2, subscription_search_attempt_count = 3 WHERE id = $1",
    )
    .bind(movie.id)
    .bind("failed_retryable")
    .execute(db.pool())
    .await
    .expect("预置订阅态");
    movie.id
}

async fn state_of(db: &TestDb, movie_id: i32) -> (String, i32, i32) {
    sqlx::query_as::<_, (String, i32, i32)>(
        "SELECT subscription_search_state, subscription_search_attempt_count, \
         subscription_search_retry_round FROM movie WHERE id = $1",
    )
    .bind(movie_id)
    .fetch_one(db.pool())
    .await
    .expect("读检索状态")
}

#[tokio::test]
async fn resetting_with_ids_only_reopens_those_movies() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("ids");
    let token = seed_token(db.pool()).await;
    let target = seed_subscribed_movie(&db).await;
    let untouched = seed_subscribed_movie(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        post(
            "/movie-subscriptions/search-resets",
            &token,
            json!({ "movie_ids": [target] }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["reset_count"], json!(1));

    let (state, attempts, retry_round) = state_of(&db, target).await;
    assert_eq!(state, "pending");
    assert_eq!(attempts, 0);
    assert_eq!(retry_round, 1, "retry_round 是加一而不是清零");

    // 没被点名的影片原样不动。
    let (other_state, other_attempts, _) = state_of(&db, untouched).await;
    assert_eq!(other_state, "failed_retryable");
    assert_eq!(other_attempts, 3);
}

#[tokio::test]
async fn resetting_without_ids_and_with_an_empty_body_reopens_exhausted_only() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("all");
    let token = seed_token(db.pool()).await;

    let exhausted = seed_subscribed_movie(&db).await;
    let retryable = seed_subscribed_movie(&db).await;
    sqlx::query("UPDATE movie SET subscription_search_state = 'exhausted' WHERE id = $1")
        .bind(exhausted)
        .execute(db.pool())
        .await
        .expect("设为已放弃");

    // ① 显式空数组 —— 上游 `if movie_ids:` 对空列表为假，所以与省略**等价**。
    let (status, body) = send(
        app(&db, &fixture),
        post(
            "/movie-subscriptions/search-resets",
            &token,
            json!({ "movie_ids": [] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["reset_count"], json!(1), "只该重开已放弃的那一部");

    let (state, _, _) = state_of(&db, exhausted).await;
    assert_eq!(state, "pending");
    let (other_state, _, _) = state_of(&db, retryable).await;
    assert_eq!(
        other_state, "failed_retryable",
        "可重试的不该被「全部重开」碰到"
    );

    // ② 完全不发 body —— 上游允许（`payload: ... | None = None`）。
    //    注意此时 `exhausted` 那部已经在 ① 里变成 `pending`，所以这一次只有一个
    //    待重开的 —— 断言必须跟着这个事实，而不是「总共两部」。
    sqlx::query("UPDATE movie SET subscription_search_state = 'exhausted' WHERE id = $1")
        .bind(retryable)
        .execute(db.pool())
        .await
        .expect("设为已放弃");
    let (status, body) = send(
        app(&db, &fixture),
        post_empty("/movie-subscriptions/search-resets", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "无 body 应当合法，响应: {body}");
    assert_eq!(body["reset_count"], json!(1), "只有刚设为已放弃的那一部");

    // ③ 非法 JSON 仍然是 422 `validation_error`（与 Json 提取器的拒绝同形状）。
    let broken = Request::builder()
        .method("POST")
        .uri("/movie-subscriptions/search-resets")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{ not json"))
        .expect("构造请求");
    let (status, body) = send(app(&db, &fixture), broken).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));
}

// ================================================================ 状态计数

/// 造一部已订阅影片并**直接**设定检索状态字段（绕过订阅流程），返回 id。
async fn seed_subscription_state(
    db: &TestDb,
    state: &str,
    error_code: Option<&str>,
    attempted: bool,
) -> i32 {
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: format!("MSC{:06}", n()),
            title: "状态计数影片".to_owned(),
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
    sqlx::query(
        "UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW(), \
         subscription_search_state = $2, subscription_search_error_code = $3, \
         subscription_search_last_attempted_at = CASE WHEN $4 THEN NOW() ELSE NULL END \
         WHERE id = $1",
    )
    .bind(movie.id)
    .bind(state)
    .bind(error_code.map(str::to_owned))
    .bind(attempted)
    .execute(db.pool())
    .await
    .expect("设定检索状态");
    movie.id
}

#[tokio::test]
async fn status_counts_partition_every_subscribed_movie() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("counts");
    let token = seed_token(db.pool()).await;

    // 四种「没有媒体、没有下载任务」的形状 —— 分类全靠检索状态字段。
    seed_subscription_state(&db, "pending", None, false).await;
    // `failed_retryable` 且 error_code 为空 → **failed**（不是 missing）。
    seed_subscription_state(&db, "failed_retryable", None, false).await;
    seed_subscription_state(&db, "exhausted", None, false).await;
    // `no_candidate_found` 不算失败，且查过 → **missing**。
    seed_subscription_state(&db, "failed_retryable", Some("no_candidate_found"), true).await;

    let (status, body) = send(
        app(&db, &fixture),
        get("/movie-subscriptions/status-counts", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["pending"], json!(1));
    assert_eq!(body["failed"], json!(1));
    assert_eq!(body["exhausted"], json!(1));
    assert_eq!(body["missing"], json!(1));
    assert_eq!(body["imported"], json!(0), "没有 media 就不算已入库");
    assert_eq!(body["downloading"], json!(0));
    assert_eq!(body["import_failed"], json!(0));

    // `CASE` 的分支互斥且必有兜底，所以各状态之和**恒等于**订阅总数。
    let sum = body["imported"].as_i64().unwrap()
        + body["downloading"].as_i64().unwrap()
        + body["import_failed"].as_i64().unwrap()
        + body["pending"].as_i64().unwrap()
        + body["missing"].as_i64().unwrap()
        + body["exhausted"].as_i64().unwrap()
        + body["failed"].as_i64().unwrap();
    assert_eq!(body["total"], json!(sum));
    assert_eq!(body["total"], json!(4));
}

// ================================================================ GET /movie-subscriptions

#[tokio::test]
async fn the_subscription_list_filters_sorts_and_validates_paging() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list");
    let token = seed_token(db.pool()).await;

    let pending = seed_subscription_state(&db, "pending", None, false).await;
    seed_subscription_state(&db, "exhausted", None, false).await;

    let (status, body) = send(app(&db, &fixture), get("/movie-subscriptions", &token)).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 2, "只列已订阅的：{body}");
    assert_eq!(body["total"], json!(2));

    let first = items
        .iter()
        .find(|item| item["movie_id"] == json!(pending))
        .expect("pending 那部应当出现");
    assert_eq!(first["status"], json!("pending"));
    assert_eq!(first["media_count"], json!(0), "没有媒体");
    assert_eq!(first["dead_download_task_count"], json!(0));
    assert_eq!(first["import_status"], json!(null));
    // computed 字段：导入状态为空时说明也是 null，不是空串。
    assert_eq!(first["import_status_label"], json!(null));
    assert!(first["movie_number"].is_string() && first["title"].is_string());

    // 按状态筛。
    let (status, body) = send(
        app(&db, &fixture),
        get("/movie-subscriptions?status=pending", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["status"], json!("pending"));

    // 检索词（片名子串）。
    let (status, body) = send(
        app(&db, &fixture),
        get("/movie-subscriptions?search=%E7%8A%B6%E6%80%81", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(2), "片名都含「状态」：{body}");

    // **这个端点的分页是校验过的**（与 GET /movies 的裸 int 不同）。
    for uri in [
        "/movie-subscriptions?page=0",
        "/movie-subscriptions?page_size=0",
        "/movie-subscriptions?page_size=101",
    ] {
        let (status, body) = send(app(&db, &fixture), get(uri, &token)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{uri} 应当是 422，响应: {body}"
        );
        assert_eq!(
            body["error"]["code"],
            json!("invalid_movie_subscription_filter"),
            "{uri}"
        );
    }
}
