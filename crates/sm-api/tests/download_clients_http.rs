//! `GET /download-clients` 与 `DELETE /download-clients/{id}` 的 HTTP 契约测试
//! （真库）。
//!
//! # 这两条**与插件无关**
//!
//! 骨架期把五个端点一起标成「接下载器 provider 插件」，其中这三条是纯库：
//! `GET`（列库）、`DELETE`（判两道 409 后删）。只有 `POST` / `PATCH` /
//! `POST /test` 需要插件声明的配置 schema 与 `prepare_client`（阶段二）。
//!
//! # 响应是**裸数组**，不是分页信封
//!
//! 上游 `routers/transfers/downloads.py:30` 是
//! `response_model=list[DownloadClientResource]` —— 没有 `{items, total}` 那层。
//! 这条最容易「顺手」加错，所以用例里直接断言 `body.is_array()`。

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
    IndexerRepository, MediaLibraryRepository, NewDownloadClient, NewDownloadTask, NewIndexer,
    NewMediaLibrary, NewUser, UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use sm_service::transfers::download_client::{
    DownloadCapabilityRegistry, DownloadClientCapability, DownloadClientConfigField,
    DownloadClientDiagnostic, PreviousClientHandle, ProviderFailureInfo,
};
use tower::ServiceExt;

const SECRET: &str = "download-client-http-secret";

/// 一份最小配置文件；`Drop` 时清掉。
struct Fixture {
    dir: PathBuf,
    config_path: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sm-dc-http-{tag}-{}", unique()));
        fs::create_dir_all(&dir).expect("建临时目录");
        let config_path = dir.join("config.toml");
        fs::write(
            &config_path,
            format!("[auth]\nfile_signature_secret = \"{SECRET}\"\n"),
        )
        .expect("写测试配置");
        Self { dir, config_path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// 一个假的下载能力注册表：只给 `local` 提供能力，字段表声明 `host`（可见）
/// 与 `token`（secret）。
///
/// **响应体里能不能看到 `provider_config` 取决于它**：`_resource` 的字段表来自
/// 插件，拿不到时上游把整份配置换成 `{}`（`client_config_service.py:73-91`）。
/// 所以那条断言「`provider_config` 是对象且带上 `host`」必须先注入本表 ——
/// 与同目录的 `media_libraries_http.rs::FakeRegistry` 是同一个套路。
struct FakeDownloads;

struct FakeCapability;

impl DownloadClientCapability for FakeCapability {
    fn config_fields(&self) -> Vec<DownloadClientConfigField> {
        vec![
            DownloadClientConfigField {
                key: "host".to_owned(),
                input: "text".to_owned(),
                read_only: false,
            },
            DownloadClientConfigField {
                key: "token".to_owned(),
                input: "secret".to_owned(),
                read_only: false,
            },
        ]
    }

    fn prepare_client(
        &self,
        submitted: &Value,
        _library_id: i32,
        _previous: Option<&PreviousClientHandle>,
    ) -> Result<Value, ProviderFailureInfo> {
        Ok(submitted.clone())
    }

    fn test_client(
        &self,
        _submitted: &Value,
        _library_id: i32,
    ) -> Result<DownloadClientDiagnostic, ProviderFailureInfo> {
        Err(ProviderFailureInfo {
            code: "unimplemented".to_owned(),
            message: "FakeCapability 不探测".to_owned(),
        })
    }
}

impl DownloadCapabilityRegistry for FakeDownloads {
    fn download_client_for(
        &self,
        provider_key: &str,
    ) -> Result<Option<Box<dyn DownloadClientCapability>>, ProviderFailureInfo> {
        if provider_key == "local" {
            Ok(Some(Box::new(FakeCapability)))
        } else {
            Ok(None)
        }
    }
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

async fn seed_token(db: &Db) -> String {
    let user = UserRepository::new(db.clone())
        .insert(&NewUser {
            username: format!("dc{}", unique()),
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

/// 带上假下载能力。**只有需要看到 `provider_config` 内容的用例用它** ——
/// 拿不到插件字段表时 `_resource` 会把配置换成 `{}`（见 `FakeDownloads` 的文档）。
fn app_with_downloads(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(
        AppState::new(
            db.pool().clone(),
            AuthConfig::new(SECRET),
            ConfigService::new(fixture.config_path.clone()),
        )
        .with_download_capabilities(Arc::new(FakeDownloads)),
    )
}

fn request(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("构造请求")
}

/// 发请求：返回状态、响应头、**原始字节**（204 没有 body，解析 JSON 会炸）。
async fn send(
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

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|err| {
        panic!(
            "响应体不是 JSON: {err}; 原始: {}",
            String::from_utf8_lossy(bytes)
        )
    })
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", unique()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("插入 media_library")
        .id
}

async fn seed_client(db: &TestDb, library_id: i32, config: Option<&str>) -> i32 {
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("client-{}", unique()),
            provider_config: config.map(str::to_owned),
            library_id,
        })
        .await
        .expect("插入 download_client")
        .id
}

/// ★ 列表：**裸数组**、最新在前、字段是上游那六个（**没有** `kind`/`enabled`/`config`）。
#[tokio::test]
async fn listing_returns_a_bare_array_newest_first() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;
    let first = seed_client(&db, library_id, None).await;
    let second = seed_client(&db, library_id, Some(r#"{"host":"h"}"#)).await;

    let (status, _, bytes) = send(
        app_with_downloads(&db, &fixture),
        request("GET", "/download-clients", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json_of(&bytes);
    let items = body
        .as_array()
        .unwrap_or_else(|| panic!("上游是裸数组，没有分页信封：{body}"));
    let ids: Vec<i64> = items
        .iter()
        .map(|item| item["id"].as_i64().expect("id 是数字"))
        .collect();
    assert!(
        ids.starts_with(&[i64::from(second), i64::from(first)]),
        "最新在前：{ids:?}"
    );

    let item = items
        .iter()
        .find(|item| item["id"].as_i64() == Some(i64::from(second)))
        .expect("第二个在列表里");
    assert_eq!(item["library_id"], library_id);
    assert_eq!(item["provider_config"]["host"], "h");
    for absent in ["kind", "enabled", "config"] {
        assert!(item.get(absent).is_none(), "{absent} 是骨架期自造的字段");
    }
}

/// ★ 删除：**204 无 body**；再删一次 → **404**。
#[tokio::test]
async fn deleting_is_204_then_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("delete");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, None).await;

    let (status, _, bytes) = send(
        app(&db, &fixture),
        request("DELETE", &format!("/download-clients/{client_id}"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(bytes.is_empty(), "204 不带 body");

    let (status, _, bytes) = send(
        app(&db, &fixture),
        request("DELETE", &format!("/download-clients/{client_id}"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code_of(&json_of(&bytes)), "download_client_not_found");
}

/// ★ 名下有任务行 → **409 `download_client_in_use`**，details 带 `client_id`。
///
/// 判据是「有没有**任何**任务行」（含历史），不是「有没有在跑的」—— 任务行会随
/// 下载器 `CASCADE` 删掉，拦的是「你的下载历史要一起没」。
#[tokio::test]
async fn a_client_with_task_rows_is_409_with_the_client_id() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("inuse");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, None).await;

    DownloadTaskRepository::new(db.pool().clone())
        .insert(&NewDownloadTask {
            client_id,
            remote_id: format!("remote-{}", unique()),
            name: "some release".to_owned(),
            // 任务允许早于影片入库 —— 这里正好顺带覆盖 `None` 那条。
            movie_number: None,
        })
        .await
        .expect("插入 download_task");

    let (status, _, bytes) = send(
        app(&db, &fixture),
        request("DELETE", &format!("/download-clients/{client_id}"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body = json_of(&bytes);
    assert_eq!(code_of(&body), "download_client_in_use");
    assert_eq!(body["error"]["details"]["client_id"], client_id);
}

/// ★ 只被索引器绑定 → **409 `download_client_in_use_by_indexers`**。
///
/// 与上一条是**两道不同的 409**：客户端按码给出不同提示（「删任务」vs「解绑」）。
#[tokio::test]
async fn a_client_bound_to_an_indexer_is_409() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("bound");
    let token = seed_token(db.pool()).await;
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, None).await;

    let indexer = IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: format!("idx-{}", unique()),
            url: "http://127.0.0.1:9117".to_owned(),
            kind: "pt".to_owned(),
            api_key: None,
        })
        .await
        .expect("插入 indexer");
    IndexerDownloadClientRepository::new(db.pool().clone())
        .bind(indexer.id, client_id)
        .await
        .expect("绑定");

    let (status, _, bytes) = send(
        app(&db, &fixture),
        request("DELETE", &format!("/download-clients/{client_id}"), &token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body = json_of(&bytes);
    assert_eq!(code_of(&body), "download_client_in_use_by_indexers");
    assert_eq!(body["error"]["details"]["client_id"], client_id);
}
