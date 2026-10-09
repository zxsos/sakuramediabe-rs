//! `GET /status/capabilities` 的 HTTP 契约测试。
//!
//! # 这个端点为什么不需要数据库
//!
//! 它是唯一一个只读配置、不查表的端点 —— 能力开关来自
//! `config.toml`（缺文件时退回 schema 默认值）。所以这里刻意用
//! **临时配置文件**驱动，而不是 `TestDb`：断言的是「读配置 → 回答」这条链，
//! 而 `TestDb` 会引入一个与被测行为无关的失败原因。
//!
//! # 但仍然要真状态
//!
//! `AppState` 必须构造完整（池 / 鉴权 / 配置服务），因为端点声明了
//! `CurrentUser` —— 没鉴权的路径不该被测成「碰巧能过」。

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "capabilities-secret";

/// 写一份配置到临时文件并返回路径。
fn write_config(contents: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("sm-api-caps-{}-{n}.toml", std::process::id()));
    std::fs::write(&path, contents).expect("写临时配置");
    path
}

/// 写一份配置、建一个用户，返回 `(库, 状态, token)`。
async fn setup_with(contents: &str) -> (TestDb, AppState, String) {
    let db = TestDb::require().await;
    let users = UserRepository::new(db.pool().clone());
    let username = format!(
        "cap{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let user = users
        .insert(&NewUser {
            username,
            // 仓储只校验非空、不解析 PHC —— 这里不需要真的 Argon2 串
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    let token = encode_access_token(
        i64::from(user.id),
        chrono::Utc::now() + chrono::Duration::hours(1),
        SECRET,
    );
    let state = AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(write_config(contents)),
    );
    (db, state, token)
}

fn app(state: &AppState) -> axum::Router {
    router(state.clone())
}

async fn get(state: &AppState, token: &str, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求");
    let response = app(state).oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "响应体不是 JSON: {e}; 原始: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

#[tokio::test]
async fn the_default_deployment_claims_neither_capability() {
    // 缺文件 -> 退回 schema 默认值 -> 两个都是 false。
    // 这是最常见的首屏场景，客户端据此隐藏图搜入口。
    // 指向一个不存在的路径：snapshot() 会退回 schema 默认值
    let (db, _state, token) = setup_with("").await;
    let missing = AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(std::env::temp_dir().join("definitely-absent-cap.toml")),
    );

    let (status, body) = get(&missing, &token, "/status/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "movie_similarity": false, "image_search": false }),
        "默认部署不该声称自己有这两个能力"
    );
}

#[tokio::test]
async fn enabling_qdrant_alone_enables_movie_similarity_only() {
    let (_db, state, token) = setup_with("[qdrant]\nenabled = true\n").await;
    let (status, body) = get(&state, &token, "/status/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "movie_similarity": true, "image_search": false }),
        "图搜还需要 image_search.enabled —— 依赖的是向量库 + 推理服务两样"
    );
}

#[tokio::test]
async fn both_switches_enable_image_search() {
    let (_db, state, token) =
        setup_with("[qdrant]\nenabled = true\n\n[image_search]\nenabled = true\n").await;
    let (status, body) = get(&state, &token, "/status/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "movie_similarity": true, "image_search": true })
    );
}

/// 存量配置里可能有「开了图搜没开 Qdrant」的组合（老版本没有那条跨节校验）。
/// 端点必须读作「图搜不可用」，而不是照抄 `image_search.enabled`。
#[tokio::test]
async fn image_search_without_qdrant_reads_as_disabled() {
    let (_db, state, token) =
        setup_with("[qdrant]\nenabled = false\n\n[image_search]\nenabled = true\n").await;
    let (status, body) = get(&state, &token, "/status/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["image_search"],
        json!(false),
        "没有 Qdrant 就没有向量库，图搜不可用"
    );
    assert_eq!(body["movie_similarity"], json!(false));
}

/// 响应体**只有**这两个键 —— 上游那个端点没有 response_model，返回的就是
/// `capabilities()` 的 dict。多一个键客户端不会报错，但形状漂移就说明
/// 有人改了 `capabilities()`。
#[tokio::test]
async fn the_response_has_exactly_the_two_capability_keys() {
    let (_db, state, token) = setup_with("[qdrant]\nenabled = true\n").await;
    let (_, body) = get(&state, &token, "/status/capabilities").await;
    let object = body.as_object().expect("应是对象");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["image_search", "movie_similarity"]);
}

#[tokio::test]
async fn the_endpoint_requires_authentication() {
    let (_db, state, _token) = setup_with("").await;
    let request = Request::builder()
        .method("GET")
        .uri("/status/capabilities")
        .body(Body::empty())
        .expect("构造请求");
    let response = app(&state).oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读体")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("应是 JSON 信封");
    assert_eq!(body["error"]["code"], json!("unauthorized"));
}

#[tokio::test]
async fn a_method_other_than_get_is_405_with_an_envelope() {
    let (_db, state, token) = setup_with("").await;
    let request = Request::builder()
        .method("POST")
        .uri("/status/capabilities")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求");
    let response = app(&state).oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读体")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("应是 JSON 信封");
    assert_eq!(body["error"]["code"], json!("http_error"));
}

/// `GET /status/metadata-providers/{provider}/test` —— **只接受 `javdb`**。
///
/// 其它值一律 422 `invalid_metadata_provider`，且 `details.provider` 回显
/// **原始**入参（上游 `{"provider": provider}`，不是归一化后的值）—— 客户端据此
/// 显示自己到底发了什么。
///
/// ⚠️ **这里只测 422 那条路**。`javdb` 那条会去连真 JavDB（host 是硬编码常量），
/// 在测试里发真实外网请求是不允许的；探测链路本身由
/// `crates/sm-service/tests/status_metadata_provider.rs` 用假 JavDB 覆盖。
#[tokio::test]
async fn an_unknown_metadata_provider_is_422_and_echoes_the_raw_value() {
    let (_db, state, token) = setup_with("").await;

    // 大小写与空格都不影响判断（上游 `provider.strip().lower()`），但回显的是原样。
    let (status, body) = get(&state, &token, "/status/metadata-providers/JavBus/test").await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(body["error"]["code"], json!("invalid_metadata_provider"));
    assert_eq!(
        body["error"]["details"]["provider"],
        json!("JavBus"),
        "details 放**原始**入参，不是归一化后的 javbus"
    );
}
