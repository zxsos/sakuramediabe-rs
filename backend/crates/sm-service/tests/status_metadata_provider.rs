//! `GET /status/metadata-providers/{provider}/test` 的**探测链路** —— 用回环 HTTP
//! 服务打桩 JavDB。
//!
//! # 为什么必须打桩
//!
//! 生产那条路把 host 写死在 [`sm_service::system::status::JAVDB_HOST`]（照上游
//! `metadata/factory.py:15` 硬编码），所以它**没法**指向本地假 JavDB。测试走的是
//! [`sm_service::system::status::probe_javdb`] 这个缝 —— 与生产共用同一条探测路径，
//! 只是 provider 由调用方给。
//!
//! # 三个 `error.type` 都覆盖到了
//!
//! 上游把三种异常捕成三档（`status_service.py:451-482`），本仓的
//! [`MetadataSourceError`] 变体与它们不是一一对应 —— 映射关系写错只会让客户端
//! 拿到错误的诊断，不会让请求失败。所以逐档断言。

use std::time::Instant;

use sm_service::catalog::javdb::JavdbProvider;
use sm_service::system::status::{probe_javdb, METADATA_PROVIDER_TEST_MOVIE_NUMBER};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 搜索响应：`data.movies` 里的候选。
fn search_body(movies: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "data": { "movies": movies } })
}

/// 详情响应：`success: 1` + `data.movie`。
fn detail_body(movie: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "success": 1, "data": { "movie": movie } })
}

/// 假 JavDB 的固定详情 id。断言 `javdb_id` 时用它。
const JAVDB_ID: &str = "SSNI888JAVDBID";

/// 打桩「搜索命中探测番号 + 详情可用」，详情内容由 `movie` 给。
async fn mount_happy_path(server: &MockServer, movie: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v2/search"))
        .and(query_param("q", METADATA_PROVIDER_TEST_MOVIE_NUMBER))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_body(serde_json::json!([{
                "id": JAVDB_ID,
                "number": METADATA_PROVIDER_TEST_MOVIE_NUMBER,
                "release_date": "2020-01-01",
            }]))),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v4/movies/{JAVDB_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(detail_body(movie)))
        .mount(server)
        .await;
}

fn provider_for(server: &MockServer) -> JavdbProvider {
    JavdbProvider::with_base_url(&server.uri()).expect("构造 provider")
}

/// ★ 健康时四个统计字段来自详情，且**过滤规则**与上游一致。
///
/// `actors` 里混了三种无效条目（缺 `id` / 非对象 / 空串 `id`），`tags` 里混了
/// 一种（非对象）—— 上游 `_build_movie_actors`（`javdb.py:920-946`）会跳过前者
/// 里的三种，而 `_build_movie_tags`（`:1000-1018`）**只跳过非对象**（不查 `id`）。
#[tokio::test]
async fn healthy_report_counts_only_valid_actor_and_tag_entries() {
    let server = MockServer::start().await;
    mount_happy_path(
        &server,
        serde_json::json!({
            "id": JAVDB_ID,
            "title": "探测用影片",
            "actors": [
                { "id": "A1", "name": "有效" },
                { "name": "缺 id —— 跳过" },
                "不是对象 —— 跳过",
                { "id": "", "name": "空 id —— 跳过" },
            ],
            "tags": [
                { "id": 1, "name": "标签一" },
                { "id": 2, "name": "标签二" },
                "不是对象 —— 跳过",
            ],
        }),
    )
    .await;

    let report = probe_javdb(&provider_for(&server), "javdb", Instant::now()).await;

    assert!(report.healthy, "{report:?}");
    assert_eq!(report.provider, "javdb");
    assert_eq!(report.movie_number, METADATA_PROVIDER_TEST_MOVIE_NUMBER);
    assert_eq!(report.javdb_id.as_deref(), Some(JAVDB_ID));
    assert_eq!(report.title.as_deref(), Some("探测用影片"));
    assert_eq!(report.actors_count, Some(1), "只有第一条 actor 有非空 id");
    assert_eq!(report.tags_count, Some(2), "tags 只跳过非对象，不查 id");
    assert!(report.error.is_none(), "健康时不该有 error");
}

/// ★ `actors` / `tags` 不是数组时按**空列表**算（上游 `_normalize_movie_list_field`）。
///
/// 这条覆盖的是「字段在，但形状不对」——上游 `javdb.py:838-860` 对 null 与非 list
/// 都返回 `[]` 并在日志里警告，而不是报错。
#[tokio::test]
async fn fields_that_are_not_lists_count_as_zero() {
    let server = MockServer::start().await;
    mount_happy_path(
        &server,
        serde_json::json!({
            "id": JAVDB_ID,
            "title": "形状不对的影片",
            "actors": "不是数组",
            "tags": null,
        }),
    )
    .await;

    let report = probe_javdb(&provider_for(&server), "javdb", Instant::now()).await;

    assert!(report.healthy, "{report:?}");
    assert_eq!(report.actors_count, Some(0));
    assert_eq!(report.tags_count, Some(0));
}

/// ★ 没搜到 → `metadata_not_found`，且四个统计字段都是 `None`。
#[tokio::test]
async fn a_missing_movie_is_metadata_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_body(serde_json::json!([]))))
        .mount(&server)
        .await;

    let report = probe_javdb(&provider_for(&server), "javdb", Instant::now()).await;

    assert!(!report.healthy, "没搜到不算健康");
    let error = report.error.expect("应当带 error 报告");
    assert_eq!(error.error_type, "metadata_not_found");
    assert_eq!(report.provider, "javdb");
    assert_eq!(report.movie_number, METADATA_PROVIDER_TEST_MOVIE_NUMBER);
    assert!(report.javdb_id.is_none(), "失败时不带统计字段");
    assert!(report.title.is_none());
    assert!(report.actors_count.is_none());
    assert!(report.tags_count.is_none());
    // 本仓的 `MetadataSourceError` 不带这四项 —— 必须是 `None` 而不是编一个值。
    assert!(error.method.is_none() && error.url.is_none());
    assert!(error.resource.is_none() && error.lookup_value.is_none());
}

/// ★ `success != 1` 是**请求失败**（`metadata_request_error`），不是「没收录」。
///
/// 抄错这一档的后果：用户看到「JavDB 没这部片」，而真实原因是服务端拒绝了请求。
#[tokio::test]
async fn a_javdb_business_failure_is_metadata_request_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/search"))
        .and(query_param("q", METADATA_PROVIDER_TEST_MOVIE_NUMBER))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_body(serde_json::json!([{
                "id": JAVDB_ID,
                "number": METADATA_PROVIDER_TEST_MOVIE_NUMBER,
                "release_date": "2020-01-01",
            }]))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v4/movies/{JAVDB_ID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "success": 0, "message": "请求被拒绝" })),
        )
        .mount(&server)
        .await;

    let report = probe_javdb(&provider_for(&server), "javdb", Instant::now()).await;

    assert!(!report.healthy);
    let error = report.error.expect("应当带 error 报告");
    assert_eq!(error.error_type, "metadata_request_error");
    assert!(
        error.message.contains("请求被拒绝"),
        "消息要带上游给的原因，实际：{}",
        error.message
    );
}
