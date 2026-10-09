//! 已加载插件的能力注册表。
//!
//! # 上游对应
//!
//! Python 侧是 `MEDIA_PROVIDER_REGISTRY`（`src/plugins/provider_protocol.py`），
//! 宿主用它做三件事：
//!
//! ```python
//! providers = MEDIA_PROVIDER_REGISTRY.require(key)      # 按 key 取
//! MEDIA_PROVIDER_REGISTRY.download_for(handle)           # 按能力取
//! ```
//!
//! 而「能力」在 Python 侧是 `supports_*()` 的 getattr 探测；gRPC 没有方法存在性，
//! 所以改成注册时显式声明 —— 本模块就是那份声明的**查询面**。
//!
//! # 为什么键是 `provider_key` 而不是插件 id
//!
//! 一个插件可以注册多个 provider（`RegisterResponse.extensions` 是列表）。所以
//! 注册表的粒度是 **provider**，不是插件；`plugin_id` 只作为来源记在条目上，
//! 便于「这个 provider 是哪个插件给的」这类排障。
//!
//! # 顺序 = `plugins.enabled` 的顺序
//!
//! 上游按 `plugins.enabled` 决定多个 provider 之间的优先级（比如两个插件都能做
//! metadata_source，取列表里靠前的那个）。所以这里**保留插入顺序**，
//! [`ProviderRegistry::providers_with`] 按它返回。

use std::collections::HashMap;

use crate::registration::capability;

/// 插件声明的一个配置项（`MediaProviderBundle` 的 `ConfigField`）。
///
/// # 为什么是**纯值**而不是 `sm_plugin_api::v1::ConfigField`
///
/// `ProviderRegistration` 要 `Eq`（注册表用它做条目比较），而 prost 生成的
/// 消息没有 `Eq`。更关键的是：这份字段表要被组合根拿去喂服务层的白名单校验与
/// 目录端点，两边都用宿主自己的值类型 —— 本模块不把 gRPC 生成类型泄漏出去。
///
/// `input` 用上游的字符串字面量（`"text"` / `"secret"` / `"path"`），不是 proto 枚举。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFieldSpec {
    pub key: String,
    pub label: String,
    /// `"text"` / `"secret"` / `"path"`。
    pub input: String,
    pub required: bool,
    pub description: Option<String>,
    pub multiline: bool,
    /// 只读字段：更新时**从旧值回填**，且用户不能提交（上游 `_prepare_config`）。
    pub read_only: bool,
    pub hint: Option<String>,
}

/// 一个 provider 的注册条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRegistration {
    /// 全局唯一（`MediaProviderBundle.provider_key`）。
    pub provider_key: String,
    pub display_name: String,
    /// 提供它的插件 id —— 只用于排障与日志，不参与查找。
    pub plugin_id: String,
    /// 注册时声明的能力。取值见 [`capability`]。
    pub capabilities: Vec<i32>,
    /// 数据面端点（gRPC）。有它时 proxy 播放与转存走这个端点，不再走控制面
    /// 的字节流（上游注释：避免为「只有 IO 的 provider」再实现一遍字节流）。
    pub data_plane_endpoint: Option<String>,
    /// **提供它的那个插件的控制面端点**（`http://127.0.0.1:port`）。
    ///
    /// # 为什么注册表里必须有它
    ///
    /// `StorageProvider` / `DownloadProvider` 这两个 service 与 `PluginControl`
    /// 由**同一个插件进程**提供（proto 里各是一个 service，但没有字段声明另一个
    /// 端口），所以「调 provider」= 「连它的控制面再建一个 client」。
    /// 没有这个字段，查表只能查到「capabilities 里有 SCAN_MEDIA_REFS」这类声明，
    /// 却**打不出去**。
    ///
    /// 与上面的 `data_plane_endpoint` 是两回事：那个是**数据面**（大文件字节
    /// 流），这个是**控制面**（结构化 rpc）。
    pub plugin_endpoint: String,
    /// 媒体库配置字段表（`MediaProviderBundle.library_config_fields`）。
    ///
    /// # 为什么注册表必须存它
    ///
    /// 这是**白名单 / secret / 只读**的唯一判据来源：服务层的 `_validate_config`
    /// 拿它判「未知字段 / 只读字段」，`_resource` 拿它决定剥哪些 secret。
    /// 不存的话字段表为空 → 用户提交的每个字段都被判未知 → **建库/改配置一律 422**。
    pub library_config_fields: Vec<ConfigFieldSpec>,
    /// 播放交付方式，**首项为默认**（proto 注释：非空且不重复，必含 REDIRECT 或
    /// PROXY）。取值 `"redirect"` / `"proxy"`。
    ///
    /// `videos` / `video_collections` 的 `play_url` 取 `[0]` —— 没有它那些端点
    /// 无法生成签名播放地址。
    pub playback_deliveries: Vec<String>,
    /// 合并播放的封装格式（`"mp4"` / `"hls"`）。未声明为 `None`。
    pub merged_playback_format: Option<String>,
    /// 下载组件的配置字段（`MediaProviderBundle.download_config_fields`）。
    /// 未声明下载能力时为空。
    pub download_config_fields: Vec<ConfigFieldSpec>,
}

impl ProviderRegistration {
    /// 是否声明了某个能力。
    pub fn has(&self, capability_value: i32) -> bool {
        self.capabilities.contains(&capability_value)
    }

    /// 是否声明了下载能力（`CAPABILITY_DOWNLOAD`）。
    pub fn is_download(&self) -> bool {
        self.has(capability::DOWNLOAD)
    }
}

/// 注册表。**顺序 = 插入顺序 = `plugins.enabled` 的顺序。**
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    by_key: HashMap<String, ProviderRegistration>,
    order: Vec<String>,
}

/// 注册表查不到时的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// 没有这个 `provider_key`。
    UnknownProvider { key: String },
    /// 有这个 provider，但它没声明该能力 —— 与「不存在」是**两回事**，
    /// 客户端要的提示不同（前者是配置错了，后者是插件不支持）。
    MissingCapability { key: String, capability: i32 },
}

impl RegistryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownProvider { .. } => "provider_not_installed",
            Self::MissingCapability { .. } => "provider_capability_missing",
        }
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 provider。**同 key 后注册者覆盖先注册者，但不改变顺序** ——
    /// 顺序表达的是 `plugins.enabled` 的优先级，不该被覆盖动作打乱。
    pub fn insert(&mut self, entry: ProviderRegistration) {
        let key = entry.provider_key.clone();
        if !self.by_key.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.by_key.insert(key, entry);
    }

    pub fn get(&self, key: &str) -> Option<&ProviderRegistration> {
        self.by_key.get(key)
    }

    /// 取一个 provider，不存在则报错。
    pub fn require(&self, key: &str) -> Result<&ProviderRegistration, RegistryError> {
        self.get(key).ok_or_else(|| RegistryError::UnknownProvider {
            key: key.to_owned(),
        })
    }

    /// 取一个 provider **并要求它声明了某个能力**。
    ///
    /// 这是调用侧的常规入口：先确认存在，再确认支持 —— 两者分开报错，客户端
    /// 才能区分「配置里写错了 key」与「这个插件不支持这个动作」。
    pub fn require_with(
        &self,
        key: &str,
        capability_value: i32,
    ) -> Result<&ProviderRegistration, RegistryError> {
        let entry = self.require(key)?;
        if entry.has(capability_value) {
            Ok(entry)
        } else {
            Err(RegistryError::MissingCapability {
                key: key.to_owned(),
                capability: capability_value,
            })
        }
    }

    /// 全部条目，**按 `plugins.enabled` 顺序**。
    ///
    /// 组合根用它把「一个插件自己的 provider 表」合进宿主那张总表 ——
    /// 上游是 `refresh_media_provider_registry(_ACTIVE_PLUGINS)` 一次刷全量。
    pub fn entries(&self) -> Vec<&ProviderRegistration> {
        self.order
            .iter()
            .filter_map(|key| self.by_key.get(key))
            .collect()
    }

    /// 所有声明了该能力的 provider，**按 `plugins.enabled` 顺序**。
    ///
    /// 上游的兜底链路（如 JavDB 未收录时的 metadata_source）就是按这个顺序
    /// 依次尝试的。
    pub fn providers_with(&self, capability_value: i32) -> Vec<&ProviderRegistration> {
        self.order
            .iter()
            .filter_map(|key| self.by_key.get(key))
            .filter(|entry| entry.has(capability_value))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, plugin: &str, capabilities: Vec<i32>) -> ProviderRegistration {
        ProviderRegistration {
            provider_key: key.to_owned(),
            display_name: key.to_uppercase(),
            plugin_id: plugin.to_owned(),
            capabilities,
            data_plane_endpoint: None,
            plugin_endpoint: "http://127.0.0.1:0".to_owned(),
            library_config_fields: Vec::new(),
            playback_deliveries: Vec::new(),
            merged_playback_format: None,
            download_config_fields: Vec::new(),
        }
    }

    #[test]
    fn lookup_by_key_and_by_capability() {
        let mut registry = ProviderRegistry::new();
        registry.insert(entry("local", "p_local", vec![capability::DOWNLOAD]));
        registry.insert(entry("cloud", "p_cloud", vec![capability::TRANSFER_TARGET]));

        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.require("local").unwrap().plugin_id,
            "p_local",
            "条目要记得它来自哪个插件"
        );
        assert!(registry.require("local").unwrap().is_download());

        // 按能力筛选：只有 local 声明了下载。
        let downloads = registry.providers_with(capability::DOWNLOAD);
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].provider_key, "local");
    }

    #[test]
    fn the_enabled_order_is_preserved() {
        let mut registry = ProviderRegistry::new();
        registry.insert(entry(
            "second",
            "p",
            vec![capability::EXTENSION_RANKING_SOURCE],
        ));
        registry.insert(entry(
            "first",
            "p",
            vec![capability::EXTENSION_RANKING_SOURCE],
        ));

        // 插入顺序即 `plugins.enabled` 顺序，兜底链路靠它决定先试谁。
        let sources = registry.providers_with(capability::EXTENSION_RANKING_SOURCE);
        assert_eq!(
            sources
                .iter()
                .map(|e| e.provider_key.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );

        // 重新注册同 key **不**改变顺序。
        let mut updated = entry("second", "p", vec![capability::EXTENSION_RANKING_SOURCE]);
        updated.display_name = "改过的名字".to_owned();
        registry.insert(updated);
        let sources = registry.providers_with(capability::EXTENSION_RANKING_SOURCE);
        assert_eq!(sources[0].display_name, "改过的名字");
        assert_eq!(sources[0].provider_key, "second", "顺序不该被覆盖打乱");
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn unknown_provider_and_missing_capability_are_different_errors() {
        let mut registry = ProviderRegistry::new();
        registry.insert(entry("local", "p", vec![capability::DOWNLOAD]));

        // ① key 写错了。
        let error = registry.require("nope").unwrap_err();
        assert_eq!(
            error,
            RegistryError::UnknownProvider {
                key: "nope".to_owned()
            }
        );
        assert_eq!(error.code(), "provider_not_installed");

        // ② key 存在，但插件没声明这个能力 —— 与上一种是两种提示。
        let error = registry
            .require_with("local", capability::TRANSFER_SOURCE)
            .unwrap_err();
        assert_eq!(
            error,
            RegistryError::MissingCapability {
                key: "local".to_owned(),
                capability: capability::TRANSFER_SOURCE
            }
        );
        assert_eq!(error.code(), "provider_capability_missing");
    }

    #[test]
    fn a_provider_without_optional_capabilities_is_still_valid() {
        // 12 项必需能力没有 capability 声明；可选能力一个都没有也是合法 provider。
        let mut registry = ProviderRegistry::new();
        registry.insert(entry("minimal", "p", vec![]));
        assert_eq!(registry.len(), 1);
        assert!(registry.providers_with(capability::DOWNLOAD).is_empty());
        assert!(registry.require("minimal").is_ok());
    }
}
