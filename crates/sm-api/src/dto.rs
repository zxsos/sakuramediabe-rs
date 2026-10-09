//! 响应 / 请求 DTO。
//!
//! # 字段集合照抄上游 `src/schema/collections/playlists.py:11-64`
//!
//! ```text
//! id, name, kind, description, is_system, is_mutable, is_deletable,
//! movie_count, created_at, updated_at
//! ```
//!
//! `is_system` / `is_mutable` / `is_deletable` 是**派生字段**：上游靠
//! `from_playlist` 里的 `extra` 注入，Rust 侧在 `From<Playlist>` 里算。
//!
//! # 一处已知偏差（不要当成已对齐）
//!
//! **`created_at` / `updated_at` 的时间戳格式。** 上游 `SchemaModel` 有
//!    全局 `@field_serializer("*")` 按**运行时本地时区**序列化；这里按
//!    naive UTC 输出 `YYYY-MM-DDTHH:MM:SS`。上游容器 `TZ=UTC`、
//!    PG `timezone=UTC`，实际值应当一致，但**没有对拍过**。
//!    另外 DB 两列可空而上游 DTO 非可空，这里 None 时输出空串。
//!
//! `movie_count` 曾恒为 0，现已由
//! [`PlaylistResource::with_movie_count`] 从 service 的聚合计数填充。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sm_db::collections::Playlist;
use sm_service::catalog::actor::{
    ActorFilterOptions, ActorFilterRange, ActorTag, ActorView, ActorYear,
};
use sm_service::catalog::movie::MovieCard;
use sm_service::collections::playlist::PlaylistMovieCard;
use sm_service::discovery::daily_recommendation::DailyRecommendationCard;
use sm_service::discovery::hot_actress_release::HotActressReleaseItem;
use sm_service::playback::media_summary::{MediaSummary, MovieMediaAttachment};
use sm_service::videos::VideoMediaItem;

/// 播放列表响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistResource {
    pub id: i32,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub is_system: bool,
    pub is_mutable: bool,
    pub is_deletable: bool,
    pub movie_count: i32,
    pub created_at: String,
    pub updated_at: String,
}

impl PlaylistResource {
    /// 带成员计数构造。
    ///
    /// `From<Playlist>` 委托到这里并传 0 —— 那条路径只用于「刚写完、
    /// 成员数必为 0」的场景（`POST /playlists`）。凡是**读**已有列表的
    /// 地方都必须走这里并传真实计数，否则客户端读到的 `movie_count: 0`
    /// 与「列表是空的」不可区分。
    pub fn with_movie_count(value: Playlist, movie_count: i32) -> Self {
        let is_system = value.is_system();
        Self {
            id: value.id,
            name: value.name,
            kind: value.kind,
            description: value.description,
            is_system,
            is_mutable: !is_system,
            is_deletable: !is_system,
            movie_count,
            created_at: format_timestamp(value.created_at),
            updated_at: format_timestamp(value.updated_at),
        }
    }
}

impl From<Playlist> for PlaylistResource {
    /// **计数恒为 0。** 见 [`PlaylistResource::with_movie_count`] 的说明 ——
    /// 新建列表的成员数确实是 0，但这个 `From` 也会被误用到读路径上。
    fn from(value: Playlist) -> Self {
        Self::with_movie_count(value, 0)
    }
}

impl From<&sm_service::collections::playlist::PlaylistWithCount> for PlaylistResource {
    fn from(value: &sm_service::collections::playlist::PlaylistWithCount) -> Self {
        Self::with_movie_count(value.playlist.clone(), value.movie_count)
    }
}

/// `GET /playlists/{id}/resolutions` 的响应项（上游 `PlaylistResolutionOption`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistResolutionOption {
    /// 档位标签（`8K` / `4K` / …）。已归一为大写形态。
    pub resolution: String,
    /// 列表内最高分辨率落在该档位的**影片**数。
    pub count: i32,
}

/// `GET /status/capabilities` 的响应体。
///
/// 上游那个端点**没有 `response_model`**，直接返回 `capabilities()` 的
/// `dict[str, bool]`。所以这里的字段集合就是全部 —— 多一个键客户端不会
/// 报错，但少一个会。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesResource {
    /// 相似影片能力（依赖 `qdrant.enabled`）。
    pub movie_similarity: bool,
    /// 图搜能力（依赖 `qdrant.enabled` **且** `image_search.enabled`）。
    pub image_search: bool,
}

impl From<sm_service::system::optional_services::Capabilities> for CapabilitiesResource {
    fn from(value: sm_service::system::optional_services::Capabilities) -> Self {
        Self {
            movie_similarity: value.movie_similarity,
            image_search: value.image_search,
        }
    }
}

impl From<&sm_service::collections::playlist::ResolutionOption> for PlaylistResolutionOption {
    fn from(value: &sm_service::collections::playlist::ResolutionOption) -> Self {
        Self {
            resolution: value.resolution.clone(),
            count: value.count,
        }
    }
}

/// naive UTC → 上游 Pydantic 的 datetime 字面量格式。
///
/// 见模块文档第 2 条：可选值缺失时输出空串，而不是让整个响应序列化失败。
fn format_timestamp(value: Option<NaiveDateTime>) -> String {
    value.map_or_else(String::new, |dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
}

// ---------------------------------------------------------------- 鉴权

/// `POST /auth/tokens` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct TokenCreateRequest {
    pub username: String,
    pub password: String,
}

/// `POST /auth/token-refreshes` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct TokenRefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUserSummary {
    pub username: String,
}

/// 令牌响应体，字段与 `src/schema/system/auth.py:19-26` 一致。
///
/// `expires_in` 是**配置窗口**（`access_token_expire_minutes * 60`）而不是
/// 实际剩余秒数 —— 上游就是这么算的，别"修正"它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResource {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expires_at: String,
    pub refresh_expires_at: String,
    pub user: AuthUserSummary,
}

/// UTC 时间戳 → 与播放列表一致的 Pydantic 字面量格式。
pub(crate) fn format_utc(value: chrono::DateTime<chrono::Utc>) -> String {
    value.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `POST /playlists` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistCreateRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// `PATCH /playlists/{id}` 请求体。两个字段都可缺省。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaylistUpdateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// `GET /config` 的响应体（上游 `ConfigResource`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigResource {
    /// 全部**可改**配置节的明文快照。只读键（`auth` / `enable_docs` /
    /// `plugins`）已被剔除 —— 见 `routes/config.rs` 的模块文档。
    pub values: serde_json::Value,
}

/// `PATCH /config` 的响应体（上游 `ConfigUpdateResource`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigUpdateResource {
    /// 写盘**之后**的公开快照。注意它与运行中进程实际在用的值可能不同 ——
    /// 那要等重启，见 `restart_required`。
    pub values: serde_json::Value,
    /// 必须重启的进程。**恒为** `["api", "aps"]`，永不为空。
    ///
    /// 上游的类型是 `list[Literal["api", "aps"]]`，字面量集合只有这两个，
    /// 所以这个字段表达的不是「哪些进程受影响」，而是「本项目由这两个进程
    /// 读配置」这一事实。客户端据此提示用户重启，而不需要判断非空。
    pub restart_required: Vec<String>,
}

impl ConfigUpdateResource {
    /// 用 service 给出的公开快照构造，`restart_required` 取常量。
    pub fn new(values: serde_json::Value) -> Self {
        Self {
            values,
            restart_required: sm_service::system::config::RESTART_REQUIRED
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

/// 图片资源（上游 `ImageResource`）。
///
/// # `origin` 必须是签名后的 URL
///
/// 上游用 pydantic 的 `field_validator` 在序列化前改写 `origin`。这里把签名
/// 放在 API 层而不是 `Serialize` 实现里，因为签名要密钥，而密钥是运行时配置
/// —— 序列化时拿不到。代价是每个构造 `ImageResource` 的地方都要记得调
/// [`sign_image_origin`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageResource {
    pub id: i32,
    /// 签名后的 `/files/images/...` URL，或空串。
    pub origin: String,
}

/// 把图片相对路径换成签名 URL，逐条对应上游的 `sign_image_path` validator。
///
/// - 空串 / 全空白 -> 原样返回（上游 `if not value`）；
/// - 已以 `/files/images/` 开头 -> 原样返回。**再签一次会得到一个指向不存在
///   资源的 URL**，因为签名串会被当成路径的一部分；
/// - 否则签名。
///
/// 签名失败返回原值而非报错：上游的 validator 抛错会让整个响应 500，而一张图
/// 签不了名不该让整个列表拿不到。
pub fn sign_image_origin(secret: &str, origin: &str, now_seconds: i64) -> String {
    let trimmed = origin.trim();
    if trimmed.is_empty() || trimmed.starts_with("/files/images/") {
        return trimmed.to_owned();
    }
    sm_core::signing::build_signed_image_url(secret, trimmed, now_seconds)
        .unwrap_or_else(|_| trimmed.to_owned())
}

/// `import_status` 的中文说明（上游 `common/media_import_status.py:88-90`
/// 的 `describe_import_status`）。
///
/// 上游是 `IMPORT_STATUS_DESCRIPTIONS.get(value or "", value or "")`：
/// **未知取值回退原值**（不是 `None`、不是空串），空值回退空串。
///
/// `import_status` 本身是 `varchar(32) NOT NULL DEFAULT 'pending'` 且**无 CHECK
/// 约束**，所以库里理论上可能存着这五个之外的值 —— 那时回退原值比回退 `null`
/// 好：客户端至少能看到后端到底写了什么。
///
/// 全仓只有这一份映射（`GET /download-tasks` 与 `GET /movie-subscriptions`
/// 两处都用它），改文案只改这里。
pub fn describe_import_status(value: &str) -> String {
    match value {
        "pending" => "待导入：下载已完成，等待自动导入触发".to_owned(),
        "running" => "导入中：导入作业正在执行".to_owned(),
        "completed" => "已导入：符合条件的媒体文件已入库".to_owned(),
        "failed" => "导入失败：存在未成功导入的文件".to_owned(),
        "skipped" => "已跳过：没有符合条件的媒体文件".to_owned(),
        other => other.to_owned(),
    }
}

/// 把「显式 `null`」与「缺键」分开的反序列化器。
///
/// # 为什么需要它
///
/// 上游的局部更新走 `payload.model_dump(exclude_unset=True)`，于是有三种状态：
/// **缺键** / **显式 `null`** / **有值**。而它们的处理各不相同，例如
/// `VideoItemUpdateRequest`：
///
/// ```text
/// {"cover_thumbnail_id": null}  -> 422（明确禁止清空封面）
/// {"title": null}               -> 忽略该字段，只推进 updated_at
/// {"release_date": null}        -> 清空发布日期
/// {}                            -> 422 空更新
/// ```
///
/// `Option<T>` 只有两种状态，会把「缺键」与「null」压成同一件事，于是上面
/// 三条里的两条会**改变契约**。
///
/// # 用法
///
/// 字段写成 `Option<Option<T>>` 并配 `#[serde(default, deserialize_with = ...)]`：
/// `#[serde(default)]` 让**缺键**走默认值（外层 `None`），于是这个函数只在
/// **键存在**时被调用 —— 显式 `null` 得到 `Some(None)`，有值得到 `Some(Some(v))`。
/// 之后用 `sm_service::videos::Field` 的三态枚举搬运。
pub fn deserialize_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// 视频归属合集的精简引用（上游 `VideoCollectionRef`）。
///
/// 只有 `id` 与 `name` —— 列表/详情里的「所属合集」标签只用得上这两个，
/// 带 `item_count` / `cover_image` 是浪费（前端 DTO 的注释也是这么写的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCollectionRefResource {
    pub id: i32,
    pub name: String,
}

/// 非 JAV 视频条目的列表项（上游 `VideoItemListItemResource`，14 个字段）。
///
/// # 字段名逐字对齐，前端 `VideoItemListItemDto` 逐个读它们
///
/// `id` / `title` / `summary` / `cover_image` / `release_date` /
/// `duration_seconds` / `file_size_bytes` / `cover_width` / `cover_height` /
/// `media_count` / `can_play` / `collections` / `created_at` / `updated_at`。
///
/// ⚠️ **骨架期这个类型写成了 6 个字段的另一套**（`thumbnail_url` /
/// `description` / `metadata`）—— 那三个键前端一个都不读。已按上游重写。
///
/// # `created_at` / `updated_at` 为什么是 `String`
///
/// 与 [`PlaylistResource`] 同一处理：naive UTC 输出
/// `YYYY-MM-DDTHH:MM:SS`，DB 列为空时输出空串。**已知偏差**，见模块文档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoItemListItemResource {
    pub id: i32,
    pub title: String,
    pub summary: String,
    pub cover_image: Option<ImageResource>,
    pub release_date: Option<String>,
    /// 首条**有效**媒体的时长（秒）。无有效媒体时为 0。
    pub duration_seconds: i32,
    pub file_size_bytes: i64,
    /// 封面像素宽高（= 首条有效媒体探测分辨率）。探测失败 / 无媒体时为 `None`，
    /// 前端按 16:9 占位。
    pub cover_width: Option<i32>,
    pub cover_height: Option<i32>,
    /// **全部**媒体数（含失效的）。
    pub media_count: i64,
    pub can_play: bool,
    /// 归属合集，后端按合集名升序返回。
    pub collections: Vec<VideoCollectionRefResource>,
    pub created_at: String,
    pub updated_at: String,
}

impl VideoItemListItemResource {
    /// 由 service 的列表项组装响应资源。
    ///
    /// `now` 由调用方传入而不是在这里取 —— 同一批条目必须用**同一个**时间戳
    /// 签名，否则列表里会出现几张图的有效期差几毫秒（可观测，但无意义）。
    pub fn from_list_item(
        secret: &str,
        now: i64,
        item: &sm_service::videos::VideoListItem,
    ) -> Self {
        Self {
            id: item.video.id,
            title: item.video.title.clone(),
            summary: item.video.summary.clone(),
            cover_image: item.cover.as_ref().map(|image| ImageResource {
                id: image.id,
                origin: sign_image_origin(secret, &image.origin, now),
            }),
            release_date: item
                .video
                .release_date
                .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string()),
            duration_seconds: item.duration_seconds,
            file_size_bytes: item.file_size_bytes,
            cover_width: item.cover_width,
            cover_height: item.cover_height,
            media_count: item.media_count,
            can_play: item.can_play,
            collections: item
                .collections
                .iter()
                .map(|row| VideoCollectionRefResource {
                    id: row.id,
                    name: row.name.clone(),
                })
                .collect(),
            created_at: item
                .video
                .created_at
                .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_default(),
            updated_at: item
                .video
                .updated_at
                .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_default(),
        }
    }
}

/// 片段所属合集的摘要（上游 `ClipCollectionSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipCollectionSummary {
    pub id: i32,
    pub name: String,
}

/// 片段资源（上游 `MediaClipResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaClipResource {
    /// 上游字段名是 `clip_id` 而非 `id` —— 与缩略图接口一致，客户端按它跳转。
    pub clip_id: i32,
    /// 来源 Media。**孤立片段为 `null`**（来源被删，外键 SET NULL）。
    pub media_id: Option<i32>,
    pub movie_number: Option<String>,
    pub start_offset_seconds: i32,
    pub end_offset_seconds: i32,
    pub title: String,
    pub duration_seconds: i32,
    pub file_size_bytes: i64,
    /// 区间首帧封面。孤立片段或该帧无缩略图时为 `null`。
    pub cover_image: Option<ImageResource>,
    /// 带签名的流播放 URL。
    pub stream_url: String,
    /// 上游非可空（`clip: datetime`）而 DB 列可空 —— `None` 时输出空串，
    /// 与本文件其余 DTO 的时间戳处理一致。
    pub created_at: String,
}

/// 片段详情（上游 `MediaClipDetailResource`）。`base` 会被摊平进 JSON，
/// 所以响应体的键与列表项**完全一致**，再加两个数组。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaClipDetailResource {
    #[serde(flatten)]
    pub base: MediaClipResource,
    /// 区间内所有帧，供前端循环播放成动态预览。
    pub preview_frames: Vec<ImageResource>,
    /// 该片段所属的合集，供「加入合集」选择器回显已勾选项。
    pub collections: Vec<ClipCollectionSummary>,
}

/// 片段区间内的一个缩略图（上游 `MediaClipThumbnailResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaClipThumbnailResource {
    pub clip_id: i32,
    /// 源媒体缩略图 id，与 `/media/{id}/thumbnails` 语义一致。
    pub thumbnail_id: i32,
    /// **相对片段起点**的秒数，供进度条定位跳转。
    pub offset_seconds: i32,
    pub image: ImageResource,
}

/// 媒体点（时刻）—— 上游 `MediaPointResource`。
///
/// # 键名是 `point_id`
///
/// 客户端按它调 `DELETE /media/{media_id}/points/{point_id}`。骨架期 service
/// 层那个类型用的字段名是 `id`，序列化出去键名不对 —— 已随本轮一起更正。
///
/// # `image` 在这里才是**可用的 URL**
///
/// service 层（`MediaPointValue`）带的是未签名的 `image_origin`，签名要密钥、
/// 只有这一层有 —— 所以这个 DTO 由 `sm_api` 组装，不由 service 直接返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPointResource {
    pub point_id: i32,
    /// 来源 Media。**可为 `None`** —— 来源被删后置空，时刻点仍在。
    pub media_id: Option<i32>,
    pub thumbnail_id: Option<i32>,
    pub offset_seconds: i32,
    pub image: ImageResource,
    /// 上游非可空（`datetime`）而 DB 列可空 —— 缺失输出空串，与其余 DTO 一致。
    pub created_at: String,
}

impl MediaPointResource {
    /// 由 service 的值对象组装。`now` 由调用方传入 —— 一批必须用同一个时间戳。
    pub fn from_value(
        secret: &str,
        now: i64,
        value: &sm_service::playback::media::MediaPointValue,
    ) -> Self {
        Self {
            point_id: value.point_id,
            media_id: value.media_id,
            thumbnail_id: value.thumbnail_id,
            offset_seconds: value.offset_seconds,
            image: ImageResource {
                id: value.image_id,
                origin: sign_image_origin(secret, &value.image_origin, now),
            },
            created_at: value
                .created_at
                .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_default(),
        }
    }
}

/// `GET /media-points` 的列表项 —— 上游 `MediaPointListItemResource`
/// （`schema/playback/media.py`）。
///
/// 与 [`MediaPointResource`] 的区别：后者是**某个媒体下的**点（`/media/{id}/points`），
/// 这个是**全局**的时刻列表项，多带 `movie_number` / `video_item_id` 供前端区分归属。
///
/// ⚠️ 骨架期 `routes/media_points.rs` 里有一个**自造的** `MediaPointListItem`
/// （`id` / `kind` / `offset_seconds` / `title`）—— 字段集合与上游毫无交集，
/// 且没有图片。已删除，改用本类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPointListItemResource {
    pub point_id: i32,
    /// 来源媒体。**可为 `None`** —— 来源被删后置空，时刻点仍在。
    pub media_id: Option<i32>,
    /// 非 JAV 媒体没有番号。
    pub movie_number: Option<String>,
    /// 供前端区分归属的非 JAV 条目 id。
    pub video_item_id: Option<i32>,
    pub thumbnail_id: Option<i32>,
    pub offset_seconds: i32,
    pub image: ImageResource,
    /// 上游非可空（`datetime`）而 DB 列可空 —— 缺失输出空串，与其余 DTO 一致。
    pub created_at: String,
}

impl MediaPointListItemResource {
    /// 由 `MediaService::list_media_points` 的 JSON 行组装。
    ///
    /// `now` 由调用方传入 —— 一批必须用同一个时间戳，否则同一页里的 URL
    /// 生效时刻不一致（前端缓存命中率会掉）。
    ///
    /// 缺 `point_id` 返回 `None`：那是 service 输出形状变了，宁可漏一条也
    /// 不要伪造一个 `point_id: 0` —— 客户端会拿它去删一个不存在的点。
    pub fn from_list_item(secret: &str, now: i64, value: &Value) -> Option<Self> {
        Some(Self {
            point_id: i32::try_from(value.get("point_id")?.as_i64()?).ok()?,
            media_id: value
                .get("media_id")
                .and_then(Value::as_i64)
                .and_then(|id| i32::try_from(id).ok()),
            movie_number: value
                .get("movie_number")
                .and_then(Value::as_str)
                .map(str::to_owned),
            video_item_id: value
                .get("video_item_id")
                .and_then(Value::as_i64)
                .and_then(|id| i32::try_from(id).ok()),
            thumbnail_id: value
                .get("thumbnail_id")
                .and_then(Value::as_i64)
                .and_then(|id| i32::try_from(id).ok()),
            offset_seconds: i32::try_from(value.get("offset_seconds")?.as_i64()?).ok()?,
            image: ImageResource {
                id: i32::try_from(value.get("image_id")?.as_i64()?).ok()?,
                origin: sign_image_origin(
                    secret,
                    value
                        .get("image_origin")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    now,
                ),
            },
            created_at: value
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

/// 播放进度 —— 上游 `MediaProgressResource`。
// `last_watched_at` 是 `String`（带堆分配），所以**不能** derive `Copy`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaProgressResource {
    pub media_id: i32,
    pub last_position_seconds: i32,
    /// 上游非可空（`datetime`）而 DB 列可空 —— 缺失输出空串。
    pub last_watched_at: String,
}

impl MediaProgressResource {
    pub fn from_value(value: &sm_service::playback::media::MediaProgressValue) -> Self {
        Self {
            media_id: value.media_id,
            last_position_seconds: value.last_position_seconds,
            last_watched_at: value
                .last_watched_at
                .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_default(),
        }
    }
}

/// 媒体缩略图 —— 上游 `MediaThumbnailResource`。
///
/// # 键名与骨架不同：`thumbnail_id` / `offset_seconds`，且多了 `width`/`height`
///
/// 骨架的 service 层类型用的是 `id` / `offset` / `image_path`，序列化出去键名
/// 全不对。见 `sm_service::playback::thumbnails::artifacts::MediaThumbnailValue`。
///
/// # `width` / `height` 是**整组共享**的
///
/// 取自该媒体的**第一条**缩略图（同一视频流的尺寸相同）。解不出来时两者都是
/// `null` —— 前端据此回退到固定比例，而不是把卡片撑成 0 高。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaThumbnailResource {
    pub thumbnail_id: i32,
    pub media_id: i32,
    /// **相对该视频起点**的秒数。
    pub offset_seconds: i32,
    pub image: ImageResource,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl MediaThumbnailResource {
    /// 由 service 的值对象组装。`now` 由调用方传入 —— 一批必须用同一个时间戳。
    pub fn from_value(
        secret: &str,
        now: i64,
        value: &sm_service::playback::thumbnails::artifacts::MediaThumbnailValue,
    ) -> Self {
        Self {
            thumbnail_id: value.thumbnail_id,
            media_id: value.media_id,
            offset_seconds: value.offset_seconds,
            image: ImageResource {
                id: value.image_id,
                origin: sign_image_origin(secret, &value.image_origin, now),
            },
            width: value.width,
            height: value.height,
        }
    }
}

/// `PATCH /media-clips/{id}` 的请求体。
///
/// `title` **必填且允许空串** —— 上游是 `title: str`（无默认值），而「清空
/// 标题」是合法的编辑动作，所以不能加 `#[serde(default)]`：那会让缺字段的
/// 请求体变成「清空标题」，而上游会 422。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaClipUpdateRequest {
    pub title: String,
}

// ------------------------------------------------------------- 片段合集

/// `POST /clip-collections` 的请求体（上游 `ClipCollectionCreateRequest`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ClipCollectionCreateRequest {
    /// 上游是 `Field(min_length=1)` + validator 双重保证：长度 0 在 pydantic
    /// 层被拒，全空白在 validator 层被拒。落到这里是 service 的同一个判据。
    pub name: String,
    /// 上游默认空串，**不是** `Option`。
    #[serde(default)]
    pub description: String,
}

/// `PATCH /clip-collections/{id}` 的请求体。
///
/// # 显式 `null` 与「不给出」在这里是同一件事 —— 刻意的偏离
///
/// 上游是 `name: str | None = None`，validator 对 `None` 直接返回 `None`，
/// 而 `update_collection` 用 `model_dump(exclude_unset=True)` —— 于是
/// `{"name": null}` 会得到 `update_data["name"] = None`，紧接着
/// `_normalize_name(None)` 调 `.strip()` 抛 `AttributeError`，**整个请求 500**。
///
/// Rust 的 `Option<String>` 天然把「不给出」与「给出 null」都收成 `None`，
/// 所以这里两者等价：显式 `null` 等于「不改这一列」。这与客户端的意图一致，
/// 也避开了那个 500。要「清空描述」请给 `""` 而不是 `null`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClipCollectionUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// `Some("")` 清空描述；`None` 或不给出 = 不改。
    #[serde(default)]
    pub description: Option<String>,
}

/// `PUT /clip-collections/{id}/clips` 的请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct ClipCollectionSetClipsRequest {
    /// 目标有序列表。**重复 id 会被去重，以首次出现的位置为准。**
    pub clip_ids: Vec<i32>,
}

/// 合集资源（上游 `ClipCollectionResource`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipCollectionResource {
    pub id: i32,
    pub name: String,
    /// 上游默认空串，DB 也是 NOT NULL —— 不会是 `null`。
    pub description: String,
    /// **只数产物有效的成员。** 客户端按它决定要不要显示数字。
    pub clip_count: i32,
    /// 第一个**有效**成员的封面；无有效成员或该成员是孤立片段时为 `null`。
    pub cover_image: Option<ImageResource>,
    pub created_at: String,
    pub updated_at: String,
}

/// 合集里的一个成员（上游 `ClipCollectionClipItemResource`）。
///
/// 继承列表项的全部字段再加 `position`，所以响应体的键与片段列表项**完全
/// 一致**，末尾多一个 `position`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipCollectionClipItemResource {
    #[serde(flatten)]
    pub base: MediaClipResource,
    /// 显式 `position` 维护的播放顺序。
    pub position: i32,
}

// ---------------------------------------------------------------- 演员目录

/// naive UTC 时间戳 → 上游 Pydantic 的 `datetime` 字面量形状；缺失时 `null`。
///
/// 与 `format_timestamp` 的区别：后者给**非可空**字段用（缺失输出空串），
/// 这里给可空字段用（缺失输出 `null`）。两个形状不能混：客户端对
/// `birthday` / `subscribed_at` 判 `null`，对 `created_at` 判空串。
fn format_optional_timestamp(value: Option<NaiveDateTime>) -> Option<String> {
    value.map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// 演员列表项（上游 `ActorResource`，`src/schema/catalog/actors.py:36-52`）。
///
/// 比 [`ActorDetailResource`] 少 7 个字段（`gender` / `birthplace` /
/// `blood_type` / `display_name_override` / `has_profile_image_override` /
/// `mutation_revision` / `manual_fields`）—— 上游列表页不需要这些，多带了
/// 只是徒增响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorResource {
    pub id: i32,
    pub javdb_id: String,
    pub name: String,
    /// 别名，`"主名 / 别名"` 形式。
    pub alias_name: String,
    /// 展示名：本地覆盖优先，否则 `name`。
    pub display_name: String,
    /// 生效头像（覆盖优先）。无头像时 `null`。
    pub profile_image: Option<ImageResource>,
    pub is_subscribed: bool,
    /// 订阅时间；未订阅时 `null`。
    pub subscribed_at: Option<String>,
    /// 关联影片数（实时按 `movie_actor` 数）。
    pub movie_count: i64,
    /// 周岁；`birthday` 为空时 `null`。
    pub age: Option<i32>,
    /// `YYYY-MM-DD`；为空时 `null`。
    pub birthday: Option<String>,
    pub height_cm: Option<i32>,
    pub bust_cm: Option<i32>,
    pub waist_cm: Option<i32>,
    pub hips_cm: Option<i32>,
    pub cup: Option<String>,
}

/// 演员详情（上游 `ActorDetailResource`）。
///
/// `#[serde(flatten)]` 让响应体的键与列表项**完全一致**，末尾多 7 个详情字段
/// —— 与上游 `ActorDetailResource(ActorResource)` 的继承语义一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorDetailResource {
    #[serde(flatten)]
    pub base: ActorResource,
    /// 1 = 女、2 = 男、0 = 未知。
    pub gender: i32,
    pub birthplace: Option<String>,
    pub blood_type: Option<String>,
    pub display_name_override: Option<String>,
    pub has_profile_image_override: bool,
    pub mutation_revision: i64,
    /// 归属为 `host:manual` 的字段名，升序。
    pub manual_fields: Vec<String>,
}

/// 一位演员的共享字段 → 列表项。
fn actor_base(view: &ActorView, secret: &str, now: i64) -> ActorResource {
    let actor = &view.actor;
    ActorResource {
        id: actor.id,
        javdb_id: actor.javdb_id.clone(),
        name: actor.name.clone(),
        alias_name: actor.alias_name.clone(),
        display_name: actor.display_name().to_owned(),
        profile_image: view.image_id.map(|id| ImageResource {
            id,
            origin: sign_image_origin(
                secret,
                view.image_origin.as_deref().unwrap_or_default(),
                now,
            ),
        }),
        is_subscribed: actor.is_subscribed,
        subscribed_at: format_optional_timestamp(actor.subscribed_at),
        movie_count: view.movie_count,
        age: view.age,
        birthday: actor
            .birthday
            .map(|date| date.format("%Y-%m-%d").to_string()),
        height_cm: actor.height_cm,
        bust_cm: actor.bust_cm,
        waist_cm: actor.waist_cm,
        hips_cm: actor.hips_cm,
        cup: actor.cup.clone(),
    }
}

impl ActorResource {
    /// 从 service 的投影构造列表项。
    ///
    /// 需要 `secret` 是因为头像 `origin` 必须签名后才能给客户端 —— 与
    /// [`sign_image_origin`] 同一个理由。
    pub fn from_view(view: &ActorView, secret: &str, now: i64) -> Self {
        actor_base(view, secret, now)
    }
}

impl ActorDetailResource {
    /// 从 service 的投影构造详情。
    pub fn from_view(view: &ActorView, secret: &str, now: i64) -> Self {
        let actor = &view.actor;
        Self {
            base: actor_base(view, secret, now),
            gender: actor.gender,
            birthplace: actor.birthplace.clone(),
            blood_type: actor.blood_type.clone(),
            display_name_override: actor.display_name_override.clone(),
            has_profile_image_override: actor.has_profile_image_override(),
            mutation_revision: actor.mutation_revision,
            manual_fields: view.manual_fields.clone(),
        }
    }
}

/// 筛选项里的一个区间（上游 `ActorFilterRangeResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorFilterRangeResource {
    pub min: Option<i32>,
    pub max: Option<i32>,
    /// 该区间里**有值**的演员数（`COUNT(col)`，不是 `COUNT(*)`）。
    pub populated_count: i64,
}

/// 罩杯筛选项（上游 `ActorCupFilterOption`）。
///
/// 字段名是 `value` 而不是 `cup` —— 客户端按 `value` 读。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorCupFilterOption {
    pub value: String,
    pub count: i64,
}

/// `GET /actors/filter-options` 的结果（上游 `ActorFilterOptionsResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorFilterOptionsResource {
    pub actor_count: i64,
    /// `YYYY-MM-DD`，客户端据此显示「截至某日」。
    pub as_of_date: String,
    pub age: ActorFilterRangeResource,
    pub height_cm: ActorFilterRangeResource,
    pub cups: Vec<ActorCupFilterOption>,
}

impl From<ActorFilterRange> for ActorFilterRangeResource {
    fn from(value: ActorFilterRange) -> Self {
        Self {
            min: value.min,
            max: value.max,
            populated_count: value.populated_count,
        }
    }
}

impl From<ActorFilterOptions> for ActorFilterOptionsResource {
    fn from(value: ActorFilterOptions) -> Self {
        Self {
            actor_count: value.actor_count,
            as_of_date: value.as_of_date.format("%Y-%m-%d").to_string(),
            age: value.age.into(),
            height_cm: value.height_cm.into(),
            cups: value
                .cups
                .into_iter()
                .map(|(value, count)| ActorCupFilterOption { value, count })
                .collect(),
        }
    }
}

/// 标签资源（上游 `src/schema/catalog/movies.py:102` 的 `TagResource`）。
///
/// 主键叫 `tag_id` 而不是 `id` —— 与片段资源里的 `clip_id` 同一种约定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagResource {
    pub tag_id: i32,
    pub name: String,
}

impl From<ActorTag> for TagResource {
    fn from(value: ActorTag) -> Self {
        Self {
            tag_id: value.tag_id,
            name: value.name,
        }
    }
}

/// `POST /actors/{id}/merge` 的请求体（上游 `ActorMergeRequest`）。
///
/// 上游是 `source_actor_ids: list[int] = Field(min_length=1)` + 正整数校验；
/// 落到本层的是原始 `Vec<i32>`，「非空 / 正整数」在 handler 里判 —— 那是
/// pydantic 的职责，serde 不表达 `min_length`。
#[derive(Debug, Clone, Deserialize)]
pub struct ActorMergeRequest {
    /// 待归并到目标名下的来源演员 id。
    pub source_actor_ids: Vec<i32>,
}

/// 年份分布（上游 `YearResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YearResource {
    pub year: i32,
    pub movie_count: i64,
}

impl From<ActorYear> for YearResource {
    fn from(value: ActorYear) -> Self {
        Self {
            year: value.year,
            movie_count: value.movie_count,
        }
    }
}

// ---------------------------------------------------------------- 影片卡片

/// 一条媒体摘要（上游 `MediaSummaryResource`，`src/schema/common/media.py`）。
///
/// 主键字段叫 `media_id` 而不是 `id` —— 上游是 `validation_alias="id"`，
/// 与片段资源的 `clip_id`、标签资源的 `tag_id` 同一种约定：**客户端按业务名读**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSummaryResource {
    pub media_id: i32,
    /// 所属库。**`null` 只在左连接未命中时出现**，而 `media_library_id_fk` 是
    /// `ON DELETE CASCADE`，所以实践里恒有值；保留可空是为了与上游 DTO 一致。
    pub library_id: Option<i32>,
    pub library_name: Option<String>,
    /// 客户端据此决定用哪个 provider 的播放/下载能力。
    pub provider_key: Option<String>,
    pub file_name: String,
    pub resolution: Option<String>,
    pub file_size_bytes: i64,
    pub duration_seconds: i32,
    /// probe 写入的视频参数，**对象**而不是字符串。
    ///
    /// 库里是 TEXT 存 JSON，上游 `JsonTextField` 在读取时就解码成 `dict`，
    /// 所以契约要求这里是对象。解码失败（脏文本 / 空串）时给 `null` ——
    /// 上游在那个情况下会 500，而**摘要的用途是渲染列表**，一个坏值不该让
    /// 整个列表拿不到（同一个理由写在 `sm_service::playback::media_summary`）。
    pub video_info: Option<Value>,
    pub valid: bool,
}

impl From<&MediaSummary> for MediaSummaryResource {
    fn from(value: &MediaSummary) -> Self {
        Self {
            media_id: value.media_id,
            library_id: value.library_id,
            library_name: value.library_name.clone(),
            provider_key: value.provider_key.clone(),
            file_name: value.file_name.clone(),
            resolution: value.resolution.clone(),
            file_size_bytes: value.file_size_bytes,
            duration_seconds: value.duration_seconds,
            video_info: value
                .video_info
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok()),
            valid: value.valid,
        }
    }
}

/// 详情页一条媒体（上游 `MovieMediaResource`，`schema/catalog/movies.py:125-129`）。
///
/// # 为什么用 `#[serde(flatten)]` 而不是重抄那 10 个字段
///
/// 上游是**继承**（`MovieMediaResource(MediaSummaryResource)`）。照抄字段名意味着
/// `MediaSummaryResource` 将来加字段时这里会**静默**漏掉 —— JSON 里只是少一个键，
/// 没有任何编译期信号。`flatten` 让两个类型只维护一处。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MovieMediaResource {
    /// 10 个摘要字段，**扁平整**在顶层（不是嵌套对象）。
    #[serde(flatten)]
    pub summary: MediaSummaryResource,
    /// 签名播放地址。**非空 `str`** —— 失效媒体给**空串**。
    ///
    /// ⚠️ 这里的空串与合集成员那条「空串 ≠ null」的红线**不冲突**：合集成员的
    /// `play_url` 是**可空**字段，空串意味着「有媒体但播不了」的误导；而本字段
    /// 上游声明就是 `str`（非空），空串是**明确**的「这条播不了」信号，前端据此
    /// 禁用单条播放（上游 `_media_items` 原话）。
    pub play_url: String,
    /// 该 provider 声明的交付方式，**首项为默认**（与 `play_url` 用的同一个）。
    pub playback_deliveries: Vec<String>,
    pub progress: Option<MovieMediaProgressResource>,
    pub points: Vec<MovieMediaPointResource>,
}

/// 上游 `MovieMediaProgressResource`（`movies.py:113-115`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MovieMediaProgressResource {
    pub last_position_seconds: i32,
    /// naive UTC 输出 `YYYY-MM-DDTHH:MM:SS`（与全仓其它时间戳同一偏差）。
    pub last_watched_at: Option<String>,
}

/// 上游 `MovieMediaPointResource`（`movies.py:118-122`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MovieMediaPointResource {
    pub point_id: i32,
    pub thumbnail_id: Option<i32>,
    pub offset_seconds: i32,
    pub image: ImageResource,
}

impl MovieMediaResource {
    /// 由服务层的一条媒体组装。`play_url` 按上游 `_media_items`（`:309-317`）：
    /// **失效媒体给空串**，有效媒体才签名。
    ///
    /// `deliveries` 是**该媒体所属 provider 声明的**交付顺序（调用方查注册表得到）。
    /// provider 查不到时上游会抛错（**不是**给空 `play_url`），所以那一步在调用方。
    pub fn from_media_item(
        item: &VideoMediaItem,
        secret: &str,
        now: i64,
        deliveries: &[String],
    ) -> Self {
        let play_url = if item.summary.valid {
            crate::signing::signed_play_url(secret, now, item.summary.media_id, deliveries)
                .unwrap_or_default()
        } else {
            String::new()
        };
        Self {
            summary: MediaSummaryResource::from(&item.summary),
            play_url,
            playback_deliveries: deliveries.to_vec(),
            progress: item
                .progress
                .as_ref()
                .map(|progress| MovieMediaProgressResource {
                    last_position_seconds: progress.position_seconds,
                    last_watched_at: progress
                        .last_watched_at
                        .map(|ts| ts.format("%Y-%m-%dT%H:%M:%S").to_string()),
                }),
            points: item
                .points
                .iter()
                .map(|row| MovieMediaPointResource {
                    point_id: row.point.id,
                    thumbnail_id: row.point.thumbnail_id,
                    offset_seconds: row.point.offset_seconds,
                    image: ImageResource {
                        id: row.image.id,
                        origin: sign_image_origin(secret, &row.image.origin, now),
                    },
                })
                .collect(),
        }
    }
}

/// 视频条目详情（上游 `VideoItemDetailResource`，`schema/videos/items.py:37-38`）。
///
/// = [`VideoItemListItemResource`]（14 字段，扁平）+ `media_items`。同
/// [`MovieMediaResource`] 的理由用 `flatten` 而不是重抄 14 个字段。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoItemDetailResource {
    #[serde(flatten)]
    pub list: VideoItemListItemResource,
    pub media_items: Vec<MovieMediaResource>,
}

/// 影片卡片（上游 `MovieListItemResource`）。
///
/// 字段集合照抄上游，共 23 个 —— **多一个少一个都是契约变更**。
///
/// # `can_play` / `media_count` / `media_items` 三个是派生字段
///
/// 上游挂在 `Movie` 实例上（`attach_movie_list_media`），Rust 侧装在
/// [`PlaylistMovieCard::media`] 里。注意 `can_play` 是「**至少一条**有效媒体」，
/// 不是「全部有效」也不是「有媒体」。
///
/// # `is_collection` / `is_subscribed` / `is_blacklisted` 取自 `movie` 表
///
/// 前两个由导入流程维护，`is_blacklisted` 由黑名单动作维护 —— 都不是这里算的。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MovieListItemResource {
    /// 影片主键。番号才是对外主标识，但统一 action 协议收的是整数 id。
    pub id: i32,
    pub javdb_id: Option<String>,
    /// 元数据来源记录（JSONB）。**原样透传**，不做字段级校验。
    pub metadata_source: Option<Value>,
    pub movie_number: String,
    pub title: String,
    pub series_id: Option<i32>,
    pub series_name: Option<String>,
    pub cover_image: Option<ImageResource>,
    pub thin_cover_image: Option<ImageResource>,
    /// `YYYY-MM-DD`；没有发布日期时 `null`（上游 validator 把空串也归成 `null`）。
    pub release_date: Option<String>,
    pub duration_minutes: i32,
    pub score: f64,
    pub watched_count: i32,
    pub want_watch_count: i32,
    pub comment_count: i32,
    pub score_number: i32,
    pub heat: i32,
    pub is_collection: bool,
    pub is_subscribed: bool,
    pub is_blacklisted: bool,
    pub can_play: bool,
    pub media_count: i64,
    pub media_items: Vec<MediaSummaryResource>,
}

/// 影片卡片 23 个字段的**唯一**映射实现。
///
/// 播放列表卡片与影片列表卡片都走这里。抄成两份的话，将来加一个字段就会漏掉
/// 一个端点 —— 而漏掉的字段在 JSON 里只是**少一个键**，客户端反序列化拿到
/// null，不报错。
fn movie_list_item(
    movie: &sm_db::catalog::movie::Movie,
    cover_image: Option<&sm_db::catalog::asset::Image>,
    thin_cover_image: Option<&sm_db::catalog::asset::Image>,
    series_name: Option<&str>,
    media: &MovieMediaAttachment,
    secret: &str,
    now: i64,
) -> MovieListItemResource {
    let signed = |image: &sm_db::catalog::asset::Image| ImageResource {
        id: image.id,
        origin: sign_image_origin(secret, &image.origin, now),
    };
    MovieListItemResource {
        id: movie.id,
        javdb_id: movie.javdb_id.clone(),
        metadata_source: movie.metadata_source.clone(),
        movie_number: movie.movie_number.clone(),
        title: movie.title.clone(),
        series_id: movie.series_id,
        series_name: series_name.map(str::to_owned),
        cover_image: cover_image.map(signed),
        thin_cover_image: thin_cover_image.map(signed),
        release_date: movie
            .release_date
            .map(|value| value.format("%Y-%m-%d").to_string()),
        duration_minutes: movie.duration_minutes,
        score: movie.score,
        watched_count: movie.watched_count,
        want_watch_count: movie.want_watch_count,
        comment_count: movie.comment_count,
        score_number: movie.score_number,
        heat: movie.heat,
        is_collection: movie.is_collection,
        is_subscribed: movie.is_subscribed,
        is_blacklisted: movie.is_blacklisted,
        can_play: media.can_play,
        media_count: media.media_count,
        media_items: media.media_items.iter().map(Into::into).collect(),
    }
}

impl MovieListItemResource {
    /// 从播放列表卡片组装（`GET /playlists/{id}/movies`）。
    ///
    /// `secret` / `now` 用于给封面签名 —— 与 [`ActorResource`] 同一个理由：
    /// 签名要运行时密钥，序列化时拿不到。
    pub fn from_card(card: &PlaylistMovieCard, secret: &str, now: i64) -> Self {
        movie_list_item(
            &card.movie,
            card.cover_image.as_ref(),
            card.thin_cover_image.as_ref(),
            card.series_name.as_deref(),
            &card.media,
            secret,
            now,
        )
    }

    /// 从影片卡片组装（`GET /movies*`）。与
    /// [`MovieListItemResource::from_card`] 共用同一份字段映射。
    pub fn from_movie_card(card: &MovieCard, secret: &str, now: i64) -> Self {
        movie_list_item(
            &card.movie,
            card.cover_image.as_ref(),
            card.thin_cover_image.as_ref(),
            card.series_name.as_deref(),
            &card.media,
            secret,
            now,
        )
    }
}

/// 每日推荐响应元素（上游 `DailyRecommendationMovieResource`，
/// `schema/discovery/daily_recommendations.py:6-14`）。
///
/// **继承完整的影片卡片**（[`MovieListItemResource`]，`#[serde(flatten)]`），
/// 再挂 8 个推荐字段。**页级没有 `snapshot_date`** —— 快照日期是元素级字段
/// （`is_stale` 也是元素级：`row.snapshot_date < today`）。
#[derive(Debug, Clone, Serialize)]
pub struct DailyRecommendationMovieResource {
    #[serde(flatten)]
    pub base: MovieListItemResource,
    /// 快照日期，`YYYY-MM-DD`。
    pub snapshot_date: String,
    /// 生成时刻，`YYYY-MM-DDTHH:MM:SS`（与 [`PlaylistResource`] 同一约定，
    /// 见模块文档「已知偏差」）。
    pub generated_at: String,
    pub rank: i32,
    /// 综合推荐分。上游 `row.score` → `recommendation_score`。
    pub recommendation_score: f64,
    pub reason_codes: Vec<String>,
    /// 理由**文案**。取库里存的 `reason_texts`，**不**由 `reason_codes` 现翻。
    pub reason_texts: Vec<String>,
    /// 六路信号分量。**原样透传库里存的 JSON 对象**（缺省 `{}`）。
    pub signal_scores: serde_json::Map<String, Value>,
    /// 快照是否早于今天（元素级）。
    pub is_stale: bool,
}

impl DailyRecommendationMovieResource {
    /// 从读侧装配结果组装（`GET /daily-recommendations`）。
    ///
    /// 日期 / 时间戳 / JSON 三种兜底的格式化都留在本模块：时间戳走本模块的
    /// `format_timestamp`，与 [`MovieListItemResource::from_movie_card`]
    /// 的封面签名是同一个调用点。
    pub fn from_daily_card(card: &DailyRecommendationCard, secret: &str, now: i64) -> Self {
        let item = &card.item;
        Self {
            base: MovieListItemResource::from_movie_card(&card.card, secret, now),
            snapshot_date: item.snapshot_date.format("%Y-%m-%d").to_string(),
            generated_at: format_timestamp(Some(item.generated_at)),
            rank: item.rank,
            recommendation_score: item.score,
            reason_codes: item.parsed_reason_codes().unwrap_or_default(),
            reason_texts: item.parsed_reason_texts().unwrap_or_default(),
            signal_scores: item.parsed_signal_scores().unwrap_or_default(),
            is_stale: card.is_stale,
        }
    }
}

/// 热播女优新作里的女优信息（上游 `HotActressResource`，
/// `schema/discovery/hot_actress_releases.py:6-12`）。
#[derive(Debug, Clone, Serialize)]
pub struct HotActressResource {
    pub id: i32,
    pub name: String,
    /// `display_name_override` 优先，否则 `name`（`Actor::display_name`）。
    pub display_name: String,
    /// **生效**头像（覆盖优先）。无头像时 `null` —— 注意不是 `profile_image_id`：
    /// 用户设的本地头像不生效是这一条最容易漏的地方。
    pub profile_image: Option<ImageResource>,
    /// 该女优的历史作品数，**已扣掉出现在本结果里的这部**。
    pub historical_movie_count: i64,
    /// 上游 `round(score, 4)` —— 与 `recommendation_score` **同一个值**。
    pub hotness_score: f64,
}

/// 热播女优新作的一条结果（上游 `HotActressReleaseMovieResource`）。
///
/// **继承完整的影片卡片**（[`MovieListItemResource`]，`#[serde.flatten]`），
/// 再挂两个字段 —— 与上游的继承语义一致。所以响应里没有 `movie_id` /
/// `title` 这类「拍平后重命名」的键：影片的一切都在卡片自己的键上。
#[derive(Debug, Clone, Serialize)]
pub struct HotActressReleaseMovieResource {
    #[serde(flatten)]
    pub base: MovieListItemResource,
    /// 推荐分。与 `hot_actress.hotness_score` **同源同值**
    /// （上游两处都是 `round(scored_movie.score, 4)`）—— 看着冗余，但客户端
    /// 各读各的，不能只给一个。
    pub recommendation_score: f64,
    pub hot_actress: HotActressResource,
}

impl HotActressReleaseMovieResource {
    /// 从读侧装配结果组装（`GET /hot-actress-releases`）。
    ///
    /// 头像的「覆盖优先」由服务层解析（`ActorView.image_id`），这里只签名 ——
    /// 与 [`ActorResource::from_view`] 同一个来源，所以演员列表与这里
    /// 看到的是同一张头像。
    pub fn from_item(item: &HotActressReleaseItem, secret: &str, now: i64) -> Self {
        let actress = &item.actress.actor;
        let score = round_to_4(item.score);
        Self {
            base: MovieListItemResource::from_movie_card(&item.card, secret, now),
            recommendation_score: score,
            hot_actress: HotActressResource {
                id: actress.id,
                name: actress.name.clone(),
                display_name: actress.display_name().to_owned(),
                profile_image: item.actress.image_id.map(|id| ImageResource {
                    id,
                    origin: sign_image_origin(
                        secret,
                        item.actress.image_origin.as_deref().unwrap_or_default(),
                        now,
                    ),
                }),
                historical_movie_count: item.historical_movie_count,
                hotness_score: score,
            },
        }
    }
}

/// 上游 `round(value, 4)`。
///
/// # 为什么不是 `(value * 1e4).round() / 1e4`
///
/// 乘 `1e4` 会再引入一次浮点误差：`0.12345 * 1e4 == 1234.4999999999998`，
/// 于是 `0.12345` 被舍成 `0.1234`，而上游（以及「对二进制真值做十进制
/// 舍入」的任何正确实现）给 `0.1235`。`{:.4}` 走的是精确十进制转换，
/// 与 Python `round` 同语义（含半值取偶），所以用**格式化再解析**。
///
/// 失败时回退原值：`{:.4}` 对任何非 NaN 的 f64 都产出可解析的十进制串。
fn round_to_4(value: f64) -> f64 {
    format!("{value:.4}").parse().unwrap_or(value)
}

/// 播放列表内的影片卡片（上游 `PlaylistMovieListItemResource`）。
///
/// `#[serde(flatten)]` 让响应体的键与 [`MovieListItemResource`] **完全一致**，
/// 末尾多一个 `playlist_item_updated_at` —— 与上游的继承语义一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistMovieListItemResource {
    #[serde(flatten)]
    pub base: MovieListItemResource,
    /// 列表关系上的最近触达时间。
    ///
    /// 上游声明成非空 `datetime`，而 DDL 里 `playlist_movie.updated_at` 可空 ——
    /// 本仓库对同类情况的约定是空串（见 [`PlaylistResource`] 的 `created_at`）。
    pub playlist_item_updated_at: String,
}

impl PlaylistMovieListItemResource {
    /// 从 service 的卡片投影组装。
    ///
    /// 时间戳的格式化留在这里而不是让调用方自己 `format!`：
    /// `format_timestamp` 是本模块对「可空 → 空串」这条约定的唯一实现。
    pub fn from_card(card: &PlaylistMovieCard, secret: &str, now: i64) -> Self {
        Self {
            base: MovieListItemResource::from_card(card, secret, now),
            playlist_item_updated_at: format_timestamp(card.playlist_item_updated_at),
        }
    }
}


// ---------------------------------------------------------------- 任务目录

/// 任务运行记录，字段与上游 `TaskRunResource`
/// （`src/schema/system/activity.py`）逐个对应。
///
/// # 时间字段的三个形状不能混
///
/// | 字段 | 上游 | 本结构 |
/// |---|---|---|
/// | `created_at` / `updated_at` | `datetime`（非可空） | `String`，缺失输出空串 |
/// | `started_at` / `finished_at` | `datetime \| None` | `Option<String>`，缺失输出 `null` |
///
/// 客户端对前者判空串、对后者判 `null`，混了会让「正在运行」的任务显示成
/// 1970 年的时间戳。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRunResource {
    pub id: i32,
    pub task_key: String,
    pub task_name: String,
    pub trigger_type: String,
    pub state: String,
    pub progress_current: Option<i32>,
    pub progress_total: Option<i32>,
    pub progress_text: Option<String>,
    pub result_text: Option<String>,
    /// 结构化摘要。DB 里是 `JsonTextField`（TEXT 里的 JSON 文本），这里已解析。
    pub result_summary: Option<Value>,
    pub error_message: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&sm_db::system::activity::BackgroundTaskRun> for TaskRunResource {
    fn from(run: &sm_db::system::activity::BackgroundTaskRun) -> Self {
        let summary = sm_db::system::activity::result_summary::from_column_text(
            run.result_summary.as_deref(),
        );
        Self {
            id: run.id,
            task_key: run.task_key.clone(),
            task_name: run.task_name.clone(),
            trigger_type: run.trigger_type.clone(),
            state: run.state.clone(),
            progress_current: run.progress_current,
            progress_total: run.progress_total,
            progress_text: run.progress_text.clone(),
            result_text: run.result_text.clone(),
            // 空对象当 `None` —— 上游那列的 DEFAULT 是 `'{}'`，而
            // 「没有摘要」与「摘要是个空对象」对客户端是同一件事。
            result_summary: (!summary.as_object().is_none_or(|map| map.is_empty()))
                .then_some(summary),
            error_message: run.error_message.clone(),
            started_at: format_optional_timestamp(run.started_at),
            finished_at: format_optional_timestamp(run.finished_at),
            created_at: format_timestamp(run.created_at),
            updated_at: format_timestamp(run.updated_at),
        }
    }
}

/// 任务目录项，字段与上游 `JobMetadataResource`
/// （`src/schema/system/jobs.py:5-18`）一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobMetadataResource {
    pub task_key: String,
    pub plugin_id: Option<String>,
    pub log_name: String,
    pub cli_name: String,
    pub cli_help: String,
    /// 配置里覆盖 cron 用的键。`manual_only` 任务为 `None`。
    pub cron_setting: Option<String>,
    /// 当前生效的 cron 表达式。`manual_only` 任务为 `None`。
    pub cron_expr: Option<String>,
    /// 能力未开时的原因。前端据此把入口置灰。
    pub disabled_reason: Option<String>,
    /// **不是目录里的原始声明**，而是「声明允许 **且** 当前未被停用」。
    ///
    /// 上游 `_build_job_metadata`（`jobs.py:46`）：
    /// `manual_trigger_allowed = job_def.manual_trigger_allowed and not disabled_reason`。
    /// 直接透传声明值会让前端在能力关闭时仍显示可点按钮，点下去吃 409。
    pub manual_trigger_allowed: bool,
    /// 参数的 JSON Schema。**当前恒为 `None`** —— 插件任务的 schema 正文要从
    /// proto 的 `google.protobuf.Struct` 转成 `serde_json::Value`，那一层还没写。
    /// 目录里只带 `has_params_schema`（有没有），见
    /// [`sm_service::system::jobs`] 的模块文档。
    pub params_schema: Option<Value>,
    pub last_task_run: Option<TaskRunResource>,
}

// ---------------------------------------------------------------- 账号资料

/// 账号资料，字段与上游 `AccountResource`
/// （`src/schema/system/account.py:6-9`）一致。
///
/// # `password_hash` 永不返回
///
/// 上游的 resource 由 Pydantic 从实体构造，`password_hash` 不在字段表里所以
/// 自动排除。这里是手写结构体 —— **加字段时不要把它带上**，那等于把 argon2
/// 哈希塞进一个「任何登录用户都能调」的响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountResource {
    pub username: String,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

impl From<&sm_db::system::user::User> for AccountResource {
    fn from(user: &sm_db::system::user::User) -> Self {
        Self {
            username: user.username.clone(),
            created_at: format_timestamp(user.created_at),
            last_login_at: format_optional_timestamp(user.last_login_at),
        }
    }
}

/// `PATCH /account` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct AccountUpdateRequest {
    pub username: String,
}

/// `POST /account/password` 请求体。
///
/// **没有** `username` 字段 —— 改的是**当前登录者**的密码，由 JWT 里的 id
/// 定位。多一个字段就意味着「改别人的密码」这条路。
#[derive(Debug, Clone, Deserialize)]
pub struct AccountPasswordChangeRequest {
    pub current_password: String,
    pub new_password: String,
}

// ---------------------------------------------------------------- 活动中心

/// 通知，字段与上游 `NotificationResource`
/// （`src/schema/system/activity.py:26-40`）一致。
///
/// # 刻意**没有** `read_at`
///
/// 上游这个 resource 就没带 `read_at` —— 只有 `NotificationReadResponse` 有，
/// 而 `activity.py` 的六个端点**一个都不用**那个 resource。所以照抄：客户端
/// 从列表里拿不到「何时被读到的时刻」。
///
/// 看起来像漏字段，但补上就是**契约变更**：客户端会开始依赖一个上游不保证的
/// 键，而上游哪天加上/改掉它，两边就静默分叉了。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationResource {
    pub id: i32,
    pub category: String,
    pub title: String,
    pub content: String,
    pub event_type: Option<String>,
    pub dedupe_key: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<i32>,
    pub is_read: bool,
    pub created_at: String,
    pub updated_at: String,
    pub related_task_run_id: Option<i32>,
    pub related_resource_type: Option<String>,
    pub related_resource_id: Option<i32>,
}

impl From<&sm_db::system::activity::SystemNotification> for NotificationResource {
    fn from(item: &sm_db::system::activity::SystemNotification) -> Self {
        Self {
            id: item.id,
            category: item.category.clone(),
            title: item.title.clone(),
            content: item.content.clone(),
            event_type: item.event_type.clone(),
            dedupe_key: item.dedupe_key.clone(),
            resource_type: item.resource_type.clone(),
            resource_id: item.resource_id,
            is_read: item.is_read,
            created_at: format_timestamp(item.created_at),
            updated_at: format_timestamp(item.updated_at),
            related_task_run_id: item.related_task_run_id,
            related_resource_type: item.related_resource_type.clone(),
            related_resource_id: item.related_resource_id,
        }
    }
}

/// `POST /system/notifications/read` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationReadBatchRequest {
    pub ids: Vec<i32>,
}

/// 批量标记已读的结果。`read` 与 `read-all` **共用**这一种响应。
///
/// 字段与上游 `NotificationBatchReadResponse`（`activity.py:54-58`）一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationBatchReadResponse {
    /// 本次新置为已读的条数。
    pub updated_count: u64,
    /// 操作**之后**剩余的未读总数。客户端靠它更新 tab 上的红点。
    pub unread_count: i64,
}

impl From<sm_service::system::activity::BatchReadResult> for NotificationBatchReadResponse {
    fn from(value: sm_service::system::activity::BatchReadResult) -> Self {
        Self {
            updated_count: value.updated_count,
            unread_count: value.unread_count,
        }
    }
}

/// 首屏聚合响应。字段与上游 `ActivityBootstrapResource`
/// （`activity.py:61-65`）一致。
///
/// 两份分页 + 两个标量，一次返回 —— 理由见
/// [`sm_service::system::activity::bootstrap`] 的模块文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityBootstrapResource {
    pub notifications: sm_core::pagination::Paginated<NotificationResource>,
    pub unread_count: i64,
    pub active_task_runs: Vec<TaskRunResource>,
    pub task_runs: sm_core::pagination::Paginated<TaskRunResource>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_db::collections::{Playlist, PLAYLIST_KIND_CUSTOM, PLAYLIST_KIND_RECENTLY_PLAYED};

    fn playlist(kind: &str) -> Playlist {
        Playlist {
            id: 3,
            name: "我的列表".to_owned(),
            description: "d".to_owned(),
            owner_plugin_id: None,
            plugin_key: None,
            kind: kind.to_owned(),
            created_at: NaiveDateTime::parse_from_str("2026-10-04 01:02:03", "%Y-%m-%d %H:%M:%S")
                .ok(),
            updated_at: None,
        }
    }

    #[test]
    fn derived_flags_follow_upstream() {
        let custom = PlaylistResource::from(playlist(PLAYLIST_KIND_CUSTOM));
        assert!(!custom.is_system);
        assert!(custom.is_mutable);
        assert!(custom.is_deletable);

        let system = PlaylistResource::from(playlist(PLAYLIST_KIND_RECENTLY_PLAYED));
        assert!(system.is_system);
        assert!(!system.is_mutable);
        assert!(!system.is_deletable);
    }

    #[test]
    fn timestamp_uses_the_pydantic_shape() {
        let resource = PlaylistResource::from(playlist(PLAYLIST_KIND_CUSTOM));
        assert_eq!(resource.created_at, "2026-10-04T01:02:03");
        // 缺失时输出空串，而不是让序列化失败
        assert_eq!(resource.updated_at, "");
    }

    #[test]
    fn field_set_matches_the_upstream_dto() {
        // 字段数量变化会直接改变响应体字节数 —— 用序列化结果钉住。
        let json = serde_json::to_value(PlaylistResource::from(playlist(PLAYLIST_KIND_CUSTOM)))
            .expect("DTO 必须可序列化");
        let object = json.as_object().expect("DTO 是 JSON 对象");
        for key in [
            "id",
            "name",
            "kind",
            "description",
            "is_system",
            "is_mutable",
            "is_deletable",
            "movie_count",
            "created_at",
            "updated_at",
        ] {
            assert!(object.contains_key(key), "缺少字段 {key}");
        }
        assert_eq!(
            object.len(),
            10,
            "字段数必须是 10，多一个少一个都是契约变更"
        );
    }
}
