//! `POST /system/jobs/{task_key}/run` 的 HTTP 契约测试，**真实 PostgreSQL**。
//!
//! # 钉住的两条
//!
//! | 用例 | 断言 | 为什么重要 |
//! |---|---|---|
//! | 已知 key | 202 + `state=pending`，库里多一行 `manual` | 手动触发的唯一通路（`manual_only` 任务没有 cron） |
//! | 未知 key | 404 `job_not_found` | 目录里没有就不能编一个出来跑 |

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{BackgroundTaskRunRepository, NewUser, UserRepository};
use sm_db::system::activity::QUEUE_MUTEX_PREFIX;
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::{ConfigService, JobCatalog, JobCatalogEntry};
use tower::ServiceExt;

const SECRET: &str = "jobs-http-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 一个 `manual_only` 的任务 —— 它没有 cron，手动触发是它唯一的出路。
fn entry(task_key: &str) -> JobCatalogEntry {
    JobCatalogEntry {
        task_key: task_key.to_owned(),
        log_name: task_key.to_owned(),
        cli_name: task_key.to_owned(),
        cli_help: "手动任务".to_owned(),
        plugin_id: None,
        cron_setting: None,
        cron_expr: None,
        manual_trigger_allowed: true,
        has_params_schema: false,
    }
}

struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-jobs-{tag}-{}", unique()));
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

async fn state(db: &Db, config_path: &Path) -> AppState {
    AppState::new(
        db.clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(config_path.to_path_buf()),
    )
    .with_jobs(JobCatalog::new(vec![entry("demo.manual")]))
}

fn post(token: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .expect("构造请求")
}

async fn token(db: &Db) -> String {
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: format!("jobs{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

#[tokio::test]
async fn triggering_a_known_task_enqueues_a_manual_run() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("trigger");
    let app_state = state(db.pool(), &fixture.config_path).await;
    let token = token(db.pool()).await;

    let response = router(app_state)
        .oneshot(post(&token, "/system/jobs/demo.manual/run"))
        .await
        .expect("oneshot 失败");
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let body: Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes(),
    )
    .expect("响应是 JSON");
    assert_eq!(body["task_key"], json!("demo.manual"));
    assert_eq!(body["state"], json!("pending"));

    // 库里真的多了一行，且形状与上游一致：trigger_type=manual、
    // mutex_key=aps:<task_key>。
    let run = BackgroundTaskRunRepository::new(db.pool().clone())
        .find_by_mutex_key(&format!("{QUEUE_MUTEX_PREFIX}demo.manual"))
        .await
        .expect("查询")
        .expect("应当入队一行");
    assert_eq!(run.trigger_type, "manual");
    assert_eq!(run.task_name, "手动任务", "展示名取 cli_help");
}

#[tokio::test]
async fn an_unknown_task_key_is_404_not_a_queue_entry() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("unknown");
    let app_state = state(db.pool(), &fixture.config_path).await;
    let token = token(db.pool()).await;

    let response = router(app_state)
        .oneshot(post(&token, "/system/jobs/nope/run"))
        .await
        .expect("oneshot 失败");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let body: Value = serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes(),
    )
    .expect("响应是 JSON");
    assert_eq!(body["error"]["code"], json!("job_not_found"));
}
