//! `GET /download-candidates` 的 HTTP 契约测试，**真实 PostgreSQL + 真实回环 HTTP**。
//!
//! # 这层测什么
//!
//! service 层的纯规则（番号大写、kind 白名单、标题过滤）已由
//! `sm-service/src/transfers/download_search.rs` 的单测钉住。这一层测的是
//! **契约翻译**：
//!
//! - 查询参数到 service 的映射（`movie_number` 必填、`indexer_kind` 白名单）；
//! - DTO 的**字段集合与数量**（多一个少一个都是契约变更）；
//! - 候选从 Torznab XML 长成响应体的整条链路 —— 索引器 URL 来自库里的行，
//!   所以只能起真回环服务；
//! - 错误信封的状态码与 code（`422 invalid_download_candidate_*`、
//!   `422 validation_error`、`502 download_candidate_search_failed`）。
//!
//! 每个用例的 `TestDb` 是**独立 schema**，所以「没有配置索引器」这种以空表为
//! 前提的断言不会被别的用例污染。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::{router, AppState};
use sm_core::jwt::encode_access_token;
use sm_db::repo::{
    DownloadClientRepository, IndexerDownloadClientRepository, IndexerRepository,
    MediaLibraryRepository, NewDownloadClient, NewIndexer, NewMediaLibrary, NewUser,
    UserRepository,
};
use sm_db::testing::TestDb;
use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::ConfigService;
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "downloads-http-secret";

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
        let base = std::env::temp_dir().join(format!("sm-downloads-{tag}-{}", unique()));
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
            username: format!("dl{}", unique()),
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

/// 两条命中的 Torznab 响应。
const TWO_HITS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>假索引器</title>
    <item>
      <title>SSNI-888 版本一</title>
      <description>&lt;b&gt;高清&lt;/b&gt;  无码</description>
      <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:AAA"/>
      <torznab:attr name="seeders" value="30"/>
      <size>1073741824</size>
    </item>
    <item>
      <title>SSNI-888 版本二</title>
      <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:BBB"/>
      <torznab:attr name="seeders" value="5"/>
      <size>4096</size>
    </item>
  </channel>
</rss>"#;

/// 造一个 indexer + 一个下载器 + 它们之间的绑定，返回 `(indexer_id, client_id, 名字对)`。
async fn seed_indexer(db: &TestDb, url: &str, kind: &str, api_key: Option<&str>) -> (i32, i32) {
    let library = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("dl-lib-{}", unique()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id;
    let client = DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("dl-client-{}", unique()),
            provider_config: None,
            library_id: library,
        })
        .await
        .expect("insert download client");
    let indexer = IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: format!("dl-indexer-{}", unique()),
            url: url.to_owned(),
            kind: kind.to_owned(),
            api_key: api_key.map(str::to_owned),
        })
        .await
        .expect("insert indexer");
    IndexerDownloadClientRepository::new(db.pool().clone())
        .bind(indexer.id, client.id)
        .await
        .expect("bind");
    (indexer.id, client.id)
}

async fn mount_xml(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_candidate_list_is_the_upstream_resource() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("shape");
    let token = seed_token(db.pool()).await;
    let server = MockServer::start().await;
    mount_xml(&server, TWO_HITS).await;
    let (_, client_id) = seed_indexer(&db, &server.uri(), "pt", None).await;

    // 请求用**小写**番号：候选里的 `movie_number` 必须是归一后的大写值。
    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-candidates?movie_number=ssni-888", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let candidates = body.as_array().expect("顶层是数组");
    assert_eq!(candidates.len(), 2, "两条命中都该留下：{body}");

    // 字段集合**逐字**对齐上游 `DownloadCandidateResource`（10 个键）。
    let mut keys: Vec<&str> = candidates[0]
        .as_object()
        .expect("候选是对象")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "download_clients",
            "indexer_kind",
            "indexer_name",
            "movie_number",
            "resolved_client_id",
            "resolved_client_name",
            "seeders",
            "size_bytes",
            "source_uri",
            "title",
        ],
        "候选的字段集合变了"
    );

    // `seeders` 降序 —— 30 的那条在前。
    assert_eq!(candidates[0]["seeders"], json!(30));
    assert_eq!(candidates[1]["seeders"], json!(5));
    assert_eq!(
        candidates[0]["title"],
        json!("SSNI-888 版本一 高清 无码"),
        "标题 = 清洗后的 title + description"
    );
    assert_eq!(
        candidates[0]["source_uri"],
        json!("magnet:?xt=urn:btih:AAA")
    );
    assert_eq!(candidates[0]["size_bytes"], json!(1073741824_i64));
    assert_eq!(candidates[0]["movie_number"], json!("SSNI-888"));
    assert_eq!(candidates[0]["indexer_kind"], json!("pt"));
    assert_eq!(candidates[0]["resolved_client_id"], json!(client_id));
    // 默认下载器 = 绑定顺序的第一个，也就是唯一绑定的那个。
    let clients = candidates[0]["download_clients"]
        .as_array()
        .expect("download_clients 是数组");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["id"], json!(client_id));
    assert!(clients[0]["name"].as_str().is_some(), "应带下载器名");
    assert!(
        candidates[0]["indexer_name"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "应带索引器名"
    );

    // 发出去的检索词用的是**大写**番号，且没有索引器 key 时不带 `apikey`。
    let requests = server.received_requests().await.expect("取请求记录");
    assert_eq!(requests.len(), 1, "只该问一个索引器");
    let query: Vec<(String, String)> = requests[0]
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert!(query.contains(&("t".to_owned(), "search".to_owned())));
    assert!(
        query.contains(&("q".to_owned(), "SSNI-888".to_owned())),
        "检索词应是归一后的大写番号，实际 {query:?}"
    );
    assert!(
        !query.iter().any(|(k, _)| k == "apikey"),
        "空 key 不该带 apikey 参数：{query:?}"
    );
}

#[tokio::test]
async fn a_title_mismatched_candidate_is_dropped() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("filter");
    let token = seed_token(db.pool()).await;
    let server = MockServer::start().await;
    // 命中两条不同番号：标题里能解析出番号且与请求不一致的那条必须被剔除。
    mount_xml(
        &server,
        r#"<rss><channel>
             <item><title>SSNI-888 对的那条</title><torznab:attr name="seeders" value="9"/></item>
             <item><title>ABC-123 错配的那条</title><torznab:attr name="seeders" value="99"/></item>
           </channel></rss>"#,
    )
    .await;
    seed_indexer(&db, &server.uri(), "pt", None).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-candidates?movie_number=SSNI-888", &token),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let candidates = body.as_array().expect("顶层是数组");
    assert_eq!(candidates.len(), 1, "错配的那条该被剔除：{body}");
    assert_eq!(candidates[0]["seeders"], json!(9));
}

#[tokio::test]
async fn only_the_requested_indexer_kind_is_searched() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("kind");
    let token = seed_token(db.pool()).await;

    let pt_server = MockServer::start().await;
    mount_xml(&pt_server, TWO_HITS).await;
    let (pt_id, _) = seed_indexer(&db, &pt_server.uri(), "pt", None).await;

    let bt_server = MockServer::start().await;
    mount_xml(&bt_server, TWO_HITS).await;
    let (bt_id, _) = seed_indexer(&db, &bt_server.uri(), "bt", None).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            "/download-candidates?movie_number=SSNI-888&indexer_kind=bt",
            &token,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "响应: {body}");
    let candidates = body.as_array().expect("顶层是数组");
    assert_eq!(candidates.len(), 2, "只该有 bt 那台的命中：{body}");
    for candidate in candidates {
        assert_eq!(candidate["indexer_kind"], json!("bt"));
    }
    assert_eq!(
        pt_server
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        0,
        "pt 索引器不该被请求"
    );
    assert_eq!(
        bt_server
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        1,
        "bt 索引器该被请求一次"
    );
    let _ = (pt_id, bt_id);
}

#[tokio::test]
async fn no_configured_indexer_yields_an_empty_list() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("empty");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-candidates?movie_number=SSNI-888", &token),
    )
    .await;

    // 空数组而不是 404 —— 「没有索引器」是「搜到 0 条」。
    assert_eq!(status, StatusCode::OK, "响应: {body}");
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn a_search_failure_is_a_502_and_leaks_neither_url_nor_api_key() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("502");
    let token = seed_token(db.pool()).await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    seed_indexer(&db, &server.uri(), "pt", Some("super-secret-key")).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-candidates?movie_number=SSNI-888", &token),
    )
    .await;

    // 「索引器全挂了」必须是 502，不能伪装成「没有资源」（200 空列表）。
    assert_eq!(status, StatusCode::BAD_GATEWAY, "响应: {body}");
    assert_eq!(code_of(&body), "download_candidate_search_failed");
    assert_eq!(body["error"]["message"], json!("Torznab search failed"));
    let detail = body["error"]["details"]["detail"]
        .as_str()
        .expect("应带 details.detail");
    assert_eq!(detail, "HTTP 500");
    assert!(
        !detail.contains("super-secret-key")
            && !detail.contains("127.0.0.1")
            && !detail.contains("localhost"),
        "错误详情泄漏了 URL 或 apikey：{detail}"
    );
}

#[tokio::test]
async fn an_empty_movie_number_is_rejected_before_any_search() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("blank");
    let token = seed_token(db.pool()).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-candidates?movie_number=", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "invalid_download_candidate_movie_number");
    assert_eq!(
        body["error"]["message"],
        json!("movie_number cannot be empty")
    );
}

#[tokio::test]
async fn a_missing_movie_number_is_a_plain_validation_error() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("missing");
    let token = seed_token(db.pool()).await;

    // 与「空串」是两个不同的码：上游分别是 pydantic 与 service 给的。
    let (status, body) = send(
        app(&db, &fixture),
        authed("GET", "/download-candidates", &token),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "validation_error");
}

#[tokio::test]
async fn an_unknown_indexer_kind_is_rejected_and_echoes_the_raw_input() {
    let db = TestDb::require().await;
    let fixture = Fixture::new("badkind");
    let token = seed_token(db.pool()).await;
    // 不挂任何索引器：这个校验必须在**搜索之前**发生，所以不该发出请求。
    let server = MockServer::start().await;
    seed_indexer(&db, &server.uri(), "pt", None).await;

    let (status, body) = send(
        app(&db, &fixture),
        authed(
            "GET",
            "/download-candidates?movie_number=SSNI-888&indexer_kind=torznab",
            &token,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "响应: {body}");
    assert_eq!(code_of(&body), "invalid_download_candidate_indexer_kind");
    assert_eq!(body["error"]["details"]["indexer_kind"], json!("torznab"));
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        0,
        "kind 非法时不该发出任何搜索请求"
    );
}
