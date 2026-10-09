//! `/system/plugins*` —— 插件管理八个端点。
//!
//! # 与上游 `src/api/routers/system/plugins.py` 的对应
//!
//! prefix `/system/plugins`。
//!
//! | 上游端点 | 状态码 | 编码 |
//! |---|---|---|
//! | `GET ""` | 200 | —— |
//! | `GET /{plugin_id}` | 200 | —— |
//! | `GET /{plugin_id}/settings` | 200（**排除 None**）| —— |
//! | `PUT /{plugin_id}/settings` | 200（**排除 None**）| JSON |
//! | `POST ""` | **201** | **multipart** |
//! | `POST /{plugin_id}/upgrade` | 200 | **multipart** |
//! | `PATCH /{plugin_id}` | 200 | **query**（不是 body！）|
//! | `DELETE /{plugin_id}` | **200 + body**（不是 204！）| —— |
//!
//! # 四处不寻常，都是契约的一部分
//!
//! **1. `DELETE` 返回 200 + `PluginInstallResponse`，不是 204。** 本仓库其他
//! 所有 delete 都返回 204 无 body，**这里不是** —— 客户端要读卸载结果。
//!
//! **2. `PATCH /{plugin_id}` 的 `enabled` 是 query 参数。** 上游
//! `set_plugin_enabled(plugin_id: str, enabled: bool)` 的 `enabled` 没有
//! `Body(...)`，FastAPI 因此按查询参数处理，即
//! `PATCH /system/plugins/foo?enabled=true`。**与本仓库其他 PATCH 不同。**
//!
//! **3. 两个 settings 端点用 `response_model_exclude_none=True`** —— 响应省略
//! 所有 None 字段而非输出 `null`。Rust 侧要逐字段标
//! `#[serde(skip_serializing_if = "Option::is_none")]`。
//!
//! 为什么不简化成输出 null：settings schema **由插件自己定义**，省略 None
//! 让响应只含插件实际支持的键 —— 客户端看到的就是该插件的完整选项集。
//!
//! **4. `PUT .../settings` 的请求体是 `dict[str, Any]`，不是模型。** settings
//! 形状由插件决定，本仓库无法预先建模。照抄用 `serde_json::Value`。
//! **不要**为常见插件猜一套结构 —— 第二个插件出现时那会变成错误的抽象。
//!
//! # install / upgrade 是 **multipart** —— `form` feature 早就是开的
//!
//! 骨架写「要开 axum 的 `form` feature」（与 `routes/image_search.rs` 同一处
//! 假前置）：**错**。本仓没关 axum 的 default features，而 `form` 是 default
//! 的一部分，`Form` / `Multipart` 一直可用。
//!
//! 表单字段（`plugins.py` 的签名）：
//! - `POST ""`：`file`（必填）+ `sha256`（**可选**）+ `enable`（**默认 `true`**）；
//! - `POST /{plugin_id}/upgrade`：`file`（必填）+ `sha256`（可选），**没有** `enable`。
//!
//! `sha256` 给了就校验、不给就跳过，**别**改成必填。两个上传端点都先按
//! `Content-Length` 拦超限包（`MAX_ARCHIVE_BYTES`）→ **413 `plugin_too_large`**。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/system/plugins",
            get(list_plugins)
                .post(install_plugin)
                .fallback(method_not_allowed),
        )
        .route(
            "/system/plugins/{plugin_id}",
            get(get_plugin)
                .patch(set_plugin_enabled)
                .delete(remove_plugin)
                .fallback(method_not_allowed),
        )
        .route(
            "/system/plugins/{plugin_id}/settings",
            get(get_plugin_settings)
                .put(update_plugin_settings)
                .fallback(method_not_allowed),
        )
        .route(
            "/system/plugins/{plugin_id}/upgrade",
            post(upgrade_plugin).fallback(method_not_allowed),
        )
}

/// 插件概要。
///
/// ⚠️ 骨架期字段是**自造**的 `{plugin_id, name, version, enabled}` —— 上游
/// `PluginSummaryResource`（`schema/system/plugins.py`）是
/// `{plugin_id, display_name, version, host_api_version: int, enabled,
/// load_status: str = "ok", load_error?, release_api_url?}`：`name` 应为
/// `display_name`，且缺四个字段。实现本文件时一并改。
#[derive(Debug, Clone, Serialize)]
pub struct PluginSummaryResource {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
}

/// 插件详情。
///
/// ⚠️ 骨架期是 `flatten(summary) + description + abi_version` —— 上游
/// `PluginDetailResource` 是 `PluginSummaryResource` 再加 `requires_python?`、
/// `author?`、`homepage?`、`manifest: dict`、`data_dir: str`。**字段几乎全不同。**
#[derive(Debug, Clone, Serialize)]
pub struct PluginDetailResource {
    #[serde(flatten)]
    pub summary: PluginSummaryResource,
    pub description: Option<String>,
    /// 插件自报的 ABI 版本。**宿主要校验它**。
    pub abi_version: Option<String>,
}

/// 插件设置响应 —— **所有 None 字段都不输出**。
///
/// ⚠️ 骨架期是**空结构体**（序列化成 `{}`）。上游 `PluginSettingsResource`
/// 是 `{settings: dict, schema?: dict（别名 `schema`）, defaults?: dict}`；
/// 而 `PUT` 回的是它的子类 `PluginSettingsUpdateResource`（多一个
/// `pending_restart: list[str]`）—— 骨架把 `PUT` 的返回类型也写成了
/// `PluginSettingsResource`，实现时要换成 `PluginSettingsUpdateResource`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct PluginSettingsResource {
    // 形状由插件决定。实现时按插件 schema 动态生成并逐字段 skip_serializing_if。
}

/// 安装 / 升级 / 卸载的响应。
///
/// ⚠️ 骨架期字段 `{plugin_id, action, requires_reload}` 是**自造**的 —— 上游
/// `PluginInstallResponse` 是 `{plugin_id, version, pending_restart: list[str]}`。
#[derive(Debug, Clone, Serialize)]
pub struct PluginInstallResponse {
    pub plugin_id: String,
    /// `installed` / `upgraded` / `removed`。
    pub action: String,
    pub requires_reload: bool,
}

/// `PATCH /{plugin_id}` 的查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct SetEnabledQuery {
    /// **query 参数，不是 body**（见模块文档）。
    pub enabled: bool,
}

async fn list_plugins(
    _user: CurrentUser,
    State(_state): State<AppState>,
) -> Result<Json<Vec<PluginSummaryResource>>, ErrorResponse> {
    todo!("骨架：接插件注册表（sm-plugins 已就位）")
}

/// `GET /{plugin_id}` —— 不存在 → **404**。
async fn get_plugin(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
) -> Result<Json<PluginDetailResource>, ErrorResponse> {
    todo!("骨架：接插件详情")
}

/// `GET /{plugin_id}/settings` —— **None 字段不输出**。
async fn get_plugin_settings(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
) -> Result<Json<PluginSettingsResource>, ErrorResponse> {
    todo!("骨架：接插件设置（逐字段 skip_serializing_if）")
}

/// `PUT /{plugin_id}/settings` —— 请求体 `dict[str, Any]`。
async fn update_plugin_settings(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<Json<PluginSettingsResource>, ErrorResponse> {
    todo!("骨架：settings 形状由插件决定，不要猜结构")
}

/// `POST ""` —— **201** + **multipart**。
async fn install_plugin(
    _user: CurrentUser,
    State(_state): State<AppState>,
    _payload: axum::extract::Multipart,
) -> Result<(StatusCode, Json<PluginInstallResponse>), ErrorResponse> {
    todo!(
        "骨架：multipart（file 必填 + sha256 可选 + enable 默认 true）；先按 Content-Length 拦 413"
    )
}

/// `POST /{plugin_id}/upgrade` —— 200 + multipart。
async fn upgrade_plugin(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
    _payload: axum::extract::Multipart,
) -> Result<Json<PluginInstallResponse>, ErrorResponse> {
    todo!("骨架：multipart（file 必填 + sha256 可选）；先按 Content-Length 拦 413")
}

/// `PATCH /{plugin_id}?enabled=...` —— **query 参数**。
async fn set_plugin_enabled(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
    EnvelopeQuery(_query): EnvelopeQuery<SetEnabledQuery>,
) -> Result<Json<PluginSummaryResource>, ErrorResponse> {
    todo!("骨架：enabled 走 query 而非 body")
}

/// `DELETE /{plugin_id}` —— **200 + body**，不是 204。
async fn remove_plugin(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
) -> Result<Json<PluginInstallResponse>, ErrorResponse> {
    todo!("骨架：删除返回 200 + 卸载结果")
}
