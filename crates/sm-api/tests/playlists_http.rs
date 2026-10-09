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
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        sm_service::system::ConfigService::new(temp_config_path()),
    ))
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

/// 指向临时目录的配置路径。
///
/// 那些**不碰配置**的端点测试也需要一个 `ConfigService`，而它们绝不能写
/// 到真实的 `config.toml` 上 —— 那是开发机/容器的配置。所以给一个每次调用
/// 都不同的临时路径：即便某个用例意外触发了写盘，也只会留下空目录里的孤立
/// 文件。
fn temp_config_path() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!("sm-api-unused-{}-{n}.toml", std::process::id()))
}

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
        sm_service::system::ConfigService::new(temp_config_path()),
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

// ------------------------------------------------------------------ 列表端点

/// 建一个播放列表，返回其 id。
async fn seed_playlist(db: &TestDb, name: &str) -> i32 {
    let (status, body) = send(
        app(db),
        authed(
            "POST",
            "/playlists",
            &seed_token(db.pool()).await,
            Some(json!({ "name": name })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "建列表失败: {body}");
    body["id"].as_i64().expect("响应应有 id") as i32
}

/// 把 `code_of` 提成对**数组**也适用的断言辅助。
fn arr(body: &Value) -> &Vec<Value> {
    body.as_array()
        .unwrap_or_else(|| panic!("期望 JSON 数组，得到 {body}"))
}

#[tokio::test]
async fn get_playlists_returns_an_array_with_counts() {
    let (db, token) = setup().await;
    let mine = seed_playlist(&db, "http-列表-甲").await;

    let (status, body) = send(app(&db), authed("GET", "/playlists", &token, None)).await;
    assert_eq!(status, StatusCode::OK);
    let items = arr(&body);
    assert!(!items.is_empty(), "刚建的列表必须出现");

    // 响应体形状：字段集合与上游 PlaylistResource 一致
    let first = &items[0];
    for key in [
        "id",
        "name",
        "kind",
        "description",
        "is_system",
        "is_mutable",
        "is_deletable",
        "movie_count",
        "created_at",
        "updated_at",
    ] {
        assert!(first.get(key).is_some(), "缺字段 {key}: {first}");
    }

    // 新建的列表 movie_count 必须是数字 0，而不是缺失/null
    let row = items
        .iter()
        .find(|r| r["id"].as_i64() == Some(i64::from(mine)))
        .expect("新建的列表应出现在列表里");
    assert_eq!(row["movie_count"].as_i64(), Some(0));
}

#[tokio::test]
async fn include_system_defaults_to_true() {
    let (db, token) = setup().await;
    // 不带参数 -> 包含系统列表
    let (_, all) = send(app(&db), authed("GET", "/playlists", &token, None)).await;
    // 显式 true -> 同样包含
    let (_, explicit) = send(
        app(&db),
        authed("GET", "/playlists?include_system=true", &token, None),
    )
    .await;

    // 两次结果长度一致（系统列表此时可能还不存在，但两次必须同样处理）
    assert_eq!(arr(&all).len(), arr(&explicit).len());
}

/// `include_system` 必须按 **pydantic 的 lax 布尔**解析。
///
/// 这条是本文件里唯一为「客户端会传什么字面量」而存在的断言：serde 默认
/// 只认 `true`/`false`，而上游 FastAPI 认 `1`/`0`/`yes`/`no`/`on`/`off`。
/// 照搬 serde 会让 `?include_system=1` 变成 400 纯文本。
#[tokio::test]
async fn include_system_accepts_every_pydantic_boolean_literal() {
    let (db, token) = setup().await;

    for truthy in ["true", "True", "1", "yes", "on", "t", "y"] {
        let (status, body) = send(
            app(&db),
            authed(
                "GET",
                &format!("/playlists?include_system={truthy}"),
                &token,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "?include_system={truthy} 应被接受: {body}"
        );
    }
    for falsy in ["false", "0", "no", "off", "f", "n"] {
        let (status, body) = send(
            app(&db),
            authed(
                "GET",
                &format!("/playlists?include_system={falsy}"),
                &token,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "?include_system={falsy} 应被接受: {body}"
        );
    }
}

#[tokio::test]
async fn a_nonsense_include_system_is_422_not_silently_false() {
    let (db, token) = setup().await;
    let (status, body) = send(
        app(&db),
        authed("GET", "/playlists?include_system=ture", &token, None),
    )
    .await;
    // 不能是 200 —— 那意味着拼错的值被当成了「关掉系统列表」
    assert!(
        status.is_client_error(),
        "拼错的布尔值不该被静默接受，得到 {status}: {body}"
    );
}

/// 拼错的布尔值必须走**错误信封**，不是 axum 的默认纯文本。
#[tokio::test]
async fn a_bad_query_value_returns_the_error_envelope() {
    let (db, token) = setup().await;
    let (status, body) = send(
        app(&db),
        authed("GET", "/playlists?include_system=ture", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        code_of(&body),
        "validation_error",
        "必须是信封里的 code，而不是纯文本 400"
    );
}

#[tokio::test]
async fn the_list_endpoint_requires_authentication() {
    let (db, _token) = setup().await;
    let request = Request::builder()
        .method("GET")
        .uri("/playlists")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
}

// ------------------------------------------------------------------ 分辨率端点

#[tokio::test]
async fn resolutions_of_a_fresh_playlist_is_an_empty_array() {
    let (db, token) = setup().await;
    let id = seed_playlist(&db, "http-档位-空").await;

    let (status, body) = send(
        app(&db),
        authed("GET", &format!("/playlists/{id}/resolutions"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([]),
        "没有影片就没有档位，且必须是空数组而非 null"
    );
}

#[tokio::test]
async fn resolutions_of_an_unknown_playlist_is_404() {
    let (db, token) = setup().await;
    let (status, body) = send(
        app(&db),
        authed("GET", "/playlists/2147483647/resolutions", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&body), "playlist_not_found");
    assert_eq!(
        body["error"]["details"]["playlist_id"].as_i64(),
        Some(2147483647)
    );
}

#[tokio::test]
async fn a_method_other_than_get_on_resolutions_is_405_with_an_envelope() {
    let (db, token) = setup().await;
    let id = seed_playlist(&db, "http-档位-405").await;
    let (status, body) = send(
        app(&db),
        authed(
            "POST",
            &format!("/playlists/{id}/resolutions"),
            &token,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(code_of(&body), "http_error");
}

#[tokio::test]
async fn the_resolutions_endpoint_requires_authentication() {
    let (db, _token) = setup().await;
    let request = Request::builder()
        .method("GET")
        .uri("/playlists/1/resolutions")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app(&db), request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&body), "unauthorized");
}

/// `/playlists/{id}/movies` **已落地**，不能再落到 router 的 fallback。
///
/// 这条曾经断言 404（端点未实现时的形状）。端点落地后它必须跟着改 ——
/// 而不是删掉：删掉之后「这条路由被误删/误改」就没有任何用例看得见
/// （本文件的其余用例一条都不走这个路径）。
///
/// 卡片内容的契约在 `tests/playlist_movies_http.rs`。
#[tokio::test]
async fn the_movies_endpoint_is_registered_not_a_fallback_404() {
    let (db, token) = setup().await;
    let id = seed_playlist(&db, "http-影片端点").await;
    let (status, body) = send(
        app(&db),
        authed("GET", &format!("/playlists/{id}/movies"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert!(body.get("items").is_some(), "应当是分页信封：{body}");
}
