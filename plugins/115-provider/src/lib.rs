//! 115 网盘 StorageProvider gRPC 插件。
//!
//! 对上游 Python 插件 `sakuramedia_115_provider` 的 Rust 重写，
//! 走同一套 gRPC 插件 ABI（`sm-plugin-api` v0.2.0）。
//!
//! 实现的 rpc：
//!
//! | rpc | 形态 |
//! |---|---|
//! | `Browse` | 一元（按 cid 分页） |
//! | `ScanImportSource` | server streaming（递归枚举） |
//! | `PlanPlayback` | 一元（直链 redirect） |
//! | `GenerateThumbnails` | 暂不支持（`unimplemented`） |
//! | `GetSpaceUsage` | 一元 |
//! | `PrepareLibrary` | 一元（校验 Cookie + 解析目录） |
//! | `RunJob` | server streaming（手动清理任务，见 [`cleanup`]） |
//!
//! 其余 rpc 由 [`sm_plugin_api::provider::StorageProviderExt`] 的默认实现
//! 提供（返回 `Status::unimplemented`）。
//!
//! 认证：115 Cookie（`web_cookie` / `device_cookie`），来源见
//! [`config::Plugin115Config`] —— 本 crate 不硬编码任何密钥。

pub mod cleanup;
pub mod client;
pub mod config;
pub mod opaque;
pub mod provider;

use std::net::SocketAddr;

pub use crate::config::Plugin115Config;
pub use crate::opaque::{dir_ref, file_ref, ref_cid, ref_pickcode, ROOT_CID};
pub use crate::provider::Provider115;

use sm_plugin_api::v1::storage_provider_client::StorageProviderClient;
use sm_plugin_api::v1::storage_provider_server::StorageProviderServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Error, Server};

/// 起一个真实的 `StorageProvider` server（测试用）。
pub async fn spawn(
    provider: Provider115,
) -> Result<(SocketAddr, tokio::task::JoinHandle<Result<(), Error>>), tokio::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        Server::builder()
            .add_service(StorageProviderServer::new(provider))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    Ok((addr, handle))
}

/// 连到 [`spawn`] 起好的 server。
pub async fn connect(addr: SocketAddr) -> Result<StorageProviderClient<Channel>, Error> {
    StorageProviderClient::connect(format!("http://{addr}")).await
}
