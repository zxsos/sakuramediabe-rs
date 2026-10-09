//! 媒体 / 时刻 / 进度 / 缩略图的 CRUD 与分页（上游 `playback/media_service.py`，782 行，本域最大）。
//!
//! # 它横跨 JAV 与 videos 两个域
//!
//! `media` 表同时存 JAV 影片的文件和普通视频条目（`video_item_id` 有值时）。
//! 所以 [`MediaService::list_duplicate_media_groups`] 的 `kind` 参数是
//! `Literal["jav", "video"]` 而不是布尔。
//!
//! # 删除媒体要**三件事**按顺序做
//!
//! ```text
//!   1. provider 删物理文件   （storage.delete_media）
//!   2. 清 Qdrant 缩略图向量   （仅当图搜启用）
//!   3. 删 DB 记录 + 图片文件   （image_cleanup）
//! ```
//!
//! ⚠️ **provider 放最后是错的**（文件没了但记录还在 → 播放时 404 且无法重试）。
//! ⚠️ **provider 放最先也是错的**（文件删了但 DB 事务回滚 → 记录指向不存在的文件）。
//!
//! 上游的顺序是 provider → Qdrant → DB，且**三步各自独立**：任何一步失败都
//! **不阻止**后续步骤。这不是「best effort」而是有意的 —— 卡在第一步会让
//! 媒体永远删不掉，而后面的清理（孤儿向量、图片文件）是**必须做**的。
//!
//! # 排序字段是**白名单映射**，不是自由字符串
//!
//! [`MEDIA_LIST_SORT_FIELD_MAP`]。`heat` 是**唯一可空**的排序字段
//! （`MEDIA_LIST_NULLABLE_SORT_FIELDS`）—— 排序时要用 `NULLS LAST`，
//! 否则 Postgres 默认 `NULLS LAST FOR ASC` / `NULLS FIRST FOR DESC` 会让
//! 「按热度降序」变成「没热度的排最前」。

use sm_db::repo::media::MediaListFilter;
use sm_db::repo::playback::MediaProgressRepository;
use sm_db::repo::MovieRepository;
use sm_db::repo::{
    ImageRepository, MediaPointRepository, MediaRepository, MediaThumbnailRepository,
};
use sm_db::Db;

use crate::catalog::image_cleanup::ImageCleanupService;
use crate::error::{details_of, ServiceError};
use crate::playback::provider_helpers::{self, json_or_null, ProviderFailure, StorageGateway};
use crate::system::config::ConfigService;

/// `media_point_not_found`（**带归属**）。
///
/// details 带两个键：客户端要能区分「这个点不存在」与「它属于别的媒体」——
/// 上游把两者合并成同一个 404（`_require_media_point_for_media` 是一条带
/// `media_id` 的查询），所以文案与码都一样。
fn point_not_found_for_media(media_id: i32, point_id: i32) -> ServiceError {
    ServiceError::not_found_with(
        "media_point_not_found",
        "Media point not found",
        details_of("media_id", media_id)
            .into_iter()
            .chain(details_of("point_id", point_id))
            .collect(),
    )
}

/// 媒体点（时刻）的种类。
pub mod media_point_kind {
    /// JAV 时刻。
    pub const JAV: &str = "jav";
    /// 视频条目时刻。
    pub const VIDEO: &str = "video";
    /// 全部。
    pub const ALL: &str = "all";
}

/// 排序字段白名单。**不在表里的一律 422**。
/// ★ 排序白名单**只有两项**，且 `heat` 在 **`movie` 表上**
/// （`media_service.py:94-97`）：
///
/// ```python
/// MEDIA_LIST_SORT_FIELD_MAP = {"file_size_bytes": Media.file_size_bytes, "heat": Movie.heat}
/// ```
///
/// ⚠️ 骨架期这里有四项（多出 `created_at` / `updated_at` / `file_name`），
/// 而且把 `heat` 写成 media 自己的列 —— 都是自造的。media 表**没有** heat。
///
/// # 别名约定
///
/// `m` = media，`mv` = movie。排序要 `LEFT JOIN movie`，因为非 JAV 视频没有
/// 影片、heat 恒空；所以 `heat` 排序必须 `NULLS LAST`（见
/// [`MEDIA_LIST_NULLABLE_SORT_FIELDS`]）。
pub const MEDIA_LIST_SORT_FIELD_MAP: [(&str, &str); 2] = [
    ("file_size_bytes", "m.file_size_bytes"),
    ("heat", "mv.heat"),
];

/// 可空的排序字段。排序时要显式 `NULLS LAST`。
pub const MEDIA_LIST_NULLABLE_SORT_FIELDS: [&str; 1] = ["heat"];

/// 解析排序表达式。`None`/空 → `None`（用默认排序）；不合法 → **422**。
///
/// # ★ 形状是 `field:direction`，**不是 `-field`**
///
/// 上游 `resolve_sort_expression(value, MEDIA_LIST_SORT_FIELD_MAP, ...)`
/// （`service_helpers.py`）：先 `strip().lower()`，再按 **`:`** 切出字段名与
/// 方向，方向只认 `asc` / `desc`，字段名要在白名单里 —— 任何一步不满足都是
/// `422 invalid_media_filter`（`details.sort`）。
///
/// ⚠️ 骨架期这里收 `-field` 前缀表示降序 —— 那是**自造**的约定，客户端按上游
/// 发 `heat:desc` 会被判成「未知字段」。已纠正为 `:` 形式（与同域的片段列表
/// [`crate::playback::media_clip`] 的 `MEDIA_CLIP_SORT_FIELDS` 一致）。
///
/// **不夹到默认值** —— 非法值就是 422，不是「悄悄用默认排序」。
pub fn resolve_sort(value: Option<&str>) -> Result<Option<(&'static str, bool)>, ServiceError> {
    let Some(raw) = value.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    // 归一化 `strip().lower()`：`HEAT:DESC` 与 `heat:desc` 等价。
    let normalized = raw.to_ascii_lowercase();
    let invalid = || {
        ServiceError::validation_with(
            "invalid_media_filter",
            "Invalid sort expression",
            details_of("sort", raw),
        )
    };
    let Some((field, direction)) = normalized.split_once(':') else {
        return Err(invalid());
    };
    if direction != "asc" && direction != "desc" {
        return Err(invalid());
    }
    let Some((_, column)) = MEDIA_LIST_SORT_FIELD_MAP
        .iter()
        .find(|(name, _)| *name == field)
    else {
        return Err(invalid());
    };
    Ok(Some((*column, direction == "desc")))
}

/// `GET /media` 的查询参数。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct MediaListQuery {
    /// 媒体种类。**有默认值** `all`（`/media/duplicates` 那个必填）。
    pub kind: Option<String>,
    pub library_id: Option<i64>,
    /// **CSV** 形态（`?actor_ids=1,2`）—— 与 transfers 的重复参数不同。
    pub actor_ids: Option<String>,
    /// 缩略图生成状态。**状态字面量**（`pending` / `retry_wait` / `terminal` /
    /// `succeeded`）—— 骨架期这里写成 i32，而列是文本（见
    /// `sm_db::playback::media::thumbnail_state`）。
    pub thumbnail_generation_state: Option<String>,
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// 媒体分页。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaListPage {
    pub items: Vec<MediaListItemResource>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
}

/// 媒体列表项。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaListItemResource {
    pub id: i64,
    pub library_id: i64,
    pub file_name: String,
    /// 所属影片。**可能为 `None`** —— `video_item` 类的媒体没有影片。
    pub movie_id: Option<i64>,
    pub movie_number: Option<String>,
    /// 时长（秒）。`0` = 未回填（见 [`super::media_metadata_probe`]）。
    pub duration_seconds: i64,
    pub resolution: Option<String>,
    pub file_size_bytes: i64,
    pub valid: bool,
    pub created_at: Option<String>,
}

/// 多版本影片（同一番号有多个媒体）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MultiVersionMovieResource {
    pub movie_id: i64,
    pub movie_number: String,
    /// 该影片的全部媒体。**含无效的** —— 用户要看到「有 3 个文件，1 个坏了」。
    pub media: Vec<MediaListItemResource>,
}

/// 重复媒体分组（去重用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DuplicateMediaGroupResource {
    /// 分组键：文件哈希，或哈希缺失时退回 `文件名+大小`。
    pub dedup_key: String,
    /// ★ 该键是**哈希**还是**退化键**。客户端要能告诉用户「这批是按大小
    /// 猜的，可能是巧合」。
    pub key_kind: DuplicateKeyKind,
    pub media: Vec<MediaListItemResource>,
}

/// 分组键的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DuplicateKeyKind {
    /// 文件哈希。**可靠**。
    Hash,
    /// ★ 退化键（哈希未回填）。**不可靠** —— 可能是巧合。
    Degraded,
}

/// 时刻（media point）。上游 `MediaPointResource`。
///
/// # 字段名是 `point_id` 而不是 `id`
///
/// 上游那个 resource 的键就叫 `point_id`，客户端按它删点
/// （`DELETE /media/{media_id}/points/{point_id}`）。骨架期这里写成了 `id`，
/// 序列化出去键名不对。**类型改名叫 `MediaPointValue`** 就是为了不再与
/// API 层的同名资源混淆 —— 下面那条约定是原因。
///
/// # `image` 这里给的是**原始相对路径**，不是可用的 URL
///
/// 签名要密钥，而密钥是运行时配置、只有 API 层拿得到。所以这一层带
/// [`Self::image_id`] 与 [`Self::image_origin`]，由 `sm_api` 组装成
/// `ImageResource { id, origin: 签名后 }`。同一约定见
/// `super::thumbnails::artifacts::MediaThumbnailResource::image_path`。
///
/// 这也是**刻意不派 `Serialize`** 的原因：派了就会有人直接把它当响应体
/// 序列化出去，于是 `origin` 是未签名的裸路径（客户端拿到 403）。
#[derive(Debug, Clone)]
pub struct MediaPointValue {
    pub point_id: i32,
    /// 来源 Media。**可为 `None`** —— 来源被删后置空（`ON DELETE SET NULL`），
    /// 时刻点本身仍在。
    pub media_id: Option<i32>,
    pub thumbnail_id: Option<i32>,
    /// 在影片里的偏移（秒）。
    pub offset_seconds: i32,
    /// 指向的图片行。**NOT NULL**（删图会被外键 RESTRICT 拒绝）。
    pub image_id: i32,
    /// 图片的相对路径，**未签名**。
    pub image_origin: String,
    pub created_at: Option<chrono::NaiveDateTime>,
}

/// 一条播放进度。上游 `MediaProgressResource`。
///
/// `last_watched_at` 上游非可空（`datetime`）而 DB 列可空 —— API 层缺失时
/// 输出空串，与其余 DTO 的时间戳处理一致。
#[derive(Debug, Clone)]
pub struct MediaProgressValue {
    pub media_id: i32,
    pub last_position_seconds: i32,
    pub last_watched_at: Option<chrono::NaiveDateTime>,
}

/// 媒体服务。
///
/// # 五个仓储**按方法用到的面**收
///
/// 时刻点要 media / points / thumbnails / images，进度要 media / progress /
/// pool（后者给 `PlaylistService`），删媒体还要 thumbnails + images + config。
pub struct MediaService {
    media: MediaRepository,
    points: MediaPointRepository,
    thumbnails: MediaThumbnailRepository,
    progress: MediaProgressRepository,
    images: ImageRepository,
    pool: Db,
    /// 删时刻要连带清掉那张只服务于它的图，而清理服务要图片根目录。
    config: ConfigService,
    /// provider 数据面（删远端文件）。** [`None`] = 没装任何插件。 **
    ///
    /// 由组合根注入（`sm-server`），理由见
    /// [`StorageGateway`](crate::playback::provider_helpers::StorageGateway) 的
    /// 文档：`sm-service` 不能依赖 `sm-plugins`（依赖方向会成环）。
    gateway: Option<std::sync::Arc<dyn StorageGateway>>,
    /// provider 的**播放投递**能力。
    ///
    /// ★ 与 `gateway` 是**两个字段**（而不是同一个 trait 多加几个方法）：上游把
    /// 「支持哪些投递方式」当**可选能力**声明，缺它时业务层要**换行为**，不是
    /// 503。见 `docs/adr/2026-10-08-provider-seam.md` D1。
    playback: Option<std::sync::Arc<dyn provider_helpers::PlaybackGateway>>,
}

impl MediaService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            media: MediaRepository::new(db.clone()),
            points: MediaPointRepository::new(db.clone()),
            thumbnails: MediaThumbnailRepository::new(db.clone()),
            progress: MediaProgressRepository::new(db.clone()),
            images: ImageRepository::new(db.clone()),
            pool: db.clone(),
            config: config.clone(),
            gateway: None,
            playback: None,
        }
    }

    /// 注入 provider 数据面。由组合根调用。
    ///
    /// **没调用过 = 没有 provider**：`delete_media` 的第一步就会报 503
    /// `provider_not_installed`，而不是「跳过删远端」—— 后者会留下远端文件，
    /// 而调用方以为删干净了。
    pub fn with_gateway(mut self, gateway: std::sync::Arc<dyn StorageGateway>) -> Self {
        self.gateway = Some(gateway);
        self
    }

    /// 注入播放投递能力。由组合根调用（与 [`Self::with_gateway`] 同一个理由）。
    ///
    /// **没调用过 = 没有 provider**：`plan_playback` 会报 503
    /// `provider_not_installed`，而不是假装能播。
    pub fn with_playback_gateway(
        mut self,
        playback: std::sync::Arc<dyn provider_helpers::PlaybackGateway>,
    ) -> Self {
        self.playback = Some(playback);
        self
    }

    /// 单条媒体的**播放投递计划**。上游 `play_media`（`media.py:249-319`）里属于
    /// 服务层的那部分。
    ///
    /// # 这是 (b) 方案的落点
    ///
    /// 上游在**路由**里拿 `bundle.playback_deliveries` 做 422 判定 —— 那要求宿主
    /// 持有「provider 声明了哪些投递方式」。本仓改为把请求的 `delivery` **传给
    /// 插件**、由插件判定（见 [`provider_helpers::RequestedDelivery`]）。
    ///
    /// ⚠️ **已知近似**：插件回 `unsupported` 时宿主分不清「不支持这种投递方式」
    /// （换一种能成）与「根本不支持播放」（换也没用），两者都报 422。想分清就得
    /// 回到 (a)。这是 (b) 明码标价的代价，不是遗漏。
    ///
    /// 上游**不做逐媒体分级**的两级 404 在 `Self::require_media` 与这里各一次：
    /// 媒体缺失 → 404 `media_not_found`；媒体在但库为空 → 404
    /// `media_library_not_found`。**两者都在 404 之前查签名** —— 那是路由的事。
    pub async fn plan_playback(
        &self,
        media_id: i32,
        resource_path: &str,
        requested: provider_helpers::RequestedDelivery,
    ) -> Result<provider_helpers::PlaybackPlan, ServiceError> {
        let media = self.require_media(media_id).await?;

        // 没有 provider 就 503，**不降级**：降级会让客户端拿到一个「空计划」，
        // 而它无法区分「没装插件」与「插件说资源不在」。
        let Some(gateway) = self.playback.as_deref() else {
            return Err(ServiceError::unavailable(
                "provider_not_installed",
                "媒体提供方未安装",
            ));
        };

        let library = sm_db::repo::MediaLibraryRepository::new(self.pool.clone())
            .find_by_id(media.library_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_library_not_found",
                    "Media library not found",
                    "library_id",
                    media.library_id,
                )
            })?;
        let handle = provider_helpers::media_handle_for(&provider_helpers::MediaRecord {
            id: i64::from(media.id),
            library_id: i64::from(media.library_id),
            storage_ref: json_or_null(media.storage_ref.as_deref()),
            provider_config: json_or_null(library.provider_config.as_deref()),
            provider_key: library.provider_key.clone(),
            account_key: library.account_key.clone(),
            file_name: media.file_name.clone(),
            file_size_bytes: media.file_size_bytes,
            duration_seconds: media.duration_seconds,
        });

        gateway
            .plan_playback(&handle, resource_path, requested)
            .await
            .map_err(|failure| Self::map_playback_failure(&failure))
    }

    /// 多媒体的**合并**投递计划。上游 `play_merged_media`
    /// （`media.py:322-379`）里属于服务层的那部分。
    ///
    /// # 五道门，顺序**不能改**
    ///
    /// 上游是「先把行全取回来 → 再逐项判」，因为每道门的判据都来自行数据。
    /// 顺序错的表现不是崩溃而是**码错**（客户端会照着码去修错的东西）：
    ///
    /// | 序 | 条件 | 码 |
    /// |---|---|---|
    /// | 1 | 有 id 取不到 | 404 `media_not_found` |
    /// | 2 | 任一分段 `valid = false` | 422 `merged_playback_unavailable` |
    /// | 3 | `movie_number` 不唯一**或为空** | 422 `merged_playback_cross_movie` |
    /// | 4 | `library_id` 不唯一 | 422 `merged_playback_cross_library` |
    /// | 5 | 库记录不存在 | 404 `media_library_not_found` |
    ///
    /// 门 1 用**逐个查映射**而不是比 `len`：入参本身可能带重复（那样 `len` 不等
    /// 会被误报成「媒体不存在」），去重是路由层的事（`invalid_merged_playback`）。
    ///
    /// 门 3 的空值判据不能省 —— `{None}` 这个集合大小也是 1，光判「集合大小」
    /// 会放过一整组孤儿媒体。
    ///
    /// # 没有对应的「声明」门（(b) 的取舍）
    ///
    /// 上游另有两道门查「provider 声明了 `merged_playback_format ∈ {mp4,hls}`」
    /// 与「`handle_merged_playback` 可调用」（`media.py:356-357`、`:364-366`），
    /// 那要求宿主持有能力清单 —— 本仓没有（ADR `2026-10-08-provider-seam.md`）。
    /// 改为把请求交给插件，其 `unsupported` 由 `Self::map_merged_failure` 落成
    /// **同一个** 422 `merged_playback_unavailable`。
    ///
    /// 投递方式**强制 `proxy`**（上游 `media.py:370`）：合并流没有单个 provider
    /// 地址可指，所以不存在 302 这个选项 —— 这也让本方法的码没有歧义
    /// （对比 [`Self::plan_playback`] 的已知近似）。
    pub async fn plan_merged_playback(
        &self,
        ordered_ids: &[i32],
        resource_path: &str,
    ) -> Result<provider_helpers::PlaybackPlan, ServiceError> {
        let media_by_id = self.media.find_by_ids(ordered_ids).await?;

        // 门 1：逐个查，缺失即 404。返回**引用**，不 clone 整行。
        let mut medias: Vec<&sm_db::Media> = Vec::with_capacity(ordered_ids.len());
        for id in ordered_ids {
            medias.push(media_by_id.get(id).ok_or_else(|| {
                ServiceError::not_found("media_not_found", "部分媒体不存在", "media_id", *id)
            })?);
        }

        // 门 2：无效分段。上游的措辞**不指出是哪一个** —— 照抄，别自作主张加 id。
        if medias.iter().any(|media| !media.valid) {
            return Err(ServiceError::validation(
                "merged_playback_unavailable",
                "合并分段存在无效媒体",
            ));
        }

        // 门 3：同一部影片。`None` 与「多个番号」都拒。
        let mut movie_numbers = std::collections::HashSet::new();
        for media in &medias {
            let Some(number) = media.movie_number.as_deref() else {
                return Err(ServiceError::validation(
                    "merged_playback_cross_movie",
                    "合并分段必须属于同一部影片",
                ));
            };
            movie_numbers.insert(number);
        }
        if movie_numbers.len() != 1 {
            return Err(ServiceError::validation(
                "merged_playback_cross_movie",
                "合并分段必须属于同一部影片",
            ));
        }

        // 门 4：同一个媒体库。
        //
        // 上游还判了 `None in library_ids`，本仓**不需要**：`media.library_id` 是
        // NOT NULL（`sm-db/src/playback/media.rs:94`），判据恒假。
        let mut library_ids = std::collections::HashSet::new();
        for media in &medias {
            library_ids.insert(media.library_id);
        }
        if library_ids.len() != 1 {
            return Err(ServiceError::validation(
                "merged_playback_cross_library",
                "合并分段必须来自同一媒体库",
            ));
        }

        // 门 5：库记录在（拿 `provider_key` / `provider_config` 要用它）。
        let library_id = medias[0].library_id;
        let library = sm_db::repo::MediaLibraryRepository::new(self.pool.clone())
            .find_by_id(library_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_library_not_found",
                    "Media library not found",
                    "library_id",
                    library_id,
                )
            })?;

        let Some(gateway) = self.playback.as_deref() else {
            return Err(ServiceError::unavailable(
                "provider_not_installed",
                "媒体提供方未安装",
            ));
        };

        // 句柄按**入参顺序**建（`ordered_ids` 的顺序进了签名载荷，不能按查询
        // 返回顺序重排）。
        let handles: Vec<provider_helpers::MediaHandle> = medias
            .iter()
            .map(|media| {
                provider_helpers::media_handle_for(&provider_helpers::MediaRecord {
                    id: i64::from(media.id),
                    library_id: i64::from(media.library_id),
                    storage_ref: json_or_null(media.storage_ref.as_deref()),
                    provider_config: json_or_null(library.provider_config.as_deref()),
                    provider_key: library.provider_key.clone(),
                    account_key: library.account_key.clone(),
                    file_name: media.file_name.clone(),
                    file_size_bytes: media.file_size_bytes,
                    duration_seconds: media.duration_seconds,
                })
            })
            .collect();

        gateway
            .plan_merged_playback(
                &handles,
                resource_path,
                provider_helpers::RequestedDelivery::Proxy,
            )
            .await
            .map_err(|failure| Self::map_merged_failure(&failure))
    }

    /// 播放投递失败的映射。
    ///
    /// ★ `unsupported` **不走** [`Self::map_provider_failure`]：上游在这个端点给
    /// 它的码是 **422** `provider_playback_delivery_unsupported`
    /// （`media.py:278-283`），**不是 5xx** —— 那是客户端**换一种 `delivery`
    /// 重试可能成功**的情形，报 5xx 会让它一直退避重试同一个必败请求。
    fn map_playback_failure(failure: &ProviderFailure) -> ServiceError {
        if failure.code == provider_helpers::PROVIDER_UNSUPPORTED {
            return ServiceError::validation(
                "provider_playback_delivery_unsupported",
                "媒体提供方不支持该播放方式",
            );
        }
        Self::map_provider_failure(failure)
    }

    /// 合并播放失败的映射。
    ///
    /// `unsupported` → 422 `merged_playback_unavailable`
    /// （上游 `media.py:356-357` 与 `:364-366` 两道门给的就是这个码）。
    ///
    /// ★ 与 [`Self::map_playback_failure`] **不是**同一个码，且这里**没有歧义**：
    /// 合并播放的投递方式不由客户端选（永远是 `proxy`），所以 `unsupported` 只
    /// 可能意味着「不支持合并播放」。那边分不清是因为 `play` 的投递方式来自请求。
    fn map_merged_failure(failure: &ProviderFailure) -> ServiceError {
        if failure.code == provider_helpers::PROVIDER_UNSUPPORTED {
            return ServiceError::validation(
                "merged_playback_unavailable",
                "媒体提供方不支持合并播放",
            );
        }
        Self::map_provider_failure(failure)
    }

    /// 取媒体，不存在则 404 `media_not_found`。
    ///
    /// 上游 `require_by_id(Media, media_id, "media", error_message="Media not
    /// found")` —— 错误码由实体名生成，details 键是 `media_id`。
    async fn require_media(&self, media_id: i32) -> Result<sm_db::Media, ServiceError> {
        self.media.find_by_id(media_id).await?.ok_or_else(|| {
            ServiceError::not_found("media_not_found", "Media not found", "media_id", media_id)
        })
    }

    /// 把一批时刻点补上图片行，组装成 [`MediaPointValue`]。
    ///
    /// 一次 `find_by_ids` 批量取图，不是逐条 —— 一部长片几十个时刻点，
    /// 逐条就是 N+1。
    async fn points_with_image(
        &self,
        points: Vec<sm_db::playback::media::MediaPoint>,
    ) -> Result<Vec<MediaPointValue>, ServiceError> {
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let mut image_ids: Vec<i32> = points.iter().map(|point| point.image_id).collect();
        image_ids.sort_unstable();
        image_ids.dedup();
        let images = self.images.find_by_ids(&image_ids).await?;

        let mut out = Vec::with_capacity(points.len());
        for point in points {
            // `image_id` 是 NOT NULL 且外键 RESTRICT —— 查不到只可能是并发删图，
            // 那时跳过这一行比编一个空路径更诚实。
            let Some(image) = images.get(&point.image_id) else {
                continue;
            };
            out.push(MediaPointValue {
                point_id: point.id,
                media_id: point.media_id,
                thumbnail_id: point.thumbnail_id,
                offset_seconds: point.offset_seconds,
                image_id: image.id,
                image_origin: image.origin.clone(),
                created_at: point.created_at,
            });
        }
        Ok(out)
    }

    /// `GET /media` —— 分页列表。
    ///
    /// `kind` **有默认值**（`all`），而 `/media/duplicates` 的 `kind`
    /// **必填** —— 见 `routes/media.rs` 的说明。
    pub async fn list_media(&self, query: &MediaListQuery) -> Result<MediaListPage, ServiceError> {
        let page = query.page.unwrap_or(1);
        let page_size = query.page_size.unwrap_or(20);
        sm_core::pagination::validate_page(page, page_size)
            .map_err(|error| ServiceError::validation("invalid_media_filter", error.message()))?;
        let offset = sm_core::pagination::page_offset(page, page_size);

        // ★ `actor_ids` 是 CSV，要先解析成 id，再（带墓碑解析）换成番号。
        // 空名单由仓储处理成「命中不了任何媒体」，不退化成不过滤。
        let actor_numbers = match query.actor_ids.as_deref() {
            Some(csv) => {
                let ids: Result<Vec<i32>, _> = csv
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|part| part.parse::<i32>())
                    .collect();
                let ids = ids.map_err(|_| {
                    ServiceError::validation("invalid_media_filter", "actor_ids 必须是整数列表")
                })?;
                if ids.is_empty() {
                    Vec::new()
                } else {
                    MovieRepository::new(self.pool.clone())
                        .numbers_for_actor_ids(&ids)
                        .await?
                }
            }
            None => Vec::new(),
        };

        let order_sql = match resolve_sort(query.sort.as_deref())? {
            Some((column, desc)) => {
                // ★ `heat` 是唯一可空字段：NULLS LAST **不受排序方向影响**
                // （非 JAV 视频没有影片、heat 恒空，永远垫底）。
                let nullable = MEDIA_LIST_NULLABLE_SORT_FIELDS
                    .iter()
                    .any(|name| column.ends_with(name));
                format!(
                    "{} {}{}, m.id ASC",
                    column,
                    if desc { "DESC" } else { "ASC" },
                    if nullable { " NULLS LAST" } else { "" }
                )
            }
            None => "m.created_at DESC, m.id DESC".to_owned(),
        };

        let filter = MediaListFilter {
            kind: query.kind.as_deref(),
            library_id: query.library_id.map(|id| id as i32),
            movie_numbers: query.actor_ids.as_ref().map(|_| actor_numbers.as_slice()),
            thumbnail_generation_state: query.thumbnail_generation_state.as_deref(),
            require_valid: None,
            search: None,
            file_hashes: None,
        };
        let repo = &self.media;
        let total = repo.count_filtered(&filter).await?;
        let rows = repo
            .list_filtered(&filter, &order_sql, page_size, offset)
            .await?;
        let items = self.to_list_items(&rows).await?;
        Ok(MediaListPage {
            items,
            page,
            page_size,
            total,
        })
    }

    /// 一批媒体行 → 列表项。影片 id 按番号补（每页最多 100 条）。
    async fn to_list_items(
        &self,
        rows: &[sm_db::playback::media::Media],
    ) -> Result<Vec<MediaListItemResource>, ServiceError> {
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let movie = match row.movie_number.as_deref() {
                Some(number) => MovieRepository::new(self.pool.clone())
                    .find_by_number(number)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            items.push(MediaListItemResource {
                id: i64::from(row.id),
                library_id: i64::from(row.library_id),
                file_name: row.file_name.clone(),
                movie_id: movie.as_ref().map(|movie| i64::from(movie.id)),
                movie_number: row.movie_number.clone(),
                duration_seconds: i64::from(row.duration_seconds),
                resolution: row.resolution.clone(),
                file_size_bytes: row.file_size_bytes,
                valid: row.valid,
                created_at: row.created_at.map(|at| at.to_string()),
            });
        }
        Ok(items)
    }

    /// `GET /media/multi-version` —— 同番号多文件。
    pub async fn list_multi_version_movies(
        &self,
        page: i64,
        page_size: i64,
        include_vr: bool,
        include_fc2: bool,
    ) -> Result<serde_json::Value, ServiceError> {
        // ★ 只按番号分组，取回**这些番号下的全部媒体**（不分页）—— 上游
        // `media_service.py:345-359` 也是先取组、再一次性取回组内媒体，
        // 因为「一个番号有几个文件」在取组时还不知道。
        sm_core::pagination::validate_page(page, page_size)
            .map_err(|error| ServiceError::validation("invalid_media_filter", error.message()))?;
        let offset = sm_core::pagination::page_offset(page, page_size);
        let total = self
            .media
            .count_multi_version_movies(include_vr, include_fc2)
            .await?;
        let numbers = self
            .media
            .multi_version_movie_numbers(include_vr, include_fc2, page_size, offset)
            .await?;
        let filter = MediaListFilter {
            kind: None,
            library_id: None,
            movie_numbers: Some(numbers.as_slice()),
            thumbnail_generation_state: None,
            require_valid: None,
            search: None,
            file_hashes: None,
        };
        let rows = self
            .media
            .list_filtered(&filter, "m.created_at ASC, m.id ASC", i64::MAX, 0)
            .await?;

        // 按番号归堆，**保持分组查询返回的顺序**（上游按 `MAX(updated_at) DESC`）。
        let mut by_number: Vec<(String, Vec<MediaListItemResource>)> = Vec::new();
        let items = self.to_list_items(&rows).await?;
        for (row, item) in rows.iter().zip(items) {
            let Some(number) = row.movie_number.clone() else {
                continue;
            };
            match by_number.iter_mut().find(|(key, _)| *key == number) {
                Some((_, group)) => group.push(item),
                None => by_number.push((number, vec![item])),
            }
        }
        // ★ 上游只保留 `len(items) > 1` 的组（`:359`）：建组到取媒体之间可能
        // 有并发删除，那时这个番号只剩一个文件，不该再算「多版本」。
        let mut groups = Vec::new();
        for (number, media) in by_number {
            if media.len() <= 1 {
                continue;
            }
            let movie_id = MovieRepository::new(self.pool.clone())
                .find_by_number(&number)
                .await
                .ok()
                .flatten()
                .map(|movie| i64::from(movie.id));
            groups.push(serde_json::json!({
                "movie_number": number,
                "media_count": media.len(),
                "media_items": media,
                "movie_id": movie_id,
            }));
        }
        Ok(serde_json::json!({
            "items": groups,
            "page": page,
            "page_size": page_size,
            "total": total,
        }))
    }

    /// ★ `GET /media/duplicates` —— 按哈希分组。**`kind` 必填**。
    ///
    /// 哈希缺失的媒体**单独成一组**并标 [`DuplicateKeyKind::Degraded`] ——
    /// 把它们混进哈希组会给出错误的「重复」判定。
    pub async fn list_duplicate_media_groups(
        &self,
        media_kind: &str,
        page: i64,
        page_size: i64,
    ) -> Result<serde_json::Value, ServiceError> {
        sm_core::pagination::validate_page(page, page_size)
            .map_err(|error| ServiceError::validation("invalid_media_filter", error.message()))?;
        if !matches!(media_kind, "jav" | "video") {
            return Err(ServiceError::validation(
                "invalid_media_filter",
                "kind 只能是 jav 或 video",
            ));
        }
        let offset = sm_core::pagination::page_offset(page, page_size);
        let kind = Some(media_kind);
        let total = self.media.count_duplicate_hash_groups(kind).await?;
        let hashes = self
            .media
            .duplicate_hash_groups(kind, page_size, offset)
            .await?;
        if hashes.is_empty() {
            return Ok(serde_json::json!({
                "items": [],
                "page": page,
                "page_size": page_size,
                "total": total,
            }));
        }
        let filter = MediaListFilter {
            kind,
            file_hashes: Some(hashes.as_slice()),
            ..Default::default()
        };
        let rows = self
            .media
            // 组内排序：哈希升序 → 入库时间升序 → id 升序（上游 `:413`）。
            .list_filtered(
                &filter,
                "m.file_hash ASC, m.created_at ASC, m.id ASC",
                // 一次取回这些哈希下的**全部**媒体：组内条数在取组时未知。
                i64::MAX,
                0,
            )
            .await?;
        let items = self.to_list_items(&rows).await?;

        // 按哈希归堆，**保持取组的顺序**（`total` 与页序都按那个顺序）。
        let mut groups = Vec::new();
        for hash in &hashes {
            let media: Vec<MediaListItemResource> = rows
                .iter()
                .zip(items.iter())
                .filter(|(row, _)| row.file_hash.as_deref() == Some(hash.as_str()))
                .map(|(_, item)| item.clone())
                .collect();
            if media.is_empty() {
                // 取组与取媒体之间被并发删空 —— 跳过，不返回一个空组。
                continue;
            }
            groups.push(serde_json::json!({
                "dedup_key": hash,
                // ⚠️ 目前**只会是** `hash`：本仓 `DuplicateKeyKind::Degraded`
                // （按「文件名+大小」猜重复）上游没有对应的分组查询，尚未启用。
                "key_kind": "hash",
                "kind": media_kind,
                "media_count": media.len(),
                "media_items": media,
            }));
        }
        Ok(serde_json::json!({
            "items": groups,
            "page": page,
            "page_size": page_size,
            "total": total,
        }))
    }

    /// `GET /media-points` —— 时刻列表。`kind` 默认 `jav`。
    pub async fn list_media_points(
        &self,
        page: i64,
        page_size: i64,
        sort: Option<&str>,
        media_kind: Option<&str>,
        keyword: Option<&str>,
        exclude_collection_id: Option<i64>,
    ) -> Result<serde_json::Value, ServiceError> {
        sm_core::pagination::validate_page(page, page_size).map_err(|error| {
            ServiceError::validation("invalid_media_point_filter", error.message())
        })?;
        let offset = sm_core::pagination::page_offset(page, page_size);
        // ★ 上游 `MEDIA_POINT_SORT_FIELDS` **只有两种取值**
        // （`media_service.py:89-92`）：`created_at:desc` / `created_at:asc`，
        // 默认 desc。归一化是 `strip().lower()`（`resolve_sort`），所以
        // `CREATED_AT:ASC` 合法；空串走默认。
        //
        // ⚠️ 骨架期这里还接受裸 `asc` / `desc` —— 上游**不认**这两个字面量，
        // 收了等于把非法输入当合法（契约被放宽，客户端会依赖它）。
        let order_sql = match sort.map(str::trim).filter(|raw| !raw.is_empty()) {
            None => "p.created_at DESC, p.id DESC",
            Some(raw) => match raw.to_ascii_lowercase().as_str() {
                "created_at:desc" => "p.created_at DESC, p.id DESC",
                "created_at:asc" => "p.created_at ASC, p.id ASC",
                _ => {
                    return Err(ServiceError::validation_with(
                        "invalid_media_point_filter",
                        "Invalid sort expression",
                        details_of("sort", raw),
                    ))
                }
            },
        };
        // `kind` 默认 `jav`（上游签名），`all` 表示不限。
        let kind = media_kind.unwrap_or("jav");
        let kind = if kind == "all" { None } else { Some(kind) };
        let exclude_collection_id = exclude_collection_id.map(|id| id as i32);

        let total = self
            .points
            .count_filtered(kind, keyword, exclude_collection_id)
            .await?;
        let points = self
            .points
            .list_filtered(
                kind,
                keyword,
                exclude_collection_id,
                order_sql,
                page_size,
                offset,
            )
            .await?;

        // 图片批量取回（与 [`Self::list_points`] 同一个模式，避免逐行 N+1）。
        let image_ids: Vec<i32> = {
            let mut ids: Vec<i32> = points.iter().map(|point| point.image_id).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let images = self.images.find_by_ids(&image_ids).await?;

        let mut items = Vec::with_capacity(points.len());
        for point in &points {
            // `image_id` NOT NULL 且外键 RESTRICT —— 查不到只可能是并发删图。
            let Some(image) = images.get(&point.image_id) else {
                continue;
            };
            items.push(serde_json::json!({
                "point_id": point.id,
                "media_id": point.media_id,
                "movie_number": point.movie_number,
                "video_item_id": point.video_item_id,
                "thumbnail_id": point.thumbnail_id,
                "offset_seconds": point.offset_seconds,
                // ★ 与 [`MediaPointValue`] 同一约定：这一层给 **id 与未签名路径**，
                // 由 API 层组装成可用的 `ImageResource`。
                "image_id": image.id,
                "image_origin": image.origin,
                "created_at": point.created_at.map(|at| at.to_string()),
            }));
        }
        Ok(serde_json::json!({
            "items": items,
            "page": page,
            "page_size": page_size,
            "total": total,
        }))
    }

    /// 某媒体的全部时刻。**按 `id` 升序**。
    ///
    /// ⚠️ 骨架期这里写的是「按 `(offset, id)` 升序」—— **没有依据**。上游
    /// `MediaService.list_points` 是 `.order_by(MediaPoint.id)`，而按时刻排的
    /// 是另一个端点（`GET /media-points` 的全局列表，默认 `created_at` 降序）。
    ///
    /// 顺序是客户端可见的（点位列表的展示次序），所以不能「看着更合理」就改。
    pub async fn list_points(&self, media_id: i32) -> Result<Vec<MediaPointValue>, ServiceError> {
        self.require_media(media_id).await?;
        let points = self.points.list_all_by_media(media_id).await?;
        self.points_with_image(points).await
    }

    /// 新建时刻。返回 `(资源, 是否新建)`。
    ///
    /// # 入参只有 `thumbnail_id`，**没有 `offset_seconds`**
    ///
    /// 骨架期这里的签名是 `(media_id, offset_seconds, thumbnail_id: Option<..>)`
    /// —— 两处都不对。上游 `MediaPointCreateRequest` **只有一个必填字段**
    /// `thumbnail_id`（`gt=0`），而时刻的秒偏移取自 `thumbnail.offset`：
    ///
    /// ```python
    /// point = MediaPoint.create(
    ///     media=media, thumbnail=thumbnail, image=thumbnail.image_id,
    ///     movie_number=media.movie_number, video_item_id=media.video_item_id,
    ///     offset_seconds=thumbnail.offset,
    /// )
    /// ```
    ///
    /// 让客户端同时给缩略图与偏移会有两个真相源（两者不一致时以谁为准？），
    /// 所以上游干脆只收缩略图。
    ///
    /// # 幂等判据是 `(media, thumbnail)`，**不是** `(media, offset, thumbnail)`
    ///
    /// 骨架期文档写的是后者。同一张缩略图在同一媒体上只会有一个时刻点 ——
    /// 命中就返回既有那条并报 `false`（路由据此决定 200 而不是 201）。
    pub async fn create_point(
        &self,
        media_id: i32,
        thumbnail_id: i32,
    ) -> Result<(MediaPointValue, bool), ServiceError> {
        // 上游 `Field(gt=0)` + validator：非正值在 pydantic 层就是 422，到不了
        // 查询。Rust 侧没有那一层，所以落在这里 —— 契约不变，只是拦在哪一层。
        if thumbnail_id <= 0 {
            return Err(ServiceError::validation(
                "validation_error",
                "thumbnail_id must be greater than 0",
            ));
        }
        let media = self.require_media(media_id).await?;
        let thumbnail = self
            .require_thumbnail_for_media(&media, thumbnail_id)
            .await?;

        // 先查后插。表上没有唯一索引兜底（见仓储的 `insert` 文档），所以并发
        // 下理论上可能建出两条 —— 上游同样是先查后插，没有更紧的做法。
        if let Some(existing) = self
            .points
            .find_by_media_and_thumbnail(media_id, thumbnail_id)
            .await?
        {
            let mut values = self.points_with_image(vec![existing]).await?;
            let value = values.pop().ok_or_else(|| {
                // 既有行的图片行没了：外键 RESTRICT，只可能是并发删图。
                ServiceError::from(sm_db::DbError::business(
                    "MediaPoint",
                    "时刻点的图片行已不存在",
                ))
            })?;
            return Ok((value, false));
        }

        let point = self
            .points
            .insert(
                Some(media_id),
                Some(thumbnail_id),
                thumbnail.image_id,
                media.movie_number.as_deref(),
                media.video_item_id,
                thumbnail.offset,
            )
            .await?;
        let mut values = self.points_with_image(vec![point]).await?;
        let value = values.pop().ok_or_else(|| {
            ServiceError::from(sm_db::DbError::business(
                "MediaPoint",
                "时刻点插入后取不到图片行",
            ))
        })?;
        Ok((value, true))
    }

    /// 取缩略图并确认它属于该媒体。否则 404 `media_thumbnail_not_found`。
    ///
    /// details 带**两个**键（`media_id` + `thumbnail_id`）：客户端要能区分
    /// 「这张缩略图不存在」与「它属于别的媒体」—— 虽然上游把两者合并成同一个
    /// 404（同 `video_item_service._require_thumbnail_for_media`）。
    async fn require_thumbnail_for_media(
        &self,
        media: &sm_db::Media,
        thumbnail_id: i32,
    ) -> Result<sm_db::playback::media::MediaThumbnail, ServiceError> {
        let not_found = || {
            ServiceError::not_found_with(
                "media_thumbnail_not_found",
                "Media thumbnail not found",
                details_of("media_id", media.id)
                    .into_iter()
                    .chain(details_of("thumbnail_id", thumbnail_id))
                    .collect(),
            )
        };
        let Some(thumbnail) = self.thumbnails.find_by_id(thumbnail_id).await? else {
            return Err(not_found());
        };
        if thumbnail.media_id != media.id {
            return Err(not_found());
        }
        Ok(thumbnail)
    }

    /// 删时刻。**连带清掉那张只服务于它的图**（记录 + 磁盘文件）。
    ///
    /// 顺序照上游：先确认归属（404），再走 [`Self::delete_point_by_id`]。
    /// 「先删点、后清图」是**必须的次序** —— 反过来的话 `is_referenced` 会因为
    /// 这个点还在而返回「有人用」，于是图永远清不掉（而点已经没了）。
    pub async fn delete_point(&self, media_id: i32, point_id: i32) -> Result<(), ServiceError> {
        self.require_media(media_id).await?;
        // 归属校验：上游 `_require_media_point_for_media` 是
        // `WHERE id = ? AND media_id = ?` 一条查询。这里分两步（取行 + 比
        // 归属）是为了能复用同一份 `media_point_not_found` 构造。
        let Some(point) = self.points.find_by_id(point_id).await? else {
            return Err(point_not_found_for_media(media_id, point_id));
        };
        if point.media_id != Some(media_id) {
            return Err(point_not_found_for_media(media_id, point_id));
        }
        self.delete_point_by_id(point_id).await
    }

    /// 删时刻（**不**校验媒体归属）。供跨域调用。
    ///
    /// 上游 `delete_point_by_id`：删行 -> `ImageCleanupService` 清掉那张已无人
    /// 引用的图 -> 清磁盘文件。
    ///
    /// # 三件事不在同一个事务里（上游在，这是**有意的偏离**）
    ///
    /// 上游把「删行 + 判图片是否还被引用 + 删图片记录」放在一个事务里，文件删除
    /// 在事务外。这里「删行」与「清图」是两个事务 —— 但**不会出错**：
    ///
    /// - 先删行、后判断引用：中间若别处引用了这张图，判断就返回「有人用」，
    ///   于是**不删**（安全）；
    /// - 而「判断 + 删记录」本身仍在同一个事务里，且带 `FOR UPDATE`
    ///   （见 `ImageRepository::delete_if_unreferenced`）。
    ///
    /// 中间崩溃最坏的结果是「图成了孤儿」——磁盘浪费，可恢复。
    pub async fn delete_point_by_id(&self, point_id: i32) -> Result<(), ServiceError> {
        let Some(point) = self.points.find_by_id(point_id).await? else {
            // 上游 `require_by_id(MediaPoint, point_id, "media_point")`：码与文案
            // 由实体名生成，details 键默认是 `{实体名}_id`。
            return Err(ServiceError::not_found(
                "media_point_not_found",
                "media_point not found",
                "media_point_id",
                point_id,
            ));
        };
        self.points.delete(point_id).await?;

        let cleanup = ImageCleanupService::new(&self.pool, &self.config);
        let obsolete = cleanup
            .delete_image_record_if_unused(Some(point.image_id))
            .await?;
        cleanup.delete_obsolete_image_files(&obsolete).await?;
        Ok(())
    }

    /// 更新播放进度。**UPSERT**，允许倒退。
    ///
    /// # 上游会在 JAV 媒体上顺带维护「最近播放」列表
    ///
    /// ```python
    /// if media.movie_number:
    ///     PlaylistService.touch_recently_played(media.movie)
    /// ```
    ///
    /// 注释写明这是「JAV 影片维度的能力，非 JAV 媒体跳过维护」。所以：
    ///
    /// - 判据是 `movie_number` 非空（而非「有没有 media 行」）；
    /// - `touch_recently_played` 收的是 **`Movie.id`**，而 `media` 表存的是
    ///   番号**字符串**（`movie_number` 指向 `movie.movie_number`，不是 id）——
    ///   所以要先按番号查 `movie` 拿 id。查不到就跳过：那说明影片记录还没落地，
    ///   而进度本身已经写成功了，不该因此失败。
    ///
    /// # 允许倒退是**刻意**的
    ///
    /// 用户拖回去重看是常态。「只许前进」会让这类操作看起来成功却没生效，
    /// 比倒退本身更糟 —— 仓储层 `save` 的文档写明了这条。
    pub async fn update_progress(
        &self,
        media_id: i32,
        position_seconds: i32,
    ) -> Result<MediaProgressValue, ServiceError> {
        let media = self.require_media(media_id).await?;
        // 负值由仓储挡下（它返回业务错误），但那是 500 口径；上游 pydantic 是
        // 422。这里先拦成 422，与 `MediaProgressUpdateRequest(ge=0)` 一致。
        if position_seconds < 0 {
            return Err(ServiceError::validation(
                "validation_error",
                "position_seconds cannot be negative",
            ));
        }
        let progress = self.progress.save(media_id, position_seconds).await?;

        if let Some(movie_number) = media.movie_number.as_deref() {
            let movies = sm_db::repo::MovieRepository::new(self.pool.clone());
            if let Some(movie) = movies.find_by_number(movie_number).await? {
                crate::collections::playlist::PlaylistService::new(&self.pool)
                    .touch_recently_played(movie.id)
                    .await?;
            }
        }

        Ok(MediaProgressValue {
            media_id: media.id,
            last_position_seconds: progress.position_seconds,
            last_watched_at: progress.last_watched_at,
        })
    }

    /// ★ 删媒体。**三步各自独立**，见模块文档。
    ///
    /// ```text
    ///   1. provider 删物理文件
    ///   2. 清 Qdrant 缩略图向量（仅 JAV + 图搜启用）
    ///   3. 删 DB 记录 + 回收缩略图图片（记录 + 磁盘文件）
    /// ```
    ///
    /// 错误码：媒体不存在 → `404 media_not_found`；锁不到 → `409
    /// media_operation_busy`；provider 报错 → `provider_{code}`
    /// （`provider_not_installed` 是 503）。
    ///
    /// # ⚠️ 运行约束：连接池**至少两条连接**
    ///
    /// 这条链路会先取一条**会话级** advisory lock 并**持有它**去做后续的库操作
    /// （查媒体、问 provider、删行、回收图片）。而那条锁自己占着一条连接 ——
    /// 池里只剩 0 条时，后续每个查询都会在 `acquire_timeout` 后报
    /// `pool timed out while waiting for an open connection`，表现为 **500**，
    /// 而错误信息里看不出是池的问题。
    ///
    /// 生产默认 20 条（`sm_server::config`），余量充足；这条是给**测试**与
    /// 自建小池部署看的 —— 与 `sm_db::common::advisory_lock` 模块文档里那条
    /// 「需要连接数 = 同时持有的锁数 + 1」是同一件事。
    /// `TestDb` 的池是 `max_connections(1)`，所以删除类用例要用
    /// `pool_with_max_connections(2)`；这条约束第一次就是这么暴露的。
    ///
    /// # `sync_video_member` 那一支**在取锁之前**就委托出去
    ///
    /// 上游是「先锁住 M，看到它属于条目 V，就
    /// `VideoItemService.delete_video(V)` 然后 `return`」；那条链路会把 V 名下
    /// 的媒体**逐条**再删一遍（M 也在其中，传 `sync_video_member=False`
    /// 避免递归回本方法）。
    ///
    /// 在 Python 里，同一个 session 重入同一把 advisory lock 是**成功**的
    /// （会话级锁可重入）。本仓不行：
    /// [`AdvisoryLock`](sm_db::common::advisory_lock::AdvisoryLock) 每条锁
    /// **独占一条池连接**，重入就是拿**另一条连接**去取同一个 key ——
    /// `pg_try_advisory_lock` 必然失败，于是「删一个属于条目的媒体」会莫名
    /// 报 `409 media_operation_busy`，而拿不到锁的那一方看不出任何原因。
    ///
    /// 所以判据提前到取锁之前：要委托就整条交给条目链路，那条链路会给它碰到
    /// 的每一条媒体**各取一次锁**（包括 M）。
    ///
    /// ⚠️ 与上游的差别只有这一处：「读 M」到「条目链路锁住 M」之间没有持有 M
    /// 的锁。上游在纸面上更严，代价却是一个必然发生的 409（见上）；而条目链路
    /// 删到 M 时仍会先锁住它，所以那段窗口里没有别的操作能改动它。
    ///
    /// # 必须先记下缩略图的 `image_id`
    ///
    /// 删 `media` 行会把 `media_thumbnail` 行一起带走（`CASCADE`），而那些行里
    /// 的 `image_id` 是**回收图片的唯一线索**。所以顺序是「先收集 → 再删」，
    /// 反了就是孤儿图片 —— 不报错，只是磁盘永远不回收。
    ///
    /// # ⚠️ 第 2 步本轮**没做**
    ///
    /// 本仓还没有 Qdrant 客户端（`qdrant.url` 只是配置项，还没有实际调用），
    /// 所以这一支跳过。上游它是 `try/except` + 记 warning 的 best-effort 步骤，
    /// 跳过与它「删除失败」的最终后果一致（留下孤儿向量），但**没有那条
    /// warning 日志** —— 补 Qdrant 客户端时要一起补上。
    pub async fn delete_media(
        &self,
        media_id: i64,
        sync_video_member: bool,
    ) -> Result<(), ServiceError> {
        // 超出 i32 的 id 按「不存在」处理：上游 Python 的 `int` 没有上界，
        // 那种 id 会一路查不到然后报 404，而不是 400。
        let media_id = i32::try_from(media_id).map_err(|_| {
            ServiceError::not_found_with(
                "media_not_found",
                "Media not found",
                details_of("media_id", media_id),
            )
        })?;

        // 要委托给条目删除链路的话，**在取锁之前**就转出去（理由见本方法的
        // 文档）：那条链路会给条目下的每条媒体各取一次锁，包括这一条。
        if sync_video_member {
            let media = self.require_media(media_id).await?;
            if let Some(video_item_id) = media.video_item_id {
                return self.delete_video_item(video_item_id).await;
            }
        }

        self.delete_media_with_lock(media_id).await
    }

    /// 取锁 + 删一条媒体。**不判断要不要委托条目** —— 条目删除链路用它。
    ///
    /// # 为什么不复用 `delete_media`
    ///
    /// 两个理由，第二个是硬约束：
    ///
    /// 1. 语义上，条目链路**已经知道**自己在删一个条目的媒体，不需要再问一次；
    /// 2. `delete_media` →（委托）→ `VideoItemService::delete` →（逐条）→
    ///    `delete_media` 是一个**静态的互相递归**，而 `async fn` 的递归必须
    ///    装箱：不在这里断开，编译期就会撞上
    ///    `E0733: recursion in an async fn requires boxing`。装箱（`Box::pin`）
    ///    能让它编过，但那是给编译器交保护费，不如把「这里不会委托」这件事
    ///    写进类型。
    pub(crate) async fn delete_media_with_lock(&self, media_id: i32) -> Result<(), ServiceError> {
        // 媒体级锁。**锁不到是 409，不是等待** —— 上游
        // `media_operation_lock` 就是 `pg_try_advisory_lock` + 409
        // `media_operation_busy`（另一处正在动它，比如生成缩略图）。
        //
        // 锁在**读之前**拿：下面的 `require_media` 读到的那一行，到删它为止
        // 都不该被别人改动。
        let lock = sm_db::common::advisory_lock::AdvisoryLock::try_acquire(
            &self.pool,
            sm_db::common::advisory_lock::namespace::MEDIA,
            media_id,
        )
        .await?
        .ok_or_else(|| {
            ServiceError::conflict("media_operation_busy", "媒体正在处理，请稍后重试", None)
        })?;

        let outcome = self.delete_one_media(media_id).await;

        // 显式解锁：正常路径把连接**干净地还回池**；不释放的话 `Drop` 会把连接
        // 从池里摘掉（那是异常路径的兜底，见 `AdvisoryLock` 的模块文档）。
        lock.release().await;
        outcome
    }

    /// 删条目及其全部媒体（上游 `VideoItemService.delete_video`）。
    ///
    /// 编排在 [`crate::videos::item::VideoItemService::delete`] —— 那是上游放
    /// 这条链路的地方（`video_item_service.py`）。这里只是把它接起来，因为
    /// 那条链路要本服务的 db / config / provider 网关。
    ///
    /// ⚠️ **不取条目级的锁** —— 与上游一致：条目下每条媒体由那条链路自己逐个
    /// 取锁。所以本方法也不该在已持有某条媒体锁的情况下被调用（那正是
    /// `delete_media` 把委托提前到取锁之前的原因）。
    pub async fn delete_video_item(&self, video_item_id: i32) -> Result<(), ServiceError> {
        crate::videos::item::VideoItemService::new(&self.pool)
            .delete(video_item_id, self)
            .await
    }

    /// 单条媒体的实际清理。**调用方必须已持有它的锁**（`delete_media` 拿锁，
    /// 条目链路里的那一条由 `VideoItemService::delete` 通过 `delete_media` 拿）。
    ///
    /// 拆出来是因为 `delete_media` 的那段「先判断要不要委托」必须在取锁**之前**
    /// 跑：如果整个函数体只有一份、锁又在外层拿，就没有地方安放那段判断。
    async fn delete_one_media(&self, media_id: i32) -> Result<(), ServiceError> {
        let media = self.require_media(media_id).await?;

        // ── 1. provider 删远端文件 ──────────────────────────────────
        //
        // 没有 provider 就 503，**不跳过**：跳过会留下远端文件，而调用方以为
        // 删干净了，下一次扫描又把它扫回来（表现为「删了又出现」）。
        let Some(gateway) = self.gateway.as_deref() else {
            return Err(ServiceError::unavailable(
                "provider_not_installed",
                "媒体提供方未安装",
            ));
        };
        let library = sm_db::repo::MediaLibraryRepository::new(self.pool.clone())
            .find_by_id(media.library_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_library_not_found",
                    "Media library not found",
                    "library_id",
                    media.library_id,
                )
            })?;
        let handle = provider_helpers::media_handle_for(&provider_helpers::MediaRecord {
            id: i64::from(media.id),
            library_id: i64::from(media.library_id),
            storage_ref: json_or_null(media.storage_ref.as_deref()),
            provider_config: json_or_null(library.provider_config.as_deref()),
            provider_key: library.provider_key.clone(),
            account_key: library.account_key.clone(),
            file_name: media.file_name.clone(),
            file_size_bytes: media.file_size_bytes,
            duration_seconds: media.duration_seconds,
        });
        // `source_not_found` = provider 确认远端早已不在 → **继续**清本地。
        // 其余按上游那张表映射成状态码。
        if let Err(failure) = gateway.delete_media(&handle).await {
            match failure.code.as_str() {
                "source_not_found" => {}
                "authentication_failed" | "unavailable" | "invalid_config" | "unsupported" => {
                    return Err(Self::map_provider_failure(&failure));
                }
                // 表里没有的码（含 `provider_not_installed`）。认不出的必须报成
                // **5xx**，**不许**伪装成上面某个已知码 —— 那会把「插件崩了」
                // 说成「你的配置不对」，排查方向会被带偏。
                _ => {
                    return Err(ServiceError::bad_gateway(
                        format!("provider_{}", failure.code),
                        failure.safe_message,
                        details_of("provider_key", handle.provider_key.as_str()),
                    ))
                }
            }
        }

        // ── 2. Qdrant 向量 ─────────────────────────────────────────
        // 仅 JAV 媒体的缩略图会进向量库（非 JAV 落 SKIPPED 从不入库），所以
        // `movie_number` 是「跳过空删省一次远端往返」的判据。
        // ⚠️ 见文档：本仓还没有 Qdrant 客户端，这一支尚未实现。

        // ── 3. 删记录 + 回收图片 ────────────────────────────────────
        //
        // 缩略图的 `image_id` **先收集再删行**：删 `media` 会把
        // `media_thumbnail` 一起 CASCADE 掉，而那些行的 `image_id` 是回收
        // 图片的唯一线索。
        let image_ids: Vec<i32> = self
            .thumbnails
            .list_all_by_media(media_id)
            .await?
            .into_iter()
            .map(|thumbnail| thumbnail.image_id)
            .collect();
        self.media.delete(media_id).await?;

        // 条目的封面图**不在这里收**：它属于条目，由
        // `VideoItemService::delete` 在删掉条目行之后回收（那时它才「不再被
        // 引用」）。这里顺手删会把它从还活着的条目上摘掉。
        self.reap_images(image_ids).await
    }

    /// 回收这批图片：**不再被引用**的删掉记录，删掉记录的那些再删磁盘文件。
    ///
    /// ⚠️ **必须在引用它们的行删掉之后调** ——
    /// [`ImageRepository::delete_if_unreferenced`](sm_db::repo::ImageRepository)
    /// 是按引用判据的，引用还在时它什么都不做（不报错）。
    ///
    /// 媒体缩略图与条目封面共用这一段：两者的「回收」是同一件事，各写一份就会
    /// 在「哪些情况算不再被引用」上分叉。
    pub(crate) async fn reap_images(&self, image_ids: Vec<i32>) -> Result<(), ServiceError> {
        let cleaner = ImageCleanupService::new(&self.pool, &self.config);
        let mut obsolete: Vec<String> = Vec::new();
        for image_id in image_ids {
            // 仍在被引用的图不会被删（胶片是否能回收由 `ImageCleanupService`
            // 的那张引用方清单决定），所以这里**不去重**：同一个 id 被多个
            // 缩略图指着，第二轮自然返回空。
            obsolete.extend(
                cleaner
                    .delete_image_record_if_unused(Some(image_id))
                    .await?,
            );
        }
        obsolete.sort();
        obsolete.dedup();
        // 磁盘文件最后删，且**失败不回滚**（记录已经删了）—— 上游同样不回滚：
        // 这里的失败是「磁盘冗余」，而回滚会试图复活一条已删的记录。
        if let Err(error) = cleaner.delete_obsolete_image_files(&obsolete).await {
            tracing::warn!(
                orphan_files = obsolete.len(),
                code = error.code(),
                "回收图片文件失败，已留下孤儿文件"
            );
        }
        Ok(())
    }

    /// provider 失败码 → HTTP 状态码。**与上游 `delete_media` 那张表逐条对应**：
    ///
    /// | provider 码 | 状态 | 上游 `media_service.py:655` |
    /// |---|---|---|
    /// | `authentication_failed` | 401 | 凭据过期了，重试没用 |
    /// | `unavailable` | 503 | 网盘挂了，值得稍后再试 |
    /// | `invalid_config` / `unsupported` | 422 | 配错了 / 这个 provider 不支持删 |
    ///
    /// 对外错误码一律 **`provider_{code}`**（上游 `media_service.py:663`），
    /// 与 `provider_not_installed` 同一个前缀 —— 前端靠这个前缀分流。
    ///
    /// 表里没有的码**不走这里**：认不出的失败要报成 5xx，硬塞进这张表会把
    /// 「插件崩了」说成「你的配置不对」。
    fn map_provider_failure(failure: &ProviderFailure) -> ServiceError {
        let code = format!("provider_{}", failure.code);
        match failure.code.as_str() {
            "authentication_failed" => ServiceError::from_status(401, code, &failure.safe_message),
            "unavailable" => ServiceError::from_status(503, code, &failure.safe_message),
            _ => ServiceError::from_status(422, code, &failure.safe_message),
        }
    }

    /// 某媒体的缩略图。**按 `(offset, id)` 升序**。
    ///
    /// 先 `require_media`（404）再转调 —— 上游同款：媒体不存在时不该返回一个
    /// 空列表，那与「存在但没有缩略图」不可区分。
    pub async fn list_thumbnails(
        &self,
        media_id: i32,
    ) -> Result<Vec<super::thumbnails::artifacts::MediaThumbnailValue>, ServiceError> {
        self.require_media(media_id).await?;
        super::thumbnails::artifacts::ThumbnailArtifactService::new(&self.pool, &self.config)
            .list_media_thumbnails(media_id)
            .await
    }

    /// `GET /media/invalid` —— 已失效媒体列表。
    pub async fn list_invalid_media(
        &self,
        page: i64,
        page_size: i64,
        search: Option<&str>,
    ) -> Result<serde_json::Value, ServiceError> {
        sm_core::pagination::validate_page(page, page_size)
            .map_err(|error| ServiceError::validation("invalid_media_filter", error.message()))?;
        let offset = sm_core::pagination::page_offset(page, page_size);
        let filter = MediaListFilter {
            require_valid: Some(false),
            search,
            ..Default::default()
        };
        let total = self.media.count_filtered(&filter).await?;
        let rows = self
            .media
            // 上游排序：`updated_at DESC, id DESC`（`:775`）—— 失效媒体按
            // **最近变动**在前，用户最关心刚坏掉的那些。
            .list_filtered(&filter, "m.updated_at DESC, m.id DESC", page_size, offset)
            .await?;
        let items = self.to_list_items(&rows).await?;
        Ok(serde_json::json!({
            "items": items,
            "page": page,
            "page_size": page_size,
            "total": total,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: &str) -> ProviderFailure {
        ProviderFailure {
            code: code.to_owned(),
            safe_message: "provider said no".to_owned(),
            retryable: false,
        }
    }

    /// ★ 状态码与上游 `media_service.py:655` 那张表**逐条**对应。
    ///
    /// 401 与 503 的区分是**给客户端的行动指令**：401 重试没用（凭据过期），
    /// 503 值得稍后再试（网盘挂了）。一律 503 会让前端把所有 provider 故障
    /// 都显示成「稍后再试」。
    #[test]
    fn provider_failures_keep_the_upstream_status_table() {
        assert_eq!(
            MediaService::map_provider_failure(&failure("authentication_failed")).status,
            401
        );
        assert_eq!(
            MediaService::map_provider_failure(&failure("unavailable")).status,
            503
        );
        for code in ["invalid_config", "unsupported"] {
            assert_eq!(
                MediaService::map_provider_failure(&failure(code)).status,
                422,
                "{code} 是 422"
            );
        }
    }

    /// ★ 对外错误码必须是 `provider_{code}`（上游 `media_service.py:663`）。
    ///
    /// 前端靠这个前缀把「provider 出错」和其它 4xx/5xx 分开；少了前缀，客户端
    /// 会把「网盘挂了」当成服务器内部错误。
    #[test]
    fn provider_failure_codes_are_prefixed() {
        let mapped = MediaService::map_provider_failure(&failure("unavailable"));
        assert_eq!(mapped.code(), "provider_unavailable");
    }

    /// ★ 认不出的码**不许**被这张表静默吸收成 422。
    ///
    /// 硬塞进去会把「插件崩了」说成「你的配置不对」，排查方向直接偏掉。所以
    /// `delete_media` 只对那四个已知码调它，其余走 5xx 分支 —— 这条用例盯的是
    /// 「前缀照加、表名照给」，别把外部可见的码形改掉。
    #[test]
    fn a_failure_code_is_never_rewritten() {
        let mapped = MediaService::map_provider_failure(&failure("invalid_config"));
        assert_eq!(mapped.code(), "provider_invalid_config");
        assert_eq!(
            mapped.code().strip_prefix("provider_"),
            Some("invalid_config")
        );
    }

    /// ★ 排序字段是**白名单**，自由字符串 → 422（不夹到默认值）。
    ///
    /// 形状是上游的 **`field:direction`**（不是 `-field`），且归一化是
    /// `strip().lower()` —— `HEAT:DESC` 与 `heat:desc` 等价。
    #[test]
    fn sort_fields_come_from_the_allow_list() {
        assert_eq!(resolve_sort(None).expect("缺省不排序"), None);
        assert_eq!(resolve_sort(Some("  ")).expect("空串不排序"), None);
        let (column, desc) = resolve_sort(Some("heat:desc"))
            .expect("heat 可排")
            .expect("有值");
        assert_eq!(column, "mv.heat");
        assert!(desc, "`desc` = 降序");
        // 归一化：大写也认。
        assert_eq!(
            resolve_sort(Some("FILE_SIZE_BYTES:ASC"))
                .expect("大写合法")
                .map(|(column, _)| column),
            Some("m.file_size_bytes")
        );
        // ★ 骨架期的 `-heat` 约定**已废除**：它现在必须是非法值。
        assert!(resolve_sort(Some("-heat")).is_err());
        // 方向只能是 asc / desc。
        assert!(resolve_sort(Some("heat:sideways")).is_err());
        let error = resolve_sort(Some("id; DROP TABLE media")).expect_err("注入应被拒");
        assert_eq!(error.code(), "invalid_media_filter");
    }

    /// ★ `heat` 是**唯一可空**的排序字段 —— 排序必须显式 `NULLS LAST`。
    ///
    /// Postgres 对 `DESC` 的默认是 `NULLS FIRST`，那会让「按热度降序」变成
    /// 「没热度的排最前」，而没热度的恰恰是最不该被优先展示的。
    #[test]
    fn heat_is_the_only_nullable_sort_field() {
        assert_eq!(MEDIA_LIST_NULLABLE_SORT_FIELDS, ["heat"]);
        assert!(MEDIA_LIST_SORT_FIELD_MAP
            .iter()
            .any(|(name, _)| *name == "heat"));
        // created_at 等都不可空。
        for (name, _) in MEDIA_LIST_SORT_FIELD_MAP.iter() {
            if *name != "heat" {
                assert!(
                    !MEDIA_LIST_NULLABLE_SORT_FIELDS.contains(name),
                    "{name} 不该被当成可空字段"
                );
            }
        }
    }

    /// ★ 退化分组键**必须标出来** —— 客户端要能告诉用户「这批是猜的」。
    #[test]
    fn a_degraded_dedup_key_is_labelled() {
        let group = DuplicateMediaGroupResource {
            dedup_key: "movie.mkv|1048576".to_owned(),
            key_kind: DuplicateKeyKind::Degraded,
            media: Vec::new(),
        };
        assert_eq!(group.key_kind, DuplicateKeyKind::Degraded);
        assert_ne!(group.key_kind, DuplicateKeyKind::Hash);
    }

    /// ★ 缩略图状态字面量**只有一个来源**：`sm-db` 的文本常量。
    ///
    /// ⚠️ 骨架期本模块另有一个 i32 版本的 `thumbnail_state`（0/1/2/3），与库里
    /// 实际存的**文本列**对不上 —— 自造的第二份定义。它已被删除，`/media` 的
    /// `thumbnail_generation_state` 过滤直接吃 `sm_db` 那四个字面量。
    ///
    /// 🔴 这个测试盯着的是**「别再长出第二份」**：谁再往本模块加 `thumbnail_state`，
    /// 这个引用就会指向它而不是 `sm-db`，值一变即失败。
    #[test]
    fn thumbnail_states_have_a_single_source() {
        use sm_db::playback::media::thumbnail_state as states;
        assert_eq!(states::PENDING, "pending");
        assert_eq!(states::RETRY_WAIT, "retry_wait");
        assert_eq!(states::TERMINAL, "terminal");
        assert_eq!(states::SUCCEEDED, "succeeded");
        assert_eq!(states::ALL.len(), 4);
    }
}
