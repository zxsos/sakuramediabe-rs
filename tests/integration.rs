//! JavDB 列表抓取的集成测试：wiremock 假服务，不许联网。

use plugin_more_movies::javdb;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 正常列表页：两条 + 页码一致。
#[tokio::test]
async fn fetch_latest_page_ok() {
    let server = MockServer::start().await;
    let payload = json!({
        "success": 1,
        "data": {
            "current_page": 1,
            "movies": [
                {"id": "abc", "number": "ssis-001", "release_date": "2026-01-01"},
                {"id": "def", "number": "SSIS-002", "release_date": null},
            ],
        },
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/movies/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&payload))
        .mount(&server)
        .await;

    let host = server.address().to_string();
    let url = javdb::latest_page_url(&host, 0, 1);
    // wiremock 的地址是 127.0.0.1:port；latest_page_url 拼的是 https://，
    // 这里直接用 http 调（签名头照带，测试只验解析）。
    let url = url.replacen("https://", "http://", 1);
    let body: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    let items = javdb::parse_latest_page(&body, 0, 1).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].javdb_id, "abc");
    assert_eq!(items[0].number, "SSIS-001");
}

/// success != 1 时报错（不是空页）。
#[tokio::test]
async fn fetch_latest_page_bad_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/movies/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": 0})))
        .mount(&server)
        .await;

    let url = javdb::latest_page_url(&server.address().to_string(), 0, 1)
        .replacen("https://", "http://", 1);
    let body: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert!(javdb::parse_latest_page(&body, 0, 1).is_err());
}

/// 榜单声明：与上游 boards.py 的来源/榜单一致。
///
/// 注意：v0.2.0 的 `RankingBoard` 只有 `board_key` + `display_name`（周期集合是
/// 后加的字段），周期合法性在 `fetch_ranking` 里按上游的 `MINNANO_PERIODS` /
/// `JAVLIBRARY_PERIODS` 校验。
#[tokio::test]
async fn ranking_sources_match_upstream() {
    let sources = plugin_more_movies::service::ranking_sources();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].source_key, "minnano_av");
    assert_eq!(sources[0].boards.len(), 1);
    assert_eq!(sources[0].boards[0].board_key, "minnano_av");
    assert_eq!(sources[0].boards[0].display_name, "Minnano AV");
    assert_eq!(sources[1].source_key, "javlibrary");
    assert_eq!(sources[1].boards.len(), 2);
    assert_eq!(sources[1].boards[0].board_key, "javlibrary_bestrated");
    assert_eq!(sources[1].boards[1].board_key, "javlibrary_mostwanted");
}
