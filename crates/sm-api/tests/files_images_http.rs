//! `/files/images/*` 的 HTTP 契约测试 —— 签名图片文件的字节服务。
//!
//! # 为什么必须同时有真文件与真包
//!
//! 这个端点最容易被写错的不是验签，而是**「字节从哪来」**：路径按约定属于
//! `assets.zip` / `thumbnails.zip` 时要从包内取条目，条目缺失才回退单文件。
//! 没有真包的话，「包优先」这条判据测不到 —— 只塞一个 loose 文件，两边实现
//! （包优先 / 单文件）都返回 200，静默分不出对错。
//!
//! # 三条盯着不放的语义
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 缺 `expires`/`signature` | 403 `file_signature_invalid` | 上游 `require_signed_params`，**不是** 400 |
//! | 文件不在 | 404 `file_not_found` | 上游 `require_existing_file`，**不是** 500 |
//! | 路径含 `..` | 403 `file_path_invalid` | 归一化在过期/签名比对**之前** |
//!
//! 另有一条 `Content-Type` 判据：上游 `mimetypes.guess_type` 给出 `image/png`
//! 之类，认不出才退 `application/octet-stream`。写死 `octet-stream` 会让浏览器
//! 把图片当附件下载。

use std::fs;
use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::catalog::image_store;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "image-http-secret";
/// 单文件内容。
const LOOSE: &[u8] = b"\x89PNG\r\n\x1a\n loose-bytes";
/// 包内同一条目的内容 —— 与 [`LOOSE`] **不同**，用来证明取的是包那一份。
const PACKED: &[u8] = b"packed-bytes";

/// 图片根 + 配置文件。`Drop` 时清理。
struct Fixture {
    root: PathBuf,
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-img-http-{tag}-{}", unique()));
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

    /// 把字节写进图片根下的相对路径，返回相对路径（供签名）。
    fn write_loose(&self, relative: &str, bytes: &[u8]) -> String {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().expect("有父目录")).expect("建目录");
        fs::write(&path, bytes).expect("写图片");
        relative.to_owned()
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
            username: format!("img{}", unique()),
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

/// 签一个图片 URL（真密钥、当前时刻的窗口对齐过期）。
fn signed_url(relative: &str) -> String {
    sm_core::signing::build_signed_image_url(SECRET, relative, Utc::now().timestamp())
        .expect("签名图片 URL")
}

/// ★ 有效签名 + 单文件 → 200 + 上游那个 Content-Type 与长缓存头。
#[tokio::test]
async fn a_signed_url_serves_the_loose_file() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("loose");
    let relative = fixture.write_loose("movies/ab/ABC-001/1.png", LOOSE);
    let token = seed_token(db.pool()).await;

    let (status, headers, bytes) =
        send_raw(app(&db, &fixture), authed(&signed_url(&relative), &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("image/png"),
        "按扩展名推断（上游 mimetypes.guess_type）"
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=2592000, immutable"),
        "图片按路径不可变，给与签名轮换无关的长缓存"
    );
    assert_eq!(bytes, LOOSE, "字节原样返回");
}

/// ★ 包优先：同一条目在 `assets.zip` 里存在时，取包那一份而不是 loose 文件。
#[tokio::test]
async fn the_pack_is_preferred_over_the_loose_file() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("pack");
    let relative = fixture.write_loose("movies/ab/ABC-001/1.png", LOOSE);
    // `movies/<shard>/<番号>/<name>` 的包与目录同级同名 `assets.zip`，条目名是文件名。
    let pack_path = fixture.root.join("movies/ab/ABC-001/assets.zip");
    image_store::write_pack(&pack_path, &[("1.png".to_owned(), PACKED.to_vec())]).expect("写包");
    let token = seed_token(db.pool()).await;

    let (status, _, bytes) =
        send_raw(app(&db, &fixture), authed(&signed_url(&relative), &token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, PACKED, "包存在时包是权威，loose 只是兜底");
}

/// ★ 签名不对 → 403（不是 401：认证过了，是 URL 不认）。
#[tokio::test]
async fn a_tampered_signature_is_forbidden() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("tamper");
    let relative = fixture.write_loose("movies/ab/ABC-001/1.png", LOOSE);
    let token = seed_token(db.pool()).await;

    let expires = 4_000_000_000_i64;
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            &format!("/files/images/{relative}?expires={expires}&signature=deadbeef"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        code_of(&body).starts_with("file_signature"),
        "码来自 sm-core 那张表：{body}"
    );
}

/// ★ 缺参数（含空串 signature）→ 403 `file_signature_invalid`。
#[tokio::test]
async fn missing_signature_params_are_forbidden() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("noparams");
    let relative = fixture.write_loose("movies/ab/ABC-001/1.png", LOOSE);
    let token = seed_token(db.pool()).await;

    for uri in [
        format!("/files/images/{relative}"),
        format!("/files/images/{relative}?expires=4000000000"),
        format!("/files/images/{relative}?signature=deadbeef"),
        // 上游是 `not signature`：空串算没给。
        format!("/files/images/{relative}?expires=4000000000&signature="),
    ] {
        let (status, body) = send(app(&db, &fixture), authed(&uri, &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(code_of(&body), "file_signature_invalid", "{uri}");
    }
}

/// ★ 签名有效但文件不在（包与单文件都没有）→ 404 `file_not_found`（不是 500）。
#[tokio::test]
async fn a_missing_file_is_404_file_not_found() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("gone");
    let token = seed_token(db.pool()).await;

    // 签名对（真密钥 + 真路径），但磁盘上没有这个文件。
    let url = signed_url("movies/ab/ABC-001/does-not-exist.png");
    let (status, body) = send(app(&db, &fixture), authed(&url, &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code_of(&body), "file_not_found");
}

/// ★ 路径含 `..` → 403 `file_path_invalid`，**在签名比对之前**就被拒。
///
/// 归一化先于过期/签名检查（上游 `verify_image_signature` 的顺序），所以即使
/// 签名是垃圾，回的也是路径那条码。
#[tokio::test]
async fn a_traversal_path_is_rejected_before_the_signature() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("escape");
    let token = seed_token(db.pool()).await;

    let expires = 4_000_000_000_i64;
    let (status, body) = send(
        app(&db, &fixture),
        authed(
            &format!("/files/images/a/../b.png?expires={expires}&signature=deadbeef"),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(code_of(&body), "file_path_invalid");
}
