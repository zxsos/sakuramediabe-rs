//! 视频条目（`VideoItem`）的 service，对应上游
//! `src/service/videos/video_item_service.py`（436 行）。
//!
//! # 落地的六条规则
//!
//! | 规则 | 上游符号 | 后果 |
//! |---|---|---|
//! | 条目不存在 | `_require_video` → `require_by_id` | 404 `video_item_not_found` |
//! | 空标题 | `VideoItemCreateRequest` 的 pydantic 校验器 | 422 `validation_error` |
//! | 空更新 | `if not update_data` | 422 `validation_error` |
//! | 封面缩略图显式传 null | `if thumbnail_id is None` | 422 `video_cover_thumbnail_required` |
//! | 封面缩略图不存在或不属于本条目 | `MediaThumbnail.join(Media).where(Media.video_item == video)` | 404 `video_cover_thumbnail_not_found` |
//! | 换封面后旧图片不再被引用 | `ImageCleanupService.delete_image_record_if_unused` | **不落地**，见 [`crate::videos`] |
//!
//! # 「缩略图不存在」与「缩略图不属于本条目」合并成同一个 404
//!
//! 上游那条查询是 `MediaThumbnail JOIN Media WHERE id = ? AND video_item = ?`，
//! 两个条件都在 `WHERE` 里 —— 命中不了就统一 `thumbnail is None`。
//! 所以「这个缩略图属于别的视频」**不是** 403 也不是 422，而是 404：
//! 错误消息不泄露「该缩略图存在」，而客户端只需要知道「你给的这个 id 对
//! 你的条目不适用」。
//!
//! # 更新时的两套 null 语义
//!
//! 逐条抄自 `update_video`：
//!
//! ```python
//! if "cover_thumbnail_id" in update_data:
//!     if update_data["cover_thumbnail_id"] is None:   # 显式 null → 422
//! if "title" in update_data and update_data["title"] is not None:   # null → 忽略
//! if "release_date" in update_data:                   # null → 清空
//! ```
//!
//! 用 [`Field`] 三态编码，见 [`crate::videos::Field`]。
//!
//! # 删除带走什么
//!
//! `video_item` 的媒体外键是 `CASCADE`，所以删条目会连带删掉它的
//! `media` 行。上游还额外做了两件本批不做的事：走
//! `MediaService.delete_media(..., sync_video_member=False)` 清理磁盘文件与
//! 缩略图产物、删封面图片文件。**本切片只删库里的行**，所以
//! 「下架但保留文件」这种用法在 Rust 侧还不成立 —— 要保留得先把媒体改挂到
//! 别的条目上。

use chrono::NaiveDateTime;
use sm_db::repo::{
    MediaRepository, MediaThumbnailRepository, NewVideoItem, VideoItemFields, VideoItemRepository,
};
use sm_db::videos::VideoItem;
use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::videos::Field;

/// 新建条目。
#[derive(Debug, Clone, Default)]
pub struct VideoItemCreate {
    pub title: String,
    /// 简介。缺省空串。
    pub summary: String,
    /// 发布日期。
    pub release_date: Option<NaiveDateTime>,
}

/// 更新条目。四个字段各自独立，缺省表示不改动。
#[derive(Debug, Clone, Default)]
pub struct VideoItemUpdate {
    /// 标题。`Null` 被**忽略**（上游 `is not None` 才赋值）。
    pub title: Field<String>,
    /// 简介。`Null` 被忽略。`Value("")` 是合法的「清空简介」。
    pub summary: Field<String>,
    /// 发布日期。`Null` 是**清空**（上游只判 key 在不在）。
    pub release_date: Field<NaiveDateTime>,
    /// 封面缩略图。`Null` 是**错误**（422），只有 `Absent` 才表示不改动。
    pub cover_thumbnail_id: Field<i32>,
}

impl VideoItemUpdate {
    /// 是否是空更新 —— 四个字段都没给。
    ///
    /// 只带 `null` 的更新**不算空**：`{"title": null}` 在上游会通过这个检查，
    /// 然后什么都不改、只推进 `updated_at`。
    pub fn is_empty(&self) -> bool {
        !self.title.is_given()
            && !self.summary.is_given()
            && !self.release_date.is_given()
            && !self.cover_thumbnail_id.is_given()
    }
}

/// 视频条目 service。
///
/// 持有四个仓储：条目本身、媒体、缩略图。**不持有连接池** —— 本 slice 的
/// 写入都是单条 UPDATE，不需要事务。
pub struct VideoItemService {
    items: VideoItemRepository,
    media: MediaRepository,
    thumbnails: MediaThumbnailRepository,
}

impl VideoItemService {
    pub fn new(db: &Db) -> Self {
        Self {
            items: VideoItemRepository::new(db.clone()),
            media: MediaRepository::new(db.clone()),
            thumbnails: MediaThumbnailRepository::new(db.clone()),
        }
    }

    // ---------------------------------------------------------- 规则

    /// 取条目，不存在则 404。
    ///
    /// `details` 的键是 `video_item_id`（上游 `require_by_id` 从实体名生成）。
    async fn require_video(&self, id: i32) -> Result<VideoItem, ServiceError> {
        self.items.find_by_id(id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "video_item_not_found",
                "Video item not found",
                "video_item_id",
                id,
            )
        })
    }

    /// 标题归一。空白 → 422。
    ///
    /// 上游这条校验在 pydantic 层（`field_validator` + `min_length=1`），
    /// 422 由 `validation_exception_handler` 统一发出。Rust 侧没有那一层，
    /// 校验落在 service —— 对客户端而言契约不变：仍然是
    /// 422 `validation_error`，只是 `details` 从 FastAPI 的
    /// `{"detail": [...], "body": {...}}` 变成空。
    fn normalize_title(title: &str) -> Result<String, ServiceError> {
        let normalized = title.trim();
        if normalized.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "title cannot be blank",
            ));
        }
        Ok(normalized.to_owned())
    }

    /// 解析封面缩略图 → 图片 id，并确认它属于本条目。
    ///
    /// 两步查（先缩略图、再媒体）而不是一条 JOIN：`MediaThumbnailRepository`
    /// 按 `(media_id, offset)` 组织，唯一索引在那上面，所以从 id 出发的
    /// 点查要走主键。两条都是主键/唯一索引点查，比 JOIN 更直接。
    async fn resolve_cover(&self, video_id: i32, thumbnail_id: i32) -> Result<i32, ServiceError> {
        let not_found = || {
            ServiceError::not_found_with(
                "video_cover_thumbnail_not_found",
                "Video cover thumbnail not found",
                details_of("video_id", video_id)
                    .into_iter()
                    .chain(details_of("thumbnail_id", thumbnail_id))
                    .collect(),
            )
        };
        let Some(thumbnail) = self.thumbnails.find_by_id(thumbnail_id).await? else {
            return Err(not_found());
        };
        let Some(media) = self.media.find_by_id(thumbnail.media_id).await? else {
            // 媒体被删而缩略图还在：外键是 CASCADE，理论上不可能。
            // 判成同一个 404 而不是 500 —— 对客户端而言「这个缩略图不可用」
            // 就是事实。
            return Err(not_found());
        };
        if media.video_item_id != Some(video_id) {
            return Err(not_found());
        }
        Ok(thumbnail.image_id)
    }

    // ---------------------------------------------------------- 写

    /// 新建条目。
    pub async fn create(&self, payload: &VideoItemCreate) -> Result<VideoItem, ServiceError> {
        let title = Self::normalize_title(&payload.title)?;
        Ok(self
            .items
            .insert(&NewVideoItem {
                title,
                summary: payload.summary.trim().to_owned(),
                // 上游 `VideoItemCreateRequest` 没有 `cover_image_id`：
                // 封面只能通过后续 update 设成某张**已有**缩略图，
                // 不支持直接指定图片 id（也不支持恢复自动首帧）。
                cover_image_id: None,
                release_date: payload.release_date,
                extra: None,
            })
            .await?)
    }

    /// 更新条目。
    pub async fn update(
        &self,
        video_id: i32,
        payload: VideoItemUpdate,
    ) -> Result<VideoItem, ServiceError> {
        self.require_video(video_id).await?;
        if payload.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "At least one field must be provided",
            ));
        }
        if payload.cover_thumbnail_id.is_given() && payload.cover_thumbnail_id.as_value().is_none()
        {
            // 上游的写法是 `if thumbnail_id is None: 422` —— 显式 null 是
            // 「我要清空封面」，而这条规则**不允许**清空（清空会让
            // 瀑布流没有图）。所以只有 Absent 才是「不改动」。
            return Err(ServiceError::validation(
                "video_cover_thumbnail_required",
                "cover_thumbnail_id cannot be null",
            ));
        }

        let title = match &payload.title {
            Field::Value(raw) => Some(Self::normalize_title(raw)?),
            _ => None,
        };
        let summary = match &payload.summary {
            Field::Value(raw) => Some(raw.trim().to_owned()),
            _ => None,
        };
        let release_date = match &payload.release_date {
            Field::Value(v) => Some(Some(*v)),
            // 显式 null = 清空；Absent = 不动。
            Field::Null => Some(None),
            Field::Absent => None,
        };

        // 封面走**另一条** UPDATE：换封面要能读到旧值（用于后续清理不再被
        //  引用的图片），而那一步在 service 层，不在 SQL 里。
        if let Some(thumbnail_id) = payload.cover_thumbnail_id.as_value() {
            let image_id = self.resolve_cover(video_id, *thumbnail_id).await?;
            self.items.set_cover(video_id, Some(image_id)).await?;
        }

        let updated = self
            .items
            .update_fields(
                video_id,
                &VideoItemFields {
                    title: title.as_deref(),
                    summary: summary.as_deref(),
                    release_date,
                },
            )
            .await?
            .ok_or_else(|| {
                // 走到这里说明 require_video 之后行被并发删了。客户端拿到
                // 500 比拿到 404 更诚实：那是一次没能完成的两段式更新。
                ServiceError::from(sm_db::DbError::business(
                    "VideoItem",
                    "条目在更新过程中消失",
                ))
            })?;
        Ok(updated)
    }

    /// 删除条目。**连带删除它的媒体行**（外键 CASCADE）。
    pub async fn delete(&self, video_id: i32) -> Result<(), ServiceError> {
        self.require_video(video_id).await?;
        self.items.delete(video_id).await?;
        Ok(())
    }

    // ---------------------------------------------------------- 读

    /// 取条目。
    pub async fn get(&self, video_id: i32) -> Result<VideoItem, ServiceError> {
        self.require_video(video_id).await
    }

    /// 列出某个条目下的全部媒体，按 `Media.id` 升序。**刻意不分页** ——
    /// 「这个条目下有哪些文件」是详情页的需求，调用方几乎总是要全部。
    ///
    /// 上游在这之上还会给每条媒体挂进度、时刻点与签名播放地址，后两者分别
    /// 需要 `playback` 域的仓储与插件 registry，都不在本批。见
    /// [`crate::videos`] 的「刻意不复刻」①。
    pub async fn list_media(&self, video_id: i32) -> Result<Vec<sm_db::Media>, ServiceError> {
        self.require_video(video_id).await?;
        Ok(self.items.list_media(video_id).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_titles_are_rejected_with_the_upstream_message() {
        for blank in ["", "   ", "\t\n"] {
            let err = VideoItemService::normalize_title(blank).unwrap_err();
            assert_eq!(
                (err.status, err.code()),
                (422, "validation_error"),
                "空白标题应是 422 validation_error"
            );
            assert_eq!(err.api.message, "title cannot be blank");
        }
        assert_eq!(
            VideoItemService::normalize_title("  素颜  ").unwrap(),
            "素颜".to_owned(),
            "上游 pydantic 校验器做 strip()"
        );
    }

    #[test]
    fn a_null_only_update_is_not_an_empty_update() {
        // 上游判的是「有没有 key」，不是「有没有非 null 的值」——
        // `{"title": null}` 会通过空更新检查，然后只推进 updated_at。
        let only_null = VideoItemUpdate {
            title: Field::Null,
            ..Default::default()
        };
        assert!(!only_null.is_empty());

        assert!(VideoItemUpdate::default().is_empty());

        // 封面显式 null 是「非空更新」，但会在 service 里被 422 拦下 ——
        // 两件事必须同时成立，区别只在拦在哪一层。
        let null_cover = VideoItemUpdate {
            cover_thumbnail_id: Field::Null,
            ..Default::default()
        };
        assert!(!null_cover.is_empty());
    }
}
