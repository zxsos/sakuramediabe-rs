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
//! | 上游端点 | 状态码 | 状态 |
//! |---|---|---|
//! | `GET /download-clients` | 200 | ✅ 已接（只查库；**裸数组**，无分页信封）|
//! | `DELETE /download-clients/{client_id}` | **204** | ✅ 已接（两道 409 在服务层）|
//! | `POST /download-clients` | **201** | ⏳ 等 provider seam（要插件声明的配置 schema + `prepare_client`）|
//! | `POST /download-clients/test` | 200 | ⏳ 同上（**探测**用）|
//! | `PATCH /download-clients/{client_id}` | 200 | ⏳ 同上 |
//!
//! ⚠️ 骨架期这张表把五个都标成「阻塞：下载器 provider 插件」—— 前两条**过度
//! 保守**：上游 `list_clients`（`client_config_service.py:257-265`）与
//! `delete_client`（`:333-351`）都是纯库操作。
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

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;
use sm_service::transfers::download_client::{DownloadClientDiagnostic, DownloadClientTestRequest};

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

/// 下载器客户端配置（响应体）。
///
/// **直接复用服务层那一份**（`sm_service::transfers::download_client`）—— 形状只留
/// 一处定义，就不会出现「路由这份与上游差一个 `library_id`」这种事。
///
/// ⚠️ 骨架期这里有一份**本地副本**，字段是 `{id, name, kind, enabled, config}`：
/// 上游 `schema/transfers/downloads.py:14-33` 是
/// `{id, name, library_id, provider_config, created_at, updated_at}` ——
/// `kind` / `enabled` 在表里根本没有，「下载器种类」由 provider 自己解释
/// `provider_config`。
pub use sm_service::transfers::download_client::DownloadClientResource;

/// 创建 / 更新请求也**直接复用服务层那一份**。
///
/// ⚠️ 骨架期这里是两份**本地副本**（`{name, kind, config}` / `{name, enabled,
/// config}`），而上游 `schema/transfers/downloads.py:35-44` 根本没有
/// `kind` / `enabled`，真名是 `provider_config`。留着它们等于让 wire 形状有
/// 两份定义 —— 改一处漏一处时，前端发出去的键后端不认。
pub use sm_service::transfers::download_client::{
    DownloadClientCreateRequest, DownloadClientUpdateRequest,
};

/// 诊断结果。
/// `GET /download-clients`
async fn list_download_clients(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<DownloadClientResource>>, ErrorResponse> {
    // ⚠️ 骨架期这条写的是「接下载器 provider 插件（未移植）」—— **过度保守**：
    // 上游 `DownloadClientService.list_clients`（`client_config_service.py:257-265`）
    // 只查库并投影，与插件无关。排序是 `created_at DESC, id DESC`（最新在前）。
    let clients = state.download_client_service().list_clients().await?;
    Ok(Json(clients))
}

/// `POST /download-clients` —— **201 Created**。
async fn create_download_client(
    _user: CurrentUser,
    State(state): State<AppState>,
    axum::extract::Json(payload): axum::extract::Json<DownloadClientCreateRequest>,
) -> Result<(StatusCode, Json<DownloadClientResource>), ErrorResponse> {
    // 服务由 `AppState` 统一拼（含「插件能力有没有注入」），别在这里 `new`：
    // 漏了注入的表现是三个写方法一律 503，而不是「缺哪个插件报哪个错」。
    let created = state
        .download_client_service()
        .create_client(payload)
        .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `POST /download-clients/test` —— **无副作用探测，成功与失败都 200**。
async fn test_download_client(
    _user: CurrentUser,
    State(state): State<AppState>,
    axum::extract::Json(payload): axum::extract::Json<DownloadClientTestRequest>,
) -> Result<Json<DownloadClientDiagnostic>, ErrorResponse> {
    // ★ 连不上也是 200：服务层把它包成 `status = "failed"` 的诊断结果。
    let diagnostic = state.download_client_service().test_client(payload).await?;
    Ok(Json(diagnostic))
}

/// `PATCH /download-clients/{client_id}` —— 不存在 → **404**。
async fn update_download_client(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(client_id): Path<i32>,
    axum::extract::Json(payload): axum::extract::Json<DownloadClientUpdateRequest>,
) -> Result<Json<DownloadClientResource>, ErrorResponse> {
    // **部分更新**：三个字段都可以改，`None` = 没传（见请求体类型上的说明）。
    let updated = state
        .download_client_service()
        .update_client(client_id, payload)
        .await?;
    Ok(Json(updated))
}

/// `DELETE /download-clients/{client_id}` —— **204，无 body**。
async fn delete_download_client(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(client_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    // 同样与插件无关：两道 409（先「名下有任务行」、后「被索引器绑定」）
    // 与删除都在服务层，见那里的文档。
    state
        .download_client_service()
        .delete_client(client_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
