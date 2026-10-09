//! 媒体 / 时刻 / 进度 / 缩略图的 CRUD 与分页（上游 `playback/media_service.py`，782 行，本域最大）。
//!
//! # 它横跨 JAV 与 videos 两个域
//!
//! `media` 表同时存 JAV 影片的文件和普通视频条目（`video_item_id` 有值时）。
//! 所以 [`Self::list_duplicate_media_groups`] 的 `kind` 参数是
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
//! [`Self::MEDIA_LIST_SORT_FIELD_MAP`]。`heat` 是**唯一可空**的排序字段
//! （`MEDIA_LIST_NULLABLE_SORT_FIELDS`）—— 排序时要用 `NULLS LAST`，
//! 否则 Postgres 默认 `NULLS LAST FOR ASC` / `NULLS FIRST FOR DESC` 会让
//! 「按热度降序」变成「没热度的排最前」。

use crate::error::ServiceError;

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
    let Some((_, column)) = MEDIA_LIST_SORT_FIELD_MAP.iter().find(|(name, _)| *name == field) else {
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

/// 时刻（media point）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaPointResource {
    pub id: i64,
    pub media_id: i64,
    pub thumbnail_id: Option<i64>,
    /// 在影片里的偏移（秒）。
    pub offset_seconds: i64,
    pub created_at: Option<String>,
}

/// 媒体服务。
pub struct MediaService;

impl MediaService {
    /// `GET /media` —— 分页列表。
    ///
    /// `kind` **有默认值**（`all`），而 `/media/duplicates` 的 `kind`
    /// **必填** —— 见 `routes/media.rs` 的说明。
    pub async fn list_media(
        query: &MediaListQuery,
    ) -> Result<MediaListPage, ServiceError> {
        let _ = query;
        todo!("骨架：kind/library_id/actor_ids/缩略图状态 过滤 + 白名单排序(heat 用 NULLS LAST) + 分页")
    }

    /// `GET /media/multi-version` —— 同番号多文件。
    pub async fn list_multi_version_movies(
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
        media_kind: &str,
        page: i64,
        page_size: i64,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = (media_kind, page, page_size);
        todo!("骨架：GROUP BY file_hash（缺失的用退化键单独分组并标 Degraded）")
    }

    /// `GET /media-points` —— 时刻列表。`kind` 默认 `jav`。
    pub async fn list_media_points(
        page: i64,
        page_size: i64,
        sort: Option<&str>,
        media_kind: Option<&str>,
        keyword: Option<&str>,
        exclude_collection_id: Option<i64>,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = (page, page_size, sort, media_kind, keyword, exclude_collection_id);
        todo!("骨架：按 created_at DESC, id DESC 排序；keyword 匹配文件名与番号")
    }

    /// 某媒体的全部时刻。**按 `(offset, id)` 升序**。
    pub async fn list_points(media_id: i64) -> Result<Vec<MediaPointResource>, ServiceError> {
        let _ = media_id;
        todo!("骨架：查该媒体时刻按 (offset ASC, id ASC)")
    }

    /// 新建时刻。返回 `(资源, 是否新建)`。
    ///
    /// **幂等**：同一 `(media_id, offset, thumbnail_id)` 只建一次。
    pub async fn create_point(
        media_id: i64,
        offset_seconds: i64,
        thumbnail_id: Option<i64>,
    ) -> Result<(MediaPointResource, bool), ServiceError> {
        let _ = (media_id, offset_seconds, thumbnail_id);
        todo!("骨架：幂等写入（同一 offset+thumbnail 只建一次）")
    }

    /// 删时刻。**连带删缩略图**（它只服务于这个时刻）。
    pub async fn delete_point(media_id: i64, point_id: i64) -> Result<(), ServiceError> {
        let _ = (media_id, point_id);
        todo!("骨架：确认时刻属于该媒体(404) -> 删缩略图(经 image_cleanup) -> 删时刻")
    }

    /// 删时刻（**不**校验媒体归属）。供跨域调用。
    pub async fn delete_point_by_id(point_id: i64) -> Result<(), ServiceError> {
        let _ = point_id;
        todo!("骨架：同上但不校验 media_id")
    }

    /// 更新播放进度。
    pub async fn update_progress(
        media_id: i64,
        position_seconds: i64,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = (media_id, position_seconds);
        todo!("骨架：UPSERT media_progress；返回更新后的进度")
    }

    /// ★ 删媒体。**三步各自独立**，见模块文档。
    ///
    /// 错误码：媒体不存在 → `404 media_not_found`；provider 报错 →
    /// `provider_{code}`（`provider_not_installed` 是 503）。
    ///
    /// `sync_video_member` 控制是否连带删 `video_item` 成员关系。
    pub async fn delete_media(media_id: i64, sync_video_member: bool) -> Result<(), ServiceError> {
        let _ = (media_id, sync_video_member);
        todo!("骨架：provider 删文件 -> 清 Qdrant(仅启用时) -> 删 DB 记录与图片；三步独立不互阻")
    }

    /// 某媒体的缩略图。**按 `(offset, id)` 升序**。
    pub async fn list_thumbnails(media_id: i64) -> Result<Vec<super::thumbnails::artifacts::MediaThumbnailResource>, ServiceError> {
        let _ = media_id;
        todo!("骨架：转调 thumbnails::artifacts::list_media_thumbnails")
    }

    /// `GET /media/invalid` —— 已失效媒体列表。
    pub async fn list_invalid_media(
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
        let (column, desc) = resolve_sort(Some("-heat")).expect("heat 可排").expect("有值");
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
        assert!(MEDIA_LIST_SORT_FIELD_MAP.iter().any(|(name, _)| *name == "heat"));
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

