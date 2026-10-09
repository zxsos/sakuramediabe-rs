//! 影片文案抓取与翻译插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//! - `SAKURAMEDIA_PLUGIN_DATA_DIR`：宿主分配的数据目录；SQLite 状态文件
//!   （`dmm_state.sqlite3`）与任务锁落在这里。

use std::net::SocketAddr;

use plugin_scrape_translate::service::Control;
use plugin_scrape_translate::settings::Settings;
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
    let settings = Settings::load();

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::new(plugin_id, settings)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
