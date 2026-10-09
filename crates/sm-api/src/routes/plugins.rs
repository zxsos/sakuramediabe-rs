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
//!
//! # 落地进度（2026-10-07）
//!
//! | 端点 | 状态 |
//! |---|---|
//! | `GET ""` | ✅ 服务层 `PluginAdmin::list` |
//! | `GET /{plugin_id}` | ✅ `PluginAdmin::detail`（不存在 → 404） |
//! | `PATCH /{plugin_id}` | ✅ `PluginAdmin::set_enabled`（写 `plugins.enabled`） |
//! | `POST ""` | ✅ `PluginAdmin::install_zip`（**201**）|
//! | `POST /{plugin_id}/upgrade` | ✅ `PluginAdmin::upgrade_zip`（200）|
//! | `GET /{plugin_id}/settings` | ⏳ 待做（要插件的 settings schema，见下） |
//! | `PUT /{plugin_id}/settings` | ⏳ 待做 |
//! | `DELETE /{plugin_id}` | ⏳ 待做（还缺 `PluginRemovalService` 的四步清理） |
//!
//! 已实现的五条都走 `AppState::plugin_admin()` —— **没注入时 500
//! `plugin_admin_unavailable`，不是空列表**（「组合根漏了接线」与「没装插件」
//! 必须能区分开）。
//!
//! # 上传是**流式落盘**，不是全缓冲
//!
//! 插件包上限 100 MiB，而 `crate::extract::Multipart` 是「整字段读进内存 +
//! 8 MiB 上限」。这里改用 `crate::extract::receive_to_file`：body 逐块写进
//! `<root>/.staging/uploads/<uuid>.zip`，总量超限**立刻**中止。上游也是
//! `copyfileobj` 到临时文件 —— 对一台 NAS，全缓冲两次并发上传就是几百 MiB。
//!
//! 顺序上 `Content-Length` 预检在**读 body 之前**（上游 `_check_upload_size`），
//! 两个端点都用 [`PLUGIN_TOO_LARGE`]（**不是**通用提取器的 `http_error`）。
//!
//! settings 那两个端点**不是纯接线**：上游的 `schema` / `defaults` 来自插件的
//! pydantic `settings_model`，Rust 侧没有对应物。要做就得先决定「Rust 插件怎么
//! 声明自己的设置项」（见 `docs/deployment.md` 的插件移植判据），所以它排在
//! 卸载之后。

use std::path::{Path as FsPath, PathBuf};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use sm_service::error::ServiceError;
use sm_service::system::plugins::PLUGIN_TOO_LARGE;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
// **流式**收上传体（100 MiB 不能全缓冲进内存），而不是 `extract::Multipart`
// 的 8 MiB 全缓冲路径 —— 见 `crate::extract::receive_to_file`。
use crate::extract::{receive_to_file, Multipart, Query as EnvelopeQuery, ReceivedForm};
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
/// ⚠️ ~~骨架期字段是自造的 `{plugin_id, name, version, enabled}`~~ —— 已换成
/// 服务层的 `PluginSummary`，它与上游 `PluginSummaryResource`
/// （`schema/system/plugins.py`）**逐字一致**：
/// `{plugin_id, display_name, version, host_api_version, enabled, load_status,
/// load_error, release_api_url}`。
///
/// 别名而不是在这里重新定义一遍：两处定义会漂移，而漂移的表现是「客户端某块
/// 显示成空白」，不是编译错误。键集合由 `sm_service::system::plugins` 的测试
/// 钉住。
pub use sm_service::system::plugins::PluginSummary as PluginSummaryResource;

/// 插件详情。
///
/// 上游 `PluginDetailResource` = `PluginSummaryResource` 的字段**平铺** +
/// `requires_python?` / `author?` / `homepage?` / `manifest: dict` /
/// `data_dir: str`。服务层那个类型就是这么定义的（`#[serde(flatten)]`）。
pub use sm_service::system::plugins::PluginDetail as PluginDetailResource;

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
/// ⚠️ ~~骨架期字段 `{plugin_id, action, requires_reload}` 是自造的~~ —— 上游
/// `PluginInstallResponse` 是 `{plugin_id, version, pending_restart}`。
pub use sm_service::system::plugins::PluginInstallOutcome as PluginInstallResponse;

/// `PATCH /{plugin_id}` 的查询参数。
#[derive(Debug, Clone, Deserialize)]
pub struct SetEnabledQuery {
    /// **query 参数，不是 body**（见模块文档）。
    pub enabled: bool,
}

async fn list_plugins(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<PluginSummaryResource>>, ErrorResponse> {
    Ok(Json(state.plugin_admin()?.list()?))
}

/// `GET /{plugin_id}` —— 不存在 → **404**。
async fn get_plugin(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> Result<Json<PluginDetailResource>, ErrorResponse> {
    match state.plugin_admin()?.detail(&plugin_id)? {
        Some(detail) => Ok(Json(detail)),
        // 服务层把「没有这个插件」表达成 `None` 而不是错误（详情查询里
        // 「不存在」是**正常结果**）；转成 HTTP 才是路由层的事。
        None => Err(unknown_plugin(&plugin_id)),
    }
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
///
/// 表单：`file`（必填）、`sha256`（可选）、`enable`（**默认 `true`**）。
async fn install_plugin(
    _user: CurrentUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    mut payload: Multipart,
) -> Result<(StatusCode, Json<PluginInstallResponse>), ErrorResponse> {
    let admin = state.plugin_admin()?;
    let limit = admin.archive_size_limit();
    check_content_length(&headers, limit)?;

    let staged = StagedUpload::new(admin.prepare_upload_slot()?);
    let form = receive_to_file(&mut payload, staged.path(), limit, PLUGIN_TOO_LARGE).await?;
    require_uploaded_file(&form)?;
    let auto_enable = parse_form_bool(form.fields.get("enable").map(String::as_str), true)?;

    let outcome = admin.install_zip(staged.path(), optional_sha256(&form), auto_enable)?;
    // **201** —— 本仓库唯一一个 201 的端点（上游 `status_code=201`）。
    Ok((StatusCode::CREATED, Json(outcome)))
}

/// `POST /{plugin_id}/upgrade` —— 200 + multipart。
///
/// 表单只有 `file`（必填）与 `sha256`（可选）—— **没有 `enable`**：
/// 升级保持原有的启停状态（上游 `manager.py:273`）。
async fn upgrade_plugin(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    headers: HeaderMap,
    mut payload: Multipart,
) -> Result<Json<PluginInstallResponse>, ErrorResponse> {
    let admin = state.plugin_admin()?;
    let limit = admin.archive_size_limit();
    check_content_length(&headers, limit)?;

    let staged = StagedUpload::new(admin.prepare_upload_slot()?);
    let form = receive_to_file(&mut payload, staged.path(), limit, PLUGIN_TOO_LARGE).await?;
    require_uploaded_file(&form)?;

    Ok(Json(admin.upgrade_zip(
        &plugin_id,
        staged.path(),
        optional_sha256(&form),
    )?))
}

/// `PATCH /{plugin_id}?enabled=...` —— **query 参数**。
async fn set_plugin_enabled(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    EnvelopeQuery(query): EnvelopeQuery<SetEnabledQuery>,
) -> Result<Json<PluginSummaryResource>, ErrorResponse> {
    // 写配置，**不拉起也不杀掉进程**：改动重启后生效（与上游一致，
    // 所以响应体里没有 `pending_restart` —— 那个字段只在安装/升级/卸载上有）。
    Ok(Json(
        state
            .plugin_admin()?
            .set_enabled(&plugin_id, query.enabled)?,
    ))
}

/// 未知插件的 404，与上游 `plugin_not_found` 同一个码。
fn unknown_plugin(plugin_id: &str) -> ErrorResponse {
    ServiceError::from_status(
        404,
        sm_service::system::plugins::PLUGIN_NOT_FOUND,
        format!("未知插件 plugin_id={plugin_id}"),
    )
    .into()
}

// ============================================================ 上传

/// 上传的临时文件。**析构时删除** —— 成功、失败、早退都删。
///
/// 不用 `Drop` 的话，「校验不过」与「发布失败」两条路径都会在
/// `.staging/uploads/` 留下一个上百 MiB 的包 —— 而它们恰恰是最常走到的路径。
struct StagedUpload {
    path: PathBuf,
}

impl StagedUpload {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &FsPath {
        &self.path
    }
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `Content-Length` 预检（上游 `_check_upload_size`，`plugins.py:43-55`）。
///
/// **在读 body 之前**拦掉明显超限的请求 —— 否则一个 10 GB 的上传会先落满
/// 磁盘再被拒。
///
/// 分块传输（没有 `Content-Length`）与非法值一律放行：真正的闸门在
/// `receive_to_file` 里，这里只是省一次无用的落盘。
fn check_content_length(headers: &HeaderMap, max_bytes: u64) -> Result<(), ErrorResponse> {
    let Some(raw) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(());
    };
    let Ok(text) = raw.to_str() else {
        return Ok(());
    };
    // 上游用的是 `content_length.isdigit()` —— 非数字一律不看。
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    let Ok(length) = text.parse::<u64>() else {
        return Ok(());
    };
    if length > max_bytes {
        return Err(upload_too_large(max_bytes, length));
    }
    Ok(())
}

/// 表单里必须有 `file` 字段。上游是 `file: UploadFile = File(...)`（必填）。
///
/// **字节数为 0 也算没给**：一个空的 `file` 字段过不了任何一道校验，
/// 早一点报 422 比让它走到「zip 阶段失败」更有指向性。
fn require_uploaded_file(form: &ReceivedForm) -> Result<(), ErrorResponse> {
    if form
        .file
        .as_ref()
        .is_some_and(|file| file.bytes_written > 0)
    {
        return Ok(());
    }
    let mut details = serde_json::Map::new();
    // 与 `extract.rs` 的 `details_of_pair` 同一个键名习惯。
    details.insert("field".to_owned(), Value::from("file"));
    Err(ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_error",
        "Request validation failed",
    )
    .with_details(details))
}

/// 表单里的 `sha256`。**空串 / 全空白等同于没给**（上游默认 `None`）。
fn optional_sha256(form: &ReceivedForm) -> Option<&str> {
    form.fields
        .get("sha256")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 413 —— 与上游 `plugin_too_large` 同一个码（**不是**通用提取器的
/// `http_error`，客户端按 `code` 分支）。
fn upload_too_large(max_bytes: u64, received: u64) -> ErrorResponse {
    let mut details = serde_json::Map::new();
    details.insert("max_bytes".to_owned(), Value::from(max_bytes));
    details.insert("received_bytes".to_owned(), Value::from(received));
    ErrorResponse::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        PLUGIN_TOO_LARGE,
        format!("插件包超过大小上限 {max_bytes} 字节"),
    )
    .with_details(details)
}

/// 解析 `Form` 里的布尔字段。
///
/// ⚠️ 与 `sm_server::config::parse_bool`（`SAKURAMEDIA_SLOW_LOG` 的白名单）
/// **不是同一套规则**：那个是上游 `perf.py` 的自定义白名单，这个是 FastAPI /
/// pydantic 的标准规则。混用会让 `enable=yes` 在一个端点上生效、在另一个上
/// 变成 `false`。
fn parse_form_bool(raw: Option<&str>, default: bool) -> Result<bool, ErrorResponse> {
    let Some(value) = raw else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
        // 空串与无法解析的值都是 422 —— pydantic 不会把「给了个空值」当作
        // 「没给」，而这里若默认成 `true`，一个手误就会让插件被启用。
        other => {
            let mut details = serde_json::Map::new();
            details.insert("field".to_owned(), Value::from("enable"));
            details.insert("value".to_owned(), Value::from(other));
            Err(ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                "Request validation failed",
            )
            .with_details(details))
        }
    }
}

/// `DELETE /{plugin_id}` —— **200 + body**，不是 204。
async fn remove_plugin(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_plugin_id): Path<String>,
) -> Result<Json<PluginInstallResponse>, ErrorResponse> {
    todo!("骨架：删除返回 200 + 卸载结果")
}
