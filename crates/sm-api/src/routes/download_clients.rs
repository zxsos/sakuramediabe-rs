//! `/download-clients*` —— 下载器客户端配置五个端点。
//!
//! # 为什么从 `routes/downloads.rs` 拆出来
//!
//! 现有 `downloads.rs` 只有 `GET /download-candidates`（**已实现**）。这五个是
//! **另一件事**：管理下载器客户端的配置，而候选搜索是「搜种子」。两者依赖
//! 不同 —— 候选搜索只依赖 Torznab 索引器，这五个依赖**下载器 provider 插件**。
//!
//! 混在一起会让那个已实现的文件被插件依赖污染。
//!
//! # 与上游 `src/api/routers/transfers/downloads.py` 的对应
//!
//! | 上游端点 | 状态码 | 阻塞 |
//! |---|---|---|
//! | `GET /download-clients` | 200 | 下载器 provider 插件 |
//! | `POST /download-clients` | **201** | 同上 |
//! | `POST /download-clients/test` | 200 | 同上（**探测**用） |
//! | `PATCH /download-clients/{client_id}` | 200 | 同上 |
//! | `DELETE /download-clients/{client_id}` | **204** | 同上 |
//!
//! # `POST /download-clients/test` 是**无副作用的诊断端点**
//!
//! 它测的是「这个下载器配置能不能连上」，**不落库**。所以：
//!
//! - 它**不该**要求 `client_id` 存在 —— 测的是**待创建**的配置
//! - 探测失败返回 **200 + 诊断结果**，不是 5xx —— 「连不上」是诊断结论，
//!   不是服务端错误。返回 502 会让客户端无法区分「下载器挂了」与
//!   「本仓库出错了」。
//!
//! 这条是本文件里最容易被写错的地方：**诊断端点的失败不是 HTTP 失败。**

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/download-clients",
            get(list_download_clients)
                .post(create_download_client)
                .fallback(method_not_allowed),
        )
        .route(
            "/download-clients/test",
            post(test_download_client).fallback(method_not_allowed),
        )
        .route(
            "/download-clients/{client_id}",
            patch(update_download_client)
                .delete(delete_download_client)
                .fallback(method_not_allowed),
        )
}

/// 下载器客户端配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadClientResource {
    pub id: i32,
    pub name: String,
    /// 下载器种类（qbittorrent / transmission / …）。**具体取值由插件决定**。
    pub kind: String,
    pub enabled: bool,
    /// 插件自定义配置。**不要建模成固定结构** —— 各插件字段不同。
    pub config: serde_json::Value,
}

/// 创建请求。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientCreateRequest {
    pub name: String,
    pub kind: String,
    pub config: serde_json::Value,
}

/// 更新请求 —— **部分更新**，字段全 `Option`。
///
/// 与 `media_libraries` 的 `PATCH` 同一个 serde 局限：`None` 不区分
/// 「没传」与「显式 null」。见那个文件模块文档的说明。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DownloadClientUpdateRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub config: Option<serde_json::Value>,
}

/// 诊断结果。
///
/// **失败也返回 200** —— 见模块文档「诊断端点的失败不是 HTTP 失败」。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadClientDiagnostic {
    /// 能否连上。
    pub reachable: bool,
    /// 延迟（毫秒）；不可达时为 `None`。
    pub latency_ms: Option<i64>,
    /// 下载器版本字符串。
    pub version: Option<String>,
    /// 失败原因。`reachable = true` 时为 `None`。
    pub error: Option<String>,
}

/// `GET /download-clients`
async fn list_download_clients(
    _user: CurrentUser,
    State(_state): State<AppState>,
) -> Result<Json<Vec<DownloadClientResource>>, ErrorResponse> {
    todo!("骨架：接下载器 provider 插件（未移植）")
}

/// `POST /download-clients` —— **201 Created**。
async fn create_download_client(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<DownloadClientCreateRequest>,
) -> Result<(StatusCode, Json<DownloadClientResource>), ErrorResponse> {
    todo!("骨架：接插件；成功返回 201 + body")
}

/// `POST /download-clients/test` —— **无副作用探测，成功与失败都 200**。
async fn test_download_client(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<Json<DownloadClientDiagnostic>, ErrorResponse> {
    todo!("骨架：接插件探测；连不上也要 200 + reachable=false，不要 502")
}

/// `PATCH /download-clients/{client_id}` —— 不存在 → **404**。
async fn update_download_client(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_client_id): Path<i32>,
    axum::extract::Json(_payload): axum::extract::Json<DownloadClientUpdateRequest>,
) -> Result<Json<DownloadClientResource>, ErrorResponse> {
    todo!("骨架：接插件的部分更新")
}

/// `DELETE /download-clients/{client_id}` —— **204，无 body**。
async fn delete_download_client(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_client_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接插件删除（成功 204 不带 body）")
}
