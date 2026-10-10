//! `/image-search/sessions/{}/results` 与 `/image-search/plot-sessions/{}/results`
//! 的 HTTP 层契约。
//!
//! 覆盖三类**只有走 HTTP 才暴露**的问题：
//!
//! 1. **未启用必须是 409，不能降级成空列表** —— 这是本组端点与「相似度」那
//!    类降级信号的**唯一**差别（上游把 `require_image_search()` 挂成 router
//!    级依赖，`image_search.py:30`）。返回空结果会让用户以为「搜过了、没有」；
//! 2. **空 `cursor` 是 422，且先于能力检查** —— 上游 `Query(min_length=1)`
//!    是 pydantic 的参数校验，发生在依赖注入之前，所以「未启用 + 空 cursor」
//!    要得到 422 而不是 409；
//! 3. **鉴权先于一切** —— 未登录是 401，不是 409/422。
//!
//! # 为什么这里**不**注入服务，而只测「未注入」的那一侧
//!
//! 注入真服务要连 Qdrant 与推理服务，而这两个在 CI 里不一定有。而未注入正是
//! 「未启用」的表达 —— 组合根就是照配置开关决定挂不挂的。所以测「没挂 → 409」
//! 等价于测「未启用 → 409」，且不依赖外部服务。

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use sm_api::AppState;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt as _;

const SECRET: &str = "image-search-http-secret";

/// 每个用例一个临时配置文件（不启用图搜 —— 见模块文档第 3 节）。
fn config_service(tag: &str) -> (ConfigService, std::path::PathBuf) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sm-api-isearch-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建目录");
    let path = dir.join("config.toml");
    (ConfigService::new(&path), path)
}

async fn router() -> (axum::Router, String, sm_db::testing::TestDb) {
    let db = sm_db::testing::TestDb::require().await;
    let user_id = seed_user(db.pool()).await;
    let (config, _path) = config_service("r");
    // ★ 刻意**不**调 `with_image_search` / `with_plot_image_search`：那正是
    // 「图搜未启用」的表达。
    let state = AppState::new(db.pool().clone(), AuthConfig::new(SECRET), config);
    (sm_api::router(state), token(user_id), db)
}

/// `CurrentUser` 提取器会查库，所以要有行。
async fn seed_user(db: &sm_db::Db) -> i32 {
    use sm_db::repo::{NewUser, UserRepository};
    let username = format!(
        "is{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let hash = sm_core::password::hash_password_with("pw", 64, 1, 1).expect("哈希");
    UserRepository::new(db.clone())
        .insert(&NewUser {
            username,
            password_hash: hash,
        })
        .await
        .expect("插入用户")
        .id
}

fn token(user_id: i32) -> String {
    sm_core::jwt::encode_access_token(
        i64::from(user_id),
        chrono::Utc::now() + chrono::Duration::days(1),
        SECRET,
    )
}

/// 打一个 GET，返回 `(状态码, 解析后的 body)`。
async fn get(router: &axum::Router, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::get(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).expect("构造请求"))
        .await
        .expect("请求");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("读响应体")
        .to_bytes();
    let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, parsed)
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

/// 打一个 `application/x-www-form-urlencoded` 的 POST。
async fn post_form(
    router: &axum::Router,
    path: &str,
    token: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    use axum::http::header::CONTENT_TYPE;
    let encoded = fields
        .iter()
        .map(|(key, value)| format!("{}={}", urlencoding_encode(key), urlencoding_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = router
        .clone()
        .oneshot(
            Request::post(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(encoded))
                .expect("构造请求"),
        )
        .await
        .expect("请求");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("读响应体")
        .to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// form 编码（`page_size` / `movie_ids` 这类字段里没有需要转义的字符，
/// 所以只处理必要的几个）。
fn urlencoding_encode(raw: &str) -> String {
    raw.replace('%', "%25")
        .replace('+', "%2B")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace(' ', "+")
}

/// ★ 未启用 → **409 `feature_disabled`**，不是 200 空列表。
#[tokio::test]
async fn a_disabled_feature_is_409_not_an_empty_list() {
    let (router, token, _db) = router().await;
    for path in [
        "/image-search/sessions/abc123/results",
        "/image-search/plot-sessions/abc123/results",
    ] {
        let (status, body) = get(&router, path, Some(&token)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path} 该是 409：{body}");
        assert_eq!(code_of(&body), "feature_disabled");
    }
}

/// ★ 未启用时**一切**参数错误都是 409，不是 422。
///
/// 顺序照 FastAPI 的 `solve_dependencies`（`fastapi/dependencies/utils.py`）：
/// 它**先**跑 `dependant.dependencies` 的循环（我们的 `require_image_search`
/// 就在这一层，`APIRouter(dependencies=[...])`），`request_params_to_args`
/// 在循环**之后**。`require_image_search` 是直接 `raise ApiError(409, ...)`，
/// 不是累积进 `errors` 列表 —— 所以一旦未启用就立刻返回，参数校验根本没跑。
///
/// 这条**推翻**了先前写在这里的「422 先于 409」：那个断言的理由是「pydantic
/// 校验在依赖之前」，而 `Query(min_length=1)` 属于 endpoint 自己的
/// `dependant.query_params`，在依赖**之后**解析。
#[tokio::test]
async fn a_disabled_feature_beats_every_validation_error() {
    let (router, token, _db) = router().await;
    for path in [
        "/image-search/sessions/abc123/results?cursor=",
        "/image-search/plot-sessions/abc123/results?cursor=",
    ] {
        let (status, body) = get(&router, path, Some(&token)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}：{body}");
        assert_eq!(code_of(&body), "feature_disabled");
    }
}

/// 四个建会话端点同样：未启用 → 409，而不是「缺 file 字段」的 422。
#[tokio::test]
async fn a_disabled_feature_also_beats_a_missing_upload() {
    let (router, token, _db) = router().await;
    for path in [
        "/image-search/sessions",
        "/image-search/plot-sessions",
        "/image-search/text-sessions",
        "/image-search/plot-text-sessions",
    ] {
        let (status, body) = post_form(&router, path, &token, &[("text", "")]).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}：{body}");
        assert_eq!(code_of(&body), "feature_disabled");
    }
}

/// 鉴权先于一切：未登录是 **401**，不是 409 也不是 422。
#[tokio::test]
async fn an_unauthenticated_request_is_401() {
    let (router, _token, _db) = router().await;
    for path in [
        "/image-search/sessions/abc123/results",
        "/image-search/sessions/abc123/results?cursor=",
    ] {
        let (status, body) = get(&router, path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}：{body}");
        assert_eq!(code_of(&body), "unauthorized");
    }
}
