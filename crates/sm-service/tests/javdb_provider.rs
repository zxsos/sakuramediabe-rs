//! `JavdbProvider` 的**出网路径**测试 —— 用回环 HTTP 服务打桩。
//!
//! # 为什么必须打桩
//!
//! 三个断言只有在真发请求时才测得到，而它们恰好是「照抄上游」最容易抄错的地方：
//!
//! | 断言 | 抄错的后果 |
//! |---|---|
//! | 候选里挑**番号完全相等**的那一个（且发行日期**新的优先**）| 导入成同前缀的另一部片（`ABC-123` ↔ `ABC-1234`）|
//! | 详情返回的是 `data.movie`，**不是整个信封** | 宿主按 `detail["id"]` 读时拿到 `null`，而入库静默建了空记录 |
//! | `success != 1` 是**请求失败**，不是「没收录」| 把它当 404 → 用户看到「JavDB 没这部片」，而真实原因是服务端拒绝了请求 |
//! | 每个请求都带**当场算的** `jdsignature` | 对端回 `ParameterInvalid`（HTTP 200），而**搜索**路径不查 `success` → 报「没这部片」|
//!
//! 打桩方式：`JavdbProvider::with_base_url`（生产用 `new(host)` 拼 `https://`）。
//! 那个缝存在的理由见它的文档。

use sm_service::catalog::javdb::{signature_at, JavdbProvider};
use sm_service::catalog::metadata_source::{MetadataProvider, MetadataSourceError};
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

fn provider_for(server: &MockServer) -> JavdbProvider {
    JavdbProvider::with_base_url(&server.uri()).expect("构造 provider")
}

#[tokio::test]
async fn by_number_picks_the_exact_match_with_the_newest_release_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/search"))
        // 归一后的番号进了查询参数 —— 大写化与分隔符归一都在这一层生效。
        .and(query_param("q", "ABC-123"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_body(serde_json::json!([
                // 精确匹配但**较旧** —— 排序后应当被较新的那条压下去。
                { "id": "OLD", "number": "ABC-123", "release_date": "2020-01-01" },
                // 前缀相同但**不是同一部片** —— 精确相等不该选中它。
                { "id": "LONGER", "number": "ABC-1234", "release_date": "2024-01-01" },
                // 精确匹配且最新 —— 这是要被选中的。
                { "id": "NEW", "number": "abc-123", "release_date": "2024-06-01" },
            ]))),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/NEW"))
        .respond_with(ResponseTemplate::new(200).set_body_json(detail_body(
            serde_json::json!({ "id": "NEW", "number": "ABC-123" }),
        )))
        .mount(&server)
        .await;

    let detail = provider_for(&server)
        .get_movie_by_number("ABC-123")
        .await
        .expect("应当查到")
        .expect("应当有详情");

    // 返回的是 `data.movie`，不是 `{success, data}` 信封。
    assert_eq!(detail["id"], "NEW", "要选发行日期最新的那条精确匹配");
    assert!(
        detail.get("success").is_none(),
        "★ 详情必须是 data.movie 本身，不带信封"
    );
}

#[tokio::test]
async fn by_javdb_id_skips_the_search_entirely() {
    let server = MockServer::start().await;
    // **不**挂搜索 mock：若实现多打了一次搜索，这里会 404。
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/A123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(detail_body(
            serde_json::json!({ "id": "A123", "title": "X" }),
        )))
        .mount(&server)
        .await;

    let detail = provider_for(&server)
        .get_movie_by_javdb_id("A123")
        .await
        .expect("应当查到")
        .expect("应当有详情");
    assert_eq!(detail["title"], "X");
}

/// ★ 每个请求都必须带 `jdsignature`，且是**当场算的**。
///
/// 这条只有**真发一次请求**才测得到：wiremock 不校验请求头，所以「头漏了」在
/// 其它用例里完全静默。而它的后果是最难排查的那一类 —— 对端回 HTTP **200** +
/// `{"success":0,"action":"ParameterInvalid"}`，搜索路径不查 `success`（上游
/// `_search_movie` 亦然）于是报 `NotFound`：用户看到「JavDB 没收录**任何**番号」。
#[tokio::test]
async fn every_request_carries_a_fresh_jdsignature() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/A123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(detail_body(serde_json::json!({ "id": "A123" }))),
        )
        .mount(&server)
        .await;

    provider_for(&server)
        .get_movie_by_javdb_id("A123")
        .await
        .expect("应当查到");

    let requests = server
        .received_requests()
        .await
        .expect("打桩服务应当记录到请求");
    assert_eq!(requests.len(), 1, "只该发一次请求");
    let headers = &requests[0].headers;

    assert_eq!(
        headers
            .get("accept-language")
            .expect("★ 少了 accept-language")
            .to_str()
            .expect("该是 ASCII"),
        "zh-TW"
    );

    let signature = headers
        .get("jdsignature")
        .expect("★ 少了 jdsignature：JavDB 会回 ParameterInvalid，而搜索路径把它当成「没这部片」")
        .to_str()
        .expect("该是 ASCII");
    let (timestamp, _) = signature
        .split_once('.')
        .expect("形状该是 {timestamp}.lpw6vgqzsp.{md5}");
    let timestamp: i64 = timestamp.parse().expect("前缀该是 Unix 秒");

    // 值与上游算法一致。算法/secret 本身的正确性由 `javdb.rs` 单元测试里那个
    // **跨实现**的固定向量锚定，这里只保证「发出去的确实是它」。
    assert_eq!(signature, signature_at(timestamp));

    // 新鲜度：必须带**这一刻**的时间戳，而不是某个缓存的常量。10s 容差吸收慢机器。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟早于 1970")
        .as_secs() as i64;
    assert!(
        (now - timestamp).abs() <= 10,
        "签名该是每次请求现算的（now={now} ts={timestamp}）"
    );
}

#[tokio::test]
async fn a_business_failure_is_a_request_error_not_a_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/A123"))
        // HTTP 200，但 `success != 1` —— JavDB 的「业务失败」形状。
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "success": 0, "message": "rate limited" })),
        )
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .get_movie_by_javdb_id("A123")
        .await
        .expect_err("业务失败该报错");
    assert!(
        matches!(error, MetadataSourceError::RequestFailed(_)),
        "★ 不能当成「没收录」（那会让用户以为 JavDB 没有这部片）：{error:?}"
    );
}

#[tokio::test]
async fn a_detail_without_a_movie_object_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/GONE"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "success": 1, "data": {} })),
        )
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .get_movie_by_javdb_id("GONE")
        .await
        .expect_err("没有 movie 对象该是 NotFound");
    assert!(matches!(error, MetadataSourceError::NotFound), "{error:?}");
}

#[tokio::test]
async fn no_exact_number_match_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_body(serde_json::json!([
                { "id": "A", "number": "ZZZ-999", "release_date": "2024-01-01" },
                { "id": "B", "number": "ABC-1234", "release_date": "2024-01-01" },
            ]))),
        )
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .get_movie_by_number("ABC-123")
        .await
        .expect_err("没有精确匹配该是 NotFound");
    assert!(
        matches!(error, MetadataSourceError::NotFound),
        "★ 前缀相同不等于同一部片：{error:?}"
    );
}

#[tokio::test]
async fn an_empty_candidate_list_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_body(serde_json::json!([]))))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .get_movie_by_number("ABC-123")
        .await
        .expect_err("空候选该是 NotFound");
    assert!(matches!(error, MetadataSourceError::NotFound), "{error:?}");
}

#[tokio::test]
async fn an_http_error_is_reported_as_a_request_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/A123"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .get_movie_by_javdb_id("A123")
        .await
        .expect_err("503 该报错");
    match error {
        MetadataSourceError::RequestFailed(message) => {
            assert!(message.contains("503"), "{message}");
            // 响应体要**截断** —— 一个几 MB 的错误页塞进错误消息会撑爆日志。
            assert!(message.len() < 400, "错误消息不该包含整个响应体");
        }
        other => panic!("应当是 RequestFailed，实际 {other:?}"),
    }
}

#[tokio::test]
async fn a_non_json_body_is_reported_as_a_request_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/movies/A123"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>captcha</html>"))
        .mount(&server)
        .await;

    let error = provider_for(&server)
        .get_movie_by_javdb_id("A123")
        .await
        .expect_err("HTML 响应该报错");
    assert!(
        matches!(error, MetadataSourceError::RequestFailed(_)),
        "{error:?}"
    );
}

/// ★ 演员搜索**显式报未实现**，不是返回空列表。
///
/// 返回空列表会让调用方（演员 SSE）报「导入 0 个」—— 那是**谎报**：用户看到
/// 「没搜到」而不是「搜不了」。
#[tokio::test]
async fn actor_search_says_not_implemented_instead_of_returning_empty() {
    let server = MockServer::start().await;
    let error = provider_for(&server)
        .search_actors("演员名")
        .await
        .expect_err("★ 未实现就该报错");
    match error {
        MetadataSourceError::RequestFailed(message) => {
            assert!(message.contains("尚未移植"), "{message}");
        }
        other => panic!("应当是 RequestFailed，实际 {other:?}"),
    }
}
