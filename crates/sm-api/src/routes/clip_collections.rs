//! `clip-collections` 的 9 个端点，对应上游
//! `src/api/routers/collections/clip_collections.py`。
//!
//! # 每个读端点都会**回收失效片段**
//!
//! `clip_count` 与封面都只算产物仍然存在的成员，而算的过程顺带把失效的行与
//! 文件删掉（见 `sm_service::collections::ordered` 的 `valid_members`）。这是
//! 上游行为，不是本项目的选择。
//!
//! # 封面逐个解析，不批量
//!
//! 上游 `_to_resource` 对每个合集调一次 `_collection_cover`，后者内部再调
//! `load_cover_map([单个 clip])`。这里照搬。改成一次批量需要先知道每个合集的
//! 首成员 —— 那要多查一遍成员，而列表本身已经查过；合集数量是用户级的小数字
//! （个位数到几十），收益不抵复杂度。
//!
//! # `runtime()` 与片段端点同一套
//!
//! 签名密钥与片段根目录都从配置读，且 `PATCH /config` 能在运行期改，所以每次
//! 请求都重读。**必须用 `snapshot()` 而不是 `get()`** —— 后者返回剔除只读键
//! 的公开快照，而签名密钥就在 `auth` 段里。

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use sm_db::catalog::asset::Image;
use sm_service::collections::ordered::{CollectionUpdate, CollectionWithCount, MemberWithClip};
use sm_service::collections::ClipCollectionService;
use sm_service::playback::media_clip::MediaClipService;

use crate::auth::CurrentUser;
use crate::dto::{
    sign_image_origin, ClipCollectionClipItemResource, ClipCollectionCreateRequest,
    ClipCollectionResource, ClipCollectionSetClipsRequest, ClipCollectionUpdateRequest,
    ImageResource, MediaClipResource,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/clip-collections",
            get(list_clip_collections)
                .post(create_clip_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/clip-collections/{collection_id}",
            get(get_clip_collection)
                .patch(update_clip_collection)
                .delete(delete_clip_collection)
                .fallback(method_not_allowed),
        )
        .route(
            // 同一个路径挂 `GET`（分页列成员）与 `PUT`（整体设置成员）。
            "/clip-collections/{collection_id}/clips",
            get(list_clip_collection_clips)
                .put(set_clip_collection_clips)
                .fallback(method_not_allowed),
        )
        .route(
            "/clip-collections/{collection_id}/clips/{clip_id}",
            axum::routing::put(add_clip_to_collection)
                .delete(remove_clip_from_collection)
                .fallback(method_not_allowed),
        )
}

#[derive(Debug, Deserialize)]
struct CollectionPath {
    collection_id: i32,
}

#[derive(Debug, Deserialize)]
struct MemberPath {
    collection_id: i32,
    clip_id: i32,
}

/// `GET /{collection_id}/clips` 的查询参数。缺省与上游 `Query(default=...)` 一致。
#[derive(Debug, Deserialize)]
struct ListClipsQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
}

fn one() -> i64 {
    1
}

fn twenty() -> i64 {
    20
}

/// 每请求解析出的运行时依赖。
struct Runtime {
    secret: String,
    clip_root: PathBuf,
}

/// 每请求解析出的运行时依赖。
///
/// # 配置读不了就让请求失败
///
/// 原来是 `state.config().snapshot().unwrap_or_default()`。那会把「配置文件
/// 非法」吞成全默认配置，于是 `media_clip_root_path` 变空串、`clip_root`
/// 退化成 `PathBuf::from(".")`，产物路径相对**进程工作目录**解析 ——
/// `has_valid_artifact` 恒为 false，`clip_count` 恒为 0，而且不报任何错。
///
/// 这就是 `clip_collections_http.rs` 那 6 个测试的成因。它们不是测试的问题，
/// 是这里静默降级被测出来了。
fn runtime(state: &AppState) -> Result<Runtime, ErrorResponse> {
    let config = crate::config::snapshot_or_500(state)?;
    let secret = crate::config::string_at(&config, "auth", "file_signature_secret")
        .unwrap_or_default()
        .to_owned();
    let root = crate::config::string_at(&config, "media", "media_clip_root_path")
        .unwrap_or_default()
        .to_owned();
    Ok(Runtime {
        secret,
        clip_root: PathBuf::from(if root.is_empty() {
            ".".to_owned()
        } else {
            root
        }),
    })
}

/// 两个 service：`ClipCollectionService` 管合集与成员，
/// `MediaClipService` 管产物有效性判定与封面解析。
fn services(state: &AppState, rt: &Runtime) -> (ClipCollectionService, MediaClipService) {
    (
        ClipCollectionService::new(state.db()),
        MediaClipService::new(state.db(), rt.clip_root.clone()),
    )
}

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

fn timestamp(value: Option<chrono::NaiveDateTime>) -> String {
    value
        .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn clip_resource(
    secret: &str,
    clip: &sm_db::playback::media::MediaClip,
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
        created_at: timestamp(clip.created_at),
    }
}

/// 合集资源组装。
///
/// `clip_count == 0` 时**不解析封面** —— 与上游 `_to_resource` 的
/// `if clip_count else None` 一致，也省掉一次查询。
async fn collection_resource(
    rt: &Runtime,
    collections: &ClipCollectionService,
    media: &MediaClipService,
    row: &CollectionWithCount,
) -> Result<ClipCollectionResource, ErrorResponse> {
    let cover = if row.clip_count > 0 {
        collections.cover(media, row.collection.id).await?
    } else {
        None
    };
    Ok(ClipCollectionResource {
        id: row.collection.id,
        name: row.collection.name.clone(),
        description: row.collection.description.clone(),
        clip_count: row.clip_count,
        cover_image: optional_cover(&rt.secret, cover.as_ref(), now_seconds()),
        created_at: timestamp(row.collection.created_at),
        updated_at: timestamp(row.collection.updated_at),
    })
}

async fn list_clip_collections(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ClipCollectionResource>>, ErrorResponse> {
    let rt = runtime(&state)?;
    let (collections, media) = services(&state, &rt);
    let rows = collections.list_collections(&media).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(collection_resource(&rt, &collections, &media, row).await?);
    }
    Ok(Json(out))
}

async fn get_clip_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<CollectionPath>,
) -> Result<Json<ClipCollectionResource>, ErrorResponse> {
    let rt = runtime(&state)?;
    let (collections, media) = services(&state, &rt);
    let row = collections
        .get_with_count(&media, path.collection_id)
        .await?;
    Ok(Json(
        collection_resource(&rt, &collections, &media, &row).await?,
    ))
}

async fn create_clip_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<ClipCollectionCreateRequest>,
) -> Result<(StatusCode, Json<ClipCollectionResource>), ErrorResponse> {
    let rt = runtime(&state)?;
    let (collections, _media) = services(&state, &rt);
    // 宏的 `create` 返回 `ClipCollection` 本身（不是带计数的结构体）——
    // 而新建合集成员数必为 0，所以不需要再查一次。
    let created = collections
        .create(&payload.name, Some(&payload.description))
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(ClipCollectionResource {
            id: created.id,
            name: created.name,
            description: created.description,
            clip_count: 0,
            cover_image: None,
            created_at: timestamp(created.created_at),
            updated_at: timestamp(created.updated_at),
        }),
    ))
}

async fn update_clip_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<CollectionPath>,
    EnvelopeJson(payload): EnvelopeJson<ClipCollectionUpdateRequest>,
) -> Result<Json<ClipCollectionResource>, ErrorResponse> {
    let rt = runtime(&state)?;
    let (collections, media) = services(&state, &rt);
    // `CollectionUpdate` 按值传 —— 宏生成的签名如此。宏的 `update` 返回
    // `ClipCollection` 本身，所以随后再取一次带计数的行。
    collections
        .update(
            path.collection_id,
            CollectionUpdate {
                name: payload.name,
                description: payload.description,
            },
        )
        .await?;
    let row = collections
        .get_with_count(&media, path.collection_id)
        .await?;
    Ok(Json(
        collection_resource(&rt, &collections, &media, &row).await?,
    ))
}

/// `DELETE /{id}` —— 204。成员行由外键 CASCADE 清掉，片段本体不动。
async fn delete_clip_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<CollectionPath>,
) -> Result<StatusCode, ErrorResponse> {
    let (collections, _) = services(&state, &runtime(&state)?);
    collections.delete(path.collection_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_clip_collection_clips(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<CollectionPath>,
    EnvelopeQuery(query): EnvelopeQuery<ListClipsQuery>,
) -> Result<Json<sm_core::pagination::Paginated<ClipCollectionClipItemResource>>, ErrorResponse> {
    let rt = runtime(&state)?;
    let (collections, media) = services(&state, &rt);
    // 先确认合集存在 —— 上游 `_require_collection` 在 `validate_page` **之前**，
    // 所以「合集不存在」不能被报成分页错误。
    collections.require(path.collection_id).await?;

    let (members, total) = collections
        .list_clips_paged(&media, path.collection_id, query.page, query.page_size)
        .await?;
    let now = now_seconds();
    // 一次批量解析本页全部封面 —— 与片段列表端点同一套。
    let covers = media
        .load_cover_map(
            &members
                .iter()
                .map(|member: &MemberWithClip| member.clip.clone())
                .collect::<Vec<_>>(),
        )
        .await?;

    let items = members
        .iter()
        .map(|member: &MemberWithClip| {
            let cover = member
                .clip
                .media_id
                .and_then(|id| covers.get(&(id, member.clip.start_offset_seconds)));
            ClipCollectionClipItemResource {
                base: clip_resource(&rt.secret, &member.clip, cover, now),
                position: member.item.position,
            }
        })
        .collect();

    Ok(Json(sm_core::pagination::Paginated::new(
        items,
        query.page,
        query.page_size,
        total,
    )))
}

/// `PUT /{id}/clips/{clip_id}` —— 204。**幂等**：已在合集里也成功。
async fn add_clip_to_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<MemberPath>,
) -> Result<StatusCode, ErrorResponse> {
    let (collections, _) = services(&state, &runtime(&state)?);
    collections.add(path.collection_id, path.clip_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /{id}/clips/{clip_id}` —— 204。不在合集里也成功（幂等）。
async fn remove_clip_from_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<MemberPath>,
) -> Result<StatusCode, ErrorResponse> {
    let (collections, _) = services(&state, &runtime(&state)?);
    collections.remove(path.collection_id, path.clip_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /{id}/clips` —— 204。幂等地把成员设为给定有序列表。
///
/// 与 `PUT /{id}/clips/{clip_id}` 是**不同**路径，所以两个 PUT 不冲突。
/// 重复 id 去重，以首次出现的位置为准。
async fn set_clip_collection_clips(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<CollectionPath>,
    EnvelopeJson(payload): EnvelopeJson<ClipCollectionSetClipsRequest>,
) -> Result<StatusCode, ErrorResponse> {
    let (collections, _) = services(&state, &runtime(&state)?);
    collections
        .set_members(path.collection_id, &payload.clip_ids)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
