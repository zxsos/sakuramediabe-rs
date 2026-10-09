//! `extract::Multipart` 的 HTTP 集成测试。
//!
//! # 为什么手拼 body 而不是用客户端
//!
//! 这个提取器要处理的**全部内容**就是边界情况：boundary 缺失、chunk 截断、
//! 超限。高层客户端会替你把 body 拼对，所以这些一个也构造不出来。
//!
//! 上游对应：FastAPI 的 `UploadFile = File(...)`，走同一个
//! `RequestValidationError` → 422 信封。

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sm_api::extract::Multipart;
use tower::ServiceExt;

/// 端点侧的显式上限。取得比默认小，让「超限」在小 body 上就能触发 ——
/// 否则每个用例都得造 8 MiB。
const LIMIT: usize = 64;

const BOUNDARY: &str = "sm-api-test-boundary";

/// 收集所有文件字段的端点。`Multipart` 只能作 handler 参数（它要读 body），
/// 这个路由同时锁定该用法。
fn app() -> Router {
    Router::new()
        .route(
            "/upload",
            post(|State(_): State<()>, files: Multipart| async move {
                // 上限设置一次（builder 消耗 self），然后循环取字段 ——
                // 「多个文件、每个都受限」是这个提取器的常态。
                let mut files = files.with_max_bytes(LIMIT);
                let mut received = Vec::new();
                while let Some(file) = files.next_file().await? {
                    received.push(json!({
                        "field": file.field,
                        "file_name": file.file_name,
                        "content_type": file.content_type,
                        "len": file.bytes.len(),
                    }));
                }
                Ok::<_, sm_api::ErrorResponse>(Json(json!({ "files": received })))
            }),
        )
        .with_state(())
}

/// 拼一个 multipart body。`file_name` 为 `None` 时是普通文本字段。
fn multipart_body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (field, file_name, bytes) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match file_name {
            Some(name) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{field}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn request(body: Vec<u8>, boundary: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/upload")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("构造请求")
}

async fn call(router: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读 body")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn a_single_file_upload_round_trips() {
    let body = multipart_body(&[("cover", Some("0.webp"), b"hello-webp")]);
    let (status, json) = call(app(), request(body, BOUNDARY)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"][0]["field"], "cover");
    assert_eq!(json["files"][0]["file_name"], "0.webp");
    assert_eq!(json["files"][0]["content_type"], "application/octet-stream");
    assert_eq!(json["files"][0]["len"], 10);
}

#[tokio::test]
async fn several_files_are_returned_in_order() {
    let body = multipart_body(&[
        ("first", Some("a.bin"), b"12345"),
        ("second", Some("b.bin"), b"678"),
    ]);
    let (status, json) = call(app(), request(body, BOUNDARY)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"].as_array().map(Vec::len), Some(2));
    assert_eq!(json["files"][0]["len"], 5);
    assert_eq!(json["files"][1]["len"], 3);
}

#[tokio::test]
async fn an_oversized_file_is_413_with_the_limit_in_details() {
    // 上限 64 字节，造 200 字节。关键是**读到超限就返回**，
    // 而不是先读完再判 —— 那样 1 GB 的上传会先把内存吃满。
    let body = multipart_body(&[("cover", Some("big.bin"), &[b'x'; 200])]);
    let (status, json) = call(app(), request(body, BOUNDARY)).await;

    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "超限必须是 413 而不是 422 —— 客户端据此提示「换个文件」"
    );
    let error = &json["error"];
    assert_eq!(error["code"], "http_error");
    assert_eq!(error["details"]["field"], "cover");
    assert_eq!(error["details"]["max_bytes"], LIMIT as i64);
}

#[tokio::test]
async fn a_missing_boundary_is_422_not_400() {
    // axum 的 `MultipartRejection` 是 400，上游
    // `RequestValidationError` 是 422。客户端对两者的重试策略不同，
    // 所以必须映射而不是透传。
    let (status, json) = call(app(), request(b"whatever".to_vec(), "no-such-boundary")).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"]["code"], "validation_error");
    assert_eq!(json["error"]["message"], "Request validation failed");
}

#[tokio::test]
async fn a_non_multipart_content_type_is_422() {
    let request = Request::builder()
        .method("POST")
        .uri("/upload")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("构造请求");
    let (status, json) = call(app(), request).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"]["code"], "validation_error");
}
