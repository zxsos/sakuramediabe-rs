//! 慢请求日志中间件的端到端行为。
//!
//! # 断言三件事
//!
//! 1. **关闭时层根本不存在** —— 由组合根决定（[`config_is_none_means_no_layer`]），
//!    不是「挂了层但内部不记」。
//! 2. **响应逐字节透传** —— 中间件包住 future，但它绝不能改状态码、头或 body。
//! 3. **超阈值才记一条 warning，且带齐字段** —— 字段名要与上游
//!    `slow request method= path= status= duration_ms= request_id=` 对齐，
//!    否则运维按 `path=` 过滤日志会筛不到。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use sm_api::middleware::slow_log::{slow_request_logger, SlowLogConfig};
use tower::ServiceExt;

/// 收集日志输出的 writer。
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("锁").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// `/slow` 的 handler **自己**睡 30ms。
///
/// 这一点很关键：中间件在 `next.run(request).await` 返回时就判定耗时，
/// 那是**响应头**产出的时刻，body 还在流。所以「让请求变慢」必须发生在
/// handler 内；在测试里读完 body 之后再 sleep 是测不到东西的。
const HANDLER_SLEEP_MS: u64 = 30;

fn app() -> Router {
    Router::new()
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(HANDLER_SLEEP_MS)).await;
                "done"
            }),
        )
        .route("/fast", get(|| async { "done" }))
}

/// 挂上慢日志层后的 app。
fn app_with(config: SlowLogConfig) -> Router {
    app().layer(axum::middleware::from_fn(move |req, next| {
        slow_request_logger(config, req, next)
    }))
}

/// 跑一次请求，返回 `(状态码, body, 日志)`。
async fn run(router: Router, uri: &str) -> (StatusCode, String, String) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    // guard 必须**跨 await 持有**，所以用 `set_default`（返回 guard）而不是
    // `with_default(closure)`：后者在闭包**返回 future 之后**就把 guard 撤了，
    // 而 future 是在闭包外被 await 的 —— 结果一条日志都抓不到。
    //
    // `set_default` 是 thread-local，`#[tokio::test]` 默认 current_thread
    // runtime，整个请求都在同一线程上，所以这里成立。
    let guard = tracing::subscriber::set_default(subscriber);
    let result = async {
        let request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("构造请求");
        let response = router.oneshot(request).await.expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读 body")
            .to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
    .await;
    drop(guard);

    let logs = String::from_utf8(capture.0.lock().expect("锁").clone()).expect("日志是 UTF-8");
    (result.0, result.1, logs)
}

#[test]
fn config_is_none_means_no_layer() {
    // 组合根据此**不挂层**：关闭时中间件不存在，零分配、零计时。
    assert_eq!(SlowLogConfig::from_env_with(Some("0"), None), None);
    assert_eq!(SlowLogConfig::from_env_with(None, Some("1")), None);
    assert!(SlowLogConfig::from_env_with(Some("yes"), Some("10")).is_some());
}

#[tokio::test]
async fn the_response_passes_through_unchanged() {
    // 中间件包住 future，最容易出的错是顺手改了状态码或吞了 body。
    let (status, body, _) = run(app_with(SlowLogConfig { threshold_ms: 1 }), "/fast").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "done");
}

#[tokio::test]
async fn a_slow_request_is_logged_with_the_upstream_fields() {
    // 阈值 1ms + handler 睡 30ms：留足余量，慢 CI 上也不会偶发失败。
    let (status, _, logs) = run(app_with(SlowLogConfig { threshold_ms: 1 }), "/slow").await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        logs.contains("slow request"),
        "应当有一条慢请求日志，实际：{logs}"
    );
    for field in ["method", "path", "status", "duration_ms", "request_id"] {
        assert!(
            logs.contains(field),
            "日志缺少字段 {field}；运维按这些字段过滤，实际：{logs}"
        );
    }
    assert!(logs.contains("/slow"), "应当记下 path，实际：{logs}");
    assert!(logs.contains("200"), "应当记下 status，实际：{logs}");
}

#[tokio::test]
async fn a_fast_request_is_not_logged() {
    // 阈值 10 秒：再快的请求也不该产生日志。
    let (_, _, logs) = run(
        app_with(SlowLogConfig {
            threshold_ms: 10_000,
        }),
        "/fast",
    )
    .await;
    assert!(
        !logs.contains("slow request"),
        "未超阈值不该记日志，实际：{logs}"
    );
}

#[tokio::test]
async fn the_query_string_is_not_logged() {
    // 上游记的是 `scope["path"]`，不含 query。带 token 的 URL 进日志
    // 等于把凭据写进磁盘。
    let (status, _, logs) = run(
        app_with(SlowLogConfig { threshold_ms: 1 }),
        "/slow?token=super-secret",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(logs.contains("slow request"), "应当照常记日志：{logs}");
    assert!(!logs.contains("super-secret"), "凭据不该进日志：{logs}");
    assert!(logs.contains("/slow"), "应当仍记 path：{logs}");
}
