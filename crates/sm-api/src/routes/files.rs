//! `/files/images/*` 与 `/files/subtitles/{id}` —— 签名文件服务两个端点。
//!
//! # 两个端点都不在 OpenAPI 里
//!
//! 上游两者都带 `include_in_schema=False`。所以它们**不是契约的一部分**，
//! 是给前端内部用的私有通道 —— 客户端拿到的是**签名 URL**，不是自己拼的。
//!
//! 与 `routes/media_playback.rs` 的播放端点同源：**客户端不自己签名，
//! 由本仓库签发带 `expires` + `signature` 的 URL**。
//!
//! # 为什么放在一个文件而不是两个
//!
//! 两者的验签逻辑完全相同（`expires` + `signature` + 过期检查），差别只有
//! 「按路径取」与「按 id 取」。拆两个文件会让验签那段写两遍 ——
//! **而验签那段是最不该写两遍的代码**（改一次漏一处就是签名可伪造）。
//!
//! # 一处路径形态差异
//!
//! | 端点 | 寻址 | 备注 |
//! |---|---|---|
//! | `/files/images/{file_path:path}` | **通配路径** | 可含 `/`，如 `poster/abc.jpg` |
//! | `/files/subtitles/{subtitle_id}` | **整数 id** | 字幕按主键取 |
//!
//! 所以图片那个用通配（axum 0.8 是 `{*file_path}`），字幕那个用 `i64`。
//! **别把字幕也用通配** —— 那会让 `/files/subtitles/../../etc/passwd` 这类
//! 路径进入解析，虽然最终会被 404，但**不该靠 404 兜住路径穿越**。
//!
//! # 验签失败的码
//!
//! 过期或签名不匹配 → **403**（不是 401）。401 是「没认证」，这里是
//! 「认证过了但这个 URL 不认」—— 两者语义不同，客户端的处理也不同
//! （401 会去重新登录，403 会去重新取 URL）。

use axum::extract::{Path, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use sm_service::catalog::movie_subtitle::MovieSubtitleService;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        // 注意 `{*file_path}` 是 axum 0.8 的通配语法（0.7 是 `{file_path:path}`）。
        .route(
            "/files/images/{*file_path}",
            get(get_image_file).fallback(method_not_allowed),
        )
        .route(
            "/files/subtitles/{subtitle_id}",
            get(get_subtitle_file).fallback(method_not_allowed),
        )
}

/// 签名参数 —— 两个端点共用。
///
/// **`expires` 与 `signature` 同生共死**：缺一即拒（与
/// `routes/media_playback.rs` 里 `require_signed_params` 同一条规则）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SignedUrlQuery {
    /// 过期时间戳（秒）。
    pub expires: Option<i64>,
    /// 签名。
    pub signature: Option<String>,
}

/// 验签。**过期或签名不匹配 → 403**（不是 401，见模块文档）。
///
/// 两个端点都调它 —— 验签逻辑只有这一处。
pub fn verify_signature(query: &SignedUrlQuery, scope: &str) -> Result<(), ErrorResponse> {
    let (Some(expires), Some(signature)) = (query.expires, query.signature.as_deref()) else {
        return Err(ErrorResponse::new(
            axum::http::StatusCode::FORBIDDEN,
            "signed_url_required",
            "缺少签名参数",
        ));
    };
    if expires < 0 {
        return Err(ErrorResponse::new(
            axum::http::StatusCode::FORBIDDEN,
            "signed_url_expired",
            "签名已过期",
        ));
    }
    let _ = signature;
    let _ = scope;
    todo!("骨架：接签名校验（时间常数比较；scope 参与签名以免跨端点复用）")
}

/// `GET /files/images/{file_path:path}` —— **不在 OpenAPI 里**。
async fn get_image_file(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_file_path): Path<String>,
    EnvelopeQuery(_query): EnvelopeQuery<SignedUrlQuery>,
) -> Result<Response, ErrorResponse> {
    todo!("骨架：接签名校验（403）+ 文件读取；返回字节流而非 JSON")
}

/// `GET /files/subtitles/{subtitle_id}` —— **不在 OpenAPI 里**。
///
/// 字幕按**整数主键**取，**不用通配**（见模块文档的路径穿越说明）。
async fn get_subtitle_file(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(subtitle_id): Path<i64>,
    EnvelopeQuery(query): EnvelopeQuery<SignedUrlQuery>,
) -> Result<Response, ErrorResponse> {
    // 上游 `require_signed_params`（`routers/_utils.py`）：**缺一即拒**，码是
    // `file_signature_invalid`（不是「缺少参数」那类 400 —— 对客户端来说
    // 「URL 不完整」与「签名不对」都是「这个 URL 不认」）。
    let (Some(expires), Some(signature)) = (query.expires, query.signature.as_deref()) else {
        return Err(ErrorResponse::new(
            StatusCode::FORBIDDEN,
            "file_signature_invalid",
            "文件签名无效",
        ));
    };
    // 路径参数是 `i64`（axum 的整数解析），签名域是 `i32`（上游同样以 int 签）。
    // 超出 i32 的 id 必然取不到记录 —— 直接 404，别静默截断成一个**别的** id。
    let subtitle_id = i32::try_from(subtitle_id).map_err(|_| {
        ErrorResponse::new(StatusCode::NOT_FOUND, "subtitle_not_found", "字幕不存在")
    })?;

    let secret = crate::signing::signing_secret(&state)?;
    // 验签的三种失败（缺参数另算）：过期、签名不匹配、格式不对 —— 都由
    // `sm_core::signing` 那张表给出**码与文案**，这里不重新映射。
    sm_core::signing::verify_subtitle(
        &secret,
        subtitle_id,
        expires,
        signature,
        crate::signing::now_seconds(),
    )
    .map_err(|error| ErrorResponse::new(StatusCode::FORBIDDEN, error.code(), error.message()))?;

    let path = MovieSubtitleService::new(state.db(), state.config())
        .resolve_file_path(subtitle_id)
        .await?;
    // 上游 `require_existing_file`：文件不在是 **404 `file_not_found`**
    // —— **不是**读接口那个 409 `subtitle_unavailable`（见服务层文档）。
    if !path.is_file() {
        return Err(ErrorResponse::new(
            StatusCode::NOT_FOUND,
            "file_not_found",
            "文件不存在",
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        tracing::error!(path = %path.display(), %error, "字幕文件读取失败");
        ErrorResponse::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "subtitle_read_failed",
            "字幕文件读取失败",
        )
    })?;

    // 上游 `FileResponse(..., media_type="text/plain; charset=utf-8")`
    // （`routers/files/subtitles.py:21`）—— 字幕是纯文本，且**编码交给客户端**，
    // 所以不带 BOM、不做转码。
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(axum::body::Body::from(bytes))
        .map_err(|error| {
            tracing::error!(%error, "构造字幕响应失败");
            ErrorResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_build_failed",
                "构造响应失败",
            )
        })
}

/// 让 `header` 导入有归属（实现时用于设 Content-Type）。
#[allow(dead_code)]
fn _content_type(name: &str) -> Option<header::HeaderValue> {
    header::HeaderValue::from_str(name).ok()
}
