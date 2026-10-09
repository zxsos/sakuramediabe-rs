//! `videos` 域模型（3 张表）与 `collections` 域（2 张表）。
//!
//! 对应 `src/model/videos/items.py` 与 `videos/collections.py`。
//! 注意 `VideoCollection` 也在 `videos` 目录下，不在 `collections` 域。
//!
//! # 与 JAV 侧的区别
//!
//! | 概念 | JAV | 非 JAV |
//! |---|---|---|
//! | 条目 | `Movie`（有番号） | `VideoItem`（无番号、无外部元数据） |
//! | 合集 | `Playlist` | `VideoCollection` |
//! | 排序字段 | `PlaylistMovie` **无** position | `VideoCollectionItem` **有** position |
//!
//! 所以播放顺序在 JAV 侧靠 join 顺序，视频侧靠显式 `position`。
//! 迁移时不能把两者当同一张表处理。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `video_item` 表：非 JAV 视频条目。
///
/// 与 `Movie` 完全平行 —— 文件都归 `Media`，区别是没有番号与外部元数据。
#[derive(Debug, Clone, FromRow)]
pub struct VideoItem {
    pub id: i64,
    /// `save` 会做 `strip()`，所以库里不会有纯空白标题。
    pub title: String,
    pub summary: String,
    /// 封面图。删图时置空而非级联删除条目。
    pub cover_image_id: Option<i64>,
    /// 发布时间。
    pub release_date: Option<NaiveDateTime>,
    /// `JsonTextField`，默认 NULL。
    pub extra: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl VideoItem {
    /// 对应 `VideoItem.save` 的标题归一化。
    ///
    /// `None` 会被折叠成空串，与 Python 的 `(self.title or "").strip()` 一致。
    pub fn normalize_title(title: Option<&str>) -> String {
        title.unwrap_or_default().trim().to_owned()
    }
}

/// `video_collection` 表：视频合集。
///
/// 与 JAV 的 `Playlist` 平行，但语义更简单 —— 不参与刮削与订阅。
#[derive(Debug, Clone, FromRow)]
pub struct VideoCollection {
    pub id: i64,
    /// 全局唯一。
    pub name: String,
    pub description: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl VideoCollection {
    /// 与 `VideoItem::normalize_title` 同一套规则。
    pub fn normalize_name(name: Option<&str>) -> String {
        name.unwrap_or_default().trim().to_owned()
    }
}

/// `video_collection_item` 表：合集成员。
///
/// 唯一索引 `(collection, video_item)` —— 同一个视频不能在一个合集里出现两次。
#[derive(Debug, Clone, FromRow)]
pub struct VideoCollectionItem {
    pub id: i64,
    pub collection_id: i64,
    pub video_item_id: i64,
    /// 显式播放顺序。JAV 侧的 `PlaylistMovie` 缺此字段。
    pub position: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl VideoCollectionItem {
    /// 按 `position` 升序、同位按 `id` 升序排序。
    ///
    /// `id` 作为次级键是必要的：删除后重排会让多条成员 `position` 相同，
    /// 只按 `position` 排序时顺序不确定，会导致播放列表抖动。
    pub fn playback_order_key(&self) -> (i32, i64) {
        (self.position, self.id)
    }
}
