//! OpenAI 兼容翻译客户端。
//!
//! # 上游对应：`translation.py`
//!
//! 逐条照搬：`TranslationError` / `_is_model_error` / `load_prompt` /
//! `normalize_translation` / `OpenAITranslationClient`（支持 Chat Completions
//! 与 Responses 两种 API）。
//!
//! # 与上游不同的地方
//!
//! 1. **异步**：上游是 `httpx.Client`（同步）；这里用 `reqwest` async。
//! 2. **提示词编译期嵌入**：上游运行时读 `prompts/*.md`；这里
//!    `include_str!` 进二进制（见 [`TITLE_PROMPT`] / [`DESC_PROMPT`]）。
//! 3. **`load_prompt` 不存在**：调用方直接用上面两个常量。

use std::time::Duration;

use regex::Regex;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::settings::{ApiType, Settings};

/// 上游 `prompts/movie_title_translation.md`（编译期嵌入）。
pub const TITLE_PROMPT: &str = include_str!("../prompts/movie_title_translation.md");
/// 上游 `prompts/movie_desc_translation.md`（编译期嵌入）。
pub const DESC_PROMPT: &str = include_str!("../prompts/movie_desc_translation.md");

/// 上游 `TranslationError`。
#[derive(Debug, Clone)]
pub struct TranslationError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub abort_batch: bool,
}

impl TranslationError {
    fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable,
            abort_batch: false,
        }
    }

    fn abort(mut self) -> Self {
        self.abort_batch = true;
        self
    }
}

impl std::fmt::Display for TranslationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TranslationError {}

/// 旧版文案规范化（上游 `normalize_translation`）：去掉 `<think>` 块，
/// 「无有效内容」记为空。
pub fn normalize_translation(value: &str) -> String {
    let re = Regex::new(r"(?s)<think>.*?</think>").expect("think 正则是常量");
    let normalized = re.replace_all(value, "").trim().to_owned();
    if normalized.contains("无有效内容") {
        String::new()
    } else {
        normalized
    }
}

/// 判断是不是「模型配错了」（上游 `_is_model_error`）：这类错误重试也没用，
/// 整批任务直接中断。
fn is_model_error(status: StatusCode, body: &Value) -> bool {
    if !matches!(status.as_u16(), 400 | 404 | 422) {
        return false;
    }
    let Some(error) = body.get("error") else {
        return false;
    };
    if let Some(obj) = error.as_object() {
        if let Some(code) = obj.get("code").and_then(|v| v.as_str()) {
            if matches!(
                code,
                "model_not_found" | "invalid_model" | "unsupported_model"
            ) {
                return true;
            }
        }
        if obj.get("param").and_then(|v| v.as_str()) == Some("model") {
            return true;
        }
        if let Some(message) = obj.get("message").and_then(|v| v.as_str()) {
            return model_message_matches(message);
        }
        return false;
    }
    false
}

fn model_message_matches(message: &str) -> bool {
    let re = Regex::new(
        r"(?i)\bmodel\b.*(?:not found|does not exist|not supported|not available)|\b(?:unknown|invalid|unsupported) model\b",
    )
    .expect("模型错误正则是常量");
    re.is_match(message)
}

/// 翻译客户端（上游 `OpenAITranslationClient`）。
pub struct TranslationClient {
    client: Client,
    model: String,
    api_type: ApiType,
    consecutive_429: u32,
}

impl TranslationClient {
    pub fn new(settings: &Settings) -> Result<Self, TranslationError> {
        Self::with_base(settings, &settings.translation_base_url())
    }

    /// 测试用：把请求打到本地假服务。
    pub fn with_base(settings: &Settings, _base_url: &str) -> Result<Self, TranslationError> {
        let session = format!("sakuramedia-translation-{}", Uuid::new_v4().simple());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-opencode-session",
            reqwest::header::HeaderValue::from_str(&session)
                .map_err(|_| TranslationError::new("bad_session", "构造会话头失败", false))?,
        );
        if !settings.api_key.is_empty() {
            headers.insert(
                AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", settings.api_key))
                    .map_err(|_| TranslationError::new("bad_key", "API 密钥格式非法", false))?,
            );
        }
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs_f64(
                settings.translation_timeout_seconds,
            ))
            .build()
            .map_err(|e| {
                TranslationError::new("client_build", &format!("构造 HTTP 客户端失败: {e}"), false)
            })?;
        Ok(Self {
            client,
            model: settings.model.clone(),
            api_type: settings.api_type,
            consecutive_429: 0,
        })
    }

    /// 翻译一段文本（上游 `translate`）。
    pub async fn translate(
        &mut self,
        system_prompt: &str,
        source_text: &str,
        base_url: &str,
    ) -> Result<String, TranslationError> {
        let (endpoint, payload) = match self.api_type {
            ApiType::Responses => (
                "responses",
                json!({
                    "model": self.model,
                    "instructions": system_prompt,
                    "input": source_text,
                }),
            ),
            ApiType::ChatCompletions => (
                "chat/completions",
                json!({
                    "model": self.model,
                    "temperature": 0,
                    "messages": [
                        {"role": "system", "content": system_prompt},
                        {"role": "user", "content": source_text},
                    ],
                }),
            ),
        };
        let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                self.consecutive_429 = 0;
                TranslationError::new("request_failed", "翻译服务请求失败", true)
                    .with_source(&e.to_string())
            })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            self.consecutive_429 += 1;
        } else {
            self.consecutive_429 = 0;
        }
        if self.consecutive_429 >= 3 {
            return Err(TranslationError::new(
                "http_429",
                "翻译服务连续 3 次返回 HTTP 429，已停止本轮任务",
                true,
            )
            .abort());
        }
        if status.is_client_error() || status.is_server_error() {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let abort = matches!(status.as_u16(), 401 | 403) || is_model_error(status, &body);
            let retryable =
                !abort && (matches!(status.as_u16(), 408 | 429) || status.is_server_error());
            let mut err = TranslationError::new(
                &format!("http_{}", status.as_u16()),
                &format!(
                    "翻译服务返回 HTTP {}{}",
                    status.as_u16(),
                    if abort {
                        "：认证或模型配置错误"
                    } else {
                        ""
                    }
                ),
                retryable,
            );
            if abort {
                err = err.abort();
            }
            return Err(err);
        }
        let body: Value = response.json().await.map_err(|e| {
            TranslationError::new("invalid_response", "翻译服务返回了非法响应", true)
                .with_source(&e.to_string())
        })?;
        self.response_content(&body)
    }

    /// 从响应里抠译文（上游 `_response_content`）。
    fn response_content(&self, body: &Value) -> Result<String, TranslationError> {
        let content: String = match self.api_type {
            ApiType::Responses => {
                if body.get("status").and_then(|v| v.as_str()) != Some("completed") {
                    return Err(TranslationError::new(
                        "incomplete_response",
                        "翻译服务未完成响应",
                        true,
                    ));
                }
                let output = body
                    .get("output")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        TranslationError::new(
                            "invalid_response",
                            "翻译服务返回了非法响应结构",
                            true,
                        )
                    })?;
                let mut parts: Vec<String> = Vec::new();
                for item in output {
                    if item.get("type").and_then(|v| v.as_str()) != Some("message") {
                        continue;
                    }
                    let content = item.get("content").and_then(|v| v.as_array());
                    let empty = Vec::new();
                    for part in content.unwrap_or(&empty) {
                        match part.get("type").and_then(|v| v.as_str()) {
                            Some("refusal") => {
                                return Err(TranslationError::new(
                                    "refusal",
                                    "翻译服务拒绝生成译文",
                                    false,
                                ));
                            }
                            Some("output_text") => {
                                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                    parts.push(text.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                parts.join("")
            }
            ApiType::ChatCompletions => body
                .get("choices")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .ok_or_else(|| {
                    TranslationError::new("invalid_response", "翻译服务返回了非法响应结构", true)
                })?
                .to_owned(),
        };
        let content = content.trim().to_owned();
        if content.is_empty() {
            return Err(TranslationError::new(
                "empty_result",
                "翻译服务返回了空译文",
                false,
            ));
        }
        Ok(content)
    }
}

impl TranslationError {
    fn with_source(mut self, source: &str) -> Self {
        if !source.is_empty() {
            self.message = format!("{} ({})", self.message, source);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_blocks_are_stripped() {
        assert_eq!(
            normalize_translation("<think> reasoning </think> 译文"),
            "译文"
        );
    }

    #[test]
    fn no_valid_content_becomes_empty() {
        assert_eq!(normalize_translation("无有效内容"), "");
        assert_eq!(normalize_translation("前缀 无有效内容 后缀"), "");
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(normalize_translation("  正常译文  "), "正常译文");
    }

    #[test]
    fn the_prompts_are_embedded() {
        assert!(!TITLE_PROMPT.is_empty(), "标题提示词为空");
        assert!(!DESC_PROMPT.is_empty(), "简介提示词为空");
        assert!(TITLE_PROMPT.contains("标题"), "标题提示词内容不对");
    }

    #[test]
    fn model_errors_are_detected() {
        let body = serde_json::json!({"error": {"code": "model_not_found"}});
        assert!(is_model_error(StatusCode::NOT_FOUND, &body));
        let body = serde_json::json!({"error": {"param": "model"}});
        assert!(is_model_error(StatusCode::BAD_REQUEST, &body));
        let body = serde_json::json!({"error": {"message": "The model does not exist"}});
        assert!(is_model_error(StatusCode::BAD_REQUEST, &body));
        let body = serde_json::json!({"error": {"message": "ok"}});
        assert!(!is_model_error(StatusCode::BAD_REQUEST, &body));
        // 非 400/404/422 直接 false。
        let body = serde_json::json!({"error": {"code": "model_not_found"}});
        assert!(!is_model_error(StatusCode::INTERNAL_SERVER_ERROR, &body));
    }

    /// Chat Completions 的往返（wiremock 假服务）。
    #[tokio::test]
    async fn chat_completions_roundtrip() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"choices": [{"message": {"content": "中文标题"}}]}),
            ))
            .mount(&server)
            .await;

        let settings = Settings {
            model: "test-model".to_owned(),
            ..Default::default()
        };
        let mut client = TranslationClient::with_base(&settings, &server.uri()).unwrap();
        let out = client
            .translate("prompt", "原文", &format!("{}/v1", server.uri()))
            .await
            .unwrap();
        assert_eq!(out, "中文标题");
    }

    /// 空译文是不可重试的错误。
    #[tokio::test]
    async fn empty_translation_is_not_retryable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"choices": [{"message": {"content": "   "}}]}),
                ),
            )
            .mount(&server)
            .await;

        let settings = Settings {
            model: "test-model".to_owned(),
            ..Default::default()
        };
        let mut client = TranslationClient::with_base(&settings, &server.uri()).unwrap();
        let err = client
            .translate("prompt", "原文", &format!("{}/v1", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err.code, "empty_result");
        assert!(!err.retryable);
    }

    /// 401 中断整批。
    #[tokio::test]
    async fn unauthorized_aborts_the_batch() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let settings = Settings {
            model: "test-model".to_owned(),
            ..Default::default()
        };
        let mut client = TranslationClient::with_base(&settings, &server.uri()).unwrap();
        let err = client
            .translate("prompt", "原文", &format!("{}/v1", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err.code, "http_401");
        assert!(err.abort_batch);
    }
}
