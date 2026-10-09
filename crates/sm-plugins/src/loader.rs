//! 加载一个插件：建通道 → 调 `Register` → 校验 → 收进注册表。
//!
//! # 上游对应
//!
//! `PluginControl.Register` 是插件生命周期的第一步：宿主注入 `plugin_id` 与
//! `abi_major`，插件回显并附上自己的声明。proto 里写明了注册阶段的边界：
//!
//! > 注册阶段只应构造声明与校验本地配置：**不要联网、不要创建外部目录、
//! > 不要启动后台线程。**
//!
//! # 为什么「连接」与「校验」分开
//!
//! 连接失败是环境问题（插件起不来 / 端口不通），校验失败是契约问题（ABI 不
//! 匹配 / id 对不上）。两者排障方向完全不同，所以错误类型也分开。
//!
//! # 进程管理**不在本模块**
//!
//! 这里只负责「插件已经起来了、给个 endpoint」这一步。拉起进程、握端口、重启
//! 与看门狗属于生命周期管理，留给后续 —— 本模块刻意不碰，免得把 I/O 与校验
//! 搅在一起。

use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::{
    plugin_control_client::PluginControlClient, RegisterRequest, RegisterResponse,
};
use tonic::transport::{Channel, Endpoint};

use crate::registration::{validate_registration, RegistrationProblem};
use crate::registry::{ProviderRegistration, ProviderRegistry};

/// 连接阶段的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// endpoint 不合法（URI 解析失败）。
    InvalidEndpoint(String),
    /// 连不上。
    Transport(String),
}

impl ConnectError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidEndpoint(_) => "plugin_endpoint_invalid",
            Self::Transport(_) => "plugin_unreachable",
        }
    }
}

/// 注册阶段的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// gRPC 调用本身失败（含插件在注册时 panic）。
    Call(String),
    /// 调用成功，但声明不符合契约。
    Invalid(Vec<RegistrationProblem>),
}

impl RegisterError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Call(_) => "plugin_register_failed",
            Self::Invalid(_) => "plugin_registration_invalid",
        }
    }
}

/// 与插件的控制面建立通道。
pub async fn connect(endpoint: Endpoint) -> Result<PluginControlClient<Channel>, ConnectError> {
    let channel = endpoint
        .connect()
        .await
        .map_err(|err| ConnectError::Transport(err.to_string()))?;
    Ok(PluginControlClient::new(channel))
}

/// 调 `Register` 并校验回显。
///
/// `manifest_id` 是插件包清单里声明的 `plugin_id` —— 必须与回显值一致，
/// 否则视为加载失败（proto 注释的原话）。
pub async fn register(
    client: &mut PluginControlClient<Channel>,
    plugin_id: &str,
    manifest_id: &str,
) -> Result<RegisterResponse, RegisterError> {
    let request = RegisterRequest {
        plugin_id: plugin_id.to_owned(),
        abi_major: sm_plugin_api::ABI_MAJOR,
    };
    let response = client
        .register(request)
        .await
        .map_err(|err| RegisterError::Call(err.to_string()))?
        .into_inner();

    // 能力是 `repeated Capability`，prost 生成为 `Vec<i32>`。
    validate_registration(
        plugin_id,
        manifest_id,
        &response.plugin_id,
        response.abi_major,
        &response.capabilities,
    )
    .map_err(RegisterError::Invalid)?;

    Ok(response)
}

/// 把注册声明里的 provider 收进注册表。
///
/// # 只收 `media_provider` 这一个扩展点
///
/// `Extension` 是 oneof（`media_provider` / metadata_source / ranking_source）。
/// 注册表目前只表达「媒体 provider」，其余两个扩展点有各自的调用面，等用到时
/// 再决定怎么存 —— 这里**显式忽略**而不是硬塞进同一个表。
pub fn collect_providers(response: &RegisterResponse) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    for extension in &response.extensions {
        // oneof 的变体名取**字段名**（prost 的规则），不是消息类型名。
        let Some(Data::MediaProvider(bundle)) = &extension.data else {
            continue;
        };
        registry.insert(ProviderRegistration {
            provider_key: bundle.provider_key.clone(),
            display_name: bundle.display_name.clone(),
            plugin_id: response.plugin_id.clone(),
            capabilities: response.capabilities.clone(),
            // 数据面端点：provider 级优先，没有则退回插件级。
            data_plane_endpoint: bundle
                .data_plane_endpoint
                .clone()
                .or_else(|| response.data_plane_endpoint.clone()),
        });
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::capability;

    fn media_bundle(key: &str) -> sm_plugin_api::v1::MediaProviderBundle {
        sm_plugin_api::v1::MediaProviderBundle {
            provider_key: key.to_owned(),
            display_name: key.to_uppercase(),
            ..Default::default()
        }
    }

    fn extension_with_media(key: &str) -> sm_plugin_api::v1::Extension {
        sm_plugin_api::v1::Extension {
            key: "media.provider".to_owned(),
            data: Some(Data::MediaProvider(media_bundle(key))),
        }
    }

    #[test]
    fn providers_are_collected_and_keep_their_plugin_and_capabilities() {
        let response = RegisterResponse {
            plugin_id: "local".to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            capabilities: vec![capability::DOWNLOAD],
            extensions: vec![extension_with_media("local_storage")],
            ..Default::default()
        };

        let registry = collect_providers(&response);
        assert_eq!(registry.len(), 1);

        let entry = registry.require("local_storage").expect("应当收进去");
        assert_eq!(entry.plugin_id, "local", "要记得来自哪个插件");
        assert!(entry.is_download());

        // 插件级的数据面端点要继承下来。
        let with_endpoint = RegisterResponse {
            data_plane_endpoint: Some("http://127.0.0.1:50051".to_owned()),
            ..response
        };
        let registry = collect_providers(&with_endpoint);
        assert_eq!(
            registry
                .require("local_storage")
                .unwrap()
                .data_plane_endpoint
                .as_deref(),
            Some("http://127.0.0.1:50051")
        );
    }

    #[test]
    fn non_media_extensions_are_ignored_on_purpose() {
        // metadata_source / ranking_source 有自己的调用面，不进这个表。
        let response = RegisterResponse {
            plugin_id: "extra".to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            extensions: vec![
                sm_plugin_api::v1::Extension {
                    key: "catalog.metadata_source".to_owned(),
                    data: Some(Data::MetadataSource(Default::default())),
                },
                sm_plugin_api::v1::Extension {
                    key: "media.provider".to_owned(),
                    data: None,
                },
            ],
            ..Default::default()
        };

        let registry = collect_providers(&response);
        assert!(registry.is_empty(), "只收 media_provider：{registry:?}");
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            ConnectError::InvalidEndpoint("x".to_owned()).code(),
            "plugin_endpoint_invalid"
        );
        assert_eq!(
            ConnectError::Transport("x".to_owned()).code(),
            "plugin_unreachable"
        );
        assert_eq!(
            RegisterError::Call("x".to_owned()).code(),
            "plugin_register_failed"
        );
        assert_eq!(
            RegisterError::Invalid(vec![]).code(),
            "plugin_registration_invalid"
        );
    }
}
