//! `media-clips` 端点的 HTTP 契约测试，**真实 PostgreSQL + 真实文件**。
//!
//! # 为什么这一批必须同时有真库与真文件
//!
//! 列表端点的核心语义是「过滤掉产物失效的片段并回收」，而判定要看磁盘：
//! 文件在不在、字节数对不对得上。用 mock 仓储或空目录替身的话，所有片段都会
//! 被判成无效，于是「返回 200」照样通过，而真实行为（`total` 少算、
//! 数据被删）完全测不到。
//!
//! 所以这里每个用例都：**建产物文件 → 造库行 → 打 HTTP → 回读库与磁盘**。
//!
//! # 配置要自己写
//!
//! 片段产物根目录来自配置的 `media.media_clip_root_path`。测试必须把它指向
//! 自己建的临时目录，否则会用默认的 `/data/media-clips`（不存在），于是每个
//! 片段都被判无效 —— 那样测的就不是端点，而是一个永远空的列表。

use std::fs;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewMediaClip, NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "clip-http-secret";

/// 片段产物根目录 + 配置文件路径。`Drop` 时清理两者。
struct Fixture {
    root: PathBuf,
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-clip-http-{tag}-{}", unique()));
        let root = base.join("clips");
        fs::create_dir_all(&root).expect("建片段根目录");
        // 契约要求根目录已规范化
        let root = root.canonicalize().expect("规范化片段根目录");

        // 配置里必须同时给签名密钥（否则 stream_url 签不出可验证的串）
        // 与片段根目录 —— 后者在 `media` 段下。
        let config_path = base.join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[auth]\nfile_signature_secret = \"{SECRET}\"\n\n\
                 [media]\nmedia_clip_root_path = \"{}\"\n",
                root.display()
            ),
        )
        .expect("写测试配置");

        Self { root, config_path }
    }

    /// 在根目录下写一个产物文件，返回 (相对路径, 字节数)。
    fn write_artifact(&self, relative: &str, bytes: &[u8]) -> (String, i64) {
        let target = self.root.join(relative);
        fs::create_dir_all(target.parent().expect("有父目录")).expect("建父目录");
        fs::write(&target, bytes).expect("写产物");
        (
            relative.to_owned(),
            i64::try_from(bytes.len()).expect("长度转 i64"),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.config_path.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

async fn seed_token(db: &Db) -> String {
    let users = UserRepository::new(db.clone());
    let user = users
        .insert(&NewUser {
            username: format!("clip{}", unique()),
            // 仓储只校验非空，不解析 PHC
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

fn app(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        ConfigService::new(fixture.config_path.clone()),
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

/// 造一个产物完好的片段，返回库里的 id。
async fn seed_clip(db: &TestDb, fixture: &Fixture, movie: &str, title: &str) -> i32 {
    let (file_path, size) =
        fixture.write_artifact(&format!("{movie}/{}.mp4", unique()), b"clip-body");
    insert(db, movie, title, &file_path, size).await
}

/// 造一个产物缺失的片段：库里有行，磁盘上没有文件。
async fn seed_clip_without_artifact(db: &TestDb, movie: &str) -> i32 {
    insert(db, movie, "", &format!("{movie}/{}.mp4", unique()), 999).await
}

async fn insert(
    db: &TestDb,
    movie: &str,
    title: &str,
    file_path: &str,
    file_size_bytes: i64,
) -> i32 {
    sm_db::repo::MediaClipRepository::new(db.pool().clone())
        .insert(&NewMediaClip {
            // 全部孤立片段：本文件只测端点契约与回收语义，不涉及封面，
            // 而造 media 需要先建 media_library。
            media_id: None,
            movie_number: Some(movie.to_owned()),
            start_offset_seconds: 0,
            end_offset_seconds: 30,
            title: title.to_owned(),
            file_path: file_path.to_owned(),
            file_size_bytes,
            duration_seconds: 30,
        })
        .await
        .expect("插入片段")
        .id
}

async fn clip_count(db: &TestDb) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_clip")
        .fetch_one(db.pool())
        .await
        .expect("数片段")
}

// ------------------------------------------------------------------ 列表

/// 列表端点的字段集与分页壳必须与上游一致。
#[tokio::test]
async fn the_list_endpoint_returns_the_upstream_shape() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "标题").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    // 分页壳
    for key in ["items", "page", "page_size", "total"] {
        assert!(body.get(key).is_some(), "分页壳缺 {key}: {body}");
    }
    assert_eq!(body["page"], 1, "默认页码");
    assert_eq!(body["page_size"], 20, "默认每页条数");

    let items = body["items"].as_array().expect("items 是数组");
    let item = items
        .iter()
        .find(|row| row["clip_id"] == id)
        .expect("刚建的片段应出现");
    // 上游 MediaClipResource 的字段集，一个不多一个不少
    for key in [
        "clip_id",
        "media_id",
        "movie_number",
        "start_offset_seconds",
        "end_offset_seconds",
        "title",
        "duration_seconds",
        "file_size_bytes",
        "cover_image",
        "stream_url",
        "created_at",
    ] {
        assert!(item.get(key).is_some(), "缺字段 {key}: {item}");
    }
    assert_eq!(
        item.as_object().expect("是对象").len(),
        11,
        "字段数必须是 11 —— 多一个少一个都是契约变更: {item}"
    );
    assert_eq!(item["title"], "标题");
    assert!(
        item["media_id"].is_null(),
        "孤立片段的 media_id 必须是 null: {item}"
    );
    assert!(
        item["cover_image"].is_null(),
        "孤立片段解析不到封面，必须是 null: {item}"
    );
}

/// `stream_url` 必须是**带签名**的片段流地址。
///
/// 签名缺失不会让端点报错，只会让客户端拿到 403 —— 所以只能断言 URL 形状
/// 与签名参数存在。
#[tokio::test]
async fn the_stream_url_is_a_signed_clip_stream_address() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("stream-url");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "t").await;

    let (_, body) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips", &token, None),
    )
    .await;

    let url = body["items"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|row| row["clip_id"] == id)
        .expect("片段在列表里")["stream_url"]
        .as_str()
        .expect("stream_url 是字符串")
        .to_owned();

    let expected_prefix = format!("/media-clips/{id}/stream?");
    assert!(
        url.starts_with(&expected_prefix),
        "stream_url 必须是 {expected_prefix}...，实际 {url}"
    );
    assert!(url.contains("expires="), "缺 expires: {url}");
    assert!(url.contains("signature="), "缺 signature: {url}");
}

/// 端点必须**回收**产物失效的片段，并让 `total` 只数有效的。
#[tokio::test]
async fn the_list_endpoint_reclaims_clips_whose_artifact_is_gone() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("reclaim");
    let token = seed_token(db.pool()).await;

    seed_clip(&db, &fixture, "AAA-001", "有效").await;
    let doomed = seed_clip_without_artifact(&db, "BBB-002").await;
    assert_eq!(clip_count(&db).await, 2, "前提：库里有两行");

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"], 1,
        "total 必须是过滤后的 1（不是 COUNT(*) 的 2）: {body}"
    );
    let items = body["items"].as_array().expect("数组");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["movie_number"], "AAA-001");
    assert_ne!(items[0]["clip_id"], doomed, "被回收的不能出现在结果里");

    // 回收是写副作用，必须真的发生
    assert_eq!(clip_count(&db).await, 1, "失效那一行应当被删掉");
    assert!(
        sm_db::repo::MediaClipRepository::new(db.pool().clone())
            .find_by_id(doomed)
            .await
            .expect("回读")
            .is_none(),
        "被回收的片段在库里不该还在"
    );
}

/// 空库时返回**空数组**而不是 null。
#[tokio::test]
async fn an_empty_database_yields_an_empty_array_not_null() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], json!([]), "必须是空数组: {body}");
    assert_eq!(body["total"], 0);
}

/// `movie_number` 是精确匹配。
#[tokio::test]
async fn movie_number_filters_exactly() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("exact");
    let token = seed_token(db.pool()).await;

    seed_clip(&db, &fixture, "AAA-001", "a").await;
    seed_clip(&db, &fixture, "XAAA-002", "b").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips?movie_number=AAA-001", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "只该命中精确匹配那条: {body}");
    assert_eq!(body["items"][0]["movie_number"], "AAA-001");
}

// ------------------------------------------------------------------ 校验

/// 六种筛选错误共用一个错误码，且都走错误信封。
#[tokio::test]
async fn rejected_filters_share_one_code_inside_the_envelope() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("codes");
    let token = seed_token(db.pool()).await;
    let shared = app(&db, &fixture);

    let cases = [
        ("page=0", "page"),
        ("page=-1", "page"),
        ("page_size=0", "page_size"),
        ("page_size=101", "page_size"),
        ("sort=movie_number%3Aasc", "sort"),
        ("keyword=a%20b%20c%20d%20e%20f%20g", "keyword"),
    ];

    for (query, label) in cases {
        let (status, body) = send(
            shared.clone(),
            authed("GET", &format!("/media-clips?{query}"), &token, None),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} 应当 422，得到 {status}: {body}"
        );
        assert_eq!(
            code_of(&body),
            "invalid_media_clip_filter",
            "{label} 应当用本域错误码，且在信封里"
        );
    }
}

/// `exclude_collection_id` 的 `ge=1`。
///
/// 上游是 FastAPI 的 `Query(ge=1)`，所以 0 是 422 而非「不过滤」。
#[tokio::test]
async fn exclude_collection_id_must_be_at_least_one() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("ge1");
    let token = seed_token(db.pool()).await;
    seed_clip(&db, &fixture, "AAA-001", "a").await;
    let shared = app(&db, &fixture);

    for bad in ["0", "-1"] {
        let (status, body) = send(
            shared.clone(),
            authed(
                "GET",
                &format!("/media-clips?exclude_collection_id={bad}"),
                &token,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "exclude_collection_id={bad} 应当 422: {body}"
        );
    }

    // 正数被接受（哪怕那个合集不存在）
    let (status, body) = send(
        shared,
        authed("GET", "/media-clips?exclude_collection_id=1", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "正数应当被接受: {body}");
}

/// 分页参数越界要**在取数之前**拒绝，所以不产生任何副作用。
#[tokio::test]
async fn a_rejected_filter_produces_no_side_effects() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("no-effect");
    let token = seed_token(db.pool()).await;
    let doomed = seed_clip_without_artifact(&db, "AAA-001").await;
    assert_eq!(clip_count(&db).await, 1);

    let (status, _) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips?sort=bogus", &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(clip_count(&db).await, 1, "被拒的请求不该回收任何片段");
    assert!(
        sm_db::repo::MediaClipRepository::new(db.pool().clone())
            .find_by_id(doomed)
            .await
            .expect("回读")
            .is_some(),
        "被拒的请求不该动数据"
    );
}

// ------------------------------------------------------------------ 鉴权

/// 每个端点都必须 401。
///
/// 逐个端点断言而不是抽查 —— 挂错 `CurrentUser` 是这类改动最常见的漏，
/// 而漏一个端点不会让其它端点失败。
#[tokio::test]
async fn every_clip_endpoint_requires_authentication() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("auth");
    seed_token(db.pool()).await; // 用户存在，但请求不带 token

    let requests: Vec<(&str, &str)> = vec![
        ("GET", "/media-clips"),
        ("GET", "/media-clips/1"),
        ("PATCH", "/media-clips/1"),
        ("DELETE", "/media-clips/1"),
        ("GET", "/media-clips/1/thumbnails"),
        ("GET", "/media/1/clips"),
    ];

    for (method, uri) in requests {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(br#"{"title":"x"}"#.to_vec()))
            .unwrap();
        let (status, body) = send(app(&db, &fixture), request).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} 应当 401，得到 {status}: {body}"
        );
        assert_eq!(code_of(&body), "unauthorized", "{method} {uri}");
    }
}

/// 未注册的 `POST /media/{id}/clips` 必须 405。
///
/// 它需要 ffmpeg，本批**刻意不做**。注意是 **405 而不是 404**：路径本身是
/// 存在的（`GET` 能匹配），只是这个方法没注册。404 会让客户端以为路径写错了，
/// 而真实原因是「这个功能还没做」。
///
/// 也不能注册一个必然 500 的占位 handler —— 那比 405 更坏，客户端会以为
/// 功能存在。
#[tokio::test]
async fn the_unimplemented_create_endpoint_is_405() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("create-405");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            "/media/1/clips",
            &token,
            Some(json!({"start_thumbnail_id": 1, "end_thumbnail_id": 2})),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "未实现的方法应是 405: {body}"
    );
    assert_eq!(code_of(&body), "http_error");
}

// ------------------------------------------------------------------ 详情 / 更新 / 删除

#[tokio::test]
async fn the_detail_endpoint_returns_the_two_extra_arrays() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("detail");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "标题").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", &format!("/media-clips/{id}"), &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    // 列表项的字段全部摊平在这里
    for key in ["clip_id", "title", "stream_url", "created_at"] {
        assert!(body.get(key).is_some(), "详情缺 {key}: {body}");
    }
    // 外加两个数组
    assert_eq!(body["preview_frames"], json!([]), "孤立片段没有预览帧");
    assert_eq!(body["collections"], json!([]), "没加入合集时是空数组");
    assert!(
        body.get("base").is_none(),
        "base 必须被摊平，不能出现在响应里"
    );
}

#[tokio::test]
async fn an_unknown_clip_is_404_with_the_clip_id_echoed() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("404");
    let token = seed_token(db.pool()).await;
    let shared = app(&db, &fixture);

    for uri in [
        "/media-clips/2147483647",
        "/media-clips/2147483647/thumbnails",
    ] {
        let (status, body) = send(shared.clone(), authed("GET", uri, &token, None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert_eq!(code_of(&body), "media_clip", "{uri}: {body}");
        assert_eq!(
            body["error"]["details"]["clip_id"], 2147483647_i64,
            "{uri}: details 要回显 clip_id"
        );
    }
}

#[tokio::test]
async fn updating_the_title_strips_it() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("patch");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "旧标题").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/media-clips/{id}"),
            &token,
            Some(json!({"title": "  新标题  "})),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body["title"], "新标题", "标题两端空白要被裁掉");
}

#[tokio::test]
async fn an_update_without_a_title_is_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("patch-bad");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "旧").await;

    // 上游是 `title: str`（必填无默认），所以缺字段要 422。
    // 若给 DTO 加了 `#[serde(default)]`，这里会变成「清空标题」并返回 200。
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/media-clips/{id}"),
            &token,
            Some(json!({})),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "缺 title 应当 422，得到 {status}: {body}"
    );
    assert_eq!(code_of(&body), "validation_error");
}

#[tokio::test]
async fn an_empty_title_is_allowed_and_clears_it() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("patch-empty");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "旧标题").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "PATCH",
            &format!("/media-clips/{id}"),
            &token,
            Some(json!({"title": ""})),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "清空标题是合法编辑: {body}");
    assert_eq!(body["title"], "", "标题应被清空");
}

#[tokio::test]
async fn deleting_removes_the_row() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("delete");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "t").await;

    // 先确认存在
    let (before, _) = send(
        app(&db, &fixture),
        authed("GET", &format!("/media-clips/{id}"), &token, None),
    )
    .await;
    assert_eq!(before, StatusCode::OK, "前提：片段存在");

    let (status, body) = send(
        app(&db, &fixture),
        authed("DELETE", &format!("/media-clips/{id}"), &token, None),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "删除应 204: {body}");
    assert!(body.is_null(), "204 不该有响应体，实际 {body}");
    assert_eq!(clip_count(&db).await, 0, "库行必须被删");
}

#[tokio::test]
async fn a_method_other_than_get_on_thumbnails_is_405() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("thumb-405");
    let token = seed_token(db.pool()).await;
    let id = seed_clip(&db, &fixture, "AAA-001", "t").await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/media-clips/{id}/thumbnails"),
            &token,
            Some(json!({})),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "响应: {body}");
    assert_eq!(code_of(&body), "http_error");
}

// ------------------------------------------------------------------ 串流

/// 串流测试用的文件内容。20 字节，便于手算区间。
const STREAM_BODY: &[u8] = b"0123456789abcdefghij";

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 造一个「产物完好且文件真实存在」的片段，返回 (db, fixture, clip_id)。
async fn seed_streamable() -> (TestDb, Fixture, i32) {
    let db = TestDb::require().await;
    let fixture = Fixture::new("stream");
    let (file_path, size) =
        fixture.write_artifact(&format!("AAA-001/{}.mp4", unique()), STREAM_BODY);
    let id = insert(&db, "AAA-001", "t", &file_path, size).await;
    (db, fixture, id)
}

/// 签一个有效 URL 并按需带 `Range` 头，返回 (状态码, 响应头, 响应体字节)。
async fn get_stream(
    db: &TestDb,
    fixture: &Fixture,
    clip_id: i32,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    // **必须是将来的** expires：上游先判 `expires <= now` -> 过期，
    // 那个检查在签名比对之前。所以拿 `now` 当 expires 会得到
    // `file_signature_expired`，根本走不到签名比对那一步。
    let expires = now_seconds() + 300;
    let sig = sm_core::signing::clip_signature(SECRET, clip_id, expires);
    let uri = format!("/media-clips/{clip_id}/stream?expires={expires}&signature={sig}");
    stream_with_range(db, fixture, &uri, range).await
}

/// 自己给完整 URI（用于测参数缺失的分支）。
async fn stream_with_range(
    db: &TestDb,
    fixture: &Fixture,
    uri: &str,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(value) = range {
        builder = builder.header("Range", value);
    }
    let response = app(db, fixture)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .expect("oneshot 失败");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
        .to_bytes();
    (status, headers, bytes.to_vec())
}

/// 不带 `Range` 头，响应体当 JSON 解析（错误分支返回的是信封）。
async fn stream_raw(db: &TestDb, fixture: &Fixture, uri: &str) -> (StatusCode, Value) {
    send(
        app(db, fixture),
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// 这个端点是**唯一不取 `CurrentUser` 的 clip 端点** —— 签名就是它的授权凭证。
///
/// 播放器在 `<video src>` 里拿到的就是这个 URL，浏览器不会带 Authorization
/// 头。所以「所有端点都要鉴权」对它不成立，而这条断言是为了防止将来有人给它
/// 加上 `CurrentUser` —— 加上之后视频就彻底播不了了。
#[tokio::test]
async fn the_stream_endpoint_works_without_a_bearer_token() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("stream-noauth");
    let (file_path, size) =
        fixture.write_artifact(&format!("AAA-001/{}.mp4", unique()), STREAM_BODY);
    let id = insert(&db, "AAA-001", "t", &file_path, size).await;

    // 从列表端点取回真正的签名 URL —— 验证的是「客户端能用它播」，
    // 而不是「我们自己造的 URL 能用」。
    let token = seed_token(db.pool()).await;
    let (_, list) = send(
        app(&db, &fixture),
        authed("GET", "/media-clips", &token, None),
    )
    .await;
    let url = list["items"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|row| row["clip_id"] == id)
        .expect("片段在列表里")["stream_url"]
        .as_str()
        .expect("stream_url 是字符串")
        .to_owned();

    let (status, _, body) = stream_with_range(&db, &fixture, &url, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "签名有效时应当直接 200，不需要 token；实际体: {:?}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(body, STREAM_BODY.to_vec(), "且内容要与产物一致");
}

/// 无 `Range` 时回 200 全量 + `Accept-Ranges`。
///
/// 少了这一头，播放器**不会**尝试拖动 —— 端点等于不支持 seek。
#[tokio::test]
async fn the_stream_endpoint_serves_the_whole_file_with_accept_ranges() {
    let (db, fixture, id) = seed_streamable().await;
    let (status, headers, body) = get_stream(&db, &fixture, id, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["accept-ranges"], "bytes",
        "必须有 Accept-Ranges: bytes"
    );
    assert_eq!(headers["content-type"], "video/mp4");
    assert_eq!(body, STREAM_BODY.to_vec(), "无 Range 时是全量");
    assert_eq!(
        headers["content-length"]
            .to_str()
            .expect("头是字符串")
            .parse::<usize>()
            .expect("数字"),
        STREAM_BODY.len()
    );
}

/// `Range: bytes=5-9` 回 206 + 精确的字节。
///
/// 这是播放器拖动进度条的实际请求，所以响应必须**恰好**是那 5 个字节 ——
/// 多给或少给都会让播放位置错乱。
#[tokio::test]
async fn a_range_request_returns_exactly_those_bytes() {
    let (db, fixture, id) = seed_streamable().await;
    let (status, headers, body) = get_stream(&db, &fixture, id, Some("bytes=5-9")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "区间请求必须是 206");
    assert_eq!(body, b"56789".to_vec(), "必须是那 5 个字节，不多不少");
    assert_eq!(
        headers["content-range"],
        format!("bytes 5-9/{}", STREAM_BODY.len()),
        "Content-Range 必须是 bytes 5-9/全长"
    );
    assert_eq!(
        headers["content-length"]
            .to_str()
            .expect("头是字符串")
            .parse::<usize>()
            .expect("数字"),
        5,
        "Content-Length 是**区间**长度，不是文件长度 —— 写错会让进度条错乱"
    );
    assert_eq!(
        headers["accept-ranges"], "bytes",
        "206 也要带 Accept-Ranges"
    );
}

/// `bytes=N-`（开区间）是播放器实际发的形态。
#[tokio::test]
async fn an_open_ended_range_runs_to_the_end_of_the_file() {
    let (db, fixture, id) = seed_streamable().await;
    let (status, headers, body) = get_stream(&db, &fixture, id, Some("bytes=15-")).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body, &STREAM_BODY[15..], "从第 15 字节到末尾");
    assert_eq!(
        headers["content-range"],
        format!("bytes 15-19/{}", STREAM_BODY.len())
    );
}

/// 越界区间回 416 + `Content-Range: bytes */全长`。
#[tokio::test]
async fn an_unsatisfiable_range_returns_416_with_the_total_size() {
    let (db, fixture, id) = seed_streamable().await;
    let (status, headers, _body) = get_stream(&db, &fixture, id, Some("bytes=9999-")).await;

    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        headers["content-range"],
        format!("bytes */{}", STREAM_BODY.len())
    );
    assert_eq!(
        headers["accept-ranges"], "bytes",
        "416 也要带 Accept-Ranges"
    );
}

/// 终点越界要**夹**到末尾而不是 416 —— 播放器估算的结束位置常常超一点。
#[tokio::test]
async fn an_end_past_the_file_is_clamped() {
    let (db, fixture, id) = seed_streamable().await;
    let (status, headers, body) = get_stream(&db, &fixture, id, Some("bytes=15-99999")).await;

    assert_eq!(
        status,
        StatusCode::PARTIAL_CONTENT,
        "终点越界应夹住，不是 416"
    );
    assert_eq!(body, &STREAM_BODY[15..]);
    assert_eq!(
        headers["content-range"],
        format!("bytes 15-19/{}", STREAM_BODY.len())
    );
}

/// 签名缺失 / 空串 / 不匹配 / 过期，各 403 且错误码不同。
///
/// 四个分开断言 —— 客户端靠 `code` 区分「刷新 URL」与「URL 被篡改」，
/// 合并成一个码就失去了意义。
#[tokio::test]
async fn every_signature_failure_mode_is_403_with_its_own_code() {
    let (db, fixture, id) = seed_streamable().await;
    let now = now_seconds();

    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?signature=x"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "缺 expires: {body}");
    assert_eq!(code_of(&body), "file_signature_invalid", "缺 expires");

    // 下面三条都刻意用**未来的** expires：过期检查在签名比对之前，
    // 用 `now` 会全部落到 expired 分支，测不到「参数缺失」与「签名不匹配」。
    let future = now + 300;
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?expires={future}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "缺 signature: {body}");
    assert_eq!(code_of(&body), "file_signature_invalid", "缺 signature");

    // 空 signature —— 上游条件是 `not signature`，空串同样算缺失
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?expires={future}&signature="),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "空 signature: {body}");
    assert_eq!(code_of(&body), "file_signature_invalid", "空 signature");

    let bad = "deadbeef".repeat(8);
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?expires={future}&signature={bad}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "签名不匹配: {body}");
    assert_eq!(code_of(&body), "file_signature_invalid", "签名不匹配");

    let expired = now - 60;
    let sig = sm_core::signing::clip_signature(SECRET, id, expired);
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?expires={expired}&signature={sig}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "过期: {body}");
    assert_eq!(
        code_of(&body),
        "file_signature_expired",
        "过期必须是独立的错误码"
    );
}

/// **验签在查库之前。**
///
/// 签名无效的请求不该碰数据库 —— 那是把一个无鉴权的入口接到库上。
/// 用「片段不存在但签名有效」这条固定顺序：它返回 404，说明验签已经通过、
/// 才走到了查库那步。
#[tokio::test]
async fn the_signature_is_checked_before_the_database() {
    let (db, fixture, id) = seed_streamable().await;
    let now = now_seconds();

    let ghost = id + 9999;
    let expires = now + 300;
    let sig = sm_core::signing::clip_signature(SECRET, ghost, expires);
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{ghost}/stream?expires={expires}&signature={sig}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "签名有效但片段不存在 -> 404，说明验签已通过: {body}"
    );
    assert_eq!(code_of(&body), "file_not_found", "上游是 file_not_found");
}

/// 产物不存在（签名有效）回 404 `file_not_found`。
///
/// 注意错误码**不是** `media_clip` —— 上游串流路径用 `require_existing_file`，
/// 它抛的是 `file_not_found`。
#[tokio::test]
async fn a_missing_artifact_is_404_file_not_found() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("stream-404");
    // 只造库行，不写产物文件
    let id = insert(
        &db,
        "AAA-001",
        "t",
        &format!("AAA-001/{}.mp4", unique()),
        999,
    )
    .await;
    let expires = now_seconds() + 300;
    let sig = sm_core::signing::clip_signature(SECRET, id, expires);
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?expires={expires}&signature={sig}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "产物不存在: {body}");
    assert_eq!(code_of(&body), "file_not_found");
}

/// 路径指向一个**目录**时也必须 404 —— `metadata().len()` 对目录会给出
/// 一个非零值（Linux 上是 4096），若只判「存在」就会去读目录并出错。
#[tokio::test]
async fn a_directory_is_not_streamable() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("stream-dir");
    let rel = format!("AAA-001/{}.mp4", unique());
    std::fs::create_dir_all(fixture.root.join(&rel)).expect("建同名目录");
    let id = insert(&db, "AAA-001", "t", &rel, 10).await;

    let expires = now_seconds() + 300;
    let sig = sm_core::signing::clip_signature(SECRET, id, expires);
    let (status, body) = stream_raw(
        &db,
        &fixture,
        &format!("/media-clips/{id}/stream?expires={expires}&signature={sig}"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "目录不可串流: {body}");
}

/// stream 路径上的其他方法是 405。
#[tokio::test]
async fn a_method_other_than_get_on_stream_is_405() {
    let (db, fixture, id) = seed_streamable().await;
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "POST",
            &format!("/media-clips/{id}/stream"),
            "",
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "响应: {body}");
    assert_eq!(code_of(&body), "http_error");
}
