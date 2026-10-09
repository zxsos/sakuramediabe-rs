//! `/media-libraries*` 的 HTTP 契约测试（真实 PostgreSQL + 内存注册表）。
//!
//! # 为什么带一个假注册表
//!
//! 这五个端点里，**写路径**与 **provider 目录**都取决于注入的插件注册表：
//!
//! - 没注入 → `POST` 一律 **503 `provider_not_installed`**、目录**空表**；
//! - 注入了但 provider **没有库能力** → **422 `provider_library_unsupported`**。
//!
//! 这两条正是骨架期自造 DTO 时看不到的语义 —— 用一个内存注册表把「装了什么」
//! 变成测试参数，比等真插件便宜得多。
//!
//! # 几条盯着不放的判据
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 响应里的 `provider_config` | **剥掉** `input == "secret"` 的键 | 原样发出去 = 泄漏 |
//! | 拿不到字段表时 | `provider_config` 是 **`{}`** 而不是原样 | 不知道哪些是 secret 就别发 |
//! | `POST` 成功 | **201**（不是 200） | 契约 |
//! | `DELETE` 成功 | **204 无 body** | 契约 |
//! | `DELETE` 不存在 | **404**（**不幂等**） | 上游先 `_require_library` |
//! | `DELETE` 被下载器引用 | **409 `media_library_in_use`** | 上游同时看 Media 与 DownloadClient |
//! | `PATCH` 一个字段都没给 | **422 `empty_media_library_update`** | 不是「原样返回」 |

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    DownloadClientRepository, MediaLibraryRepository, NewDownloadClient, NewUser, UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::playback::media_library::{
    LibraryConfigField, LibraryForFuture, MediaLibraryCapability, MediaLibraryRegistry,
    PrepareLibraryFuture, PreparedLibrary, PreviousLibraryHandle, ProviderCatalogEntry,
    SpaceUsageFuture,
};
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use sm_service::transfers::download_client::ProviderFailureInfo;
use tower::ServiceExt;

const SECRET: &str = "media-libraries-http-secret";
const PROVIDER: &str = "fakelib";

/// 注册表的行为模式。
#[derive(Clone, Copy)]
enum Mode {
    /// 装了插件，`PROVIDER` 有库能力。
    Installed,
    /// 装了插件，但 `PROVIDER` **没有**库能力（`Ok(None)`）。
    NoCapability,
    /// 没装插件（`Err`）。
    NotInstalled,
}

#[derive(Clone)]
struct FakeRegistry {
    mode: Mode,
}

struct FakeCapability;

impl MediaLibraryCapability for FakeCapability {
    fn library_config_fields(&self) -> Vec<LibraryConfigField> {
        vec![
            LibraryConfigField {
                key: "root".to_owned(),
                input: "text".to_owned(),
                read_only: false,
            },
            LibraryConfigField {
                key: "token".to_owned(),
                input: "secret".to_owned(),
                read_only: false,
            },
        ]
    }

    /// ★ **刻意不做**「未提交的 secret 从旧值回填」—— 那是**宿主**的活
    /// （`MediaLibraryService::merge_previous_config`，上游 `_prepare_config`
    /// `:144-151`）。
    ///
    /// 替身若顺手替宿主做了这件事，[`patch_refills_secrets_and_missing_library_is_404`]
    /// 就会在「宿主根本没合并」时**照样绿** —— 那是替身替被测代码把断言做掉了。
    /// 所以这里只在**没收到** `token` 时报 `invalid_config`：宿主漏合并时，
    /// 那个 PATCH 会变成 422 而不是 200，测试当场红。
    fn prepare_library(
        &self,
        submitted: &Value,
        _previous: Option<&PreviousLibraryHandle>,
    ) -> PrepareLibraryFuture<'_> {
        let config = submitted.as_object().cloned().unwrap_or_default();
        let outcome = if !config.contains_key("token") {
            // 首次创建又没给 token → provider 报配置无效（上游会返回 failed 项）。
            Err(ProviderFailureInfo {
                code: "invalid_config".to_owned(),
                message: "token is required".to_owned(),
            })
        } else {
            Ok(PreparedLibrary {
                provider_config: Value::Object(config),
                account_key: Some("acct-1".to_owned()),
            })
        };
        Box::pin(async move { outcome })
    }
}

impl MediaLibraryRegistry for FakeRegistry {
    fn library_for(&self, provider_key: &str) -> LibraryForFuture<'_> {
        let outcome: Result<Option<Box<dyn MediaLibraryCapability>>, ProviderFailureInfo> =
            match self.mode {
                // 裸码：服务层 `bundle_for` 会补 `provider_` 前缀，拼出
                // `provider_not_installed`（见 `MediaLibraryRegistry::library_for` 的文档）。
                // 写 `unavailable` 会拼成 `provider_unavailable` —— 那是「装了但连不上」，
                // 与「没安装」是两种语义，`provider_failure` 的状态分流也靠这个区分。
                Mode::NotInstalled => Err(ProviderFailureInfo {
                    code: "not_installed".to_owned(),
                    message: "provider not installed".to_owned(),
                }),
                Mode::Installed if provider_key == PROVIDER => Ok(Some(Box::new(FakeCapability))),
                Mode::Installed | Mode::NoCapability => Ok(None),
            };
        Box::pin(async move { outcome })
    }

    fn supports_in_place_import(&self, provider_key: &str) -> bool {
        matches!(self.mode, Mode::Installed) && provider_key == PROVIDER
    }

    fn list_bundles(&self) -> Vec<ProviderCatalogEntry> {
        vec![ProviderCatalogEntry {
            provider_key: PROVIDER.to_owned(),
            display_name: "假库".to_owned(),
            library_config_fields: vec![json!({"key": "root"})],
            playback_deliveries: vec!["proxy".to_owned()],
            download_config_fields: Some(vec![json!({"key": "url"})]),
        }]
    }

    fn space_usage(
        &self,
        _library_id: i32,
        _provider_key: &str,
        _provider_config: &Value,
    ) -> SpaceUsageFuture<'_> {
        Box::pin(async { None })
    }
}

async fn seed_token(db: &Db) -> String {
    let users = UserRepository::new(db.clone());
    let user = users
        .insert(&NewUser {
            username: format!("ml{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 每次调用一个新的临时配置文件（这几个端点不读配置，但不能让 `ConfigService`
/// 去解析一个不存在的路径）。
fn config() -> ConfigService {
    let path = std::env::temp_dir().join(format!(
        "sm-ml-http-{}-{}.toml",
        std::process::id(),
        unique()
    ));
    std::fs::write(
        &path,
        format!("[auth]\nfile_signature_secret = \"{SECRET}\"\n"),
    )
    .expect("写测试配置");
    ConfigService::new(path)
}

fn app(db: &TestDb, registry: Option<Mode>) -> axum::Router {
    let state = AppState::new(db.pool().clone(), AuthConfig::new(SECRET), config());
    let state = match registry {
        Some(mode) => state.with_media_library_registry(Arc::new(FakeRegistry { mode })),
        None => state,
    };
    router(state)
}

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.expect("oneshot 失败");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "响应体不是 JSON: {err}; {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

fn authed(method: &str, uri: &str, token: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let payload = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    builder.body(payload).expect("构造请求")
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<missing>")
}

// ------------------------------------------------------------ 创建 / 列表

/// ★ 创建回 201，且响应里的 `provider_config` **剥掉了 secret 字段**。
#[tokio::test]
async fn create_strips_secret_fields_and_returns_201() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed(
            "POST",
            "/media-libraries",
            &token,
            Some(json!({
                "name": "lib-1",
                "provider_key": PROVIDER,
                "provider_config": {"root": "/mnt", "token": "s3cr3t"}
            })),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body={body}");
    assert_eq!(body["provider_key"], PROVIDER, "字段名是 provider_key");
    assert_eq!(body["provider_config"]["root"], "/mnt");
    assert!(
        body["provider_config"].get("token").is_none(),
        "secret 字段必须从响应里剥掉: {body}"
    );
    assert_eq!(
        body["account_key"], "acct-1",
        "provider 派生的账号键要跟着落库"
    );
    assert_eq!(body["supports_in_place_import"], true);
    for absent in ["enabled", "handle", "provider", "kinds"] {
        assert!(
            body.get(absent).is_none(),
            "骨架期自造字段 {absent} 不该出现"
        );
    }
}

/// ★ 列表是**裸数组**（无分页信封），且带上刚建的库。
#[tokio::test]
async fn list_is_a_bare_array_and_includes_the_created_library() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let name = format!("lib-{}", unique());
    let (status, _) = send(
        app(&db, Some(Mode::Installed)),
        authed(
            "POST",
            "/media-libraries",
            &token,
            Some(json!({
                "name": name,
                "provider_key": PROVIDER,
                "provider_config": {"root": "/mnt", "token": "s3cr3t"}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed("GET", "/media-libraries", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().expect("裸数组");
    assert!(items.iter().any(|item| item["name"] == name.as_str()));
}

// ------------------------------------------------------------ 注册表缺失 / 能力缺失

/// ★ 没注入注册表 → `POST` **503**；provider 目录是**空表**（不是 503）。
#[tokio::test]
async fn without_a_registry_create_is_503_and_the_catalog_is_empty() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, None),
        authed(
            "POST",
            "/media-libraries",
            &token,
            Some(json!({"name": "x", "provider_key": PROVIDER, "provider_config": {}})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body={body}");
    assert_eq!(code_of(&body), "provider_not_installed");

    let (status, body) = send(
        app(&db, None),
        authed("GET", "/media-libraries/providers", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "目录端点不该因缺插件而 503");
    assert_eq!(body.as_array().map(Vec::len), Some(0));
}

/// ★ 装了插件但 provider **没有库能力** → **422 `provider_library_unsupported`**。
#[tokio::test]
async fn a_provider_without_library_capability_is_422() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, Some(Mode::NoCapability)),
        authed(
            "POST",
            "/media-libraries",
            &token,
            Some(json!({
                "name": "y",
                "provider_key": PROVIDER,
                "provider_config": {"root": "/mnt", "token": "t"}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(code_of(&body), "provider_library_unsupported");
}

/// ★ provider 未安装（注册表报错） → **503**。
#[tokio::test]
async fn an_uninstalled_provider_is_503() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, Some(Mode::NotInstalled)),
        authed(
            "POST",
            "/media-libraries",
            &token,
            Some(json!({
                "name": "z",
                "provider_key": PROVIDER,
                "provider_config": {"root": "/mnt", "token": "t"}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body={body}");
    assert_eq!(code_of(&body), "provider_not_installed");
}

/// ★ 未知的 `provider_config` 字段 → **422 `invalid_media_library_provider_config`**。
#[tokio::test]
async fn an_unknown_config_field_is_422() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed(
            "POST",
            "/media-libraries",
            &token,
            Some(json!({
                "name": "w",
                "provider_key": PROVIDER,
                "provider_config": {"root": "/mnt", "token": "t", "bogus": 1}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(code_of(&body), "invalid_media_library_provider_config");
}

// ------------------------------------------------------------ 更新

/// 建一个库，返回它的 id。
async fn create_library(db: &TestDb, token: &str) -> i32 {
    let (status, body) = send(
        app(db, Some(Mode::Installed)),
        authed(
            "POST",
            "/media-libraries",
            token,
            Some(json!({
                "name": format!("lib-{}", unique()),
                "provider_key": PROVIDER,
                "provider_config": {"root": "/mnt", "token": "s3cr3t"}
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body={body}");
    i32::try_from(body["id"].as_i64().expect("有 id")).expect("id 在 i32 内")
}

/// ★ 一个字段都没给 → **422 `empty_media_library_update`**（不是原样返回）。
#[tokio::test]
async fn an_empty_patch_is_422() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let id = create_library(&db, &token).await;

    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed(
            "PATCH",
            &format!("/media-libraries/{id}"),
            &token,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={body}");
    assert_eq!(code_of(&body), "empty_media_library_update");
}

/// ★ 改 `root` 时 `token` 从旧值回填，不会丢；不存在的库 → **404**。
///
/// # 这一条怎么"看见"回填
///
/// 响应里**看不到** `token`（被剥掉了，见下面的断言），所以不能靠响应证明。
/// 证明在**替身那里**：`FakeCapability::prepare_library` 收到没有 `token` 的配置
/// 就报 `invalid_config`（→ 422）。宿主一旦不合并，本用例第一段立刻从 200 变 422。
#[tokio::test]
async fn patch_refills_secrets_and_missing_library_is_404() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let id = create_library(&db, &token).await;

    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed(
            "PATCH",
            &format!("/media-libraries/{id}"),
            &token,
            Some(json!({"provider_config": {"root": "/new"}})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["provider_config"]["root"], "/new");
    assert!(
        body["provider_config"].get("token").is_none(),
        "secret 仍被剥掉"
    );

    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed(
            "PATCH",
            "/media-libraries/2147483646",
            &token,
            Some(json!({"name": "nope"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body}");
    assert_eq!(code_of(&body), "media_library_not_found");
}

/// ★ 超出 `i32` 的 id 是 **404**（不是 400）—— 上游是 Python 无界 int。
#[tokio::test]
async fn an_out_of_range_id_is_404_not_400() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed("DELETE", "/media-libraries/99999999999", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body}");
    assert_eq!(code_of(&body), "media_library_not_found");
}

// ------------------------------------------------------------ 删除

/// ★ 删除：不存在 **404**、被下载器引用 **409**、否则 **204 无 body**。
#[tokio::test]
async fn delete_is_404_then_409_when_referenced_then_204() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let id = create_library(&db, &token).await;

    // 不存在 → 404（**不幂等**）
    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed("DELETE", "/media-libraries/2147483646", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body}");
    assert_eq!(code_of(&body), "media_library_not_found");

    // 被下载器客户端引用 → 409
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("client-{}", unique()),
            provider_config: None,
            library_id: id,
        })
        .await
        .expect("插入下载器");
    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed("DELETE", &format!("/media-libraries/{id}"), &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body={body}");
    assert_eq!(code_of(&body), "media_library_in_use");

    // 移除引用后 → 204
    let clients = DownloadClientRepository::new(db.pool().clone())
        .list_by_library(
            id,
            sm_db::common::page::PageRequest::first_page(1).expect("page"),
        )
        .await
        .expect("列下载器");
    for client in &clients.items {
        DownloadClientRepository::new(db.pool().clone())
            .delete(client.id)
            .await
            .expect("删下载器");
    }
    let response = app(&db, Some(Mode::Installed))
        .oneshot(authed(
            "DELETE",
            &format!("/media-libraries/{id}"),
            &token,
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty(), "204 必须没有正文");

    assert!(
        MediaLibraryRepository::new(db.pool().clone())
            .find_by_id(id)
            .await
            .expect("查询")
            .is_none(),
        "库里不该再有这行"
    );
}

// ------------------------------------------------------------ provider 目录

/// ★ 有注册表时目录里能看到插件自报的条目。
#[tokio::test]
async fn the_catalog_lists_installed_bundles() {
    let db = TestDb::require().await;
    let token = seed_token(db.pool()).await;
    let (status, body) = send(
        app(&db, Some(Mode::Installed)),
        authed("GET", "/media-libraries/providers", &token, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().expect("裸数组");
    assert_eq!(items.len(), 1, "body={body}");
    assert_eq!(items[0]["provider_key"], PROVIDER);
    assert_eq!(items[0]["display_name"], "假库");
    assert_eq!(items[0]["playback_deliveries"][0], "proxy");
}
