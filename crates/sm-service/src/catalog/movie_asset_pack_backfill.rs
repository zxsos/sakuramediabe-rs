//! 影片图片包的手动回填（上游 `catalog/movie_asset_pack_backfill_service.py`，181 行）。
//!
//! # 任务键 `movie_asset_pack_backfill` —— **manual_only**
//!
//! `cron_spec::builtin_jobs` 里它是三条无 cron 的任务之一（另两条是
//! `media_video_info_backfill` 与 `media_thumbnail_pack_backfill`）。
//! 按上游 `contracts.py` 的不变式，`manual_only` 的任务**必须**允许手动触发
//! —— 否则它既没有 cron 又不能手动，等于永远不会跑。
//!
//! # 与 [`super::movie_asset_pack`] 的关系：一个是重建，一个是**回填存量**
//!
//! 导入流程里已经会建包（见 `catalog_import`）。这个任务处理的是**历史遗留**：
//! 早期版本把图片存成散文件，没有 `assets.zip`。
//!
//! # DB 为准，缺文件**整条跳过**
//!
//! 上游注释明写：「DB 为准，缺文件整条跳过」。即：候选来自 `image` 表（记录
//! 在），而若对应的物理文件已经不在磁盘上，**跳过这部影片**而不是报错。
//!
//! 为什么不是报错：那部影片的记录本来就该被清理（见 `image_cleanup`），
//! 清理之前它会一直「缺文件」。报错会让这个任务永远无法跑完。
//!
//! ⚠️ 反过来也别把它当成「顺手清理」—— 删除是 `image_cleanup` 的职责，
//! 这里只负责重建包。

use crate::error::ServiceError;

/// 任务键。与 `cron_spec::builtin_jobs` 里的 `movie_asset_pack_backfill` 一致。
pub const TASK_KEY: &str = "movie_asset_pack_backfill";

/// 一部待回填的影片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillCandidate {
    pub movie_id: i64,
    pub movie_number: String,
    /// 影片目录的**相对路径**。
    pub movie_dir_relative: String,
    /// DB 里的图片记录数。**实际文件数可能更少**（见模块文档）。
    pub image_record_count: usize,
}

/// 回填统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct PackBackfillStats {
    /// 考察的影片数。
    pub examined: i32,
    /// 成功建成包的部数。
    pub packed: i32,
    /// **因为文件已缺失而整条跳过**的部数（不是失败）。
    pub skipped_missing_files: i32,
    /// 重建失败（自检三次仍不过）的部数。
    pub failed: i32,
}

/// 判定候选：这部影片**是否值得**尝试回填。
///
/// **纯函数** —— 判据是「有图片记录」且「还没有包」。两个条件都必要：
/// 无记录 = 没东西可打包（`rebuild_movie_asset_pack` 会返回 `Ok(false)`，
/// 白跑一趟）；已有包 = 不该动它（用户可能刚上传过）。
pub fn should_backfill(image_record_count: usize, pack_already_exists: bool) -> bool {
    image_record_count > 0 && !pack_already_exists
}

/// 回填服务。
pub struct MovieAssetPackBackfillService;

impl MovieAssetPackBackfillService {
    /// 列出候选：库里有图片记录、但还没有 `assets.zip` 的影片。
    pub async fn candidates(limit: Option<i64>) -> Result<Vec<BackfillCandidate>, ServiceError> {
        let _ = limit;
        todo!("骨架：查「有 image 记录 且 目标包不存在」的影片，按 id 升序")
    }

    /// ★ 跑一轮（任务执行体）。上游 `backfill(cls, *, reporter) -> dict`。
    ///
    /// 逐部重建包；**缺文件的整条跳过**（`skipped_missing_files`），
    /// 自检三次不过的记为 `failed`。
    ///
    /// 单部失败**不中断**整批 —— 这是个可能要跑很久的存量任务。
    pub async fn backfill(&self) -> Result<PackBackfillStats, ServiceError> {
        todo!("骨架：逐部 candidates() -> 查实际文件数(缺则整条跳过) -> rebuild_movie_asset_pack")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个条件**都**必要。
    #[test]
    fn both_conditions_are_required() {
        // 有记录、无包 -> 该回填
        assert!(should_backfill(3, false));
        // 无记录 -> 没东西可打包，白跑
        assert!(!should_backfill(0, false));
        // 已有包 -> 不该动它（用户可能刚上传过）
        assert!(!should_backfill(3, true));
        assert!(!should_backfill(0, true));
    }

    /// ★ `skipped_missing_files` 与 `failed` **必须分开**。
    ///
    /// 「文件已缺失」是数据问题（等 `image_cleanup` 清理），
    /// 「重建失败」是算法问题。合成一个数就看不出该修哪边，
    /// 而前者跳过是**正确行为**、不该让整个任务失败。
    #[test]
    fn missing_files_are_skipped_not_failed() {
        let stats = PackBackfillStats {
            examined: 10,
            packed: 6,
            skipped_missing_files: 3,
            failed: 1,
        };
        assert_eq!(
            stats.packed + stats.skipped_missing_files + stats.failed,
            stats.examined
        );
        assert_eq!(stats.skipped_missing_files, 3);
    }

    /// 任务键与 `cron_spec` 一致，且它是 **manual_only** 之一。
    #[test]
    fn the_task_key_is_pinned() {
        assert_eq!(TASK_KEY, "movie_asset_pack_backfill");
    }
}
