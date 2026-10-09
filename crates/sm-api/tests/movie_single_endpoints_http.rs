//! 单片动作端点的 HTTP 契约测试：**订阅 / 退订 / 相似影片**，**真实 PostgreSQL**。
//!
//! # 三条里最容易写错的
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 订阅 / 退订的响应 | **204 且无 body** | 返回 `Json(())` 会让 204 带 body，非法的响应会被客户端忽略 |
//! | 拉黑的影片 | **不能**订阅 → 409 `movie_is_blacklisted` | 先解除黑名单 |
//! | 有媒体的影片 | **不能**退订 → 409 `movie_subscription_has_media` | 避免把「停止追踪」与「删除本地资源」混成一个动作 |
//! | 相似影片（未启用） | **空列表**，不是 503/404 | 上游的降级语义：Qdrant 不可用只降级相似度信号 |
//!
//! 每个用例的 `TestDb` 是独立 schema。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    MediaLibraryRepository, MovieRepository, NewMedia, NewMediaLibrary, NewMovie, NewUser,
    UserRepository,
};
use sm_db::testing::TestDb;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "movie-single-secret";

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
        let base = std::env::temp_dir().join(format!("sm-msingle-{tag}-{}", unique()));
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

async fn seed_token(db: &TestDb) -> String {
    let user = UserRepository::new(db.pool().clone())
        .insert(&NewUser {
            username: format!("ms{}", unique()),
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

fn request(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

/// 204 的响应体必须是**空的** —— 带 body 的 204 是非法响应。
async fn send_empty(router: axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
        .to_bytes();
    (status, bytes.to_vec())
}

async fn send_json(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let (status, bytes) = send_empty(router, request).await;
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

/// 建一部影片，返回 `(id, 番号)`。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("MS-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "单片端点测试影片".to_owned(),
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
        .expect("建影片失败");
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

async fn is_subscribed(db: &TestDb, movie_id: i32) -> bool {
    sqlx::query_scalar("SELECT is_subscribed FROM movie WHERE id = $1")
        .bind(movie_id)
        .fetch_one(db.pool())
        .await
        .expect("读 is_subscribed")
}

/// 给影片挂一条媒体（用来验「有媒体不许退订」）。
async fn seed_media(db: &TestDb, movie_number: &str) {
    let library_id = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{:06}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("建媒体库失败")
        .id;
    sm_db::repo::MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("m{:06}.mkv", n()),
            file_size_bytes: 1024,
            movie_number: Some(movie_number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("建媒体失败");
}

// ================================================================ 订阅

/// ★ `PUT /movies/{number}/subscription` → **204 且 body 为空**。
#[tokio::test]
async fn subscribing_a_movie_returns_204_with_no_body() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("subscribe");
    let token = seed_token(&db).await;
    let (movie_id, number) = seed_movie(&db).await;

    let (status, body) = send_empty(
        app(&db, &fixture),
        request("PUT", &format!("/movies/{number}/subscription"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty(), "★ 204 不得带 body");
    assert!(is_subscribed(&db, movie_id).await, "订阅状态要真的落库");
}

#[tokio::test]
async fn subscribing_an_unknown_movie_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("subscribe-404");
    let token = seed_token(&db).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("PUT", "/movies/MS-NOPE/subscription", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "movie_not_found");
}

/// ★ 拉黑的影片不能订阅 → 409 `movie_is_blacklisted`。
#[tokio::test]
async fn subscribing_a_blacklisted_movie_is_a_409() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("subscribe-blacklisted");
    let token = seed_token(&db).await;
    let (movie_id, number) = seed_movie(&db).await;
    set_blacklisted(&db, movie_id, true).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("PUT", &format!("/movies/{number}/subscription"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "movie_is_blacklisted");
    assert_eq!(body["error"]["details"]["movie_number"], number);
    assert!(!is_subscribed(&db, movie_id).await, "被拒时不能顺手订阅上");
}

// ================================================================ 退订

/// ★ `DELETE /movies/{number}/subscription` → 204，且状态真的翻了。
#[tokio::test]
async fn unsubscribing_a_movie_without_media_returns_204() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("unsubscribe");
    let token = seed_token(&db).await;
    let (movie_id, number) = seed_movie(&db).await;
    // 先订上，否则「已退订」测不出状态变化。
    send_empty(
        app(&db, &fixture),
        request("PUT", &format!("/movies/{number}/subscription"), &token),
    )
    .await;
    assert!(is_subscribed(&db, movie_id).await);

    let (status, body) = send_empty(
        app(&db, &fixture),
        request("DELETE", &format!("/movies/{number}/subscription"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty(), "★ 204 不得带 body");
    assert!(!is_subscribed(&db, movie_id).await, "退订要真的落库");
}

/// ★ 有本地媒体时**拒绝退订**（409），且 `details.media_count` 是真实计数。
#[tokio::test]
async fn unsubscribing_a_movie_with_media_is_a_409() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("unsubscribe-media");
    let token = seed_token(&db).await;
    let (movie_id, number) = seed_movie(&db).await;
    seed_media(&db, &number).await;
    send_empty(
        app(&db, &fixture),
        request("PUT", &format!("/movies/{number}/subscription"), &token),
    )
    .await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("DELETE", &format!("/movies/{number}/subscription"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "movie_subscription_has_media");
    assert_eq!(body["error"]["details"]["movie_number"], number);
    assert_eq!(body["error"]["details"]["media_count"], 1);
    assert!(
        is_subscribed(&db, movie_id).await,
        "被拒时不能把订阅状态改掉"
    );
}

// ================================================================ 影片详情

/// ★ 详情端点：键集合 + **三条新 SQL 真的能执行**。
///
/// 这条用例最实在的价值是把 `tags_for_movie` / `list_for_movie` /
/// `actor_ids_for_movie` 三条 join 查询跑一遍 —— 它们是为详情页新写的，
/// 字段名写错只会在运行时报 PostgreSQL 错误，编译期完全看不出来。
#[tokio::test]
async fn the_detail_carries_every_sub_resource_it_has_and_not_the_two_it_lacks() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("detail");
    let token = seed_token(&db).await;
    let (_movie_id, number) = seed_movie(&db).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("GET", &format!("/movies/{number}"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    for key in [
        "actors",
        "tags",
        "media_items",
        "media_count",
        "can_play",
        "rankings",
        "playlists",
    ] {
        assert!(body.get(key).is_some(), "缺 {key}：{body}");
    }
    assert_eq!(body["media_count"], 0);
    assert_eq!(body["can_play"], false);

    // ★ 这两项是「本仓还没接」，所以**不带键** —— 给空数组会让客户端以为
    // 「这部片没有剧照」，而真实原因是那一层不存在。
    assert!(body.get("plot_images").is_none(), "不该伪造 plot_images");
    assert!(
        body.get("merge_playback_candidates").is_none(),
        "不该伪造 merge_playback_candidates"
    );
}

#[tokio::test]
async fn the_detail_of_an_unknown_movie_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("detail-404");
    let token = seed_token(&db).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("GET", "/movies/MS-NOPE", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "movie_not_found");
}

// ================================================================ 相似影片

/// ★ 没配置影片相似度时返回**空列表**（不是 503、不是 404）。
///
/// 那是上游的降级语义：`qdrant.url` 没配 / `movie_similarity.enabled` 为假，
/// 相似度就只是一个「给不出理由」的可选项，不该让端点报错。
#[tokio::test]
async fn similar_movies_are_an_empty_list_when_similarity_is_not_configured() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("similar-off");
    let token = seed_token(&db).await;
    let (_movie_id, number) = seed_movie(&db).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("GET", &format!("/movies/{number}/similar"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Array(vec![]));
}

/// `limit=0` 合法（上游 `ge=0`），返回空列表而**不是** 422。
///
/// 写成 `ge=1` 的后果：客户端想「不要相似影片」时会收到 422 —— 那是改变契约。
#[tokio::test]
async fn a_zero_limit_is_accepted_and_returns_an_empty_list() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("similar-zero");
    let token = seed_token(&db).await;
    let (_movie_id, number) = seed_movie(&db).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("GET", &format!("/movies/{number}/similar?limit=0"), &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Array(vec![]));
}

/// 番号不存在 → 404（相似度没启用也一样：番号校验在前面）。
#[tokio::test]
async fn similar_movies_of_an_unknown_movie_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("similar-404");
    let token = seed_token(&db).await;

    let (status, body) = send_json(
        app(&db, &fixture),
        request("GET", "/movies/MS-NOPE/similar", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "movie_not_found");
}
