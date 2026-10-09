//! 插件配置：从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读。
//!
//! # 为什么是「文件可选、字段有默认」
//!
//! 宿主在**每次拉起**时重写这个文件；插件只在启动时读一次。文件不存在
//! （宿主没配过）就按默认跑 —— 把「没配」写成启动失败会让插件在宿主的
//! 探活里直接判死，而默认的 javdb.com 对公开榜单本来就能匿名访问。

use std::path::PathBuf;

/// 宿主注入的配置文件路径。
const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 插件配置。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// JavDB 站点基址。上游默认 `https://javdb.com`。
    pub base_url: String,
    /// 请求超时（秒）。
    pub timeout_secs: u64,
    /// TOP250 是否需要账号。没配账号时跳过 TOP250 抓取（上游同行为）。
    pub top250_require_auth: bool,
    /// JavDB 登录后的 Cookie（如 `_javdb_session=xxx`），用于绕过反爬和访问 TOP250。
    pub cookie: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: "https://javdb.com".to_owned(),
            timeout_secs: 30,
            top250_require_auth: true,
            cookie: String::new(),
        }
    }
}

impl Settings {
    /// 从环境变量指定的文件加载；没有就用默认。
    pub fn load() -> Self {
        let path = std::env::var(SETTINGS_FILE_ENV).ok().map(PathBuf::from);
        let path = match path {
            Some(p) if p.is_file() => p,
            _ => return Self::default(),
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// settings 表单的字段列表（宿主渲染用）。
    pub fn schema() -> Vec<sm_plugin_api::v1::SettingsField> {
        use sm_plugin_api::v1::SettingsField;
        vec![
            SettingsField {
                key: "base_url".to_owned(),
                label: "JavDB 基址".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("默认 https://javdb.com".to_owned()),
                multiline: false,
                hint: Some("供测试与镜像站使用".to_owned()),
                default: Some("https://javdb.com".to_owned()),
            },
            SettingsField {
                key: "timeout_secs".to_owned(),
                label: "请求超时（秒）".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("5 到 120，默认 30".to_owned()),
                multiline: false,
                hint: None,
                default: Some("30".to_owned()),
            },
            SettingsField {
                key: "cookie".to_owned(),
                label: "JavDB Cookie".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("登录后的 Cookie，用于绕过反爬和访问 TOP250".to_owned()),
                multiline: true,
                hint: Some("如 _javdb_session=xxx；从浏览器开发者工具复制".to_owned()),
                default: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url_is_javdb() {
        assert_eq!(Settings::default().base_url, "https://javdb.com");
    }

    #[test]
    fn schema_has_three_fields() {
        let fields = Settings::schema();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].key, "base_url");
        assert_eq!(fields[2].key, "cookie");
    }
}
