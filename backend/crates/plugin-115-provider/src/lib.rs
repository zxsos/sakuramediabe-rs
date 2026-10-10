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
pub mod control;
pub mod opaque;
pub mod provider;

use std::net::SocketAddr;

pub use crate::config::Plugin115Config;
pub use crate::control::{Control, DEFAULT_PROVIDER_KEY, DISPLAY_NAME, PLUGIN_ID};
pub use crate::opaque::{dir_ref, file_ref, ref_cid, ref_pickcode, ROOT_CID};
pub use crate::provider::Provider115;

use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
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

/// 把本插件的全部 gRPC service 起在 `addr` 上。
///
/// **进程式与进程内共用同一份装配**：
///
/// - **进程式**：可执行文件从 `SAKURAMEDIA_PLUGIN_*` 环境变量取地址 / id /
///   配置文件，解析成 `settings` 后调这里；
/// - **进程内**：组合根（`sm-server`）直接把 `settings` 传进来，在**同一个
///   进程**里起一个 loopback 服务 —— 不起子进程。
///
/// 与其它插件不同的是：本插件**同时** serve 控制面（`PluginControl`：只
/// `register`，声明 `media.provider`）与数据面（`StorageProvider`），走同一个
/// 监听地址（proto 里只有 `data_plane_endpoint` 是另一个地址，而它留给字节搬运）。
///
/// `settings` 是 `plugins.<id>.settings` 这个 `Value`，按
/// [`Plugin115Config`] 解析（缺键回落缺省）。
///
/// `host_endpoint` 本插件用不上：它是**被宿主拉过来问**的 provider，不反向
/// 回调宿主。留着这个参数只为与其它插件的 `serve` 同形。
pub async fn serve(
    addr: SocketAddr,
    plugin_id: String,
    settings: serde_json::Value,
    _host_endpoint: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config: Plugin115Config = serde_json::from_value(settings).unwrap_or_default();
    let provider = Provider115::new(config.clone());

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::with_settings(
            plugin_id, config,
        )))
        .add_service(StorageProviderServer::new(provider))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
