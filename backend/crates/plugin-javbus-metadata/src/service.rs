//! gRPC 服务面：`PluginControl` 与 `MetadataSourceExtensionService`。
//!
//! # 为什么两个 service 在同一个进程、同一个端口
//!
//! proto 里它们各是一个 service，但**没有任何字段声明另一个端口** —— 只有
//! 数据面有 `data_plane_endpoint`，而它留给字节搬运（`docs/adr/
//! 2026-10-05-plugin-lifecycle.md` 第 3 节）。所以控制面与扩展点共用宿主分配
//! 的那个地址，与 `plugin-ref-local` 同一个手法。
//!
//! # `found = false` 是正常响应
//!
//! `FetchMovieResponse.found` 上的 proto 注释：「未收录时返回空响应，宿主会
//! 尝试下一个来源」。所以「没收录」走 `Ok`，`Err` 只留给「调用失败」——
//! 混起来的后果是宿主的兜底链路在第一个来源就停下。
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control`] 的
//! `register` 只回声明；HTTP 客户端是在 `main` 里建好的（建客户端不发请求）。

use std::path::PathBuf;

use futures::stream::BoxStream;
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::metadata_source_extension_service_server::MetadataSourceExtensionService;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::{
    Capability, Extension, FetchMovieRequest, FetchMovieResponse, JobEvent,
    MetadataSourceExtension, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tonic::{Request, Response, Status};

use crate::javbus::{FetchError, JavBusSource};
use crate::settings;

/// `catalog.metadata_source` 扩展点 key。
///
/// 与宿主侧 `sm_plugins::extensions::METADATA_SOURCE` 是同一个字面量，但插件
/// 不依赖 `sm-plugins`（那条依赖只在测试里用），所以各持一份 —— 它是 proto
/// 里写死的协议常量，不是两处各自定义的常量。
pub const METADATA_SOURCE_KEY: &str = "catalog.metadata_source";

/// 控制面。
pub struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
    plugin_id: String,
}

impl Control {
    pub fn new(plugin_id: String) -> Self {
        Self { plugin_id }
    }
}

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    /// 本插件没有后台任务。生成的 trait 没有默认方法体，只能显式写它 ——
    /// 与 `plugin-ref-local` 同一个处理。
    async fn run_job(
        &self,
        _request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        Err(Status::unimplemented("JavBus 元数据插件不提供后台任务"))
    }

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let injected = request.into_inner().plugin_id;
        // 宿主注入什么就回什么：不一致会让宿主的加载校验直接失败，而这个
        // 进程自己也说不清「我是谁」，不如当场拒。
        if injected != self.plugin_id {
            return Err(Status::invalid_argument(format!(
                "宿主注入的 plugin_id 与启动参数不一致：注入 {injected}，本进程 {}",
                self.plugin_id
            )));
        }
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: crate::DISPLAY_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            // **不要**抄 manifest 里的 `host_api_version: 6` —— 那是 Python 侧
            // 的版本号；这里是 Rust 侧的 ABI 主版本。
            abi_major: sm_plugin_api::ABI_MAJOR,
            // 扩展点的调用前提：proto 的原话是「仅在声明对应 capability 时才
            // 会被调用」，宿主对没声明能力的扩展点一律不收。
            capabilities: vec![Capability::ExtensionCatalogMetadataSource as i32],
            extensions: vec![Extension {
                key: METADATA_SOURCE_KEY.to_owned(),
                data: Some(Data::MetadataSource(MetadataSourceExtension::default())),
            }],
            jobs: Vec::new(),
            // 让宿主渲染配置表单；值从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读。
            settings_schema: settings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// `catalog.metadata_source` 扩展点。
pub struct Metadata {
    source: JavBusSource,
}

impl Metadata {
    pub fn new(source: JavBusSource) -> Self {
        Self { source }
    }
}

#[tonic::async_trait]
impl MetadataSourceExtensionService for Metadata {
    async fn fetch_movie(
        &self,
        request: Request<FetchMovieRequest>,
    ) -> Result<Response<FetchMovieResponse>, Status> {
        let inner = request.into_inner();
        // 空的 `delivery_dir` 会让图片落到进程的当前目录 —— 那是宿主的用法
        // 错，而且会绕过交付校验的边界，所以先挡住。
        if inner.delivery_dir.trim().is_empty() {
            return Err(Status::invalid_argument(
                "FetchMovieRequest.delivery_dir 为空：图片没有可落的边界",
            ));
        }

        match self
            .source
            .fetch_movie(&inner.movie_number, &PathBuf::from(&inner.delivery_dir))
            .await
        {
            Ok(Some(response)) => Ok(Response::new(response)),
            // 「没收录」：正常响应（`found` 缺省就是 false），不是 Err。
            Ok(None) => Ok(Response::new(FetchMovieResponse::default())),
            Err(err) => Err(status_of(&err)),
        }
    }
}

/// 把取数失败压成 gRPC 状态。
///
/// 只分两类：宿主真正会区别对待的只有「时限到」（它自己在
/// `sm_plugins::extension_calls::classify` 里认），其余都按「这次没取到」报
/// `unavailable` —— 换一个来源或稍后重试都可能成功。写不进交付目录是另一类：
/// 那是宿主给错了目录，重试也一样，报 `internal`。
fn status_of(err: &FetchError) -> Status {
    match err {
        FetchError::Delivery(_) | FetchError::Client(_) => Status::internal(err.to_string()),
        _ => Status::unavailable(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_failure_is_internal_but_a_site_failure_is_not() {
        // 前者重试也没用（宿主给的目录有问题），后者换个时间或来源可能就成了。
        assert_eq!(
            status_of(&FetchError::Delivery("x".to_owned())).code(),
            tonic::Code::Internal
        );
        assert_eq!(
            status_of(&FetchError::Verification).code(),
            tonic::Code::Unavailable
        );
    }
}
