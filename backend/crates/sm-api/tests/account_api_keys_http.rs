//! `/account/api-keys` 的 HTTP 集成测试（真实 PostgreSQL）。
//!
//! # 为什么这几个用例值得单独一个文件
//!
//! 这是**唯一**一个「明文只出现一次」的契约。断言全落在两件事上：
//!
//! 1. **明文永不回读** —— 列表响应里不能有 `key`，库里存的也不是明文。
//! 2. **删除即吊销** —— 删掉之后同一个 Bearer 立刻变 401。少了这一条，
//!    「删除」可能只是删了一行展示数据，而鉴权那条路还在放行。
//!
//! 用例形态对齐上游 `tests/api/test_api_key_api.py`。

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::password::hash_password_with;
use sm_db::repo::{ApiKeyRepository, NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_service::system::auth::AuthConfig;
use tower::ServiceExt;

const SECRET: &str = "api-key-secret";
const PASSWORD: &str = "correct horse battery staple";

/// 建库 + 建一个密码已知的用户。
async fn setup() -> (TestDb, String) {
    let db = TestDb::require().await;
    let users = UserRepository::new(db.pool().clone());
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
    (db, username)
}

fn app(db: &TestDb) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        sm_service::system::ConfigService::new(temp_config_path()),
    ))
}

/// 不碰配置的用例也需要一个 `ConfigService`；给它一个临时路径，避免写真实的
/// `config.toml`。（与 `auth_http.rs` 同款理由。）
fn temp_config_path() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("sm-api-apikey-{}-{n}.toml", std::process::id()))
}

/// 发一个请求，返回（状态码，JSON 体）。空体 → `Value::Null`。
async fn send(
    router: axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let payload = match body {
        Some(value) => Body::from(serde_json::to_vec(&value).unwrap()),
        None => Body::empty(),
    };
    let response = router
        .oneshot(builder.body(payload).unwrap())
        .await
        .expect("oneshot 失败");
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

/// 登录拿 access token。
async fn login(db: &TestDb, username: &str) -> String {
    let (status, body) = send(
        app(db),
        Method::POST,
        "/auth/tokens",
        Some(json!({"username": username, "password": PASSWORD})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "登录失败: {body}");
    body["access_token"].as_str().unwrap().to_owned()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

// ---------------------------------------------------------------- 生成 / 列表 / 删除

#[tokio::test]
async fn create_list_and_delete_round_trip() {
    let (db, username) = setup().await;
    let token = login(&db, &username).await;

    let (status, created) = send(
        app(&db),
        Method::POST,
        "/account/api-keys",
        Some(json!({"name": "MCP server"})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={created}");
    assert_eq!(created["name"], "MCP server");
    let plain = created["key"].as_str().expect("必须回明文 key");
    assert!(plain.starts_with("sk-"), "明文必须带 sk- 前缀: {plain}");
    // 上游断言 `body["key_hint"] == body["key"][:11]`。
    assert_eq!(created["key_hint"], &plain[..11]);
    assert_eq!(created["last_used_at"], Value::Null, "从未使用过应为 null");
    assert!(created["created_at"].as_str().unwrap().contains('T'));
    let key_id = created["id"].as_i64().unwrap();

    // 列表：裸数组，且**不带**明文与哈希。
    let (status, listed) = send(
        app(&db),
        Method::GET,
        "/account/api-keys",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={listed}");
    let items = listed.as_array().expect("必须是顶层 JSON 数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_i64().unwrap(), key_id);
    assert!(items[0].get("key").is_none(), "列表不能回明文: {listed}");
    assert!(items[0].get("key_hash").is_none(), "列表不能回哈希: {listed}");

    // 库里只存哈希，且不是明文。
    let stored = ApiKeyRepository::new(db.pool().clone())
        .find_by_hash(&sm_core::api_key::hash_key(plain))
        .await
        .expect("查库失败")
        .expect("按哈希应能查到");
    assert_eq!(stored.id as i64, key_id);
    assert_ne!(stored.key_hash, plain, "落库的不能是明文");
    assert_eq!(stored.key_hash.len(), 64, "sha256 hex");

    // 删除 → 204；再删 → 404 api_key_not_found。
    let (status, _) = send(
        app(&db),
        Method::DELETE,
        &format!("/account/api-keys/{key_id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, listed) = send(
        app(&db),
        Method::GET,
        "/account/api-keys",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed, json!([]));

    let (status, body) = send(
        app(&db),
        Method::DELETE,
        &format!("/account/api-keys/{key_id}"),
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "api_key_not_found");
}

/// `{}`（不带 `name`）也是合法请求 —— 上游 `Field(default="")`。
#[tokio::test]
async fn create_without_a_name_is_allowed() {
    let (db, username) = setup().await;
    let token = login(&db, &username).await;

    let (status, created) = send(
        app(&db),
        Method::POST,
        "/account/api-keys",
        Some(json!({})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={created}");
    assert_eq!(created["name"], "");
}

/// 备注名超过列宽（64）→ 422，而不是让 `varchar(64)` 在 INSERT 时炸成 500。
#[tokio::test]
async fn an_over_long_name_is_a_validation_error() {
    let (db, username) = setup().await;
    let token = login(&db, &username).await;

    let (status, body) = send(
        app(&db),
        Method::POST,
        "/account/api-keys",
        Some(json!({"name": "x".repeat(65)})),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(code_of(&body), "validation_error");
    assert_eq!(body["error"]["details"]["name"], "长度不能超过 64 个字符");
}

// ---------------------------------------------------------------- 用密钥鉴权

#[tokio::test]
async fn a_generated_key_authenticates_protected_endpoints() {
    let (db, username) = setup().await;
    let token = login(&db, &username).await;

    let (_, created) = send(
        app(&db),
        Method::POST,
        "/account/api-keys",
        Some(json!({"name": "集成"})),
        Some(&token),
    )
    .await;
    let plain = created["key"].as_str().unwrap().to_owned();
    let key_id = created["id"].as_i64().unwrap();

    let (status, account) = send(app(&db), Method::GET, "/account", None, Some(&plain)).await;
    assert_eq!(status, StatusCode::OK, "API key 应能过鉴权: {account}");
    assert_eq!(account["username"], username.as_str());

    // 首次使用会写入 last_used_at。
    let listed = ApiKeyRepository::new(db.pool().clone())
        .list_all()
        .await
        .expect("查库失败");
    let row = listed.iter().find(|k| k.id as i64 == key_id).expect("应存在");
    assert!(row.last_used_at.is_some(), "使用后应记录 last_used_at");
}

#[tokio::test]
async fn a_deleted_key_stops_working() {
    let (db, username) = setup().await;
    let token = login(&db, &username).await;

    let (_, created) = send(
        app(&db),
        Method::POST,
        "/account/api-keys",
        Some(json!({})),
        Some(&token),
    )
    .await;
    let plain = created["key"].as_str().unwrap().to_owned();
    let key_id = created["id"].as_i64().unwrap();

    let (status, _) = send(app(&db), Method::GET, "/account", None, Some(&plain)).await;
    assert_eq!(status, StatusCode::OK);

    send(
        app(&db),
        Method::DELETE,
        &format!("/account/api-keys/{key_id}"),
        None,
        Some(&token),
    )
    .await;

    let (status, body) = send(app(&db), Method::GET, "/account", None, Some(&plain)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
}

#[tokio::test]
async fn an_unknown_key_is_rejected() {
    let (db, _username) = setup().await;

    let (status, body) = send(
        app(&db),
        Method::GET,
        "/account",
        None,
        Some("sk-not-a-real-key"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
}

// ---------------------------------------------------------------- 鉴权是必需的

/// 反例保护：管理端点必须要求登录（上游 `get_current_user`）。
/// 三个方法逐个验 —— 漏挂一个就是「任何人可生成/删除密钥」。
#[tokio::test]
async fn managing_keys_requires_login() {
    let (db, _username) = setup().await;

    let (status, body) = send(app(&db), Method::GET, "/account/api-keys", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body={body}");

    let (status, body) = send(
        app(&db),
        Method::POST,
        "/account/api-keys",
        Some(json!({})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body={body}");

    let (status, body) = send(app(&db), Method::DELETE, "/account/api-keys/1", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body={body}");
}
