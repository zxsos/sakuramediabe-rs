//! `/media/*/play*` —— **两个协议端点** + 播放模式查询。
//!
//! # 为什么这两个单独成文件，而**不**并进 `routes/media.rs`
//!
//! 其余 12 个 `/media*` 端点是普通 CRUD：解析参数 → 查库 → 返回 JSON。
//! 这两个不是 —— 它们：
//!
//! 1. **先验签名**（`require_signed_params` + `verify_media_signature`），
//!    签名不过直接拒，**不查库**。
//! 2. **返回的不是 JSON**，而是 `Response`（字节流）或 **302**。
//! 3. 需要 `library_handle`（provider 插件提供的真实路径）—— 这是
//!    `provider_protocol` 的第一个真实消费方。
//!
//! 混进 CRUD 那个文件里，那个文件就得同时处理「提取器 + 签名校验 + 流式响应
//! + 重定向」四类关注点。**协议端点与资源端点是两种东西。**
//!
//! # 投递方式：`proxy` 与 `redirect` 是**两种响应形态**
//!
//! | delivery | 响应 | 客户端行为 |
//! |---|---|---|
//! | `proxy`（或不传） | **200 + 字节流** | 本仓库中转，客户端只连本仓库 |
//! | `redirect` | **302** 到 provider 的真实地址 | 客户端直连 provider |
//!
//! # 三处容易漏的语义
//!
//! **1. `expires` 与 `signature` 是「同生共死」的。** `require_signed_params`
//! 先检查两个**都存在**，缺一即拒 —— 不是一个可选一个必填。
//!
//! **2. `playback_attempt_id` 有严格格式。** 上游
//! `Query(min_length=20, max_length=64, pattern=r"^[A-Za-z0-9_-]+$")` ——
//! 长度 20~64 且只允许 URL 安全字符。宽松接受会让脏 id 进到 provider 侧。
//!
//! **3. 库或媒体不存在都是 404，但 code 不同。**
//! 媒体缺失 → `media_not_found`；媒体在但 `library` 为空 → `media_library_not_found`。
//! **合成一个 code 会让客户端无法区分「影片被删」与「媒体库配置坏了」** ——
//! 后者要提示用户去修配置。
//!
//! # `merged-play` 与 `play` 的区别
//!
//! `merged-play` 接受 **CSV 的 `media_ids`**（多个媒体按时间轴拼接），且**没有**
//! `delivery` 参数 —— 合并流只能中转，不能重定向到单个 provider 地址。

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/media/playback-attempts/{attempt_id}",
            get(get_playback_attempt_mode),
        )
        .route("/media/{media_id}/play/{*resource_path}", get(play_media))
        .route(
            "/media/merged-play/{*resource_path}",
            get(play_merged_media),
        )
}

/// 播放投递方式。
///
/// `None` = 客户端没指定 → 与上游一致按 `proxy` 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// 本仓库中转，200 + 字节流。
    #[default]
    Proxy,
    /// 302 到 provider 的真实地址。
    Redirect,
}

/// `GET /media/{media_id}/play/{resource_path}` 的查询参数。
#[derive(Debug, Default, Deserialize)]
pub struct PlayQuery {
    /// 签名过期时间戳。**与 `signature` 同生共死**。
    pub expires: Option<i64>,
    /// 签名。**与 `expires` 同生共死**。
    pub signature: Option<String>,
    /// 投递方式；不传按 `proxy`。
    pub delivery: Option<Delivery>,
    /// 播放尝试 id。**上游有严格格式**：长度 20~64、`^[A-Za-z0-9_-]+$`。
    pub playback_attempt_id: Option<String>,
}

/// 播放模式响应。
#[derive(Debug, Serialize)]
pub struct PlaybackModeResponse {
    pub attempt_id: String,
    /// `proxy` 还是 `redirect`。
    pub delivery: &'static str,
    /// 实际选定的投递方式（可能与请求的不同，见 handler 注释）。
    pub actual_delivery: &'static str,
}

/// `GET /media/playback-attempts/{attempt_id}` —— 查播放模式。
///
/// 上游 `get_playback_attempt_mode`（`media.py` 第 6 个端点）。
async fn get_playback_attempt_mode(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_attempt_id): Path<String>,
) -> Result<Json<PlaybackModeResponse>, ErrorResponse> {
    todo!("骨架：接 playback attempt 查询")
}

/// `GET /media/{media_id}/play/{resource_path}` —— 单媒体播放。
///
/// 上游 `play_media`。顺序**不能改**：
///
/// 1. `require_signed_params(expires, signature)` —— 两个都在才继续
/// 2. `verify_media_signature(media_id, resource_path, expires, signature)`
/// 3. 查媒体 → 不存在则 `media_not_found`（404）
/// 4. 查媒体库 → 为空则 `media_library_not_found`（404）
/// 5. 取 `library_handle`（**provider 插件**，本仓库尚未接）
/// 6. 按 `delivery` 分支：proxy 中转 / redirect 302
///
/// 返回 `Response` 而**不是** `Json` —— 两条分支都不是 JSON。
async fn play_media(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path((_media_id, _resource_path)): Path<(i64, String)>,
    Query(_query): Query<PlayQuery>,
) -> Result<Response, ErrorResponse> {
    todo!("骨架：需先接 provider 插件拿 library_handle；照上游 play_media 实现（签名 -> 404 两级 -> proxy/redirect）")
}

/// `GET /media/merged-play/{resource_path}` —— 多媒体合并播放。
#[derive(Debug, Default, Deserialize)]
pub struct MergedPlayQuery {
    /// CSV 的媒体 id。**与 `play` 不同：合并流只能中转，不能 302**，
    /// 所以这里没有 `delivery` 参数。
    pub media_ids: Option<String>,
    pub expires: Option<i64>,
    pub signature: Option<String>,
}

/// 多媒体合并播放。
///
/// 上游 `play_merged_media`。**注意它不做逐媒体的 404 分级** —— 与 `play_media`
/// 的两级 404 语义不同，实现时别照抄错。
async fn play_merged_media(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_resource_path): Path<String>,
    Query(_query): Query<MergedPlayQuery>,
) -> Result<Response, ErrorResponse> {
    todo!("骨架：需 provider 插件；照上游 play_merged_media 实现")
}

/// 构造 302 响应（`redirect` 分支）。
///
/// 单独立函数是因为两个分支都要用，而 302 的 `Location` 头必须
/// **百分号编码** —— provider 路径里有空格与中文时不编码会让客户端解析失败。
pub fn redirect_response(location: &str) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = axum::http::StatusCode::FOUND;
    if let Ok(value) = header::HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}
