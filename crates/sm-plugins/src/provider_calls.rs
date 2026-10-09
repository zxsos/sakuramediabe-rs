//! provider 数据面的**调用面**：真的把 storage / download rpc 发出去。
//!
//! # 上游对应 / 与 [`crate::extension_calls`] 的分工
//!
//! | 模块 | 上游 | 服务 |
//! |---|---|---|
//! | [`crate::extension_calls`] | `MetadataSourceExtensionService` / `RankingSourceExtensionService` | 两个扩展点 |
//! | 本模块 | `StorageProvider` / `DownloadProvider`（`proto/storage.proto`） | **provider 数据面** |
//!
//! 两者发的是**同一个插件进程**上的不同 service（proto 里各是一个 service，
//! 但没有任何字段声明另一个端口 —— 只有数据面才有 `data_plane_endpoint`）。
//! 所以客户端都用控制面那条 `Channel` 建：`Client::new(channel)`。
//!
//! # ✅ 已闭合的 ABI 缺口：**错误码过线了**
//!
//! 上游的失败是一条结构化记录：
//!
//! ```text
//!   ProviderOperationError(provider_key, operation, code, safe_message, retryable)
//!   code ∈ {invalid_config, authentication_failed, source_not_found,
//!           task_not_managed, source_blacklisted, unsupported, unavailable}
//! ```
//!
//! 而**宿主的控制流按 `code` 与 `retryable` 分支**，不是按「成功/失败」：
//!
//! | 调用点 | 分支 |
//! |---|---|
//! | `delete_media` | `source_not_found` → **继续**清理本地元数据（远端对象早已不在）|
//! | 缩略图生成 | `unavailable` 且 `retryable` → 走**延迟轨**（不是失败轨）|
//! | 播放 | `authentication_failed` → 401；`unavailable` → 503 |
//!
//! ## 闭合方式：`ProviderError` 走 `Status` 的 `details`
//!
//! `proto/common.proto:322` 那个 `ProviderError` 之前没有任何 rpc 用它做
//! 错误通道，宿主只拿得到 `tonic::Status`。现在它由
//! [`sm_plugin_api::error::to_status`] 编进 `Status::details`、
//! 由 [`sm_plugin_api::error::from_status`] 还原 —— **不改任何 rpc 签名**，
//! 老插件（不带结构）走回落路径。
//!
//! ## 为什么回落路径**必须留着**
//!
//! 没带结构的插件（手写 `Status::unimplemented` 的那 28 个未实现 rpc 就是）
//! 解不出 `ProviderError`。那时仍按 gRPC 码猜 `code`，`retryable` 用
//! [`sm_plugin_api::error::default_retryable`] 的保守猜测。
//!
//! ## 猜的那条路**仍然有损**
//!
//! 最危险的一处没变，只是现在只在回落时才发生：
//!
//! > `Status::NotFound` 同时被「远端文件不在」与「媒体库不存在」用。
//! > 映射成 `source_not_found` 之后，`delete_media` 会当作「远端早已删掉」
//! > 而**继续清理本地记录** —— 那正是我们要的结果没错；但如果真实原因是
//! > 「库没配好」，我们就**以为远端删过了**，而实际上文件还在。
//!
//! 所以插件作者应当调 `to_status` 报结构化错误，而不是只给一个 gRPC 码。

use sm_plugin_api::v1::storage_provider_client::StorageProviderClient;
use sm_plugin_api::v1::{
    generate_thumbnails_response, AbortImportRequest, ComputeFileHashRequest,
    DeleteImportFileRequest, DeleteMediaRequest, FinalizeImportRequest, GenerateThumbnailsRequest,
    LibraryHandle, MediaHandle, ProviderErrorCode, ScanImportSourceRequest, SourceDisposition,
    StageImportFileRequest, ThumbnailGeneration,
};
use tonic::transport::Channel;
use tonic::{Code, Status};

/// provider 操作失败。
///
/// # `safe_message` 与 `plugin_detail` **必须分开**
///
/// 上游 proto 的注释写得很清楚：`safe_message` 是「对外展示的安全文案，**不得
/// 包含 Cookie、密码或内部路径**」。而 `tonic::Status` 里那句话是**插件自己拼的
/// 调试文本**（`plugin-ref-local` 就会把文件系统路径拼进去）。所以：
///
/// - [`Self::safe_message`] 是**宿主生成**的固定文案（按 code 决定），可以直接
///   给客户端；
/// - [`Self::plugin_detail`] 是插件的原文，**只该进日志**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOperationError {
    pub provider_key: String,
    /// 哪个操作（`delete_media` / `generate_thumbnails` …）。上游把它一并带上，
    /// 因为同一个 provider 的不同操作失败原因可能不同。
    pub operation: String,
    pub code: ProviderErrorCode,
    /// 对外文案。**优先**用插件给的 `safe_message`（结构化错误带过来的），
    /// 没有才用宿主自己的模板 —— 见 [`Self::retryable`] 旁边同源的说明。
    pub safe_message: String,
    /// 插件的原始文本。**只进日志。**
    pub plugin_detail: String,
    /// provider **显式**给的「值不值得重试」。`None` = 它没说（没带结构化
    /// 错误），此时 [`Self::retryable`] 退回按码猜。
    pub provider_retryable: Option<bool>,
}

impl ProviderOperationError {
    /// 与上游 `ProviderOperationError.code` **逐字一致**的字符串。
    ///
    /// 这个值会原样透传给客户端（proto 注释：「`code` 是对外契约」），
    /// 所以不能改成别的写法。
    pub fn code(&self) -> &'static str {
        provider_error_code_name(self.code)
    }

    pub fn safe_message(&self) -> &str {
        &self.safe_message
    }

    pub fn plugin_detail(&self) -> &str {
        &self.plugin_detail
    }

    /// 是否值得重试。
    ///
    /// **优先信 provider 说的**（[`Self::provider_retryable`]）—— 上游的
    /// `retryable` 是 provider 给的独立字段，不是从 `code` 推的
    /// （`provider_protocol.py:661-665`：`unsupported` 显式给 `retryable=False`）。
    /// 它没说时才退回按码猜（[`sm_plugin_api::error::default_retryable`]）。
    pub fn retryable(&self) -> bool {
        self.provider_retryable
            .unwrap_or_else(|| sm_plugin_api::error::default_retryable(self.code))
    }
}

/// 与上游 `ProviderOperationError.code` 的字符串字面量一一对应。
///
/// ⚠️ `Unspecified` **不在**上游那七个里 —— 它表示「宿主没认出这个错误」。
/// 调用方必须把它当**未知失败**（500 一类）而不是硬塞进七个之一，
/// 否则会把「插件崩了」说成「你的配置不对」。
pub fn provider_error_code_name(code: ProviderErrorCode) -> &'static str {
    match code {
        ProviderErrorCode::InvalidConfig => "invalid_config",
        ProviderErrorCode::AuthenticationFailed => "authentication_failed",
        ProviderErrorCode::SourceNotFound => "source_not_found",
        ProviderErrorCode::TaskNotManaged => "task_not_managed",
        ProviderErrorCode::SourceBlacklisted => "source_blacklisted",
        ProviderErrorCode::Unsupported => "unsupported",
        ProviderErrorCode::Unavailable => "unavailable",
        ProviderErrorCode::Unspecified => "unspecified",
    }
}

/// gRPC 状态 → 上游的 provider 错误。
///
/// # 先试结构化错误，解不出才猜
///
/// 插件若按契约用 [`sm_plugin_api::error::to_status`] 报错，`Status::details`
/// 里就是一条 `ProviderError`：`code` / `safe_message` / `retryable` 三个字段
/// 全部**无损**到达，猜的那条路一步都不走。
///
/// 只有解不出（老插件、或手写 `Status` 的插件）才进下面的 `match` 按 gRPC 码
/// 猜 —— 那条路**必然有损**，危险的那处写在模块文档里。
pub fn classify_status(
    provider_key: &str,
    operation: &str,
    status: Status,
) -> ProviderOperationError {
    if let Some(structured) = sm_plugin_api::error::from_status(&status) {
        let code =
            ProviderErrorCode::try_from(structured.code).unwrap_or(ProviderErrorCode::Unspecified);
        return ProviderOperationError {
            provider_key: provider_key.to_owned(),
            operation: operation.to_owned(),
            code,
            // 插件给的 `safe_message` 是 proto 定义的「对外展示的安全文案」；
            // 空串时（它懒得给）仍用宿主自己的模板。
            safe_message: match structured.safe_message.as_str() {
                "" => safe_message_for(code).to_owned(),
                text => text.to_owned(),
            },
            plugin_detail: status.message().to_owned(),
            provider_retryable: Some(structured.retryable),
        };
    }
    let code = match status.code() {
        // 上游 `delete_media` / `plan_playback` 的「远端对象不在」。
        //
        // ⚠️ 危险方向：`not_found` 也被「媒体库不存在」之类用。映射成
        // `source_not_found` 之后 `delete_media` 会当「远端早已删掉」而继续，
        // 于是「库没配好」这种真实原因会被吞掉。见模块文档。
        Code::NotFound => ProviderErrorCode::SourceNotFound,
        // 认证失败（cookie 过期、token 失效）。上游把它当 401 对外。
        Code::Unauthenticated => ProviderErrorCode::AuthenticationFailed,
        // 权限不足。上游的七个码里**没有** permission —— 取语义最近的认证失败
        // （都是「你的账号对这个源没权限」），而不是 `invalid_config`。
        Code::PermissionDenied => ProviderErrorCode::AuthenticationFailed,
        // 插件的 `Status::invalid_argument` 基本都是「请求字段缺失 / 配置不对」。
        Code::InvalidArgument | Code::OutOfRange => ProviderErrorCode::InvalidConfig,
        // 上游 `unsupported`：provider 明确不支持这个操作。
        // `plugin-ref-local` 那 28 个未实现的 rpc 全返回它。
        Code::Unimplemented => ProviderErrorCode::Unsupported,
        // 「当前状态不满足前置条件」。`plugin-ref-local` 用它报「源暂不可用」。
        Code::FailedPrecondition => ProviderErrorCode::Unavailable,
        // 后端/网络不可用，以及超时 —— 两者在上游都是「稍后再说」。
        Code::Unavailable | Code::DeadlineExceeded => ProviderErrorCode::Unavailable,
        // 其余（Unknown / Internal / DataLoss / Aborted / Cancelled / …）：
        // **认出就是错的**。上游七个码表达的都是「provider 可预期的失败」，
        // 而这些是「插件崩了/线路断了」——调用方该按未知失败处置。
        _ => ProviderErrorCode::Unspecified,
    };
    ProviderOperationError {
        provider_key: provider_key.to_owned(),
        operation: operation.to_owned(),
        code,
        safe_message: safe_message_for(code).to_owned(),
        plugin_detail: status.message().to_owned(),
        // 没带结构 → provider 什么都没说，只能按码猜。
        provider_retryable: None,
    }
}

/// 宿主生成的对外文案。
///
/// **不复用插件的文本**：那句里可能有内部路径、Cookie 片段或 provider 的内部
/// 术语（proto 的注释专门点了这条）。
fn safe_message_for(code: ProviderErrorCode) -> &'static str {
    match code {
        ProviderErrorCode::InvalidConfig => "媒体提供方配置无效",
        ProviderErrorCode::AuthenticationFailed => "媒体提供方认证失败",
        ProviderErrorCode::SourceNotFound => "远端对象不存在",
        ProviderErrorCode::TaskNotManaged => "任务不由此提供方管理",
        ProviderErrorCode::SourceBlacklisted => "该来源已被提供方拉黑",
        ProviderErrorCode::Unsupported => "媒体提供方不支持该操作",
        ProviderErrorCode::Unavailable => "媒体提供方暂不可用",
        ProviderErrorCode::Unspecified => "媒体提供方操作失败",
    }
}

/// 连到某个 provider 所在的**控制面**端点，拿一个 storage 客户端。
///
/// `endpoint` 形如 `http://127.0.0.1:50051`（[`crate::supervisor::PluginProcess::endpoint`]）。
pub async fn connect_storage(
    provider_key: &str,
    endpoint: &str,
    operation: &str,
) -> Result<StorageProviderClient<Channel>, ProviderOperationError> {
    let channel = Channel::from_shared(endpoint.to_owned())
        .map_err(|err| {
            // 连不上是**宿主侧**的问题（端点串写错/进程没起来），不是 provider
            // 的失败 —— 但对外还是走同一个信封，码按「暂不可用」。
            ProviderOperationError {
                provider_key: provider_key.to_owned(),
                operation: operation.to_owned(),
                code: ProviderErrorCode::Unavailable,
                safe_message: safe_message_for(ProviderErrorCode::Unavailable).to_owned(),
                plugin_detail: format!("连接 {endpoint} 失败：{err}"),
                // 连不上的是**宿主**自己（端点写错 / 进程没起），不是 provider
                // 在说话。它没给 retryable，按码猜（unavailable → 值得重试）。
                provider_retryable: None,
            }
        })?
        .connect()
        .await
        .map_err(|err| ProviderOperationError {
            provider_key: provider_key.to_owned(),
            operation: operation.to_owned(),
            code: ProviderErrorCode::Unavailable,
            safe_message: safe_message_for(ProviderErrorCode::Unavailable).to_owned(),
            plugin_detail: format!("连接 {endpoint} 失败：{err}"),
            provider_retryable: None,
        })?;
    Ok(StorageProviderClient::new(channel))
}

/// 让 provider 删掉远端媒体文件。上游 `StorageProvider.delete_media`。
///
/// ⚠️ **只删远端**。宿主那边的记录与本地图片由 `MediaService::delete_media`
/// 的后续两步负责 —— 而这个调用的**失败处置**是它的一部分：
/// `source_not_found` 表示远端早已不在，**继续**清理本地；其余码要冒到调用方。
pub async fn delete_media(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    media: MediaHandle,
) -> Result<(), ProviderOperationError> {
    client
        .delete_media(DeleteMediaRequest {
            library: Some(library),
            media: Some(media),
        })
        .await
        .map(|_| ())
        .map_err(|status| classify_status(provider_key, "delete_media", status))
}

// ══════════════════════════════════════════════════════════════════════
// 播放组
//
// 上游 `StorageProvider.handle_playback` / `handle_merged_playback`。
//
// ★ 上游返回 starlette `Response`（插件自己写 302 / 开流），跨进程不可行 ——
// 契约层把职责切开（`proto/storage.proto:20-24`）：
//
//     插件回答「字节从哪里来」  -> PlaybackPlan
//     宿主回答「HTTP 怎么写」   -> 统一构造 200/206/416/302
//
// 所以下面拿到的只是一份**描述**。好处是 Range / 206 / 416 只实现一次
// （上游 `local_provider` 与 115 各写了一份 `_parse_range`）。
// ══════════════════════════════════════════════════════════════════════

/// 让 provider 算出「字节从哪里来」。上游 `StorageProvider.handle_playback`。
///
/// ⚠️ 成功返回**不等于**能播：`PlaybackPlan.unavailable == true` 是**正常应答里
/// 的否定结果**（文件不在 / 权限没了），调用方该按「资源不存在」处理。把它当
/// 502 会让「影片文件被删」看起来像「插件坏了」。
pub async fn plan_playback(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    media: sm_plugin_api::v1::MediaHandle,
    resource_path: &str,
    delivery: i32,
) -> Result<sm_plugin_api::v1::PlanPlaybackResponse, ProviderOperationError> {
    client
        .plan_playback(sm_plugin_api::v1::PlanPlaybackRequest {
            media: Some(media),
            resource_path: resource_path.to_owned(),
            delivery,
        })
        .await
        .map(|response| response.into_inner())
        .map_err(|status| classify_status(provider_key, "plan_playback", status))
}

/// 合并播放的投递计划。上游 `StorageProvider.handle_merged_playback`。
///
/// 与 [`plan_playback`] 分开是因为合并流**没有单个 provider 地址可指** ——
/// 上游同样只允许中转。签发前还要过 [`preflight_merged_playback`]。
pub async fn plan_merged_playback(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    medias: Vec<sm_plugin_api::v1::MediaHandle>,
    resource_path: &str,
    delivery: i32,
) -> Result<sm_plugin_api::v1::PlanMergedPlaybackResponse, ProviderOperationError> {
    client
        .plan_merged_playback(sm_plugin_api::v1::PlanMergedPlaybackRequest {
            medias,
            resource_path: resource_path.to_owned(),
            delivery,
        })
        .await
        .map(|response| response.into_inner())
        .map_err(|status| classify_status(provider_key, "plan_merged_playback", status))
}

/// 合并播放预检。上游 `StorageProvider.preflight_merged_playback`。
///
/// 「签发合并播放 URL 前校验，不通过则**整体失败**」（`storage.proto:65-70`）。
/// 返回值是空消息 —— 信息全在 `Err` 里。
pub async fn preflight_merged_playback(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    medias: Vec<sm_plugin_api::v1::MediaHandle>,
) -> Result<(), ProviderOperationError> {
    client
        .preflight_merged_playback(sm_plugin_api::v1::PreflightMergedPlaybackRequest { medias })
        .await
        .map(|_| ())
        .map_err(|status| classify_status(provider_key, "preflight_merged_playback", status))
}

// ══════════════════════════════════════════════════════════════════════
// 导入组（`import_service` 用的那四个 + 指纹）
//
// 上游 `StorageProvider.scan_import_source` / `stage_import_file` /
// `finalize_import` / `abort_import` / `delete_import_file` /
// `compute_file_hash`。宿主侧的编排在 `sm_service::transfers::import_service`。
// ══════════════════════════════════════════════════════════════════════

/// 扫描到的一条导入文件（proto `ImportFile`）。
///
/// `source_ref` **原样回传**给后续调用 —— 宿主不解释它。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportFileEntry {
    pub source_ref: serde_json::Value,
    pub name: String,
    pub relative_path: String,
    pub size_bytes: i64,
    pub is_video: bool,
}

/// 已暂存的媒体（proto `StagedMedia`）。
///
/// ★ `receipt` 是 finalize / abort / 删源的**凭据**，宿主只保存与回传
/// （`Struct`，不解释）。丢了它就既没法提交也没法回滚 —— 所以暂存之后必须
/// 立刻把 `receipt` 落到一个「后面一定能拿到」的地方。
#[derive(Debug, Clone, PartialEq)]
pub struct StagedImport {
    pub storage_ref: serde_json::Value,
    pub receipt: serde_json::Value,
    pub size_bytes: i64,
    pub duration_seconds: Option<i64>,
    pub video_info: serde_json::Value,
    pub resolution: Option<String>,
}

/// 扫描导入来源（`server stream`）。返回**全部**条目。
///
/// # 为什么在这里把流收干成 `Vec`
///
/// 上游的 `scan_import_source` 也是「一次性拿到列表」再逐条处理
/// （`import_service.py:203-215`）：先全部扫描、再做去重与过滤。**中途失败
/// 就是整批失败** —— 半张列表会让「已索引」的判据拿到不完整的输入。
///
/// # 流以什么结束
///
/// 空流（`None`）= 正常结束；流中途 `Err` = provider 出错，按 `classify_status`
/// 分类后冒给调用方。
pub async fn scan_import_source_all(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    source_ref: &serde_json::Value,
) -> Result<Vec<ImportFileEntry>, ProviderOperationError> {
    let mut stream = client
        .scan_import_source(ScanImportSourceRequest {
            library: Some(library),
            source_ref: sm_plugin_api::json_struct::json_to_struct(source_ref),
        })
        .await
        .map_err(|status| classify_status(provider_key, "scan_import_source", status))?
        .into_inner();
    let mut entries = Vec::new();
    loop {
        match stream
            .message()
            .await
            .map_err(|status| classify_status(provider_key, "scan_import_source", status))?
        {
            None => return Ok(entries),
            Some(entry) => {
                let file = entry.file.unwrap_or_default();
                entries.push(ImportFileEntry {
                    source_ref: sm_plugin_api::json_struct::struct_to_json(
                        file.source_ref.as_ref(),
                    ),
                    name: file.name,
                    relative_path: file.relative_path,
                    size_bytes: file.size_bytes,
                    is_video: file.is_video,
                });
            }
        }
    }
}

/// 暂存一个导入文件。返回凭据。
///
/// # ★ `operation_key` 是幂等键
///
/// proto 的原话：「同一 operation_key 重复调用必须返回同一结果」。导入会重试
/// （宿主侧的失败重试、用户手动 retry），没有幂等键就会**复制出第二份媒体**。
/// 由调用方拼（上游是 `f"{namespace}:{index}"`），本函数只透传。
///
/// # ⚠️ 契约缺口：`in_place` 传不过去
///
/// proto 的 `SourceDisposition` **只有** `KEEP` / `DELETE_AFTER_COMMIT`
/// （`proto/common.proto` 里那个 enum 只有这两个值），而上游三方都认
/// `in_place`。也就是说：**这个 ABI 现在表达不了「原地导入」**。
/// 宿主侧必须按「不支持」处理（上游的 `in_place_import_unsupported` 那条
/// 422），而不是塞一个 UNSPECIFIED 蒙混过去 —— UNSPECIFIED 在 provider 侧
/// 的语义未定义。
pub async fn stage_import_file_call(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    source: &ImportFileEntry,
    placement: &str,
    disposition: SourceDisposition,
    operation_key: &str,
) -> Result<StagedImport, ProviderOperationError> {
    let staged = client
        .stage_import_file(StageImportFileRequest {
            library: Some(library),
            source: Some(crate::provider_calls::import_file_of(source)),
            placement: Some(sm_plugin_api::v1::ImportPlacement {
                relative_path: placement.to_owned(),
            }),
            source_disposition: disposition as i32,
            operation_key: operation_key.to_owned(),
        })
        .await
        .map_err(|status| classify_status(provider_key, "stage_import_file", status))?
        .into_inner();
    Ok(StagedImport {
        storage_ref: sm_plugin_api::json_struct::struct_to_json(staged.storage_ref.as_ref()),
        receipt: sm_plugin_api::json_struct::struct_to_json(staged.receipt.as_ref()),
        size_bytes: staged.size_bytes,
        duration_seconds: staged.duration_seconds,
        video_info: sm_plugin_api::json_struct::struct_to_json(staged.video_info.as_ref()),
        resolution: staged.resolution,
    })
}

/// 提交暂存（让 provider 真正落定）。
pub async fn finalize_import_call(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    receipt: &serde_json::Value,
) -> Result<(), ProviderOperationError> {
    client
        .finalize_import(FinalizeImportRequest {
            library: Some(library),
            receipt: sm_plugin_api::json_struct::json_to_struct(receipt),
        })
        .await
        .map(|_| ())
        .map_err(|status| classify_status(provider_key, "finalize_import", status))
}

/// 回滚暂存。**失败只记日志** —— 回滚失败不该改变这一条的最终结局
/// （它反正已经算失败了）。
pub async fn abort_import_call(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    receipt: &serde_json::Value,
) -> Result<(), ProviderOperationError> {
    client
        .abort_import(AbortImportRequest {
            library: Some(library),
            receipt: sm_plugin_api::json_struct::json_to_struct(receipt),
        })
        .await
        .map(|_| ())
        .map_err(|status| classify_status(provider_key, "abort_import", status))
}

/// 删掉导入来源文件（`delete_after_commit` 的收尾）。
pub async fn delete_import_file_call(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    receipt: &serde_json::Value,
) -> Result<(), ProviderOperationError> {
    client
        .delete_import_file(DeleteImportFileRequest {
            library: Some(library),
            receipt: sm_plugin_api::json_struct::json_to_struct(receipt),
        })
        .await
        .map(|_| ())
        .map_err(|status| classify_status(provider_key, "delete_import_file", status))
}

/// 算采样指纹。返回 `media-file-hash-v1:<40 hex>`。
///
/// 宿主侧用它做「同一份文件是否被导入过两次」的判据，所以算法必须与内置
/// 实现同源（proto 注释指向 `media-file-hash` crate）。
pub async fn compute_file_hash_call(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    media: MediaHandle,
) -> Result<String, ProviderOperationError> {
    Ok(client
        .compute_file_hash(ComputeFileHashRequest {
            library: Some(library),
            media: Some(media),
        })
        .await
        .map_err(|status| classify_status(provider_key, "compute_file_hash", status))?
        .into_inner()
        .file_hash)
}

/// 宿主侧的 [`ImportFileEntry`] → proto 的 `ImportFile`（回传时原样带回去）。
pub fn import_file_of(entry: &ImportFileEntry) -> sm_plugin_api::v1::ImportFile {
    sm_plugin_api::v1::ImportFile {
        source_ref: sm_plugin_api::json_struct::json_to_struct(&entry.source_ref),
        name: entry.name.clone(),
        relative_path: entry.relative_path.clone(),
        size_bytes: entry.size_bytes,
        is_video: entry.is_video,
    }
}

/// 缩略图生成的进度回调：`(文案, 当前, 总数)`。
///
/// 抽成别名既是为了 `clippy::type_complexity`，也是为了让调用点一眼看清三个参数
/// 的含义（裸写 `&mut dyn FnMut(&str, i32, i32)` 看不出谁是总数）。
///
/// # ★ `+ Send` 是**被逼出来的**，不是风格
///
/// 少了它，任何 `async fn` 只要带这个参数就**不是 `Send`** —— 于是它的 future
/// 进不了 `tokio::spawn`，也塞不进 `Pin<Box<dyn Future + Send>>`。
/// 组合根的 `StorageGateway` 正是后者，第一个真实调用点就把这个洞踩出来了。
pub type ThumbnailProgress = dyn FnMut(&str, i32, i32) + Send;

/// `generate_thumbnails` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailGenerationResult {
    /// provider 期望生成多少张。
    pub expected_count: u32,
    /// 产物清单。**相对 workspace 的路径** —— 宿主拿它去拼绝对路径。
    pub artifacts: Vec<GeneratedThumbnail>,
    /// 收到的进度事件条数。**不是产物数**（可能多也可能少）。
    pub progress_events: u32,
}

/// 一件产物：`workspace` 内的相对路径 + 它在媒体里的偏移（秒）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedThumbnail {
    pub offset_seconds: i32,
    /// 相对 `workspace` 的路径（proto 的 `ThumbnailArtifact.relative_path`）。
    pub relative_path: String,
}

/// 让 provider 生成缩略图，把产物写进宿主提供的 `workspace`。
///
/// 上游 `StorageProvider.generate_thumbnails`。流里有两类帧：
/// 进度事件（可多次）与**终态的 `ThumbnailGeneration`**（必须且只有一个）。
///
/// # ★ 没有 `done` 就是 provider 违约
///
/// 产物清单是**唯一**的落库依据：宿主不知道生成了几张、叫什么名字、该写到哪一
/// 格 timeline。所以流在没有它的情况下结束，按失败处理 —— 留着半成品（一堆没人
/// 认领的文件 + 一条 `succeeded` 状态）比报错糟得多。
/// （这正是 `docs/parallel/grpc-plugin-report.md` §4.3 P1-1 修掉的那个缺口：
/// 修订前宿主只能**扫磁盘猜**产物名字。）
///
/// # 出错时机
///
/// 流**中途**报错与**收不到 `done`** 都走 [`ProviderOperationError`]。前者由
/// `classify_status` 归类（缩略图任务据此决定进失败轨还是延迟轨），后者是
/// `Unspecified` —— 宿主认不出的失败，不该被当成「配置不对」。
pub async fn generate_thumbnails(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
    media: MediaHandle,
    workspace: &str,
    mut on_progress: Option<&mut ThumbnailProgress>,
) -> Result<ThumbnailGenerationResult, ProviderOperationError> {
    let mut stream = client
        .generate_thumbnails(GenerateThumbnailsRequest {
            library: Some(library),
            media: Some(media),
            workspace: workspace.to_owned(),
        })
        .await
        .map_err(|status| classify_status(provider_key, "generate_thumbnails", status))?
        .into_inner();

    let mut progress_events = 0_u32;
    let mut generation: Option<ThumbnailGeneration> = None;
    // `Streaming::message()` 是 tonic 自带的，不用为此引一个 stream 工具 crate
    // （本 crate 的 `tokio-stream` 只在 dev-dependencies 里）。
    loop {
        let Some(frame) = stream
            .message()
            .await
            .map_err(|status| classify_status(provider_key, "generate_thumbnails", status))?
        else {
            break;
        };
        match frame.payload {
            Some(generate_thumbnails_response::Payload::Progress(event)) => {
                progress_events += 1;
                if let Some(callback) = on_progress.as_mut() {
                    callback(&event.text, event.current, event.total);
                }
            }
            Some(generate_thumbnails_response::Payload::Done(done)) => {
                if generation.is_some() {
                    // 第二条 `done` 说明插件实现有问题。取**第一条**而不是覆盖：
                    // 第一条对应它已经写完的那批文件。
                    tracing::warn!(
                        provider_key,
                        "缩略图流里有不止一个 `done`，已忽略后续（产物清单必须与磁盘一致）"
                    );
                    continue;
                }
                generation = Some(done);
            }
            // `payload` 为 `None` = 插件发了个空帧。不致命，跳过。
            None => continue,
        }
    }

    let Some(generation) = generation else {
        return Err(ProviderOperationError {
            provider_key: provider_key.to_owned(),
            operation: "generate_thumbnails".to_owned(),
            code: ProviderErrorCode::Unspecified,
            safe_message: safe_message_for(ProviderErrorCode::Unspecified).to_owned(),
            plugin_detail: format!(
                "缩略图流在没有 `done` 帧的情况下结束了（收到 {progress_events} 条进度）—— \
                 宿主拿不到产物清单，无法落库"
            ),
            // 这是**宿主**判出的协议违反（流没给 `done`），不是 provider 报的错。
            provider_retryable: None,
        });
    };

    Ok(ThumbnailGenerationResult {
        expected_count: generation.expected_count.max(0) as u32,
        artifacts: generation
            .artifacts
            .into_iter()
            .map(|artifact| GeneratedThumbnail {
                offset_seconds: artifact.offset_seconds,
                relative_path: artifact.relative_path,
            })
            .collect(),
        progress_events,
    })
}

// ══════════════════════════════════════════════════════════════════════
// bundle 级能力（准备媒体库 / 容量）
//
// 上游 `MediaProviderBundle.prepare_library` 与 `StorageProvider.get_space_usage`。
// 这两个 rpc 早就定义在 `proto/storage.proto`（`:419` / `:400`），只是宿主侧一直
// 没有调用面 —— 组合根实现 `MediaLibraryRegistry` 才第一次要用它们。
// ══════════════════════════════════════════════════════════════════════

/// 让 provider 校验 / 归一化媒体库配置。上游 `bundle.prepare_library`。
///
/// `previous` 只在更新时给：secret / 只读字段的回填语义在**宿主服务层**
/// （`_prepare_config`），这里只负责过线。
pub async fn prepare_library(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    submitted_config: &serde_json::Value,
    previous: Option<LibraryHandle>,
) -> Result<sm_plugin_api::v1::PrepareLibraryResponse, ProviderOperationError> {
    client
        .prepare_library(sm_plugin_api::v1::PrepareLibraryRequest {
            submitted_config: sm_plugin_api::json_struct::json_to_struct(submitted_config),
            previous,
        })
        .await
        .map(|response| response.into_inner())
        .map_err(|status| classify_status(provider_key, "prepare_library", status))
}

/// 问 provider 要存储容量。上游 `storage.get_space_usage`。
///
/// 声明了 `CAPABILITY_SPACE_USAGE` 才该调；未实现的插件会回 `Unimplemented`，
/// 由调用方按「不支持」处理（`None`），不是 502。
pub async fn get_space_usage(
    client: &mut StorageProviderClient<Channel>,
    provider_key: &str,
    library: LibraryHandle,
) -> Result<sm_plugin_api::v1::StorageSpaceUsage, ProviderOperationError> {
    client
        .get_space_usage(sm_plugin_api::v1::GetSpaceUsageRequest {
            library: Some(library),
        })
        .await
        .map(|response| response.into_inner().usage.unwrap_or_default())
        .map_err(|status| classify_status(provider_key, "get_space_usage", status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(code: Code) -> ProviderOperationError {
        classify_status(
            "demo",
            "delete_media",
            Status::new(code, "插件内部细节 /path/to/x"),
        )
    }

    /// ★ 七个码的字符串与上游**逐字一致** —— 它们会原样透传给客户端。
    #[test]
    fn the_seven_codes_match_upstream_literals() {
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::InvalidConfig),
            "invalid_config"
        );
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::AuthenticationFailed),
            "authentication_failed"
        );
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::SourceNotFound),
            "source_not_found"
        );
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::TaskNotManaged),
            "task_not_managed"
        );
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::SourceBlacklisted),
            "source_blacklisted"
        );
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::Unsupported),
            "unsupported"
        );
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::Unavailable),
            "unavailable"
        );
        // 第八个**不在**上游那七个里：宿主没认出来。
        assert_eq!(
            provider_error_code_name(ProviderErrorCode::Unspecified),
            "unspecified"
        );
    }

    /// ★ 只把 `Unavailable` 当可重试 —— 缩略图任务的延迟轨靠它。
    ///
    /// 这是**猜测**那份（`provider` 没给 `retryable` 时）。契约仓里还有一份
    /// 同名判据，测试在那边；这里钉的是「宿主侧退回它」这条路径。
    #[test]
    fn only_unavailable_is_retryable_when_the_provider_said_nothing() {
        let guess = |code| {
            classify_status(
                "demo",
                "op",
                Status::new(
                    match code {
                        ProviderErrorCode::Unavailable => Code::Unavailable,
                        _ => Code::Unknown,
                    },
                    "x",
                ),
            )
            .retryable()
        };
        assert!(guess(ProviderErrorCode::Unavailable));
        for code in [
            ProviderErrorCode::InvalidConfig,
            ProviderErrorCode::AuthenticationFailed,
            ProviderErrorCode::SourceNotFound,
            ProviderErrorCode::TaskNotManaged,
            ProviderErrorCode::SourceBlacklisted,
            ProviderErrorCode::Unsupported,
            ProviderErrorCode::Unspecified,
        ] {
            assert!(!guess(code), "{code:?} 不该算可重试");
        }
    }

    /// ★ **结构化错误优先**：provider 说的值盖过按码猜的值。
    ///
    /// 上游的 `retryable` 是 provider 给的独立字段（不是从 `code` 推的）；
    /// 这条用例钉的是「它说了就算」—— 包括**说不该重试**这种反直觉的情况
    /// （`unavailable` + `retryable=false` 要照办，不能自作主张改成 true）。
    #[test]
    fn a_structured_error_beats_the_guess() {
        use sm_plugin_api::v1::ProviderError;

        for retryable in [true, false] {
            let status = sm_plugin_api::error::to_status(
                &ProviderError {
                    provider_key: "local".to_owned(),
                    operation: "delete_media".to_owned(),
                    code: ProviderErrorCode::Unavailable as i32,
                    safe_message: "本地源正忙".to_owned(),
                    retryable,
                },
                Code::Unavailable,
            );
            let error = classify_status("demo", "delete_media", status);
            assert_eq!(error.code(), "unavailable");
            assert_eq!(
                error.retryable(),
                retryable,
                "provider 说了 {retryable} 就该照办"
            );
            // 对外文案也换成插件给的那句（它有 `safe_message`）。
            assert_eq!(error.safe_message(), "本地源正忙");
        }
    }

    /// 结构化错误**没给** `safe_message` 时，仍用宿主自己的模板 ——
    /// 不能把空串当文案展示给用户。
    #[test]
    fn an_empty_safe_message_falls_back_to_the_host_template() {
        use sm_plugin_api::v1::ProviderError;

        let status = sm_plugin_api::error::to_status(
            &ProviderError {
                provider_key: "local".to_owned(),
                operation: "op".to_owned(),
                code: ProviderErrorCode::Unsupported as i32,
                safe_message: String::new(),
                retryable: false,
            },
            Code::Unimplemented,
        );
        let error = classify_status("demo", "op", status);
        assert_eq!(error.safe_message(), "媒体提供方不支持该操作");
    }

    /// `delete_media` 的 `source_not_found` 分支靠这条映射 —— 而它是**有损的**。
    #[test]
    fn not_found_maps_to_source_not_found() {
        let error = classify(Code::NotFound);
        assert_eq!(error.code(), "source_not_found");
        assert!(!error.retryable(), "远端不在，重试还是不在");
    }

    /// ★ 认不出的失败**不许**塞进七个之一。
    ///
    /// 把 `Internal` 映射成 `invalid_config` 会把「插件崩了」说成
    /// 「你的配置不对」—— 用户照着改配置只会更糊涂。
    #[test]
    fn unknown_failures_stay_unspecified() {
        for code in [Code::Internal, Code::Unknown, Code::DataLoss, Code::Aborted] {
            let error = classify(code);
            assert_eq!(error.code(), "unspecified", "{code:?} 不该被硬塞进上游七码");
            assert_eq!(error.safe_message(), "媒体提供方操作失败");
        }
    }

    /// ★ 对外的安全文案**不含插件的原文**。
    ///
    /// proto 的注释写明了 `safe_message` 不得包含 Cookie、密码或内部路径，
    /// 而 `Status.message()` 是插件自己拼的调试文本（本地 provider 就会拼路径）。
    #[test]
    fn the_plugin_text_never_reaches_the_safe_message() {
        let error = classify(Code::NotFound);
        assert!(
            !error.safe_message().contains("/path/to/x"),
            "插件原文只能进 plugin_detail"
        );
        assert!(error.plugin_detail().contains("/path/to/x"));
    }

    /// 几个常见映射各钉一条（改错了要知道是哪一条）。
    #[test]
    fn the_common_mappings_are_pinned() {
        assert_eq!(
            classify(Code::Unauthenticated).code(),
            "authentication_failed"
        );
        assert_eq!(
            classify(Code::PermissionDenied).code(),
            "authentication_failed"
        );
        assert_eq!(classify(Code::InvalidArgument).code(), "invalid_config");
        assert_eq!(classify(Code::Unimplemented).code(), "unsupported");
        assert_eq!(classify(Code::FailedPrecondition).code(), "unavailable");
        assert_eq!(classify(Code::DeadlineExceeded).code(), "unavailable");
        assert!(classify(Code::DeadlineExceeded).retryable());
    }
}
