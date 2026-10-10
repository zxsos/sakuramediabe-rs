//! `/video-collections*` —— 视频集合九个端点。
//!
//! # 与上游 `src/api/routers/videos/collections.py` 的对应
//!
//! prefix **`/video-collections`** —— 注意**不含 `videos/` 这一段**，
//! 别照目录结构拼路径。
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `GET ""` | 200（裸列表）|
//! | `POST ""` | **201** |
//! | `GET /{id}` | 200 |
//! | `PATCH /{id}` | 200 |
//! | `DELETE /{id}` | **204** |
//! | `GET /{id}/items` | 200（`PageResponse`）|
//! | `POST /{id}/items` | **204** |
//! | `DELETE /{id}/items/{item_id}` | **204** |
//! | `POST /{id}/items/reorder` | **200 + 列表** |
//!
//! # 三处与 `moment_collections` 不同，别照抄错
//!
//! | | 本文件 | `routes/moment_collections.rs` |
//! |---|---|---|
//! | 添加成员 | **`POST`** `/items` → 204 | **`PUT`** `/points/{point_id}` → 204 |
//! | 整体重排 | **有** `POST /items/reorder` → 200 + 列表 | **无** |
//! | 成员 id | `item_id`（视频条目）| `point_id`（媒体点）|
//!
//! # `reorder` 是**唯一返回数据的写操作**
//!
//! 其余三个成员操作都返回 204 无 body，只有 `reorder` 返回 **200 + 完整新
//! 列表**。理由：重排是**整体替换语义** —— 客户端要确认服务端理解的顺序与
//! 自己的一致，所以回填权威顺序。**reorder 后不要再让客户端回查。**
//!
//! # `include_play_url` 默认 `false`
//!
//! 生成播放 URL 要走签名流程（见 `routes/media_playback.rs`），逐条生成不
//! 便宜。别「优化」成总是返回。
//!
//! # 骨架期写错、本轮对照上游更正的四处
//!
//! 读的是 `schema/videos/collections.py` 与
//! `service/videos/video_collection_service.py`：
//!
//! | 项 | 骨架期 | 上游实际 |
//! |---|---|---|
//! | 合集资源字段 | 只有 `id` / `name` / `item_count` | 还有 `description` / `cover_image` / `created_at` / `updated_at` |
//! | 创建请求 | 只有 `name` | 还有 `description` |
//! | 添加条目请求字段 | `item_id` | **`video_item_id`** |
//! | 重排请求字段 | `item_ids` | **`ordered_item_ids`**（且 `min_length=1`）|
//!
//! 还有一处**文档写反**的：骨架写「添加条目已存在 → 409」，而上游
//! `add_item` 查到已有成员就 `return` —— **幂等 204**（见下面 handler）。
//!
//! # 八个端点都已接线
//!
//! 5 个合集级 + 3 个成员操作全部接通。成员端点组装
//! `VideoCollectionItemResource`，其 `video` 字段是完整的
//! `VideoItemListItemResource`（14 字段），与 `GET /videos` 共用同一套组装。
//!
//! `include_play_url=true` 时 `play_url` 由**媒体库能力缝**提供：服务层给出
//! `provider_key → playback_deliveries`（见 `MediaLibraryService::playback_deliveries`），
//! 这里取 `[0]` 交给 [`signed_play_url`] 签地址。两处细节：
//!
//! - provider **没装**时（`play_provider_key` 查不到）与上游一致地把
//!   `video.can_play` **打成 `false`**，不是只丢 `play_url`（上游 `:263-264`）。
//! - `play_url` 无值发 **`null`**，**绝不用空串** —— 空串在客户端是「有媒体但
//!   播不了」，会让整条播放列表被判成不可播放。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_service::videos::{Field, VideoCollectionService, VideoCollectionUpdate};

use crate::auth::CurrentUser;
use crate::dto::{
    deserialize_double_option, sign_image_origin, ImageResource, VideoItemListItemResource,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::{one, twenty};
use crate::routes::method_not_allowed;
use crate::signing::signed_play_url;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/video-collections",
            get(list_collections)
                .post(create_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}",
            get(get_collection)
                .patch(update_collection)
                .delete(delete_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}/items",
            get(list_collection_items)
                .post(add_collection_item)
                .fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}/items/reorder",
            post(reorder_collection_items).fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}/items/{item_id}",
            delete(remove_collection_item).fallback(method_not_allowed),
        )
}

/// 视频集合（上游 `VideoCollectionResource`）。
#[derive(Debug, Clone, Serialize)]
pub struct VideoCollectionResource {
    pub id: i32,
    pub name: String,
    pub description: String,
    pub item_count: i32,
    /// 首个成员的条目封面。**空合集为 `None`**。
    pub cover_image: Option<ImageResource>,
    pub created_at: String,
    pub updated_at: String,
}

/// 集合条目（上游 `VideoCollectionItemResource`）。
///
/// ⚠️ 骨架期写成 `{ item_id, title, play_url, duration_seconds }` —— 上游是
/// `{ item_id, position, video, play_url, first_media_id }`，`video` 内嵌一个
/// **完整的 14 字段列表项**（与 `GET /videos` 同一个资源）。已按上游重写。
#[derive(Debug, Clone, Serialize)]
pub struct VideoCollectionItemResource {
    /// **关联行 id**（`video_collection_item.id`），不是视频条目 id ——
    /// 「移除成员」端点收的也是它。两者外观相同，见 `sm_service::videos`。
    pub item_id: i32,
    pub position: i32,
    pub video: VideoItemListItemResource,
    /// 「首个有效媒体」的签名播放地址。
    ///
    /// **仅在 `include_play_url=true` 且成员有有效媒体时才有值** —— 由
    /// [`signed_play_url`] 用 provider 的 `playback_deliveries[0]` 签出。
    ///
    /// **不要用空串冒充** —— 空串在客户端是「这个媒体有，但播不了」，
    /// 会让整条播放列表被判成不可播放。`None` 才是「没提供」。
    ///
    /// 与上游一致地**不省略这个键**（上游 `SchemaModel` 不过滤 `None`，
    /// 序列化出来是 `null`）。骨架期那个 `skip_serializing_if` 会让键消失。
    pub play_url: Option<String>,
    /// 「首个有效媒体」的 id，**恒返回**（不依赖 `include_play_url`）。
    /// 成员没有有效媒体时为 `None`。连播页关键帧面板用它。
    pub first_media_id: Option<i32>,
}

/// 创建请求（上游 `VideoCollectionCreateRequest`）。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    /// 缺省空串。上游 `description: str = ""`。
    #[serde(default)]
    pub description: String,
}

/// 更新请求（上游 `VideoCollectionUpdateRequest`）。
///
/// # 为什么是 `Option<Option<T>>`
///
/// 上游用 `model_dump(exclude_unset=True)`，要区分**三**种状态：
/// 缺键 / 显式 `null` / 有值。而 `update_collection` 的判据是
/// `if "name" in update_data and update_data["name"] is not None` ——
/// 所以 `{"name": null}` 在上游是**非空更新**（过得了空更新检查），
/// 但**不改任何字段**，只推进 `updated_at`，返回 **200**。
///
/// 用 `Option<String>` 会把「缺键」与「null」压成同一件事，于是
/// `{"name": null}` 被当成空更新报 422 —— 那是**改变契约**。
/// 外层 `None` = 缺键，`Some(None)` = 显式 null，`Some(Some(v))` = 有值。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateRequest {
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub description: Option<Option<String>>,
}

/// 三态搬运：`Option<Option<T>>` → [`Field<T>`]。
fn to_field(value: Option<Option<String>>) -> Field<String> {
    match value {
        None => Field::Absent,
        Some(None) => Field::Null,
        Some(Some(text)) => Field::Value(text),
    }
}

/// 添加条目请求（上游 `VideoCollectionItemAddRequest`）。
///
/// ⚠️ 骨架期字段名写成 `item_id`，上游是 **`video_item_id`**（且 `gt=0`）。
/// 取值空间相同、外观相同，写错不会报错，只会「加了一个不存在的条目」。
/// 已按上游更正 —— 虽然端点本身还没接线。
#[derive(Debug, Clone, Deserialize)]
pub struct ItemAddRequest {
    pub video_item_id: i32,
}

/// 重排请求（上游 `VideoCollectionReorderRequest`）。
///
/// ⚠️ 骨架期字段名写成 `item_ids`，上游是 **`ordered_item_ids`**，且
/// `min_length=1`（空列表直接 422 —— service 层也拦了一道，见
/// `VideoCollectionService::reorder`）。
///
/// 元素类型是 `i32`：关联行 id 与 `video_item.id` 在库里都是 `integer`，
/// 骨架期的 `Vec<i64>` 会在绑定时才暴露宽度不符。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReorderRequest {
    /// 目标顺序，**元素是关联行 id（`video_collection_item.id`）**。
    pub ordered_item_ids: Vec<i32>,
}

/// 每请求解析出的签名密钥（合集封面要签图片 URL）。
fn secret(state: &AppState) -> Result<String, ErrorResponse> {
    let config = crate::config::snapshot_or_500(state)?;
    Ok(
        crate::config::string_at(&config, "auth", "file_signature_secret")
            .unwrap_or_default()
            .to_owned(),
    )
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn timestamp(value: Option<chrono::NaiveDateTime>) -> String {
    value
        .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn collection_resource(
    secret: &str,
    row: &sm_service::videos::collection::VideoCollectionWithCount,
) -> VideoCollectionResource {
    VideoCollectionResource {
        id: row.collection.id,
        name: row.collection.name.clone(),
        description: row.collection.description.clone(),
        item_count: row.item_count,
        // `item_count == 0` 时**不解析封面** —— 与上游
        // `_collection_cover(...) if item_count else None` 一致。
        cover_image: row
            .cover
            .as_ref()
            .filter(|_| row.item_count > 0)
            .map(|image| ImageResource {
                id: image.id,
                origin: sign_image_origin(secret, &image.origin, now_seconds()),
            }),
        created_at: timestamp(row.collection.created_at),
        updated_at: timestamp(row.collection.updated_at),
    }
}

/// `GET ""`
async fn list_collections(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<VideoCollectionResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = VideoCollectionService::new(state.db());
    let rows = service.list_collections().await?;
    Ok(Json(
        rows.iter()
            .map(|row| collection_resource(&secret, row))
            .collect(),
    ))
}

/// `POST ""` —— **201**。重名 → **409**。
async fn create_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<CreateRequest>,
) -> Result<(StatusCode, Json<VideoCollectionResource>), ErrorResponse> {
    let service = VideoCollectionService::new(state.db());
    let created = service
        .create(&payload.name, Some(&payload.description))
        .await?;
    // 新建合集成员数必为 0 → 没有封面，也就不必解析签名密钥。
    Ok((
        StatusCode::CREATED,
        Json(VideoCollectionResource {
            id: created.id,
            name: created.name,
            description: created.description,
            item_count: 0,
            cover_image: None,
            created_at: timestamp(created.created_at),
            updated_at: timestamp(created.updated_at),
        }),
    ))
}

/// `GET /{id}` —— 404。
async fn get_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
) -> Result<Json<VideoCollectionResource>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = VideoCollectionService::new(state.db());
    let row = service.get_with_count(collection_id).await?;
    Ok(Json(collection_resource(&secret, &row)))
}

/// `PATCH /{id}` —— 部分更新。
async fn update_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeJson(payload): EnvelopeJson<UpdateRequest>,
) -> Result<Json<VideoCollectionResource>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = VideoCollectionService::new(state.db());
    service
        .update(
            collection_id,
            VideoCollectionUpdate {
                name: to_field(payload.name),
                description: to_field(payload.description),
            },
        )
        .await?;
    // 返回带计数的行：上游 `update_collection` 结尾是 `get_collection(...)`。
    let row = service.get_with_count(collection_id).await?;
    Ok(Json(collection_resource(&secret, &row)))
}

/// `DELETE /{id}` —— **204，无 body**。成员行随外键 CASCADE 清掉。
async fn delete_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    let service = VideoCollectionService::new(state.db());
    service.delete(collection_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /{id}/items` —— 分页 + `include_play_url`。
#[derive(Debug, Deserialize)]
pub struct ListItemsQuery {
    /// `field:direction`，白名单见 `sm_service::videos::COLLECTION_ITEM_SORT_KEYS`
    /// （比条目列表多一个 `position`）。缺省 `position:asc`。
    pub sort: Option<String>,
    #[serde(default = "one")]
    pub page: i64,
    #[serde(default = "twenty")]
    pub page_size: i64,
    /// **默认 `false`**。用 pydantic 的宽松布尔口径解析
    /// （`1` / `yes` / `on` 等 6 种真值），否则 `?include_play_url=1`
    /// 在 Rust 侧会 422 而上游是 `True`。
    #[serde(default, deserialize_with = "crate::query::deser_bool")]
    pub include_play_url: bool,
}

/// 一个成员行的资源化。`play_url` 由调用方决定（见两处调用点）。
fn item_resource(
    secret: &str,
    now: i64,
    row: &sm_service::videos::collection::VideoCollectionItemRow,
    play_url: Option<String>,
) -> VideoCollectionItemResource {
    VideoCollectionItemResource {
        item_id: row.item.id,
        position: row.item.position,
        video: VideoItemListItemResource::from_list_item(secret, now, &row.video),
        play_url,
        first_media_id: row.first_media_id,
    }
}

async fn list_collection_items(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeQuery(query): EnvelopeQuery<ListItemsQuery>,
) -> Result<Json<sm_core::pagination::Paginated<VideoCollectionItemResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = VideoCollectionService::new(state.db());
    let (rows, total) = service
        .list_items_paged(
            collection_id,
            query.sort.as_deref(),
            query.page,
            query.page_size,
        )
        .await?;
    let now = now_seconds();
    // `provider_key → 交付顺序`。**没注入注册表时是空表**，行为退化成「全部无
    // play_url」，而不是报错 —— 目录数据是可选增强。
    let deliveries = state.media_library_service().playback_deliveries();
    let items = rows
        .iter()
        .map(|row| {
            let mut resource = item_resource(&secret, now, row, None);
            // 上游 `_query_item_resources`（`:259-268`）：
            // `if include_play_url and link.play_media_id:` 才去查 provider。
            if query.include_play_url {
                if let (Some(media_id), Some(provider_key)) =
                    (row.first_media_id, row.provider_key.as_deref())
                {
                    match deliveries.get(provider_key) {
                        Some(list) => {
                            resource.play_url = signed_play_url(&secret, now, media_id, list);
                        }
                        // `MEDIA_PROVIDER_REGISTRY.require` 抛 `ProviderUnavailableError`：
                        // 上游**顺手把 can_play 打成 false**（`:263-264`），不是只丢 play_url。
                        None => resource.video.can_play = false,
                    }
                }
            }
            resource
        })
        .collect();
    Ok(Json(sm_core::pagination::Paginated::new(
        items,
        query.page,
        query.page_size,
        total,
    )))
}

/// `POST /{id}/items` —— **204，无 body**。**是 `POST` 不是 `PUT`**。
///
/// 已存在 → **仍然 204**（幂等，不是 409）。骨架期这里写的是「已存在 → 409」，
/// 与上游相反：`add_item` 查到已是成员就 `return`。
async fn add_collection_item(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeJson(payload): EnvelopeJson<ItemAddRequest>,
) -> Result<StatusCode, ErrorResponse> {
    let service = VideoCollectionService::new(state.db());
    // `Added` / `AlreadyPresent` 两个分支都返回 204 —— 上游就是这么写的。
    service
        .add_item(collection_id, payload.video_item_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /{id}/items/{item_id}` —— **204，无 body**，**幂等**。
///
/// `item_id` 是**关联行 id**（`video_collection_item.id`），不是视频条目 id。
async fn remove_collection_item(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((collection_id, item_id)): Path<(i32, i32)>,
) -> Result<StatusCode, ErrorResponse> {
    let service = VideoCollectionService::new(state.db());
    service.remove_item(collection_id, item_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /{id}/items/reorder` —— **200 + 完整新列表**（唯一返回数据的写操作）。
///
/// 响应里**不含** `play_url` —— reorder 请求没有 `include_play_url`。
/// 返回的是**重排后的全部成员**（不分页），客户端据此确认权威顺序。
async fn reorder_collection_items(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeJson(payload): EnvelopeJson<ReorderRequest>,
) -> Result<Json<Vec<VideoCollectionItemResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = VideoCollectionService::new(state.db());
    service
        .reorder(collection_id, &payload.ordered_item_ids)
        .await?;
    // 重新读一遍成员 —— `reorder` 返回的是重排前的行（位置是旧值），而上游
    // `reorder_items` 结尾是 `_query_item_resources(collection)`，读的是库。
    let rows = service.list_item_rows(collection_id).await?;
    let now = now_seconds();
    Ok(Json(
        rows.iter()
            .map(|row| item_resource(&secret, now, row, None))
            .collect(),
    ))
}
