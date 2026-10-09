//! `/system/plugins` 的 HTTP 契约测试。
//!
//! # 挂的是**真实**实现，不是假实现
//!
//! `AppState::with_plugin_admin` 收到的是 `sm_plugins::admin::PluginAdminService`
//! —— 于是「配置里写了 `enabled = ["local"]`，`GET` 里就 `enabled: true`」
//! 这类**跨层**行为真的被跑到了。假实现只能证明路由会转发，证明不了这件事。
//!
//! # 这一层测的四件事
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 响应字段名 | 与上游 `PluginSummaryResource` 逐字一致 | 少一个键不报错，只会让客户端某块空白 |
//! | 详情平铺 | `plugin_id` 在**顶层**，不在 `summary` 子对象里 | 上游是继承 |
//! | `PATCH` 的 `enabled` | **query 参数**，不是 body | 上游签名里它没有 `Body(...)` |
//! | 未知插件 | 404 `plugin_not_found` | 详情查询的「不存在」是正常结果，不是 500 |
//!
//! 还有一条**没注入**的路径：那种情况下必须 500 `plugin_admin_unavailable`，
//! **不能**返回空列表 —— 否则「组合根漏了接线」与「一台插件都没装」长得一样。
//!
//! 每个用例的 `TestDb` 是独立 schema（鉴权要读库）。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_plugins::admin::PluginAdminService;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use tower::ServiceExt;

const SECRET: &str = "plugins-secret";

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 临时插件根 + 临时配置文件。
struct Fixture {
    base: PathBuf,
    /// 插件根目录（`plugins.root_dir`）。
    pub root: PathBuf,
    config_path: PathBuf,
}

impl Fixture {
    /// `plugins_toml` 是除 `root_dir` 之外要写进 `[plugins]` 的内容。
    fn new(tag: &str, plugins_toml: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sm-plugins-http-{tag}-{}", unique()));
        let root = base.join("plugins");
        std::fs::create_dir_all(&root).expect("建插件根");
        let config_path = base.join("config.toml");
        // 路径里的反斜杠在 TOML 基本字符串里是转义符 —— 统一换成 `/`（Windows 也认）。
        let root_toml = root.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &config_path,
            format!(
                "[auth]\nfile_signature_secret = \"{SECRET}\"\n\n\
                 [plugins]\nroot_dir = \"{root_toml}\"\n{plugins_toml}"
            ),
        )
        .expect("写测试配置");
        Self {
            base,
            root,
            config_path,
        }
    }

    /// 装一个插件（写清单即可 —— 管理接口不读可执行文件）。
    fn install(&self, plugin_id: &str, version: &str) {
        let dir = self.root.join(plugin_id);
        std::fs::create_dir_all(&dir).expect("建插件目录");
        std::fs::write(
            dir.join("manifest.json"),
            json!({
                "plugin_id": plugin_id,
                "display_name": "本地存储",
                "version": version,
                "author": "SakuraMedia",
                "homepage": "https://example.invalid",
                "release_api_url": "https://example.invalid/releases",
            })
            .to_string(),
        )
        .expect("写清单");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn seed_token(db: &TestDb) -> String {
    let user = UserRepository::new(db.pool().clone())
        .insert(&NewUser {
            username: format!("pl{}", unique()),
            password_hash: "$argon2id$v=19$m=64,t=1,p=1$c2FsdA$hash".to_owned(),
        })
        .await
        .expect("插入测试用户失败");
    encode_access_token(i64::from(user.id), Utc::now() + Duration::hours(1), SECRET)
}

fn config(fixture: &Fixture) -> ConfigService {
    ConfigService::new(fixture.config_path.clone())
}

/// 接上插件管理的 app。
fn app(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(
        AppState::new(db.pool().clone(), AuthConfig::new(SECRET), config(fixture))
            .with_plugin_admin(Arc::new(PluginAdminService::new(config(fixture)))),
    )
}

/// 没接插件管理的 app —— 用来证明「漏接线」会报错而不是空列表。
fn app_without_admin(db: &TestDb, fixture: &Fixture) -> axum::Router {
    router(AppState::new(
        db.pool().clone(),
        AuthConfig::new(SECRET),
        config(fixture),
    ))
}

fn request(method: &str, uri: &str, token: &str) -> Request<Body> {
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

// ================================================================ 列表

#[tokio::test]
async fn the_list_reports_installed_plugins_with_the_upstream_field_names() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("list", "enabled = [\"local\"]\n");
    fixture.install("local", "1.2.3");
    fixture.install("other", "0.1.0");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().expect("数组");
    assert_eq!(items.len(), 2, "两个插件目录都该列出来");

    let local = items
        .iter()
        .find(|item| item["plugin_id"] == "local")
        .expect("local");
    // 上游字段名逐字一致（骨架期那个自造的 `name` 已经没有了）。
    let keys: Vec<&str> = local
        .as_object()
        .expect("对象")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "plugin_id",
            "display_name",
            "version",
            "host_api_version",
            "enabled",
            "load_status",
            "load_error",
            "release_api_url",
        ]
    );
    assert_eq!(local["display_name"], "本地存储");
    assert_eq!(local["version"], "1.2.3");
    assert_eq!(local["enabled"], true, "配置里启用了它");
    assert_eq!(local["load_status"], "ok");
    // 详情/概要**不**做 exclude_none：可选字段是 null，不是消失。
    assert!(local["load_error"].is_null());
    assert_eq!(local["release_api_url"], "https://example.invalid/releases");

    let other = items
        .iter()
        .find(|item| item["plugin_id"] == "other")
        .expect("other");
    assert_eq!(other["enabled"], false, "没在 enabled 里");
}

#[tokio::test]
async fn an_empty_plugin_root_returns_an_empty_array() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty", "");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

// ================================================================ 详情

#[tokio::test]
async fn the_detail_is_flattened_and_carries_the_whole_manifest() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("detail", "");
    fixture.install("local", "1.2.3");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins/local", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    // 平铺：`plugin_id` 在顶层，没有 `summary` 子对象（上游是继承）。
    assert_eq!(body["plugin_id"], "local");
    assert!(body.get("summary").is_none());
    assert_eq!(body["display_name"], "本地存储");
    assert_eq!(body["author"], "SakuraMedia");
    assert_eq!(body["homepage"], "https://example.invalid");
    assert!(body["requires_python"].is_null());
    // 整份清单原文都在。
    assert_eq!(body["manifest"]["plugin_id"], "local");
    // data 目录挂在插件目录下。
    assert!(body["data_dir"]
        .as_str()
        .expect("data_dir 是字符串")
        .replace('\\', "/")
        .ends_with("local/data"));
}

#[tokio::test]
async fn an_unknown_plugin_is_a_404_with_the_upstream_code() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("unknown", "");
    fixture.install("local", "1.0.0");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins/ghost", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "plugin_not_found");
}

// ================================================================ 启停

/// ★ `enabled` 是 **query 参数**，不是 body。
///
/// 上游 `set_plugin_enabled(plugin_id: str, enabled: bool)` 的 `enabled` 没有
/// `Body(...)`，FastAPI 因此按查询参数处理。照 body 传会 422 —— 而这个差别
/// 只有真发一次请求才能发现。
#[tokio::test]
async fn the_enabled_flag_travels_as_a_query_parameter() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("toggle", "enabled = [\"local\"]\n");
    fixture.install("local", "1.0.0");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        request("PATCH", "/system/plugins/local?enabled=false", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "query 形态必须被接受");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["plugin_id"], "local");

    // 配置**真的写盘了**（而不是只改了内存）。
    let text = std::fs::read_to_string(&fixture.config_path).expect("读配置");
    assert!(
        text.contains("enabled = []"),
        "停用后配置里不该还有 local：{text}"
    );

    // 再启用回来，列表立刻反映新状态（每次操作都重读磁盘快照）。
    let (status, _) = send(
        app(&db, &fixture),
        request("PATCH", "/system/plugins/local?enabled=true", &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;
    assert_eq!(listed[0]["enabled"], true);
}

#[tokio::test]
async fn toggling_an_unknown_plugin_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("toggle-unknown", "");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app(&db, &fixture),
        request("PATCH", "/system/plugins/ghost?enabled=true", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "plugin_not_found");
}

// ================================================================ 未注入

/// ★ 没接插件管理 → 500，**不是**空列表。
///
/// 「组合根忘了接线」与「一台插件都没装」都会让插件页面空着，而前者是故障。
/// 一个明确的错误码把它们区分开 —— 这条测试就是钉住这个决定。
#[tokio::test]
async fn without_injection_the_endpoints_report_500_instead_of_an_empty_list() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("no-admin", "");
    fixture.install("local", "1.0.0");
    let token = seed_token(&db).await;

    let (status, body) = send(
        app_without_admin(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "plugin_admin_unavailable");
    assert!(
        body.get("items").is_none(),
        "绝不能把「没接线」伪装成空列表"
    );
}

// ================================================================ 安装 / 升级

const BOUNDARY: &str = "----smtestboundary";

/// 造一个 multipart 请求体。
///
/// `file` 是 `(字段名, 文件名, 内容)`；`fields` 是普通文本字段。
fn multipart_body(fields: &[(&str, &str)], file: Option<(&str, &str, &[u8])>) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some((name, file_name, content)) = file {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; \
                 filename=\"{file_name}\"\r\nContent-Type: application/zip\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn upload_content_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// 一个合法的插件包（根部清单 + 入口文件）。
fn plugin_package(plugin_id: &str, version: &str) -> Vec<u8> {
    use std::io::Write as _;

    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("manifest.json", options).expect("写清单");
        writer
            .write_all(
                json!({
                    "plugin_id": plugin_id,
                    "display_name": "本地存储",
                    "version": version,
                })
                .to_string()
                .as_bytes(),
            )
            .expect("写清单内容");
        writer.start_file(plugin_id, options).expect("写入口");
        writer.write_all(b"binary").expect("写入口内容");
        writer.finish().expect("收尾");
    }
    buffer.into_inner()
}

fn upload_request(
    method: &str,
    uri: &str,
    token: &str,
    body: Vec<u8>,
    content_length: Option<u64>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, upload_content_type());
    if let Some(length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, length.to_string());
    }
    builder.body(Body::from(body)).expect("构造请求")
}

/// ★ `POST /system/plugins` —— **201**，且插件真的落到目录里、写进配置。
#[tokio::test]
async fn uploading_a_package_installs_it_with_201() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("install", "");
    let token = seed_token(&db).await;

    let package = plugin_package("local", "1.2.3");
    let body = multipart_body(&[], Some(("file", "local.zip", &package)));
    let (status, response) = send(
        app(&db, &fixture),
        upload_request("POST", "/system/plugins", &token, body, None),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "安装是 201 而不是 200");
    assert_eq!(response["plugin_id"], "local");
    assert_eq!(response["version"], "1.2.3");
    // 没有 `dependencies` → 只重启两个进程。
    assert_eq!(response["pending_restart"], json!(["api", "aps"]));

    // 列表里看得见，且默认 `enable=true`。
    let (_, listed) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;
    assert_eq!(listed[0]["plugin_id"], "local");
    assert_eq!(listed[0]["enabled"], true);

    // 临时上传文件必须被清掉（`.staging/uploads` 不该留下东西）。
    let uploads = fixture.root.join(".staging").join("uploads");
    let leftover = std::fs::read_dir(&uploads)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftover, 0, "上传完要删掉临时文件");
}

/// `enable=false` → 装上但**不启用**。
#[tokio::test]
async fn install_accepts_enable_false() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("install-disabled", "");
    let token = seed_token(&db).await;

    let package = plugin_package("local", "1.0.0");
    let body = multipart_body(
        &[("enable", "false")],
        Some(("file", "local.zip", &package)),
    );
    let (status, _) = send(
        app(&db, &fixture),
        upload_request("POST", "/system/plugins", &token, body, None),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "enable 在 file **之前**也要能读到"
    );
    let (_, listed) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;
    assert_eq!(listed[0]["enabled"], false);
}

/// 没有 `file` 字段 → 422（上游 `File(...)` 必填）。
#[tokio::test]
async fn install_without_a_file_field_is_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("install-nofile", "");
    let token = seed_token(&db).await;

    let body = multipart_body(&[("sha256", "abc")], None);
    let (status, response) = send(
        app(&db, &fixture),
        upload_request("POST", "/system/plugins", &token, body, None),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response["error"]["code"], "validation_error");
    assert_eq!(response["error"]["details"]["field"], "file");
}

/// 坏 zip → 422 `plugin_install_failed`（不是 500）。
#[tokio::test]
async fn a_broken_package_is_a_422_install_failed() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("install-broken", "");
    let token = seed_token(&db).await;

    let body = multipart_body(&[], Some(("file", "local.zip", b"not a zip at all")));
    let (status, response) = send(
        app(&db, &fixture),
        upload_request("POST", "/system/plugins", &token, body, None),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response["error"]["code"], "plugin_install_failed");
}

/// ★ `Content-Length` 超限 → **413 `plugin_too_large`**（在读 body 之前）。
///
/// 上游 `_check_upload_size` 的码是 `plugin_too_large`，**不是**通用提取器
/// 的 `http_error` —— 客户端按 `code` 分支，两者状态码相同而语义不同。
#[tokio::test]
async fn an_oversized_content_length_is_a_413_plugin_too_large() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("install-toolarge", "");
    let token = seed_token(&db).await;

    // 只声明一个巨大的 `Content-Length`，body 本身很小 —— 预检要在读它之前就拦下。
    let body = multipart_body(&[], Some(("file", "local.zip", b"tiny")));
    let (status, response) = send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins",
            &token,
            body,
            Some(200 * 1024 * 1024),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response["error"]["code"], "plugin_too_large");
    assert_eq!(response["error"]["details"]["max_bytes"], 100 * 1024 * 1024);
}

/// ★ 升级：200 + 版本变化，且**保留 `data/`**。
#[tokio::test]
async fn upgrading_a_package_bumps_the_version_and_keeps_user_data() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("upgrade", "");
    let token = seed_token(&db).await;

    let first = plugin_package("local", "1.0.0");
    let (status, _) = send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins",
            &token,
            multipart_body(&[], Some(("file", "local.zip", &first))),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // 宿主托管的用户数据。
    let data = fixture.root.join("local").join("data");
    std::fs::create_dir_all(&data).expect("建 data 目录");
    std::fs::write(data.join("state.json"), b"user data").expect("写用户数据");

    let second = plugin_package("local", "2.0.0");
    let (status, response) = send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins/local/upgrade",
            &token,
            multipart_body(&[], Some(("file", "local.zip", &second))),
            None,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "升级是 200（不是 201）");
    assert_eq!(response["version"], "2.0.0");
    assert_eq!(
        std::fs::read(data.join("state.json")).expect("data 必须还在"),
        b"user data"
    );
}

/// 版本不高于当前 → 422 `plugin_upgrade_failed`。
#[tokio::test]
async fn upgrading_to_a_lower_version_is_a_422() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("upgrade-lower", "");
    let token = seed_token(&db).await;

    let first = plugin_package("local", "2.0.0");
    send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins",
            &token,
            multipart_body(&[], Some(("file", "local.zip", &first))),
            None,
        ),
    )
    .await;

    let older = plugin_package("local", "1.0.0");
    let (status, response) = send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins/local/upgrade",
            &token,
            multipart_body(&[], Some(("file", "local.zip", &older))),
            None,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(response["error"]["code"], "plugin_upgrade_failed");
}

/// 升级一个没装的插件 → 404（不是「顺手安装」）。
#[tokio::test]
async fn upgrading_an_unknown_plugin_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("upgrade-unknown", "");
    let token = seed_token(&db).await;

    let package = plugin_package("ghost", "1.0.0");
    let (status, response) = send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins/ghost/upgrade",
            &token,
            multipart_body(&[], Some(("file", "ghost.zip", &package))),
            None,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(response["error"]["code"], "plugin_not_found");
    assert!(!fixture.root.join("ghost").exists(), "404 时不该把它装进来");
}

// ================================================================ 卸载

/// ★ `DELETE /system/plugins/{id}` —— **200 + body**（不是 204），
/// 且 **`data/` 保留**、版本取自删除**前**的那份。
#[tokio::test]
async fn deleting_a_plugin_returns_200_with_the_version_and_keeps_data() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("remove", "");
    let token = seed_token(&db).await;

    let package = plugin_package("local", "2.3.4");
    let (status, _) = send(
        app(&db, &fixture),
        upload_request(
            "POST",
            "/system/plugins",
            &token,
            multipart_body(&[], Some(("file", "local.zip", &package))),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let data = fixture.root.join("local").join("data");
    std::fs::create_dir_all(&data).expect("建 data 目录");
    std::fs::write(data.join("state.json"), b"user data").expect("写用户数据");

    let (status, response) = send(
        app(&db, &fixture),
        request("DELETE", "/system/plugins/local", &token),
    )
    .await;

    // 本仓库其他 delete 都回 204 无 body，**这里不是**。
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["plugin_id"], "local");
    // 代码已删，所以版本只能取自删除前 —— 上游也是先 get_plugin 再 remove。
    assert_eq!(response["version"], "2.3.4");
    // 上游对卸载**硬编**这两个目标（不像安装那样按 dependencies 分叉）。
    assert_eq!(response["pending_restart"], json!(["api", "aps"]));

    assert!(!fixture.root.join("local").join("manifest.json").exists());
    assert!(data.join("state.json").is_file(), "★ data/ 必须保留");

    // 列表里不再有它。
    let (_, listed) = send(
        app(&db, &fixture),
        request("GET", "/system/plugins", &token),
    )
    .await;
    assert_eq!(listed, json!([]));
}

/// 删一个没装的插件 → 404。
#[tokio::test]
async fn deleting_an_unknown_plugin_is_a_404() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("remove-unknown", "");
    let token = seed_token(&db).await;

    let (status, response) = send(
        app(&db, &fixture),
        request("DELETE", "/system/plugins/ghost", &token),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(response["error"]["code"], "plugin_not_found");
}

// ================================================================ 鉴权

#[tokio::test]
async fn the_endpoints_require_authentication() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("auth", "");

    let anonymous = Request::builder()
        .method("GET")
        .uri("/system/plugins")
        .body(Body::empty())
        .expect("构造请求");
    let (status, body) = send(app(&db, &fixture), anonymous).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}
