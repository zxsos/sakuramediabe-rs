//! `POST /imports/{task_run_id}/failed-items/{item_id}/retry` 的 HTTP 契约测试
//! （真库）。
//!
//! # 为什么还要一层 HTTP
//!
//! 服务层的判据（[`sm_service::transfers::import_task`]）已经由
//! `sm-service/tests/media_import_failed_items.rs` 逐条钉住了。这里只测**接线**
//! 与**线格式**，也就是服务层测不到的三件事：
//!
//! | 判据 | 期望 |
//! |---|---|
//! | 状态码 | **202**（不是 200 —— 后台有活在跑） |
//! | 响应体 | `{task_run_id, task_key, state}`，`state` 是 `"pending"` |
//! | 没装搜索服务 | **503**，不是 500/404 |
//! | 错误信封 | 坏候选走**错误信封**（`error.code`），不是裸文本 |
//!
//! 503 那条尤其重要：它证明这条端点的依赖是**注入进来**的（`AppState` 的
//! `metadata_search` 槽位），而不是在 handler 里 `unwrap` 出一个。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{BackgroundTaskRunRepository, NewTaskRun, NewUser, UserRepository};
use sm_db::system::task_state;
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::catalog::metadata_source::MetadataSourceService;
use sm_service::catalog::movie_metadata_search::MovieMetadataSearchService;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "retry-route-secret";

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
        let base = std::env::temp_dir().join(format!("sm-retry-{tag}-{}", unique()));
        std::fs::create_dir_all(&base).expect("建临时目录");
        let config_path = base.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[auth]\nfile_signature_secret = \"{SECRET}\"\n"),
        )
        .expect("写测试配置");
        Self { config_path }
    }

    /// 搜索服务指向**不存在**的配置文件：`snapshot()` 对不存在的路径返回默认
    /// 配置，于是「哪些插件启用」= 空 —— 而 `javdb:` 候选不过那道闸。
    fn search(&self) -> MovieMetadataSearchService {
        MovieMetadataSearchService::new(
            ConfigService::new(self.config_path.with_file_name("plugins.toml")),
            std::sync::Arc::new(MetadataSourceService::new(Vec::new(), None)),
        )
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
            username: format!("rt{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(
        i64::from(user.id),
        chrono::Utc::now() + chrono::Duration::hours(1),
        SECRET,
    )
}

fn app(db: &TestDb, fixture: &Fixture, with_search: bool) -> axum::Router {
    let state = AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(fixture.config_path.clone()),
    );
    let state = if with_search {
        state.with_metadata_search(std::sync::Arc::new(fixture.search()))
    } else {
        state
    };
    router(state)
}

fn post(uri: &str, token: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
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

/// 一条终态导入任务 + 它里面的一个待处理失败项，返回 `(任务行 id, 媒体库 id)`。
async fn seed_case(db: &TestDb) -> (i32, i32) {
    let (library_id,): (i32,) = sqlx::query_as(
        "INSERT INTO media_library (name, provider_key) VALUES ($1, 'local') RETURNING id",
    )
    .bind(format!("重试路由库-{}", n()))
    .fetch_one(db.pool())
    .await
    .expect("insert media_library");

    let run = BackgroundTaskRunRepository::new(db.pool().clone())
        .enqueue(&NewTaskRun {
            task_key: "library_import".to_owned(),
            task_name: "JAV媒体库导入".to_owned(),
            trigger_type: "manual".to_owned(),
            mutex_key: Some(format!("retry-route-{}", n())),
            params: None,
            scheduled_at: Some(sm_db::common::time::now_utc()),
        })
        .await
        .expect("enqueue");
    let summary = json!({
        "failed_files": [{
            "id": "item-1",
            "name": "item-1.mkv",
            "relative_path": "人妻/item-1.mkv",
            "size_bytes": 2048,
            "is_video": true,
            "source_ref": {"path": "人妻/item-1.mkv"},
            "library_id": library_id,
            "media_kind": "jav",
            "source_disposition": "keep",
            "path": "人妻/item-1.mkv",
            "reason": "movie_number_not_found",
            "detail": "boom",
            "kind": "file",
            "state": "pending",
            "retry_task_run_id": null,
            "resolved_movie_id": null,
            "resolved_media_id": null,
            "last_retry_error": null,
        }],
    })
    .to_string();
    sqlx::query("UPDATE background_task_run SET state = $1, result_summary = $2 WHERE id = $3")
        .bind(task_state::COMPLETED)
        .bind(&summary)
        .bind(run.id)
        .execute(db.pool())
        .await
        .expect("置终态并写摘要");
    (run.id, library_id)
}

fn candidate() -> String {
    MovieMetadataSearchService::javdb_candidate_id("ABP-123", "javdb-id-1")
}

/// 主路径：**202** + 三字段响应体 + 失败项被标成 `queued`。
#[tokio::test]
async fn retrying_a_failed_item_is_202() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("ok");
    let token = seed_token(db.pool()).await;
    let (run_id, library_id) = seed_case(&db).await;

    let (status, body) = send(
        app(&db, &fixture, true),
        post(
            &format!("/imports/{run_id}/failed-items/item-1/retry"),
            &token,
            &json!({ "candidate_id": candidate() }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED, "202 —— 后台有活在跑");
    let new_run = body["task_run_id"].as_i64().expect("有 task_run_id");
    assert_ne!(new_run as i32, run_id, "重试是另一条任务");
    assert_eq!(body["task_key"], "library_import");
    assert_eq!(body["state"], task_state::PENDING);

    // 库里确实是这个状态：新任务 pending、同库互斥键、旧任务的那条标 queued。
    let (state, mutex_key): (String, Option<String>) =
        sqlx::query_as("SELECT state, mutex_key FROM background_task_run WHERE id = $1")
            .bind(new_run)
            .fetch_one(db.pool())
            .await
            .expect("查新任务");
    assert_eq!(state, task_state::PENDING);
    assert_eq!(
        mutex_key.as_deref(),
        Some(format!("library_import:{library_id}").as_str())
    );

    let (summary,): (Option<String>,) =
        sqlx::query_as("SELECT result_summary FROM background_task_run WHERE id = $1")
            .bind(run_id)
            .fetch_one(db.pool())
            .await
            .expect("查旧任务");
    let summary: Value = serde_json::from_str(summary.as_deref().expect("有摘要")).expect("可解析");
    assert_eq!(summary["failed_files"][0]["state"], "queued");
    assert_eq!(summary["failed_files"][0]["retry_task_run_id"], new_run);
}

/// 坏候选 → **422 错误信封**（`error.code`），不是裸文本。
#[tokio::test]
async fn a_broken_candidate_comes_back_as_an_error_envelope() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("bad");
    let token = seed_token(db.pool()).await;
    let (run_id, _) = seed_case(&db).await;

    let (status, body) = send(
        app(&db, &fixture, true),
        post(
            &format!("/imports/{run_id}/failed-items/item-1/retry"),
            &token,
            &json!({ "candidate_id": "不是候选" }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_metadata_candidate");
}

/// 空 `candidate_id` → **422 `validation_error`**（字段约束先于候选解码）。
#[tokio::test]
async fn a_blank_candidate_id_is_a_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("blank");
    let token = seed_token(db.pool()).await;
    let (run_id, _) = seed_case(&db).await;

    let (status, body) = send(
        app(&db, &fixture, true),
        post(
            &format!("/imports/{run_id}/failed-items/item-1/retry"),
            &token,
            &json!({ "candidate_id": "   " }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_error");
}

/// 没装搜索服务 → **503 `provider_not_installed`**（不是 500）。
///
/// 这条端点的候选校验依赖插件栈（「那个来源现在还启用吗」），平台没起时这个
/// 能力不存在 —— 503 而不是「没搜到」，也不是「服务坏了」。
///
/// ★ 这条用例把一处**文档与行为的分歧**逼了出来：`AppState::metadata_search`
/// 的文档一直写 503，而实现复用的是 `plugin_admin_unavailable`（**500**）。
/// 500 会让客户端/监控把它当故障报警，而它其实是「这台机器没装插件」。
#[tokio::test]
async fn without_the_search_service_the_route_is_503() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("noservice");
    let token = seed_token(db.pool()).await;
    let (run_id, _) = seed_case(&db).await;

    let (status, body) = send(
        app(&db, &fixture, false),
        post(
            &format!("/imports/{run_id}/failed-items/item-1/retry"),
            &token,
            &json!({ "candidate_id": candidate() }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "provider_not_installed");
}
