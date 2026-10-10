//! `/media-libraries*` —— 媒体库 CRUD 五个端点。
//!
//! # 与上游 `src/api/routers/playback/media_libraries.py` 的对应
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `GET /media-libraries` | 200 |
//! | `GET /media-libraries/providers` | 200 |
//! | `POST /media-libraries` | **201** |
//! | `PATCH /media-libraries/{library_id}` | 200 |
//! | `DELETE /media-libraries/{library_id}` | **204** |
//!
//! # 请求 / 响应形状**直接复用服务层那一份**
//!
//! `MediaLibraryResource` / `MediaLibraryCreateRequest` / `MediaLibraryUpdateRequest`
//! 都来自 [`sm_service::playback::media_library`]（上游 `schema/playback/media_libraries.py`）。
//! ⚠️ 骨架期这里有**两份自造本地副本**（`MediaLibraryResponse` 用
//! `{provider, handle, enabled}`、`MediaLibraryProviderResponse` 用 `kinds`）——
//! 与上游毫无交集，已删除。provider 目录的形状由服务层的
//! `ProviderCatalogEntry`（`provider_key` / `display_name` /
//! `library_config_fields` / `playback_deliveries` / `download_config_fields`）
//! 序列化而来。
//!
//! # 用 `PATCH` 而不是 `PUT` —— 语义是「部分更新」
//!
//! 上游是 `PATCH /{library_id}`。所以 `MediaLibraryUpdateRequest` 的字段
//! **全部是 `Option`**，且**不传 = 不改**（不是置空）。
//!
//! 这里有个容易写错的点：`Option<T>` 在 serde 里的默认语义是
//! **`None` 与「显式 `null`」不分** —— 两者都会得到 `None`。所以
//! 「把某字段清成 null」和「不改这个字段」在 PATCH 里**无法区分**。
//!
//! 上游也有同样的问题（Pydantic 的 `exclude_unset` 能区分，但要走
//! `model_fields_set`）。**照抄上游的宽松行为，并在字段文档里标注** ——
//! 擅自改成 `Option<Option<T>>` 会让「清空字段」这个操作变得可达，
//! 而那需要配套的 DB 语义，不该在路由层单方面开。
//!
//! # `POST` 返回 **201** 且带 body，`DELETE` 返回 **204** 不带 body
//!
//! 这两个状态码是契约的一部分：201 让客户端知道「已创建」并可以直接用
//! 返回的 id；204 表示删除成功且**无正文**。返回 `Json<()>` 会产出
//! `null` 正文，与 204 冲突。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
// ⚠️ 用包装过的 `Json`：axum 原生的 rejection 是 422 纯文本，而项目契约是
// 422 + 错误信封（见 `crate::extract` 的说明）。
use crate::extract::Json as EnvelopeJson;
use crate::state::AppState;

use sm_service::playback::media_library::{
    MediaLibraryCreateRequest, MediaLibraryResource, MediaLibraryUpdateRequest,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/media-libraries",
            get(list_media_libraries).post(create_media_library),
        )
        .route(
            "/media-libraries/providers",
            get(list_media_library_providers),
        )
        .route(
            "/media-libraries/{library_id}",
            patch(update_media_library).delete(delete_media_library),
        )
}

/// `GET /media-libraries` —— 上游 `:21-24`。排序 `created_at DESC, id DESC`。
async fn list_media_libraries(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<MediaLibraryResource>>, ErrorResponse> {
    Ok(Json(state.media_library_service().list_libraries().await?))
}

/// `GET /media-libraries/providers` —— 上游 `:27-29`。
///
/// 列出**可用的 provider 类型**。这份数据来自**插件注册表**（`sm-plugins`），
/// 不是数据库 —— 没有注入注册表时返回**空列表**而**不是 503**
/// （「没装插件」与「装了但一个都没注册」表现一致，上游亦然）。
///
/// 每项的形状由服务层的 `ProviderCatalogEntry` 序列化而来（`serde_json::Value`，
/// 因为目录是**插件自定**的，且服务层还要把它喂给状态端点）。
async fn list_media_library_providers(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<serde_json::Value>>, ErrorResponse> {
    Ok(Json(
        state
            .media_library_service()
            .list_provider_catalog()
            .await?,
    ))
}

/// `POST /media-libraries` —— **201 Created**。
///
/// 服务层在**落库之前**先 `prepare_library`（provider 建目录失败时不留
/// 「库存在但用不了」的记录）；未注入注册表 → **503 `provider_not_installed`**。
async fn create_media_library(
    State(state): State<AppState>,
    _user: CurrentUser,
    EnvelopeJson(payload): EnvelopeJson<MediaLibraryCreateRequest>,
) -> Result<(StatusCode, Json<MediaLibraryResource>), ErrorResponse> {
    let created = state
        .media_library_service()
        .create_library(payload)
        .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `PATCH /media-libraries/{library_id}` —— 部分更新。
///
/// 库不存在 → **404 `media_library_not_found`**（details `library_id`）。
/// `exclude_unset` 之后**一个字段都没给** → **422 `empty_media_library_update`**
/// （`media_library_service.py:291-292`）—— 不是「原样返回」。
async fn update_media_library(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(library_id): Path<i64>,
    EnvelopeJson(payload): EnvelopeJson<MediaLibraryUpdateRequest>,
) -> Result<Json<MediaLibraryResource>, ErrorResponse> {
    let resource = state
        .media_library_service()
        .update_library(narrow_library_id(library_id)?, payload)
        .await?;
    Ok(Json(resource))
}

/// `DELETE /media-libraries/{library_id}` —— **204，无 body**。
///
/// ⚠️ 骨架期这里写「库不存在时**仍然 204**：删除是幂等的」—— **错**。上游
/// `delete_library`（`media_library_service.py:313-326`）先 `_require_library`：
///
/// - 库不存在 → **404 `media_library_not_found`**（details `library_id`）
///   —— **不幂等**，别照抄本仓其它 delete 的 204 语义；
/// - 仍被 `Media` 或 `DownloadClient` 引用 → **409 `media_library_in_use`**
///   （details `library_id`）—— **不**级联删媒体，也不删下载客户端。
///
/// 只有「存在且未被引用」才走到 204。
async fn delete_media_library(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(library_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    state
        .media_library_service()
        .delete_library(narrow_library_id(library_id)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 路径里的 id 是 `i64`（axum 的整数解析），而库里的列是 `i32`。
///
/// # 超出 `i32` 时是 **404，不是 400**
///
/// 上游是 Python 的 `int`，**没有上界** —— 那种 id 会一路走到查询、查不到、
/// 报「不存在」。用 `Path<i32>` 会在提取阶段就变成 400，与上游不一致。
/// 所以这里保留 `i64` 再显式收窄，溢出按「不存在」处理。
///
/// 详情键是 `library_id`（上游 `require_by_id(..., error_details_key="library_id")`，
/// `media_library_service.py:37-44`）—— 与 [`crate::routes::media::narrow_media_id`]
/// 用的是**不同**的键，别混。
fn narrow_library_id(library_id: i64) -> Result<i32, ErrorResponse> {
    i32::try_from(library_id).map_err(|_| {
        let mut details = serde_json::Map::new();
        details.insert("library_id".to_owned(), serde_json::Value::from(library_id));
        ErrorResponse::from(sm_service::error::ServiceError::not_found_with(
            "media_library_not_found",
            "Media library not found",
            details,
        ))
    })
}
