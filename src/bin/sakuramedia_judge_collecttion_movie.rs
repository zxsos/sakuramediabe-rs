//! 合集判定插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//! - `SAKURAMEDIA_PLUGIN_HOST_ADDR`：宿主 `PluginHost` 服务的地址，可选 ——
//!   宿主目前不注入它，缺省时 `run_job` 回 `unimplemented`。

use std::net::SocketAddr;
use std::sync::Arc;

use plugin_judge_collection::host::GrpcHostMovies;
use plugin_judge_collection::service::{Control, HOST_ADDR_ENV};
use plugin_judge_collection::settings::DurationCollectionSettings;
use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// 宿主分配的监听地址。
const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = std::env::var(ADDR_ENV)
        .map_err(|_| format!("缺少环境变量 {ADDR_ENV}"))?
        .parse()?;
    let plugin_id = std::env::var(ID_ENV).map_err(|_| format!("缺少环境变量 {ID_ENV}"))?;
    if plugin_id != plugin_judge_collection::PLUGIN_ID {
        eprintln!(
            "警告：宿主注入的 plugin_id（{plugin_id}）与本插件声明的（{}）不一致",
            plugin_judge_collection::PLUGIN_ID
        );
    }
    let settings = DurationCollectionSettings::load();

    // 宿主回调用：目前宿主不注入该变量，缺省就传 None（run_job 会回 unimplemented）。
    let host: Option<Arc<dyn plugin_judge_collection::host::HostMovies>> =
        match std::env::var(HOST_ADDR_ENV) {
            Ok(addr) if !addr.is_empty() => {
                let grpc = GrpcHostMovies::connect(&addr).await?;
                Some(Arc::new(grpc))
            }
            _ => None,
        };

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::new(
            plugin_id, settings, host,
        )))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
