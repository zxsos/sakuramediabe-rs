//! gRPC 参考插件（并行线 C）。
//!
//! 用一个「把本地目录包装成 `StorageProvider`」的真实插件，把 `proto/` 定义的
//! 插件 ABI 打穿一遍，回答三件事：
//!
//! 1. server streaming（`GenerateThumbnails` / `ScanImportSource`）能否跑通
//! 2. 同机回环的一元 RPC 往返开销量级，够不够支撑「插件拆进程」
//! 3. 现有 proto 够不够做一个真实插件 —— 缺口清单见
//!    `docs/parallel/grpc-plugin-report.md`
//!
//! 只实现最小集（[`provider::LocalRefProvider`]）：
//!
//! | rpc | 形态 |
//! |---|---|
//! | `Browse` | 一元 |
//! | `PlanPlayback` | 一元 |
//! | `GenerateThumbnails` | server streaming（`ProgressEvent`） |
//! | `ScanImportSource` | server streaming（`ImportFileEntry`） |
//!
//! 其余 28 个 rpc 一律返回 `Status::unimplemented`：tonic 0.14 生成的 trait
//! **没有默认方法体**，「只想实现 4 个方法」也必须写满 32 个（见报告 §4.1）。

pub mod fixture;
pub mod latency;
pub mod opaque;
pub mod provider;

use std::net::SocketAddr;

pub use crate::opaque::{ref_path, string_ref};
pub use crate::provider::LocalRefProvider;

use sm_plugin_api::v1::storage_provider_client::StorageProviderClient;
use sm_plugin_api::v1::storage_provider_server::StorageProviderServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Error, Server};

/// 起一个真实的 `StorageProvider` server。
///
/// 端口交给内核分配（`127.0.0.1:0`），避免和其他并行线的测试抢端口。
/// 返回的句柄只是「server 还活着」的凭证，测试里通常不需要 join：
/// 进程退出即结束，不影响任何断言。
pub async fn spawn(
    provider: LocalRefProvider,
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
