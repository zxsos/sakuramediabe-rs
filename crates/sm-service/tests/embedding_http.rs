//! `EmbeddingClient` 的 HTTP 契约测试。
//!
//! 按 ADR §6 的口径：HTTP 路径用 `wiremock` 起**真实回环服务**测，不 mock
//! `reqwest::Client`。理由是这一层要验的东西全在 wire 上 —— 方法、路径、
//! `Authorization` 头、multipart 的字段名与文件名、状态码透传。mock 掉
//! 客户端就等于把这些全 mock 掉了，测出来的东西没有意义。
//!
//! 这一组**不需要数据库**（client 是纯 HTTP），所以不调 `TestDb::require()` ——
//! 有库时跑、无库时也跑。
//!
//! 参照物：上游 `upstream/sakuramediabe/src/service/discovery/embedding_client.py`。

use std::time::Duration;

use serde_json::json;
use sm_service::discovery::{EmbeddingClient, EmbeddingSpace};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 起一个 client，指向假推理服务。`api_key` 为 `None` 时不发 Authorization 头。
fn client_for(server: &MockServer, api_key: Option<&str>) -> EmbeddingClient {
    EmbeddingClient::with_http_client(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .expect("构建测试用 HTTP 客户端"),
        server.uri().to_string(),
        api_key.map(str::to_owned),
    )
}

fn space_body(space_id: &str, dimension: i64, modalities: &[&str]) -> serde_json::Value {
    json!({
        "space_id": space_id,
        "dimension": dimension,
        "modalities": modalities,
    })
}

// ---------------------------------------------------------------- describe

/// 正常路径：GET `/v1/embedding-space`，带 `Authorization: Bearer`。
#[tokio::test]
async fn describe_reads_the_space_over_get() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/embedding-space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(space_body(
            "clip-vit-b32",
            512,
            &["image", "text"],
        )))
        .mount(&server)
        .await;

    let space = client_for(&server, Some("secret-key"))
        .describe()
        .await
        .expect("describe 应成功");

    assert_eq!(space.space_id, "clip-vit-b32");
    assert_eq!(space.dimension, 512);
    assert!(space.modalities.contains("image"));
    assert!(space.modalities.contains("text"));
    assert!(space.is_usable());

    // wire 上必须真的带了 Bearer 头 —— 少了它真实服务会 401
    let requests = server.received_requests().await.expect("应记录到请求");
    assert_eq!(requests.len(), 1);
    let auth = requests[0]
        .headers
        .get("authorization")
        .expect("应带 Authorization 头");
    assert_eq!(auth.to_str().expect("头应可读"), "Bearer secret-key");
}

/// `api_key` 为空时**不发** Authorization 头，而不是发一个空的 `Bearer `。
///
/// 上游 `:39-40` 是 `if self.api_key:`，空值跳过。发空 Bearer 会被真实服务
/// 当成「提供了无效凭据」而 401，比不发更难排查。
#[tokio::test]
async fn no_api_key_means_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/embedding-space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(space_body(
            "s",
            4,
            &["image", "text"],
        )))
        .mount(&server)
        .await;

    client_for(&server, None)
        .describe()
        .await
        .expect("无 api_key 也应成功");

    let requests = server.received_requests().await.expect("应记录到请求");
    assert!(
        !requests[0].headers.contains_key("authorization"),
        "api_key 为 None 时不得发 Authorization 头，实际头：{:?}",
        requests[0].headers
    );
}

/// 尾斜杠要被去掉：`http://host:port/` + `/v1/...` 不能拼成 `//v1/...`。
#[tokio::test]
async fn trailing_slash_in_base_url_does_not_double_the_path_separator() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/embedding-space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(space_body(
            "s",
            4,
            &["image", "text"],
        )))
        .mount(&server)
        .await;

    let client = EmbeddingClient::with_http_client(
        reqwest::Client::new(),
        format!("{}/", server.uri()),
        None,
    );
    client.describe().await.expect("尾斜杠应被裁掉");
}

// ------------------------------------------------- 4xx/5xx 状态码透传

/// **状态码透传远端** —— 这是最容易照抄错的一条。
///
/// 上游 `:59-64` 把 `status >= 400` 的响应原样抛出，所以 429 就该是 429、
/// 401 就该是 401。而 502/503 那两个是给「没有响应」的情况用的
/// （连接失败、响应不合法）。把远端的 429 改写成 502，客户端就分不清
/// 「我该退避重试」还是「对方明确拒绝、我改请求也没用」。
#[tokio::test]
async fn upstream_error_status_is_passed_through_not_rewritten() {
    for status in [400u16, 401, 404, 429, 500, 503] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/embedding-space"))
            .respond_with(ResponseTemplate::new(status).set_body_string("upstream said no"))
            .mount(&server)
            .await;

        let err = client_for(&server, None)
            .describe()
            .await
            .expect_err("4xx/5xx 必须报错");
        assert_eq!(
            err.status, status,
            "远端 {status} 必须原样透传，不能被改写成 502"
        );
        assert_eq!(err.code(), "image_search_inference_failed");
    }
}

// ------------------------------------------------------ 传输层 → 503

/// 连不上 → 503 `unavailable`。
///
/// 这条与 502 的分界是**能不能重试**：503 值得退避重试，502 是「连上了但
/// 没成功」。**文案与「超时」那条相同** —— 见下面那条的说明。
#[tokio::test]
async fn unreachable_service_is_503_not_502() {
    // 绑一个临时端口再立刻放开，拿到一个确定没人听的端口
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("应能绑到临时端口");
        listener.local_addr().expect("应能读到地址").port()
    };
    let client = EmbeddingClient::new(
        format!("http://127.0.0.1:{closed}"),
        None,
        Duration::from_secs(2),
        Duration::from_millis(500),
    );
    let err = client.describe().await.expect_err("连不上必须报错");
    assert_eq!(err.status, 503, "连不上是 503，不是 502");
    assert_eq!(err.code(), "image_search_inference_unavailable");
    assert_eq!(
        err.api.message,
        "Embedding service is unreachable or timed out"
    );
}

/// 慢响应 → 同样 503、**同样的文案**。
///
/// 用 wiremock 的 `set_delay` 制造确定性的慢响应，而不是靠一个不可路由的
/// IP（那会 flaky）。
///
/// **为什么不按上游分两套文案**：上游 `TimeoutException`（`:45-48`）与
/// `NetworkError`（`:49-54`）的消息不同，但**状态码与错误码本来就一样**，
/// 而 reqwest 0.13 对这两种失败返回**完全相同**的分类（实测见
/// `probe_reqwest_error_classification`）。所以本 crate 合并成一条 ——
/// 硬按上游写两套文案，只会在「连接被拒」时**误报成超时**，那是编造
/// 一个底层给不出的信息。
#[tokio::test]
async fn slow_service_is_503_with_the_same_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/embedding-space"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(800))
                .set_body_json(space_body("s", 4, &["image", "text"])),
        )
        .mount(&server)
        .await;

    let client = EmbeddingClient::new(
        server.uri().to_string(),
        None,
        Duration::from_millis(120),
        Duration::from_millis(120),
    );
    let err = client.describe().await.expect_err("超时必须报错");
    assert_eq!(err.status, 503);
    assert_eq!(err.code(), "image_search_inference_unavailable");
    assert_eq!(
        err.api.message,
        "Embedding service is unreachable or timed out"
    );
}

// ------------------------------------------------- 响应体不合法 → 502

/// 三种「响应不合法」各自的消息文案（上游 `:67-101`）。
#[tokio::test]
async fn malformed_responses_map_to_distinct_502_messages() {
    let cases: [(&str, wiremock::ResponseTemplate, &str); 3] = [
        (
            "not json at all",
            ResponseTemplate::new(200).set_body_string("<html>gateway</html>"),
            "Embedding service returned invalid JSON",
        ),
        (
            "top level array",
            ResponseTemplate::new(200).set_body_string("[1,2,3]"),
            "Embedding service returned invalid payload",
        ),
        (
            "dimension zero",
            ResponseTemplate::new(200).set_body_json(space_body("s", 0, &["image", "text"])),
            "Embedding service returned invalid space",
        ),
    ];

    for (label, template, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/embedding-space"))
            .respond_with(template)
            .mount(&server)
            .await;

        let err = client_for(&server, None)
            .describe()
            .await
            .expect_err("不合法响应必须报错");
        assert_eq!(err.status, 502, "case={label}");
        assert_eq!(err.code(), "image_search_inference_failed", "case={label}");
        assert_eq!(err.api.message, expected, "case={label}");
    }
}

/// 只回图搜模态（缺 `text`）→ 不可用。
///
/// 这一条对应真实配置组合「开了图搜、推理服务只支持图搜」。
#[tokio::test]
async fn space_without_text_modality_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/embedding-space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(space_body("s", 512, &["image"])))
        .mount(&server)
        .await;

    let err = client_for(&server, None)
        .describe()
        .await
        .expect_err("缺 text 模态必须报错");
    assert_eq!(err.status, 502);
    assert_eq!(err.api.message, "Embedding service returned invalid space");
}

// --------------------------------------------------------- embed_images

/// multipart 的字段名、文件名、content-type 都要对 —— 少一样真实服务就
/// 收不到图，而症状是「返回 0 条向量」。
#[tokio::test]
async fn embed_images_posts_multipart_with_repeated_files_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embed/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "vectors": [[0.1, 0.2], [0.3, 0.4]]
        })))
        .mount(&server)
        .await;

    let images = vec![vec![1u8, 2, 3], vec![4u8, 5, 6]];
    let vectors = client_for(&server, None)
        .embed_images(&images)
        .await
        .expect("批量嵌图应成功");
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[1], vec![0.3_f32, 0.4_f32]);

    let requests = server.received_requests().await.expect("应记录到请求");
    let raw = &requests[0].body;
    // 头部是 ASCII，但**图片字节不是** —— 用 lossy UTF-8 转换会把 [1,2,3]
    // 变成替换字符，所以头部断言走 lossy 字符串、字节断言走原始切片。
    let body = String::from_utf8_lossy(raw).to_string();

    // 字段名固定 `files`，两次 append 同名（上游 `:107-110`）
    assert_eq!(
        body.matches(r#"name="files""#).count(),
        2,
        "两个文件都要用 `files` 字段名，实际 body：{body}"
    );
    // 文件名带下标，且上游固定用 .png 后缀（哪怕内容不是 PNG）
    assert!(body.contains(r#"filename="image-0.png""#), "body={body}");
    assert!(body.contains(r#"filename="image-1.png""#), "body={body}");
    assert!(
        body.contains("application/octet-stream"),
        "每个 part 的 content-type 应为 application/octet-stream，body={body}"
    );
    // 图片字节要真的在 body 里。**在原始字节上找，不能在 lossy 字符串上找** ——
    // [1,2,3] 不是合法 UTF-8，转字符串后被替换成 U+FFFD，按字节匹配必然失败。
    assert!(
        raw.windows(3).any(|w| w == [1u8, 2, 3]),
        "第一张图的字节 [1,2,3] 应原样出现在 body 中"
    );
    assert!(
        raw.windows(3).any(|w| w == [4u8, 5, 6]),
        "第二张图的字节 [4,5,6] 应原样出现在 body 中"
    );
}

/// 返回条数与请求条数不符 → 502，**不能截断或补齐**。
///
/// 少一条意味着索引里少了那一张图，而调用方以为整批成功了。
#[tokio::test]
async fn embed_images_rejects_a_short_vector_list() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embed/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "vectors": [[0.1, 0.2]]
        })))
        .mount(&server)
        .await;

    let err = client_for(&server, None)
        .embed_images(&[vec![1u8], vec![2u8]])
        .await
        .expect_err("少给一条向量必须报错");
    assert_eq!(err.status, 502);
    assert_eq!(err.code(), "image_search_inference_failed");
    assert_eq!(
        err.api.message,
        "Embedding service returned invalid vectors"
    );
}

/// 空批次是合法 no-op，**不发请求**。
///
/// 上游 `:107-108` 是 `if not images: return []`，而 `embed_texts` 对空列表
/// 抛错（`:118`）—— 两者不对称是刻意的：批处理末批恰好为空时不该报错。
/// 断言「不发请求」是这条的关键，光断言返回值证明不了它短路了。
#[tokio::test]
async fn empty_image_batch_short_circuits_without_a_request() {
    let server = MockServer::start().await;
    // 故意不 mount 任何 Mock：真发请求会拿到 wiremock 的默认 404 而失败。
    let vectors = client_for(&server, None)
        .embed_images(&[])
        .await
        .expect("空批次应直接返回");
    assert!(vectors.is_empty());
    assert!(
        server
            .received_requests()
            .await
            .expect("可读请求记录")
            .is_empty(),
        "空批次不得发出任何 HTTP 请求"
    );
}

// --------------------------------------------------------- embed_texts

/// 请求体形状是 `{"texts": [...]}`，路径与方法要对。
#[tokio::test]
async fn embed_texts_posts_json_with_a_texts_array() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embed/texts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "vectors": [[0.5, 0.6], [0.7, 0.8]]
        })))
        .mount(&server)
        .await;

    let texts = vec!["第一条".to_owned(), "第二条".to_owned()];
    let vectors = client_for(&server, None)
        .embed_texts(&texts)
        .await
        .expect("批量嵌文本应成功");
    assert_eq!(vectors.len(), 2);

    let requests = server.received_requests().await.expect("应记录到请求");
    let sent: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("请求体应是 JSON");
    assert_eq!(sent["texts"][0], "第一条");
    assert_eq!(sent["texts"][1], "第二条");
}

/// 空列表与空白项都是 422（上游 `:118` 的 `not texts or any(...)`）。
#[tokio::test]
async fn embed_texts_rejects_empty_and_blank_queries_with_422() {
    let server = MockServer::start().await;
    let client = client_for(&server, None);

    let empty = client.embed_texts(&[]).await.expect_err("空列表必须 422");
    assert_eq!(empty.status, 422, "空列表不是「没有结果」，是无效请求");

    let blank = client
        .embed_texts(&["有内容".to_owned(), "\t \n".to_owned()])
        .await
        .expect_err("含空白项必须 422");
    assert_eq!(blank.status, 422);

    // 全空白
    let all_blank = client
        .embed_texts(&["   ".to_owned()])
        .await
        .expect_err("全空白必须 422");
    assert_eq!(all_blank.status, 422);
}

/// 两个 embed 端点的空输入语义相反，所以文本的空输入**不得**发请求。
#[tokio::test]
async fn empty_text_query_is_rejected_before_any_request() {
    let server = MockServer::start().await;
    let _ = client_for(&server, None).embed_texts(&[]).await;
    assert!(
        server
            .received_requests()
            .await
            .expect("可读请求记录")
            .is_empty(),
        "空文本查询应在本地判掉，不该发到推理服务"
    );
}

// ------------------------------------------------------------ 类型导出

/// `EmbeddingSpace` 是公开类型，索引任务要拿它比对维度。
#[test]
fn embedding_space_is_constructible_and_validates_itself() {
    let space = EmbeddingSpace {
        space_id: "clip-vit-b32".to_owned(),
        dimension: 512,
        modalities: ["image".to_owned(), "text".to_owned()]
            .into_iter()
            .collect(),
    };
    assert!(space.is_usable());
    assert_eq!(space.dimension, 512);
}

// ------------------------------------------------- 错误分类的实测探针

/// **实测断言**：reqwest 0.13 区分不了「连接被拒」与「连接超时」。
///
/// 这条测试存在的理由是**它本来会失败**。写它的时候，`unreachable` 用例断言
/// 连接被拒应得到 "is unreachable"，而实际拿到 "Embedding service timed out"
/// —— 模块文档里那条「超时必须判在连接失败之前」的规则，其**前提**就是这两类
/// 能被区分。实测证明不能，于是规则被删掉、改成合并文案。
///
/// 留成永久测试的理由：将来升级 reqwest 后如果两者**变得可区分**，这条会
/// 失败，那正是把分界加回来的时机。**靠人记得去查升级日志，是发现不了这种
/// 变化的** —— 而它决定的是线上报错文案指向「地址写错了」还是「服务变慢了」。
#[tokio::test]
async fn reqwest_cannot_distinguish_refused_from_timed_out() {
    // 两个语义不同但都连不上的端口：
    //  - 保留端口 1：立刻被拒（ECONNREFUSED）
    //  - 绑过再放开的临时端口：同样立刻被拒，但走的是正常路径
    let released = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("应能绑到临时端口");
        listener.local_addr().expect("应能读到地址").port()
    };

    let mut observed = Vec::new();
    for (label, port) in [("reserved_port_1", 1u16), ("released_ephemeral", released)] {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .connect_timeout(Duration::from_millis(500))
            .no_proxy()
            .build()
            .expect("构建客户端");
        let error = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("这些端口上不该有服务在听");
        observed.push((
            label,
            error.is_timeout(),
            error.is_connect(),
            error.is_request(),
        ));
    }

    for (label, is_timeout, is_connect, _) in &observed {
        // 若某个端口返回的不是「连接期失败」，说明 reqwest 改了行为，
        // 那正是本测试要提醒重新评估文案分界的时刻。
        assert!(
            *is_connect,
            "{label} 应被判为连接期错误（is_connect），实际 is_timeout={is_timeout}"
        );
        // **关键断言**：被拒与超时**无法区分**，两者都是 is_timeout=true。
        // 正是这个实测结果让上游「timed out / is unreachable」的分界在
        // Rust 侧被合并成一条消息。
        assert!(
            *is_timeout,
            "{label} 实测 is_timeout=true —— 与「超时」不可区分，\
             这条断言若失败说明 reqwest 升级后行为已变，可以把文案分界加回来"
        );
    }

    // 两类失败的分类完全相同 —— 打印出来便于在 CI 日志里核对。
    println!("reqwest 错误分类实测：{observed:?}");
}
