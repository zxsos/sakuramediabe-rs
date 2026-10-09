//! 推理（embedding）服务客户端 —— 把图片/文本变成向量。
//!
//! # 这个服务是**可替换的外部依赖**，不是本仓库的一部分
//!
//! 上游的 `embedding_client.py` 走 HTTP 调一个独立服务，本文件照搬那份契约。
//! **不要把它改成「本地跑一个 CLIP」** —— 三个理由：
//!
//! 1. `image_search.inference_api_key` 这个配置项的存在就否定了本地模型：
//!    本地模型没有「API key」可言。上游的设计前提就是远端服务。
//! 2. `describe()` 返回的 `space_id` / `dimension` 正好喂给
//!    [`sm_db::discovery::ImageSearchIndexState`]（其 `space_id` 注释写着
//!    「Qdrant collection / space id」）与 `accepts_session()` 的维度校验。
//!    **那套错配防护只有在推理服务可替换时才有意义** —— 换模型是改
//!    `inference_base_url`，不是改代码。
//! 3. 内置模型要引入 ONNX Runtime 或 candle + 模型下载 + 模型生命周期管理。
//!    那才是真正重的东西，而产物是塞进 `sakuramedia` 镜像的静态二进制。
//!
//! # 契约（`embedding_client.py` 逐条照搬）
//!
//! | 上游 | 端点 | 请求 | 响应 |
//! |---|---|---|---|
//! | `:85-102` | `GET /v1/embedding-space` | — | `{space_id, dimension, modalities}` |
//! | `:104-114` | `POST /v1/embed/images` | multipart，字段名 `files` | `{vectors}` |
//! | `:116-124` | `POST /v1/embed/texts` | `{"texts": [...]}` | `{vectors}` |
//!
//! # 三处容易照抄错的地方
//!
//! **1. `images` 与 `texts` 的空输入语义相反。** 上游 `embed_images` 对空列表
//! 直接返回 `[]`（`:107-108`），而 `embed_texts` 对空列表**抛
//! `ValueError`**（`:118`，条件是 `not texts or any(...)`）—— 空列表也抛。
//! 这不是笔误：`texts` 那个 `not texts` 拦的是「空查询」，而 `images` 的空
//! 批次是合法的 no-op（批处理循环里会走到）。写成一样会让批处理在末批
//! 恰好为空时报错。
//!
//! **2. 上游的「超时 vs 不可达」区分在 Rust 侧复现不了，已合并。**
//! 上游把 `TimeoutException`（`:45-48`）与 `NetworkError`（`:49-54`）分成两条
//! 消息，但**两者的状态码与错误码本来就相同**，只有文案不同。而 reqwest 0.13
//! 对「连接被拒」和「连接超时」返回**完全相同**的分类（实测见
//! `Self::map_send_error` 的文档与
//! `tests/embedding_http.rs::probe_reqwest_error_classification`）。
//! 所以这里合成一条消息，而不是靠判断顺序去伪造一个不存在的区分。
//!
//! **3. `status >= 400` 那一支透传远端状态码**（`:59-64`），其余全是固定的
//! 502/503。所以这一支**不能**用 `ServiceError` 的构造器 —— 状态码由远端
//! 决定，得直接构造结构体。

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::error::{ProgrammerError, ServiceError};

/// 上游固定的错误码。
mod code {
    /// 网络层失败（超时/不可达）→ 503。
    pub const UNAVAILABLE: &str = "image_search_inference_unavailable";
    /// 协议层失败（4xx/5xx 透传、响应不合法、向量不合法）→ 502。
    pub const FAILED: &str = "image_search_inference_failed";
    /// 请求形状不对（空查询）→ 422。上游抛 `ValueError`，FastAPI 也回 422。
    pub const VALIDATION: &str = "validation_error";
}

/// 推理服务声明的嵌入空间。
///
/// 三个字段都不是装饰 —— [`Self::is_usable`] 的三条校验逐条对应上游
/// `describe()`（`:92-101`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpace {
    /// 空间标识。进 Qdrant 是 collection 名，进
    /// [`sm_db::discovery::ImageSearchIndexState`] 是 `space_id`。
    pub space_id: String,
    /// 向量维度。**必须与库里已索引的维度一致**，否则
    /// `ImageSearchIndexState::accepts_session` 会拒绝会话。
    pub dimension: usize,
    /// 支持的模态。上游要求 `{"image","text"}` 都在里面。
    pub modalities: BTreeSet<String>,
}

impl EmbeddingSpace {
    /// 是否可用：标识非空、维度为正、图搜与文搜两个模态都在。
    ///
    /// 照抄上游 `:92-101`。三条都缺一不可，但**报错时不区分是哪条** ——
    /// 上游只回一句「invalid space」，刻意不告诉调用方「你换了个维度更小的
    /// 模型」。这里保持一致：泄露具体哪条不满足，等于帮调用方省掉了它该做
    /// 的判断（维度不符时应该重建索引，而不是重试）。
    pub fn is_usable(&self) -> bool {
        !self.space_id.is_empty()
            && self.dimension > 0
            && self.modalities.contains("image")
            && self.modalities.contains("text")
    }

    /// 从 `GET /v1/embedding-space` 的响应体解析。
    ///
    /// 上游用 `str(payload.get("space_id") or "")` 与
    /// `int(payload.get("dimension") or 0)`，**缺失与零值同归为无效**。
    /// 这里照搬：字段缺失、类型不对、非数值，全部落到默认值再被
    /// [`Self::is_usable`] 判掉，不额外报错。
    fn from_payload(payload: &Map<String, Value>) -> Self {
        Self {
            space_id: payload
                .get("space_id")
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    // 上游 `str(...)`：数字/布尔会被转成字符串形态。
                    other => other.to_string(),
                })
                .unwrap_or_default(),
            dimension: payload
                .get("dimension")
                .and_then(|value| match value {
                    Value::Number(number) => number.as_f64(),
                    // 上游 `int(...)` 对字符串会抛 ValueError 被外面捕获成
                    // 「invalid payload」；这里按 0 处理，最终同样判为不可用。
                    Value::String(text) => text.parse::<f64>().ok(),
                    _ => None,
                })
                .map(|value| value.max(0.0) as usize)
                .unwrap_or(0),
            modalities: payload
                .get("modalities")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .map(|value| match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// 推理服务客户端。
///
/// 结构与 `crate::transfers::TorznabClient` 一致：`http` 客户端 + 配置字段，
/// `new()` 建默认、`with_http_client()` 给测试注入。
#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl EmbeddingClient {
    /// 按配置建客户端。
    ///
    /// - `base_url`：对应 `image_search.inference_base_url`。上游做
    ///   `.rstrip("/")`（`:26`），这里同样去尾斜杠 —— 留着就会拼出 `//v1/...`。
    /// - `api_key`：对应 `image_search.inference_api_key`，**可空**。为空时
    ///   不发 `Authorization` 头（`:39-40`），而不是发一个空的 `Bearer`。
    /// - `timeout` / `connect_timeout`：对应 `inference_timeout_seconds` 与
    ///   `inference_connect_timeout_seconds`。**两个都要**：上游用
    ///   `httpx.Timeout(total, connect=...)` 把它们分开，因为「连不上」
    ///   （connect 超时）与「连上了但太慢」（total 超时）的处置不同。
    /// - `batch_size` 不在这里 —— 它是调用方的循环边界，不是 HTTP 参数。
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            // 对应上游 httpx 的 `trust_env=False`（`:35`）。否则环境里的
            // `HTTP_PROXY` 会把请求导到别处，而症状表现为「超时」——
            // 与真的推理服务慢无法区分。同 `TorznabClient` 的理由。
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
        }
    }

    /// 注入自定义 HTTP 客户端（测试用）。
    pub fn with_http_client(
        http: reqwest::Client,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
        }
    }

    /// 推理服务声明的嵌入空间（`GET /v1/embedding-space`）。
    ///
    /// 索引任务**必须先调它**：Qdrant 里已有 collection 的维度与本服务不一致
    /// 时，唯一正确的动作是重建索引，而不是继续写。
    pub async fn describe(&self) -> Result<EmbeddingSpace, ServiceError> {
        let payload = self
            .payload(self.request("GET", "/v1/embedding-space", None).await?)
            .await?;
        let space = EmbeddingSpace::from_payload(&payload);
        if !space.is_usable() {
            return Err(Self::failed("Embedding service returned invalid space"));
        }
        Ok(space)
    }

    /// 图片 → 向量（`POST /v1/embed/images`）。
    ///
    /// **空批次返回空向量列表，不报错** —— 见模块文档第 1 条。
    pub async fn embed_images(&self, images: &[Vec<u8>]) -> Result<Vec<Vec<f32>>, ServiceError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let mut form = reqwest::multipart::Form::new();
        for (index, image) in images.iter().enumerate() {
            let part = reqwest::multipart::Part::bytes(image.clone())
                .file_name(format!("image-{index}.png"))
                .mime_str("application/octet-stream")
                .map_err(|error| ProgrammerError::new(format!("常量 MIME 字符串无效: {error}")))?;
            // 字段名固定 `files`，多次 append 同名 —— multipart 允许重复字段，
            // 上游靠它在一个请求里传一批图（`:107-110`）。
            form = form.part("files", part);
        }
        let payload = self
            .payload(
                self.request(
                    "POST",
                    "/v1/embed/images",
                    Some(RequestBody::Multipart(form)),
                )
                .await?,
            )
            .await?;
        Self::vectors(&payload, images.len())
    }

    /// 文本 → 向量（`POST /v1/embed/texts`）。
    ///
    /// **空列表或含空白项都报 422** —— 见模块文档第 1 条。
    pub async fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ServiceError> {
        if texts.is_empty() || texts.iter().any(|text| text.trim().is_empty()) {
            return Err(ServiceError::validation(
                code::VALIDATION,
                "text query must not be empty",
            ));
        }
        let body = serde_json::json!({ "texts": texts });
        let payload = self
            .payload(
                self.request("POST", "/v1/embed/texts", Some(RequestBody::Json(body)))
                    .await?,
            )
            .await?;
        Self::vectors(&payload, texts.len())
    }

    /// 发请求并把传输层与 HTTP 层错误映射成 [`ServiceError`]。
    ///
    /// `body` 为 `None` 表示 GET（不带 body），`Some(Value)` 是 JSON，
    /// `Some(Form)` 是 multipart —— 两者类型不同，用 enum 收口。
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<RequestBody>,
    ) -> Result<reqwest::Response, ServiceError> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| ProgrammerError::new(format!("常量 HTTP 方法无效: {error}")))?;
        let url = format!("{}{path}", self.base_url);
        let mut builder = self.http.request(method, &url);
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }
        match body {
            None => {}
            Some(RequestBody::Json(value)) => builder = builder.json(&value),
            Some(RequestBody::Multipart(form)) => builder = builder.multipart(form),
        }
        let response = builder.send().await.map_err(Self::map_send_error)?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            // 透传远端状态码（上游 `:59-64`）。这一支不能走构造器 ——
            // 状态码由远端决定。见模块文档第 3 条。
            return Err(ServiceError {
                status: status.as_u16(),
                api: Box::new(sm_core::ApiError::new(
                    code::FAILED,
                    "Embedding service rejected the request",
                )),
            });
        }
        Ok(response)
    }

    /// 传输层错误 → 503/502。
    ///
    /// # 上游的「超时 vs 不可达」之分**复现不了**，已合并
    ///
    /// 上游 `:45-54` 分两支：`TimeoutException` → "timed out"（503）、
    /// `NetworkError` → "is unreachable"（503）。**两支状态码与错误码本来就
    /// 相同**，只有 `message` 不同，所以合并的代价只限文案。
    ///
    /// 而 reqwest 0.13 **给不出这个区分**。实测（`tests/embedding_http.rs`
    /// 里的 `probe_reqwest_error_classification` 用 `--nocapture` 跑）：
    ///
    /// ```text
    /// 127.0.0.1:1        （保留端口，立即被拒）  is_timeout=true is_connect=true
    /// 127.0.0.1:50395    （已关闭的临时端口）    is_timeout=true is_connect=true
    /// ```
    ///
    /// 两种截然不同的失败（瞬间被拒 vs 等到超时）**分类完全相同**。所以
    /// 「先判 `is_timeout` 再判 `is_connect`」这条规则在本 crate 里
    /// **没有依据** —— 写了也只是把「被拒」误报成「超时」。
    ///
    /// 那个探针留作永久回归测试：将来若升级 reqwest 后两者能区分了，
    /// 它会失败，届时可以把这个分界加回来。**靠猜升级不会注意到这种变化。**
    fn map_send_error(error: reqwest::Error) -> ServiceError {
        // 连接期失败（含被拒与超时）→ 503，可退避重试。
        if error.is_connect() || error.is_timeout() {
            return ServiceError::unavailable(
                code::UNAVAILABLE,
                "Embedding service is unreachable or timed out",
            );
        }
        // 其余（请求构造/重定向/编码等）：连上了但没成功 → 502。
        tracing::debug!(error = %error, "推理服务请求失败，但不属于连接期错误");
        Self::failed("Embedding service request failed")
    }

    /// 解析响应体为 JSON 对象。
    ///
    /// 上游 `_payload`（`:67-83`）要求**顶层是 dict**；数组或标量都算
    /// 「invalid payload」。这里显式判 `is_object`。
    async fn payload(
        &self,
        response: reqwest::Response,
    ) -> Result<Map<String, Value>, ServiceError> {
        let value: Value = response
            .json()
            .await
            .map_err(|_| Self::failed("Embedding service returned invalid JSON"))?;
        match value {
            Value::Object(map) => Ok(map),
            _ => Err(Self::failed("Embedding service returned invalid payload")),
        }
    }

    /// 取 `vectors` 并逐项转成 `f32`，**数量必须与请求条数相等**。
    ///
    /// 照抄上游 `_vectors`（`:126-142`）。数量不符就报错而不是截断/补齐 ——
    /// 少一条意味着索引里少了那一张图，客户端却以为整批成功。
    fn vectors(
        payload: &Map<String, Value>,
        expected: usize,
    ) -> Result<Vec<Vec<f32>>, ServiceError> {
        let raw = payload
            .get("vectors")
            .and_then(Value::as_array)
            .ok_or_else(|| Self::failed("Embedding service returned invalid vectors"))?;
        if raw.len() != expected {
            return Err(Self::failed("Embedding service returned invalid vectors"));
        }
        raw.iter()
            .map(|vector| {
                vector
                    .as_array()
                    .ok_or_else(|| Self::failed("Embedding service returned invalid vectors"))?
                    .iter()
                    .map(|value| {
                        value.as_f64().map(|number| number as f32).ok_or_else(|| {
                            Self::failed("Embedding service returned invalid vectors")
                        })
                    })
                    .collect()
            })
            .collect()
    }

    /// 502 + `image_search_inference_failed`。
    ///
    /// 上游这一支的 `details` 从不填充，所以固定传空 map。
    fn failed(message: &'static str) -> ServiceError {
        ServiceError::bad_gateway(code::FAILED, message, Map::new())
    }
}

/// 请求体。`Value` 是 JSON，`Form` 是 multipart —— 上游两个 embed 端点各用一种。
enum RequestBody {
    Json(Value),
    Multipart(reqwest::multipart::Form),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space_payload(space_id: &str, dimension: i64, modalities: &[&str]) -> Value {
        serde_json::json!({
            "space_id": space_id,
            "dimension": dimension,
            "modalities": modalities,
        })
    }

    /// 上游 `:92-101` 的三条校验逐条来一遍。
    #[test]
    fn space_validates_all_three_conditions() {
        let usable = EmbeddingSpace::from_payload(
            space_payload("clip-vit-b32", 512, &["image", "text"])
                .as_object()
                .unwrap(),
        );
        assert!(usable.is_usable());
        assert_eq!(usable.dimension, 512);

        // 标识为空
        let no_id = EmbeddingSpace::from_payload(
            space_payload("", 512, &["image", "text"])
                .as_object()
                .unwrap(),
        );
        assert!(!no_id.is_usable(), "空 space_id 必须判为不可用");

        // 维度为零 —— 上游 `int(payload.get("dimension") or 0)`，缺失同归零
        let zero_dim = EmbeddingSpace::from_payload(
            space_payload("s", 0, &["image", "text"])
                .as_object()
                .unwrap(),
        );
        assert!(!zero_dim.is_usable(), "dimension=0 必须判为不可用");

        let missing_dim = EmbeddingSpace::from_payload(
            space_payload("s", 0, &["image", "text"])
                .as_object()
                .unwrap(),
        );
        assert!(!missing_dim.is_usable());

        // 缺 text 模态（只有图搜）—— 这正是「只开了图搜没开文搜」那类配置
        let image_only =
            EmbeddingSpace::from_payload(space_payload("s", 512, &["image"]).as_object().unwrap());
        assert!(!image_only.is_usable(), "缺 text 模态必须判为不可用");

        let text_only =
            EmbeddingSpace::from_payload(space_payload("s", 512, &["text"]).as_object().unwrap());
        assert!(!text_only.is_usable(), "缺 image 模态必须判为不可用");
    }

    /// 字段缺失与类型不对都落到默认值 → 不可用，而不是 panic。
    #[test]
    fn space_tolerates_missing_and_mistyped_fields() {
        let empty = EmbeddingSpace::from_payload(serde_json::json!({}).as_object().unwrap());
        assert!(!empty.is_usable());
        assert_eq!(empty.dimension, 0);
        assert!(empty.modalities.is_empty());

        // 上游 `int("abc")` 会抛 ValueError；这里落到 0 → 同样判为不可用
        let mistyped = EmbeddingSpace::from_payload(
            serde_json::json!({"space_id": "s", "dimension": "abc", "modalities": ["image", "text"]})
                .as_object()
                .unwrap(),
        );
        assert!(!mistyped.is_usable());

        // modalities 不是数组
        let bad_modalities = EmbeddingSpace::from_payload(
            serde_json::json!({"space_id": "s", "dimension": 4, "modalities": "image"})
                .as_object()
                .unwrap(),
        );
        assert!(!bad_modalities.is_usable());
    }

    /// 数量不符必须报错，不能截断 —— 少一条索引就缺那一张图。
    #[test]
    fn vectors_reject_count_mismatch() {
        let two = serde_json::json!({"vectors": [[0.1, 0.2], [0.3, 0.4]]});
        let ok = EmbeddingClient::vectors(two.as_object().unwrap(), 2).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0], vec![0.1_f32, 0.2_f32]);

        let err = EmbeddingClient::vectors(two.as_object().unwrap(), 3).unwrap_err();
        assert_eq!(err.status, 502);
        assert_eq!(err.code(), code::FAILED);

        // 少给一条
        let short = serde_json::json!({"vectors": [[0.1, 0.2]]});
        assert!(EmbeddingClient::vectors(short.as_object().unwrap(), 2).is_err());
    }

    /// 顶层不是 object、或向量项含非数值，都算协议失败。
    #[test]
    fn vectors_reject_malformed_payloads() {
        for payload in [
            serde_json::json!({"vectors": "not a list"}),
            serde_json::json!({"other": 1}),
            serde_json::json!({"vectors": ["not a list"]}),
            serde_json::json!({"vectors": [[0.1, "x"]]}),
        ] {
            let err = EmbeddingClient::vectors(payload.as_object().unwrap(), 1).unwrap_err();
            assert_eq!(err.status, 502, "payload={payload}");
            assert_eq!(err.code(), code::FAILED, "payload={payload}");
        }
    }

    /// 上游 `embed_texts` 对空列表**也**抛错（`:118` 的 `not texts`），
    /// 而 `embed_images` 对空批次返回 `Ok(vec![])`（`:107`）。这个不对称
    /// 是刻意的，批处理末批恰好为空时不能报错。
    #[test]
    fn empty_input_semantics_are_asymmetric() {
        let client =
            EmbeddingClient::with_http_client(reqwest::Client::new(), "http://127.0.0.1:1", None);
        // 图片：空批次 = 合法 no-op，不需要发请求
        let images = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(client.embed_images(&[]));
        assert_eq!(images.unwrap(), Vec::<Vec<f32>>::new());

        // 文本：空列表 → 422
        let texts = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(client.embed_texts(&[]));
        let err = texts.unwrap_err();
        assert_eq!(err.status, 422, "空文本列表必须 422，不能是 200 空结果");

        // 文本：含空白项 → 422（`any(not text.strip())`）
        let blank = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(client.embed_texts(&["ok".to_owned(), "   ".to_owned()]));
        assert_eq!(blank.unwrap_err().status, 422);
    }

    /// base_url 的尾斜杠要去掉，否则拼出 `//v1/embedding-space`。
    #[test]
    fn base_url_trims_trailing_slashes() {
        let client = EmbeddingClient::with_http_client(
            reqwest::Client::new(),
            "http://example.test:8080///",
            None,
        );
        assert_eq!(client.base_url, "http://example.test:8080");
    }
}
