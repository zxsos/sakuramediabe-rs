//! 字幕两个端点的 HTTP 契约测试，**真实 PostgreSQL + 真实文件**。
//!
//! | 端点 | 形状 |
//! |---|---|
//! | `GET /movies/{n}/subtitles` | 列表，每项带**签名 URL** |
//! | `GET /files/subtitles/{id}` | 签名校验后返回**字节流**（`text/plain; charset=utf-8`）|
//!
//! # 为什么必须同时有真库与真文件
//!
//! 列表的每一项都要 `stat` 磁盘：库里有记录、文件不在时**跳过**（不是报错）。
//! 没有真文件的话这条判据测不到 —— 「返回 200」照样通过，而死链会被原样返回。
//!
//! # 两条路由的「文件不在」**不是**同一个码（本文件盯着这个）
//!
//! | 入口 | 文件不在 | 理由 |
//! |---|---|---|
//! | 读内容（服务层） | 409 `subtitle_unavailable` | 记录还在，是冲突 |
//! | **下载**（本文件） | 404 `file_not_found` | 上游 `require_existing_file` |
//!
//! 合并两者会让客户端把「字幕被删了」当成「重试就好」。

use std::fs;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{MovieRepository, NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::catalog::media_paths::movie_subtitle_dir;
use sm_service::catalog::subtitle_asset::SubtitleAssetService;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "subtitle-http-secret";
const SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nhello\n";

/// 图片根（字幕目录挂在它下面）+ 配置文件。`Drop` 时清理。
struct Fixture {
    root: PathBuf,
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-sub-http-{tag}-{}", unique()));
        let root = base.join("assets");
        fs::create_dir_all(&root).expect("建图片根");
        let config_path = base.join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[auth]\nfile_signature_secret = \"{SECRET}\"\n\n\
                 [media]\nimport_image_root_path = '{}'\n",
                root.display()
            ),
        )
        .expect("写测试配置");
        Self { root, config_path }
    }

    fn config(&self) -> ConfigService {
        ConfigService::new(self.config_path.clone())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.config_path.parent() {
            let _ = fs::remove_dir_all(base);
        }
        let _ = &self.root;
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
            username: format!("sub{}", unique()),
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
        fixture.config(),
    ))
}

fn authed(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

/// 发请求并**连响应头一起**返回 —— 下载端点验的就是 `Content-Type`。
async fn send_raw(
    router: axum::Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let (status, _, bytes) = send_raw(router, request).await;
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

/// 建影片 + 导一份字幕，返回 (番号, 字幕 id, 字幕文件绝对路径)。
async fn seed_subtitle(db: &TestDb, fixture: &Fixture, tag: &str) -> (String, i32, PathBuf) {
    let movie_number = format!("SUBHTTP-{tag}-{}", unique());
    let repo = MovieRepository::new(db.pool().clone());
    repo.insert(&sm_db::repo::NewMovie {
        movie_number: movie_number.clone(),
        title: movie_number.clone(),
        ..sm_db::repo::NewMovie::default()
    })
    .await
    .expect("插入 movie");

    let config = fixture.config();
    let imported = SubtitleAssetService::new(db.pool(), &config)
        .import_subtitle_content(&movie_number, SRT, "zh.srt", None)
        .await
        .expect("导入字幕");
    assert_eq!(
        imported.status,
        sm_service::catalog::subtitle_asset::SubtitleImportStatus::Imported
    );
    let subtitle_id = imported.subtitle_id.expect("导入成功应有 id");
    let dir = movie_subtitle_dir(&config, &movie_number).expect("字幕目录");
    let path = dir.join(format!("{movie_number}-1.srt"));
    (movie_number, subtitle_id, path)
}

/// ★ 列表：返回番号 + 每项的签名 URL，且**不泄露**服务端路径。
#[tokio::test]
async fn the_listing_returns_signed_urls() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list");
    let (movie_number, subtitle_id, _) = seed_subtitle(&db, &fixture, "list").await;
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(&format!("/movies/{movie_number}/subtitles"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["movie_number"], movie_number);
    let items = body["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["subtitle_id"], subtitle_id);
    assert_eq!(item["file_name"], format!("{movie_number}-1.srt"));
    let url = item["url"].as_str().expect("带 url");
    assert!(
        url.starts_with(&format!("/files/subtitles/{subtitle_id}?")),
        "URL 形状：{url}"
    );
    assert!(
        url.contains("expires=") && url.contains("signature="),
        "{url}"
    );
    // 协议字段表：上游的列表项**没有** format / size_bytes / file_path。
    assert!(item.get("format").is_none(), "别多塞字段：{item}");
    assert!(item.get("size_bytes").is_none(), "别多塞字段：{item}");
    assert!(!body.to_string().contains("assets/"), "不该泄露服务端路径");
}

/// ★ 下载：签名有效 → 200 + 原始字节 + 上游那个 Content-Type。
#[tokio::test]
async fn the_download_serves_the_bytes_with_the_upstream_content_type() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("dl");
    let (movie_number, _, _) = seed_subtitle(&db, &fixture, "dl").await;
    let token = seed_token(db.pool()).await;

    let (_, listing) = send(
        app(&db, &fixture),
        authed(&format!("/movies/{movie_number}/subtitles"), &token),
    )
    .await;
    let url = listing["items"][0]["url"]
        .as_str()
        .expect("带 url")
        .to_owned();

    let (status, headers, bytes) = send_raw(app(&db, &fixture), authed(&url, &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; charset=utf-8"),
        "上游 FileResponse 的 media_type 就是这个"
    );
    assert_eq!(bytes, SRT, "字节原样返回");
}

/// ★ 签名不对 → 403（不是 401：认证过了，是 URL 不认）。
#[tokio::test]
async fn a_tampered_signature_is_forbidden() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("tamper");
    let (movie_number, subtitle_id, _) = seed_subtitle(&db, &fixture, "tamper").await;
    let token = seed_token(db.pool()).await;

    let expires = 4_000_000_000_i64;
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            &format!("/files/subtitles/{subtitle_id}?expires={expires}&signature=deadbeef"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        code_of(&body).starts_with("file_signature"),
        "码来自 sm-core 那张表：{body}"
    );
    let _ = movie_number;
}

/// ★ 缺参数 → 403 `file_signature_invalid`（上游 `require_signed_params`）。
///
/// 不是 400：对客户端来说「URL 不完整」与「签名不对」都是「这个 URL 不认」。
#[tokio::test]
async fn missing_signature_params_are_forbidden() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("noparams");
    let (_, subtitle_id, _) = seed_subtitle(&db, &fixture, "noparams").await;
    let token = seed_token(db.pool()).await;

    for uri in [
        format!("/files/subtitles/{subtitle_id}"),
        format!("/files/subtitles/{subtitle_id}?expires=4000000000"),
        format!("/files/subtitles/{subtitle_id}?signature=deadbeef"),
    ] {
        let (status, body) = send(app(&db, &fixture), authed(&uri, &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(code_of(&body), "file_signature_invalid", "{uri}");
    }
}

/// ★ 番号不存在 → 404 `movie_not_found`，details 带番号。
#[tokio::test]
async fn an_unknown_movie_number_is_404_with_the_number_in_details() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("nomovie");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("/movies/NOPE-000000/subtitles", &token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code_of(&body), "movie_not_found");
    assert_eq!(body["error"]["details"]["movie_number"], "NOPE-000000");
}

/// ★ 文件被删 → 列表跳过它，但**下载**报 404 `file_not_found`（不是 409）。
///
/// 这条与读内容接口的 409 `subtitle_unavailable` 是两种语义，见模块文档。
#[tokio::test]
async fn a_vanished_file_is_skipped_by_the_list_and_404_on_download() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("gone");
    let (movie_number, subtitle_id, path) = seed_subtitle(&db, &fixture, "gone").await;
    let token = seed_token(db.pool()).await;

    let (_, listing) = send(
        app(&db, &fixture),
        authed(&format!("/movies/{movie_number}/subtitles"), &token),
    )
    .await;
    let url = listing["items"][0]["url"]
        .as_str()
        .expect("先有 url")
        .to_owned();

    fs::remove_file(&path).expect("删掉字幕文件");

    // 列表：文件没了 → 跳过（不是报错，也不是给死链）。
    let (_, after) = send(
        app(&db, &fixture),
        authed(&format!("/movies/{movie_number}/subtitles"), &token),
    )
    .await;
    assert_eq!(after["items"].as_array().expect("数组").len(), 0);

    // 下载：签名**仍然有效**（签的是 id 与过期时间），但文件不在 → 404。
    let (status, body) = send(app(&db, &fixture), authed(&url, &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code_of(&body), "file_not_found");
    let _ = subtitle_id;
}

/// ★ 字幕 id 不存在（但签名合法）→ 404 `subtitle_not_found`。
#[tokio::test]
async fn an_unknown_subtitle_id_is_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("noid");
    let token = seed_token(db.pool()).await;

    // 用真密钥签一个**不存在**的 id —— 签名对，记录不在。
    let expires = sm_core::signing::signature_expires(Utc::now().timestamp());
    let signature = sm_core::signing::subtitle_signature(SECRET, 2_000_000_000, expires);
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            &format!("/files/subtitles/2000000000?expires={expires}&signature={signature}"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code_of(&body), "subtitle_not_found");
    let _ = json!({});
}
