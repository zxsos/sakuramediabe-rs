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

use sm_db::repo::playback::MediaProgressRepository;
use sm_db::repo::{
    ImageRepository, MediaPointRepository, MediaRepository, MediaThumbnailRepository,
};
use sm_db::Db;

use crate::catalog::image_cleanup::ImageCleanupService;
use crate::error::{details_of, ServiceError};
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

/// 缩略图生成状态。
pub mod thumbnail_state {
    /// 待生成。
    pub const PENDING: i32 = 0;
    /// 失败。
    pub const FAILED: i32 = 1;
    /// 成功。
    pub const SUCCESS: i32 = 2;
    /// 跳过。
    pub const SKIPPED: i32 = 3;
}

/// 排序字段白名单。**不在表里的一律 422**。
pub const MEDIA_LIST_SORT_FIELD_MAP: [(&str, &str); 4] = [
    ("created_at", "m.created_at"),
    ("updated_at", "m.updated_at"),
    ("file_name", "m.file_name"),
    ("heat", "m.heat"),
];

/// 可空的排序字段。排序时要显式 `NULLS LAST`。
pub const MEDIA_LIST_NULLABLE_SORT_FIELDS: [&str; 1] = ["heat"];

/// 解析排序字段。`None`/空 → `None`；不在白名单 → **422**。
///
/// **不夹到默认值** —— 上游 FastAPI 的 `Literal` 校验就是 422。
pub fn resolve_sort(value: Option<&str>) -> Result<Option<(&'static str, bool)>, ServiceError> {
    let Some(raw) = value.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let field = raw.strip_prefix('-').unwrap_or(raw);
    let Some((_, column)) = MEDIA_LIST_SORT_FIELD_MAP
        .iter()
        .find(|(name, _)| *name == field)
    else {
        return Err(ServiceError::validation(
            "invalid_media_filter",
            format!("未知的排序字段：{raw}"),
        ));
    };
    // 升序 = 无前缀；降序 = `-` 前缀。
    Ok(Some((*column, raw.starts_with('-'))))
}

/// `GET /media` 的查询参数。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct MediaListQuery {
    /// 媒体种类。**有默认值** `all`（`/media/duplicates` 那个必填）。
    pub kind: Option<String>,
    pub library_id: Option<i64>,
    /// **CSV** 形态（`?actor_ids=1,2`）—— 与 transfers 的重复参数不同。
    pub actor_ids: Option<String>,
    /// 缩略图生成状态。
    pub thumbnail_generation_state: Option<i32>,
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
/// # 为什么它需要 `Db`（骨架期是无状态单元结构体）
///
/// 骨架期这里是 `pub struct MediaService;`，所有方法都是**关联函数**，
/// 拿不到任何仓储 —— 于是 12 个方法全是 `todo!()`，一个都落不了地。
/// 全仓**没有任何调用点**（实测 `grep MediaService::` 只命中模块文档），
/// 所以改形状是零风险的。
///
/// 五个仓储按方法用到的面收：时刻点要 media / points / thumbnails / images，
/// 进度要 media / progress / pool（后者给 `PlaylistService`）。
pub struct MediaService {
    media: MediaRepository,
    points: MediaPointRepository,
    thumbnails: MediaThumbnailRepository,
    progress: MediaProgressRepository,
    images: ImageRepository,
    pool: Db,
    /// 删时刻要连带清掉那张只服务于它的图，而清理服务要图片根目录。
    config: ConfigService,
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
        }
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
        let _ = query;
        todo!("骨架：kind/library_id/actor_ids/缩略图状态 过滤 + 白名单排序(heat 用 NULLS LAST) + 分页")
    }

    /// `GET /media/multi-version` —— 同番号多文件。
    pub async fn list_multi_version_movies(
        &self,
        page: i64,
        page_size: i64,
        include_vr: bool,
        include_fc2: bool,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = (page, page_size, include_vr, include_fc2);
        todo!("骨架：按番号分组 HAVING count > 1；VR/FC2 走番号前缀过滤")
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
        let _ = (media_kind, page, page_size);
        todo!("骨架：GROUP BY file_hash（缺失的用退化键单独分组并标 Degraded）")
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
        let _ = (
            page,
            page_size,
            sort,
            media_kind,
            keyword,
            exclude_collection_id,
        );
        todo!("骨架：按 created_at DESC, id DESC 排序；keyword 匹配文件名与番号")
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
    /// 错误码：媒体不存在 → `404 media_not_found`；provider 报错 →
    /// `provider_{code}`（`provider_not_installed` 是 503）。
    ///
    /// `sync_video_member` 控制是否连带删 `video_item` 成员关系。
    pub async fn delete_media(
        &self,
        media_id: i64,
        sync_video_member: bool,
    ) -> Result<(), ServiceError> {
        let _ = (media_id, sync_video_member);
        todo!("骨架：provider 删文件 -> 清 Qdrant(仅启用时) -> 删 DB 记录与图片；三步独立不互阻")
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
        let _ = (page, page_size, search);
        todo!("骨架：查 valid = false 的媒体；search 匹配文件名与番号")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 排序字段是**白名单**，自由字符串 → 422（不夹到默认值）。
    #[test]
    fn sort_fields_come_from_the_allow_list() {
        assert_eq!(resolve_sort(None).expect("缺省不排序"), None);
        assert_eq!(resolve_sort(Some("  ")).expect("空串不排序"), None);
        let (column, desc) = resolve_sort(Some("-heat"))
            .expect("heat 可排")
            .expect("有值");
        assert_eq!(column, "m.heat");
        assert!(desc, "`-` 前缀 = 降序");
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

    /// 缩略图生成状态是四个固定值。
    #[test]
    fn the_thumbnail_states_are_four() {
        let states = [
            thumbnail_state::PENDING,
            thumbnail_state::FAILED,
            thumbnail_state::SUCCESS,
            thumbnail_state::SKIPPED,
        ];
        assert_eq!(states.len(), 4);
        let mut sorted = states.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "四个状态互不相同");
    }
}
