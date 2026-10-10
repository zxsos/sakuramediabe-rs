//! `IndexerSettingsService::test_connection` 的集成测试：真实 PostgreSQL +
//! 真实回环 HTTP（`wiremock` 起假 indexer）。
//!
//! # 为什么必须同时有真库与真 HTTP
//!
//! 这条路径要跑通「读 indexer → 读绑定 → 发 Torznab 请求 → 解析 XML → 数结果」。
//! 任何一段用替身都会让另外几段失去意义：比如把 HTTP 换成 stub，就测不到
//! **失败信息里有没有泄漏 apikey/URL**（那是上游专门写 `_describe_search_error`
//! 拦的东西）。

use sm_db::repo::{
    DownloadClientRepository, IndexerDownloadClientRepository, IndexerRepository,
    MediaLibraryRepository, NewDownloadClient, NewIndexer, NewMediaLibrary,
};
use sm_db::testing::TestDb;
use sm_service::system::indexer_settings::IndexerSettingsService;
use sm_service::transfers::torznab::TorznabClient;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 两条命中的 Torznab 响应。
const TWO_HITS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>假索引器</title>
    <item>
      <title>SSNI-888 版本一</title>
      <link>magnet:?xt=urn:btih:AAA</link>
      <torznab:attr name="seeders" value="30"/>
    </item>
    <item>
      <title>SSNI-888 版本二</title>
      <link>magnet:?xt=urn:btih:BBB</link>
      <torznab:attr name="seeders" value="5"/>
    </item>
  </channel>
</rss>"#;

/// 造一个 indexer + 一个下载器 + 它们之间的绑定，返回 indexer id。
async fn seed_indexer(db: &TestDb, url: &str, api_key: Option<&str>) -> i32 {
    let library = MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("conn-lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert library")
        .id;
    let client = DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("conn-client-{}", n()),
            provider_config: None,
            library_id: library,
        })
        .await
        .expect("insert download client");
    let indexer = IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: format!("conn-indexer-{}", n()),
            url: url.to_owned(),
            kind: "pt".to_owned(),
            api_key: api_key.map(str::to_owned),
        })
        .await
        .expect("insert indexer");
    IndexerDownloadClientRepository::new(db.pool().clone())
        .bind(indexer.id, client.id)
        .await
        .expect("bind");
    indexer.id
}

/// 造一个 indexer 但**不**绑定下载器。
async fn seed_unbound_indexer(db: &TestDb, url: &str) {
    IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: format!("conn-unbound-{}", n()),
            url: url.to_owned(),
            kind: "pt".to_owned(),
            api_key: None,
        })
        .await
        .expect("insert indexer");
}

fn service(db: &TestDb, server: &MockServer) -> IndexerSettingsService {
    // 假 indexer 是回环 http，`TorznabClient::new()` 的 `no_proxy()` 不影响它。
    let _ = server;
    IndexerSettingsService::with_torznab(db.pool(), TorznabClient::new())
}

#[tokio::test]
async fn no_indexers_yields_the_no_indexers_configured_report() {
    let db = TestDb::require().await;
    let report = IndexerSettingsService::new(db.pool())
        .test_connection()
        .await
        .expect("探测本身不该失败");

    assert!(!report.healthy);
    assert_eq!(report.indexers_checked, 0);
    assert_eq!(report.result_count, 0);
    assert_eq!(report.query, "SSNI-888");
    let error = report.error.expect("应带错误");
    assert_eq!(error.error_type, "no_indexers_configured");
    assert_eq!(
        error.message,
        "尚未配置任何 indexer，无法测试 Torznab 连通性"
    );
}

#[tokio::test]
async fn a_reachable_indexer_yields_a_healthy_report_with_the_hit_count() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(TWO_HITS))
        .mount(&server)
        .await;
    seed_indexer(&db, &server.uri(), None).await;

    let report = service(&db, &server)
        .test_connection()
        .await
        .expect("探测本身不该失败");

    assert!(report.healthy, "应判健康：{report:?}");
    assert_eq!(report.indexers_checked, 1);
    assert_eq!(report.result_count, 2, "应数出两条命中");
    assert!(report.error.is_none());
    assert_eq!(report.query, "SSNI-888");
}

#[tokio::test]
async fn an_http_error_is_reported_without_leaking_the_url_or_api_key() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    // 带一个 apikey：它绝不能出现在错误消息里。
    seed_indexer(&db, &server.uri(), Some("super-secret-key")).await;

    let report = service(&db, &server)
        .test_connection()
        .await
        .expect("探测本身不该失败");

    assert!(!report.healthy);
    assert_eq!(report.indexers_checked, 1);
    assert_eq!(report.result_count, 0);
    let error = report.error.expect("应带错误");
    assert_eq!(error.error_type, "torznab_request_error");
    assert_eq!(error.message, "HTTP 500", "HTTP 错误只留状态码");
    assert!(
        !error.message.contains("super-secret-key"),
        "错误消息泄漏了 apikey：{}",
        error.message
    );
    assert!(
        !error.message.contains("127.0.0.1") && !error.message.contains("localhost"),
        "错误消息泄漏了 indexer 地址：{}",
        error.message
    );
}

#[tokio::test]
async fn a_malformed_xml_response_is_a_request_error_not_zero_results() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    // 被截断的响应：未闭合标签。若不报错，会表现为「健康的 0 条结果」。
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<rss><channel><item>"))
        .mount(&server)
        .await;
    seed_indexer(&db, &server.uri(), None).await;

    let report = service(&db, &server)
        .test_connection()
        .await
        .expect("探测本身不该失败");
    assert!(!report.healthy, "截断的响应不能算健康：{report:?}");
    assert_eq!(
        report.error.expect("应带错误").error_type,
        "torznab_request_error"
    );
}

#[tokio::test]
async fn an_indexer_without_bound_clients_is_skipped_but_still_counted() {
    let db = TestDb::require().await;
    let server = MockServer::start().await;
    // 不挂任何 mock：若真发了请求，wiremock 会回 404，探测就会判不健康。
    seed_unbound_indexer(&db, &server.uri()).await;

    let report = service(&db, &server)
        .test_connection()
        .await
        .expect("探测本身不该失败");

    assert!(report.healthy, "无绑定的 indexer 应被跳过而不是报错");
    assert_eq!(report.indexers_checked, 1, "计数算的是全部 indexer");
    assert_eq!(report.result_count, 0);
}
