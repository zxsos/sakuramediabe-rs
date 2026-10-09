//! 115 插件的可执行文件：宿主按生命周期协议拉起的进程。
//!
//! # 协议
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `SAKURAMEDIA_PLUGIN_DATA_DIR`：宿主给的数据目录，**必填**。
//! - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`：宿主写好的配置文件，**可选**。
//!
//! 115 的 Cookie 等配置走 `LibraryHandle.provider_config`（每次请求），
//! 或环境变量 `PLUGIN_115_WEB_COOKIE` / `PLUGIN_115_DEVICE_COOKIE`，
//! 或宿主配置文件 —— 优先级见 `config::Plugin115Config`。

use std::net::SocketAddr;

use plugin_115::{Plugin115Config, Provider115};
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::plugin_control_server::{PluginControl, PluginControlServer};
use sm_plugin_api::v1::storage_provider_server::StorageProviderServer;
use sm_plugin_api::v1::{
    Extension, JobEvent, MediaProviderBundle, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

/// 宿主分配的监听地址。
const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";
/// 宿主给的数据目录。
const DATA_DIR_ENV: &str = "SAKURAMEDIA_PLUGIN_DATA_DIR";

/// 插件 id（与上游 Python 插件一致）。
const PLUGIN_ID: &str = "sakuramedia_115_provider";

struct Control {
    plugin_id: String,
}

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = futures::stream::BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        _request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        Err(Status::unimplemented("PluginControl::run_job 未实现"))
    }

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let injected = request.into_inner().plugin_id;
        if injected != self.plugin_id {
            return Err(Status::invalid_argument(format!(
                "宿主注入的 plugin_id 与启动参数不一致：注入 {injected}，本进程 {}",
                self.plugin_id
            )));
        }
        let config = Plugin115Config::from_env();
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: "115 网盘".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            // 本插件 serve StorageProvider（见 src/lib.rs），不虚报额外能力。
            capabilities: Vec::new(),
            extensions: vec![Extension {
                key: "media.provider".to_owned(),
                data: Some(Data::MediaProvider(MediaProviderBundle {
                    provider_key: config.provider_key_or_default().to_owned(),
                    display_name: "115 网盘".to_owned(),
                    ..Default::default()
                })),
            }],
            ..Default::default()
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = std::env::var(ADDR_ENV)
        .map_err(|_| format!("缺少环境变量 {ADDR_ENV}"))?
        .parse()?;
    let plugin_id = std::env::var(ID_ENV).unwrap_or_else(|_| PLUGIN_ID.to_owned());
    let data_dir =
        std::env::var(DATA_DIR_ENV).map_err(|_| format!("缺少环境变量 {DATA_DIR_ENV}"))?;
    std::fs::create_dir_all(&data_dir)?;

    let config = Plugin115Config::from_env();
    let provider = Provider115::new(config);

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control { plugin_id }))
        .add_service(StorageProviderServer::new(provider))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
