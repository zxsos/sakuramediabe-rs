//! 宿主侧的 provider 调用抽象（host-side）。
//!
//! # 为什么需要这一层
//!
//! `sm-service` 刻意不依赖 `sm-plugins`（防循环依赖：
//! `sm-plugins → sm-scheduler → sm-service → sm-plugins`）。
//! 但 service 层又需要调插件的 gRPC 方法（如 `browse`）。
//!
//! 解法是**依赖倒置**：
//!
//! - 契约（本文件）定义在 `sm-plugin-api` —— 它是叶子 crate，不依赖本仓任何 crate；
//! - `sm-plugins` 实现这些 trait（用它的 gRPC client）；
//! - `sm-service` 只认 trait，不认实现；
//! - `sm-server`（组合根）在启动时把实现注入给 service。
//!
//! # 与 `provider.rs` 的分工
//!
//! [`crate::provider`] 是**插件侧**的默认实现（插件作者继承它）；
//! 本模块是**宿主侧**的调用抽象（宿主调插件时用）。
//! 两者都是「契约」，只是方向相反。

use std::sync::Arc;

use async_trait::async_trait;

use crate::v1::{
    BrowsePage, ImportPlacement, LibraryHandle, MediaHandle, StagedMediaTransfer,
    TransferSourceSession,
};

/// 插件能力取值（`proto/plugin.proto` 的 `Capability` 枚举）。
///
/// 用 `i32` 而不用生成的枚举：注册表存的就是 `Vec<i32>`
/// （`sm-plugins::registry::ProviderRegistration::capabilities`），
/// 且 `sm-plugins::registration::capability` 是同一组值的本地镜像 ——
/// 契约层只认线上的数字。
pub mod capability {
    /// 源端：`OpenTransferSource` 等。上游 `supports_media_transfer_source`。
    pub const TRANSFER_SOURCE: i32 = 30;
    /// 源端清理：`CleanupTransferSource`（可选，不支持则转存后保留源）。
    /// 上游 `supports_media_transfer_source_cleanup`。
    pub const TRANSFER_SOURCE_CLEANUP: i32 = 31;
    /// 目标端：`StageTransfer` / `FinalizeTransfer` / `AbortTransfer`。
    /// 上游 `supports_media_transfer_target`。
    pub const TRANSFER_TARGET: i32 = 32;
}

/// 宿主调 provider 失败。
///
/// 这是 `sm-plugins` 的 `ProviderOperationError` 在契约层的**精简版** ——
/// 只保留 service 层做错误映射需要的字段。完整版（含 `plugin_detail` 日志
/// 文本）在 `sm-plugins` 那边，转换时按需取用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProviderError {
    /// 哪个 provider（如 `"local"`）。
    pub provider_key: String,
    /// 哪个操作（如 `"browse"`）。
    pub operation: String,
    /// 与上游 `ProviderOperationError.code` 逐字一致的字符串
    /// （`invalid_config` / `authentication_failed` / `source_not_found` /
    /// `task_not_managed` / `source_blacklisted` / `unsupported` /
    /// `unavailable` / `unspecified`）。
    pub code: String,
    /// 对外展示的安全文案（可直接给客户端）。
    pub safe_message: String,
    /// 是否值得重试（provider 显式给的；没给时按码猜）。
    pub retryable: bool,
}

impl HostProviderError {
    /// 插件没装 / 连不上（宿主侧的问题，不是 provider 的失败）。
    ///
    /// 调用方据此报 503 `provider_not_installed`。
    pub fn not_installed(provider_key: &str, operation: &str) -> Self {
        Self {
            provider_key: provider_key.to_owned(),
            operation: operation.to_owned(),
            code: "unavailable".to_owned(),
            safe_message: "媒体提供方未安装".to_owned(),
            retryable: false,
        }
    }

    /// 判断是不是「插件没装 / 连不上」。
    ///
    /// 实现方在查不到注册表条目时用 [`Self::not_installed`] 构造；
    /// 连不上端点时 `connect_storage` 给的是 `unavailable` +
    /// "媒体提供方暂不可用"。调用方把这两种都按 503 处理
    /// （`provider_not_installed`），与「provider 连上了但操作失败」
    /// （502 一类）区分开。
    pub fn is_not_installed(&self) -> bool {
        self.code == "unavailable"
    }
}

impl std::fmt::Display for HostProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider {} 操作 {} 失败 [{}]: {}",
            self.provider_key, self.operation, self.code, self.safe_message
        )
    }
}

impl std::error::Error for HostProviderError {}

/// 宿主侧的存储 provider 抽象。
///
/// `sm-plugins` 用 gRPC client 实现它；`sm-service` 只认这个 trait。
///
/// # 目前只有 `browse`
///
/// 按「先接线一个、跑通再补」的节奏：`provider_browse` 是第一个调用方。
/// 后续的 `delete_media` / `scan_import_source` 等按同样的形状往下加。
#[async_trait]
pub trait HostStorageProvider: Send + Sync {
    /// 浏览 provider 的命名空间（单页，`next_cursor` 透传）。
    ///
    /// 对应 proto `StorageProvider.Browse`。
    async fn browse(
        &self,
        library: LibraryHandle,
        parent_ref: Option<serde_json::Value>,
        cursor: Option<String>,
        limit: i32,
    ) -> Result<BrowsePage, HostProviderError>;

    /// 打开转存源会话。对应 `StorageProvider.OpenTransferSource`。
    ///
    /// 返回的 [`TransferSourceSession`] 是不透明的：宿主只保存与回传，
    /// 不解释里面的 `info`（校验用 `info.file_name` / `info.size_bytes`）。
    async fn open_transfer_source(
        &self,
        library: LibraryHandle,
        media: MediaHandle,
    ) -> Result<TransferSourceSession, HostProviderError>;

    /// 断言源在会话期间未变化。对应 `AssertTransferSourceUnchanged`。
    ///
    /// 返回 `false` = 源已变化，宿主必须中止转存（上游 `source.assert_unchanged()`）。
    async fn assert_transfer_source_unchanged(
        &self,
        session_id: String,
    ) -> Result<bool, HostProviderError>;

    /// 关闭源会话。对应 `CloseTransferSource`。
    ///
    /// 宿主保证**一定调用**（上游 `open_transfer_source` 上下文管理器的退出语义）。
    async fn close_transfer_source(&self, session_id: String)
        -> Result<(), HostProviderError>;

    /// 清理源（只删当前会话对应且未变化的源文件）。对应 `CleanupTransferSource`。
    ///
    /// 能力可选（[`capability::TRANSFER_SOURCE_CLEANUP`]）：不支持则转存后保留源。
    async fn cleanup_transfer_source(
        &self,
        library: LibraryHandle,
        media: MediaHandle,
        session_id: String,
    ) -> Result<(), HostProviderError>;

    /// 暂存转存。对应 `StorageProvider.StageTransfer`。
    ///
    /// `operation_key` 按操作键幂等（上游 `f"task:{task_id}:{index + 1}"`）。
    async fn stage_transfer(
        &self,
        library: LibraryHandle,
        source: TransferSourceSession,
        placement: ImportPlacement,
        operation_key: String,
    ) -> Result<StagedMediaTransfer, HostProviderError>;

    /// 提交转存。对应 `FinalizeTransfer`。
    ///
    /// `receipt` 是 `stage_transfer` 给的凭据（不透明 `Struct`），宿主只回传。
    async fn finalize_transfer(
        &self,
        library: LibraryHandle,
        receipt: Option<prost_types::Struct>,
    ) -> Result<(), HostProviderError>;

    /// 回滚转存。对应 `AbortTransfer`。
    ///
    /// 只在「已暂存但未切换」时调用（上游：`switch_attempted` 为假且
    /// `staged.status == "staged"`）。切换已提交后**绝不**调用。
    async fn abort_transfer(
        &self,
        library: LibraryHandle,
        receipt: Option<prost_types::Struct>,
    ) -> Result<(), HostProviderError>;
}

/// 按 `provider_key` 找 provider 的工厂。
///
/// 实现方（如 `sm-plugins` 的 `RegistryProviderFactory`）从注册表查端点、
// 建 gRPC client、包成 [`HostStorageProvider`]。
///
/// # 返回 `Arc`
///
/// 同一个 provider 可能被多个 service 同时用，`Arc` 让它们共享同一个 client
/// （tonic 的 `Channel` 本来就是 `Clone` 且便宜的）。
#[async_trait]
pub trait HostProviderFactory: Send + Sync {
    /// 取 provider。查不到注册表条目或连不上时返回 `Err`（且
    /// [`HostProviderError::is_not_installed`] 为真），调用方据此报 503。
    async fn for_provider_key(
        &self,
        provider_key: &str,
    ) -> Result<Arc<dyn HostStorageProvider>, HostProviderError>;

    /// 插件是否声明了某能力（只查注册表，不建连接）。
    ///
    /// 对应上游 `supports_media_transfer_*` 那组**本地**鸭子类型检查 ——
    /// 在 gRPC 世界里「有没有这个方法」=「注册时有没有声明这个 capability」。
    ///
    /// 返回 `None` = 注册表里没有这个 provider（调用方报 503）；
    /// `Some(false)` = 有但没声明该能力（调用方报 422）。
    fn has_capability(&self, provider_key: &str, capability: i32) -> Option<bool>;
}
