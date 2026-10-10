//! 插件侧的 **provider 默认实现层**。
//!
//! # 为什么需要它（gRPC 报告的 P1-4）
//!
//! `tonic-prost-build` 0.14 生成的 `trait StorageProvider` **没有默认实现** ——
//! 32 个方法一个都不能少。后果是：只想实现 4 个方法的插件，也要先手写 28 个
//! `Status::unimplemented` 签名。参考插件 `plugin-ref-local` 的 `provider.rs`
//! 共 811 行，**其中约 170 行只是为了把 trait 填满**；而且这是**编译期强制**的
//! —— proto 每加一个 rpc，全部插件都编译不过，每个插件都要重缴一遍这笔税。
//!
//! # 怎么用
//!
//! 不用实现生成的 trait，改实现这里的 [`StorageProviderExt`] 与
//! [`DownloadProviderExt`]：它们每个方法都有默认体，返回
//! `Status::unimplemented`，插件**只覆盖自己那几个**。
//!
//! ```ignore
//! use sm_plugin_api::provider::StorageProviderExt;
//! use sm_plugin_api::v1;
//!
//! struct MyProvider;
//!
//! #[tonic::async_trait]
//! impl StorageProviderExt for MyProvider {
//!     async fn browse(
//!         &self,
//!         _request: tonic::Request<v1::BrowseRequest>,
//!     ) -> Result<tonic::Response<v1::BrowsePage>, tonic::Status> {
//!         Ok(tonic::Response::new(v1::BrowsePage::default()))
//!     }
//! }
//! ```
//!
//! 下面的**空白实现**（blanket impl）把它接到生成的 trait 上，所以
//! `StorageProviderServer::new(MyProvider)` 照常可用。
//!
//! # 流式方法返回 `BoxStream`
//!
//! 生成的 trait 对 3 个流式 rpc 有**关联类型**（`ScanImportSourceStream` /
//! `GenerateThumbnailsStream` / `ReadTransferSourceStream`），而关联类型在
//! stable Rust 上不能有默认值。所以空白实现把三者**固定**为
//! `BoxStream<'static, Result<…, Status>>` —— 插件的流式方法返回一个 boxed
//! stream 即可（`futures::stream::… .boxed()`）。代价是一次装箱，换来的是
//! 「不必为每个流式方法再定义关联类型」。
//!
//! # 为什么不是「拆分 service」那条路
//!
//! 报告里另一个方向是把 32 个 rpc 拆成「必需 / 可选 / 转存 / 合并播放」几个
//! 小 service。那要**改 proto 并重生成**，且宿主得管理多条 service 的注册与
//! 版本 —— 在一个默认实现层就能消灭这笔税的前提下，代价不匹配。

use futures::stream::BoxStream;
use tonic::{Request, Response, Status, Streaming};

use crate::v1;

/// 统一的「未实现」状态。
///
/// 带上方法名，宿主日志里能直接看出插件缺的是哪个能力，而不是一个光秃秃的
/// `UNIMPLEMENTED`。
fn unimplemented(method: &'static str) -> Status {
    Status::unimplemented(format!("StorageProvider::{method} 未实现"))
}

/// `StorageProvider` 的插件侧 trait：**每个方法都有默认体**。
///
/// 对应上游 `STORAGE_PROVIDER_METHODS` 的 32 个方法（12 个必需 + 可选能力 +
/// 合并播放 + 转存）。
#[tonic::async_trait]
#[allow(clippy::too_many_arguments)]
pub trait StorageProviderExt: Send + Sync + 'static {
    // ── ① ~ ⑫ 必需能力 ──

    /// ① 目录浏览。
    async fn browse(
        &self,
        _request: Request<v1::BrowseRequest>,
    ) -> Result<Response<v1::BrowsePage>, Status> {
        Err(unimplemented("browse"))
    }

    /// ② 扫描导入来源（server streaming）。
    async fn scan_import_source(
        &self,
        _request: Request<v1::ScanImportSourceRequest>,
    ) -> Result<Response<BoxStream<'static, Result<v1::ImportFileEntry, Status>>>, Status> {
        Err(unimplemented("scan_import_source"))
    }

    /// ③ 读取导入来源文件。
    async fn read_import_file(
        &self,
        _request: Request<v1::ReadImportFileRequest>,
    ) -> Result<Response<v1::ImportFileContent>, Status> {
        Err(unimplemented("read_import_file"))
    }

    /// ④ 删除导入来源文件。
    async fn delete_import_file(
        &self,
        _request: Request<v1::DeleteImportFileRequest>,
    ) -> Result<Response<v1::DeleteImportFileResponse>, Status> {
        Err(unimplemented("delete_import_file"))
    }

    /// ⑤ 暂存导入文件。
    async fn stage_import_file(
        &self,
        _request: Request<v1::StageImportFileRequest>,
    ) -> Result<Response<v1::StagedMedia>, Status> {
        Err(unimplemented("stage_import_file"))
    }

    /// ⑥ 完成导入。
    async fn finalize_import(
        &self,
        _request: Request<v1::FinalizeImportRequest>,
    ) -> Result<Response<v1::FinalizeImportResponse>, Status> {
        Err(unimplemented("finalize_import"))
    }

    /// ⑦ 中止导入。
    async fn abort_import(
        &self,
        _request: Request<v1::AbortImportRequest>,
    ) -> Result<Response<v1::AbortImportResponse>, Status> {
        Err(unimplemented("abort_import"))
    }

    /// ⑧ 删除媒体。
    async fn delete_media(
        &self,
        _request: Request<v1::DeleteMediaRequest>,
    ) -> Result<Response<v1::DeleteMediaResponse>, Status> {
        Err(unimplemented("delete_media"))
    }

    /// ⑨ 计算文件指纹。
    async fn compute_file_hash(
        &self,
        _request: Request<v1::ComputeFileHashRequest>,
    ) -> Result<Response<v1::ComputeFileHashResponse>, Status> {
        Err(unimplemented("compute_file_hash"))
    }

    /// ⑩ 播放计划。
    async fn plan_playback(
        &self,
        _request: Request<v1::PlanPlaybackRequest>,
    ) -> Result<Response<v1::PlanPlaybackResponse>, Status> {
        Err(unimplemented("plan_playback"))
    }

    /// ⑪ 生成缩略图（server streaming：进度事件 + **终态产物清单**）。
    ///
    /// 流里最后一条**必须**是 `payload = done`（`ThumbnailGeneration`）——
    /// 宿主拿不到它就无法落库，会按「provider 违约」处理。
    /// 见 `proto/storage.proto` 里 `GenerateThumbnailsResponse` 的注释。
    async fn generate_thumbnails(
        &self,
        _request: Request<v1::GenerateThumbnailsRequest>,
    ) -> Result<Response<BoxStream<'static, Result<v1::GenerateThumbnailsResponse, Status>>>, Status>
    {
        Err(unimplemented("generate_thumbnails"))
    }

    /// ⑫ 切片段。
    async fn create_clip(
        &self,
        _request: Request<v1::CreateClipRequest>,
    ) -> Result<Response<v1::CreateClipResponse>, Status> {
        Err(unimplemented("create_clip"))
    }

    // ── 可选能力 ──

    /// 探测时长。
    async fn probe_duration_seconds(
        &self,
        _request: Request<v1::ProbeDurationRequest>,
    ) -> Result<Response<v1::ProbeDurationResponse>, Status> {
        Err(unimplemented("probe_duration_seconds"))
    }

    /// 探测分辨率。
    async fn probe_resolution(
        &self,
        _request: Request<v1::ProbeResolutionRequest>,
    ) -> Result<Response<v1::ProbeResolutionResponse>, Status> {
        Err(unimplemented("probe_resolution"))
    }

    /// 探测视频信息。
    async fn probe_video_info(
        &self,
        _request: Request<v1::ProbeVideoInfoRequest>,
    ) -> Result<Response<v1::ProbeVideoInfoResponse>, Status> {
        Err(unimplemented("probe_video_info"))
    }

    /// 打开封面来源。
    async fn open_cover_source(
        &self,
        _request: Request<v1::OpenCoverSourceRequest>,
    ) -> Result<Response<v1::OpenCoverSourceResponse>, Status> {
        Err(unimplemented("open_cover_source"))
    }

    /// 导入来源身份。
    async fn get_import_source_identity(
        &self,
        _request: Request<v1::GetImportSourceIdentityRequest>,
    ) -> Result<Response<v1::GetImportSourceIdentityResponse>, Status> {
        Err(unimplemented("get_import_source_identity"))
    }

    /// 扫描媒体引用。
    async fn scan_media_refs(
        &self,
        _request: Request<v1::ScanMediaRefsRequest>,
    ) -> Result<Response<v1::ScanMediaRefsResponse>, Status> {
        Err(unimplemented("scan_media_refs"))
    }

    /// 扫描受管媒体引用键。
    async fn scan_managed_media_ref_keys(
        &self,
        _request: Request<v1::ScanManagedMediaRefKeysRequest>,
    ) -> Result<Response<v1::ScanManagedMediaRefKeysResponse>, Status> {
        Err(unimplemented("scan_managed_media_ref_keys"))
    }

    /// 单个受管媒体引用键。
    async fn managed_media_ref_key(
        &self,
        _request: Request<v1::ManagedMediaRefKeyRequest>,
    ) -> Result<Response<v1::ManagedMediaRefKeyResponse>, Status> {
        Err(unimplemented("managed_media_ref_key"))
    }

    /// 空间占用。
    async fn get_space_usage(
        &self,
        _request: Request<v1::GetSpaceUsageRequest>,
    ) -> Result<Response<v1::GetSpaceUsageResponse>, Status> {
        Err(unimplemented("get_space_usage"))
    }

    // ── 合并播放 ──

    /// 合并播放计划。
    async fn plan_merged_playback(
        &self,
        _request: Request<v1::PlanMergedPlaybackRequest>,
    ) -> Result<Response<v1::PlanMergedPlaybackResponse>, Status> {
        Err(unimplemented("plan_merged_playback"))
    }

    /// 合并播放预检。
    async fn preflight_merged_playback(
        &self,
        _request: Request<v1::PreflightMergedPlaybackRequest>,
    ) -> Result<Response<v1::PreflightMergedPlaybackResponse>, Status> {
        Err(unimplemented("preflight_merged_playback"))
    }

    // ── 转存：源端 ──

    /// 打开转存源。
    async fn open_transfer_source(
        &self,
        _request: Request<v1::OpenTransferSourceRequest>,
    ) -> Result<Response<v1::OpenTransferSourceResponse>, Status> {
        Err(unimplemented("open_transfer_source"))
    }

    /// 读取转存源（**双向流**）。
    async fn read_transfer_source(
        &self,
        _request: Request<Streaming<v1::TransferReadRequest>>,
    ) -> Result<Response<BoxStream<'static, Result<v1::TransferReadResponse, Status>>>, Status>
    {
        Err(unimplemented("read_transfer_source"))
    }

    /// 断言转存源未变化。
    async fn assert_transfer_source_unchanged(
        &self,
        _request: Request<v1::TransferAssertRequest>,
    ) -> Result<Response<v1::TransferAssertResponse>, Status> {
        Err(unimplemented("assert_transfer_source_unchanged"))
    }

    /// 关闭转存源。
    async fn close_transfer_source(
        &self,
        _request: Request<v1::CloseTransferSourceRequest>,
    ) -> Result<Response<v1::CloseTransferSourceResponse>, Status> {
        Err(unimplemented("close_transfer_source"))
    }

    /// 清理转存源。
    async fn cleanup_transfer_source(
        &self,
        _request: Request<v1::CleanupTransferSourceRequest>,
    ) -> Result<Response<v1::CleanupTransferSourceResponse>, Status> {
        Err(unimplemented("cleanup_transfer_source"))
    }

    // ── 转存：目标端 ──

    /// 暂存转存。
    async fn stage_transfer(
        &self,
        _request: Request<v1::StageTransferRequest>,
    ) -> Result<Response<v1::StageTransferResponse>, Status> {
        Err(unimplemented("stage_transfer"))
    }

    /// 完成转存。
    async fn finalize_transfer(
        &self,
        _request: Request<v1::FinalizeTransferRequest>,
    ) -> Result<Response<v1::FinalizeTransferResponse>, Status> {
        Err(unimplemented("finalize_transfer"))
    }

    /// 中止转存。
    async fn abort_transfer(
        &self,
        _request: Request<v1::AbortTransferRequest>,
    ) -> Result<Response<v1::AbortTransferResponse>, Status> {
        Err(unimplemented("abort_transfer"))
    }

    // ── bundle 级能力 ──

    /// 准备媒体库。
    async fn prepare_library(
        &self,
        _request: Request<v1::PrepareLibraryRequest>,
    ) -> Result<Response<v1::PrepareLibraryResponse>, Status> {
        Err(unimplemented("prepare_library"))
    }
}

/// `DownloadProvider` 的插件侧 trait：5 个方法，全部有默认体。
#[tonic::async_trait]
pub trait DownloadProviderExt: Send + Sync + 'static {
    /// 提交下载。
    async fn submit(
        &self,
        _request: Request<v1::SubmitRequest>,
    ) -> Result<Response<v1::SubmitResponse>, Status> {
        Err(Status::unimplemented("DownloadProvider::submit 未实现"))
    }

    /// 列出下载任务。
    async fn list_tasks(
        &self,
        _request: Request<v1::ListTasksRequest>,
    ) -> Result<Response<v1::ListTasksResponse>, Status> {
        Err(Status::unimplemented("DownloadProvider::list_tasks 未实现"))
    }

    /// 删除下载任务。
    async fn delete_task(
        &self,
        _request: Request<v1::DeleteTaskRequest>,
    ) -> Result<Response<v1::DeleteTaskResponse>, Status> {
        Err(Status::unimplemented(
            "DownloadProvider::delete_task 未实现",
        ))
    }

    /// 准备下载客户端。
    async fn prepare_client(
        &self,
        _request: Request<v1::PrepareClientRequest>,
    ) -> Result<Response<v1::PrepareClientResponse>, Status> {
        Err(Status::unimplemented(
            "DownloadProvider::prepare_client 未实现",
        ))
    }

    /// 测试下载客户端连通性。
    async fn test_client(
        &self,
        _request: Request<v1::TestClientRequest>,
    ) -> Result<Response<v1::TestClientResponse>, Status> {
        Err(Status::unimplemented(
            "DownloadProvider::test_client 未实现",
        ))
    }
}

/// 空白实现：把 [`StorageProviderExt`] 接到生成的 `StorageProvider` 上。
///
/// 3 个流式关联类型**固定**为 `BoxStream<…>` —— 见模块文档「流式方法返回
/// `BoxStream`」。
#[tonic::async_trait]
impl<T: StorageProviderExt> v1::storage_provider_server::StorageProvider for T {
    type ScanImportSourceStream = BoxStream<'static, Result<v1::ImportFileEntry, Status>>;
    // P1-1 修订后：流里既要进度也要**终态产物清单**，所以是
    // `GenerateThumbnailsResponse`（含 `oneof {progress, done}`），不是裸的
    // `ProgressEvent`。见 `proto/storage.proto` 里那条消息上的注释。
    type GenerateThumbnailsStream =
        BoxStream<'static, Result<v1::GenerateThumbnailsResponse, Status>>;
    type ReadTransferSourceStream = BoxStream<'static, Result<v1::TransferReadResponse, Status>>;

    async fn browse(
        &self,
        request: Request<v1::BrowseRequest>,
    ) -> Result<Response<v1::BrowsePage>, Status> {
        StorageProviderExt::browse(self, request).await
    }

    async fn scan_import_source(
        &self,
        request: Request<v1::ScanImportSourceRequest>,
    ) -> Result<Response<Self::ScanImportSourceStream>, Status> {
        StorageProviderExt::scan_import_source(self, request).await
    }

    async fn read_import_file(
        &self,
        request: Request<v1::ReadImportFileRequest>,
    ) -> Result<Response<v1::ImportFileContent>, Status> {
        StorageProviderExt::read_import_file(self, request).await
    }

    async fn delete_import_file(
        &self,
        request: Request<v1::DeleteImportFileRequest>,
    ) -> Result<Response<v1::DeleteImportFileResponse>, Status> {
        StorageProviderExt::delete_import_file(self, request).await
    }

    async fn stage_import_file(
        &self,
        request: Request<v1::StageImportFileRequest>,
    ) -> Result<Response<v1::StagedMedia>, Status> {
        StorageProviderExt::stage_import_file(self, request).await
    }

    async fn finalize_import(
        &self,
        request: Request<v1::FinalizeImportRequest>,
    ) -> Result<Response<v1::FinalizeImportResponse>, Status> {
        StorageProviderExt::finalize_import(self, request).await
    }

    async fn abort_import(
        &self,
        request: Request<v1::AbortImportRequest>,
    ) -> Result<Response<v1::AbortImportResponse>, Status> {
        StorageProviderExt::abort_import(self, request).await
    }

    async fn delete_media(
        &self,
        request: Request<v1::DeleteMediaRequest>,
    ) -> Result<Response<v1::DeleteMediaResponse>, Status> {
        StorageProviderExt::delete_media(self, request).await
    }

    async fn compute_file_hash(
        &self,
        request: Request<v1::ComputeFileHashRequest>,
    ) -> Result<Response<v1::ComputeFileHashResponse>, Status> {
        StorageProviderExt::compute_file_hash(self, request).await
    }

    async fn plan_playback(
        &self,
        request: Request<v1::PlanPlaybackRequest>,
    ) -> Result<Response<v1::PlanPlaybackResponse>, Status> {
        StorageProviderExt::plan_playback(self, request).await
    }

    async fn generate_thumbnails(
        &self,
        request: Request<v1::GenerateThumbnailsRequest>,
    ) -> Result<Response<Self::GenerateThumbnailsStream>, Status> {
        StorageProviderExt::generate_thumbnails(self, request).await
    }

    async fn create_clip(
        &self,
        request: Request<v1::CreateClipRequest>,
    ) -> Result<Response<v1::CreateClipResponse>, Status> {
        StorageProviderExt::create_clip(self, request).await
    }

    async fn probe_duration_seconds(
        &self,
        request: Request<v1::ProbeDurationRequest>,
    ) -> Result<Response<v1::ProbeDurationResponse>, Status> {
        StorageProviderExt::probe_duration_seconds(self, request).await
    }

    async fn probe_resolution(
        &self,
        request: Request<v1::ProbeResolutionRequest>,
    ) -> Result<Response<v1::ProbeResolutionResponse>, Status> {
        StorageProviderExt::probe_resolution(self, request).await
    }

    async fn probe_video_info(
        &self,
        request: Request<v1::ProbeVideoInfoRequest>,
    ) -> Result<Response<v1::ProbeVideoInfoResponse>, Status> {
        StorageProviderExt::probe_video_info(self, request).await
    }

    async fn open_cover_source(
        &self,
        request: Request<v1::OpenCoverSourceRequest>,
    ) -> Result<Response<v1::OpenCoverSourceResponse>, Status> {
        StorageProviderExt::open_cover_source(self, request).await
    }

    async fn get_import_source_identity(
        &self,
        request: Request<v1::GetImportSourceIdentityRequest>,
    ) -> Result<Response<v1::GetImportSourceIdentityResponse>, Status> {
        StorageProviderExt::get_import_source_identity(self, request).await
    }

    async fn scan_media_refs(
        &self,
        request: Request<v1::ScanMediaRefsRequest>,
    ) -> Result<Response<v1::ScanMediaRefsResponse>, Status> {
        StorageProviderExt::scan_media_refs(self, request).await
    }

    async fn scan_managed_media_ref_keys(
        &self,
        request: Request<v1::ScanManagedMediaRefKeysRequest>,
    ) -> Result<Response<v1::ScanManagedMediaRefKeysResponse>, Status> {
        StorageProviderExt::scan_managed_media_ref_keys(self, request).await
    }

    async fn managed_media_ref_key(
        &self,
        request: Request<v1::ManagedMediaRefKeyRequest>,
    ) -> Result<Response<v1::ManagedMediaRefKeyResponse>, Status> {
        StorageProviderExt::managed_media_ref_key(self, request).await
    }

    async fn get_space_usage(
        &self,
        request: Request<v1::GetSpaceUsageRequest>,
    ) -> Result<Response<v1::GetSpaceUsageResponse>, Status> {
        StorageProviderExt::get_space_usage(self, request).await
    }

    async fn plan_merged_playback(
        &self,
        request: Request<v1::PlanMergedPlaybackRequest>,
    ) -> Result<Response<v1::PlanMergedPlaybackResponse>, Status> {
        StorageProviderExt::plan_merged_playback(self, request).await
    }

    async fn preflight_merged_playback(
        &self,
        request: Request<v1::PreflightMergedPlaybackRequest>,
    ) -> Result<Response<v1::PreflightMergedPlaybackResponse>, Status> {
        StorageProviderExt::preflight_merged_playback(self, request).await
    }

    async fn open_transfer_source(
        &self,
        request: Request<v1::OpenTransferSourceRequest>,
    ) -> Result<Response<v1::OpenTransferSourceResponse>, Status> {
        StorageProviderExt::open_transfer_source(self, request).await
    }

    async fn read_transfer_source(
        &self,
        request: Request<Streaming<v1::TransferReadRequest>>,
    ) -> Result<Response<Self::ReadTransferSourceStream>, Status> {
        StorageProviderExt::read_transfer_source(self, request).await
    }

    async fn assert_transfer_source_unchanged(
        &self,
        request: Request<v1::TransferAssertRequest>,
    ) -> Result<Response<v1::TransferAssertResponse>, Status> {
        StorageProviderExt::assert_transfer_source_unchanged(self, request).await
    }

    async fn close_transfer_source(
        &self,
        request: Request<v1::CloseTransferSourceRequest>,
    ) -> Result<Response<v1::CloseTransferSourceResponse>, Status> {
        StorageProviderExt::close_transfer_source(self, request).await
    }

    async fn cleanup_transfer_source(
        &self,
        request: Request<v1::CleanupTransferSourceRequest>,
    ) -> Result<Response<v1::CleanupTransferSourceResponse>, Status> {
        StorageProviderExt::cleanup_transfer_source(self, request).await
    }

    async fn stage_transfer(
        &self,
        request: Request<v1::StageTransferRequest>,
    ) -> Result<Response<v1::StageTransferResponse>, Status> {
        StorageProviderExt::stage_transfer(self, request).await
    }

    async fn finalize_transfer(
        &self,
        request: Request<v1::FinalizeTransferRequest>,
    ) -> Result<Response<v1::FinalizeTransferResponse>, Status> {
        StorageProviderExt::finalize_transfer(self, request).await
    }

    async fn abort_transfer(
        &self,
        request: Request<v1::AbortTransferRequest>,
    ) -> Result<Response<v1::AbortTransferResponse>, Status> {
        StorageProviderExt::abort_transfer(self, request).await
    }

    async fn prepare_library(
        &self,
        request: Request<v1::PrepareLibraryRequest>,
    ) -> Result<Response<v1::PrepareLibraryResponse>, Status> {
        StorageProviderExt::prepare_library(self, request).await
    }
}

/// 空白实现：把 [`DownloadProviderExt`] 接到生成的 `DownloadProvider` 上。
#[tonic::async_trait]
impl<T: DownloadProviderExt> v1::download_provider_server::DownloadProvider for T {
    async fn submit(
        &self,
        request: Request<v1::SubmitRequest>,
    ) -> Result<Response<v1::SubmitResponse>, Status> {
        DownloadProviderExt::submit(self, request).await
    }

    async fn list_tasks(
        &self,
        request: Request<v1::ListTasksRequest>,
    ) -> Result<Response<v1::ListTasksResponse>, Status> {
        DownloadProviderExt::list_tasks(self, request).await
    }

    async fn delete_task(
        &self,
        request: Request<v1::DeleteTaskRequest>,
    ) -> Result<Response<v1::DeleteTaskResponse>, Status> {
        DownloadProviderExt::delete_task(self, request).await
    }

    async fn prepare_client(
        &self,
        request: Request<v1::PrepareClientRequest>,
    ) -> Result<Response<v1::PrepareClientResponse>, Status> {
        DownloadProviderExt::prepare_client(self, request).await
    }

    async fn test_client(
        &self,
        request: Request<v1::TestClientRequest>,
    ) -> Result<Response<v1::TestClientResponse>, Status> {
        DownloadProviderExt::test_client(self, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::storage_provider_server::StorageProvider as GeneratedStorageProvider;

    /// 只实现 `browse` 的极简插件 —— 其余 31 个方法全部走默认体。
    struct MinimalProvider;

    #[tonic::async_trait]
    impl StorageProviderExt for MinimalProvider {
        async fn browse(
            &self,
            _request: Request<v1::BrowseRequest>,
        ) -> Result<Response<v1::BrowsePage>, Status> {
            Ok(Response::new(v1::BrowsePage::default()))
        }
    }

    #[tokio::test]
    async fn only_the_overridden_method_is_implemented() {
        let provider = MinimalProvider;
        // 覆盖过的方法可用。
        assert!(GeneratedStorageProvider::browse(
            &provider,
            Request::new(v1::BrowseRequest::default())
        )
        .await
        .is_ok());
        // 没覆盖的方法统一是 UNIMPLEMENTED，且消息里带方法名。
        let err = GeneratedStorageProvider::plan_playback(
            &provider,
            Request::new(v1::PlanPlaybackRequest::default()),
        )
        .await
        .expect_err("未覆盖的方法应是 unimplemented");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        assert!(err.message().contains("plan_playback"), "{}", err.message());
    }

    #[tokio::test]
    async fn streaming_defaults_are_unimplemented_too() {
        let provider = MinimalProvider;
        // 用 match 而不是 `expect_err`：流式的 `Ok` 类型是 `BoxStream`，
        // 它不实现 `Debug`，`expect_err` 编译不过。
        match GeneratedStorageProvider::scan_import_source(
            &provider,
            Request::new(v1::ScanImportSourceRequest::default()),
        )
        .await
        {
            Ok(_) => panic!("流式方法默认也应是 unimplemented"),
            Err(err) => assert_eq!(err.code(), tonic::Code::Unimplemented),
        }
    }

    /// 只实现 `submit` 的极简下载插件。
    struct MinimalDownload;

    #[tonic::async_trait]
    impl DownloadProviderExt for MinimalDownload {
        async fn submit(
            &self,
            _request: Request<v1::SubmitRequest>,
        ) -> Result<Response<v1::SubmitResponse>, Status> {
            Ok(Response::new(v1::SubmitResponse::default()))
        }
    }

    #[tokio::test]
    async fn the_download_default_layer_works_too() {
        use crate::v1::download_provider_server::DownloadProvider as GeneratedDownloadProvider;
        let provider = MinimalDownload;
        assert!(GeneratedDownloadProvider::submit(
            &provider,
            Request::new(v1::SubmitRequest::default())
        )
        .await
        .is_ok());
        let err = GeneratedDownloadProvider::list_tasks(
            &provider,
            Request::new(v1::ListTasksRequest::default()),
        )
        .await
        .expect_err("未覆盖的方法应是 unimplemented");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }
}
