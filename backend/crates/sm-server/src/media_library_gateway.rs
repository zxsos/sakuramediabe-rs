//! 组合根里的**媒体库能力适配器**：把 `sm-service` 声明的
//! [`sm_service::playback::media_library::MediaLibraryRegistry`] /
//! [`sm_service::playback::media_library::MediaLibraryCapability`]
//! 接到真实插件的 gRPC 上。
//!
//! # 为什么这个文件必须存在
//!
//! 与 [`crate::provider_gateway`] 完全同源：`sm-service` 不能依赖 `sm-plugins`
//! （依赖方向会成环），所以它只声明 trait，实现只能落在同时看得见两边的组合根。
//!
//! | 层 | 职责 |
//! |---|---|
//! | `sm-service` | 声明 trait，用宿主类型（`LibraryConfigField` 等） |
//! | 本模块 | 宿主类型 → proto → `sm_plugins::provider_calls` |
//! | `sm-plugins` | 发 gRPC、把 `tonic::Status` 归类成上游七码 |
//!
//! # ★ 描述符来自注册表，**不是**运行期去问插件
//!
//! `library_config_fields` / `playback_deliveries` / `download_config_fields` 是
//! 注册期就随 `MediaProviderBundle` 收进
//! [`sm_plugins::registry::ProviderRegistration`] 的（见
//! `sm-plugins::loader::collect_providers`）。目录端点与白名单校验都只读这份
//! **本地**快照 —— 不为了渲染一个下拉框去连插件。
//!
//! 但 `prepare_library` / `get_space_usage` 必须**现连**：它们是动作，不是数据。
//! 注册表是**活的**（插件重启会换控制面端口），所以这里拿的是
//! `Arc<Mutex<ProviderRegistry>>`，每次调用现查端点（理由见
//! `provider_gateway.rs` 的模块文档）。
//!
//! # ⚠️ 两处已知缺口（不在这里编造）
//!
//! 1. `supports_in_place_import` 在 proto 的 `MediaProviderBundle` 里**没有这个
//!    字段**（上游读的是 bundle 对象属性，`provider_protocol.py:601-605`）。宿主
//!    无从得知，只能恒报 `false` —— 这会让「支持原地导入的 provider」在
//!    `MediaLibraryResource` 里显示为不支持。补齐需要给 proto 加字段（改 ABI）。
//! 2. `account_key` 在 `PreviousLibraryHandle` 里没有携带，更新时的
//!    `previous` 句柄只能给 `None`；首个 `prepare_library` 仍会如实返回并落库。

use std::sync::{Arc, Mutex, MutexGuard};

use sm_plugins::provider_calls::{self, ProviderOperationError};
use sm_plugins::registration::capability;
use sm_plugins::registry::{ConfigFieldSpec, ProviderRegistration, ProviderRegistry};
use sm_service::playback::media_library::{
    LibraryConfigField, LibraryForFuture, MediaLibraryCapability, MediaLibraryRegistry,
    PrepareLibraryFuture, PreparedLibrary, PreviousLibraryHandle, ProviderCatalogEntry,
    SpaceUsageFuture,
};
use sm_service::playback::provider_helpers::SpaceUsage;
use sm_service::transfers::download_client::ProviderFailureInfo;

/// 未被插件声明时的失败码。服务层会补上 `provider_` 前缀拼成
/// `provider_not_installed`（与 `ProviderRegistration` 查不到同义）。
const NOT_INSTALLED: &str = "not_installed";

/// 适配器。**可克隆**：内部只有一把共享注册表的句柄。
#[derive(Clone)]
pub struct MediaLibraryGateway {
    providers: Arc<Mutex<ProviderRegistry>>,
}

impl MediaLibraryGateway {
    /// 用组合根那一份（活的）注册表构造。
    pub fn new(providers: Arc<Mutex<ProviderRegistry>>) -> Self {
        Self { providers }
    }

    /// 注册表的读锁。容忍中毒，理由同 `provider_gateway::ProviderGateway::registry`。
    fn registry(&self) -> MutexGuard<'_, ProviderRegistry> {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl MediaLibraryRegistry for MediaLibraryGateway {
    fn library_for(&self, provider_key: &str) -> LibraryForFuture<'_> {
        let outcome: Result<Option<Box<dyn MediaLibraryCapability>>, ProviderFailureInfo> =
            match self.registry().get(provider_key) {
                // 查不到 = 没安装（服务层拼成 503 `provider_not_installed`）。
                None => Err(ProviderFailureInfo {
                    code: NOT_INSTALLED.to_owned(),
                    message: "媒体提供方未安装".to_owned(),
                }),
                Some(entry) => Ok(Some(Box::new(MediaLibraryCapabilityAdapter {
                    provider_key: entry.provider_key.clone(),
                    endpoint: entry.plugin_endpoint.clone(),
                    fields: entry.library_config_fields.clone(),
                }))),
            };
        Box::pin(async move { outcome })
    }

    fn supports_in_place_import(&self, _provider_key: &str) -> bool {
        // 见模块文档缺口 1：proto 没有这个字段，无从得知。
        false
    }

    fn list_bundles(&self) -> Vec<ProviderCatalogEntry> {
        self.registry()
            .entries()
            .into_iter()
            .map(bundle_entry)
            .collect()
    }

    fn space_usage(
        &self,
        library_id: i32,
        provider_key: &str,
        provider_config: &serde_json::Value,
    ) -> SpaceUsageFuture<'_> {
        let provider_key = provider_key.to_owned();
        let provider_config = provider_config.clone();
        Box::pin(async move {
            // 端点与能力声明先取出来（锁不能跨 await —— `MutexGuard` 不是 `Send`）。
            let endpoint = {
                let registry = self.registry();
                let entry = registry.get(&provider_key)?;
                if !entry.has(capability::SPACE_USAGE) {
                    // 没声明 = 不支持这个可选能力，上游同样「不出现在结果里」。
                    return None;
                }
                entry.plugin_endpoint.clone()
            };
            let mut client =
                provider_calls::connect_storage(&provider_key, &endpoint, "get_space_usage")
                    .await
                    .ok()?;
            let library = sm_plugin_api::v1::LibraryHandle {
                library_id: i64::from(library_id),
                provider_key: provider_key.clone(),
                provider_config: sm_plugin_api::json_struct::json_to_struct(&provider_config),
                account_key: None,
            };
            let usage = provider_calls::get_space_usage(&mut client, &provider_key, library)
                .await
                .ok()?;
            Some(SpaceUsage {
                total_bytes: usage.total_bytes,
                used_bytes: usage.used_bytes,
                free_bytes: usage.free_bytes,
            })
        })
    }
}

/// 一个已注册 provider 的媒体库能力。持有**注册期收下的描述符** + 活的端点。
struct MediaLibraryCapabilityAdapter {
    provider_key: String,
    endpoint: String,
    fields: Vec<ConfigFieldSpec>,
}

impl MediaLibraryCapability for MediaLibraryCapabilityAdapter {
    fn library_config_fields(&self) -> Vec<LibraryConfigField> {
        self.fields
            .iter()
            .map(|field| LibraryConfigField {
                key: field.key.clone(),
                input: field.input.clone(),
                read_only: field.read_only,
            })
            .collect()
    }

    fn prepare_library(
        &self,
        submitted: &serde_json::Value,
        previous: Option<&PreviousLibraryHandle>,
    ) -> PrepareLibraryFuture<'_> {
        let provider_key = self.provider_key.clone();
        let endpoint = self.endpoint.clone();
        let submitted = submitted.clone();
        let previous = previous.cloned();
        Box::pin(async move {
            let mut client =
                provider_calls::connect_storage(&provider_key, &endpoint, "prepare_library")
                    .await
                    .map_err(to_failure)?;
            // `previous` 只在更新时给；句柄里的 `account_key` 缺口见模块文档。
            let previous_proto = previous.map(|previous| sm_plugin_api::v1::LibraryHandle {
                library_id: i64::from(previous.library_id),
                provider_key: provider_key.clone(),
                provider_config: sm_plugin_api::json_struct::json_to_struct(
                    &previous.provider_config,
                ),
                account_key: None,
            });
            let response = provider_calls::prepare_library(
                &mut client,
                &provider_key,
                &submitted,
                previous_proto,
            )
            .await
            .map_err(to_failure)?;
            // 「返回必须是对象」由服务层 `prepared_object` 判 502，这里不重复。
            Ok(PreparedLibrary {
                provider_config: sm_plugin_api::json_struct::struct_to_json(
                    response.provider_config.as_ref(),
                ),
                account_key: response.account_key,
            })
        })
    }
}

/// 注册条目 → 目录项。字段形状与上游 `list_provider_catalog`（`:240-259`）逐字对齐。
fn bundle_entry(entry: &ProviderRegistration) -> ProviderCatalogEntry {
    ProviderCatalogEntry {
        provider_key: entry.provider_key.clone(),
        display_name: entry.display_name.clone(),
        library_config_fields: entry
            .library_config_fields
            .iter()
            .map(config_field_json)
            .collect(),
        playback_deliveries: entry.playback_deliveries.clone(),
        // 声明了下载能力才有这一节；`None`（不是空数组）前端据此隐藏。
        download_config_fields: entry.has(capability::DOWNLOAD).then(|| {
            entry
                .download_config_fields
                .iter()
                .map(config_field_json)
                .collect()
        }),
    }
}

/// 一个字段 → 上游 `asdict(ConfigField)` 的形状（8 个键，`description` / `hint`
/// 可空发 `null`）。
fn config_field_json(field: &ConfigFieldSpec) -> serde_json::Value {
    serde_json::json!({
        "key": field.key,
        "label": field.label,
        "input": field.input,
        "required": field.required,
        "description": field.description,
        "multiline": field.multiline,
        "read_only": field.read_only,
        "hint": field.hint,
    })
}

/// `sm-plugins` 的错误 → 服务层的失败信封。**只换形状，不再归类。**
fn to_failure(error: ProviderOperationError) -> ProviderFailureInfo {
    ProviderFailureInfo {
        code: error.code().to_owned(),
        message: error.safe_message().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_plugins::registry::ConfigFieldSpec;
    use std::sync::Mutex;

    fn registration() -> ProviderRegistration {
        ProviderRegistration {
            provider_key: "local".to_owned(),
            display_name: "本地盘".to_owned(),
            plugin_id: "p_local".to_owned(),
            capabilities: vec![capability::DOWNLOAD],
            data_plane_endpoint: None,
            plugin_endpoint: "http://127.0.0.1:51001".to_owned(),
            library_config_fields: vec![ConfigFieldSpec {
                key: "root".to_owned(),
                label: "根目录".to_owned(),
                input: "path".to_owned(),
                required: true,
                description: Some("媒体根".to_owned()),
                multiline: false,
                read_only: true,
                hint: None,
            }],
            playback_deliveries: vec!["proxy".to_owned()],
            merged_playback_format: None,
            download_config_fields: vec![ConfigFieldSpec {
                key: "token".to_owned(),
                label: "令牌".to_owned(),
                input: "secret".to_owned(),
                required: true,
                description: None,
                multiline: false,
                read_only: false,
                hint: None,
            }],
        }
    }

    fn gateway_with(entry: ProviderRegistration) -> MediaLibraryGateway {
        let mut registry = ProviderRegistry::new();
        registry.insert(entry);
        MediaLibraryGateway::new(Arc::new(Mutex::new(registry)))
    }

    /// ★ 目录项字段形状与上游 `asdict(ConfigField)` 一致（8 个键，含可空键）。
    #[test]
    fn a_bundle_becomes_the_upstream_catalog_shape() {
        let bundles = gateway_with(registration()).list_bundles();
        assert_eq!(bundles.len(), 1);
        let entry = &bundles[0];
        assert_eq!(entry.provider_key, "local");
        assert_eq!(entry.playback_deliveries, vec!["proxy"]);
        let field = &entry.library_config_fields[0];
        for key in [
            "key",
            "label",
            "input",
            "required",
            "description",
            "multiline",
            "read_only",
            "hint",
        ] {
            assert!(field.get(key).is_some(), "缺键 {key}");
        }
        assert_eq!(field["input"], "path");
        assert_eq!(field["read_only"], true);
        // 声明了 DOWNLOAD → `download_config_fields` 是 Some（哪怕字段空）。
        assert_eq!(entry.download_config_fields.as_ref().unwrap().len(), 1);
    }

    /// 没有下载能力的 provider：`download_config_fields` 是 **`None`** 不是空数组。
    #[test]
    fn a_bundle_without_downloads_has_no_fields_section() {
        let mut entry = registration();
        entry.capabilities.clear();
        entry.download_config_fields.clear();
        let bundles = gateway_with(entry).list_bundles();
        assert!(bundles[0].download_config_fields.is_none());
    }

    /// ★ 没安装的 provider：`Err`（服务层拼 `provider_not_installed`），不是 `Ok(None)`。
    #[tokio::test]
    async fn an_unknown_provider_is_not_installed() {
        let gateway = gateway_with(registration());
        // `expect_err` 要 `T: Debug`，而 `Box<dyn MediaLibraryCapability>` 不是 Debug。
        let error = match gateway.library_for("nope").await {
            Err(error) => error,
            Ok(_) => panic!("没注册就该报错"),
        };
        assert_eq!(error.code, NOT_INSTALLED);
    }

    /// 装了的 provider：`Ok(Some(..))`，且能力带着注册期的字段表。
    #[tokio::test]
    async fn a_known_provider_yields_a_capability_with_its_fields() {
        let gateway = gateway_with(registration());
        let capability = gateway
            .library_for("local")
            .await
            .expect("装了就不报错")
            .expect("有库能力");
        let fields = capability.library_config_fields();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].key, "root");
        assert_eq!(fields[0].input, "path");
        assert!(fields[0].read_only);
    }
}
