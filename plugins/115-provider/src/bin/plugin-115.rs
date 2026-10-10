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
//!
//! 控制面除 `Register` 外还实现了 `RunJob` 的手动清理任务
//! （[`plugin_115::cleanup`]，删除媒体根目录下的空子目录；破坏性确认位
//! 必填，见 `cleanup::extract_confirm_params`）。

use std::net::SocketAddr;

use futures::StreamExt;
use plugin_115::{Plugin115Config, Provider115};
use sm_plugin_api::v1::config_field::Input;
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::plugin_control_server::{PluginControl, PluginControlServer};
use sm_plugin_api::v1::storage_provider_server::StorageProviderServer;
use sm_plugin_api::v1::{
    Capability, ConfigField, Extension, JobDefinition, JobEvent, MediaProviderBundle,
    PlaybackDelivery, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
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

/// 上游 `manifest.json` 的 `display_name`。
const DISPLAY_NAME: &str = "115 网盘";

struct Control {
    plugin_id: String,
    /// 运行设置（Cookie、媒体根目录）。清理任务从这里读 —— 数据面按请求带
    /// `LibraryHandle`，任务面只有 settings。
    config: Plugin115Config,
}

/// 建库配置字段（`PrepareLibrary` / provider 私有配置的契约）。
///
/// 与 [`Plugin115Config`] 读取的键一一对应：Cookie 两个（secret），
/// 媒体 / 下载目录两个（path）。
fn library_config_fields() -> Vec<ConfigField> {
    vec![
        field(
            "web_cookie",
            "网页端 Cookie",
            Input::Secret,
            false,
            "从 115 网页端复制的 Cookie（与设备 Cookie 至少填一个）",
        ),
        field(
            "device_cookie",
            "设备 Cookie",
            Input::Secret,
            false,
            "115 小程序的设备 Cookie（与网页端 Cookie 至少填一个）",
        ),
        field(
            "media_root_path",
            "媒体根目录",
            Input::Path,
            true,
            "导入媒体的目标目录（115 绝对路径，如 /媒体/电影）",
        ),
        field(
            "downloads_root_path",
            "离线下载目录",
            Input::Path,
            false,
            "离线任务保存目录（115 绝对路径）",
        ),
    ]
}

fn field(key: &str, label: &str, input: Input, required: bool, description: &str) -> ConfigField {
    ConfigField {
        key: key.to_owned(),
        label: label.to_owned(),
        input: input as i32,
        required,
        description: Some(description.to_owned()),
        multiline: false,
        read_only: false,
        hint: None,
    }
}

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = futures::stream::BoxStream<'static, Result<JobEvent, Status>>;

    /// 手动清理任务（见 [`plugin_115::cleanup`]）。流式：扫描与删除的进度
    /// 逐帧发，终态摘要作为 `result` 帧收尾。
    ///
    /// 未知任务回 `invalid_argument`（不是 `not_found`）：让调用方看出
    /// 「任务键写错了」，而不是「任务存在但找不到」。
    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let request = request.into_inner();
        match request.task_key.as_str() {
            plugin_115::cleanup::CLEANUP_EMPTY_DIRS_TASK => {
                plugin_115::cleanup::extract_confirm_params(&request)?;
                let (tx, rx) = mpsc::channel(16);
                let config = self.config.clone();
                tokio::spawn(async move {
                    match plugin_115::cleanup::run_cleanup_empty_media_dirs(&tx, &config).await {
                        Ok(result) => {
                            let _ = tx.send(Ok(plugin_115::cleanup::result_event(result))).await;
                        }
                        Err(status) => {
                            let _ = tx.send(Err(status)).await;
                        }
                    }
                });
                Ok(Response::new(ReceiverStream::new(rx).boxed()))
            }
            other => Err(Status::invalid_argument(format!(
                "未知任务：{other}（本插件只声明 {cleanup}）",
                cleanup = plugin_115::cleanup::CLEANUP_EMPTY_DIRS_TASK
            ))),
        }
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
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: DISPLAY_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            // 声明必须与实现对齐（见 src/lib.rs 的 rpc 表）：空间用量实现了，
            // probe / 缩略图 / 转存 / 离线下载没实现 —— 那些能力一个都不声明。
            capabilities: vec![Capability::SpaceUsage as i32],
            extensions: vec![Extension {
                key: "media.provider".to_owned(),
                data: Some(Data::MediaProvider(MediaProviderBundle {
                    provider_key: self.config.provider_key_or_default().to_owned(),
                    display_name: DISPLAY_NAME.to_owned(),
                    library_config_fields: library_config_fields(),
                    // 只支持 redirect（`PlanPlayback` 的 proxy 是 unimplemented）。
                    playback_deliveries: vec![PlaybackDelivery::Redirect as i32],
                    merged_playback_format: None,
                    download_config_fields: Vec::new(),
                    data_plane_endpoint: None,
                })),
            }],
            // 手动清理任务（`cleanup` 模块文档说明了另一个任务为什么不声明）。
            jobs: vec![JobDefinition {
                task_key: plugin_115::cleanup::CLEANUP_EMPTY_DIRS_TASK.to_owned(),
                log_name: "115-cleanup-empty-media-dirs".to_owned(),
                cli_name: "115-cleanup-empty-media-dirs".to_owned(),
                cli_help: "删除 115 媒体库目录下的空子目录（复核无文件才删）".to_owned(),
                // 五段式 cron 留空 + manual_only：上游就没给它排期，只能手动触发。
                default_cron: String::new(),
                manual_only: true,
                params_schema: Some(plugin_115::cleanup::confirm_params_schema()),
                required_capabilities: Vec::new(),
            }],
            settings_schema: Vec::new(),
            data_plane_endpoint: None,
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
    let provider = Provider115::new(config.clone());

    let listener = TcpListener::bind(addr).await?;
    Server::builder()
        .add_service(PluginControlServer::new(Control { plugin_id, config }))
        .add_service(StorageProviderServer::new(provider))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}
