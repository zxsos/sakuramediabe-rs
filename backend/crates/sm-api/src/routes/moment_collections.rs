//! `/moment-collections*` —— 瞬时集合九个端点。
//!
//! # 与上游 `src/api/routers/collections/moment_collections.py` 的对应
//!
//! prefix `/moment-collections`。
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `GET ""` | 200（`list`，**不分页**）|
//! | `POST ""` | **201** |
//! | `GET /{id}` | 200 |
//! | `PATCH /{id}` | 200 |
//! | `DELETE /{id}` | **204** |
//! | `GET /{id}/points` | 200（`PageResponse`，**分页**）|
//! | `PUT /{id}/points/{point_id}` | **204** |
//! | `DELETE /{id}/points/{point_id}` | **204** |
//! | `PUT /{id}/points` | **204** |
//!
//! # 四个点位操作**全部 204 且无 body** —— 包括「添加」这个非幂等动作
//!
//! `PUT /{id}/points/{point_id}` 是「添加一个点」，语义上**不是**幂等的
//! （加两次理应报错或加两条），但它返回 **204 无 body** —— 与「设置整个列表」
//! 和「移除」**完全一样**。
//!
//! 所以**不要**给「添加」加一个返回新增成员的 JSON body：客户端拿不到
//! `point_id` 就得回查一次列表。照抄。
//!
//! # 列表**不分页**，点位列表**分页**
//!
//! `GET ""` 返回 `list[MomentCollectionResource]`（裸列表），
//! `GET /{id}/points` 返回 `PageResponse[...]`。
//!
//! 同一资源组里两种分页形态。集合数量天然少（用户手建），点位数量多
//! （一个集合可挂很多片段）—— **这个区分有道理，不要「统一」**。
//!
//! # 与 `routes/playlists.rs` 的对比
//!
//! `playlists`（歌单）与 `moment_collections`（瞬时片段集合）是**两个不同
//! 实体**，形状几乎一样（CRUD + 成员管理），容易混。判别：歌单里放**音频
//! 文件**，瞬时集合放**媒体点**（视频片段），数据库表也不同。
//! **不要**因为形状像就复用同一个路由文件。
//!
//! # 骨架期写错、本轮对照上游更正的五处
//!
//! 接线时逐条读了 `schema/collections/moments.py` 与
//! `service/collections/moment_collection_service.py`，更正如下：
//!
//! | 项 | 骨架期 | 上游实际 |
//! |---|---|---|
//! | 计数字段名 | `item_count` | **`point_count`** |
//! | 合集资源字段 | 少了 `description` / `cover_image` / `updated_at` | 7 个字段全有 |
//! | 创建/更新请求 | 只有 `name` | 还有 `description` |
//! | 点位条目 | `{point_id, media_id, kind, offset_seconds, thumbnail_url}` | = `MediaPointListItemResource` + `position`（**没有 `kind`**，有 `image` / `video_item_id` / `created_at`）|
//! | 重复添加点位 | 文档写「已存在 → **409**」 | **幂等 204**（上游 `add_point` 查到就 `return`）|
//!
//! # 两处**刻意**与上游不同
//!
//! 1. **`{"name": null}` 不报 500。** 上游 pydantic 允许 `name=None`，而
//!    service 立刻 `None.strip()` → `AttributeError` → **500**。Rust 侧
//!    serde 把「缺键」与「显式 null」都映成 `None`，于是当作**不改动**。
//!    复刻一个 500 没有价值，但这是一个**可见差异**，写在这里。
//! 2. **`{"description": null}` 不清空。** 上游 `_normalize_description(None)`
//!    返回 `""`（= 清空）；Rust 侧同样区分不出「缺键」与「显式 null」，所以
//!    与上一条一起被当作「不改动」。清空描述要用 `{"description": ""}`。
//!
//! 两条都源自同一个原因：`CollectionUpdate` 用的是 `Option` 而不是
//! `videos` 域那个三态的 `Field<T>`。改它要动 clip 侧共用的宏，不在本轮。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_service::collections::ordered::MomentPointWithImage;
use sm_service::collections::{CollectionUpdate, MomentCollectionService};

use crate::auth::CurrentUser;
use crate::dto::{sign_image_origin, ImageResource};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::{one, twenty};
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/moment-collections",
            get(list_moment_collections)
                .post(create_moment_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/moment-collections/{collection_id}",
            get(get_moment_collection)
                .patch(update_moment_collection)
                .delete(delete_moment_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/moment-collections/{collection_id}/points",
            get(list_moment_collection_points)
                .put(set_moment_collection_points)
                .fallback(method_not_allowed),
        )
        .route(
            "/moment-collections/{collection_id}/points/{point_id}",
            put(add_point_to_moment_collection)
                .delete(remove_point_from_moment_collection)
                .fallback(method_not_allowed),
        )
}

/// 瞬时集合（上游 `MomentCollectionResource`）。
#[derive(Debug, Clone, Serialize)]
pub struct MomentCollectionResource {
    pub id: i32,
    pub name: String,
    pub description: String,
    /// 成员数量。**冗余字段** —— 列表接口靠它免掉 N+1 查询。
    ///
    /// 名字是 `point_count`（不是 `item_count`）：时刻点的成员数，与
    /// clip 侧的 `clip_count` 是两套命名。
    pub point_count: i32,
    /// 首个成员的点位图。**空合集为 `None`** —— 上游连 `point_count == 0`
    /// 时都直接给 `None`，即使有残留的封面数据。
    pub cover_image: Option<ImageResource>,
    pub created_at: String,
    pub updated_at: String,
}

/// 创建请求（上游 `MomentCollectionCreateRequest`）。
#[derive(Debug, Clone, Deserialize)]
pub struct MomentCollectionCreateRequest {
    pub name: String,
    /// 缺省空串。上游 `description: str = ""`。
    #[serde(default)]
    pub description: String,
}

/// 更新请求 —— 部分更新，字段全 `Option`（见模块文档的两处刻意差异）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MomentCollectionUpdateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// 设置整个点位列表的请求（上游 `MomentCollectionSetPointsRequest`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MomentCollectionSetPointsRequest {
    /// 目标点位 id 列表，**顺序即播放顺序**。
    pub point_ids: Vec<i32>,
}

/// 集合内的一个点位（上游 `MomentCollectionPointItemResource`）。
///
/// = `MediaPointListItemResource` + `position`。**没有 `kind`** ——
/// 骨架期那个字段是凭印象加的，上游不存在。
#[derive(Debug, Clone, Serialize)]
pub struct MomentCollectionPointItemResource {
    pub point_id: i32,
    /// 来源媒体。来源被删后置空（`ON DELETE SET NULL`）。
    pub media_id: Option<i32>,
    /// 非 JAV 媒体没有番号，此时为 `None` 并靠 `video_item_id` 区分归属。
    pub movie_number: Option<String>,
    pub video_item_id: Option<i32>,
    pub thumbnail_id: Option<i32>,
    pub offset_seconds: i32,
    pub image: ImageResource,
    pub created_at: String,
    /// 该成员在集合里的顺序。
    pub position: i32,
}

/// `GET /{collection_id}/points` 的查询参数。
///
/// 上游是裸 `page: int = 1, page_size: int = 20`（**无 `ge`/`le`**），
/// 上下界由 service 层的 `validate_page` 兜，错误码是
/// `invalid_moment_collection_filter`（不是通用的 `invalid_page`）。
#[derive(Debug, Deserialize)]
struct ListPointsQuery {
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "twenty")]
    page_size: i64,
}

/// 每请求解析出的签名密钥。
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
    row: &sm_service::collections::ordered::MomentCollectionWithCount,
) -> MomentCollectionResource {
    MomentCollectionResource {
        id: row.collection.id,
        name: row.collection.name.clone(),
        description: row.collection.description.clone(),
        point_count: row.point_count,
        // `point_count == 0` 时**不解析封面** —— 与上游
        // `_to_resource` 的 `cover_image if point_count else None` 一致。
        cover_image: row
            .cover
            .as_ref()
            .filter(|_| row.point_count > 0)
            .map(|image| ImageResource {
                id: image.id,
                origin: sign_image_origin(secret, &image.origin, now_seconds()),
            }),
        created_at: timestamp(row.collection.created_at),
        updated_at: timestamp(row.collection.updated_at),
    }
}

/// `GET ""` —— **裸列表，不分页**。
async fn list_moment_collections(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<MomentCollectionResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = MomentCollectionService::new(state.db());
    let rows = service.list_collections().await?;
    Ok(Json(
        rows.iter()
            .map(|row| collection_resource(&secret, row))
            .collect(),
    ))
}

/// `POST ""` —— **201 Created**。重名 → **409**。
async fn create_moment_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<MomentCollectionCreateRequest>,
) -> Result<(StatusCode, Json<MomentCollectionResource>), ErrorResponse> {
    let service = MomentCollectionService::new(state.db());
    let created = service
        .create(&payload.name, Some(&payload.description))
        .await?;
    // 新建合集成员数必为 0，也必然没有封面 —— 不必再查一次，
    // 因此这里**不解析签名密钥**（解析它只是为了签封面 URL）。
    Ok((
        StatusCode::CREATED,
        Json(MomentCollectionResource {
            id: created.id,
            name: created.name,
            description: created.description,
            point_count: 0,
            cover_image: None,
            created_at: timestamp(created.created_at),
            updated_at: timestamp(created.updated_at),
        }),
    ))
}

/// `GET /{collection_id}` —— 不存在 → **404**。
async fn get_moment_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
) -> Result<Json<MomentCollectionResource>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = MomentCollectionService::new(state.db());
    let row = service.get_with_count(collection_id).await?;
    Ok(Json(collection_resource(&secret, &row)))
}

/// `PATCH /{collection_id}` —— 部分更新。
async fn update_moment_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeJson(payload): EnvelopeJson<MomentCollectionUpdateRequest>,
) -> Result<Json<MomentCollectionResource>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = MomentCollectionService::new(state.db());
    // 宏的 `update` 返回合集本身（不带计数），所以随后再取一次带计数的行。
    service
        .update(
            collection_id,
            CollectionUpdate {
                name: payload.name,
                description: payload.description,
            },
        )
        .await?;
    let row = service.get_with_count(collection_id).await?;
    Ok(Json(collection_resource(&secret, &row)))
}

/// `DELETE /{collection_id}` —— **204，无 body**。
async fn delete_moment_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    let service = MomentCollectionService::new(state.db());
    service.delete(collection_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_moment_collection_points(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeQuery(query): EnvelopeQuery<ListPointsQuery>,
) -> Result<Json<sm_core::pagination::Paginated<MomentCollectionPointItemResource>>, ErrorResponse>
{
    let secret = secret(&state)?;
    let service = MomentCollectionService::new(state.db());
    let (rows, total) = service
        .list_points_paged(collection_id, query.page, query.page_size)
        .await?;
    let now = now_seconds();
    let items = rows
        .iter()
        .map(|row| point_resource(&secret, now, row))
        .collect();
    Ok(Json(sm_core::pagination::Paginated::new(
        items,
        query.page,
        query.page_size,
        total,
    )))
}

/// 一个点位条目的资源化。抽出来是因为 `map` 里放不下块注释。
fn point_resource(
    secret: &str,
    now: i64,
    row: &MomentPointWithImage,
) -> MomentCollectionPointItemResource {
    MomentCollectionPointItemResource {
        point_id: row.point.id,
        media_id: row.point.media_id,
        movie_number: row.point.movie_number.clone(),
        video_item_id: row.point.video_item_id,
        thumbnail_id: row.point.thumbnail_id,
        offset_seconds: row.point.offset_seconds,
        image: ImageResource {
            id: row.image.id,
            origin: sign_image_origin(secret, &row.image.origin, now),
        },
        created_at: timestamp(row.point.created_at),
        position: row.item.position,
    }
}

/// `PUT /{collection_id}/points/{point_id}` —— **204，无 body**。
///
/// 点不存在 → 404；**已存在 → 仍然 204**（上游 `add_point` 查到就 `return`）。
/// 「添加」不返回新增成员（见模块文档）。
async fn add_point_to_moment_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((collection_id, point_id)): Path<(i32, i32)>,
) -> Result<StatusCode, ErrorResponse> {
    let service = MomentCollectionService::new(state.db());
    service.add(collection_id, point_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /{collection_id}/points/{point_id}` —— **204，无 body**。
///
/// **幂等**：该点不在集合里时**仍然 204**。
async fn remove_point_from_moment_collection(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((collection_id, point_id)): Path<(i32, i32)>,
) -> Result<StatusCode, ErrorResponse> {
    let service = MomentCollectionService::new(state.db());
    service.remove(collection_id, point_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /{collection_id}/points` —— **整体替换，204，无 body**。
///
/// 覆盖式：先清空该集合的点位再按 `point_ids` 写入。宏的 `set_members`
/// **先全量校验再动数据**（逐个 `require_member` 在事务之前）。
async fn set_moment_collection_points(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(collection_id): Path<i32>,
    EnvelopeJson(payload): EnvelopeJson<MomentCollectionSetPointsRequest>,
) -> Result<StatusCode, ErrorResponse> {
    let service = MomentCollectionService::new(state.db());
    service
        .set_members(collection_id, &payload.point_ids)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
