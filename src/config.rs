//! 插件配置。
//!
//! 115 认证走 Cookie（与 Python 版一致）：
//!
//! - `web_cookie`：从 115 网页端复制的 Cookie（可选）。
//! - `device_cookie`：微信/支付宝小程序的设备 Cookie（可选）。
//!
//! 两者至少填一个。密钥**绝不**硬编码：来源按优先级是
//!
//! 1. `LibraryHandle.provider_config`（宿主下发的 `google.protobuf.Struct`）
//! 2. 环境变量 `PLUGIN_115_WEB_COOKIE` / `PLUGIN_115_DEVICE_COOKIE`
//! 3. 宿主写好的配置文件 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`
//!
//! 目录配置：
//!
//! - `media_root_path`：导入媒体的目标目录（115 绝对路径，如 `/媒体/电影`）
//! - `downloads_root_path`：离线任务保存目录（115 绝对路径）

use prost_types::Struct;
use serde::Deserialize;

/// 从 `google.protobuf.Struct` 里取字符串字段。
fn struct_str(source: &Struct, key: &str) -> Option<String> {
    use prost_types::value::Kind;
    match source.fields.get(key)?.kind.as_ref()? {
        Kind::StringValue(value) => Some(value.clone()),
        _ => None,
    }
}

/// 115 插件配置。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Plugin115Config {
    /// 网页端 Cookie。
    #[serde(default)]
    pub web_cookie: String,
    /// 设备 Cookie（小程序）。
    #[serde(default)]
    pub device_cookie: String,
    /// 媒体根目录（115 绝对路径）。
    #[serde(default)]
    pub media_root_path: String,
    /// 离线下载根目录（115 绝对路径）。
    #[serde(default)]
    pub downloads_root_path: String,
    /// provider key（默认 `115`）。
    #[serde(default)]
    pub provider_key: String,
}

impl Plugin115Config {
    /// 有效的 Cookie（优先 device_cookie）。
    pub fn cookie(&self) -> Option<&str> {
        if !self.device_cookie.is_empty() {
            return Some(&self.device_cookie);
        }
        if !self.web_cookie.is_empty() {
            return Some(&self.web_cookie);
        }
        None
    }

    /// provider key，缺省 `115`。
    pub fn provider_key_or_default(&self) -> &str {
        if self.provider_key.is_empty() {
            "115"
        } else {
            &self.provider_key
        }
    }

    /// 从 `LibraryHandle.provider_config` 解析。
    pub fn from_provider_config(config: Option<&Struct>) -> Self {
        let Some(config) = config else {
            return Self::from_env();
        };
        let mut out = Self {
            web_cookie: struct_str(config, "web_cookie").unwrap_or_default(),
            device_cookie: struct_str(config, "device_cookie").unwrap_or_default(),
            media_root_path: struct_str(config, "media_root_path").unwrap_or_default(),
            downloads_root_path: struct_str(config, "downloads_root_path")
                .unwrap_or_default(),
            provider_key: struct_str(config, "provider_key").unwrap_or_default(),
        };
        // 环境变量兜底。
        if out.web_cookie.is_empty() {
            out.web_cookie = std::env::var("PLUGIN_115_WEB_COOKIE").unwrap_or_default();
        }
        if out.device_cookie.is_empty() {
            out.device_cookie = std::env::var("PLUGIN_115_DEVICE_COOKIE").unwrap_or_default();
        }
        if out.media_root_path.is_empty() {
            out.media_root_path =
                std::env::var("PLUGIN_115_MEDIA_ROOT").unwrap_or_default();
        }
        if out.downloads_root_path.is_empty() {
            out.downloads_root_path =
                std::env::var("PLUGIN_115_DOWNLOADS_ROOT").unwrap_or_default();
        }
        out
    }

    /// 从环境变量 / 宿主配置文件解析（无 provider_config 时）。
    pub fn from_env() -> Self {
        let mut out = Self {
            web_cookie: std::env::var("PLUGIN_115_WEB_COOKIE").unwrap_or_default(),
            device_cookie: std::env::var("PLUGIN_115_DEVICE_COOKIE").unwrap_or_default(),
            media_root_path: std::env::var("PLUGIN_115_MEDIA_ROOT").unwrap_or_default(),
            downloads_root_path: std::env::var("PLUGIN_115_DOWNLOADS_ROOT").unwrap_or_default(),
            provider_key: std::env::var("PLUGIN_115_PROVIDER_KEY").unwrap_or_default(),
        };
        // 宿主配置文件兜底。
        if let Ok(path) = std::env::var("SAKURAMEDIA_PLUGIN_SETTINGS_FILE") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(file) = serde_json::from_str::<serde_json::Value>(&text) {
                    let get = |key: &str| {
                        file.get(key)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned()
                    };
                    if out.web_cookie.is_empty() {
                        out.web_cookie = get("web_cookie");
                    }
                    if out.device_cookie.is_empty() {
                        out.device_cookie = get("device_cookie");
                    }
                    if out.media_root_path.is_empty() {
                        out.media_root_path = get("media_root_path");
                    }
                    if out.downloads_root_path.is_empty() {
                        out.downloads_root_path = get("downloads_root_path");
                    }
                    if out.provider_key.is_empty() {
                        out.provider_key = get("provider_key");
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_prefers_device_cookie() {
        let config = Plugin115Config {
            web_cookie: "web".to_owned(),
            device_cookie: "device".to_owned(),
            ..Default::default()
        };
        assert_eq!(config.cookie(), Some("device"));
    }

    #[test]
    fn cookie_falls_back_to_web_cookie() {
        let config = Plugin115Config {
            web_cookie: "web".to_owned(),
            ..Default::default()
        };
        assert_eq!(config.cookie(), Some("web"));
    }

    #[test]
    fn cookie_none_when_empty() {
        let config = Plugin115Config::default();
        assert_eq!(config.cookie(), None);
    }

    #[test]
    fn provider_key_defaults_to_115() {
        let config = Plugin115Config::default();
        assert_eq!(config.provider_key_or_default(), "115");
    }
}
