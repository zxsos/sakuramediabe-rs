//! 更多影片插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//! - `SAKURAMEDIA_HOST_GRPC_ADDR`：宿主 `PluginHost` 服务地址，任务执行时必填。

use std::net::SocketAddr;

use plugin_more_movies::service::MoreMoviesControl;
use plugin_more_movies::settings::MoreMoviesSettings;
use plugin_more_movies::MORE_MOVIES_PLUGIN_ID;
use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = std::env::var(ADDR_ENV)
        .map_err(|_| format!("缺少环境变量 {ADDR_ENV}"))?
        .parse()?;
    let plugin_id = std::env::var(ID_ENV).map_err(|_| format!("缺少环境变量 {ID_ENV}"))?;
    if plugin_id != MORE_MOVIES_PLUGIN_ID {
        return Err(
            format!("plugin_id 不匹配：注入 {plugin_id}，本进程 {MORE_MOVIES_PLUGIN_ID}").into(),
        );
    }
    let settings = MoreMoviesSettings::load();
    let control = MoreMoviesControl::new(plugin_id, settings)?;

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(control))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
