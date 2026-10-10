//! 控制面：`PluginControl` 的 115 实现。
//!
//! 115 **主要是数据面插件**（`StorageProvider`），但带一个**手动清理任务**
//! （[`crate::cleanup::CLEANUP_EMPTY_DIRS_TASK`]，见 `cleanup` 模块文档 ——
//! 另一个任务被 ABI 卡住，不声明）。注册同时向宿主声明 `media.provider`
//! 扩展点；这段原先不存在 —— vendored 的 `plugin-115-provider` 只重写了
//! provider 侧，宿主侧没有任何东西告诉宿主「有这么一个 provider」。进程内
//! 接线必须补上它，否则 `collect_providers` 收不到 115。
//!
//! # 声明必须与实现对齐
//!
//! `capabilities` / `playback_deliveries` 只声明本 crate **真的实现了**的那部分
//! （见 `provider.rs`）：
//!
//! - 实现了 `GetSpaceUsage` → 声明 [`Capability::SpaceUsage`]；
//! - `PlanPlayback` 只支持 redirect（`proxy` 返回 `unimplemented`）→
//!   `playback_deliveries = [REDIRECT]`（proto 要求至少含 REDIRECT / PROXY，
//!   首项为默认方式）；
//! - 没实现 probe / 缩略图 / 转存 / 离线下载 → 那些能力**一个都不声明**。

use sm_plugin_api::v1::config_field::Input;
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::{
    Capability, ConfigField, Extension, JobDefinition, JobEvent, MediaProviderBundle,
    PlaybackDelivery, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tonic::{Request, Response, Status};

use futures::StreamExt;

/// 上游 `manifest.json` 的 `plugin_id`。宿主按
/// `<plugins.root_dir>/<plugin_id>/<plugin_id>` 找可执行文件，所以它也决定了
/// 目录名与二进制名。
pub const PLUGIN_ID: &str = "sakuramedia_115_provider";

/// 缺省 provider key（[`crate::config::Plugin115Config::provider_key_or_default`]）。
pub const DEFAULT_PROVIDER_KEY: &str = "115";

/// 上游 `manifest.json` 的 `display_name`。
pub const DISPLAY_NAME: &str = "115 网盘";

/// 控制面。
pub struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
    plugin_id: String,
    /// 数据面声明的 provider 键。
    provider_key: String,
    /// 运行设置（Cookie、媒体根目录）。清理任务从这里读 —— 数据面按请求带
    /// `LibraryHandle`，任务面只有 settings。
    config: crate::config::Plugin115Config,
}

impl Control {
    /// 缺省 provider 键、缺省设置。
    pub fn new(plugin_id: String) -> Self {
        Self {
            plugin_id,
            provider_key: DEFAULT_PROVIDER_KEY.to_owned(),
            config: crate::config::Plugin115Config::default(),
        }
    }

    /// 显式 provider 键（进程内从 `settings` 读）。
    pub fn with_provider_key(plugin_id: String, provider_key: String) -> Self {
        Self {
            plugin_id,
            provider_key,
            config: crate::config::Plugin115Config::default(),
        }
    }

    /// 完整运行设置（进程内装配用：任务面要 Cookie 与媒体根目录）。
    pub fn with_settings(plugin_id: String, config: crate::config::Plugin115Config) -> Self {
        Self {
            provider_key: config.provider_key_or_default().to_owned(),
            plugin_id,
            config,
        }
    }

    /// 建库配置字段（`PrepareLibrary` / provider 私有配置的契约）。
    ///
    /// 与 [`crate::config::Plugin115Config`] 读取的键一一对应：Cookie 两个
    /// （secret），媒体 / 下载目录两个（path）。
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

    /// 手动清理任务（见 [`crate::cleanup`]）。流式：扫描与删除的进度逐帧发，
    /// 终态摘要作为 `result` 帧收尾。
    ///
    /// 未知任务回 `invalid_argument`（不是 `not_found`）：让调用方看出
    /// 「任务键写错了」，而不是「任务存在但找不到」。
    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let request = request.into_inner();
        match request.task_key.as_str() {
            crate::cleanup::CLEANUP_EMPTY_DIRS_TASK => {
                crate::cleanup::extract_confirm_params(&request)?;
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                let config = self.config.clone();
                tokio::spawn(async move {
                    match crate::cleanup::run_cleanup_empty_media_dirs(&tx, &config).await {
                        Ok(result) => {
                            let _ = tx.send(Ok(crate::cleanup::result_event(result))).await;
                        }
                        Err(status) => {
                            let _ = tx.send(Err(status)).await;
                        }
                    }
                });
                Ok(Response::new(
                    tokio_stream::wrappers::ReceiverStream::new(rx).boxed(),
                ))
            }
            other => Err(Status::invalid_argument(format!(
                "未知任务：{other}（本插件只声明 {cleanup}）",
                cleanup = crate::cleanup::CLEANUP_EMPTY_DIRS_TASK
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
            // 只声明真的实现了的：空间用量。
            capabilities: vec![Capability::SpaceUsage as i32],
            extensions: vec![Extension {
                key: "media.provider".to_owned(),
                data: Some(Data::MediaProvider(MediaProviderBundle {
                    provider_key: self.provider_key.clone(),
                    display_name: DISPLAY_NAME.to_owned(),
                    library_config_fields: Self::library_config_fields(),
                    // 只支持 redirect（见模块文档）。
                    playback_deliveries: vec![PlaybackDelivery::Redirect as i32],
                    merged_playback_format: None,
                    download_config_fields: Vec::new(),
                    data_plane_endpoint: None,
                })),
            }],
            // 手动清理任务（`cleanup` 模块文档说明了另一个任务为什么不声明）。
            jobs: vec![JobDefinition {
                task_key: crate::cleanup::CLEANUP_EMPTY_DIRS_TASK.to_owned(),
                log_name: "115-cleanup-empty-media-dirs".to_owned(),
                cli_name: "115-cleanup-empty-media-dirs".to_owned(),
                cli_help: "删除 115 媒体库目录下的空子目录（复核无文件才删）".to_owned(),
                // 五段式 cron 留空 + manual_only：上游就没给它排期，只能手动触发。
                default_cron: String::new(),
                manual_only: true,
                params_schema: Some(crate::cleanup::confirm_params_schema()),
                required_capabilities: Vec::new(),
            }],
            settings_schema: Vec::new(),
            data_plane_endpoint: None,
        }))
    }
}
