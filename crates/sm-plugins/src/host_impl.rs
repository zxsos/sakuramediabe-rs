//! `sm_plugin_api::host` trait 的 gRPC 实现。
//!
//! # 在依赖图里的位置
//!
//! ```text
//! sm-service ──(trait)──▶ sm-plugin-api::host
//!      ▲                        ▲
//!      │                        │ 实现
//! sm-server ──(注入)──▶ sm-plugins::host_impl
//! ```
//!
//! `sm-service` 只认 trait（`sm-plugin-api` 是叶子 crate，不成环）；
//! 真正的 gRPC 调用在这里；`sm-server` 在启动时把
//! `RegistryProviderFactory` 塞给需要的 service。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sm_plugin_api::host::{HostProviderError, HostProviderFactory, HostStorageProvider};
use sm_plugin_api::v1::{
    BrowsePage, ImportPlacement, LibraryHandle, MediaHandle, StagedMediaTransfer,
    TransferSourceSession,
};
use tonic::transport::Channel;

use crate::provider_calls::{self, ProviderOperationError};
use crate::registry::ProviderRegistry;
use sm_plugin_api::v1::storage_provider_client::StorageProviderClient;

/// 把 `ProviderOperationError` 转成契约层的 [`HostProviderError`]。
///
/// 字段一一对应：`code` 用 [`ProviderOperationError::code`] 的字符串
/// （与上游逐字一致），`retryable` 用 [`ProviderOperationError::retryable`]。
fn to_host_error(err: ProviderOperationError) -> HostProviderError {
    // 先借后移：`code()` 与 `retryable()` 都借 `&self`，必须在 move 字段前算好。
    let code = err.code().to_owned();
    let retryable = err.retryable();
    HostProviderError {
        provider_key: err.provider_key,
        operation: err.operation,
        code,
        safe_message: err.safe_message,
        retryable,
    }
}

/// gRPC 实现的存储 provider。
///
/// 包着一个已连好的 `StorageProviderClient` 与它的 `provider_key`
/// （报错时填 `HostProviderError.provider_key`）。
pub struct GrpcStorageProvider {
    client: tokio::sync::Mutex<StorageProviderClient<Channel>>,
    provider_key: String,
}

impl GrpcStorageProvider {
    /// 从已连好的 client 构造。
    pub fn new(client: StorageProviderClient<Channel>, provider_key: String) -> Self {
        Self {
            client: tokio::sync::Mutex::new(client),
            provider_key,
        }
    }
}

#[async_trait]
impl HostStorageProvider for GrpcStorageProvider {
    async fn browse(
        &self,
        library: LibraryHandle,
        parent_ref: Option<serde_json::Value>,
        cursor: Option<String>,
        limit: i32,
    ) -> Result<BrowsePage, HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::browse(
            &mut client,
            &self.provider_key,
            library,
            parent_ref.as_ref(),
            cursor.as_deref(),
            limit,
        )
        .await
        .map_err(to_host_error)
    }

    async fn open_transfer_source(
        &self,
        library: LibraryHandle,
        media: MediaHandle,
    ) -> Result<TransferSourceSession, HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::open_transfer_source(&mut client, &self.provider_key, library, media)
            .await
            .map_err(to_host_error)
    }

    async fn assert_transfer_source_unchanged(
        &self,
        session_id: String,
    ) -> Result<bool, HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::assert_transfer_source_unchanged(
            &mut client,
            &self.provider_key,
            &session_id,
        )
        .await
        .map_err(to_host_error)
    }

    async fn close_transfer_source(&self, session_id: String) -> Result<(), HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::close_transfer_source(&mut client, &self.provider_key, &session_id)
            .await
            .map_err(to_host_error)
    }

    async fn cleanup_transfer_source(
        &self,
        library: LibraryHandle,
        media: MediaHandle,
        session_id: String,
    ) -> Result<(), HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::cleanup_transfer_source(
            &mut client,
            &self.provider_key,
            library,
            media,
            &session_id,
        )
        .await
        .map_err(to_host_error)
    }

    async fn stage_transfer(
        &self,
        library: LibraryHandle,
        source: TransferSourceSession,
        placement: ImportPlacement,
        operation_key: String,
    ) -> Result<StagedMediaTransfer, HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::stage_transfer(
            &mut client,
            &self.provider_key,
            library,
            source,
            placement,
            &operation_key,
        )
        .await
        .map_err(to_host_error)
    }

    async fn finalize_transfer(
        &self,
        library: LibraryHandle,
        receipt: Option<prost_types::Struct>,
    ) -> Result<(), HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::finalize_transfer(&mut client, &self.provider_key, library, receipt)
            .await
            .map_err(to_host_error)
    }

    async fn abort_transfer(
        &self,
        library: LibraryHandle,
        receipt: Option<prost_types::Struct>,
    ) -> Result<(), HostProviderError> {
        let mut client = self.client.lock().await;
        provider_calls::abort_transfer(&mut client, &self.provider_key, library, receipt)
            .await
            .map_err(to_host_error)
    }
}

/// 从注册表查端点、建 client 的工厂。
///
/// 包着 `Arc<Mutex<ProviderRegistry>>` —— 与 `sm-server` 持有的类型一致，
/// 插件重启换端点时这里能看到最新的。
pub struct RegistryProviderFactory {
    registry: Arc<Mutex<ProviderRegistry>>,
}

impl RegistryProviderFactory {
    /// 构造。`registry` 通常来自 `sm-server` 的插件子系统。
    pub fn new(registry: Arc<Mutex<ProviderRegistry>>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl HostProviderFactory for RegistryProviderFactory {
    async fn for_provider_key(
        &self,
        provider_key: &str,
    ) -> Result<Arc<dyn HostStorageProvider>, HostProviderError> {
        // 1. 查注册表拿端点。
        let endpoint = {
            let registry = self.registry.lock().expect("注册表锁中毒");
            match registry.get(provider_key) {
                Some(entry) => entry.plugin_endpoint.clone(),
                None => {
                    return Err(HostProviderError::not_installed(provider_key, "browse"));
                }
            }
        };
        // 2. 建 gRPC client。
        let client = provider_calls::connect_storage(provider_key, &endpoint, "browse")
            .await
            .map_err(to_host_error)?;
        // 3. 包成 trait 对象。
        Ok(Arc::new(GrpcStorageProvider::new(
            client,
            provider_key.to_owned(),
        )))
    }

    fn has_capability(&self, provider_key: &str, capability: i32) -> Option<bool> {
        let registry = self.registry.lock().expect("注册表锁中毒");
        registry
            .get(provider_key)
            .map(|entry| entry.has(capability))
    }
}
