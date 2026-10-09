//! 缩略图包回填（上游 `playback/media_thumbnail_pack_backfill_service.py`，217 行）。
//!
//! # 任务键 `media_thumbnail_pack_backfill` —— **manual_only**
//!
//! 三条无 cron 的任务之二（另一条是 `movie_asset_pack_backfill`；第三条是
//! `media_video_info_backfill`）。
//!
//! # 与 [`super::thumbnails::task_service`] 是**不同**的两件事
//!
//! | | 本文件 | `task_service` |
//! |---|---|---|
//! | 做什么 | 把**已有**单文件缩略图打成 `thumbnails.zip` | 调 provider **生成**缩略图 |
//! | 跑多久 | 快（纯文件 IO） | 慢（ffprobe + 抽帧） |
//!
//! 分不清会导致「包回填」去调 provider —— 那是浪费一次生成。
//!
//! # DB 为准，缺文件**整条跳过**
//!
//! 与 `catalog::movie_asset_pack_backfill` 同一取舍（见那个文件）。
//!
//! # 自检：包能打开且条目数正确，否则**抛错**
//!
//! 上游 `RuntimeError("thumbnail_pack_self_check_failed")`。
//!
//! ⚠️ 与 `movie_asset_pack` 的「重试 3 次」不同 —— 这里是**直接抛错**。
//! 因为打包只读本地文件，失败原因通常是「文件真没了」或「磁盘满了」，
//! 重试 3 次毫无意义，还会把任务拖长。

use crate::error::ServiceError;

/// 任务键。
pub const TASK_KEY: &str = "media_thumbnail_pack_backfill";

/// 回填统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ThumbnailPackBackfillStats {
    pub examined: i32,
    /// 成功成包的数量。
    pub packed: i32,
    /// 已有包、跳过的数量。
    pub skipped_existing_pack: i32,
    /// 缺文件而整条跳过的数量（**不是失败**）。
    pub skipped_missing_files: i32,
    /// 自检失败的数量。**真的抛错**。
    pub failed: i32,
}

/// 该媒体是否需要回填包。**纯函数**。
///
/// 「已有缩略图但没有包」才需要。**没有缩略图**的**不需要**——
/// 那属于 [`super::thumbnails::task_service`] 的活（先生成再打包）。
pub fn needs_pack(thumbnail_count: usize, pack_exists: bool) -> bool {
    thumbnail_count > 0 && !pack_exists
}

/// 缩略图包回填服务。
pub struct MediaThumbnailPackBackfillService;

impl MediaThumbnailPackBackfillService {
    /// ★ 跑一轮。任务执行体。
    pub async fn backfill(&self) -> Result<ThumbnailPackBackfillStats, ServiceError> {
        todo!("骨架：查有缩略图但无包的媒体 -> 查实际文件(缺则跳过) -> 写 ZIP_STORED 包 -> 解包自检条目数")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 「没有缩略图」**不需要**回填包 —— 那是先生成再打包的活。
    #[test]
    fn a_media_without_thumbnails_needs_no_pack() {
        assert!(!needs_pack(0, false), "没缩略图 -> 不该在这里打包");
        // 有缩略图且没包 -> 需要
        assert!(needs_pack(5, false));
        // 已有包 -> 跳过
        assert!(!needs_pack(5, true));
    }

    /// 四类计数互斥且完备。
    #[test]
    fn the_four_outcomes_partition_the_examined_media() {
        let stats = ThumbnailPackBackfillStats {
            examined: 12,
            packed: 6,
            skipped_existing_pack: 3,
            skipped_missing_files: 2,
            failed: 1,
        };
        assert_eq!(
            stats.packed + stats.skipped_existing_pack + stats.skipped_missing_files + stats.failed,
            stats.examined
        );
    }
}
