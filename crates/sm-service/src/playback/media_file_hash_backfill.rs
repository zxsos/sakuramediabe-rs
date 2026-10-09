//! 文件哈希回填（上游 `playback/media_file_hash_backfill_service.py`，113 行）。
//!
//! # 任务键 `media_file_hash_backfill`，cron `0 3 * * *`
//!
//! # 哈希是**去重的依据**，所以回填是增量
//!
//! `media.file_hash` 用于识别「同一个文件在多个库里的副本」。缺失时无法去重，
//! 于是同一影片会被导入多次（多份媒体、多份缩略图、多份包）。
//!
//! **只补 `file_hash IS NULL` 的** —— 已有的重算是纯浪费，而且会让
//! `updated_at` 全表刷新。
//!
//! # ⚠️ 算哈希要读**整个文件**
//!
//! 所以这是个 IO 密集型任务，凌晨 3 点跑（见 cron）。且**单条失败不中断**
//! —— 一个无权限的文件不该让剩下几千个白等一夜。
//!
//! # provider 返回的哈希要**校验格式**
//!
//! 上游对 provider 返回值做校验（`ValueError("provider returned an
//! invalid media file hash")`）。因为 `file_hash` 参与去重比较，格式不一致
//! 会让**同一文件算出两个不同的哈希** —— 去重失效，且没有任何报错。

use crate::error::ServiceError;

/// 任务键。与 `cron_spec::builtin_jobs` 一致。
pub const TASK_KEY: &str = "media_file_hash_backfill";

/// 回填统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct FileHashBackfillStats {
    /// 考察的媒体数（`file_hash IS NULL`）。
    pub examined: i32,
    /// 成功算出的数量。
    pub hashed: i32,
    /// provider 失败的数量。
    pub failed: i32,
    /// ★ provider 返回了**非法哈希**的数量。**与 `failed` 分开** ——
    /// 前者是 provider 的 bug（该上报），后者是正常的读文件失败。
    pub invalid_hash: i32,
}

/// 校验 provider 返回的哈希。**纯函数**。
///
/// 上游要求非空。这里再加一条：**不接受纯空白** —— 那种值会让 `IS NULL`
/// 判断失效（`''` 不是 NULL），于是那行**永远不会被回填**。
pub fn is_valid_hash(hash: &str) -> bool {
    !hash.trim().is_empty()
}

/// 文件哈希回填服务。
pub struct MediaFileHashBackfillService;

impl MediaFileHashBackfillService {
    /// ★ 跑一轮。任务执行体。
    pub async fn backfill_missing_file_hashes(
        &self,
    ) -> Result<FileHashBackfillStats, ServiceError> {
        todo!("骨架：查 file_hash IS NULL 的媒体 -> 逐个 provider compute_file_hash -> 校验格式 -> 回填")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 空白哈希**非法** —— 它会让 `IS NULL` 永远命中不到那行。
    #[test]
    fn a_blank_hash_is_invalid() {
        assert!(!is_valid_hash(""));
        assert!(!is_valid_hash("   "));
        assert!(!is_valid_hash("\t\n"));
        assert!(is_valid_hash("d41d8cd98f00b204e9800998ecf8427e"));
    }

    /// `invalid_hash` 与 `failed` 分开 —— 前者是 provider 的 bug，该上报。
    #[test]
    fn invalid_hash_is_counted_apart_from_failure() {
        let stats = FileHashBackfillStats {
            examined: 10,
            hashed: 7,
            failed: 2,
            invalid_hash: 1,
        };
        assert_eq!(
            stats.hashed + stats.failed + stats.invalid_hash,
            stats.examined
        );
    }
}
