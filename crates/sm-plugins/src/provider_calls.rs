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
//! # ⚠️ 尚未闭合的 ABI 缺口：**错误码过不了线**
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
//! 但 `proto/common.proto` 里那个 `ProviderError` 与 `ProviderErrorCode`
//! **没有任何 rpc 用它做错误通道**（`crates/plugin-ref-local/src/provider.rs:249`
//! 的注释就是这么写的：「失败只有一个布尔位 `unavailable`，区分不了『文件不存在』
//! 『无权限』『已被黑名单』，也带不了 ProviderErrorCode / retryable」）。
//! 也就是说宿主**只拿得到 `tonic::Status`**：一个 gRPC 状态码 + 一句人话。
//!
//! 后果是 `classify_status` 的映射
//! **必然有损**，最危险的一处是：
//!
//! > `Status::NotFound` 同时被「远端文件不在」与「媒体库不存在」用。
//! > 映射成 `source_not_found` 之后，`delete_media` 会当作「远端早已删掉」
//! > 而**继续清理本地记录** —— 那正是我们要的结果没错；但如果真实原因是
//! > 「库没配好」，我们就**以为远端删过了**，而实际上文件还在。
//!
//! 另外 `retryable` **完全拿不到**，只能按码猜
//! （见 `retryable_for`）。
//!
//! # 闭合它要动 proto（**这不是本模块能单独解决的**）
//!
//! 两条路，都不小：
//!
//! 1. 让 `ProviderError` 真的上错误通道 —— 用 `google.rpc.Status` 的
//!    `details` 塞一个 protobuf 编码的 `ProviderError`（tonic 侧要引
//!    `tonic-types`），**所有插件都要跟着改**；
//! 2. 每个响应消息加 `optional ProviderError error = n;` —— 更直白，但要改
//!    全部 28 个 rpc 的响应。
//!
//! 在那之前，本模块把有损映射**集中在一处**并标注每个分支的依据，让将来的迁移
//! 只需要改 `classify_status` 一个函数。

use sm_plugin_api::v1::storage_provider_client::StorageProviderClient;
use sm_plugin_api::v1::{
    generate_thumbnails_response, DeleteMediaRequest, GenerateThumbnailsRequest, LibraryHandle,
    MediaHandle, ProviderErrorCode, ThumbnailGeneration,
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
    /// 宿主生成的安全文案。**不含插件文本。**
    pub safe_message: String,
    /// 插件的原始文本。**只进日志。**
    pub plugin_detail: String,
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

    /// 是否值得重试。**宿主按码猜的**，见
    /// `retryable_for`。
    pub fn retryable(&self) -> bool {
        retryable_for(self.code)
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

/// 该码是否值得重试。
///
/// # 这是**猜的** —— 上游的 `retryable` 是 provider 给的独立字段
///
/// proto 过不了线（见模块文档），所以只能按语义近似：
///
/// | code | 猜 | 理由 |
/// |---|---|---|
/// | `unavailable` | **是** | 网络抖动、后端重启 —— 上游缩略图任务正是靠它走延迟轨 |
/// | `source_not_found` | 否 | 远端对象不在，重试还是不在（**除非**是挂载点还没就绪 —— 那种情况上游由 provider 显式给 `retryable=true`）|
/// | 其余 | 否 | 配置/认证/黑名单/不支持 —— 全是确定性失败 |
///
/// 猜错的代价是**不对称的**：把「该重试的」判成不该重试 → 差一次重试机会；
/// 反过来把确定性失败判成该重试 → 每轮都白烧一次 provider 调用（而缩略图任务的
/// 重试有次数上限，最终仍会进终态）。所以这里**偏保守**。
pub fn retryable_for(code: ProviderErrorCode) -> bool {
    matches!(code, ProviderErrorCode::Unavailable)
}

/// gRPC 状态 → 上游的 provider 错误码。**这是那个有损映射的唯一一处。**
///
/// 每个分支的依据都写在下面的 `match` 里；将来 proto 真的把 `ProviderError`
/// 送上错误通道之后，这个函数会瘦成「解析 `ProviderError` + 兜底」。
pub fn classify_status(
    provider_key: &str,
    operation: &str,
    status: Status,
) -> ProviderOperationError {
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

/// 缩略图生成的进度回调：`(文案, 当前, 总数)`。
///
/// 抽成别名既是为了 `clippy::type_complexity`，也是为了让调用点一眼看清三个参数
/// 的含义（裸写 `&mut dyn FnMut(&str, i32, i32)` 看不出谁是总数）。
pub type ThumbnailProgress = dyn FnMut(&str, i32, i32);

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
    #[test]
    fn only_unavailable_is_retryable() {
        assert!(retryable_for(ProviderErrorCode::Unavailable));
        for code in [
            ProviderErrorCode::InvalidConfig,
            ProviderErrorCode::AuthenticationFailed,
            ProviderErrorCode::SourceNotFound,
            ProviderErrorCode::TaskNotManaged,
            ProviderErrorCode::SourceBlacklisted,
            ProviderErrorCode::Unsupported,
            ProviderErrorCode::Unspecified,
        ] {
            assert!(!retryable_for(code), "{code:?} 不该算可重试");
        }
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
