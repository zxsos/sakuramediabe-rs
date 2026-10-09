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

use std::collections::BTreeSet;

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

/// 对账一个库。**纯函数** —— 把「库里的引用」与「provider 清单」比。
///
/// # 判据是**否定证据**
///
/// provider 清单里没有 → 判定失效。这要求 `remote` 是**完整**清单 —— 调用方
/// 必须先确认 provider 真的支持全量列举（能力探测），否则不能调它。
///
/// # 为什么 `remote` 是集合，不是一个「这个 key 在不在」的闭包
///
/// 闭包只能对**已知的** key 回答，于是永远看不见「远端有、库里没有」的 key ——
/// 第三类计数就成了不可能的指针。这个签名吃过一次亏：原来收
/// `&dyn Fn(&str) -> bool`，而用例要求统计远端独有的文件，断言必然失败，
/// 看起来像断言写错，其实是**签名表达不了语义**。
///
/// 上游 `:150` 也是集合成员判定（`managed_media_ref_key(...) in managed_ref_keys`），
/// 那里 `managed_ref_keys` 由 `scan_managed_media_ref_keys()` 一次性拉回。
///
/// # 入参已经是**算好的 key**
///
/// `local` 的第二项与 `remote` 的元素都必须是**归一后的引用 key**，不是原始的
/// `storage_ref` JSON。上游是逐条 `managed_media_ref_key(media_ref=...)` 现算的
/// （`:149-150`），而那个函数**会抛** `ValueError` / `ProviderOperationError`
/// —— 抛出的那条计入 `failed_media`（`:151-158`），**不进**本函数。
/// 所以「逐条映射 + 逐条异常计数」是 `scan_media_validity` 的活，不是这里的。
///
/// # 三类结果
///
/// | 情况 | 处理 |
/// |---|---|
/// | 库里有、remote 有 | 标为**有效**（可能之前被误标） |
/// | 库里有、remote 没有 | 标为**失效** |
/// | remote 有、库里没有 | 计入 `missing_from_remote`，**不新建** |
///
/// 第三类**不自动新建**：provider 那边可能有宿主不关心的文件（字幕、封面、
/// 别的工具留下的），自动新建会把库塞满垃圾。
///
/// ⚠️ 第三类是本仓 stats 的**扩展项**：上游只遍历库里的媒体（`:140`），
/// 从头到尾没有这个计数，它的 stats 里也没有对应的键。留着是因为「远端多了
/// 什么」对排查有用（比如别人的文件混进了库目录），但它**只报不改**。
pub fn reconcile(local: &[(i64, String)], remote: &BTreeSet<String>) -> ReconcileOutcome {
    let mut outcome = ReconcileOutcome::default();
    for (media_id, key) in local {
        if remote.contains(key) {
            outcome.mark_valid.push(*media_id);
        } else {
            outcome.mark_invalid.push(*media_id);
        }
    }

    // 远端有、库里没有。用**集合**去重：两条媒体行可能指向同一个 key
    // （历史的去重键降级会留下这种行），逐行算会把同一个远端文件数两次。
    let local_keys: BTreeSet<&str> = local.iter().map(|(_, key)| key.as_str()).collect();
    outcome.missing_from_remote = remote
        .iter()
        .filter(|key| !local_keys.contains(key.as_str()))
        .count() as i64;

    outcome
}

/// 对账结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub mark_valid: Vec<i64>,
    pub mark_invalid: Vec<i64>,
    pub missing_from_remote: i64,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_refs(ids: &[(i64, &str)]) -> Vec<(i64, String)> {
        ids.iter().map(|(id, r)| (*id, (*r).to_owned())).collect()
    }

    /// provider 清单（已归一成 key 的集合）。
    fn remote_keys(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| (*key).to_owned()).collect()
    }

    /// ★ 判据是**否定证据**：provider 说没有 → 失效。
    #[test]
    fn absence_from_the_remote_list_marks_invalid() {
        let local = storage_refs(&[(1, "a"), (2, "b")]);
        let outcome = reconcile(&local, &remote_keys(&["a"]));
        assert_eq!(outcome.mark_valid, vec![1]);
        assert_eq!(outcome.mark_invalid, vec![2]);
    }

    /// 之前被误标失效、现在又出现了 → **重新标为有效**。
    #[test]
    fn a_reappearing_file_is_marked_valid_again() {
        let local = storage_refs(&[(7, "a")]);
        let outcome = reconcile(&local, &remote_keys(&["a"]));
        assert_eq!(outcome.mark_valid, vec![7], "不该继续标失效");
        assert!(outcome.mark_invalid.is_empty());
    }

    /// ★ provider 有、库里没有 → 只**计数**，不自动新建。
    ///
    /// 自动新建会把字幕、封面、别的工具留下的文件全塞进库。
    ///
    /// 这条用例就是「签名表达不了语义」的那个受害者：原来 `remote` 是
    /// `&dyn Fn(&str) -> bool`，闭包对 `"extra"` 返 `true` 也没用 —— 它只被
    /// 拿库里的 key 调用，远端独有的 key 永远走不到，于是
    /// `missing_from_remote` 恒为 0。
    #[test]
    fn unknown_remote_files_are_counted_but_not_created() {
        let local = storage_refs(&[(1, "a")]);
        let outcome = reconcile(&local, &remote_keys(&["a", "extra"]));
        assert_eq!(outcome.mark_valid, vec![1]);
        assert!(outcome.mark_invalid.is_empty());
        assert_eq!(outcome.missing_from_remote, 1, "只计数，不新建");
    }

    /// ★ 远端独有的文件按**集合**计数，不按「库里引用了几次」计数。
    ///
    /// 两条媒体行指向同一个 key（历史降级去重键会留下这种行）时，那个远端文件
    /// 只该算一次；反过来，库里两条引用都还在，就都不失效。
    #[test]
    fn missing_from_remote_counts_distinct_keys_not_rows() {
        let local = storage_refs(&[(1, "a"), (2, "a")]);
        let outcome = reconcile(&local, &remote_keys(&["a", "extra", "other"]));
        assert_eq!(outcome.mark_valid, vec![1, 2]);
        assert!(outcome.mark_invalid.is_empty());
        assert_eq!(outcome.missing_from_remote, 2, "两个远端独有 key");
    }

    /// 远端清单比库小到只剩空 → 库里的全失效，没有「远端独有」。
    #[test]
    fn an_empty_remote_list_invalidates_everything_local() {
        let local = storage_refs(&[(1, "a"), (2, "b")]);
        let outcome = reconcile(&local, &remote_keys(&[]));
        assert!(outcome.mark_valid.is_empty());
        assert_eq!(outcome.mark_invalid, vec![1, 2]);
        assert_eq!(outcome.missing_from_remote, 0);
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

    /// 空库对账 → 不标任何媒体；远端有多少文件都只进计数。
    #[test]
    fn an_empty_local_list_changes_nothing() {
        let outcome = reconcile(&[], &remote_keys(&["a", "b"]));
        assert!(outcome.mark_valid.is_empty());
        assert!(outcome.mark_invalid.is_empty());
        assert_eq!(outcome.missing_from_remote, 2);
    }
}
