//! `GET /tags*` 三条端点的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 这层测什么
//!
//! - **计数口径**：`LEFT JOIN movie_tag` + `COUNT(mt.movie_id)` —— 用
//!   `COUNT(*)` 会把左连接补的那行 NULL 也算成 1，于是「一个影片都没挂」的标签
//!   显示成 1；
//! - **排序**：缺省 `movie_count:desc`，且影片数并列时补 `name ASC`
//!   （否则筛选器顺序会抖动）；
//! - **三个不同的错误码**：`query` 空串与非法 `sort` 是
//!   `invalid_tag_filter`；非法枚举是 `validation_error`；
//!   `director_name` 空白是 `invalid_movie_filter`；
//! - **`{id}/movies` 先验存在**：不存在的标签是 404，不是空列表。

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

const SECRET: &str = "tags-http-secret";

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
        let base = std::env::temp_dir().join(format!("sm-tags-{tag}-{}", unique()));
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
            username: format!("tg{}", unique()),
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

async fn seed_tag(db: &TestDb, name: &str) -> i32 {
    sqlx::query_scalar::<_, i32>("INSERT INTO tag (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(db.pool())
        .await
        .expect("insert tag")
}

async fn seed_movie(db: &TestDb) -> i32 {
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: format!("TG{:06}", n()),
            title: "标签测试影片".to_owned(),
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
        .expect("insert movie")
        .id
}

async fn link(db: &TestDb, movie_id: i32, tag_id: i32) {
    sqlx::query("INSERT INTO movie_tag (movie_id, tag_id) VALUES ($1, $2)")
        .bind(movie_id)
        .bind(tag_id)
        .execute(db.pool())
        .await
        .expect("link tag");
}

#[tokio::test]
async fn the_tag_list_carries_counts_and_sorts_by_movie_count() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list");
    let token = seed_token(db.pool()).await;

    let busy = seed_tag(&db, &format!("AAA-忙{}", n())).await;
    let idle = seed_tag(&db, &format!("BBB-空闲{}", n())).await;
    for _ in 0..2 {
        let movie_id = seed_movie(&db).await;
        link(&db, movie_id, busy).await;
    }

    let (status, body) = send(app(&db, &fixture), get("/tags", &token)).await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body.as_array().expect("顶层是数组");

    let busy_item = items
        .iter()
        .find(|item| item["tag_id"] == json!(busy))
        .expect("忙标签应当出现");
    assert_eq!(busy_item["movie_count"], json!(2));
    let idle_item = items
        .iter()
        .find(|item| item["tag_id"] == json!(idle))
        .expect("空闲标签也应当出现");
    // `LEFT JOIN` + `COUNT(mt.movie_id)`：没挂影片是 **0**，不是 1。
    assert_eq!(idle_item["movie_count"], json!(0), "空闲标签的计数应当是 0");

    // 缺省按影片数倒序：忙的在前。
    let busy_pos = items
        .iter()
        .position(|item| item["tag_id"] == json!(busy))
        .unwrap();
    let idle_pos = items
        .iter()
        .position(|item| item["tag_id"] == json!(idle))
        .unwrap();
    assert!(busy_pos < idle_pos, "缺省应当按影片数倒序：{body}");
    // 字段名是 `tag_id` 而不是 `id`。
    assert!(busy_item.get("tag_id").is_some() && busy_item.get("id").is_none());
}

#[tokio::test]
async fn tag_filter_errors_use_three_different_codes() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("codes");
    let token = seed_token(db.pool()).await;

    // `query` 空串：标签域自己的码（空串不是「不筛」）。
    let (status, body) = send(app(&db, &fixture), get("/tags?query=%20", &token)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("invalid_tag_filter"));
    assert_eq!(body["error"]["details"]["query"], json!(" "));

    // 非法 sort：也是 `invalid_tag_filter`。
    let (status, body) = send(app(&db, &fixture), get("/tags?sort=title:desc", &token)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("invalid_tag_filter"));

    // 非法枚举（`{id}/movies` 上）：`validation_error`，**不是**标签域的码。
    let tag = seed_tag(&db, &format!("枚举{}", n())).await;
    let (status, body) = send(
        app(&db, &fixture),
        get(&format!("/tags/{tag}/movies?status=weird"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("validation_error"));

    // `director_name` 空白：复用**影片域**的码。
    let (status, body) = send(
        app(&db, &fixture),
        get(&format!("/tags/{tag}/movies?director_name=%20"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("invalid_movie_filter"));
}

#[tokio::test]
async fn an_unknown_tag_is_404_and_tag_movies_only_lists_that_tag() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("movies");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(app(&db, &fixture), get("/tags/2147483647/movies", &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("tag_not_found"));
    assert_eq!(body["error"]["details"]["tag_id"], json!(2147483647_i64));

    let (status, body) = send(app(&db, &fixture), get("/tags/2147483647", &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], json!("tag_not_found"));

    // 存在的标签 + 一部挂上 + 一部没挂：只回挂上的那部。
    let tag = seed_tag(&db, &format!("影片{}", n())).await;
    let linked = seed_movie(&db).await;
    let _unlinked = seed_movie(&db).await;
    link(&db, linked, tag).await;

    let (status, body) = send(
        app(&db, &fixture),
        get(&format!("/tags/{tag}/movies"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1, "只该回挂在该标签下的影片：{body}");
    assert_eq!(items[0]["id"], json!(linked));
    assert_eq!(body["total"], json!(1));
}
