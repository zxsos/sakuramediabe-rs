//! `media-clips` 的 7 个端点，对应上游
//! `src/api/routers/playback/media_clips.py`。
//!
//! # 第 8 个端点没做，且是有意的
//!
//! `POST /media/{id}/clips`（圈选缩略图切出片段）需要真正切视频 —— 那是
//! ffmpeg，属于 `svc-probe` 那条线。
//!
//! 它的表现是 **405** 而不是 404：`/media/{id}/clips` 这个路径是存在的
//! （`GET` 能匹配），只是这个方法没注册。404 会让客户端以为路径写错了，而
//! 真实原因是「功能还没做」。同样也不注册一个必然 500 的占位 handler ——
//! 那比 405 更坏，客户端会以为功能存在。
//!
//! # 每个 handler 显式取 `CurrentUser`
//!
//! 与本 crate 其余路由一致，不靠 router 层挂鉴权 —— 层会让「这个端点忘了保护」
//! 变得不可见。
//!
//! # 两处配置在每个请求里读
//!
//! 签名密钥与片段产物根目录都来自配置，且 `PATCH /config` 能在运行期改它们。
//! 这里每次请求都重读（配置读是内存快照，很便宜），这样改配置后无需重启即可
//! 生效 —— 与 `routes/config.rs` 里 `restart_required` 的语义一致：配置改动本身
//! 立即可见，只有「读配置的那个进程」需要重启。
//!
//! # 列表端点有写副作用
//!
//! `GET /media-clips` 与 `GET /media/{id}/clips` 会**回收产物失效的片段**
//! （删库行 + 删文件）。这是上游行为，理由见
//! `sm_service::playback::media_clip` 的模块文档。

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use sm_db::catalog::asset::Image;
use sm_db::playback::media::MediaClip;
use sm_service::playback::media_clip::{ClipDetail, ClipListParams, ClipPage, MediaClipService};

use crate::auth::CurrentUser;
use crate::dto::{
    sign_image_origin, ClipCollectionSummary, ImageResource, MediaClipDetailResource,
    MediaClipResource, MediaClipThumbnailResource, MediaClipUpdateRequest,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

/// 本文件所有端点的路由。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/media-clips",
            get(list_media_clips).fallback(method_not_allowed),
        )
        .route(
            "/media-clips/{clip_id}",
            get(get_clip)
                .patch(update_clip)
                .delete(delete_clip)
                .fallback(method_not_allowed),
        )
        .route(
            "/media-clips/{clip_id}/thumbnails",
            get(list_clip_thumbnails).fallback(method_not_allowed),
        )
        .route(
            "/media-clips/{clip_id}/stream",
            get(stream_clip).fallback(method_not_allowed),
        )
        .route(
            "/media/{media_id}/clips",
            get(list_clips_for_media).fallback(method_not_allowed),
        )
}

/// 路径参数。
#[derive(Debug, Deserialize)]
struct ClipPath {
    clip_id: i32,
}

/// `GET /media-clips` 的查询参数。
///
/// 缺省值逐条对应上游的 `Query(default=...)`；`exclude_collection_id` 的
/// `ge=1` 约束在 service 之外做（见 [`validate_exclude_collection_id`]）。
#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
    sort: Option<String>,
    movie_number: Option<String>,
    keyword: Option<String>,
    exclude_collection_id: Option<i32>,
}

fn one() -> i64 {
    1
}

fn twenty() -> i64 {
    20
}

/// `exclude_collection_id` 的 `ge=1`。
///
/// 上游用 FastAPI 的 `Query(ge=1)`，所以 0 与负数在**进 service 之前**就是
/// 422，且错误细节由 FastAPI 生成（`detail` 数组）。这里手写同样形状的拒绝，
/// 因为 `ge` 属于校验而非业务规则，而把它放进 service 会让它排到关键词校验
/// 之后 —— 那样「两个都错」时错误码/details 就与上游不同了。
fn validate_exclude_collection_id(value: Option<i32>) -> Result<(), ErrorResponse> {
    match value {
        Some(id) if id < 1 => {
            let mut details = serde_json::Map::new();
            details.insert("exclude_collection_id".to_owned(), id.into());
            Err(ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                "Request validation failed",
            )
            .with_details(details))
        }
        _ => Ok(()),
    }
}

/// 每请求解析出的运行时依赖：签名密钥与产物根目录。
struct Runtime {
    secret: String,
    clip_root: PathBuf,
}

/// 从配置里取签名密钥与片段根目录。
///
/// # 必须用 `snapshot()` 而不是 `get()`
///
/// `get()` 返回的是**公开**快照，而公开快照的定义就是「剔除只读键」，
/// `auth` 整段被剔除 —— `file_signature_secret` 正在里面。用 `get()` 的
/// 后果是密钥恒为空串，于是**每一个**签名 URL 都验不过，而症状是
/// 「列表里能拿到 stream_url、点开却 403」—— 看起来像签名算法坏了，
/// 实际是取错了访问器。
///
/// 取不到时**不报错**而是给空串 / 当前目录：那会让签名校验必然失败（403），
/// 而不是让整个列表端点 500。配置缺失是部署问题，不该让读接口全部不可用。
fn runtime(state: &AppState) -> Runtime {
    let config = state.config().snapshot().unwrap_or_default();
    let secret = config
        .get("auth")
        .and_then(|auth| auth.get("file_signature_secret"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let root = config
        .get("media")
        .and_then(|media| media.get("media_clip_root_path"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Runtime {
        secret,
        // 上游 `expanduser()` + 相对路径转 cwd + `resolve()`。这里只做绝对化：
        // 片段根目录在生产上是绝对路径，而 `~` 展开需要引入额外的 home 查找。
        clip_root: PathBuf::from(if root.is_empty() {
            ".".to_owned()
        } else {
            root
        }),
    }
}

fn service(state: &AppState, runtime: &Runtime) -> MediaClipService {
    MediaClipService::new(state.db(), runtime.clip_root.clone())
}

/// 当前秒。签名 URL 的过期时间以它为基准。
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn image_resource(secret: &str, image: &Image, now: i64) -> ImageResource {
    ImageResource {
        id: image.id,
        origin: sign_image_origin(secret, &image.origin, now),
    }
}

fn optional_cover(secret: &str, cover: Option<&Image>, now: i64) -> Option<ImageResource> {
    cover.map(|image| image_resource(secret, image, now))
}

/// 构造列表项。`stream_url` 内联签名，与上游 `clip_resource_fields` 一致。
fn clip_resource(
    secret: &str,
    clip: &MediaClip,
    cover: Option<&Image>,
    now: i64,
) -> MediaClipResource {
    MediaClipResource {
        clip_id: clip.id,
        media_id: clip.media_id,
        movie_number: clip.movie_number.clone(),
        start_offset_seconds: clip.start_offset_seconds,
        end_offset_seconds: clip.end_offset_seconds,
        title: clip.title.clone(),
        duration_seconds: clip.duration_seconds,
        file_size_bytes: clip.file_size_bytes,
        cover_image: optional_cover(secret, cover, now),
        stream_url: sm_core::signing::build_signed_clip_url(secret, clip.id, now),
        created_at: clip
            .created_at
            .map(|value| value.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default(),
    }
}

async fn list_media_clips(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListQuery>,
) -> Result<Json<sm_core::pagination::Paginated<MediaClipResource>>, ErrorResponse> {
    validate_exclude_collection_id(query.exclude_collection_id)?;
    let runtime = runtime(&state);
    let page = service(&state, &runtime)
        .list(&ClipListParams {
            page: query.page,
            page_size: query.page_size,
            sort: query.sort,
            movie_number: query.movie_number,
            keyword: query.keyword,
            exclude_collection_id: query.exclude_collection_id,
        })
        .await?;
    Ok(Json(into_page(
        &runtime,
        &page,
        query.page,
        query.page_size,
    )))
}

/// 把 `ClipPage` 摊成分页响应，并把回收事实记进日志。
///
/// 回收是**写副作用**，而这里没有任何响应字段能表达它。不记日志的话，
/// 「客户端发现某个片段不见了」这件事就彻底无迹可寻。
fn into_page(
    runtime: &Runtime,
    page: &ClipPage,
    page_number: i64,
    page_size: i64,
) -> sm_core::pagination::Paginated<MediaClipResource> {
    if !page.reclaimed.is_empty() {
        tracing::warn!(
            reclaimed = page.reclaimed.len(),
            ids = ?page.reclaimed,
            "片段列表回收了产物失效的片段"
        );
    }
    let now = now_seconds();
    let items = page
        .clips
        .iter()
        .map(|clip| {
            let cover = clip
                .media_id
                .and_then(|id| page.covers.get(&(id, clip.start_offset_seconds)));
            clip_resource(&runtime.secret, clip, cover, now)
        })
        .collect();
    sm_core::pagination::Paginated::new(items, page_number, page_size, page.total)
}

async fn list_clips_for_media(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(media_id): Path<i32>,
) -> Result<Json<Vec<MediaClipResource>>, ErrorResponse> {
    let runtime = runtime(&state);
    let page = service(&state, &runtime).list_for_media(media_id).await?;
    let now = now_seconds();
    Ok(Json(
        page.clips
            .iter()
            .map(|clip| {
                let cover = clip
                    .media_id
                    .and_then(|id| page.covers.get(&(id, clip.start_offset_seconds)));
                clip_resource(&runtime.secret, clip, cover, now)
            })
            .collect(),
    ))
}

async fn get_clip(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ClipPath>,
) -> Result<Json<MediaClipDetailResource>, ErrorResponse> {
    let runtime = runtime(&state);
    let detail = service(&state, &runtime).detail(path.clip_id).await?;
    Ok(Json(detail_resource(&runtime, &detail)))
}

fn detail_resource(runtime: &Runtime, detail: &ClipDetail) -> MediaClipDetailResource {
    let now = now_seconds();
    let base = clip_resource(&runtime.secret, &detail.clip, detail.cover.as_ref(), now);
    MediaClipDetailResource {
        base,
        preview_frames: detail
            .preview_frames
            .iter()
            .map(|image| image_resource(&runtime.secret, image, now))
            .collect(),
        collections: detail
            .collections
            .iter()
            .map(|c| ClipCollectionSummary {
                id: c.id,
                name: c.name.clone(),
            })
            .collect(),
    }
}

/// `GET /media-clips/{id}/thumbnails`。
///
/// `offset_seconds` 已由 service 重基到片段自身时间轴，这里只补图。
async fn list_clip_thumbnails(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ClipPath>,
) -> Result<Json<Vec<MediaClipThumbnailResource>>, ErrorResponse> {
    let runtime = runtime(&state);
    let detail = service(&state, &runtime).detail(path.clip_id).await?;

    // 详情已把图片批量取回；缩略图条目按 `image_id` 对回去，避免第二次查库。
    let by_id: std::collections::HashMap<i32, &Image> = detail
        .preview_frames
        .iter()
        .map(|image| (image.id, image))
        .collect();

    let now = now_seconds();
    let items = detail
        .thumbnails
        .iter()
        .filter_map(|thumbnail| {
            by_id
                .get(&thumbnail.image_id)
                .map(|image| MediaClipThumbnailResource {
                    clip_id: detail.clip.id,
                    thumbnail_id: thumbnail.thumbnail_id,
                    offset_seconds: thumbnail.offset_seconds,
                    image: image_resource(&runtime.secret, image, now),
                })
        })
        .collect();
    Ok(Json(items))
}

async fn update_clip(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ClipPath>,
    EnvelopeJson(payload): EnvelopeJson<MediaClipUpdateRequest>,
) -> Result<Json<MediaClipResource>, ErrorResponse> {
    let runtime = runtime(&state);
    let (clip, cover) = service(&state, &runtime)
        .update_title(path.clip_id, &payload.title)
        .await?;
    Ok(Json(clip_resource(
        &runtime.secret,
        &clip,
        cover.as_ref(),
        now_seconds(),
    )))
}

/// `DELETE /media-clips/{id}` —— 204，无响应体。
async fn delete_clip(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<ClipPath>,
) -> Result<StatusCode, ErrorResponse> {
    let runtime = runtime(&state);
    service(&state, &runtime).delete(path.clip_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /media-clips/{id}/stream` —— 带签名的片段串流。
///
/// # 这个端点**不取 `CurrentUser`**
///
/// 它是本文件里唯一不走鉴权中间件的端点，而且是有意的：签名**就是**它的
/// 授权凭证。播放器在 `<video src>` 里拿到的就是这个 URL，浏览器不会附带
/// Authorization 头 —— 若强制登录，视频根本播不了。
///
/// 代价是签名成了唯一的访问控制，所以下面三步一步都不能少。
///
/// # 三步校验的顺序与上游逐条一致
///
/// 1. `require_signed_params`：`expires` 缺失或 `signature` 为空 -> 403
///    `file_signature_invalid`；
/// 2. `verify_clip_signature`：过期 -> 403 `file_signature_expired`，不匹配 ->
///    403 `file_signature_invalid`；
/// 3. `require_existing_file`：文件不在 -> 404 `file_not_found`。
///
/// **先验签再查库**是安全相关的：未通过签名之前不碰数据库，也不碰文件系统。
///
/// # `Range` 支持
///
/// 播放器要靠它拖动进度条，见 [`crate::range`]。
#[derive(Debug, Deserialize)]
struct StreamQuery {
    expires: Option<i64>,
    signature: Option<String>,
}

async fn stream_clip(
    State(state): State<AppState>,
    Path(path): Path<ClipPath>,
    // `HeaderMap` 是 `FromRequestParts`，axum 要求它排在消费 body 的
    // `FromRequest` 提取器**之前** —— 顺序反了就没有 `Handler` 实现。
    headers: axum::http::HeaderMap,
    EnvelopeQuery(query): EnvelopeQuery<StreamQuery>,
) -> Result<axum::response::Response, ErrorResponse> {
    // 1) 参数齐备性。上游 `require_signed_params` 的条件是
    //    `expires is None or not signature` —— 注意 `not signature` 对空串
    //    也成立，所以 `?signature=` 与不传是同一个错。
    let (Some(expires), Some(signature)) = (query.expires, query.signature) else {
        return Err(signature_invalid());
    };
    if signature.is_empty() {
        return Err(signature_invalid());
    }

    let runtime = runtime(&state);

    // 2) 验签。过期与不匹配是两个不同��错误码，客户端据此决定是「刷新 URL」
    //    还是「URL 被篡改」。
    sm_core::signing::verify_clip(
        &runtime.secret,
        path.clip_id,
        expires,
        &signature,
        now_seconds(),
    )
    .map_err(|err| {
        // `STATUS` 是关联常量（恒为 403），不是字段。三个变体都是 403 ——
        // 区别只在 `code`，客户端靠它区分「刷新 URL」与「URL 被篡改」。
        ErrorResponse::new(
            StatusCode::from_u16(sm_core::SignatureError::STATUS).unwrap_or(StatusCode::FORBIDDEN),
            err.code(),
            err.message(),
        )
    })?;

    // 3) 解析产物路径 + 确认文件在。路径解析失败（脏数据、被穿越尝试）与
    //    文件不存在都是 404 `file_not_found` —— 与上游
    //    `_require_clip` / `require_existing_file` 的最终效果一致。
    let clip = service(&state, &runtime)
        .require_clip_for_stream(path.clip_id)
        .await?;
    let Some(absolute) = clip else {
        return Err(file_not_found());
    };

    // 4) 按 Range 返回。IO 错误在这里才可能发生（文件在验签后被删）。
    let range_header = headers
        .get(axum::http::header::RANGE)
        .and_then(|value| value.to_str().ok());
    crate::range::serve_file(&absolute, range_header).map_err(|err| {
        tracing::warn!(clip_id = path.clip_id, error = %err, "片段串流读取失败");
        file_not_found()
    })
}

/// 403 `file_signature_invalid`。
fn signature_invalid() -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::FORBIDDEN,
        "file_signature_invalid",
        "文件签名无效",
    )
}

/// 404 `file_not_found`。
fn file_not_found() -> ErrorResponse {
    ErrorResponse::new(StatusCode::NOT_FOUND, "file_not_found", "文件不存在")
}
