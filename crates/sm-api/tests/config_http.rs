//! `GET /config` 与 `PATCH /config` 的 HTTP 层测试。
//!
//! 覆盖三类只有走 HTTP 才暴露的问题：
//!
//! 1. **鉴权**：两个端点都要 JWT（上游 `config.py:19`/`:24` 都声明了
//!    `current_user`），未登录必须是 401 而不是「碰巧能调」；
//! 2. **405 走信封**：`POST /config` 路径命中但方法不匹配，axum 默认会给
//!    405 + 空响应体；
//! 3. **响应形状**：`values` 的键集、只读键被剔除、`restart_required` 恒非空。

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use sm_api::AppState;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt as _;

const SECRET: &str = "config-http-secret";

/// 每个用例一个临时配置文件，`ConfigService` 指向它。
fn service(tag: &str) -> (ConfigService, std::path::PathBuf) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sm-api-config-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建目录");
    let path = dir.join("config.toml");
    (ConfigService::new(&path), path)
}

/// 构造一个带真实 PG 的 router。**必须**有库：state 里装着连接池。
///
/// 走真实数据库而不是假 state：这些用例要断言的是「路由 + service + 文件」
/// 这条链，而 `AppState` 的构造签名把连接池变成了必经参数。
/// 返回 router、配置服务、一个已登录用户的 token，以及**必须活到用例结束**的
/// `TestDb`。
///
/// # 为什么 `TestDb` 必须在返回值里
///
/// 它的 `Drop` 会清掉测试 schema（`testing/db.rs:173`，理由是每次跑都留废弃
/// schema 会把库弄脏）。而 router 持有的是 `PgPool` 的克隆 —— 池还活着，
/// schema 没了，于是 `CurrentUser` 查 `users` 表得到
/// 「relation "users" does not exist」，表现为 **500**。
///
/// 这个症状离原因很远，所以让每个用例都显式持有 `TestDb`（哪怕写成
/// `let _db = ...`）比在 helper 里悄悄 `mem::forget` 它要好：后者会让
/// schema 永久残留。
async fn router(tag: &str) -> (axum::Router, ConfigService, String, sm_db::testing::TestDb) {
    let db = sm_db::testing::TestDb::require().await;
    let (user_id, _username) = seed_user(db.pool()).await;
    let (config, _path) = service(tag);
    let state = AppState::new(db.pool().clone(), AuthConfig::new(SECRET), config.clone());
    (sm_api::router(state), config, token(user_id), db)
}

/// 一个已存在于库里的用户（`CurrentUser` 提取器会查库，所以要有行）。
async fn seed_user(db: &sm_db::Db) -> (i32, String) {
    use sm_db::repo::{NewUser, UserRepository};
    let username = format!(
        "cfg{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let hash = sm_core::password::hash_password_with("pw", 64, 1, 1).expect("哈希");
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: username.clone(),
            password_hash: hash,
        })
        .await
        .expect("插入用户");
    (user.id, username)
}

/// 造一个已登录用户的 access token。
///
/// 直接签发而**不**走 `POST /auth/tokens`：这些用例要验的是 `/config` 的
/// 鉴权是否生效，不是登录端点。后者另有 `auth_http.rs` 覆盖，而多走一层
/// 只会让「`/config` 返 401」的原因可能是登录失败而不是鉴权失败。
fn token(user_id: i32) -> String {
    sm_core::jwt::encode_access_token(
        // i64：JWT 的 `sub` 是字符串化的用户 id。
        i64::from(user_id),
        // 1 天后过期：测试不会跑那么久。
        chrono::Utc::now() + chrono::Duration::days(1),
        SECRET,
    )
}

async fn get(router: &axum::Router, token: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::get("/config")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("GET /config");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).expect("响应是 JSON"))
}

/// PATCH 并在失败时把响应体打进断言消息。
///
/// 500 而不是 422 时，只看状态码是查不出原因的 —— 错误体里才有
/// `programmer_error` 的具体文本。
async fn patch(router: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::patch("/config")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("PATCH /config");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).expect("响应是 JSON"))
}

#[tokio::test]
async fn both_endpoints_require_a_token() {
    let (app, _config, _token, _db) = router("auth").await;
    for method in [Method::GET, Method::PATCH] {
        let is_patch = method == Method::PATCH;
        let mut request = Request::builder().method(method.clone()).uri("/config");
        if is_patch {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from("{}")).unwrap())
            .await
            .expect("请求");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} /config 未登录必须是 401"
        );
    }
}

#[tokio::test]
async fn get_returns_the_public_snapshot_without_readonly_keys() {
    let (app, _config, token, _db) = router("get").await;
    let (status, body) = get(&app, &token).await;
    assert_eq!(status, StatusCode::OK);

    let values = &body["values"];
    for key in ["auth", "enable_docs", "plugins"] {
        assert!(
            values.get(key).is_none(),
            "{key} 是只读键，不该出现在 GET 响应里"
        );
    }
    for key in [
        "database",
        "scheduler",
        "logging",
        "qdrant",
        "media",
        "downloads",
    ] {
        assert!(values.get(key).is_some(), "缺少可改节 {key}");
    }
    assert_eq!(
        values["scheduler"]["media_thumbnail_cron"],
        json!("*/30 * * * *"),
        "默认值来自 sm_core::config_schema 的模式表"
    );
}

#[tokio::test]
async fn patch_returns_the_written_snapshot_and_always_demands_a_restart() {
    let (app, _config, token, _db) = router("patch").await;
    let (status, body) = patch(&app, &token, json!({"logging": {"level": "DEBUG"}})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["values"]["logging"]["level"], json!("DEBUG"));
    assert_eq!(
        body["restart_required"],
        json!(["api", "aps"]),
        "restart_required 恒为 [\"api\", \"aps\"]，永不为空"
    );
    assert!(
        body["values"].get("auth").is_none(),
        "PATCH 的响应同样要剔除只读键"
    );
}

#[tokio::test]
async fn a_readonly_key_is_rejected_with_its_own_code() {
    let (app, _config, token, _db) = router("readonly").await;
    let (status, body) = patch(&app, &token, json!({"auth": {"username": "x"}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(body["error"]["code"], json!("readonly_config_key"));
    assert_eq!(body["error"]["details"]["field"], json!("auth"));
}

#[tokio::test]
async fn an_unknown_key_is_rejected_with_the_pointed_path() {
    let (app, _config, token, _db) = router("unknown").await;

    let (status, body) = patch(&app, &token, json!({"scheduer": {}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], json!("unknown_config_field"));
    assert_eq!(body["error"]["details"]["field"], json!("scheduer"));

    // 子键的回显是点分路径，客户端据此高亮对应控件。
    let (_, body) = patch(&app, &token, json!({"scheduler": {"enabld": true}})).await;
    assert_eq!(body["error"]["details"]["field"], json!("scheduler.enabld"));
}

#[tokio::test]
async fn a_non_object_body_is_a_validation_error_not_an_unknown_field() {
    let (app, _config, token, _db) = router("nonobject").await;
    for bad in [json!([1, 2, 3]), json!("nope"), json!(7)] {
        let (status, body) = patch(&app, &token, bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad} 应是 422");
        assert_eq!(
            body["error"]["code"],
            json!("validation_error"),
            "{bad} 是提取器层的错，不是 unknown_config_field"
        );
    }
}

#[tokio::test]
async fn an_empty_object_is_the_empty_config_update_code() {
    let (app, _config, token, _db) = router("empty").await;
    let (status, body) = patch(&app, &token, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], json!("empty_config_update"));
    assert_eq!(
        body["error"]["message"],
        json!("At least one field must be provided")
    );
}

#[tokio::test]
async fn method_not_allowed_still_returns_an_envelope() {
    let (app, _config, token, _db) = router("405").await;
    for method in [Method::POST, Method::DELETE, Method::PUT] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri("/config")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .expect("请求");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{method} 的 405 必须是信封，实际是 {bytes:?}"));
        assert_eq!(body["error"]["code"], json!("http_error"));
        assert_eq!(body["error"]["message"], json!("Method Not Allowed"));
    }
}

#[tokio::test]
async fn a_patch_survives_as_a_file_and_shows_up_on_the_next_get() {
    let (app, config, token, _db) = router("persist").await;
    patch(&app, &token, json!({"logging": {"level": "ERROR"}})).await;

    // 直接问服务（绕开 HTTP）：值真的落到文件里了。
    assert_eq!(
        config.get().expect("读配置")["logging"]["level"],
        json!("ERROR")
    );
    let (status, body) = get(&app, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["values"]["logging"]["level"], json!("ERROR"));
}

#[tokio::test]
async fn consecutive_patches_do_not_undo_each_other_over_http() {
    // 「每次从磁盘快照起算」这条规则在 HTTP 层最容易丢：两个请求可能落在
    // 同一个进程里，读的是同一份内存状态。
    let (app, _config, token, _db) = router("consecutive").await;
    patch(
        &app,
        &token,
        json!({"scheduler": {"log_dir": "/data/logs-a"}}),
    )
    .await;
    patch(&app, &token, json!({"scheduler": {"enabled": false}})).await;

    let (_, body) = get(&app, &token).await;
    assert_eq!(
        body["values"]["scheduler"]["log_dir"],
        json!("/data/logs-a"),
        "第二次 PATCH 不能把第一次的结果用旧快照覆盖掉"
    );
    assert_eq!(body["values"]["scheduler"]["enabled"], json!(false));
}
