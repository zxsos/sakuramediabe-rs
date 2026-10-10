//! `GET /download-tasks` 与 `POST /download-tasks/{task_id}/import` 的 HTTP 契约测试
//! （**真实 PostgreSQL**）。
//!
//! # 这层测什么
//!
//! service 层的纯规则（状态白名单、排序白名单、分页、可导入判据）已由
//! `sm-service/src/transfers/{download_common,download_task}.rs` 的单测钉住。
//! 这一层测的是**契约翻译**：
//!
//! - **响应字段集合与数量** —— 上游 `DownloadTaskResource` 是 14 个键，
//!   多一个少一个都是契约变更（骨架期那版多了 `importable` / `client_name`、
//!   少了 `name` / `remote_id` / `import_status` / 封面）；
//! - **重复 query 参数** `?state=a&state=b` —— `state` 是 `list[str]`，而
//!   axum 自带的 `Query`（`serde_urlencoded`）根本不支持序列，所以这条
//!   在 HTTP 层才有意义；
//! - 错误信封的状态码与 code（`422 invalid_download_task_filter`、
//!   `404 download_task_not_found`、`422 invalid_download_task_import`、
//!   `409 download_task_import_conflict`）；
//! - `movie_number` 是**精确匹配**（不是 `LIKE`）。
//!
//! ⚠️ **仍不覆盖 202 正常路径**，但理由变了：入队（
//! `ImportTaskService::enqueue`）已经落地，能真的建出 TaskRun；卡住的是**执行**
//! —— `library_import` 的 worker 处理器还没注册，那条 run 会被领取后以
//! `NoHandler` 判失败，而「入队成功之后」的状态断言要连 worker 一起起。
//! 所以这里仍然只钉两道门。
//!
//! 每个用例的 `TestDb` 是**独立 schema**，互不污染。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, ImageRepository, MediaLibraryRepository,
    MovieRepository, NewDownloadClient, NewDownloadTask, NewImage, NewMediaLibrary, NewMovie,
    NewUser, UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "download-tasks-http-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 只提供一个可写的临时配置（带签名密钥），避免碰到真实的 `config.toml`。
struct Fixture {
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-download-tasks-{tag}-{}", unique()));
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

async fn seed_token(db: &Db) -> String {
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: format!("dt{}", unique()),
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

fn app(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(fixture.config_path.clone()),
    ))
}

fn authed(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
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

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

/// 造一个媒体库 + 一个下载器，返回 client id。
async fn seed_client(db: &TestDb) -> i32 {
    let library = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("dt-lib-{}", unique()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id;
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("dt-client-{}", unique()),
            provider_config: None,
            library_id: library,
        })
        .await
        .expect("insert download client")
        .id
}

/// 造一条下载任务（`state` / `import_status` 走 DDL 默认：`queued` / `pending`）。
async fn seed_task(db: &TestDb, client_id: i32, movie_number: Option<&str>) -> i32 {
    DownloadTaskRepository::new(db.pool().clone())
        .insert(&NewDownloadTask {
            client_id,
            remote_id: format!("remote-{}", unique()),
            name: "some release".to_owned(),
            movie_number: movie_number.map(str::to_owned),
        })
        .await
        .expect("insert download task")
        .id
}

/// 造一部带（竖）封面的影片，返回 `(cover_image_id, thin_cover_image_id)`。
async fn seed_movie_with_covers(db: &TestDb, number: &str, title: &str) -> (i32, i32) {
    let images = ImageRepository::new(db.pool().clone());
    let cover = images
        .upsert(&NewImage {
            origin: format!("dt/cover-{}.webp", unique()),
        })
        .await
        .expect("upsert cover")
        .0;
    let thin = images
        .upsert(&NewImage {
            origin: format!("dt/thin-{}.webp", unique()),
        })
        .await
        .expect("upsert thin cover")
        .0;
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.to_owned(),
            title: title.to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: None,
            duration_minutes: 120,
            score: 0.0,
            score_number: 0,
            series_id: None,
            cover_image_id: Some(cover),
            thin_cover_image_id: Some(thin),
            metadata_source: None,
        })
        .await
        .expect("insert movie");
    (cover, thin)
}

/// ★ 响应形状 = 上游 `DownloadTaskResource`（**14 个键**，一个不多一个不少）。
///
/// 骨架期这里是自造形状：多了 `importable` / `client_name`，少了 `name` /
/// `remote_id` / `import_status` / `movie_title` / 两个封面 / `updated_at` ——
/// 客户端的 `DownloadTaskDto` 会把这些全解析成 `null`。
#[tokio::test]
async fn the_task_list_matches_the_upstream_resource_shape() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;
    let client_id = seed_client(&db).await;
    let (cover_id, thin_id) = seed_movie_with_covers(&db, "DT-001", "  标题  ").await;
    let task_id = seed_task(&db, client_id, Some("DT-001")).await;

    let (status, body) = send(app(&db, &fixture), authed("GET", "/download-tasks", &token)).await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["page"], json!(1), "缺省 page = 1");
    assert_eq!(body["page_size"], json!(20), "缺省 page_size = 20");
    assert_eq!(body["total"], json!(1));
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    let task = &items[0];

    let mut keys: Vec<&str> = task
        .as_object()
        .expect("任务项是对象")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "client_id",
            "created_at",
            "id",
            "import_status",
            "import_status_label",
            "movie_cover",
            "movie_number",
            "movie_thin_cover",
            "movie_title",
            "name",
            "progress",
            "remote_id",
            "state",
            "updated_at",
        ],
        "任务项的字段集合变了（上游 DownloadTaskResource 是 14 个键）"
    );

    assert_eq!(task["id"], json!(task_id));
    assert_eq!(task["client_id"], json!(client_id));
    assert_eq!(task["movie_number"], json!("DT-001"));
    assert_eq!(task["name"], json!("some release"));
    assert!(task["remote_id"].as_str().is_some_and(|v| !v.is_empty()));
    assert_eq!(task["state"], json!("queued"));
    assert_eq!(task["progress"], json!(0.0));
    assert_eq!(task["import_status"], json!("pending"));
    // 上游 `(movie.title or "").strip() or None` —— 首尾空白被去掉。
    assert_eq!(task["movie_title"], json!("标题"));
    // computed：中文说明（映射在 `sm_api::dto::describe_import_status`）。
    assert_eq!(
        task["import_status_label"],
        json!("待导入：下载已完成，等待自动导入触发")
    );
    // 封面是**签名后**的 URL。
    assert_eq!(task["movie_cover"]["id"], json!(cover_id));
    assert_eq!(task["movie_thin_cover"]["id"], json!(thin_id));
    for key in ["movie_cover", "movie_thin_cover"] {
        let origin = task[key]["origin"].as_str().expect("origin 是字符串");
        assert!(
            origin.starts_with("/files/images/"),
            "{key}.origin 应是签名后的 URL，实际 {origin:?}"
        );
    }
    // 时间戳是 ISO 串（实体里可空）。
    assert!(task["created_at"].as_str().is_some(), "created_at 应是串");
}

/// ★ 没有影片时 `movie_title` / 封面都是 `null` —— 任务早于影片入库是正常流程。
#[tokio::test]
async fn a_task_without_a_movie_gets_nulls_not_an_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("no-movie");
    let token = seed_token(db.pool()).await;
    let client_id = seed_client(&db).await;
    seed_task(&db, client_id, None).await;

    let (status, body) = send(app(&db, &fixture), authed("GET", "/download-tasks", &token)).await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let task = &body["items"][0];
    assert_eq!(task["movie_number"], json!(null));
    assert_eq!(task["movie_title"], json!(null));
    assert_eq!(task["movie_cover"], json!(null));
    assert_eq!(task["movie_thin_cover"], json!(null));
}

/// ★ `state` 是**重复 query 参数**（`list[str]`），**不是 CSV**。
///
/// 这条只有在 HTTP 层才测得出来：axum 自带的 `Query` 走 `serde_urlencoded`，
/// 它把「字段期待序列」转发成 `visit_str`，于是 `?state=a&state=b` 与
/// `?state=a` **都会 422**。路由必须用 `HtmlFormQuery`（`serde_html_form`）。
#[tokio::test]
async fn repeated_state_params_are_honoured() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("states");
    let token = seed_token(db.pool()).await;
    let client_id = seed_client(&db).await;
    let queued = seed_task(&db, client_id, None).await;
    let completed = seed_task(&db, client_id, None).await;
    DownloadTaskRepository::new(db.pool().clone())
        .set_state(completed, "completed", Some(1.0), Some(r#"{"version":1}"#))
        .await
        .expect("置为 completed");

    // 重复键：两个状态都取。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            "/download-tasks?state=queued&state=completed",
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(2), "两个状态都该命中：{body}");

    // 单值也走同一个提取器（不能因此 422）。
    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?state=queued", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(queued));
    assert_eq!(body["items"][0]["state"], json!("queued"));

    // 大小写不敏感（上游 `strip().lower()`）。
    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?state=COMPLETED", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(completed));
}

/// ★ `seeding` / `done` **不在**状态白名单里（上游 `DOWNLOAD_STATES` 只有四个）。
#[tokio::test]
async fn an_unknown_state_is_rejected_with_the_filter_code_and_raw_input() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("bad-state");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?state=seeding", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "invalid_download_task_filter");
    assert_eq!(body["error"]["message"], json!("Invalid state"));
    // details 放**原始**入参（未归一小写），前端据此高亮那个筛选控件。
    assert_eq!(body["error"]["details"]["state"], json!("seeding"));
}

/// ★ 排序键是**六个 `field:dir`** 的白名单；骨架期的 `-field` 语法不再合法。
#[tokio::test]
async fn sort_accepts_field_dir_and_rejects_everything_else() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("sort");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?sort=progress:desc", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?sort=-created_at", &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "invalid_download_task_filter");
    assert_eq!(body["error"]["message"], json!("Invalid sort expression"));
    assert_eq!(body["error"]["details"]["sort"], json!("-created_at"));
}

/// ★ 分页非法用**专用**错误码，不是通用的 `validation_error`。
#[tokio::test]
async fn a_bad_page_is_rejected_with_the_filter_code() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("page");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?page=0", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "invalid_download_task_filter");
    assert_eq!(
        body["error"]["message"],
        json!("page must be greater than 0")
    );
    assert_eq!(body["error"]["details"]["page"], json!(0));
}

/// ★ `movie_number` 是**精确匹配**（上游 `build_task_movie_filter` 是
/// `DownloadTask.movie == value`），**不是 `LIKE`**。
#[tokio::test]
async fn movie_number_is_matched_exactly_not_by_prefix() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("movie-number");
    let token = seed_token(db.pool()).await;
    let client_id = seed_client(&db).await;
    seed_task(&db, client_id, Some("DT-777")).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-tasks?movie_number=DT-777", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["total"], json!(1));

    // 前缀 / 大小写不同都不该命中（上游不做大小写归一，那是影片检索的事）。
    for bogus in ["DT", "DT-77", "dt-777"] {
        let (status, body) = send(
            app(&db, &fixture),
            authed(
                "GET",
                &format!("/download-tasks?movie_number={bogus}"),
                &token,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应: {body}");
        assert_eq!(
            body["total"],
            json!(0),
            "{bogus:?} 不该命中（精确匹配，不是 LIKE）"
        );
    }
}

/// ★ `POST /download-tasks/{id}/import` 的两道门：**404 → 422 → 409**。
///
/// ⚠️ 202 的正常路径不在这里 —— 它要连 worker 一起起（见文件头说明）。
#[tokio::test]
async fn trigger_import_gates_are_404_then_422_then_409() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("import");
    let token = seed_token(db.pool()).await;
    let client_id = seed_client(&db).await;

    // 1. 任务不存在 → 404。
    let (status, body) = send(
        app(&db, &fixture),
        authed("POST", "/download-tasks/999999/import", &token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "响应: {body}");
    assert_eq!(code_of(&body), "download_task_not_found");
    assert_eq!(body["error"]["details"]["task_id"], json!(999999));

    // 2. 下载没完成 → **422**（不是 409）——「本来就不该导入」。
    let downloading = seed_task(&db, client_id, None).await;
    DownloadTaskRepository::new(db.pool().clone())
        .set_state(downloading, "downloading", Some(0.5), None)
        .await
        .expect("置为 downloading");
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/download-tasks/{downloading}/import"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "invalid_download_task_import");
    assert_eq!(body["error"]["details"]["task_id"], json!(downloading));

    // 3. 下载已完成 + 有来源，但导入状态不允许 → **409**（「现在不行」）。
    let running = seed_task(&db, client_id, None).await;
    let tasks = DownloadTaskRepository::new(db.pool().clone());
    tasks
        .set_state(running, "completed", Some(1.0), Some(r#"{"version":1}"#))
        .await
        .expect("置为 completed");
    tasks
        .set_import_status(running, "running", None)
        .await
        .expect("置为 running");
    let (status, body) = send(
        app(&db, &fixture),
        authed("POST", &format!("/download-tasks/{running}/import"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "响应: {body}");
    assert_eq!(code_of(&body), "download_task_import_conflict");
    assert_eq!(body["error"]["details"]["task_id"], json!(running));
    assert_eq!(body["error"]["details"]["import_status"], json!("running"));
}

/// ★ `DELETE` 的两步确认：`delete_files=true` 但没确认 → **422 + 专用码 + task_id**。
///
/// 确认先于查库（那里是 `delete_files` 的判据），所以这个用例不需要真建任务 ——
/// 而这也正是「不在拒绝前泄漏任务是否存在」的副作用。
#[tokio::test]
async fn deleting_files_without_confirmation_is_refused() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("delete");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("DELETE", "/download-tasks/12345?delete_files=true", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "download_task_delete_confirmation_required");
    assert_eq!(
        body["error"]["message"],
        json!("Deleting downloaded files requires explicit confirmation")
    );
    assert_eq!(body["error"]["details"]["task_id"], json!(12345));
}
