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
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_service::playback::provider_helpers::{DeliveryTarget, PlaybackPlan, RequestedDelivery};
use sm_service::playback::proxy;

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

/// 播放模式响应。上游 `MediaPlaybackModeResource`
/// （`schema/playback/media.py:70-71`）。
///
/// # ⚠️ 骨架期这里是**三个**字段，那是猜的
///
/// 原先写的是 `{attempt_id, delivery, actual_delivery}`。**不是本仓的有意扩展** ——
/// `actual_delivery` 在整个工作区只出现在上游一个**局部变量名**里，从没有过响应
/// 字段。契约由消费者定：
///
/// ```dart
/// // sakuramedia/lib/widgets/domain/media/media_playback_info_button.dart:94
/// final label = switch (response['mode']) { 'direct' => '直连', 'proxy' => '后端代理', ... };
/// ```
///
/// # ★ 值域是 `direct` / `proxy`，**不是 `redirect`**
///
/// 上游在**写入时**就把 delivery 折成 mode（`media.py:83`：
/// `"direct" if delivery == "redirect" else "proxy"`）。透传 `redirect` 不会报错 ——
/// 前端落到 `_ => '未确认'`，**静默显示「未确认」**。
#[derive(Debug, Serialize)]
pub struct PlaybackModeResponse {
    /// `direct` / `proxy`。
    ///
    /// `None` = 没登记或已过期 → **200 + `null`，不是 404**：前端起播后会重试 3 次
    /// （「playlist 通知可能先于网关响应」），「还没登记」是正常中间态。
    pub mode: Option<&'static str>,
}

/// `GET /media/playback-attempts/{attempt_id}` —— 查播放模式。
///
/// 上游 `get_playback_attempt_mode`（`media.py` 第 6 个端点）。
async fn get_playback_attempt_mode(
    _user: CurrentUser,
    Path(attempt_id): Path<String>,
) -> Result<Json<PlaybackModeResponse>, ErrorResponse> {
    // ★ 没登记**不是** 404。
    //
    // 前端 `_openMedia` 起播后立刻来问，还可能问一次没拿到就再试（
    // `media_playback_info_button.dart:84-86` 重试 3 次、每次隔 1 秒，注释写着
    // 「playlist 通知可能先于网关响应」）。所以「还没登记」是**正常中间态**，
    // 回 200 + `mode: null`。回 404 会把它说成「这个 attempt 不存在」——
    // 那是另一个意思，客户端会据此停止重试。
    Ok(Json(PlaybackModeResponse {
        mode: sm_service::playback::playback_mode::shared().get(&attempt_id),
    }))
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
    State(state): State<AppState>,
    _user: CurrentUser,
    Path((media_id, resource_path)): Path<(i64, String)>,
    Query(query): Query<PlayQuery>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    // ① 签名参数：缺一即拒，**不查库**。
    let (expires, signature) = require_signed_params(query.expires, query.signature.as_deref())?;

    // ② 路径参数是 `i64`，签名域是 `i32`（上游同样以 int 签）。超界的 id 必然取不到
    //    记录 —— 直接 404，别截断成一个**别的** id（那会验成别的媒体的签名）。
    let media_id = i32::try_from(media_id)
        .map_err(|_| ErrorResponse::new(StatusCode::NOT_FOUND, "media_not_found", "媒体不存在"))?;

    // ③ 验签。归一化在过期/签名比对**之前** —— 非法路径（`..` / 绝对路径）优先被拒。
    let secret = crate::signing::signing_secret(&state)?;
    let normalized = sm_core::signing::verify_media(
        &secret,
        media_id,
        &resource_path,
        expires,
        signature,
        crate::signing::now_seconds(),
    )
    .map_err(|error| ErrorResponse::new(StatusCode::FORBIDDEN, error.code(), error.message()))?;

    // ④ `playback_attempt_id` 的格式（上游是 pydantic 的 `Query(...)` 约束）。
    //    必须在**打到 provider 之前**拒 —— 脏 id 不该进到插件侧。
    if let Some(attempt_id) = query.playback_attempt_id.as_deref() {
        validate_playback_attempt_id(attempt_id)?;
    }

    // ⑤ 两级 404（`media_not_found` / `media_library_not_found`）、取 provider、
    //    调插件拿计划 —— 都在服务层，路由不重复那套判定。
    let requested = match query.delivery.unwrap_or_default() {
        Delivery::Proxy => RequestedDelivery::Proxy,
        Delivery::Redirect => RequestedDelivery::Redirect,
    };
    let plan = state
        .media_service()
        .plan_playback(media_id, &normalized, requested)
        .await
        .map_err(ErrorResponse::from)?;

    let (response, actual_delivery) = deliver(&plan, &normalized, range_header(&headers)).await?;

    // ⑥ 记下「这次实际用了哪种投递方式」。
    //
    //    ⚠️ 只在**主文件**时记（资源路径为空）—— 上游 `media.py:309-313` 就是
    //    `if playback_attempt_id is not None and not normalized_path`。字幕、封面
    //    这类子资源不进这张表：播放器上报的模式是针对整条流的。
    if let Some(attempt_id) = query.playback_attempt_id.as_deref() {
        if normalized.is_empty() {
            sm_service::playback::playback_mode::shared().record(attempt_id, actual_delivery);
        }
    }

    Ok(response)
}

/// 上游 `require_signed_params`（`routers/_utils.py`）：**缺一即拒**。
///
/// 与 `files.rs` 那份同源但没共用：那边是私有函数、且绑定 `SignedUrlQuery`，
/// 这里两个端点的查询类型不同（`PlayQuery` / `MergedPlayQuery`）。判据一致 ——
/// **空串也算没给**（Python 的真值判断），光判 `Some` 不够。
fn require_signed_params(
    expires: Option<i64>,
    signature: Option<&str>,
) -> Result<(i64, &str), ErrorResponse> {
    match (expires, signature) {
        (Some(expires), Some(signature)) if !signature.is_empty() => Ok((expires, signature)),
        _ => Err(ErrorResponse::new(
            StatusCode::FORBIDDEN,
            "file_signature_invalid",
            "文件签名无效",
        )),
    }
}

/// `playback_attempt_id` 的格式校验。
///
/// 上游：`Query(default=None, min_length=20, max_length=64, pattern=r"^[A-Za-z0-9_-]+$")`。
/// **宽松接受会让脏 id 进到 provider 侧**，那时已经没法说是谁的错。
///
/// 码走 [`crate::error::validation_error`]（422 + `validation_error`）：上游这里是
/// pydantic 层抛的，形态是 422 + `detail` 数组、**没有**具名 code，所以照本仓既有
/// 的 pydantic 校验口径处理。
fn validate_playback_attempt_id(attempt_id: &str) -> Result<(), ErrorResponse> {
    let length_ok = (20..=64).contains(&attempt_id.len());
    let charset_ok = attempt_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if length_ok && charset_ok {
        return Ok(());
    }
    Err(crate::error::validation_error(
        "playback_attempt_id 长度须为 20~64，且只含 [A-Za-z0-9_-]",
    ))
}

/// 客户端请求里的 `Range` 头（原样交给转发层，**不解析**）。
fn range_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
}

/// 把一份投递计划落成 HTTP 响应，并回报**实际选定**的投递方式。
///
/// 回报值是给 `playback-attempts` 查询用的（上游 `_PLAYBACK_MODE_RESULTS.record`
/// 记的就是它）—— 在这里返回而不是让调用方从 `plan` 再推一遍：那种推法要在
/// 「计划里没有目标」的分支上编一个值，而那个分支根本到不了这里。
async fn deliver(
    plan: &PlaybackPlan,
    resource_path: &str,
    range: Option<&str>,
) -> Result<(Response, &'static str), ErrorResponse> {
    // `unavailable` 是**正常应答里的否定结果**（文件不在 / 权限没了），不是 provider
    // 故障 —— 所以是 404，不是 502。合成 502 会让「影片文件被删」看起来像
    // 「插件坏了」，用户会去重装插件。
    if plan.unavailable {
        return Err(ErrorResponse::new(
            StatusCode::NOT_FOUND,
            "media_unavailable",
            "媒体文件不可用",
        ));
    }

    let Some(target) = plan.delivery.as_ref() else {
        // 插件违约：既没标 `unavailable`、也没给投递方式。网关转换时已 `warn`。
        return Err(ErrorResponse::new(
            StatusCode::BAD_GATEWAY,
            "provider_playback_unavailable",
            "媒体提供方未给出投递方式",
        ));
    };

    match target {
        DeliveryTarget::Redirect { url, headers } => {
            Ok((redirect_with_headers(url, headers), "redirect"))
        }
        DeliveryTarget::Proxy {
            endpoint,
            path_prefix,
            headers,
        } => {
            // 转发层**不解析 Range**（只原样转发），Range 归持有字节的那一侧算 ——
            // 避免两侧各算一遍 `Content-Range` 导致头与 body 长度不一致。
            let proxied = proxy::shared()
                .fetch(endpoint, path_prefix, headers, resource_path, range)
                .await?;
            Ok((proxy_response(proxied), "proxy"))
        }
    }
}

/// 302 + 计划自带的响应头。
fn redirect_with_headers(location: &str, headers: &[(String, String)]) -> Response {
    let mut response = redirect_response(location);
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// 把转发结果包成响应：**状态码与头都来自插件**。
///
/// 状态码不在这里重算 —— 200 / 206 / 416 是插件算出来的，宿主改写它就会与
/// `Content-Range` 对不上。
fn proxy_response(proxied: proxy::ProxiedMedia) -> Response {
    let mut response = Response::new(axum::body::Body::from_stream(proxied.body));
    *response.status_mut() =
        StatusCode::from_u16(proxied.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in proxied.headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
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
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(resource_path): Path<String>,
    Query(query): Query<MergedPlayQuery>,
    headers: HeaderMap,
) -> Result<Response, ErrorResponse> {
    // ① 签名参数：与 `play` 共用同一个判据（上游 `_utils.py` 也只有一份）。
    let (expires, signature) = require_signed_params(query.expires, query.signature.as_deref())?;

    // ② 分段 id。★ **顺序敏感** —— 这个序列进签名载荷，所以解析出来的顺序要原样
    //    带到验签与服务层，中途不能排序或去重（去重是**判据**，不是整理）。
    let media_ids = parse_merged_media_ids(query.media_ids.as_deref())?;

    // ③ 验签（载荷含整个 id 序列，见 `sm_core::signing::verify_merged`）。
    let secret = crate::signing::signing_secret(&state)?;
    let normalized = sm_core::signing::verify_merged(
        &secret,
        &media_ids,
        &resource_path,
        expires,
        signature,
        crate::signing::now_seconds(),
    )
    .map_err(|error| ErrorResponse::new(StatusCode::FORBIDDEN, error.code(), error.message()))?;

    // ④ 五道门（无效分段 / 跨影片 / 跨媒体库 / 库缺失 / 插件没装）在服务层。
    //    合并流的投递方式**不由客户端选**（`MergedPlayQuery` 里没有 `delivery`）。
    let plan = state
        .media_service()
        .plan_merged_playback(&media_ids, &normalized)
        .await
        .map_err(ErrorResponse::from)?;

    // ⑤ 落成响应。**没有 302 分支可选** —— 合并流没有单个 provider 地址可指。
    //    但计划若仍是 `Redirect`（插件答非所问），交给 `deliver` 按计划走，宿主
    //    不替它猜；`deliver` 的 `actual_delivery` 在这条路上没人用。
    let (response, _actual_delivery) = deliver(&plan, &normalized, range_header(&headers)).await?;
    Ok(response)
}

/// `media_ids` CSV → **有序** id 列表。上游 `_parse_merged_media_ids`
/// （`media.py:184-196`）。
///
/// # 三道判据，码**各不相同**
///
/// | 条件 | 码 |
/// |---|---|
/// | 非正整数 / 有空项 | 422 `invalid_merged_playback`（来自共享的 CSV 解析）|
/// | 缺失或**少于 2 个** | 422 `merged_playback_need_at_least_two` |
/// | 有重复 | 422 `invalid_merged_playback` |
///
/// ★ 第二条是合并播放**独有**的：一个分段不构成合并。上游把下限设成 2 而不是
/// 「非空」，所以 `media_ids=5` 是 **422**，而**不是**退化成单媒体播放 ——
/// 静默降级会让客户端拿到一条「成功但只有一段」的流。
fn parse_merged_media_ids(raw: Option<&str>) -> Result<Vec<i32>, ErrorResponse> {
    // 复用图搜那份解析（上游也是共享 `_utils.py` 的一个函数）；**只有码不同**。
    let parsed = crate::routes::image_search::parse_csv_positive_ints(
        raw,
        "media_ids",
        "invalid_merged_playback",
    )?;
    let Some(ids) = parsed else {
        return Err(merged_playback_need_at_least_two());
    };
    if ids.len() < 2 {
        return Err(merged_playback_need_at_least_two());
    }
    // 去重判据与上游 `len(set(media_ids)) != len(media_ids)` 一致：**必须保序**，
    // 所以判重不能靠「排序后看相邻」。
    let mut seen = std::collections::HashSet::new();
    if !ids.iter().all(|id| seen.insert(*id)) {
        return Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_merged_playback",
            "合并播放分段不可重复",
        ));
    }
    ids.into_iter()
        .map(|id| {
            // 与 `play_media` 同一个理由：路径/查询里的整数超 i32 必然取不到记录。
            i32::try_from(id).map_err(|_| {
                ErrorResponse::new(StatusCode::NOT_FOUND, "media_not_found", "媒体不存在")
            })
        })
        .collect()
}

fn merged_playback_need_at_least_two() -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "merged_playback_need_at_least_two",
        "合并播放至少需要 2 个分段",
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_with(delivery: Option<DeliveryTarget>, unavailable: bool) -> PlaybackPlan {
        PlaybackPlan {
            delivery,
            file_name: "m.mp4".to_owned(),
            size_bytes: Some(1),
            content_type: None,
            unavailable,
        }
    }

    /// ★ `unavailable` 落成 **404**，不是 502。
    ///
    /// `unavailable` 是**正常应答里的否定结果**（文件不在 / 权限没了）。合成 502
    /// 会让「影片文件被删」看起来像「插件坏了」—— 用户会去重装插件，而真正该做的
    /// 是换一个文件或修媒体库配置。
    #[tokio::test]
    async fn an_unavailable_plan_is_a_404_not_a_502() {
        let error = deliver(&plan_with(None, true), "", None)
            .await
            .expect_err("provider 说资源不可用");

        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.error.code, "media_unavailable");
    }

    /// 计划里既没标 `unavailable`、也没给目标 = 插件违约 → 502（宿主兜底）。
    ///
    /// 与上一条**必须不同**：上一条是「资源确实不在」，这条是「插件答非所问」。
    /// 两者都 404 的话，插件把自己的 bug 伪装成了「文件被删」。
    #[tokio::test]
    async fn a_plan_without_a_target_is_a_502() {
        let error = deliver(&plan_with(None, false), "", None)
            .await
            .expect_err("插件没给投递方式");

        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    }

    /// ★ `redirect` 落成 **302**，且计划自带的头**原样**在响应上。
    ///
    /// 那些头是 provider 给的（常见是鉴权 / 防盗链），少带一个会让客户端直连存储
    /// 时 403 —— 而错误指向的是「存储拒绝了我」，不是「宿主漏带了头」。
    #[tokio::test]
    async fn a_redirect_plan_becomes_a_302_with_its_headers() {
        let (response, actual) = deliver(
            &plan_with(
                Some(DeliveryTarget::Redirect {
                    url: "https://pan.example/d/abc".to_owned(),
                    headers: vec![("x-token".to_owned(), "t".to_owned())],
                }),
                false,
            ),
            "",
            None,
        )
        .await
        .expect("可以 302");

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .expect("Location 必须设上")
                .to_str()
                .expect("ASCII"),
            "https://pan.example/d/abc"
        );
        assert_eq!(
            response
                .headers()
                .get("x-token")
                .expect("计划自带的头要原样带上")
                .to_str()
                .expect("ASCII"),
            "t"
        );
        assert_eq!(
            actual, "redirect",
            "回报给 playback-attempts 的必须是实际选定的"
        );
    }

    /// `require_signed_params`：**空串也算没给**。
    ///
    /// 判据是 `expires is None or not signature`（Python 真值判断）—— 光判 `Some`
    /// 会让 `?signature=` 这种 URL 通过，然后在验签环节报成「签名不匹配」，
    /// 把「URL 不完整」指成「签名算错了」。
    #[test]
    fn an_empty_signature_counts_as_missing() {
        assert!(
            require_signed_params(Some(1), Some("")).is_err(),
            "空串等于没给"
        );
        assert!(
            require_signed_params(None, Some("x")).is_err(),
            "缺 expires"
        );
        assert!(
            require_signed_params(Some(1), None).is_err(),
            "缺 signature"
        );
        assert_eq!(
            require_signed_params(Some(1), Some("x")).expect("两个都在"),
            (1, "x")
        );
    }

    /// `playback_attempt_id` 的边界与字符集。
    #[test]
    fn the_attempt_id_bounds_and_charset_are_enforced() {
        assert!(
            validate_playback_attempt_id(&"a".repeat(19)).is_err(),
            "19 太短"
        );
        assert!(validate_playback_attempt_id(&"a".repeat(20)).is_ok());
        assert!(validate_playback_attempt_id(&"a".repeat(64)).is_ok());
        assert!(
            validate_playback_attempt_id(&"a".repeat(65)).is_err(),
            "65 太长"
        );
        assert!(
            validate_playback_attempt_id(&format!("{}!", "a".repeat(19))).is_err(),
            "`!` 不在 [A-Za-z0-9_-] 里"
        );
        assert!(
            validate_playback_attempt_id(&format!("{}-_", "a".repeat(19))).is_ok(),
            "连字符与下划线是允许的"
        );
    }

    /// `media_ids`：顺序**原样保留**（它进签名载荷）。
    #[test]
    fn merged_ids_keep_their_input_order() {
        assert_eq!(
            parse_merged_media_ids(Some("9, 3,7")).expect("合法"),
            vec![9, 3, 7],
            "排序过就会让一份调换顺序的 URL 验成合法"
        );
    }

    /// ★ 少于 2 个是 **422 `merged_playback_need_at_least_two`**，
    /// **不是**退化成单媒体播放。
    ///
    /// 降级会让客户端拿到一条「成功但只有一段」的流 —— 而它以为自己要看的是拼
    /// 接结果。缺参数（`None`）与只给一个走同一个码。
    #[test]
    fn merged_ids_need_at_least_two() {
        for raw in [None, Some("5")] {
            let error = parse_merged_media_ids(raw).expect_err("不足 2 个");
            assert_eq!(
                error.error.code, "merged_playback_need_at_least_two",
                "{raw:?}"
            );
        }
    }

    /// 重复分段 → 422 `invalid_merged_playback`。
    ///
    /// 与「不足 2 个」**不同码**：`1,1` 的长度是 2，但只有一段素材。
    #[test]
    fn duplicate_merged_ids_are_rejected() {
        let error = parse_merged_media_ids(Some("1,1")).expect_err("重复");
        assert_eq!(error.error.code, "invalid_merged_playback");
    }

    /// ★ 未登记 → `{"mode":null}`，且响应**只有这一个字段**。
    ///
    /// 键名与值与前端逐字对齐（`media_playback_info_button.dart:94` 的
    /// `switch (response['mode'])`）。多一个字段不会报错，但少一个键名对不上就是
    /// 静默显示「未确认」—— 所以这里用**全等**断言，顺带钉住「没有多余字段」。
    #[test]
    fn the_mode_response_is_exactly_one_nullable_field() {
        let unknown = serde_json::to_value(PlaybackModeResponse { mode: None }).expect("序列化");
        assert_eq!(
            unknown,
            serde_json::json!({ "mode": null }),
            "未登记是 null，不是缺字段也不是 404"
        );

        let known = serde_json::to_value(PlaybackModeResponse {
            mode: Some("direct"),
        })
        .expect("序列化");
        assert_eq!(known, serde_json::json!({ "mode": "direct" }));
    }

    /// 非正整数与空项走共享解析 —— 但码是**合并播放的**那个。
    ///
    /// 这条盯着「码是参数」这件事：图搜那边同一个函数报的是
    /// `invalid_image_search_filter`，写死就会串码。
    #[test]
    fn a_bad_merged_id_uses_the_merged_error_code() {
        for raw in ["0", "-1", "abc", "1,,2", ""] {
            let error = parse_merged_media_ids(Some(raw)).expect_err("非法");
            assert_eq!(error.error.code, "invalid_merged_playback", "raw={raw:?}");
        }
    }
}
