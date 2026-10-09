//! `GET/PATCH /indexer-settings` 的 HTTP 契约测试。
//!
//! # 三个最容易悄悄改错的契约
//!
//! **① `api_key` 的三态。** 省略 = 沿用旧值；`null` = 清空；有值 = 设为新值。
//! 写成 `Option<String>` 就把前两种合并了 —— 升级后用户不改 key 点一次保存，
//! 所有索引器的 key 会被清空，而**没有任何报错**。
//!
//! **② 整表替换。** 请求体里的 `indexers` 是完整列表，保存后库里就正好是
//! 這些。客户端基于 `GET` 的结果提交全文，所以「保存」必须能删掉条目。
//!
//! **③ 绑定顺序。** `download_clients` 按**绑定行 id** 升序，不是按名字 ——
//! 提交下载时「同 kind 内按绑定顺序挑选」，所以顺序是行为。
//!
//! 这三条都不可能靠 `cargo test --lib` 覆盖：前两条是 HTTP 三态与事务语义，
//! 第三条要真的写库再读回。

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{DownloadClientRepository, NewDownloadClient};
use sm_db::testing::TestDb;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "indexer-secret";

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn temp_config_path() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("sm-api-idx-{}-{n}.toml", std::process::id()))
}

async fn setup() -> (TestDb, AppState, String) {
    let db = TestDb::require().await;
    let users = sm_db::repo::UserRepository::new(db.pool().clone());
    let username = format!(
        "ix{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let user = users
        .insert(&sm_db::repo::NewUser {
            username,
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    let token = encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET);
    let state = AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(temp_config_path()),
    );
    (db, state, token)
}

fn app(state: &AppState) -> axum::Router {
    router(state.clone())
}

async fn patch(state: &AppState, token: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::PATCH)
        .uri("/indexer-settings")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

async fn get(state: &AppState, token: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/indexer-settings")
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

// ====================================================== GET /indexer-settings/test

/// `GET /test` —— **探测失败也是 200**（那是一份报告，不是请求失败），
/// 且字段集合照抄上游 `IndexerConnectionTestResponse`。
#[tokio::test]
async fn the_connection_test_without_indexers_is_a_200_report() {
    let (_db, state, token) = setup().await;
    let request = Request::builder()
        .method(Method::GET)
        .uri("/indexer-settings/test")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求");
    let response = app(&state).oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("响应是 JSON");

    assert_eq!(status, StatusCode::OK, "探测报告该是 200：{body}");
    for key in [
        "healthy",
        "checked_at",
        "query",
        "indexers_checked",
        "result_count",
        "elapsed_ms",
        "error",
    ] {
        assert!(body.get(key).is_some(), "缺字段 {key}: {body}");
    }
    assert_eq!(
        body.as_object().expect("是对象").len(),
        7,
        "字段数变了：{body}"
    );
    // 全新的 schema 没有任何 indexer —— 正是 `no_indexers_configured` 那条。
    assert_eq!(body["healthy"], json!(false));
    assert_eq!(body["query"], json!("SSNI-888"));
    assert_eq!(body["indexers_checked"], json!(0));
    assert_eq!(body["result_count"], json!(0));
    assert_eq!(body["error"]["type"], json!("no_indexers_configured"));
    assert!(body["error"]["message"].as_str().is_some());
}

/// 一个合法的索引器项。
fn item(name: &str, kind: &str, clients: Vec<i32>) -> Value {
    json!({
        "name": name,
        "url": format!("https://{name}.example.com/api"),
        "kind": kind,
        "download_client_ids": clients,
    })
}

async fn seed_client(db: &TestDb, name: &str) -> i32 {
    // `download_client.library_id` 是**非空外键**指向 `media_library`，
    // 所以先建一个库。
    let library_id = sm_db::repo::MediaLibraryRepository::new(db.pool().clone())
        .insert(&sm_db::repo::NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id;
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: name.to_owned(),
            provider_config: None,
            library_id,
        })
        .await
        .expect("insert download client")
        .id
}

// ================================================================ 形状

#[tokio::test]
async fn the_response_has_the_upstream_shape() {
    let (_db, state, token) = setup().await;
    let (status, body) = get(&state, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["indexers"].is_array(), "indexers 必须是数组：{body}");

    // 空库时是空数组而不是 null —— 客户端直接迭代它
    let items = body["indexers"].as_array().expect("数组");
    // 库里可能有其它测试留下的索引器，所以只检查形状
    if let Some(first) = items.first() {
        for key in ["id", "name", "url", "kind", "api_key", "download_clients"] {
            assert!(first.get(key).is_some(), "缺 {key}: {first}");
        }
    }
}

#[tokio::test]
async fn a_saved_indexer_comes_back_with_its_bindings() {
    let (db, state, token) = setup().await;
    let client_name = format!("dl-{}", n());
    let client_id = seed_client(&db, &client_name).await;
    let name = format!("idx-{}", n());

    let (status, body) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&name, "pt", vec![client_id])] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let items = body["indexers"].as_array().expect("数组");
    let mine = items
        .iter()
        .find(|i| i["name"] == json!(name))
        .expect("刚存的索引器应出现");
    assert_eq!(mine["kind"], json!("pt"));
    assert_eq!(
        mine["url"],
        json!(format!("https://{name}.example.com/api"))
    );
    assert_eq!(mine["api_key"], json!(null), "没传 api_key 时应为 null");
    let clients = mine["download_clients"].as_array().expect("数组");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["id"], json!(client_id));
    assert_eq!(clients[0]["name"], json!(client_name));
}

// ================================================================ api_key 三态

/// 省略 = **沿用旧值**。这是三态里最容易丢的一个。
#[tokio::test]
async fn omitting_the_api_key_keeps_the_existing_one() {
    let (db, state, token) = setup().await;
    let name = format!("keepkey-{}", n());

    // 第一轮：显式设一个 key
    let mut with_key = item(&name, "pt", vec![]);
    with_key["api_key"] = json!("secret-1");
    let (status, _) = patch(&state, &token, json!({ "indexers": [with_key] })).await;
    assert_eq!(status, StatusCode::OK);

    // 第二轮：同一名字，**不带** api_key 键
    let (status, body) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&name, "bt", vec![])] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mine = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|i| i["name"] == json!(name))
        .expect("索引器应存在")
        .clone();
    assert_eq!(
        mine["api_key"],
        json!("secret-1"),
        "省略 api_key 必须沿用旧值 —— 丢了它就等于每次保存都清空 key"
    );
    assert_eq!(mine["kind"], json!("bt"), "其余字段照常更新");
    let _ = db;
}

/// 显式 `null` = **清空**。
#[tokio::test]
async fn an_explicit_null_api_key_clears_it() {
    let (db, state, token) = setup().await;
    let name = format!("clearkey-{}", n());

    let mut with_key = item(&name, "pt", vec![]);
    with_key["api_key"] = json!("secret-2");
    patch(&state, &token, json!({ "indexers": [with_key] })).await;

    let mut with_null = item(&name, "pt", vec![]);
    with_null["api_key"] = json!(null);
    let (status, body) = patch(&state, &token, json!({ "indexers": [with_null] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mine = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|i| i["name"] == json!(name))
        .expect("索引器应存在");
    assert_eq!(
        mine["api_key"],
        json!(null),
        "显式 null 必须清空，而不是被当成「省略」而沿用旧值"
    );
    let _ = db;
}

/// 空串与纯空白归一为 `null`（Torznab 的「不带 apikey」形态）。
#[tokio::test]
async fn a_blank_api_key_normalizes_to_null() {
    let (db, state, token) = setup().await;
    let name = format!("blankkey-{}", n());
    let mut blank = item(&name, "pt", vec![]);
    blank["api_key"] = json!("   ");
    let (status, body) = patch(&state, &token, json!({ "indexers": [blank] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mine = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|i| i["name"] == json!(name))
        .expect("索引器应存在");
    assert_eq!(mine["api_key"], json!(null), "空白 key 归一为 null");
    let _ = db;
}

// ================================================================ 整表替换

/// 保存后库里**正好**是请求里给的那些 —— 删掉的条目真的消失。
#[tokio::test]
async fn saving_replaces_the_whole_table() {
    let (db, state, token) = setup().await;
    let keep = format!("keep-{}", n());
    let drop = format!("drop-{}", n());

    let (status, _) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&keep, "pt", vec![]), item(&drop, "bt", vec![])] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(get(&state, &token).await.1["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .any(|i| i["name"] == json!(drop)));

    // 只提交 keep -> drop 必须消失
    let (status, body) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&keep, "pt", vec![])] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(names.contains(&keep.as_str()), "keep 必须还在");
    assert!(
        !names.contains(&drop.as_str()),
        "整表替换语义：没提交的条目必须消失，实际 {names:?}"
    );
    let _ = db;
}

/// `indexers: []` = **清空全部**（不是「什么都不做」）。
#[tokio::test]
async fn an_empty_array_clears_everything() {
    let (db, state, token) = setup().await;
    let name = format!("wipe-{}", n());
    patch(
        &state,
        &token,
        json!({ "indexers": [item(&name, "pt", vec![])] }),
    )
    .await;

    let (status, body) = patch(&state, &token, json!({ "indexers": [] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        !names.contains(&name.as_str()),
        "空数组是「清空全部」，实际 {names:?}"
    );
    let _ = db;
}

/// 省略 `indexers` 键 = 不动索引器。
#[tokio::test]
async fn omitting_indexers_leaves_the_table_alone() {
    let (db, state, token) = setup().await;
    let name = format!("keepall-{}", n());
    patch(
        &state,
        &token,
        json!({ "indexers": [item(&name, "pt", vec![])] }),
    )
    .await;

    // 只带废弃字段 -> 不动索引器，也不报错（兼容旧前端）
    let (status, body) = patch(
        &state,
        &token,
        json!({ "type": "torznab", "api_key": "legacy" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "废弃字段应被忽略而非 422：{body}");
    let names: Vec<&str> = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(
        names.contains(&name.as_str()),
        "只带废弃字段时索引器表不该被动"
    );
    let _ = db;
}

// ================================================================ 校验

#[tokio::test]
async fn an_empty_body_is_422_with_its_own_code() {
    let (_db, state, token) = setup().await;
    let (status, body) = patch(&state, &token, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        code_of(&body),
        "empty_indexer_settings_update",
        "空更新有专属码，客户端要区分「什么都没改」与「键不存在」"
    );
}

#[tokio::test]
async fn each_validation_failure_has_its_own_code() {
    let (db, state, token) = setup().await;
    // 重复 id 那一条需要**真实存在**的下载器：上游在同一个循环里既查重复
    // 又查存在性，所以不存在的 id 会先撞 404 —— 用一个不存在的 id 去测
    // 「重复」根本到不了那条分支。
    let real_client = seed_client(&db, &format!("dup-dl-{}", n())).await;

    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "indexers": [{ "name": "  ", "url": "https://x.example/api",
                                  "kind": "pt", "download_client_ids": [] }] }),
            "invalid_indexer_settings_name",
        ),
        (
            json!({ "indexers": [{ "name": "a", "url": "not-a-url",
                                  "kind": "pt", "download_client_ids": [] }] }),
            "invalid_indexer_settings_url",
        ),
        (
            json!({ "indexers": [{ "name": "a", "url": "https://x.example/api",
                                  "kind": "torznab", "download_client_ids": [] }] }),
            "invalid_indexer_settings_kind",
        ),
        (
            json!({ "indexers": [{ "name": "a", "url": "https://x.example/api",
                                  "kind": "pt", "download_client_ids": [0] }] }),
            "invalid_indexer_settings_download_client_ids",
        ),
    ];
    for (payload, expected) in cases {
        let (status, body) = patch(&state, &token, payload.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{payload}");
        assert_eq!(code_of(&body), expected, "{payload}");
    }

    // 重复 id 用真实存在的 client
    let (status, body) = patch(
        &state,
        &token,
        json!({ "indexers": [{ "name": "dup", "url": "https://x.example/api",
                              "kind": "pt",
                              "download_client_ids": [real_client, real_client] }] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        code_of(&body),
        "duplicate_indexer_settings_download_client_id"
    );
}

/// 同名（大小写不同）也算重复 —— 数据库区分大小写，但对用户是同一个。
#[tokio::test]
async fn a_case_insensitive_duplicate_name_is_rejected() {
    let (_db, state, token) = setup().await;
    let name = format!("Dup-{}", n());
    let (status, body) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&name, "pt", vec![]), item(&name.to_lowercase(), "bt", vec![])] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code_of(&body), "duplicate_indexer_settings_name");
}

/// 引用一个不存在的下载器是 **404**，不是 422。
#[tokio::test]
async fn an_unknown_download_client_is_404() {
    let (db, state, token) = setup().await;
    let name = format!("missing-dl-{}", n());
    // 用一个极大 id，几乎不可能存在
    let (status, body) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&name, "pt", vec![i32::MAX])] }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code_of(&body), "indexer_settings_download_client_not_found");
    assert_eq!(
        body["error"]["details"]["download_client_id"].as_i64(),
        Some(i64::from(i32::MAX))
    );
    let _ = db;
}

/// 校验失败时**不能**动表 —— 整表替换的全删全插必须在校验之后。
#[tokio::test]
async fn a_rejected_payload_leaves_the_table_untouched() {
    let (db, state, token) = setup().await;
    let good = format!("survivor-{}", n());
    patch(
        &state,
        &token,
        json!({ "indexers": [item(&good, "pt", vec![])] }),
    )
    .await;

    // 一批里第二项非法 -> 整批拒绝，第一项也不该被写进去
    let bad = format!("bad-{}", n());
    let (status, _) = patch(
        &state,
        &token,
        json!({ "indexers": [item(&good, "pt", vec![]), item(&bad, "WRONG", vec![])] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, body) = get(&state, &token).await;
    let names: Vec<&str> = body["indexers"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(names.contains(&good.as_str()), "原有条目必须还在");
    assert!(
        !names.contains(&bad.as_str()),
        "被拒的批次里不该有任何一条被写入 —— 全删全插在校验之后"
    );
    let _ = db;
}

// ================================================================ 鉴权与 405

#[tokio::test]
async fn the_endpoint_requires_authentication() {
    let (_db, state, _token) = setup().await;
    let request = Request::builder()
        .method(Method::GET)
        .uri("/indexer-settings")
        .body(Body::empty())
        .expect("构造请求");
    let response = app(&state).oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_method_other_than_get_or_patch_is_405_with_an_envelope() {
    let (_db, state, token) = setup().await;
    for method in [Method::POST, Method::DELETE, Method::PUT] {
        let request = Request::builder()
            .method(method.clone())
            .uri("/indexer-settings")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("构造请求");
        let response = app(&state).oneshot(request).await.expect("oneshot");
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读体")
            .to_bytes();
        let body: Value = serde_json::from_slice(&bytes).expect("应是 JSON 信封");
        assert_eq!(body["error"]["code"], json!("http_error"), "{method}");
    }
}

#[tokio::test]
async fn a_malformed_body_is_422_validation_error() {
    let (_db, state, token) = setup().await;
    let request = Request::builder()
        .method(Method::PATCH)
        .uri("/indexer-settings")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(br#"{"indexers":"#.to_vec()))
        .expect("构造请求");
    let response = app(&state).oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读体")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("应是 JSON 信封");
    assert_eq!(body["error"]["code"], json!("validation_error"));
}
