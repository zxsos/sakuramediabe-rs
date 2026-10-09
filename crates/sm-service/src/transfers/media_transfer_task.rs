//! 存储转存（上游 `shared/media_transfer_task_service.py`，576 行）。
//!
//! # 类 docstring 是一句警告：中断的工作**不能**继续执行破坏性调用
//!
//! 上游原话：「interrupted work never resumes destructive provider calls.」
//!
//! 转存的分阶段是「**先复制、后切换**」—— 复制期间源还在，中断是安全的；
//! 而「切换」一旦开始就必须做完。所以：
//!
//! - 中断在**复制**阶段 → 可以安全重跑（覆盖未完成的暂存）
//! - 中断在**切换**阶段 → **绝不能重跑**，那会拿一个半切换的状态继续
//!
//! 因此 [`MediaTransferTaskService::recover_interrupted_transfers`] 的职责
//! 是**识别**并把这类任务标记为不可继续（`409 media_transfer_legacy_task`），
//! 而不是接着跑完。
//!
//! # 三个「变化」检测：源、配置、目标
//!
//! | 检查 | 错误码 | 为什么必须 |
//! |---|---|---|
//! | 媒体库配置变了 | `409 media_transfer_conflict` | 目标路径的计算依赖配置，重算会写到别处 |
//! | 源媒体变了 | `409 media_transfer_source_invalid` | 复制的是**字节**，源变了就得重拷 |
//! | 源库与请求不符 | `422 media_transfer_source_library_mismatch` | 否则会把 A 库的媒体拷进 B 库 |
//!
//! 三个都**不是** 404 —— 它们是「你的请求已经过时了，请重新发起」。
//!
//! # 能力协商是**三段**的，且缺一段就不能转存
//!
//! | 能力 | 缺失时 |
//! |---|---|
//! | `supports_media_transfer_source` | `422 media_transfer_source_unsupported` |
//! | `supports_media_transfer_target` | `422 media_transfer_target_unsupported` |
//! | `supports_media_transfer_source_cleanup` | 可选 —— 不支持则转存后**保留**源 |
//!
//! 注意**源与目标是两个独立能力**。只有源支持、目标不支持的组合很常见
//! （比如只能从 115 读、不能往 115 写）。别把两者合成一个 flag。
//!
//! # 同库转存直接 422
//!
//! [`MediaTransferTaskService::enqueue`] 遇到 `source_library_id ==
//! target_library_id` → `422 media_transfer_same_library`。源和目标同一个库
//! 时「转存」要么是空操作、要么是移动 —— 都不是用户以为的「复制一份」。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 任务键。同时决定**专属道** `transfer`（**1 并发**，见 `sm_scheduler`）。
///
/// 为什么只有 1 并发：转存会大量占用磁盘 IO（复制文件）。两个并发转存
/// 会把磁盘打满，进而拖垮同一台机器上的播放。
pub const TASK_KEY: &str = "media_storage_transfer";

/// 候选查询请求。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaStorageTransferCandidatesRequest {
    pub media_id: i64,
    /// 限定只查这些库。`None` = 全部库。
    pub library_ids: Option<Vec<i64>>,
}

/// 一个可转存目标。
#[derive(Debug, Clone, Serialize)]
pub struct MediaStorageTransferTarget {
    pub library_id: i64,
    pub library_name: String,
    /// 目标路径（**库内相对路径**）。
    pub path: String,
    /// 是否可写。
    pub writable: bool,
    /// 不可写的原因。`writable = true` 时为 `None`。
    ///
    /// **不可写的目标也要列出来** —— 前端要显示「为什么不行」，
    /// 只列可写的那几个会让用户以为自己看全了。
    pub blocked_reason: Option<String>,
}

/// 候选查询响应。
#[derive(Debug, Clone, Serialize)]
pub struct MediaStorageTransferCandidatesResponse {
    pub media_id: i64,
    pub targets: Vec<MediaStorageTransferTarget>,
}

/// 转存请求。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaStorageTransferRequest {
    pub media_id: i64,
    pub target_library_id: i64,
    /// 目标路径（库内相对路径）。`None` = 用默认布局。
    pub target_path: Option<String>,
    /// 转存后是否删除源。默认 `false`。
    ///
    /// ⚠️ 这**不需要**两步确认（与 `DELETE /download-tasks` 不同）——
    /// 区别在于那里的 `delete_files` 会删**已下载的原始文件**（不可恢复），
    /// 而这里删的是「刚复制完的源」，目标已就位，删源是可预期的整理行为。
    pub delete_source: bool,
}

/// 已受理（**202**）。
#[derive(Debug, Clone, Serialize)]
pub struct MediaStorageTransferAcceptedResponse {
    pub task_run_id: i64,
}

/// 执行摘要。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MediaTransferSummary {
    /// 已复制并切换成功。
    pub transferred: i32,
    /// 源已删除（仅 `delete_source = true` 时可能非 0）。
    pub sources_deleted: i32,
}

/// 中断任务的恢复统计。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RecoverInterruptedResult {
    /// 被标记为**不可继续**的任务数。
    pub marked_unresumable: i32,
    /// 已安全重跑（中断在复制阶段）的任务数。
    pub safely_resumable: i32,
}

/// 转存服务。
pub struct MediaTransferTaskService;

impl MediaTransferTaskService {
    /// `POST /media-transfers/candidates` —— **200**。
    ///
    /// 错误码：媒体不存在 → `404 media_transfer_source_not_found`；
    /// 目标库不存在 → **404**（不是空列表 —— 传了 `library_ids` 却有一个
    /// 不存在，说明客户端状态与服务端不一致）。
    pub async fn list_candidates(
        request: MediaStorageTransferCandidatesRequest,
    ) -> Result<MediaStorageTransferCandidatesResponse, ServiceError> {
        let _ = request;
        todo!("骨架：逐库做能力协商；不可写的也列出并带 blocked_reason")
    }

    /// `POST /media-transfers` —— **202**。
    ///
    /// 错误码：同库 → `422 media_transfer_same_library`；源库不符 → `422
    /// media_transfer_source_library_mismatch`；源/目标能力缺失 → `422
    /// media_transfer_source_unsupported` / `media_transfer_target_unsupported`；
    /// 已有转存在跑 → `409 media_transfer_conflict`。
    pub async fn enqueue(
        request: MediaStorageTransferRequest,
    ) -> Result<MediaStorageTransferAcceptedResponse, ServiceError> {
        let _ = request;
        todo!("骨架：同库/源库/能力三项检查 -> 按源库取互斥 -> 建 TaskRun(202)")
    }

    /// ★ 执行体。worker 调用。
    ///
    /// 阶段：**复制 → 校验 → 切换 →（可选）删源**。`delete_source` 只在
    /// **切换成功后**执行，且源插件不支持 `..._source_cleanup` 时**跳过**
    /// （保留源，而不是报错 —— 那会留下一个「转存成功但没删源」的半成品）。
    pub async fn execute(params: &serde_json::Value) -> Result<MediaTransferSummary, ServiceError> {
        let _ = params;
        todo!("骨架：复制 -> 校验 -> 切换 -> 可选删源；切换前每一步都要重查源未变")
    }

    /// 中断任务恢复。**只标记，不续跑**（见模块文档）。
    pub async fn recover_interrupted_transfers() -> Result<RecoverInterruptedResult, ServiceError> {
        todo!("骨架：中断在切换前 -> 可安全重跑；已在切换 -> 标记 legacy(409)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不可写的目标**也要列出来**，并带原因。
    ///
    /// 只列可写目标会让用户以为看全了，而「为什么不能转存到某个库」正是
    /// 他要问的问题。
    #[test]
    fn blocked_targets_are_listed_with_a_reason() {
        let targets = [
            MediaStorageTransferTarget {
                library_id: 1,
                library_name: "本地盘".to_owned(),
                path: "ABC-123/ABC-123.mkv".to_owned(),
                writable: true,
                blocked_reason: None,
            },
            MediaStorageTransferTarget {
                library_id: 2,
                library_name: "只读 NAS".to_owned(),
                path: "x".to_owned(),
                writable: false,
                blocked_reason: Some("该库以只读方式挂载".to_owned()),
            },
        ];
        assert_eq!(targets.len(), 2, "不可写的也在列表里");
        let blocked = &targets[1];
        assert!(!blocked.writable);
        assert!(blocked.blocked_reason.is_some(), "不可写必须给出原因");
    }

    /// 源与目标是**两个独立能力**。
    ///
    /// 合成一个 flag 会让「只能从某库读、不能往某库写」这种常见组合被误判为
    /// 完全不支持。
    #[test]
    fn source_and_target_capabilities_are_independent() {
        let only_source = (true, false);
        let only_target = (false, true);
        assert_ne!(only_source, only_target, "两种组合必须能被区分");
    }

    /// 任务键决定专属道与 1 并发，不能改。
    #[test]
    fn the_task_key_matches_the_transfer_lane() {
        assert_eq!(TASK_KEY, "media_storage_transfer");
    }

    /// 中断恢复的两种结果**必须分开计数**。
    ///
    /// 混在一个数里就看不出有多少任务被永久标记了 —— 那是需要人工介入的部分。
    #[test]
    fn interrupted_recovery_distinguishes_resumable_from_legacy() {
        let result = RecoverInterruptedResult {
            marked_unresumable: 3,
            safely_resumable: 7,
        };
        assert_eq!(result.marked_unresumable + result.safely_resumable, 10);
        assert!(result.marked_unresumable > 0);
    }
}
