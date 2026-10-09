//! JavDB 排行榜插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//!
//! # 为什么一个进程同时 serve 两个 service
//!
//! `PluginControl`（控制面）与 `RankingSourceExtensionService`（扩展点）没有
//! 第二个端口可去：proto 里只有 `data_plane_endpoint` 是插件回给宿主的另一个
//! 地址，而它留给字节搬运。

use std::net::SocketAddr;

use plugin_javdb_ranking::javdb::JavDbSource;
use plugin_javdb_ranking::service::{Control, Ranking};
use plugin_javdb_ranking::settings::Settings;
use sm_plugin_api::v1::plugin_control_server::PluginControlServer;
use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionServiceServer;
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
    let source = JavDbSource::new(&settings)?;

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::new(plugin_id)))
        .add_service(RankingSourceExtensionServiceServer::new(Ranking::new(
            source,
        )))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
