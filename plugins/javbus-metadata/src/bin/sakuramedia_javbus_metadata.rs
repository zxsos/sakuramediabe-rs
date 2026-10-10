//! JavBus 元数据插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。端口由宿主
//!   选定并让出来，插件只管 bind —— 不做「插件自选端口再回报」。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//!
//! # 刻意不读 `SAKURAMEDIA_PLUGIN_DATA_DIR`
//!
//! 上游把图片写到 `<data_dir>/metadata-tmp/<uuid>/`，而 proto 要求图片落在
//! `FetchMovieRequest.delivery_dir` 里 —— 那是**每次请求**给的，不是插件自己
//! 的目录。所以本进程没有需要跨重启保留的东西，也就不碰 `data_dir`
//! （宿主照样会注入它；没用到就不读，免得把「用不上」写成「必填」）。
//!
//! # 为什么一个进程同时 serve 两个 service
//!
//! `PluginControl`（控制面）与 `MetadataSourceExtensionService`（扩展点）没有
//! 第二个端口可去：proto 里只有 `data_plane_endpoint` 是插件回给宿主的另一个
//! 地址，而它留给字节搬运。

use std::net::SocketAddr;

use plugin_javbus_metadata::javbus::JavBusSource;
use plugin_javbus_metadata::service::{Control, Metadata};
use plugin_javbus_metadata::settings::Settings;
use sm_plugin_api::v1::metadata_source_extension_service_server::MetadataSourceExtensionServiceServer;
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
    // 配置是可选的：没有这个变量就按默认跑（`settings.rs` 里那条理由）。
    // 宿主每次拉起都重写它，所以只在启动时读一次。
    let settings = Settings::load();
    let source = JavBusSource::new(&settings)?;

    // 宿主已经把这个端口让出来了；bind 不上说明端口被别人抢了，直接失败退出，
    // 宿主的探活会把它判成「起不来」。
    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::new(plugin_id)))
        .add_service(MetadataSourceExtensionServiceServer::new(Metadata::new(
            source,
        )))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
