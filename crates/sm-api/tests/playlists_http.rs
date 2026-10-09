//! HTTP 层集成测试：**真实 PostgreSQL**，不起端口。
//!
//! 用 `tower::ServiceExt::oneshot` 直接驱动 `Router` —— 比 `TcpListener`
//! 快一个数量级，也不会在 CI 上抢端口。代价是测不到 TCP 层行为（超时、
//! 连接重置），那些不是本层该验证的东西。
//!
//! # 为什么必须连真库
//!
//! 这些断言看着像"路由逻辑"，但每一个都穿过 `service → 仓储 → PostgreSQL`：
//! 名称唯一性撞的是 `playlist.name` 的 unique 索引，404 撞的是
//! `find_by_id` 返回 `None`。用 mock 替掉仓储，测的就成了 mock 自己。

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use tower::ServiceExt;

const SECRET: &str = "integration-secret";

/// 建一个测试库 + 一个真实用户，返回可用于鉴权的 token。
async fn setup() -> (TestDb, String) {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    (db, token)
}

async fn seed_token(db: &Db) -> String {
    let users = UserRepository::new(db.clone());
    let username = format!(
        "u{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let user = users
        .insert(&NewUser {
            username,
            // 仓储只校验非空，不解析 PHC —— 这里不需要真的 Argon2 串
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

fn app(db: &TestDb) -> axum::Router {
    router(AppState::new(db.pool().clone(), AuthConfig::new(SECRET)))
}

fn authed(method: &str, uri: &str, token: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(value) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        builder
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

/// 发一个请求，返回状态码与解析后的 JSON（空体为 `Null`）。
async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "响应体不是 JSON: {err}; 原始内容: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

fn message_of(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or("<missing>")
}

// ------------------------------------------------------------------ 鉴权

#[tokio::test]
async fn missing_authorization_header_is_401_authentication_required() {
    let (db, _token) = setup().await;
    let request = Request::builder()
        .method("POST")
        .uri("/playlists")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(br#"{"name":"x"}"#.to_vec()))
        .unwrap();

    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
    assert_eq!(message_of(&body), "Authentication required");
}

#[tokio::test]
async fn non_bearer_scheme_is_401_authentication_required() {
    let (db, _token) = setup().await;
    let request = Request::builder()
        .method("POST")
        .uri("/playlists")
        .header(header::AUTHORIZATION, "Basic abc")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(br#"{"name":"x"}"#.to_vec()))
        .unwrap();

    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(message_of(&body), "Authentication required");
}

#[tokio::test]
async fn malformed_token_is_401_invalid_access_token() {
    let (db, _token) = setup().await;
    let request = authed(
        "POST",
        "/playlists",
        "not-a-jwt",
        Some(json!({"name": "x"})),
    );

    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
    // 与上一条的差别就在这个 message —— 客户端不靠它分支，但日志靠
    assert_eq!(message_of(&body), "Invalid access token");
}

#[tokio::test]
async fn token_for_a_deleted_user_is_401() {
    let (db, _token) = setup().await;
    // 签名有效，但库里没有这个用户 —— 上游 `get_current_user` 的第 4 步
    let ghost = encode_access_token(999_999, Utc::now() + Duration::hours(1), SECRET);
    let request = authed("POST", "/playlists", &ghost, Some(json!({"name": "x"})));

    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(message_of(&body), "Invalid access token");
}

#[tokio::test]
async fn token_signed_with_another_secret_is_401() {
    let (db, token) = setup().await;
    // 换密钥重建 app，同一个 token 必须失效
    let other = router(AppState::new(
        db.pool().clone(),
        AuthConfig::new("another-secret"),
    ));
    let request = authed("POST", "/playlists", &token, Some(json!({"name": "x"})));

    let (status, body) = send(other, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(message_of(&body), "Invalid access token");
}

// ------------------------------------------------------------------ 业务

#[tokio::test]
async fn create_get_delete_roundtrip() {
    let (db, token) = setup().await;

    let (status, created) = send(
        app(&db),
        authed(
            "POST",
            "/playlists",
            &token,
            Some(json!({"name": "我的列表", "description": "d"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={created}");
    assert_eq!(created["name"], "我的列表");
    assert_eq!(created["kind"], "custom");
    assert_eq!(created["is_system"], false);
    assert_eq!(created["movie_count"], 0);
    let id = created["id"].as_i64().expect("响应必须有 id");

    let (status, fetched) = send(
        app(&db),
        authed("GET", &format!("/playlists/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], id);

    let (status, body) = send(
        app(&db),
        authed("DELETE", &format!("/playlists/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_null(), "204 必须是空响应体，实际 {body}");

    let (status, body) = send(
        app(&db),
        authed("GET", &format!("/playlists/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "playlist_not_found");
    assert_eq!(body["error"]["details"]["playlist_id"], id);
}

#[tokio::test]
async fn duplicate_name_is_409_with_details() {
    let (db, token) = setup().await;
    let payload = Some(json!({"name": "重名"}));

    let (first, _) = send(
        app(&db),
        authed("POST", "/playlists", &token, payload.clone()),
    )
    .await;
    assert_eq!(first, StatusCode::CREATED);

    let (status, body) = send(app(&db), authed("POST", "/playlists", &token, payload)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code_of(&body), "playlist_name_conflict");
    assert_eq!(body["error"]["details"]["name"], "重名");
}

#[tokio::test]
async fn reserved_name_is_409_playlist_reserved_name() {
    let (db, token) = setup().await;
    let (status, body) = send(
        app(&db),
        authed(
            "POST",
            "/playlists",
            &token,
            Some(json!({"name": "最近播放"})),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    // 保留名优先于唯一性检查 —— 客户端据此区分"你不能占"与"已被占"
    assert_eq!(code_of(&body), "playlist_reserved_name");
}

#[tokio::test]
async fn blank_name_is_422_validation_error() {
    let (db, token) = setup().await;
    let (status, body) = send(
        app(&db),
        authed("POST", "/playlists", &token, Some(json!({"name": "   "}))),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}

#[tokio::test]
async fn unknown_playlist_is_404_with_the_entity_detail_key() {
    let (db, token) = setup().await;
    let (status, body) = send(app(&db), authed("GET", "/playlists/424242", &token, None)).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "playlist_not_found");
    assert_eq!(body["error"]["details"]["playlist_id"], 424_242);
}

#[tokio::test]
async fn patch_requires_at_least_one_field() {
    let (db, token) = setup().await;
    let (_, created) = send(
        app(&db),
        authed("POST", "/playlists", &token, Some(json!({"name": "待改"}))),
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let (status, body) = send(
        app(&db),
        authed(
            "PATCH",
            &format!("/playlists/{id}"),
            &token,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");

    // 只改描述不改名字：不能因为撞到自己而失败
    let (status, updated) = send(
        app(&db),
        authed(
            "PATCH",
            &format!("/playlists/{id}"),
            &token,
            Some(json!({"description": "新描述"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={updated}");
    assert_eq!(updated["description"], "新描述");
    assert_eq!(updated["name"], "待改");
}

#[tokio::test]
async fn add_movie_to_an_unknown_playlist_is_404() {
    let (db, token) = setup().await;
    let (status, body) = send(
        app(&db),
        authed("PUT", "/playlists/424242/movies/ABC-123", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "playlist_not_found");
}

// ------------------------------------------------------------------ 兜底

#[tokio::test]
async fn unknown_route_is_404_http_error() {
    let (db, token) = setup().await;
    let (status, body) = send(app(&db), authed("GET", "/nope", &token, None)).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    // 与业务 404 的区别：unmatched 路由走 Starlette 的 HTTPException，
    // 错误码是 http_error 而不是业务码
    assert_eq!(code_of(&body), "http_error");
    assert_eq!(message_of(&body), "Not Found");
}

#[tokio::test]
async fn wrong_method_on_a_known_path_is_405_envelope() {
    let (db, token) = setup().await;
    // /playlists/{id} 只挂了 GET / PATCH / DELETE
    let (status, body) = send(
        app(&db),
        authed("POST", "/playlists/1", &token, Some(json!({}))),
    )
    .await;

    // axum 默认返回 405 + 空 body，那不是信封 —— 这条断言就是为了防止
    // 有人把 MethodRouter::fallback 删掉
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(code_of(&body), "http_error");
    assert_eq!(message_of(&body), "Method Not Allowed");
}

#[tokio::test]
async fn malformed_json_body_is_422_validation_error() {
    let (db, token) = setup().await;
    let request = Request::builder()
        .method("POST")
        .uri("/playlists")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(br#"{"name":"#.to_vec()))
        .unwrap();

    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code_of(&body), "validation_error");
}
