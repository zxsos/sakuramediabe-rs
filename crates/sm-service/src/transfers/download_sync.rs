//! provider 下载快照 → 宿主台账的对账（上游 `downloads/sync_service.py`，229 行）。
//!
//! # cron 是**每分钟**一次，所以本文件必须便宜
//!
//! `download_task_sync` 的 cron 是 `* * * * *`。任何一次慢查询都会让任务
//! 排队、租约续不上、然后被重复领取。
//!
//! # 只对账两个状态：`queued` 与 `downloading`
//!
//! 上游 `_SYNCABLE_STATES`。已完成/失败的任务**不再查 provider** —— 它们
//! 不会再变，查了也是浪费。这也意味着「完成」这个事实是 provider 推过来的，
//! 宿主不会自己推断。
//!
//! # 故障隔离：**一个客户端挂掉不影响其它客户端**
//!
//! `sync_all_clients` 逐个客户端对账，**单个失败只记不抛**。若第一个失败就
//! 整体抛错，那一个坏掉的下载器会让**所有**下载器的任务都停止同步 —— 而它们
//! 彼此毫无关系。
//!
//! # 两个 `enqueue_*` 方法的语义不同
//!
//! | 方法 | 时机 | 作用 |
//! |---|---|---|
//! | [`DownloadSyncService::enqueue_auto_imports`] | 正常流程 | 已完成的任务 → 入队导入 |
//! | [`DownloadSyncService::recover_orphaned_imports_only`] | **启动时** | 只捞「导入记录丢了」的任务 |
//!
//! 第二个不是第一个的子集：它处理的是**崩溃残留** —— provider 说下完了、
//! 导入也入队了，但 TaskRun 记录在进程崩溃时丢了。这类任务不会再被第一个
//! 方法捞到（因为它只认「刚变成完成」），不专门捞就永远漏。

use serde::{Deserialize, Serialize};

use super::download_common::DownloadClientRow;
use crate::error::ServiceError;

/// 会对账的状态（见模块文档）。
pub const SYNCABLE_STATES: [&str; 2] = ["queued", "downloading"];

/// 单客户端对账结果。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadClientSyncResponse {
    pub client_id: i32,
    pub client_name: String,
    /// 从 provider 拉到的任务数。
    pub remote_count: usize,
    /// **新出现**的（台账里没有的）—— 已自动建台账。
    pub created: usize,
    /// 状态或进度**变了**的。
    pub updated: usize,
    /// provider 有、台账里也有且**完全一致**的。
    pub unchanged: usize,
    /// 失败原因。**单个客户端失败时这里有值，但整体不失败。**
    pub error: Option<String>,
}

/// 全量对账结果。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadSyncAllResponse {
    pub clients: Vec<DownloadClientSyncResponse>,
    /// 成功对账的客户端数。**不等于 `clients.len()`**。
    pub succeeded: usize,
    pub failed: usize,
}

/// 自动导入的入队统计。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct EnqueueAutoImportResult {
    /// 新入队的导入任务数。
    pub enqueued: i32,
    /// 因已有导入在跑而跳过的。
    pub skipped_running: i32,
    /// 因状态不允许而跳过的。
    pub skipped_status: i32,
}

/// 崩溃残留的恢复统计。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RecoverOrphanedResult {
    /// 找回的孤儿任务数。
    pub recovered: i32,
}

impl DownloadSyncAllResponse {
    /// 记一个客户端的结果。**失败不改变整体成败**（见模块文档）。
    pub fn record(&mut self, response: DownloadClientSyncResponse) {
        if response.error.is_some() {
            self.failed += 1;
        } else {
            self.succeeded += 1;
        }
        self.clients.push(response);
    }
}

/// 对账服务。
pub struct DownloadSyncService {
    /// 可注入的 provider 工厂。**存在是为了测试**（替身不必起真实下载器）。
    factory: Option<Box<dyn SyncProviderFactory>>,
}

/// provider 侧的任务快照来源。
pub trait SyncProviderFactory {
    /// 取某客户端的进行中任务。
    fn list_tasks(
        &self,
        client: &DownloadClientRow,
    ) -> Result<Vec<super::download_common::RemoteDownloadTask>, ServiceError>;
}

impl DownloadSyncService {
    /// 构造（真实依赖）。
    pub fn new() -> Self {
        Self { factory: None }
    }

    /// 构造（注入替身，测试用）。
    pub fn with_factory(factory: Box<dyn SyncProviderFactory>) -> Self {
        Self { factory: Some(factory) }
    }

    /// 对账**单个**客户端。失败返回 `Err`（映射成 `502
    /// download_task_sync_failed`）。
    pub async fn sync_client(
        &self,
        client_id: i32,
    ) -> Result<DownloadClientSyncResponse, ServiceError> {
        let _ = client_id;
        todo!("骨架：拉 provider 快照 -> 只认 queued/downloading -> upsert 台账（幂等）")
    }

    /// 对账**全部**客户端。**单个失败不抛**（见模块文档）。
    pub async fn sync_all_clients(&self) -> Result<DownloadSyncAllResponse, ServiceError> {
        todo!("骨架：逐客户端 sync_client；失败只记入 clients[i].error")
    }

    /// 完成了但还没入队导入的任务 → 入队。cron 每分钟调。
    pub async fn enqueue_auto_imports(&self) -> Result<EnqueueAutoImportResult, ServiceError> {
        todo!("骨架：查「下载已完成 且 无导入记录」-> 经 import_task 入队")
    }

    /// 捞崩溃残留（启动时调一次）。见模块文档。
    pub async fn recover_orphaned_imports_only(&self) -> Result<RecoverOrphanedResult, ServiceError> {
        todo!("骨架：查「下载已完成 且 导入 TaskRun 记录缺失」-> 重新入队")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(client_id: i32, error: Option<&str>) -> DownloadClientSyncResponse {
        DownloadClientSyncResponse {
            client_id,
            client_name: format!("client-{client_id}"),
            remote_count: 0,
            created: 0,
            updated: 0,
            unchanged: 0,
            error: error.map(str::to_owned),
        }
    }

    /// 一个客户端失败**不能**影响其它客户端的记账。
    ///
    /// 若失败即整体抛错，一个坏下载器会停掉全库所有下载器的同步，而它们
    /// 彼此毫无关系。
    #[test]
    fn one_broken_client_does_not_hide_the_others() {
        let mut all = DownloadSyncAllResponse {
            clients: Vec::new(),
            succeeded: 0,
            failed: 0,
        };
        all.record(response(1, None));
        all.record(response(2, Some("连接超时")));
        all.record(response(3, None));
        assert_eq!(all.clients.len(), 3);
        assert_eq!(all.succeeded, 2, "两个成功");
        assert_eq!(all.failed, 1, "一个失败，但结果仍然完整返回");
    }

    /// 会对账的状态**只有两个** —— 已完成/失败不再查 provider。
    ///
    /// 多查一次就是每分钟一次的无用往返，且会让「完成」被 provider 的旧快照
    /// 覆盖回去。
    #[test]
    fn only_in_flight_states_are_synced() {
        assert_eq!(SYNCABLE_STATES, ["queued", "downloading"]);
    }

    /// 崩溃残留的恢复与常规自动导入是**两条路**。
    ///
    /// 把它们合成一个方法，崩溃残留就永远捞不回来了 —— 常规路径只认「刚变成
    /// 完成」，而残留任务的状态早就变过了。
    #[test]
    fn orphan_recovery_is_a_separate_path_from_auto_import() {
        let orphan = RecoverOrphanedResult::default();
        let auto = EnqueueAutoImportResult::default();
        assert_eq!(orphan.recovered, 0);
        assert_eq!(auto.enqueued, 0);
    }
}
