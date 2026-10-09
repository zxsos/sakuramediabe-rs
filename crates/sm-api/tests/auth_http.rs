//! 登录与刷新的 HTTP 集成测试（真实 PostgreSQL）。
//!
//! # 为什么这几个用例值得单独一个文件
//!
//! 令牌轮换是**唯一**一处「读起来对、写错了要很久才发现」的逻辑：
//! 分两次提交会在中间留下「旧已吊销、新未插入」的窗口，用户被彻底登出且
//! 不能自愈。所以这里除了覆盖错误码，还专门验了**旧令牌必须立即失效**。

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_core::password::hash_password_with;
use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use tower::ServiceExt;

const SECRET: &str = "auth-secret";
const PASSWORD: &str = "correct horse battery staple";

/// 建库 + 建一个密码已知的用户，返回其用户名。
async fn setup() -> (TestDb, String) {
    let db = TestDb::require().await;
    let username = seed_user(db.pool()).await;
    (db, username)
}

async fn seed_user(db: &Db) -> String {
    let users = UserRepository::new(db.clone());
    // 测试用低参数，避免每个用例都吃 19 MiB
    let hash = hash_password_with(PASSWORD, 64, 1, 1).expect("哈希失败");
    let username = format!(
        "user{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    users
        .insert(&NewUser {
            username: username.clone(),
            password_hash: hash,
        })
        .await
        .expect("插入用户失败");
    username
}

fn app(db: &TestDb) -> axum::Router {
    router(AppState::new(db.pool().clone(), AuthConfig::new(SECRET)))
}

async fn post(
    router: axum::Router,
    uri: &str,
    body: Value,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "响应体不是 JSON: {err}; {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

// ---------------------------------------------------------------- 登录

#[tokio::test]
async fn login_returns_a_full_token_resource() {
    let (db, username) = setup().await;
    let (status, body) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": PASSWORD}),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body={body}");
    assert!(!body["access_token"].as_str().unwrap_or_default().is_empty());
    assert!(!body["refresh_token"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["user"]["username"], username.as_str());
    // 上游算的是配置窗口，不是实际剩余秒数
    assert_eq!(body["expires_in"], 60 * 24 * 30 * 60);
    assert!(body["expires_at"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn wrong_password_and_unknown_user_share_one_error_code() {
    let (db, username) = setup().await;

    let (status, body) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": "wrong"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "invalid_credentials");

    let (status, body) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": "no-such-user", "password": PASSWORD}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // 区分二者等于泄露哪些用户名存在
    assert_eq!(code_of(&body), "invalid_credentials");
}

#[tokio::test]
async fn login_does_not_leak_the_password_hash() {
    let (db, username) = setup().await;
    let (_, body) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": PASSWORD}),
        None,
    )
    .await;

    let rendered = body.to_string();
    assert!(
        !rendered.contains("argon2"),
        "响应体绝不能带哈希: {rendered}"
    );
    assert!(!rendered.contains(PASSWORD), "响应体绝不能带明文密码");
}

// ---------------------------------------------------------------- 刷新

#[tokio::test]
async fn refresh_rotates_and_the_old_token_stops_working() {
    let (db, username) = setup().await;
    let (_, first) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": PASSWORD}),
        None,
    )
    .await;

    let access = first["access_token"].as_str().unwrap().to_owned();
    let old_refresh = first["refresh_token"].as_str().unwrap().to_owned();

    let (status, second) = post(
        app(&db),
        "/auth/token-refreshes",
        json!({"refresh_token": old_refresh.clone()}),
        Some(&access),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={second}");

    let new_refresh = second["refresh_token"].as_str().unwrap();
    assert_ne!(new_refresh, old_refresh, "刷新必须换发新令牌");

    // 重放旧令牌必须被拒 —— 这是轮换的全部意义
    let (status, body) = post(
        app(&db),
        "/auth/token-refreshes",
        json!({"refresh_token": old_refresh}),
        Some(&access),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "invalid_refresh_token");
}

#[tokio::test]
async fn refresh_requires_an_access_token() {
    let (db, username) = setup().await;
    let (_, first) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": PASSWORD}),
        None,
    )
    .await;
    let refresh = first["refresh_token"].as_str().unwrap();

    // 上游 /auth/token-refreshes 挂了 get_current_user，不带 token 就是 401
    let (status, body) = post(
        app(&db),
        "/auth/token-refreshes",
        json!({"refresh_token": refresh}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
}

#[tokio::test]
async fn an_unknown_refresh_token_is_rejected() {
    let (db, username) = setup().await;
    let (_, first) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": PASSWORD}),
        None,
    )
    .await;
    let access = first["access_token"].as_str().unwrap();

    let (status, body) = post(
        app(&db),
        "/auth/token-refreshes",
        json!({"refresh_token": "not-a-real-token"}),
        Some(access),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "invalid_refresh_token");
}

#[tokio::test]
async fn a_forged_access_token_does_not_open_the_refresh_endpoint() {
    let (db, _username) = setup().await;
    // 自签一个"格式正确但库里没有对应用户"的 token
    let ghost = encode_access_token(
        999_999,
        chrono::Utc::now() + chrono::Duration::hours(1),
        SECRET,
    );
    let (status, body) = post(
        app(&db),
        "/auth/token-refreshes",
        json!({"refresh_token": "whatever"}),
        Some(&ghost),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
}

#[tokio::test]
async fn the_login_route_is_reachable_without_any_credentials() {
    // 反例保护：如果有人给 /auth/tokens 也挂上 CurrentUser，登录就死锁了
    let (db, username) = setup().await;
    let (status, _) = post(
        app(&db),
        "/auth/tokens",
        json!({"username": username, "password": PASSWORD}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}
