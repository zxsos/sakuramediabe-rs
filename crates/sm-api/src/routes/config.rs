//! `GET /config` 与 `PATCH /config`。
//!
//! 对应上游 `src/api/routers/system/config.py`（**2 个端点**）。
//!
//! # 鉴权挂在 handler 上
//!
//! 上游两个端点都声明 `current_user=Depends(get_current_user)`
//! （`config.py:19` 与 `:24`），router 级只有 `db_deps`。这里照此让每个
//! handler 显式写 `CurrentUser` 提取器 —— 而不是靠一个 router 级 layer 悄悄
//! 生效，后者会让「这个端点其实没鉴权」变得看不见。
//!
//! # GET 会把只读键**排除**掉
//!
//! 响应体是 `_public_values()`，即剔除 `auth` / `enable_docs` / `plugins`
//! 之后的那份。其中 `auth` 含 `secret_key` 与 `file_signature_secret` ——
//! 把它塞进响应体就是凭据外流，而 `GET /config` 是**任何登录用户**都能调的。
//!
//! # PATCH 的体是自由形状的 dict
//!
//! 上游签名是 `payload: dict[str, Any] = Body(...)`，即**嵌套 dict partial**：
//! 未知键由 service 层拒绝（`unknown_config_field`），而不是由 pydantic。
//! 所以这里用 `serde_json::Value` 接，而不是一个固定结构的 DTO —— 固定结构会
//! 让未知的键在反序列化时就被 axum 拒掉，错误码从 `unknown_config_field`
//! 变成 `validation_error`，客户端分支就变了。
//!
//! 但体**必须是对象**：`[1,2,3]` 或 `"str"` 在上游触发 FastAPI 的
//! `RequestValidationError` → 422 `validation_error`，那是提取器层的事，
//! 与「键不认识」是两回事。见本模块的 `reject_non_object_body`。

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Map, Value};

use crate::auth::CurrentUser;
use crate::dto::{ConfigResource, ConfigUpdateResource};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/config",
        get(get_config)
            .patch(update_config)
            .fallback(method_not_allowed),
    )
}

/// `GET /config` → `ConfigResource`
async fn get_config(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<ConfigResource>, ErrorResponse> {
    let values = state.config().get()?;
    Ok(Json(ConfigResource { values }))
}

/// `PATCH /config` → `ConfigUpdateResource`
///
/// # 返回的不是「已生效的值」
///
/// `restart_required` 恒为 `["api", "aps"]`：写盘只改文件，运行中的进程继续用
/// 启动时快照。客户端靠这个字段提示「需要重启」，而它**总是**非空 ——
/// 所以前端不该把它当成「有时需要重启」。
async fn update_config(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(body): EnvelopeJson<Value>,
) -> Result<Json<ConfigUpdateResource>, ErrorResponse> {
    let patch = reject_non_object_body(body)?;
    let values = state.config().update(&patch)?;
    Ok(Json(ConfigUpdateResource::new(values)))
}

/// 体必须是 JSON 对象。
///
/// 上游那一行 `payload: dict[str, Any] = Body(...)` 让 FastAPI 在**提取阶段**
/// 就把非对象的体变成 `RequestValidationError` → 422 `validation_error`。
/// Rust 侧 `Value` 接受任何 JSON，所以这一步要显式补，否则 `[1,2,3]` 会一路
/// 走到 service 层，被当成「空 patch」或「未知键」，错误码就错了。
fn reject_non_object_body(body: Value) -> Result<Map<String, Value>, ErrorResponse> {
    match body {
        Value::Object(object) => Ok(object),
        other => Err(ErrorResponse::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "Request validation failed",
        )
        .with_details(detail_of(&other))),
    }
}

fn detail_of(body: &Value) -> Map<String, Value> {
    let mut details = Map::new();
    // 与 `extract.rs` 里 JSON / multipart 解析失败用同一个 `detail` 键：
    // 「体不是我要的形状」是一类错误，客户端只需要文本。
    details.insert("detail".to_owned(), Value::from(body.to_string()));
    details
}
