//! service 层的错误类型。
//!
//! # 为什么不是直接复用 `sm_core::ApiError`
//!
//! [`sm_core::ApiError`] 是**响应体**的形状（`code` / `message` / `details`），
//! 而 HTTP 状态码在它之外 —— 上游 `ApiError` 的第一个参数就是状态码：
//!
//! ```python
//! raise ApiError(409, "playlist_name_conflict", "Playlist name already exists",
//!                {"name": name})
//! ```
//!
//! 所以这里把两者绑在一起：**状态码与错误码是一个整体**，不该让调用方
//! 分别记住「这个名字冲突该用 409」。分散开的结果是同一类错误在不同地方
//! 用了不同状态码 —— 而客户端是按 `code` 分支、按状态码决定重试的。
//!
//! # 状态码与错误码的对应，逐条对齐上游
//!
//! | 构造 | 状态 | 上游 `code` 的例子 |
//! |---|---|---|
//! | [`ServiceError::validation`] | 422 | `validation_error` |
//! | [`ServiceError::conflict`] | 409 | `playlist_name_conflict`、`playlist_reserved_name`、`playlist_managed_by_system` |
//! | [`ServiceError::not_found`] | 404 | `playlist_not_found`、`movie_not_found` |
//! | [`ServiceError::bad_gateway`] | 502 | `download_candidate_search_failed` |
//! | [`ServiceError::unavailable`] | 503 | `image_search_inference_unavailable` |
//!
//! 上游 `require_by_id` 默认生成 `{entity}_not_found` 与 `{entity}_id` 详情键
//! —— [`ServiceError::not_found`] 的 `details_key` 参数保留了这个约定。

use serde_json::{Map, Value};
use sm_core::ApiError;

/// service 层的错误：状态码 + 响应体。
///
/// # 为什么 `api` 是 `Box`
///
/// [`ApiError`] 内联进来是 96 字节（两个 `String` 加一个
/// `Option<Map<String, Value>>`），加上状态码就超过 128 —— clippy 的
/// `result_large_err` 会报，而报的不是「有点慢」，是**每一个 `?` 都在搬
/// 128 字节**。service 层的方法全是 `?` 链，那笔开销出现在每条错误路径上。
///
/// 装箱后 `ServiceError` 只有 16 字节。代价是一次分配，而错误路径本来就
/// 是罕见路径 —— 那个分配换来的是热路径上更小的移动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceError {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应体的 `error` 对象。**装箱**是有意的 —— 见类型文档。
    pub api: Box<ApiError>,
}

impl ServiceError {
    /// 参数校验失败（422）。
    pub fn validation(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: 422,
            api: Box::new(ApiError::new(code, message)),
        }
    }

    /// 参数校验失败（422），**带 details**。
    ///
    /// 上游 `validate_page` 与 `resolve_sort_expression` 都把出错的字段值
    /// 原样放进 `details`（`{"page": 0}` / `{"sort": "title:up"}`），客户端
    /// 据此高亮对应控件。没有 details 的那个构造留给「消息本身就是全部信息」
    /// 的场景（空更新、空名称）。
    pub fn validation_with(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Map<String, Value>,
    ) -> Self {
        Self {
            status: 422,
            api: Box::new(ApiError::new(code, message).with_details(details)),
        }
    }

    /// 认证失败（401）。
    ///
    /// 上游在鉴权路径上抛的三种码：`invalid_credentials`、
    /// `invalid_refresh_token`、`unauthorized`。
    pub fn unauthorized(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: 401,
            api: Box::new(ApiError::new(code, message)),
        }
    }

    /// 资源冲突（409）：名称重复、系统保留、系统托管。
    ///
    /// `details` 可选 —— 上游在这些错误里都带上了上下文（哪个名字、
    /// 哪个 id），客户端据此定位到具体字段。
    pub fn conflict(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Option<Map<String, Value>>,
    ) -> Self {
        Self {
            status: 409,
            api: Box::new(match details {
                Some(d) => ApiError::new(code, message).with_details(d),
                None => ApiError::new(code, message),
            }),
        }
    }

    /// 资源不存在（404）。
    ///
    /// `details_key` 对应上游 `require_by_id` 生成的 `{entity}_id` 键。
    /// 传 `"playlist"` 得到 `{"playlist_id": 7}`，与上游一致。
    pub fn not_found(
        code: impl Into<String>,
        message: impl Into<String>,
        details_key: &str,
        entity_id: i32,
    ) -> Self {
        let mut details = Map::new();
        details.insert(details_key.to_owned(), Value::from(entity_id));
        Self {
            status: 404,
            api: Box::new(ApiError::new(code, message).with_details(details)),
        }
    }

    /// 上游服务失败（502）。
    ///
    /// 上游把「我们依赖的外部服务没成功」单列成 502 而不是 500 —— 客户端对
    /// 两者的处置不同：502 值得重试（换一个索引器、过一会儿再试），500 是
    /// 自身的缺陷（重试只会重复失败）。所以这条边界必须留在 service 层，
    /// 不能让调用方自己拼状态码。
    pub fn bad_gateway(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Map<String, Value>,
    ) -> Self {
        Self {
            status: 502,
            api: Box::new(ApiError::new(code, message).with_details(details)),
        }
    }

    /// 带自定义 details 的 404 —— 用于按非主键查找（如 `movie_number`）。
    pub fn not_found_with(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Map<String, Value>,
    ) -> Self {
        Self {
            status: 404,
            api: Box::new(ApiError::new(code, message).with_details(details)),
        }
    }

    /// 配置文件**读不了或不是合法 TOML**（500）。
    ///
    /// # 为什么不复用 `ProgrammerError` 那条
    ///
    /// 那条给的是 `programmer_error` —— 客户端看到它会以为是代码 bug，
    /// 而实际上是**部署坏了**：配置文件被手改坏、编码不对、权限不足。
    /// 两者的排查方向完全不同，所以单独一个码。
    ///
    /// # 为什么是 500 而不是 503
    ///
    /// 按本文件顶部定的分界：503 是「依赖连不上，**退避后重试**可能变好」，
    /// 500 是「自身状态错了，重试只会重复失败」。配置文件坏了正属于后者 ——
    /// 同一个文件再读一百次还是坏的。**该做的是改文件或回滚，不是等重试。**
    ///
    /// 但它和 [`ServiceError::unavailable`] 有个共同点：**都不能静默降级**。
    /// 曾经 `clip_collections.rs` / `media_clips.rs` / `jobs.rs` /
    /// `movie_subscriptions.rs` 用 `snapshot().unwrap_or_default()` 把它吞成
    /// 全默认配置，于是 `media_clip_root_path` 变成空串、`clip_root` 退化成
    /// 进程工作目录、产物存在性判定恒为 false。**一个手误的转义符，静默关掉
    /// 了路径解析。** 这条构造器存在的意义就是让那条路走不通。
    pub fn config_invalid(message: impl Into<String>, details: Map<String, Value>) -> Self {
        Self {
            status: 500,
            api: Box::new(ApiError::new("config_invalid", message).with_details(details)),
        }
    }

    /// 外部依赖**不可用**（503）。
    ///
    /// 与 [`ServiceError::bad_gateway`] 的分界是**能不能重试**：
    ///
    /// | | 状态 | 含义 | 客户端处置 |
    /// |---|---|---|---|
    /// | [`ServiceError::unavailable`] | 503 | 依赖**连不上或超时**（网络层） | 退避后重试 |
    /// | [`ServiceError::bad_gateway`] | 502 | 依赖**连上了但没成功**（协议层） | 换参数或换依赖后重试 |
    ///
    /// 上游 `embedding_client.py:45-58` 正是按这个分界写的：`TimeoutException`
    /// 与 `NetworkError` → 503 `image_search_inference_unavailable`，其余
    /// `HTTPError` → 502 `image_search_inference_failed`。**这个 503/502 的
    /// 分界要保留** —— 它决定客户端是退避重试还是改请求。
    ///
    /// 但上游那两支**之间**的区分（`timed out` vs `is unreachable`）在 Rust
    /// 侧复现不了：reqwest 0.13 对「连接被拒」与「连接超时」返回相同的分类
    /// （实测见 `discovery::embedding::EmbeddingClient` 的
    /// `tests/embedding_http.rs::probe_reqwest_error_classification`）。
    /// 两支的 `code` 与状态码本就相同，所以合并文案不影响客户端分支。
    ///
    /// 真正透传远端状态码的是 `status >= 400` 那一支（上游 `:59-64`）——
    /// 那个状态码由远端决定，不属于本文件的构造器，调用方直接构造结构体。
    pub fn unavailable(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: 503,
            api: Box::new(ApiError::new(code, message)),
        }
    }

    /// **任意**状态码 + 错误码。
    ///
    /// # 为什么需要它
    ///
    /// 现有构造器每个都把状态码写死（`validation` = 422、`conflict` = 409 …），
    /// 适合「本层自己产生的错误」。但有两类错误的状态码是**外部决定的**：
    ///
    /// | 场景 | 状态码来源 |
    /// |---|---|
    /// | provider 操作失败 | `ProviderOperationError.code` 经映射表得出（401/404/409/422/503/502） |
    /// | 远端 HTTP 错误 | 远端响应本身 |
    ///
    /// 这两类若只能「挑一个最近的构造器」，就得先把状态码**降级**再表达 ——
    /// 于是「provider 报了 401」会变成 422，客户端拿到的状态码与上游不一致，
    /// 而它正是靠这个区分「该重新登录」与「该改请求」。
    ///
    /// 因此这里给一个**不做任何翻译**的入口。调用方要自己保证 `status` 与
    /// `code` 搭配合理 —— 那是协议层的责任，不是本类型的责任。
    pub fn from_status(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            api: Box::new(ApiError::new(code, message)),
        }
    }

    /// 取底层错误码，便于测试断言。
    pub fn code(&self) -> &str {
        &self.api.code
    }

    /// 取 `details`，便于测试断言。
    ///
    /// 与 [`Self::code`] 同用途：`ServiceError` 不实现 `Display`（错误正文要走
    /// HTTP 层的错误信封），所以要断言「details 里带了这个键」只能经这里。
    pub fn details(&self) -> Option<&Map<String, Value>> {
        self.api.details.as_ref()
    }
}

/// 编程错误 —— 上游在这一类上抛 `ValueError` 而不是 `ApiError`。
///
/// # 为什么单独一类
///
/// 上游 `plugin_collection_service._validate_key` 与 `_normalize_name` 抛
/// `ValueError`，而同一文件里 `_ensure_collection` 抛 `ApiError(409, ...)`。
/// 两者都处理「参数不对」，但**归属不同**：
///
/// - `ApiError` —— 调用方的输入有问题，HTTP 层该回 4xx
/// - `ValueError` —— **我们自己的代码**调用姿势不对（插件 facade 传了空
///   key），HTTP 层该回 500，因为这不是用户的错
///
/// 把两者都映射成 422 会让「服务端自己传错参数」伪装成「用户输入无效」。
/// 所以这里单列一类，状态码 500，与上游的异常类型对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgrammerError(pub String);

impl ProgrammerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ProgrammerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProgrammerError {}

impl From<ProgrammerError> for ServiceError {
    /// 500 而不是 4xx —— 见类型文档。
    fn from(value: ProgrammerError) -> Self {
        Self {
            status: 500,
            api: Box::new(ApiError::new("programmer_error", value.0)),
        }
    }
}

/// 构造一个单键 details。
///
/// 上游大量使用单键 details（`{"name": ...}`、`{"movie_number": ...}`），
/// 每次手写 `Map::new()` 加 `insert` 既啰嗦又容易写错键名。
pub fn details_of(key: &str, value: impl Into<Value>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(key.to_owned(), value.into());
    map
}

impl From<sqlx::Error> for ServiceError {
    /// 500。仓储层的 SQL 错误一律是服务端问题，不是调用方输入有问题。
    fn from(value: sqlx::Error) -> Self {
        Self {
            status: 500,
            api: Box::new(ApiError::new("internal_error", value.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_codes_match_upstream() {
        assert_eq!(
            ServiceError::validation("validation_error", "x").status,
            422
        );
        assert_eq!(
            ServiceError::conflict("playlist_name_conflict", "x", None).status,
            409
        );
        assert_eq!(
            ServiceError::not_found("playlist_not_found", "x", "playlist_id", 7).status,
            404
        );
    }

    #[test]
    fn not_found_details_key_follows_upstream_convention() {
        // 上游 require_by_id 生成 `{entity}_id` 详情键。
        let err =
            ServiceError::not_found("playlist_not_found", "Playlist not found", "playlist_id", 7);
        assert_eq!(
            err.api.details.as_ref().unwrap().get("playlist_id"),
            Some(&json!(7))
        );
    }

    #[test]
    fn conflict_without_details_omits_the_key() {
        // 与 sm_core::ApiError 的 skip_serializing_if 一致：details 为 None
        // 时序列化不输出该键。
        let err = ServiceError::conflict("c", "m", None);
        assert!(err.api.details.is_none());
        assert!(!serde_json::to_string(&err.api).unwrap().contains("details"));
    }

    #[test]
    fn details_of_builds_a_single_key_map() {
        let d = details_of("name", "abc");
        assert_eq!(d.len(), 1);
        assert_eq!(d.get("name"), Some(&json!("abc")));
    }
}
