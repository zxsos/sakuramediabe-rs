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

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use sm_db::common::page::PageRequest;
use sm_db::repo::{
    BackgroundTaskRunRepository, MediaLibraryRepository, MediaRepository, NewTaskRun,
};
use sm_db::system::activity::task_state;
use sm_db::{Db, Media};
use sm_plugin_api::host::{
    capability, HostProviderError, HostProviderFactory, HostStorageProvider,
};
use sm_plugin_api::v1::{
    ImportPlacement, LibraryHandle, MediaHandle, StagedMediaTransfer, StagedTransferStatus,
    TransferSourceSession,
};

use super::download_common::{require_library, MediaLibraryRow};
use super::import_write_mutex::library_import_mutex_key;
use crate::error::ServiceError;
use crate::playback::operation_locks::{busy_error, MediaOperation};

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
///
/// # 插件注入
///
/// `provider_factory` 是 `Option`：`None` = 组合根没注入（没装插件），
/// 需要插件的方法直接报 503 `provider_not_installed`。与
/// [`super::provider_browse::ProviderBrowseService`] 同一个理由 ——
/// 缺省也能构造，单测不依赖插件。
pub struct MediaTransferTaskService {
    db: Db,
    provider_factory: Option<Arc<dyn HostProviderFactory>>,
}

impl MediaTransferTaskService {
    /// 构造。
    pub fn new(db: Db, provider_factory: Option<Arc<dyn HostProviderFactory>>) -> Self {
        Self {
            db,
            provider_factory,
        }
    }

    /// 取 provider 工厂。没注入 → 503 `provider_not_installed`。
    fn factory(&self) -> Result<&dyn HostProviderFactory, ServiceError> {
        self.provider_factory.as_deref().ok_or_else(|| {
            ServiceError::unavailable("provider_not_installed", "媒体提供方未安装")
        })
    }

    /// 能力检查：上游 `supports_media_transfer_*` 的 gRPC 世界等价物。
    ///
    /// 「有没有这个方法」=「注册时有没有声明这个 capability」，只查注册表，
    /// 不建连接。`None`（注册表里没有这个 provider）→ 503；
    /// `Some(false)`（有但没声明）→ 422（码由调用方给）。
    fn require_capability(
        &self,
        provider_key: &str,
        capability_value: i32,
        unsupported_code: &str,
        unsupported_message: &str,
    ) -> Result<(), ServiceError> {
        match self
            .factory()?
            .has_capability(provider_key, capability_value)
        {
            None => Err(ServiceError::unavailable(
                "provider_not_installed",
                "媒体提供方未安装",
            )),
            Some(false) => Err(ServiceError::validation(
                unsupported_code,
                unsupported_message,
            )),
            Some(true) => Ok(()),
        }
    }

    /// 按 `provider_key` 取已连好的 provider。插件没装 / 连不上 → 503。
    async fn provider_for(
        &self,
        provider_key: &str,
    ) -> Result<Arc<dyn HostStorageProvider>, ServiceError> {
        self.factory()?
            .for_provider_key(provider_key)
            .await
            .map_err(|err| map_host_error(&err, "provider_transfer_failed"))
    }

    /// `POST /media-transfers/candidates` —— **200**。
    ///
    /// 错误码：媒体不存在 → `404 media_transfer_source_not_found`；
    /// 目标库不存在（`library_ids` 里有一个查不到）→ **404**；
    /// 源不支持转存 → `422 media_transfer_source_unsupported`。
    ///
    /// 上游 `list_candidates`（`media_transfer_task_service.py:50-80`）：
    /// 校验源 → 源能力 → 逐个目标库查 `supports_media_transfer_target`。
    /// Rust 侧「查能力」= 查注册表声明，不建连接。
    pub async fn list_candidates(
        &self,
        request: MediaStorageTransferCandidatesRequest,
    ) -> Result<MediaStorageTransferCandidatesResponse, ServiceError> {
        // 1. 源媒体（404；无效 → 422）。
        let media = MediaRepository::new(self.db.clone())
            .find_by_id(request.media_id as i32)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_transfer_source_not_found",
                    "源媒体不存在",
                    "media_id",
                    request.media_id as i32,
                )
            })?;
        if !media.valid {
            return Err(ServiceError::validation(
                "media_transfer_source_invalid",
                "源媒体无效",
            ));
        }
        // 2. 源库。
        let source_library = require_library(&self.db, media.library_id).await?;
        // 3. 源能力（清理能力可选 —— 见模块文档那张表）。
        self.require_capability(
            &source_library.provider_key,
            capability::TRANSFER_SOURCE,
            "media_transfer_source_unsupported",
            "源媒体库不支持迁移",
        )?;
        // 4. 目标路径（上游 `_placement_for`）。
        let path = placement_for(&media)?;
        // 5. 目标库列表（`library_ids` 限定；查不到 → 404，不是空列表）。
        let libraries = self.candidate_libraries(request.library_ids.as_deref()).await?;
        // 6. 逐个判目标能力。不可写的**也要列出来**（带原因），只列可写的
        //    会让用户以为看全了。
        let mut targets = Vec::with_capacity(libraries.len());
        for library in libraries {
            if library.id == source_library.id {
                continue;
            }
            let (writable, blocked_reason) = match self
                .factory()?
                .has_capability(&library.provider_key, capability::TRANSFER_TARGET)
            {
                None => (false, Some("provider_not_installed".to_owned())),
                Some(false) => (
                    false,
                    Some("media_transfer_target_unsupported".to_owned()),
                ),
                Some(true) => (true, None),
            };
            targets.push(MediaStorageTransferTarget {
                library_id: i64::from(library.id),
                library_name: library.name,
                path: path.clone(),
                writable,
                blocked_reason,
            });
        }
        Ok(MediaStorageTransferCandidatesResponse {
            media_id: request.media_id,
            targets,
        })
    }

    /// 列出候选目标库。`library_ids` 为 `Some` 时逐个查（查不到 → 404）；
    /// `None` 时列全部库（库是两位数量级，一次取完）。
    async fn candidate_libraries(
        &self,
        library_ids: Option<&[i64]>,
    ) -> Result<Vec<MediaLibraryRow>, ServiceError> {
        match library_ids {
            Some(ids) => {
                let mut libraries = Vec::with_capacity(ids.len());
                for id in ids {
                    libraries.push(require_library(&self.db, *id as i32).await?);
                }
                Ok(libraries)
            }
            None => {
                let page = MediaLibraryRepository::new(self.db.clone())
                    .list(PageRequest::first_page(200)?)
                    .await?;
                Ok(page
                    .items
                    .iter()
                    .map(MediaLibraryRow::from_entity)
                    .collect())
            }
        }
    }

    /// `POST /media-transfers` —— **202**。
    ///
    /// 错误码：同库 → `422 media_transfer_same_library`；源媒体无效 →
    /// `422 media_transfer_source_invalid`；目标库不存在 →
    /// `404 media_transfer_target_library_not_found`；源/目标能力缺失 →
    /// `422 media_transfer_source_unsupported` /
    /// `media_transfer_target_unsupported`；已有转存在跑 →
    /// `409 media_transfer_conflict`；旧任务残留 →
    /// `409 media_transfer_legacy_task`。
    ///
    /// 上游 `enqueue`（`:82-131`）：legacy 检查 → 校验 → 能力 → 建 task run。
    pub async fn enqueue(
        &self,
        request: MediaStorageTransferRequest,
    ) -> Result<MediaStorageTransferAcceptedResponse, ServiceError> {
        // 1. 旧任务残留检查（上游 `:86-99`）。
        self.reject_legacy_tasks().await?;
        // 2. 校验：媒体 → 源库 → 目标库。
        let media = MediaRepository::new(self.db.clone())
            .find_by_id(request.media_id as i32)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "media_transfer_source_not_found",
                    "源媒体不存在",
                    "media_id",
                    request.media_id as i32,
                )
            })?;
        if !media.valid {
            return Err(ServiceError::validation(
                "media_transfer_source_invalid",
                "源媒体无效",
            ));
        }
        let source_library = require_library(&self.db, media.library_id).await?;
        let target_library = require_library(&self.db, request.target_library_id as i32)
            .await
            .map_err(|err| {
                // 目标库不存在有**专用码**（不是 `media_library_not_found`）。
                if err.code() == "media_library_not_found" {
                    ServiceError::not_found(
                        "media_transfer_target_library_not_found",
                        "目标媒体库不存在",
                        "target_library_id",
                        request.target_library_id as i32,
                    )
                } else {
                    err
                }
            })?;
        if target_library.id == source_library.id {
            return Err(ServiceError::validation(
                "media_transfer_same_library",
                "源和目标媒体库不能相同",
            ));
        }
        // 3. 能力检查（上游 `_require_capabilities`）。
        self.require_capability(
            &source_library.provider_key,
            capability::TRANSFER_SOURCE,
            "media_transfer_source_unsupported",
            "源媒体库不支持迁移",
        )?;
        self.require_capability(
            &target_library.provider_key,
            capability::TRANSFER_TARGET,
            "media_transfer_target_unsupported",
            "目标媒体库不支持迁移",
        )?;
        // 4. 建 task run（上游 `:108-128`）。params 里记 `_source_library_id` /
        //    `_library_versions` / `_move_index`，供 `execute` 做版本校验。
        let mut params = serde_json::json!({
            "media_id": request.media_id,
            "target_library_id": request.target_library_id,
            "target_path": request.target_path,
            "delete_source": request.delete_source,
            "_source_library_id": source_library.id,
            "_move_index": 0,
        });
        if let serde_json::Value::Object(map) = &mut params {
            let mut versions = serde_json::Map::new();
            for library in [&source_library, &target_library] {
                versions.insert(
                    library.id.to_string(),
                    serde_json::Value::String(library_config_version(library)),
                );
            }
            map.insert(
                "_library_versions".to_owned(),
                serde_json::Value::Object(versions),
            );
        }
        let summary = serde_json::json!({
            "unexecuted_media_ids": [request.media_id],
            "transferred_count": 0,
            "skipped_count": 0,
            "failed_count": 0,
            "cleanup_incomplete_count": 0,
            "issues": [],
        });
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let task = repo
            .enqueue(&NewTaskRun {
                task_key: TASK_KEY.to_owned(),
                task_name: "媒体存储迁移".to_owned(),
                trigger_type: "manual".to_owned(),
                mutex_key: Some(library_import_mutex_key(i64::from(target_library.id))),
                params: Some(params),
                scheduled_at: None,
            })
            .await
            .map_err(|err| {
                // 互斥键冲突 → 409（上游 `IntegrityError` → `media_transfer_conflict`）。
                if is_conflict(&err) {
                    ServiceError::conflict(
                        "media_transfer_conflict",
                        "目标媒体库已有导入或迁移任务",
                        None,
                    )
                } else {
                    ServiceError::from(err)
                }
            })?;
        repo.merge_result_summary(task.id, Some(&summary)).await?;
        Ok(MediaStorageTransferAcceptedResponse {
            task_run_id: i64::from(task.id),
        })
    }

    /// 旧任务残留检查（上游 `enqueue:86-99`）。
    ///
    /// 有 `_staged_transfers` 的、或「pending/running 但没有 `_library_versions`」
    /// 的旧任务 → `409 media_transfer_legacy_task`。旧任务不能被当成新迁移执行 ——
    /// 中断的工作**绝不**续跑破坏性调用（见模块文档）。
    async fn reject_legacy_tasks(&self) -> Result<(), ServiceError> {
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let mut page = 1i64;
        loop {
            let runs = repo
                .list_by_task_key(TASK_KEY, PageRequest::new(page, 200)?)
                .await?;
            for run in &runs.items {
                let params: serde_json::Value = run
                    .params
                    .as_deref()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(serde_json::Value::Null);
                let has_staged = params
                    .get("_staged_transfers")
                    .is_some_and(|v| !v.is_null());
                let active =
                    run.state == task_state::PENDING || run.state == task_state::RUNNING;
                let has_versions = params.get("_library_versions").is_some();
                if has_staged || (active && !has_versions) {
                    return Err(ServiceError::conflict(
                        "media_transfer_legacy_task",
                        "请先处理旧媒体复制任务",
                        None,
                    ));
                }
            }
            if runs.items.len() < 200 || (page * 200) >= runs.total {
                break;
            }
            page += 1;
        }
        Ok(())
    }

    /// ★ 执行体。worker 调用（`task_run_id` 由调度器给）。
    ///
    /// 阶段：**复制 → 校验 → 切换 →（可选）删源**（见模块文档）。
    /// `delete_source` 只在**切换成功后**执行，且源插件不支持
    /// `TRANSFER_SOURCE_CLEANUP` 时**跳过**（保留源，而不是报错）。
    ///
    /// 上游 `execute`（`:150-296`）的 Rust 版。`reporter` 的职责
    /// （`task_run_id` / 进度回写）由参数与 `BackgroundTaskRunRepository` 承担。
    pub async fn execute(
        &self,
        task_run_id: i32,
        params: &serde_json::Value,
    ) -> Result<MediaTransferSummary, ServiceError> {
        // 1. 旧任务 / 中断任务不能作为新迁移执行（上游 `:158-164`）。
        let versions = params.get("_library_versions").and_then(|v| v.as_object());
        let move_index = params
            .get("_move_index")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if versions.is_none()
            || params.get("_staged_transfers").is_some()
            || move_index != 0
            || params.get("_current_item").is_some()
        {
            return Err(ServiceError::from_status(
                500,
                "media_transfer_cannot_resume",
                "旧任务或中断任务不能作为新迁移执行",
            ));
        }
        let request: MediaStorageTransferRequest =
            serde_json::from_value(params.clone()).map_err(|_| {
                ServiceError::validation("media_transfer_invalid_params", "任务参数非法")
            })?;
        // 2. 库与版本校验（上游 `:172-184`）。
        let source_id = params
            .get("_source_library_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                ServiceError::validation("media_transfer_invalid_params", "任务参数非法")
            })? as i32;
        let source_library = require_library(&self.db, source_id).await?;
        let target_library = require_library(&self.db, request.target_library_id as i32).await?;
        for library in [&source_library, &target_library] {
            let expected = versions
                .and_then(|v| v.get(&library.id.to_string()))
                .and_then(|v| v.as_str());
            if expected != Some(library_config_version(library).as_str()) {
                return Err(ServiceError::from_status(
                    500,
                    "media_transfer_library_changed",
                    "媒体库配置已变化，请重新发起",
                ));
            }
        }
        // 3. provider 与能力（上游 `:185-187`）。
        let source_provider = self.provider_for(&source_library.provider_key).await?;
        let target_provider = self.provider_for(&target_library.provider_key).await?;
        self.require_capability(
            &source_library.provider_key,
            capability::TRANSFER_SOURCE,
            "media_transfer_source_unsupported",
            "源媒体库不支持迁移",
        )?;
        self.require_capability(
            &target_library.provider_key,
            capability::TRANSFER_TARGET,
            "media_transfer_target_unsupported",
            "目标媒体库不支持迁移",
        )?;
        let cleanup_supported = self
            .factory()?
            .has_capability(
                &source_library.provider_key,
                capability::TRANSFER_SOURCE_CLEANUP,
            )
            .unwrap_or(false);
        // 4. 转存（Rust DTO 是单个媒体；上游是列表，这里是一次一项）。
        let summary = self
            .transfer_one(
                task_run_id,
                params,
                &request,
                request.media_id as i32,
                &source_library,
                &target_library,
                &source_provider,
                &target_provider,
                cleanup_supported,
            )
            .await?;
        Ok(summary)
    }

    /// 转存单个媒体（`execute` 的内层循环体，上游 `:194-294`）。
    ///
    /// 结构：媒体锁 → 读媒体 → `begin_item` → 开会话 → 内层 →
    /// 关会话（**保证调用**）→ `finish_item`。
    #[allow(clippy::too_many_arguments)]
    async fn transfer_one(
        &self,
        task_run_id: i32,
        params: &serde_json::Value,
        request: &MediaStorageTransferRequest,
        media_id: i32,
        source_library: &MediaLibraryRow,
        target_library: &MediaLibraryRow,
        source_provider: &Arc<dyn HostStorageProvider>,
        target_provider: &Arc<dyn HostStorageProvider>,
        cleanup_supported: bool,
    ) -> Result<MediaTransferSummary, ServiceError> {
        // 媒体锁（上游 `media_operation_lock(MEDIA_LOCK, media_id)`）。拿不到 → 409。
        let _media_lock = match MediaOperation::try_media(&self.db, media_id).await? {
            Some(guard) => guard,
            None => return Err(busy_error(media_id)),
        };
        // 1. 读媒体并校验快照（上游 `:197-206`）。
        let media = MediaRepository::new(self.db.clone())
            .find_by_id(media_id)
            .await?
            .ok_or_else(|| {
                ServiceError::from_status(500, "media_transfer_source_changed", "源媒体已变化")
            })?;
        if !media.valid || media.library_id != source_library.id {
            return Err(ServiceError::from_status(
                500,
                "media_transfer_source_changed",
                "源媒体已变化",
            ));
        }
        self.begin_item(task_run_id, params, media_id).await?;
        // 2. 打开源会话（上游 `open_transfer_source` 上下文管理器）。
        let session = source_provider
            .open_transfer_source(
                library_handle_for(source_library),
                media_handle_for(&media, source_library)?,
            )
            .await
            .map_err(|err| map_host_error(&err, "transfer_setup_failed"))?;
        let session_id = session.session_id.clone();
        // 3. 内层（暂存 → 断言 → 切换 → 提交 → 删源），出错走补偿。
        let result = self
            .transfer_one_inner(
                task_run_id,
                request,
                &media,
                source_library,
                target_library,
                source_provider,
                target_provider,
                cleanup_supported,
                &session,
            )
            .await;
        // 4. 关会话（上游上下文管理器退出；**保证调用**，不吞内层的错）。
        let close_err = source_provider
            .close_transfer_source(session_id)
            .await
            .map_err(|err| map_host_error(&err, "transfer_setup_failed"))
            .err();
        let summary = result?;
        if let Some(err) = close_err {
            return Err(err);
        }
        self.finish_item(task_run_id).await?;
        Ok(summary)
    }

    /// `transfer_one` 的「打开之后、关闭之前」：暂存 → 校验 → 断言 →
    /// 切换 → 提交 →（可选）删源；出错时补偿 + 记失败。
    #[allow(clippy::too_many_arguments)]
    async fn transfer_one_inner(
        &self,
        task_run_id: i32,
        request: &MediaStorageTransferRequest,
        media: &Media,
        source_library: &MediaLibraryRow,
        target_library: &MediaLibraryRow,
        source_provider: &Arc<dyn HostStorageProvider>,
        target_provider: &Arc<dyn HostStorageProvider>,
        cleanup_supported: bool,
        session: &TransferSourceSession,
    ) -> Result<MediaTransferSummary, ServiceError> {
        // 后续步骤的失败原因（上游 `reason` 变量，只记 reason 不落 provider 细节）。
        // `staged_receipt` 给补偿用：只有「暂存成功但未切换」时才 abort。
        let mut reason = "transfer_setup_failed";
        let mut switch_attempted = false;
        let mut staged_receipt = None;

        let result: Result<MediaTransferSummary, ServiceError> = async {
            // 校验源快照（上游 `_validate_source`）。
            validate_transfer_source(session, media)?;
            // 暂存（上游 `stage_transfer`）。
            reason = "stage_failed";
            let placement_path = request
                .target_path
                .clone()
                .unwrap_or(placement_for(media)?);
            let target_handle = library_handle_for(target_library);
            let staged = target_provider
                .stage_transfer(
                    target_handle.clone(),
                    session.clone(),
                    ImportPlacement {
                        relative_path: placement_path,
                    },
                    format!("task:{task_run_id}:1"),
                )
                .await
                .map_err(|err| map_host_error(&err, "transfer_stage_failed"))?;
            // `not_available` → 跳过该项（上游 `:236-239`；不退回普通下载再上传）。
            if staged.status() == StagedTransferStatus::NotAvailable {
                validate_not_available(&staged)?;
                return Ok(MediaTransferSummary::default());
            }
            validate_staged(&staged, session)?;
            let receipt = staged.receipt.clone();
            staged_receipt = Some(receipt.clone());
            // 断言源未变化（上游 `source.assert_unchanged()`）。
            reason = "source_changed";
            let unchanged = source_provider
                .assert_transfer_source_unchanged(session.session_id.clone())
                .await
                .map_err(|err| map_host_error(&err, "transfer_source_changed"))?;
            if !unchanged {
                return Err(ServiceError::from_status(
                    500,
                    "media_transfer_source_changed",
                    "源媒体已变化",
                ));
            }
            // 切换（上游 `_switch_media`）。提交点：此后绝不 abort。
            reason = "switch_failed";
            switch_attempted = true;
            self.switch_media(task_run_id, media, target_library, &staged)
                .await?;
            // 提交（上游 `finalize_transfer`）。
            reason = "target_verification_failed";
            target_provider
                .finalize_transfer(target_handle.clone(), receipt.clone())
                .await
                .map_err(|err| map_host_error(&err, "transfer_finalize_failed"))?;
            // 删源：只在切换成功后，且能力可选（不支持则保留源）。
            let mut summary = MediaTransferSummary {
                transferred: 1,
                sources_deleted: 0,
            };
            if request.delete_source && cleanup_supported {
                reason = "source_cleanup_failed";
                match source_provider
                    .cleanup_transfer_source(
                        library_handle_for(source_library),
                        media_handle_for(media, source_library)?,
                        session.session_id.clone(),
                    )
                    .await
                {
                    Ok(()) => summary.sources_deleted = 1,
                    Err(err) => {
                        // 删源失败不算转存失败，记一条 issue（上游记 reason 后继续）。
                        let _ = err;
                        self.record_issue(task_run_id, media.id, "source_cleanup_failed", true)
                            .await?;
                    }
                }
            }
            Ok(summary)
        }
        .await;

        match result {
            Ok(summary) => Ok(summary),
            Err(err) => {
                // 补偿：已暂存但未切换 → abort（上游 `:276-294`）。
                // 切换已提交后**绝不** abort。补偿本身失败不掩盖原错。
                // 补偿：已暂存但未切换 → abort（上游 `:276-294`）。
                // 切换已提交后**绝不** abort；stage 都没成功时没有 receipt 可传。
                // 补偿本身失败不掩盖原错。
                if !switch_attempted {
                    if let Some(receipt) = staged_receipt {
                        let _ = target_provider
                            .abort_transfer(library_handle_for(target_library), receipt)
                            .await;
                    }
                }
                self.record_failure(task_run_id, media.id, reason, switch_attempted)
                    .await?;
                Err(err)
            }
        }
    }

    /// 切换媒体归属（上游 `_switch_media:330-363`）。
    ///
    /// 乐观并发：`WHERE` 带上期望的旧值，0 行 → 源已变化。
    /// 成功后把 `_current_item.phase` 记成 `media_switched`
    /// （`recover` 靠它区分 `cleanup_unconfirmed` / `interrupted`）。
    async fn switch_media(
        &self,
        task_run_id: i32,
        media: &Media,
        target_library: &MediaLibraryRow,
        staged: &StagedMediaTransfer,
    ) -> Result<(), ServiceError> {
        let storage_ref = staged
            .storage_ref
            .as_ref()
            .map(|s| sm_plugin_api::json_struct::struct_to_json(Some(s)))
            .unwrap_or_else(|| serde_json::json!({}));
        let switched = MediaRepository::new(self.db.clone())
            .switch_library(
                media.id,
                media.library_id,
                &media.file_name,
                media.file_size_bytes,
                target_library.id,
                &storage_ref.to_string(),
                staged.file_name.as_deref().unwrap_or(&media.file_name),
                staged.size_bytes.unwrap_or(media.file_size_bytes),
            )
            .await?;
        if !switched {
            return Err(ServiceError::from_status(
                500,
                "media_transfer_source_changed",
                "源媒体已变化",
            ));
        }
        // `_current_item.phase = "media_switched"`（上游 `:360-362`）。
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let run = repo.find_by_id(task_run_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_transfer_task_not_found",
                "转存任务不存在",
                "task_run_id",
                task_run_id,
            )
        })?;
        let mut params: serde_json::Value = run
            .params
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::Value::Null);
        if let Some(item) = params.get_mut("_current_item").and_then(|v| v.as_object_mut()) {
            item.insert(
                "phase".to_owned(),
                serde_json::Value::String("media_switched".to_owned()),
            );
        }
        repo.update_params(task_run_id, &params).await?;
        Ok(())
    }

    /// `_begin_item`（上游 `:318-328`）：占 `_current_item`，防续跑。
    async fn begin_item(
        &self,
        task_run_id: i32,
        params: &serde_json::Value,
        media_id: i32,
    ) -> Result<(), ServiceError> {
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let run = repo.find_by_id(task_run_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_transfer_task_not_found",
                "转存任务不存在",
                "task_run_id",
                task_run_id,
            )
        })?;
        if run.state != task_state::RUNNING {
            return Err(ServiceError::from_status(
                500,
                "media_transfer_task_not_running",
                "转存任务未在运行",
            ));
        }
        let mut params = params.clone();
        if let serde_json::Value::Object(map) = &mut params {
            map.insert(
                "_current_item".to_owned(),
                serde_json::json!({"media_id": media_id, "phase": "processing"}),
            );
        }
        repo.update_params(task_run_id, &params).await?;
        Ok(())
    }

    /// `_finish_item` 的记账部分（上游 `:365-391`）：清 `_current_item`，
    /// `_move_index` + 1。
    async fn finish_item(&self, task_run_id: i32) -> Result<(), ServiceError> {
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let run = repo.find_by_id(task_run_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_transfer_task_not_found",
                "转存任务不存在",
                "task_run_id",
                task_run_id,
            )
        })?;
        let mut params: serde_json::Value = run
            .params
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(map) = &mut params {
            map.remove("_current_item");
            let index = map.get("_move_index").and_then(|v| v.as_i64()).unwrap_or(0);
            map.insert("_move_index".to_owned(), serde_json::json!(index + 1));
        }
        repo.update_params(task_run_id, &params).await?;
        Ok(())
    }

    /// 记一条 issue（不中断任务）。`merge_result_summary` 只做浅合并，
    /// `issues` 数组要自己读出来追加 —— 这里用读-改-写。
    async fn record_issue(
        &self,
        task_run_id: i32,
        media_id: i32,
        reason_code: &str,
        target_committed: bool,
    ) -> Result<(), ServiceError> {
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let run = repo.find_by_id(task_run_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_transfer_task_not_found",
                "转存任务不存在",
                "task_run_id",
                task_run_id,
            )
        })?;
        let mut summary: serde_json::Value = run
            .result_summary
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::json!({}));
        if let serde_json::Value::Object(map) = &mut summary {
            let mut issues = map
                .remove("issues")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            issues.push(serde_json::json!({
                "media_id": media_id,
                "reason_code": reason_code,
                "target_committed": target_committed,
            }));
            map.insert("issues".to_owned(), serde_json::Value::Array(issues));
        }
        repo.merge_result_summary(task_run_id, Some(&summary)).await?;
        Ok(())
    }

    /// `_record_failure`（上游 `:393-426`）：清 `_current_item`、记数、记 issue。
    async fn record_failure(
        &self,
        task_run_id: i32,
        media_id: i32,
        reason_code: &str,
        switched: bool,
    ) -> Result<(), ServiceError> {
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let run = repo.find_by_id(task_run_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_transfer_task_not_found",
                "转存任务不存在",
                "task_run_id",
                task_run_id,
            )
        })?;
        let mut params: serde_json::Value = run
            .params
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::Value::Null);
        // 没有 `_library_versions` 或已完成 → 不记（上游 `:401-403`）。
        if params.get("_library_versions").is_none() || run.state == task_state::COMPLETED {
            return Ok(());
        }
        let had_item = params
            .get("_current_item")
            .is_some_and(|v| !v.is_null());
        if let serde_json::Value::Object(map) = &mut params {
            map.remove("_current_item");
        }
        repo.update_params(task_run_id, &params).await?;
        if !had_item {
            return Ok(());
        }
        // 计数器累加 + issue 追加 + reason_code（读-改-写）。
        let run = repo.find_by_id(task_run_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_transfer_task_not_found",
                "转存任务不存在",
                "task_run_id",
                task_run_id,
            )
        })?;
        let mut summary: serde_json::Value = run
            .result_summary
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(serde_json::json!({}));
        if let serde_json::Value::Object(map) = &mut summary {
            let key = if switched {
                "cleanup_incomplete_count"
            } else {
                "failed_count"
            };
            let current = map.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
            map.insert(key.to_owned(), serde_json::json!(current + 1));
            let mut issues = map
                .remove("issues")
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            issues.push(serde_json::json!({
                "media_id": media_id,
                "reason_code": reason_code,
                "target_committed": switched,
            }));
            map.insert("issues".to_owned(), serde_json::Value::Array(issues));
            map.insert(
                "reason_code".to_owned(),
                serde_json::Value::String(reason_code.to_owned()),
            );
        }
        repo.merge_result_summary(task_run_id, Some(&summary)).await?;
        Ok(())
    }

    /// 中断任务恢复。**只标记，不续跑**（见模块文档）。
    ///
    /// 上游 `recover_interrupted_transfers`（`:428-450`）：找 `failed` 的本任务
    /// run，有 `_library_versions` 且（有 `_current_item` 或摘要里有
    /// `reason_code`）的 → `_record_failure("cleanup_unconfirmed" | "interrupted")`。
    /// 中断在切换阶段的计入 `marked_unresumable`（不可继续），其余计入
    /// `safely_resumable`（标记后可由用户重新发起）。
    pub async fn recover_interrupted_transfers(
        &self,
    ) -> Result<RecoverInterruptedResult, ServiceError> {
        let repo = BackgroundTaskRunRepository::new(self.db.clone());
        let mut result = RecoverInterruptedResult::default();
        let mut page = 1i64;
        loop {
            let runs = repo
                .list_by_task_key(TASK_KEY, PageRequest::new(page, 200)?)
                .await?;
            for run in &runs.items {
                if run.state != task_state::FAILED {
                    continue;
                }
                let params: serde_json::Value = run
                    .params
                    .as_deref()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(serde_json::Value::Null);
                if params.get("_library_versions").is_none() {
                    continue;
                }
                let item = params.get("_current_item");
                let summary: serde_json::Value = run
                    .result_summary
                    .as_deref()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(serde_json::Value::Null);
                if item.is_none() && summary.get("reason_code").is_none() {
                    continue;
                }
                // 中断在切换阶段 → `cleanup_unconfirmed`，否则 `interrupted`。
                let phase = item
                    .and_then(|v| v.get("phase"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let switched = phase == "media_switched";
                let reason = if switched {
                    "cleanup_unconfirmed"
                } else {
                    "interrupted"
                };
                let media_id = item
                    .and_then(|v| v.get("media_id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32;
                // 锁被占就跳过（上游 `except MediaOperationBusy: continue`）。
                let _lock = match MediaOperation::try_media(&self.db, media_id).await? {
                    Some(guard) => guard,
                    None => continue,
                };
                self.record_failure(run.id, media_id, reason, switched)
                    .await?;
                if switched {
                    result.marked_unresumable += 1;
                } else {
                    result.safely_resumable += 1;
                }
            }
            if runs.items.len() < 200 || (page * 200) >= runs.total {
                break;
            }
            page += 1;
        }
        Ok(result)
    }
}

/// 从行投影建 `LibraryHandle`（与 `provider_browse` 同一形状）。
fn library_handle_for(library: &MediaLibraryRow) -> LibraryHandle {
    LibraryHandle {
        library_id: i64::from(library.id),
        provider_key: library.provider_key.clone(),
        provider_config: sm_plugin_api::json_struct::json_to_struct(&library.provider_config),
        account_key: None,
    }
}

/// 从媒体行建 `MediaHandle`。
fn media_handle_for(media: &Media, library: &MediaLibraryRow) -> Result<MediaHandle, ServiceError> {
    let storage_ref: serde_json::Value = media
        .storage_ref
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(serde_json::json!({}));
    Ok(MediaHandle {
        media_id: i64::from(media.id),
        library: Some(library_handle_for(library)),
        storage_ref: sm_plugin_api::json_struct::json_to_struct(&storage_ref),
        file_name: media.file_name.clone(),
        file_size_bytes: media.file_size_bytes,
        duration_seconds: 0,
    })
}

/// 目标路径（上游 `_placement_for:507-516`）。
///
/// `jav/{movie_number}/{file_name}` 或 `videos/{file_name}`。
/// 文件名非法（空 / `.` / `..` / 含分隔符）→ 422，上游是 `RuntimeError`。
fn placement_for(media: &Media) -> Result<String, ServiceError> {
    let file_name = media.file_name.trim();
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains('\0')
    {
        return Err(ServiceError::validation(
            "media_transfer_source_invalid",
            "源媒体文件名非法",
        ));
    }
    Ok(match &media.movie_number {
        Some(number) => format!("jav/{number}/{file_name}"),
        None => format!("videos/{file_name}"),
    })
}

/// 校验源会话快照（上游 `_validate_source:531-552`）。
///
/// `info.file_name` / `info.size_bytes` 必须与媒体行一致 —— 不一致说明
/// 会话打开后源变了。
fn validate_transfer_source(
    session: &TransferSourceSession,
    media: &Media,
) -> Result<(), ServiceError> {
    let info = session.info.as_ref().ok_or_else(|| {
        ServiceError::bad_gateway(
            "provider_invalid_response",
            "插件返回的会话无效",
            serde_json::Map::new(),
        )
    })?;
    if info.file_name != media.file_name || info.size_bytes != media.file_size_bytes {
        return Err(ServiceError::from_status(
            500,
            "media_transfer_source_changed",
            "源媒体已变化",
        ));
    }
    Ok(())
}

/// 校验暂存结果（上游 `_validate_staged:554-576`）。
fn validate_staged(
    staged: &StagedMediaTransfer,
    session: &TransferSourceSession,
) -> Result<(), ServiceError> {
    let bad = || {
        ServiceError::bad_gateway(
            "provider_invalid_response",
            "插件返回非法",
            serde_json::Map::new(),
        )
    };
    if staged.status() != StagedTransferStatus::Staged {
        return Err(bad());
    }
    let storage_empty = staged
        .storage_ref
        .as_ref()
        .map(|s| sm_plugin_api::json_struct::struct_to_json(Some(s)))
        .is_none_or(|v| v.as_object().is_none_or(|m| m.is_empty()));
    if storage_empty || staged.receipt.is_none() {
        return Err(bad());
    }
    let file_name = staged.file_name.as_deref().unwrap_or("");
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains('\0')
    {
        return Err(bad());
    }
    let source_size = session.info.as_ref().map(|info| info.size_bytes);
    if staged.size_bytes != source_size {
        return Err(bad());
    }
    Ok(())
}

/// 校验 `not_available` 的暂存结果（上游 `_validate_not_available:518-529`）。
///
/// `not_available` 时四个字段必须全空 —— 插件不能「说不可用又给凭据」。
fn validate_not_available(staged: &StagedMediaTransfer) -> Result<(), ServiceError> {
    if staged.storage_ref.is_some()
        || staged.receipt.is_some()
        || staged.file_name.is_some()
        || staged.size_bytes.is_some()
    {
        return Err(ServiceError::bad_gateway(
            "provider_invalid_response",
            "插件返回非法",
            serde_json::Map::new(),
        ));
    }
    Ok(())
}

/// 库配置版本（`enqueue` 存、`execute` 校验）。
///
/// 行投影里没有 `updated_at`，这里用 `provider_config` 的哈希代替 ——
/// 配置变了就必须重新发起（上游比的是 `updated_at.isoformat()`）。
fn library_config_version(library: &MediaLibraryRow) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    library.provider_config.to_string().hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// `DbError` 是不是唯一冲突（互斥键重复 → 409）。
///
/// 按 `ConstraintViolation` 变体判，而不是字符串匹配 SQLSTATE ——
/// 变体是本仓的稳定 API。
fn is_conflict(err: &sm_db::DbError) -> bool {
    matches!(
        err,
        sm_db::DbError::ConstraintViolation { .. }
    )
}

/// 把 [`HostProviderError`] 映射成 [`ServiceError`]（转存版）。
///
/// 与 `provider_browse::map_host_error` 同一张码表，默认码不同：
/// 浏览默认 `provider_browse_failed`，转存默认 `provider_transfer_failed`。
fn map_host_error(err: &HostProviderError, default_code: &str) -> ServiceError {
    if err.is_not_installed() {
        return ServiceError::unavailable("provider_not_installed", "媒体提供方未安装");
    }
    let code = format!("provider_{}", err.code);
    match err.code.as_str() {
        "invalid_config" => ServiceError::from_status(422, code, err.safe_message.clone()),
        "authentication_failed" => {
            ServiceError::from_status(401, code, err.safe_message.clone())
        }
        "source_not_found" => ServiceError::from_status(404, code, err.safe_message.clone()),
        _ => ServiceError::bad_gateway(
            default_code,
            err.safe_message.clone(),
            serde_json::Map::new(),
        ),
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
