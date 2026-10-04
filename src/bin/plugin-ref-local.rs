//! 参考插件的**可执行文件**：宿主按生命周期协议拉起的那个进程。
//!
//! # 协议（`docs/adr/2026-10-05-plugin-lifecycle.md`）
//!
//! - `SAKURAMEDIA_PLUGIN_GRPC_ADDR`：宿主分配的监听地址，**必填**。端口由宿主
//!   选定并让出来，插件只管 bind —— 不做「插件自选端口再回报」。
//! - `SAKURAMEDIA_PLUGIN_ID`：宿主注入的插件 id，`Register` 原样回显，**必填**。
//! - `argv[1]`：本地目录后端的数据根。这是**本插件自己的命令行**，不属于协议 ——
//!   协议只管「地址与 id 怎么传」，其余参数各家自定。
//!
//! # 为什么一个进程同时 serve 两个 service
//!
//! `PluginControl`（控制面）与 `StorageProvider`（数据面）没有第二个端口可去：
//! proto 里只有 `data_plane_endpoint` 是插件回给宿主的另一个地址，而它留给字节
//! 搬运。所以控制面与 provider 走同一个监听地址。

use std::net::SocketAddr;
use std::path::PathBuf;

use plugin_ref_local::LocalRefProvider;
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::plugin_control_server::{PluginControl, PluginControlServer};
use sm_plugin_api::v1::storage_provider_server::StorageProviderServer;
use sm_plugin_api::v1::{
    Capability, Extension, JobEvent, MediaProviderBundle, RegisterRequest, RegisterResponse,
    RunJobRequest,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

/// 宿主分配的监听地址。
const ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_GRPC_ADDR";
/// 宿主注入的插件 id。
const ID_ENV: &str = "SAKURAMEDIA_PLUGIN_ID";
/// 宿主给的数据目录。**必填**：proto 承诺它可读写且重装时保留。
const DATA_DIR_ENV: &str = "SAKURAMEDIA_PLUGIN_DATA_DIR";
/// 宿主写好的配置文件。**可选**：没配置时宿主不给这个变量。
const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 配置里的 `provider_key`，缺省这个值。
const DEFAULT_PROVIDER_KEY: &str = "local_ref";

/// 读宿主写好的配置。**只取用得到的字段**，其余忽略（未知的键不该让插件起不来）。
fn configured_provider_key() -> String {
    let Some(path) = std::env::var(SETTINGS_FILE_ENV).ok() else {
        return DEFAULT_PROVIDER_KEY.to_owned();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return DEFAULT_PROVIDER_KEY.to_owned();
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else {
        return DEFAULT_PROVIDER_KEY.to_owned();
    };
    settings
        .get("provider_key")
        .and_then(|value| value.as_str())
        .unwrap_or(DEFAULT_PROVIDER_KEY)
        .to_owned()
}

/// 控制面。参考插件不提供后台任务，所以 `RunJob` 一律 `unimplemented` ——
/// 与 provider 侧那 28 个方法同一个处理（见 `sm_plugin_api::provider` 的模块
/// 文档：生成的 trait 没有默认方法体，只能一个个写）。
struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
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
        // 宿主注入什么就回什么：不一致会让宿主的加载校验直接失败，
        // 而这个进程自己也说不清「我是谁」，不如当场拒。
        if injected != self.plugin_id {
            return Err(Status::invalid_argument(format!(
                "宿主注入的 plugin_id 与启动参数不一致：注入 {injected}，本进程 {}",
                self.plugin_id
            )));
        }
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: "参考插件（本地目录）".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            // 声明下载能力：provider 侧实现了 `DownloadProvider` 的 submit。
            capabilities: vec![Capability::Download as i32],
            extensions: vec![Extension {
                key: "media.provider".to_owned(),
                data: Some(Data::MediaProvider(MediaProviderBundle {
                    provider_key: configured_provider_key(),
                    display_name: "本地目录".to_owned(),
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
    let plugin_id = std::env::var(ID_ENV).map_err(|_| format!("缺少环境变量 {ID_ENV}"))?;
    // 数据目录是宿主的承诺（proto 的原话）；宿主没给就起不来，而不是自己找地方。
    let data_dir =
        std::env::var(DATA_DIR_ENV).map_err(|_| format!("缺少环境变量 {DATA_DIR_ENV}"))?;
    std::fs::create_dir_all(&data_dir)?;
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("plugin-ref-local"));

    // 宿主已经把这个端口让出来了；bind 不上说明端口被别人抢了，直接失败退出，
    // 宿主的探活会把它判成「起不来」。
    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control { plugin_id }))
        .add_service(StorageProviderServer::new(LocalRefProvider::new(root)))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
