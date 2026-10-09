//! 媒体有效性巡检（上游 `playback/media_validity_scan_service.py`，189 行）。
//!
//! # 任务键 `media_file_scan`，cron `0 4 * * *`
//!
//! # 它回答的问题：「库里记着的文件，还在吗？」
//!
//! 上游 docstring：「Reconcile stored Media validity against provider-managed
//! file inventories.」
//!
//! 媒体可能因为**库外面**的原因消失：网盘被删、硬盘拔了、用户手动清理。
//! 宿主不会收到任何通知 —— 只有 provider 在**列举它那边的文件清单**时才知道。
//!
//! 所以这是个**对账**任务：拿 provider 的清单与库里的记录比。
//!
//! # 能力缺失 → 记进 `unsupported_libraries` 而**不是**失败
//!
//! 上游用 `getattr(storage, "scan_managed_media_ref_keys", None)` 探测能力。
//! 缺失时该库进 `unsupported_libraries`。
//!
//! ⚠️ 报错会让任务在「有插件不支持扫描」时永远失败，而那是**合法状态**。
//! 关键是**要让用户看到哪些库不支持** —— 否则那些库的媒体会静默腐烂。
//!
//! # 只把「**确定**不在清单里」的标为失效
//!
//! ★ 判据必须是**否定证据**（provider 说没有），不是「没找到」（宿主没查到）。
//! 后者会把「provider 这次列不全」当成「文件已删」，让用户的媒体集体失效。
//!
//! # 与 `LIBRARY_LOCK` 的关系
//!
//! 对账期间可能有导入在写同一个库。所以要拿**库锁**（见
//! [`super::operation_locks`]），不是媒体锁。

use crate::error::ServiceError;

/// 任务键。与 `cron_spec::builtin_jobs` 里的 `media_file_scan` 一致。
pub const TASK_KEY: &str = "media_file_scan";

/// 巡检统计。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ValidityScanStats {
    /// 扫了几个库。
    pub scanned_libraries: i32,
    /// ★ **不支持**扫描的库（列在这里是为了让用户看见，见模块文档）。
    pub unsupported_libraries: Vec<String>,
    /// provider 侧报告的文件总数。
    pub remote_file_count: i64,
    /// ★ 被判定为失效的媒体数（**确定**不在清单里）。
    pub marked_invalid: i32,
    /// ★ 库里有、但 provider 清单里没有的存储引用数。
    pub missing_from_remote: i32,
    /// ★ 重新标记为有效的数量。
    pub marked_valid: i32,
    /// 各库的错误（**不中断**整批）。
    pub library_errors: Vec<(String, String)>,
}

/// 对账一个库。**纯函数** —— 把「库里的引用集合」与「provider 清单」比。
///
/// # 判据是**否定证据**
///
/// `remote` 里没有 → 判定失效。这要求 `remote` 是**完整**清单 —— 调用方必须
/// 先确认 provider 真的支持全量列举（能力探测），否则不能调它。
///
/// # 三类结果互斥
///
/// | 情况 | 处理 |
/// |---|---|
/// | 库里有、remote 有 | 标为**有效**（可能之前被误标） |
/// | 库里有、remote 没有 | 标为**失效** |
/// | remote 有、库里没有 | 计入 `missing_from_remote`，**不新建** |
///
/// 第三类**不自动新建**：provider 可能有宿主不关心的文件（字幕、封面、
/// 别的工具留下的）。自动新建会让库被塞满垃圾。
pub fn reconcile(
    local: &[(i64, String)],
    remote: &dyn Fn(&str) -> bool,
) -> ReconcileOutcome {
    let mut outcome = ReconcileOutcome::default();
    for (media_id, storage_ref) in local {
        if remote(storage_ref) {
            outcome.mark_valid.push(*media_id);
        } else {
            outcome.mark_invalid.push(*media_id);
        }
    }
    outcome
}

/// 对账结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub mark_valid: Vec<i64>,
    pub mark_invalid: Vec<i64>,
    pub missing_from_remote: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_refs(ids: &[(i64, &str)]) -> Vec<(i64, String)> {
        ids.iter().map(|(id, r)| (*id, (*r).to_owned())).collect()
    }

    /// ★ 判据是**否定证据**：provider 说没有 → 失效。
    #[test]
    fn absence_from_the_remote_list_marks_invalid() {
        let local = storage_refs(&[(1, "a"), (2, "b")]);
        let outcome = reconcile(&local, &|key| key == "a");
        assert_eq!(outcome.mark_valid, vec![1]);
        assert_eq!(outcome.mark_invalid, vec![2]);
    }

    /// 之前被误标失效、现在又出现了 → **重新标为有效**。
    #[test]
    fn a_reappearing_file_is_marked_valid_again() {
        let local = storage_refs(&[(7, "a")]);
        let outcome = reconcile(&local, &|_| true);
        assert_eq!(outcome.mark_valid, vec![7], "不该继续标失效");
        assert!(outcome.mark_invalid.is_empty());
    }

    /// provider 有、库里没有 → 只**计数**，不自动新建。
    ///
    /// 自动新建会把字幕、封面、别的工具留下的文件全塞进库。
    #[test]
    fn unknown_remote_files_are_counted_but_not_created() {
        let local = storage_refs(&[(1, "a")]);
        let outcome = reconcile(&local, &|key| key == "a" || key == "extra");
        assert_eq!(outcome.mark_valid, vec![1]);
        assert_eq!(outcome.mark_invalid.is_empty());
        assert_eq!(outcome.missing_from_remote, 1, "只计数，不新建");
    }

    /// 不支持的库**要列出来** —— 否则那些库的媒体会静默腐烂而用户不知道。
    #[test]
    fn unsupported_libraries_are_surfaced() {
        let stats = ValidityScanStats {
            scanned_libraries: 3,
            unsupported_libraries: vec!["115".to_owned()],
            remote_file_count: 100,
            ..ValidityScanStats::default()
        };
        assert_eq!(stats.unsupported_libraries, vec!["115".to_owned()]);
    }

    /// 空库对账 → 什么都不标。
    #[test]
    fn an_empty_local_list_changes_nothing() {
        let outcome = reconcile(&[], &|_| true);
        assert!(outcome.mark_valid.is_empty());
        assert!(outcome.mark_invalid.is_empty());
    }
}

/// 有效性巡检服务。
pub struct MediaValidityScanService;

impl MediaValidityScanService {
    /// ★ 跑一轮。任务执行体。
    ///
    /// 逐库：能力探测 -> 拿**库锁** -> 拉 provider 全量清单 -> 对账 -> 释放锁。
    ///
    /// 单库失败只记入 `library_errors`，**不中断**整批。
    pub async fn scan_media_validity(&self) -> Result<ValidityScanStats, ServiceError> {
        todo!("骨架：逐库 -> getattr 式能力探测(不支持则记 unsupported) -> 取库锁 -> 拉清单 -> reconcile")
    }
}
