//! 控制面：`PluginControl` 的参考实现。
//!
//! 原先这段住在可执行文件里（`src/bin/plugin-ref-local.rs`）。挪进 lib 是为了让
//! **进程式与进程内共用同一份装配**：进程内形态由 `sm-server` 直接调
//! [`crate::serve`]，那条路够不到 bin 里的私有结构。
//!
//! 参考插件不提供后台任务，所以 `RunJob` 一律 `unimplemented` —— 与 provider
//! 侧那 28 个方法同一个处理（见 `sm_plugin_api::provider` 的模块文档：生成的
//! trait 没有默认方法体，只能一个个写）。

use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::{
    Extension, JobEvent, MediaProviderBundle, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tonic::{Request, Response, Status};

/// 配置里的 `provider_key`，缺省这个值。
pub const DEFAULT_PROVIDER_KEY: &str = "local_ref";

/// 控制面。`provider_key` 在**构造时定死**：进程式从宿主写好的配置文件读，
/// 进程内由组合根把 `settings` 交进来 —— 两条路都不在任务里读进程环境。
pub struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
    plugin_id: String,
    /// 数据面声明的 provider 键。
    provider_key: String,
}

impl Control {
    /// 缺省 provider 键。
    pub fn new(plugin_id: String) -> Self {
        Self {
            plugin_id,
            provider_key: DEFAULT_PROVIDER_KEY.to_owned(),
        }
    }

    /// 显式 provider 键（进程式从配置文件读、进程内从 settings 读）。
    pub fn with_provider_key(plugin_id: String, provider_key: String) -> Self {
        Self {
            plugin_id,
            provider_key,
        }
    }
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
            // ⚠️ 这里**曾经**声明 `Capability::Download`，而本 crate 里根本没有
            // `DownloadProvider` 的实现 —— 宿主会以为能提交下载，照着抄的插件
            // 作者也会以为「声明了就算实现了」。声明必须与实现对齐：
            // 本插件只 serve `StorageProvider`（见 `src/lib.rs`），所以不声明
            // 任何"额外"能力。
            //
            // 参考：宿主侧在注册期**不**校验「声明 ↔ 是否 serve 了对应 service」
            // （那是 `docs/adr/2026-10-06-plugin-architecture.md` 登记的缺口之一），
            // 所以虚报不会被发现 —— 这正是它危险的地方。
            capabilities: Vec::new(),
            extensions: vec![Extension {
                key: "media.provider".to_owned(),
                data: Some(Data::MediaProvider(MediaProviderBundle {
                    provider_key: self.provider_key.clone(),
                    display_name: "本地目录".to_owned(),
                    ..Default::default()
                })),
            }],
            ..Default::default()
        }))
    }
}
