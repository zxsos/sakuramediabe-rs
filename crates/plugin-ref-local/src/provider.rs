//! `StorageProvider` 的本地目录参考实现。
//!
//! 只落地最小集的 4 个 rpc，其余 28 个返回 `Status::unimplemented`。
//! 实现过程中遇到的 proto 缺口都就地标注了 `GAP:` 注释，并汇总到
//! `docs/parallel/grpc-plugin-report.md`。

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use prost_types::Struct;
use sm_plugin_api::v1::storage_provider_server::StorageProvider;
use sm_plugin_api::v1::{
    playback_plan, BrowseEntry, BrowsePage, BrowseRequest, EntryType, GenerateThumbnailsRequest,
    ImportFile, ImportFileEntry, LibraryHandle, MediaHandle, PlanPlaybackRequest,
    PlanPlaybackResponse, PlaybackDelivery, PlaybackPlan, ProgressEvent, RedirectPlan,
    ScanImportSourceRequest, StagedMedia, TransferReadRequest, TransferReadResponse,
};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Empty};
use tonic::{Request, Response, Status, Streaming};

use crate::opaque::{ref_path, string_ref};

/// `BrowseRequest.limit` 未填 / 非正数时的兜底页大小。
///
/// GAP: proto 没有规定默认值与上限，`limit` 还是 `int32`（可为负）。
/// 这些语义只能落到每个插件各自的约定里。
const DEFAULT_PAGE_SIZE: usize = 100;
/// 单页硬上限，避免一次把整个目录塞进一页超过 gRPC 默认 4MB 的消息体。
const MAX_PAGE_SIZE: usize = 1000;

/// 流式通道缓冲槽数。刻意远小于真实流的长度，让背压真的发生：
/// 消费慢时生产者会停在 `tx.send()`，而不是先把整棵树囤在内存里。
const STREAM_BUFFER: usize = 8;

/// 缩略图采样数量下限与上限（替代「按视频时长每 10 秒一张」的实作，
/// 本 crate 不真正解码视频）。
const THUMBNAIL_MIN: i64 = 3;
const THUMBNAIL_MAX: i64 = 24;
/// 采样间隔（秒）。
const THUMBNAIL_INTERVAL_SECONDS: i64 = 10;

/// 视为视频的扩展名。
const VIDEO_EXTENSIONS: [&str; 9] = [
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "ts", "flv", "wmv",
];

/// file:// URL 里需要转义的字符：控制字符之外补上路径里常见的几个。
const FILE_URL_ESCAPE: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'?');

/// 本地参考插件在注册阶段会声明的 provider key。
pub const PROVIDER_KEY: &str = "local-ref";

/// 本地目录 provider。
///
/// 根目录在构造时给定。真实部署里它来自 `LibraryHandle.provider_config`
/// （同样是 `Struct`，宿主无法校验）；这里简化为构造参数，
/// 是为了让请求-响应之外不引入额外的运行时状态。
#[derive(Debug, Clone)]
pub struct LocalRefProvider {
    root: PathBuf,
    provider_key: String,
}

impl LocalRefProvider {
    /// 以 `root` 为根目录构造 provider。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: absolutize(&root.into()),
            provider_key: PROVIDER_KEY.to_owned(),
        }
    }

    /// 覆盖 provider key（默认 [`PROVIDER_KEY`]；测试夹具 `fixture` 也用同一个
    /// 常量，所以两边不会漂移）。
    pub fn with_provider_key(mut self, provider_key: impl Into<String>) -> Self {
        self.provider_key = provider_key.into();
        self
    }

    /// provider 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 未实现方法的统一返回。
    ///
    /// GAP: tonic 0.14 生成的 trait 没有默认方法体（报告 §4.1），
    /// 于是「只实现一个 4 方法切片」也要手写全部 32 个签名。
    fn not_implemented<T>(&self, method: &str) -> Result<Response<T>, Status> {
        Err(Status::unimplemented(format!(
            "plugin-ref-local 未实现 StorageProvider::{method}"
        )))
    }

    /// 校验 library 句柄是不是指向自己。
    fn check_library(&self, library: &LibraryHandle) -> Result<(), Status> {
        if !library.provider_key.is_empty() && library.provider_key != self.provider_key {
            return Err(Status::invalid_argument(format!(
                "provider_key 不匹配：期望 {}，收到 {}",
                self.provider_key, library.provider_key
            )));
        }
        Ok(())
    }

    /// 把请求里的 `path` 解析成 root 之内的绝对路径。
    ///
    /// 路径穿透（`../..`）在此被拦下，用的是词法归一化而非 `canonicalize`：
    /// 后者要求路径已存在，而 Browse 允许列举尚不存在的子路径。
    /// 代价是符号链接跳出 root 的情况拦不住 —— 真实 provider 需要自己在
    /// 打开文件前做一次 `O_NOFOLLOW` 级别的校验。
    fn confined_path(&self, source: Option<&Struct>) -> Result<PathBuf, Status> {
        let relative = ref_path(source).unwrap_or("");
        let candidate = if relative.is_empty() {
            self.root.clone()
        } else {
            absolutize(&self.root.join(relative))
        };
        if candidate != self.root && !candidate.starts_with(&self.root) {
            return Err(Status::invalid_argument(format!(
                "路径 {} 逃出了 provider 根目录 {}",
                candidate.display(),
                self.root.display()
            )));
        }
        Ok(candidate)
    }

    /// 媒体句柄指向的绝对路径。
    fn media_path(&self, media: &MediaHandle) -> Result<PathBuf, Status> {
        if let Some(path) = ref_path(media.storage_ref.as_ref()) {
            return Ok(absolutize(&self.root.join(path)));
        }
        // GAP: `MediaHandle` 没有 `relative_path`，也没有地方声明
        // storage_ref 的 schema。缺少 path 时只能猜「同名文件在根目录」。
        Ok(absolutize(&self.root.join(&media.file_name)))
    }
}

#[async_trait]
impl StorageProvider for LocalRefProvider {
    type ScanImportSourceStream = ReceiverStream<Result<ImportFileEntry, Status>>;
    type GenerateThumbnailsStream = ReceiverStream<Result<ProgressEvent, Status>>;
    type ReadTransferSourceStream = Empty<Result<TransferReadResponse, Status>>;

    // ── ① browse ──────────────────────────────────────────────────
    async fn browse(
        &self,
        request: Request<BrowseRequest>,
    ) -> Result<Response<BrowsePage>, Status> {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("BrowseRequest.library 缺失"))?;
        self.check_library(&library)?;

        let directory = self.confined_path(payload.parent_ref.as_ref())?;
        let all = list_dir(&directory).await?;

        let limit = page_size(payload.limit);
        let start = cursor_offset(&all, payload.cursor.as_deref())?;
        let end = all.len().min(start + limit);

        let parent_rel = ref_path(payload.parent_ref.as_ref()).unwrap_or("");
        let entries = all[start..end]
            .iter()
            .map(|item| browse_entry(parent_rel, item))
            .collect();

        // 游标 = 本页最后一个条目的名字，下一页从它之后继续。
        let next_cursor = all
            .get(end.saturating_sub(1))
            .filter(|_| end < all.len())
            .map(|item| item.name.clone());

        // GAP: BrowsePage 只有 next_cursor，没有总数，宿主无法渲染「共 N 项」。
        Ok(Response::new(BrowsePage {
            entries,
            next_cursor,
        }))
    }

    // ── ② scan_import_source（server streaming）──────────────────
    async fn scan_import_source(
        &self,
        request: Request<ScanImportSourceRequest>,
    ) -> Result<Response<Self::ScanImportSourceStream>, Status> {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("ScanImportSourceRequest.library 缺失"))?;
        self.check_library(&library)?;

        let start = self.confined_path(payload.source_ref.as_ref())?;
        match tokio::fs::metadata(&start).await {
            Ok(_) => {}
            Err(source) => return Err(Status::not_found(format!("导入来源不存在：{source}"))),
        }

        let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
        let base = self.root.clone();
        tokio::spawn(async move {
            walk_and_emit(base, start, sender).await;
        });

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn read_import_file(
        &self,
        _request: Request<sm_plugin_api::v1::ReadImportFileRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ImportFileContent>, Status> {
        self.not_implemented("read_import_file")
    }

    async fn delete_import_file(
        &self,
        _request: Request<sm_plugin_api::v1::DeleteImportFileRequest>,
    ) -> Result<Response<sm_plugin_api::v1::DeleteImportFileResponse>, Status> {
        self.not_implemented("delete_import_file")
    }

    async fn stage_import_file(
        &self,
        _request: Request<sm_plugin_api::v1::StageImportFileRequest>,
    ) -> Result<Response<StagedMedia>, Status> {
        self.not_implemented("stage_import_file")
    }

    async fn finalize_import(
        &self,
        _request: Request<sm_plugin_api::v1::FinalizeImportRequest>,
    ) -> Result<Response<sm_plugin_api::v1::FinalizeImportResponse>, Status> {
        self.not_implemented("finalize_import")
    }

    async fn abort_import(
        &self,
        _request: Request<sm_plugin_api::v1::AbortImportRequest>,
    ) -> Result<Response<sm_plugin_api::v1::AbortImportResponse>, Status> {
        self.not_implemented("abort_import")
    }

    async fn delete_media(
        &self,
        _request: Request<sm_plugin_api::v1::DeleteMediaRequest>,
    ) -> Result<Response<sm_plugin_api::v1::DeleteMediaResponse>, Status> {
        self.not_implemented("delete_media")
    }

    async fn compute_file_hash(
        &self,
        _request: Request<sm_plugin_api::v1::ComputeFileHashRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ComputeFileHashResponse>, Status> {
        self.not_implemented("compute_file_hash")
    }

    // ── ⑩ plan_playback ──────────────────────────────────────────
    async fn plan_playback(
        &self,
        request: Request<PlanPlaybackRequest>,
    ) -> Result<Response<PlanPlaybackResponse>, Status> {
        let payload = request.into_inner();
        let media = payload
            .media
            .ok_or_else(|| Status::invalid_argument("PlanPlaybackRequest.media 缺失"))?;
        if let Some(library) = media.library.as_ref() {
            self.check_library(library)?;
        }

        // GAP: 本地 provider 最自然的答案是「给你一个本地路径，你自己读」，
        // 但 PlaybackPlan 的 delivery 只有 redirect(url) / proxy(endpoint) 两种，
        // 没有 local_path —— 而 OpenCoverSourceResponse 恰恰有 local_path。
        // 这里只能退化成 file:// 的伪 redirect，宿主必须为此开一个特殊分支。
        let delivery =
            PlaybackDelivery::try_from(payload.delivery).unwrap_or(PlaybackDelivery::Unspecified);
        if matches!(delivery, PlaybackDelivery::Proxy) {
            return Err(Status::unimplemented(
                "本地 provider 不支持 proxy 投放：PlaybackPlan 缺少『宿主直接读本地文件』这一 delivery",
            ));
        }
        if !payload.resource_path.is_empty() {
            return Err(Status::invalid_argument("本地 provider 不支持子资源播放"));
        }

        let path = self.media_path(&media)?;
        let metadata = tokio::fs::metadata(&path).await;

        let plan = match metadata {
            Ok(info) if info.is_file() => PlaybackPlan {
                delivery: Some(playback_plan::Delivery::Redirect(RedirectPlan {
                    url: file_url(&path),
                    headers: Default::default(),
                })),
                file_name: media.file_name.clone(),
                size_bytes: Some(clamp_i64(info.len())),
                content_type: content_type_of(&media.file_name),
                unavailable: false,
            },
            // GAP: 失败只有一个布尔位 `unavailable`，区分不了
            // 「文件不存在」「无权限」「已被黑名单」，也带不了
            // ProviderErrorCode / retryable —— common.proto 里定义了
            // ProviderError，但没有任何 rpc 用它来做错误通道。
            _ => PlaybackPlan {
                delivery: None,
                file_name: media.file_name.clone(),
                size_bytes: None,
                content_type: None,
                unavailable: true,
            },
        };

        Ok(Response::new(PlanPlaybackResponse { plan: Some(plan) }))
    }

    // ── ⑪ generate_thumbnails（server streaming）─────────────────
    async fn generate_thumbnails(
        &self,
        request: Request<GenerateThumbnailsRequest>,
    ) -> Result<Response<Self::GenerateThumbnailsStream>, Status> {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("GenerateThumbnailsRequest.library 缺失"))?;
        self.check_library(&library)?;
        let media = payload
            .media
            .ok_or_else(|| Status::invalid_argument("GenerateThumbnailsRequest.media 缺失"))?;

        // 按 proto 注释，workspace 是宿主给的工作目录、路径共享同一挂载，
        // 因此不套用 root 约束。
        let workspace = PathBuf::from(payload.workspace.clone());
        let total = thumbnail_count(media.duration_seconds);
        let stem = file_stem_of(&media.file_name);

        let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            if let Err(source) = tokio::fs::create_dir_all(&workspace).await {
                let _ = sender
                    .send(Err(Status::failed_precondition(format!(
                        "宿主提供的工作目录不可用：{source}"
                    ))))
                    .await;
                return;
            }

            for index in 1..=total {
                // 本 crate 不解码视频，落一个占位文件代表「第 index 张真图」。
                let artifact = format!("{stem}-{index:04}.jpg");
                if let Err(source) =
                    tokio::fs::write(workspace.join(&artifact), b"placeholder").await
                {
                    let _ = sender
                        .send(Err(Status::internal(format!(
                            "写入缩略图产物 {artifact} 失败：{source}"
                        ))))
                        .await;
                    return;
                }

                let event = ProgressEvent {
                    text: format!("已生成第 {index}/{total} 张：{artifact}"),
                    current: index,
                    total,
                };
                if sender.send(Ok(event)).await.is_err() {
                    // 客户端断开：直接收尾，不要往已关闭的流里塞。
                    return;
                }
            }
            // GAP: 流就这样结束了。`ThumbnailGeneration`（含 expected_count 与
            // 产物列表）没有任何返回通道 —— `GenerateThumbnailsResponse`
            // 定义了却没被这个 rpc 用上，宿主拿不到产物文件名。
            // 见报告 §4.3。
        });

        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn create_clip(
        &self,
        _request: Request<sm_plugin_api::v1::CreateClipRequest>,
    ) -> Result<Response<sm_plugin_api::v1::CreateClipResponse>, Status> {
        self.not_implemented("create_clip")
    }

    async fn probe_duration_seconds(
        &self,
        _request: Request<sm_plugin_api::v1::ProbeDurationRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ProbeDurationResponse>, Status> {
        self.not_implemented("probe_duration_seconds")
    }

    async fn probe_resolution(
        &self,
        _request: Request<sm_plugin_api::v1::ProbeResolutionRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ProbeResolutionResponse>, Status> {
        self.not_implemented("probe_resolution")
    }

    async fn probe_video_info(
        &self,
        _request: Request<sm_plugin_api::v1::ProbeVideoInfoRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ProbeVideoInfoResponse>, Status> {
        self.not_implemented("probe_video_info")
    }

    async fn open_cover_source(
        &self,
        _request: Request<sm_plugin_api::v1::OpenCoverSourceRequest>,
    ) -> Result<Response<sm_plugin_api::v1::OpenCoverSourceResponse>, Status> {
        self.not_implemented("open_cover_source")
    }

    async fn get_import_source_identity(
        &self,
        _request: Request<sm_plugin_api::v1::GetImportSourceIdentityRequest>,
    ) -> Result<Response<sm_plugin_api::v1::GetImportSourceIdentityResponse>, Status> {
        self.not_implemented("get_import_source_identity")
    }

    async fn scan_media_refs(
        &self,
        _request: Request<sm_plugin_api::v1::ScanMediaRefsRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ScanMediaRefsResponse>, Status> {
        self.not_implemented("scan_media_refs")
    }

    async fn scan_managed_media_ref_keys(
        &self,
        _request: Request<sm_plugin_api::v1::ScanManagedMediaRefKeysRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ScanManagedMediaRefKeysResponse>, Status> {
        self.not_implemented("scan_managed_media_ref_keys")
    }

    async fn managed_media_ref_key(
        &self,
        _request: Request<sm_plugin_api::v1::ManagedMediaRefKeyRequest>,
    ) -> Result<Response<sm_plugin_api::v1::ManagedMediaRefKeyResponse>, Status> {
        self.not_implemented("managed_media_ref_key")
    }

    async fn get_space_usage(
        &self,
        _request: Request<sm_plugin_api::v1::GetSpaceUsageRequest>,
    ) -> Result<Response<sm_plugin_api::v1::GetSpaceUsageResponse>, Status> {
        self.not_implemented("get_space_usage")
    }

    async fn plan_merged_playback(
        &self,
        _request: Request<sm_plugin_api::v1::PlanMergedPlaybackRequest>,
    ) -> Result<Response<sm_plugin_api::v1::PlanMergedPlaybackResponse>, Status> {
        self.not_implemented("plan_merged_playback")
    }

    async fn preflight_merged_playback(
        &self,
        _request: Request<sm_plugin_api::v1::PreflightMergedPlaybackRequest>,
    ) -> Result<Response<sm_plugin_api::v1::PreflightMergedPlaybackResponse>, Status> {
        self.not_implemented("preflight_merged_playback")
    }

    async fn open_transfer_source(
        &self,
        _request: Request<sm_plugin_api::v1::OpenTransferSourceRequest>,
    ) -> Result<Response<sm_plugin_api::v1::OpenTransferSourceResponse>, Status> {
        self.not_implemented("open_transfer_source")
    }

    async fn read_transfer_source(
        &self,
        _request: Request<Streaming<TransferReadRequest>>,
    ) -> Result<Response<Self::ReadTransferSourceStream>, Status> {
        self.not_implemented("read_transfer_source")
    }

    async fn assert_transfer_source_unchanged(
        &self,
        _request: Request<sm_plugin_api::v1::TransferAssertRequest>,
    ) -> Result<Response<sm_plugin_api::v1::TransferAssertResponse>, Status> {
        self.not_implemented("assert_transfer_source_unchanged")
    }

    async fn close_transfer_source(
        &self,
        _request: Request<sm_plugin_api::v1::CloseTransferSourceRequest>,
    ) -> Result<Response<sm_plugin_api::v1::CloseTransferSourceResponse>, Status> {
        self.not_implemented("close_transfer_source")
    }

    async fn cleanup_transfer_source(
        &self,
        _request: Request<sm_plugin_api::v1::CleanupTransferSourceRequest>,
    ) -> Result<Response<sm_plugin_api::v1::CleanupTransferSourceResponse>, Status> {
        self.not_implemented("cleanup_transfer_source")
    }

    async fn stage_transfer(
        &self,
        _request: Request<sm_plugin_api::v1::StageTransferRequest>,
    ) -> Result<Response<sm_plugin_api::v1::StageTransferResponse>, Status> {
        self.not_implemented("stage_transfer")
    }

    async fn finalize_transfer(
        &self,
        _request: Request<sm_plugin_api::v1::FinalizeTransferRequest>,
    ) -> Result<Response<sm_plugin_api::v1::FinalizeTransferResponse>, Status> {
        self.not_implemented("finalize_transfer")
    }

    async fn abort_transfer(
        &self,
        _request: Request<sm_plugin_api::v1::AbortTransferRequest>,
    ) -> Result<Response<sm_plugin_api::v1::AbortTransferResponse>, Status> {
        self.not_implemented("abort_transfer")
    }

    async fn prepare_library(
        &self,
        _request: Request<sm_plugin_api::v1::PrepareLibraryRequest>,
    ) -> Result<Response<sm_plugin_api::v1::PrepareLibraryResponse>, Status> {
        self.not_implemented("prepare_library")
    }
}

/// 目录里的一条目，已经排好序。
struct DirectoryItem {
    name: String,
    is_directory: bool,
    size_bytes: Option<i64>,
    modified_at: Option<String>,
    is_video: bool,
}

/// 读取并排序一层目录。文件与目录混排，按文件名字典序。
async fn list_dir(directory: &Path) -> Result<Vec<DirectoryItem>, Status> {
    let mut reader = tokio::fs::read_dir(directory).await.map_err(|source| {
        Status::not_found(format!("无法读取目录 {}：{source}", directory.display()))
    })?;

    let mut items = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|source| Status::internal(format!("读取目录项失败：{source}")))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry
            .file_type()
            .await
            .map_err(|source| Status::internal(format!("读取文件类型失败：{source}")))?;

        if file_type.is_symlink() {
            // GAP: EntryType 只有 FILE / DIRECTORY，符号链接没有归属，
            // 这里直接跳过以免把「穿越借口」暴露给上层——更好的做法需要 proto 表态。
            continue;
        }
        if file_type.is_dir() {
            let modified = tokio::fs::metadata(entry.path())
                .await
                .ok()
                .and_then(|info| rfc3339(info.modified().ok()?));
            items.push(DirectoryItem {
                name,
                is_directory: true,
                size_bytes: None,
                modified_at: modified,
                is_video: false,
            });
            continue;
        }
        let info = tokio::fs::metadata(entry.path()).await.map_err(|source| {
            Status::internal(format!(
                "读取 {} 元数据失败：{source}",
                entry.path().display()
            ))
        })?;
        items.push(DirectoryItem {
            is_video: is_video_extension(&name),
            name,
            is_directory: false,
            size_bytes: Some(clamp_i64(info.len())),
            modified_at: info.modified().ok().and_then(rfc3339),
        });
    }

    // GAP: proto 没有给 Browse 提供排序方式 / 排序字段，宿主也无法要求
    // 「目录在前」或「按修改时间倒序」。本地插件只能自定一套。
    items.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(items)
}

/// 目录项 → BrowseEntry。
fn browse_entry(parent_rel: &str, item: &DirectoryItem) -> BrowseEntry {
    BrowseEntry {
        source_ref: Some(string_ref(&join_ref(parent_rel, &item.name))),
        name: item.name.clone(),
        entry_type: if item.is_directory {
            EntryType::Directory
        } else {
            EntryType::File
        } as i32,
        size_bytes: item.size_bytes,
        modified_at: item.modified_at.clone(),
        is_video: item.is_video,
    }
}

/// 把 parent_ref 的路径与本级名字拼成下一级引用的 `path`。
fn join_ref(parent_rel: &str, name: &str) -> String {
    if parent_rel.is_empty() {
        name.to_owned()
    } else {
        format!("{parent_rel}/{name}")
    }
}

/// limit → 实际页大小。
fn page_size(limit: i32) -> usize {
    if limit <= 0 {
        return DEFAULT_PAGE_SIZE;
    }
    (limit as usize).min(MAX_PAGE_SIZE)
}

/// 游标（上一页最后一个条目名）→ 本页起始下标。
fn cursor_offset(items: &[DirectoryItem], cursor: Option<&str>) -> Result<usize, Status> {
    let Some(cursor) = cursor else { return Ok(0) };
    let index = items
        .iter()
        .position(|item| item.name == cursor)
        .ok_or_else(|| Status::invalid_argument(format!("游标 {cursor} 已失效")))?;
    Ok(index + 1)
}

/// 递归遍历并把每个文件作为 [`ImportFileEntry`] 送进通道。
///
/// 顺序是确定的：**同一层按文件名字典序，本层文件先出，子目录后下沉**。
/// 这让「事件顺序」可以被断言 —— 流式 ABI 能否信赖，取决于这件事。
async fn walk_and_emit(
    base: PathBuf,
    start: PathBuf,
    sender: mpsc::Sender<Result<ImportFileEntry, Status>>,
) {
    let mut stack = vec![start];

    while let Some(directory) = stack.pop() {
        let mut reader = match tokio::fs::read_dir(&directory).await {
            Ok(reader) => reader,
            Err(source) => {
                let _ = sender
                    .send(Err(Status::internal(format!(
                        "读取 {} 失败：{source}",
                        directory.display()
                    ))))
                    .await;
                return;
            }
        };

        let mut level: Vec<(String, PathBuf, bool)> = Vec::new();
        while let Some(entry) = match reader.next_entry().await {
            Ok(entry) => entry,
            Err(source) => {
                let _ = sender
                    .send(Err(Status::internal(format!("读取目录项失败：{source}"))))
                    .await;
                return;
            }
        } {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            level.push((
                entry.file_name().to_string_lossy().into_owned(),
                path,
                file_type.is_dir(),
            ));
        }
        level.sort_by(|left, right| left.0.cmp(&right.0));

        let mut subdirectories = Vec::new();
        for (name, path, is_directory) in level {
            if is_directory {
                subdirectories.push(path);
                continue;
            }
            let Ok(metadata) = tokio::fs::metadata(&path).await else {
                continue;
            };
            let relative = path
                .strip_prefix(&base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let entry = ImportFileEntry {
                file: Some(ImportFile {
                    source_ref: Some(string_ref(&relative)),
                    is_video: is_video_extension(&name),
                    name,
                    relative_path: relative,
                    size_bytes: clamp_i64(metadata.len()),
                }),
            };
            if sender.send(Ok(entry)).await.is_err() {
                return;
            }
        }
        // 栈是后进先出：倒着压才能让字典序的第一个子目录先被访问。
        for subdirectory in subdirectories.into_iter().rev() {
            stack.push(subdirectory);
        }
    }
}

/// 采样张数。真实实现按视频时长每 `THUMBNAIL_INTERVAL_SECONDS` 秒一张。
fn thumbnail_count(duration_seconds: i64) -> i32 {
    if duration_seconds <= 0 {
        return THUMBNAIL_MIN as i32;
    }
    let wanted = duration_seconds / THUMBNAIL_INTERVAL_SECONDS;
    let counted = wanted.clamp(THUMBNAIL_MIN, THUMBNAIL_MAX);
    i32::try_from(counted).unwrap_or(i32::MAX)
}

/// `file://` URL。路径里的空格等字符必须转义，否则宿主的 HTTP 客户端会解析出错。
fn file_url(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("file://{}", utf8_percent_encode(&raw, FILE_URL_ESCAPE))
}

/// 按扩展名猜 Content-Type。
fn content_type_of(file_name: &str) -> Option<String> {
    let extension = extension_of(file_name)?;
    let mime = match extension.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "ts" => "video/mp2t",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(mime.to_owned())
}

/// 按扩展名判断是不是视频。
fn is_video_extension(file_name: &str) -> bool {
    extension_of(file_name)
        .map(|extension| VIDEO_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

/// 取小写扩展名。
fn extension_of(file_name: &str) -> Option<String> {
    Some(
        Path::new(file_name)
            .extension()?
            .to_string_lossy()
            .to_ascii_lowercase(),
    )
}

/// 去掉扩展名的文件名主干。
fn file_stem_of(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_owned())
}

/// SystemTime → RFC 3339（`BrowseEntry.modified_at` 要求的格式）。
fn rfc3339(instant: SystemTime) -> Option<String> {
    let moment = DateTime::<Utc>::from(instant).to_rfc3339_opts(SecondsFormat::Secs, true);
    Some(moment)
}

/// u64 → i64，溢出时给 i64::MAX 而不是回绕。
fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// 词法规范化：去掉 `.` 与 `..`，相对路径补上工作目录。
fn absolutize(path: &Path) -> PathBuf {
    let candidate = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf()),
    };
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
