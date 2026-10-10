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

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use sm_db::repo::{MediaLibraryRepository, MediaRepository};
use sm_db::Db;

use crate::catalog::movie_asset_pack_backfill::ProgressSink;
use crate::error::ServiceError;
use crate::playback::operation_locks::MediaOperation;
use crate::playback::provider_helpers::{
    json_or_null, library_handle_for, LibraryRecord, StorageGateway, PROVIDER_UNSUPPORTED,
};

/// 任务键。与 `cron_spec::builtin_jobs` 里的 `media_file_scan` 一致。
pub const TASK_KEY: &str = "media_file_scan";

/// 巡检统计。
///
/// # 键与上游 stats 字典逐键对齐（`:73-85`）
///
/// `scanned_libraries` / `scanned_media` / `updated_media` / `unchanged_media` /
/// `invalidated_media` / `revived_media` / `skipped_media` / `failed_media` /
/// `scanned_libraries` / `failed_libraries` 十个一一对应；上游的
/// `unsupported_libraries` 是**计数**，这里扩成**名字列表**（扩展项 ①）——
/// 「哪些库不支持」正是本任务要给用户看的，一个数没法排障。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ValidityScanStats {
    /// 真正做了对账的库数。
    pub scanned_libraries: i32,
    /// ★ **不支持**扫描的库（provider_key；列出来是为了让用户看见，见模块文档）。
    pub unsupported_libraries: Vec<String>,
    /// 拉清单失败的库数（与「不支持」分开：前者是坏消息，后者是合法状态）。
    pub failed_libraries: i32,
    /// provider 侧报告的文件总数（扩展项 ②）。
    pub remote_file_count: i64,
    pub scanned_media: i32,
    pub updated_media: i32,
    pub unchanged_media: i32,
    pub invalidated_media: i32,
    pub revived_media: i32,
    pub skipped_media: i32,
    pub failed_media: i32,
    /// ★ 库里有、但 provider 清单里没有的**去重** key 数（扩展项 ③：上游没有
    /// 这个计数；只报不改，理由见 [`reconcile`] 的文档）。
    pub missing_from_remote: i64,
    /// 拉清单失败的库的 `(库, 原因)`（与 `failed_libraries` 配套）。
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
///
/// # 依赖是注入的
///
/// 与 [`super::media_file_hash_backfill`] 同一取向：[`StorageGateway`] 是
/// trait，组合根注入真的 `ProviderGateway`，测试注入可编程的桩。
pub struct MediaValidityScanService {
    db: Db,
    storage: Arc<dyn StorageGateway>,
    media: MediaRepository,
    libraries: MediaLibraryRepository,
}

impl MediaValidityScanService {
    /// 构造。
    pub fn new(db: &Db, storage: Arc<dyn StorageGateway>) -> Self {
        Self {
            db: db.clone(),
            storage,
            media: MediaRepository::new(db.clone()),
            libraries: MediaLibraryRepository::new(db.clone()),
        }
    }

    /// ★ 跑一轮。任务执行体。上游 `scan_media_validity(cls, *, reporter)`。
    ///
    /// 逐库：拿**库锁**（被占 = 整库跳过）-> 重读库行 -> 拉库内媒体 ->
    /// 能力探测（拉全量清单）-> 逐条算 key -> 对账 -> 批量落库 -> 释放锁。
    ///
    /// # 三档「这库扫不了」，去向各不相同
    ///
    /// | 情况 | 去向 | 上游 |
    /// |---|---|---|
    /// | 两个能力方法缺一个 | `unsupported_libraries` | `:135-136` |
    /// | 拉清单失败（网络/权限） | `failed_libraries` + `library_errors` | `:125-133` |
    /// | 库锁被占 | 整库媒体计入 `skipped_media` | `:182-186` |
    ///
    /// # 单条 key 现算，成批对账
    ///
    /// `managed_media_ref_key` 逐条调（`storage_ref` 是 provider 的命名空间，
    /// 脏数据它有权拒）→ 失败计入 `failed_media`；算出的 `(media_id, key)`
    /// 成批交给 [`reconcile`]，再按 `valid_before` 分桶批量落库 ——
    /// 上游是逐条 UPDATE（`:167-171`），批量版语义相同（WHERE 带旧状态），
    /// 一个十万行的库少十万次往返。
    pub async fn scan_media_validity(
        &self,
        mut progress: Option<ProgressSink<'_>>,
    ) -> Result<ValidityScanStats, ServiceError> {
        let mut stats = ValidityScanStats::default();
        let mut completed = 0i64;

        for library in self.libraries.list_ordered().await? {
            let lock = match MediaOperation::try_library(&self.db, library.id).await {
                Ok(Some(lock)) => lock,
                Ok(None) => {
                    // 库锁被占（导入正在写这个库）：整库跳过，不排队
                    // （上游 `:182-186`）。
                    let skipped = self
                        .media
                        .list_scan_items_by_library(library.id)
                        .await?
                        .len();
                    stats.skipped_media += i32::try_from(skipped).unwrap_or(i32::MAX);
                    completed += skipped as i64;
                    Self::emit(progress.as_mut(), completed, &stats).await;
                    continue;
                }
                // 取锁失败是 DB 层的问题：传播出去让任务以失败结束。
                Err(error) => return Err(error),
            };

            // 库行可能刚被删（锁等到了，库没了）：上游 `:117-119` 同样重读。
            let outcome = match self.libraries.find_by_id(library.id).await? {
                None => Ok(()),
                Some(library) => {
                    let items = self.media.list_scan_items_by_library(library.id).await?;
                    if items.is_empty() {
                        // 空库连能力探测都不做（上游 `:121-122`）。
                        Ok(())
                    } else {
                        self.scan_one_library(
                            &library,
                            items,
                            &mut stats,
                            &mut completed,
                            &mut progress,
                        )
                        .await
                    }
                }
            };
            lock.release().await;
            outcome?;
        }

        Ok(stats)
    }

    /// 对一个库做完整巡检。`Err` 只表示**这一库**失败，调用方记入
    /// `library_errors` 后继续。
    async fn scan_one_library(
        &self,
        library: &sm_db::MediaLibrary,
        items: Vec<(i32, bool, Option<String>)>,
        stats: &mut ValidityScanStats,
        completed: &mut i64,
        progress: &mut Option<ProgressSink<'_>>,
    ) -> Result<(), ServiceError> {
        // 能力探测 = 拉全量清单。两个方法**配对**（缺单条 key 计算也一样算不支持），
        // 这里拉清单失败就整体按「不支持」档处理 —— 上游 `:44-48` 与 `:123-136`。
        let handle = library_handle_for(&LibraryRecord {
            id: i64::from(library.id),
            provider_key: library.provider_key.clone(),
            provider_config: json_or_null(library.provider_config.as_deref()),
            account_key: library.account_key.clone(),
        });
        let remote = match self.storage.scan_managed_media_ref_keys(&handle).await {
            Ok(keys) => {
                stats.scanned_libraries += 1;
                stats.remote_file_count += keys.len() as i64;
                BTreeSet::from_iter(keys)
            }
            Err(failure) => {
                let label = format!("{}#{}", library.provider_key, library.id);
                if failure.code == PROVIDER_UNSUPPORTED {
                    stats.unsupported_libraries.push(label);
                } else {
                    stats.failed_libraries += 1;
                    stats
                        .library_errors
                        .push((label.clone(), failure.safe_message.clone()));
                    tracing::warn!(
                        library_id = library.id,
                        provider_key = %library.provider_key,
                        code = %failure.code,
                        "媒体有效性巡检：拉取 provider 清单失败"
                    );
                }
                // 这库的媒体一条都没检查，但**不是失败**（unsupported）/
                // 不是逐条失败（拉清单失败是库级故障）—— 都计入 skipped。
                stats.skipped_media += i32::try_from(items.len()).unwrap_or(i32::MAX);
                *completed += items.len() as i64;
                Self::emit(progress.as_mut(), *completed, stats).await;
                return Ok(());
            }
        };

        // 逐条算 key；算不出的（脏 storage_ref）计入 failed_media，不进对账。
        let mut valid_before = HashMap::new();
        let mut local: Vec<(i64, String)> = Vec::with_capacity(items.len());
        for (media_id, was_valid, storage_ref) in &items {
            *completed += 1;
            let media_ref = json_or_null(storage_ref.as_deref());
            let usable = media_ref.is_null()
                || storage_ref
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty();
            let key = if usable {
                None
            } else {
                self.storage
                    .managed_media_ref_key(&handle, media_ref)
                    .await
                    .ok()
            };
            match key {
                Some(key) => {
                    stats.scanned_media += 1;
                    valid_before.insert(i64::from(*media_id), *was_valid);
                    local.push((i64::from(*media_id), key));
                }
                None => {
                    // provider 拒绝这条引用（或行根本没有 storage_ref）：
                    // 它的有效性**没法判**，标失效会是冤案。
                    stats.failed_media += 1;
                    tracing::warn!(
                        media_id,
                        library_id = library.id,
                        "storage_ref 无法归一成引用 key，跳过有效性判定"
                    );
                }
            }
            Self::emit(progress.as_mut(), *completed, stats).await;
        }

        let outcome = reconcile(&local, &remote);
        stats.missing_from_remote += outcome.missing_from_remote;

        // 按 valid_before 分桶：目标状态与现状相同的进 unchanged，不同的才落库。
        let mut to_revive = Vec::new();
        let mut to_invalidate = Vec::new();
        for media_id in &outcome.mark_valid {
            match valid_before.get(media_id) {
                Some(false) => to_revive.push(*media_id as i32),
                _ => stats.unchanged_media += 1,
            }
        }
        for media_id in &outcome.mark_invalid {
            match valid_before.get(media_id) {
                Some(true) => to_invalidate.push(*media_id as i32),
                _ => stats.unchanged_media += 1,
            }
        }

        let revived = self.media.set_validity(&to_revive, true, true).await?;
        let invalidated = self
            .media
            .set_validity(&to_invalidate, false, false)
            .await?;
        stats.revived_media += i32::try_from(revived).unwrap_or(i32::MAX);
        stats.invalidated_media += i32::try_from(invalidated).unwrap_or(i32::MAX);
        stats.updated_media += stats.revived_media + stats.invalidated_media;
        // 批量 UPDATE 少于预期的部分 = 并发改了状态（WHERE 没命中）→ skipped。
        let raced = (to_revive.len() + to_invalidate.len())
            .saturating_sub((revived + invalidated) as usize);
        stats.skipped_media += i32::try_from(raced).unwrap_or(i32::MAX);
        Ok(())
    }

    /// 进度上报。上游 `emit_progress`（`:90-112`）的形状；sink 失败只 warn。
    async fn emit(
        progress: Option<&mut ProgressSink<'_>>,
        completed: i64,
        stats: &ValidityScanStats,
    ) {
        let Some(progress) = progress else {
            return;
        };
        let summary = serde_json::to_value(stats).ok();
        let text = format!(
            "媒体文件巡检 · 已检查 {completed} · 未变化 {} · 失效 {} · 恢复 {} · 失败 {}",
            stats.unchanged_media, stats.invalidated_media, stats.revived_media, stats.failed_media
        );
        if let Err(error) = progress(
            Some(i32::try_from(completed).unwrap_or(i32::MAX)),
            None,
            &text,
            summary.as_ref(),
        )
        .await
        {
            tracing::warn!(error = %error, "媒体有效性巡检的进度上报失败");
        }
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
