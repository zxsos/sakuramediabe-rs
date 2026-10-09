//! SubtitleCat 中文字幕插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，可选。
//!
//! # 刻意不读 `SAKURAMEDIA_PLUGIN_DATA_DIR`
//!
//! 上游用 SQLite 记「已抓取」状态（`state.py`），Rust 侧暂未移植（见
//! `lib.rs` 的偏离说明），所以本进程没有需要跨重启保留的东西，也就不碰
//! `data_dir`。

use std::net::SocketAddr;

use plugin_subtitlecat::service::Control;
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

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control::new(plugin_id)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
