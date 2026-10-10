//! 女优资料补全插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。端口由宿主
//!   选定并让出来，插件只管 bind —— 不做「插件自选端口再回报」。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//! - `SAKURAMEDIA_HOST_GRPC_ADDR`：宿主能力出口（`PluginHost`），可选 ——
//!   任务执行时才需要，注册阶段不用。
//!
//! # 为什么一个进程只 serve 控制面
//!
//! 本插件没有扩展点（纯后台任务），所以只 serve `PluginControl`。

use std::net::SocketAddr;

use plugin_actor_metadata::service::Control;
use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// 宿主分配的监听地址。
const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = std::env::var(ADDR_ENV)
        .map_err(|_| format!("缺少环境变量 {ADDR_ENV}"))?
        .parse()?;
    let plugin_id = std::env::var(ID_ENV).map_err(|_| format!("缺少环境变量 {ID_ENV}"))?;

    // 宿主已经把这个端口让出来了；bind 不上说明端口被别人抢了，直接失败退出，
    // 宿主的探活会把它判成「起不来」。
    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::new(plugin_id)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
