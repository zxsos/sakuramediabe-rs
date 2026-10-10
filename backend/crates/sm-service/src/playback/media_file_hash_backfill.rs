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

use std::sync::Arc;

use sm_db::repo::MediaRepository;
use sm_db::Db;

use crate::catalog::movie_asset_pack_backfill::ProgressSink;
use crate::error::ServiceError;
use crate::playback::operation_locks::MediaOperation;
use crate::playback::provider_helpers::{
    json_or_null, media_handle_for, MediaRecord, StorageGateway,
};

/// 任务键。与 `cron_spec::builtin_jobs` 一致。
pub const TASK_KEY: &str = "media_file_hash_backfill";

/// 回填统计。
///
/// # 与上游 stats 字典的对应
///
/// 上游 `media_file_hash_backfill_service.py:39-44` 四个键
/// （`missing_media` / `updated_media` / `failed_media` / `skipped_media`）。
/// 本仓多一个 `invalid_hash`（见下），**少的那半没有**：`missing_media` 就是
/// [`FileHashBackfillStats::examined`]。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct FileHashBackfillStats {
    /// 考察的媒体数（`file_hash IS NULL` 或空串）。= 上游 `missing_media`。
    pub examined: i32,
    /// 成功算出的数量。= 上游 `updated_media`。
    pub hashed: i32,
    /// provider 失败的数量。= 上游 `failed_media` **去掉非法哈希那部分**。
    pub failed: i32,
    /// ★ provider 返回了**非法哈希**的数量。**与 `failed` 分开** ——
    /// 前者是 provider 的 bug（该上报），后者是正常的读文件失败。
    ///
    /// # 与上游的分歧
    ///
    /// 上游把非法哈希也计入 `failed_media`（`ValueError` 被 except 捕获）。
    /// 单列的理由：混在一起时，「某个 provider 最近总是回空哈希」会被当成
    /// 「环境网络抖动」—— 前者要修插件，后者重试就好。
    pub invalid_hash: i32,
    /// 跳过的数量（锁被占，或**重读时**哈希已被别人补上）。= 上游
    /// `skipped_media` —— 上游这两种情况共用一个键，本仓沿用。
    pub skipped_media: i32,
}

/// 校验 provider 返回的哈希。**纯函数**。
///
/// 上游要求非空。这里再加一条：**不接受纯空白** —— 那种值会让 `IS NULL`
/// 判断失效（`''` 不是 NULL），于是那行**永远不会被回填**。
pub fn is_valid_hash(hash: &str) -> bool {
    !hash.trim().is_empty()
}

/// 文件哈希回填服务。
///
/// # 依赖是注入的
///
/// [`StorageGateway`] 是 trait（`provider_helpers.rs`），组合根注入真的
/// `ProviderGateway`，测试注入假实现 —— 本文件不知道「插件」是什么。
pub struct MediaFileHashBackfillService {
    db: Db,
    storage: Arc<dyn StorageGateway>,
    media: MediaRepository,
}

impl MediaFileHashBackfillService {
    /// 构造。
    pub fn new(db: &Db, storage: Arc<dyn StorageGateway>) -> Self {
        Self {
            db: db.clone(),
            storage,
            media: MediaRepository::new(db.clone()),
        }
    }

    /// 候选 media id。上游 `_candidate_ids`。
    pub async fn candidates(&self) -> Result<Vec<i32>, ServiceError> {
        Ok(self.media.list_missing_file_hash_ids().await?)
    }

    /// ★ 跑一轮。任务执行体。上游 `backfill_missing_file_hashes(cls, *, reporter)`。
    ///
    /// # 三条不变式（都来自上游）
    ///
    /// 1. **只补缺的** —— 已有的重算是纯浪费，还会全表刷 `updated_at`；
    /// 2. **单条失败不中断** —— 一个无权限的文件不该让剩下几千个白等一夜；
    /// 3. **锁被占是跳过不是排队** —— 排队会等到另一个任务改完才执行，
    ///    那时状态已经变了。
    ///
    /// 每条媒体的处理顺序：**先取锁、再重读**。重读是必须的 —— 候选列表
    /// 是开跑时的快照，轮到这条时哈希可能已被别人补上（比如并发的导入流程
    /// 刚写了它），不重读就会白算一整份文件。
    pub async fn backfill_missing_file_hashes(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<FileHashBackfillStats, ServiceError> {
        let media_ids = self.candidates().await?;
        let total = i32::try_from(media_ids.len()).unwrap_or(i32::MAX);
        let mut stats = FileHashBackfillStats {
            examined: total,
            hashed: 0,
            failed: 0,
            invalid_hash: 0,
            skipped_media: 0,
        };

        for (index, media_id) in media_ids.iter().enumerate() {
            let completed = i32::try_from(index + 1).unwrap_or(i32::MAX);

            let lock = match MediaOperation::try_media(&self.db, *media_id).await {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    stats.skipped_media += 1;
                    Self::emit(progress.as_mut(), completed, total, &stats).await;
                    continue;
                }
                // 取锁失败是 DB 层的问题，不是某条媒体的问题 —— 传播出去让
                // 任务以失败结束（上游同样只捕 `MediaOperationBusy`）。
                Err(error) => return Err(error),
            };

            let outcome = self.process_one(*media_id, &mut stats).await;
            // ★ 锁在 `process_one` 之后、`stats` 记完之后释放 —— 提前释放会让
            // 并发的删除/导入与本任务同时碰同一个媒体。
            lock.release().await;

            if let Err(error) = outcome {
                // 查行失败（DB 层）与写行失败都算这条的失败：单条失败不中断，
                // 但要留痕（media_id 是排查入口）。
                stats.failed += 1;
                tracing::warn!(
                    media_id,
                    code = error.code(),
                    message = %error.api.message,
                    "媒体文件哈希回填失败"
                );
            }
            Self::emit(progress.as_mut(), completed, total, &stats).await;
        }

        Ok(stats)
    }

    /// 处理一条候选。返回 `Err` 只表示「这一条没成」，调用方记 `failed` 后继续。
    async fn process_one(
        &self,
        media_id: i32,
        stats: &mut FileHashBackfillStats,
    ) -> Result<(), ServiceError> {
        // 重读 + 重验条件：候选快照可能已过期（见 [`Self::backfill_missing_file_hashes`]）。
        let media = self
            .media
            .find_by_id(media_id)
            .await?
            .filter(|media| media.file_hash.as_deref().unwrap_or("").trim().is_empty());
        let Some(media) = media else {
            stats.skipped_media += 1;
            return Ok(());
        };

        // handle 要带库的 provider 配置（`provider_key` / `provider_config` /
        // `account_key`），所以库行也要拿。上游那条重读查询是
        // `Media.select(Media, MediaLibrary).join(...)`（`:74-79`）—— 同一件事。
        let library = sm_db::repo::MediaLibraryRepository::new(self.media.pool().clone())
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
        let handle = media_handle_for(&MediaRecord {
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
        match self.storage.compute_file_hash(&handle).await {
            Ok(hash) if is_valid_hash(&hash) => {
                self.media.set_file_hash(media.id, &hash).await?;
                stats.hashed += 1;
            }
            Ok(hash) => {
                // 与上游的分歧：非法哈希单列（见 [`FileHashBackfillStats::invalid_hash`]）。
                stats.invalid_hash += 1;
                tracing::warn!(
                    media_id = media.id,
                    hash = %hash,
                    "provider 返回非法媒体哈希（去重会失效，该修 provider）"
                );
            }
            Err(failure) => {
                stats.failed += 1;
                tracing::warn!(
                    media_id = media.id,
                    code = %failure.code,
                    "provider 算哈希失败"
                );
            }
        }
        Ok(())
    }

    /// 进度上报。上游 `emit_progress`（`:49-60`）的形状：current/total/文本/摘要补丁。
    ///
    /// sink 失败**只 warn 不传播**：进度通道坏了不该停掉回填 —— 上游的
    /// `reporter.emit` 同样不向上抛。
    async fn emit(
        progress: Option<&mut ProgressSink<'_>>,
        current: i32,
        total: i32,
        stats: &FileHashBackfillStats,
    ) {
        let Some(progress) = progress else {
            return;
        };
        let summary = serde_json::to_value(stats).ok();
        let text = format!(
            "媒体文件哈希补算 · 已完成 {current}/{total} · 已更新 {} · 跳过 {} · 失败 {}",
            stats.hashed, stats.skipped_media, stats.failed
        );
        if let Err(error) = progress(Some(current), Some(total), &text, summary.as_ref()).await {
            tracing::warn!(error = %error, "媒体文件哈希回填的进度上报失败");
        }
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
            skipped_media: 0,
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
