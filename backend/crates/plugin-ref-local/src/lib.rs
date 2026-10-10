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

pub mod control;
pub mod fixture;
pub mod latency;
pub mod opaque;
pub mod provider;

use std::net::SocketAddr;
use std::path::PathBuf;

pub use crate::control::{Control, DEFAULT_PROVIDER_KEY};
pub use crate::opaque::{ref_path, string_ref};
pub use crate::provider::LocalRefProvider;

use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
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

/// 把本插件的全部 gRPC service 起在 `addr` 上。
///
/// **进程式与进程内共用同一份装配**（与各业务插件的 `serve` 同形）：
///
/// - **进程式**：可执行文件（`src/bin/plugin-ref-local.rs`）从
///   `SAKURAMEDIA_PLUGIN_*` 环境变量取地址 / id / 配置文件，把命令行给的
///   本地目录塞进 `settings["root"]` 后调这里；
/// - **进程内**：组合根（`sm-server`）直接把 `settings` 传进来，在同一进程里
///   起一个 loopback 服务 —— 不起子进程，省掉一整套运行时。
///
/// 配置项：
///
/// - `provider_key`（缺省 `local_ref`）：数据面在 `register` 里声明的 provider 键；
/// - `root`（缺省系统临时目录下的 `plugin-ref-local`）：本地目录后端的根。
///
/// `host_endpoint` 本插件用不上：它是**被宿主拉过来问**的 provider 与一个没有
/// 后台任务的控制面，不反向回调宿主。留着这个参数只为与其它插件的 `serve` 同形。
pub async fn serve(
    addr: SocketAddr,
    plugin_id: String,
    settings: serde_json::Value,
    _host_endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let provider_key = settings
        .get("provider_key")
        .and_then(|value| value.as_str())
        .unwrap_or(DEFAULT_PROVIDER_KEY)
        .to_owned();
    let root = settings
        .get("root")
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("plugin-ref-local"));

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::with_provider_key(
            plugin_id,
            provider_key,
        )))
        .add_service(StorageProviderServer::new(LocalRefProvider::new(root)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
