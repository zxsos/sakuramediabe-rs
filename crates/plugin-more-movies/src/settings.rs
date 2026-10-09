//! 插件配置。
//!
//! # 上游对应
//!
//! - `sakuramedia_more_movies/settings.py:MoreMoviesSettings`
//! - `sakuramedia_more_rank_movies` 没有 settings.py（榜单插件无私有配置）
//!
//! # 读取方式
//!
//! 与 `plugin-javbus-metadata` 同一理由：配置走「宿主写文件 + 环境变量指路」
//! （`SAKURAMEDIA_PLUGIN_SETTINGS_FILE`），插件只读不写回。文件缺失或解析失败
//! 时按默认值跑 —— 注册期抛错的表现是「进程起来又被判不合规」，看门狗会按退避
//! 反复重拉，为了一个配错的值把插件打成反复重启不值得。
//!
//! # 越界值的处理
//!
//! 与 javbus 插件一致：越界按边界取值，不报错。

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use sm_plugin_api::v1::SettingsField;

use crate::javdb::DEFAULT_API_HOST;

/// 宿主写好的配置文件（`docs/adr/2026-10-05-plugin-lifecycle.md`）。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 更多影片插件的配置（上游 `MoreMoviesSettings`）。
#[derive(Debug, Clone)]
pub struct MoreMoviesSettings {
    /// 翻页间隔（上游 `page_delay_ms`，默认 350）。
    pub page_delay: Duration,
    /// 详情请求间隔（上游 `detail_delay_ms`，默认 0）。
    pub detail_delay: Duration,
    /// 入库热度阈值（上游 `min_heat`，默认 100，0 表示不过滤）。
    pub min_heat: u64,
    /// JavDB API host（上游写死在 provider 里，这里提到配置，默认上游值）。
    pub javdb_api_host: String,
    /// HTTP 超时。
    pub timeout: Duration,
}

impl Default for MoreMoviesSettings {
    fn default() -> Self {
        Self {
            page_delay: Duration::from_millis(350),
            detail_delay: Duration::ZERO,
            min_heat: 100,
            javdb_api_host: DEFAULT_API_HOST.to_owned(),
            timeout: Duration::from_secs(20),
        }
    }
}

impl MoreMoviesSettings {
    /// 从宿主写好的文件加载；缺失/解析失败时返回默认值。
    pub fn load() -> Self {
        let path = std::env::var(SETTINGS_FILE_ENV).unwrap_or_default();
        if path.is_empty() {
            return Self::default();
        }
        let text = std::fs::read_to_string(Path::new(&path)).unwrap_or_default();
        if text.is_empty() {
            return Self::default();
        }
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Self::from_value(&value)
    }

    fn from_value(v: &Value) -> Self {
        let mut s = Self::default();
        let get_u64 = |key: &str| v.get(key).and_then(Value::as_u64);
        if let Some(ms) = get_u64("page_delay_ms") {
            s.page_delay = Duration::from_millis(ms);
        }
        if let Some(ms) = get_u64("detail_delay_ms") {
            s.detail_delay = Duration::from_millis(ms);
        }
        if let Some(heat) = get_u64("min_heat") {
            s.min_heat = heat;
        }
        if let Some(host) = v.get("javdb_api_host").and_then(Value::as_str) {
            let host = host
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            if !host.is_empty() {
                s.javdb_api_host = host.to_owned();
            }
        }
        if let Some(secs) = get_u64("timeout_seconds") {
            // 上游 clamp 到 1..=120 的手法（见 javbus 插件 settings.rs）
            s.timeout = Duration::from_secs(secs.clamp(1, 120));
        }
        s
    }

    /// 宿主渲染配置表单的 schema（v0.2.0 的 `SettingsField` 字段）。
    pub fn schema() -> Vec<SettingsField> {
        vec![
            SettingsField {
                key: "page_delay_ms".to_owned(),
                label: "翻页间隔（毫秒）".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("JavDB 最新列表翻页之间的等待".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
            SettingsField {
                key: "detail_delay_ms".to_owned(),
                label: "详情请求间隔（毫秒）".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("每个影片详情请求之间的等待，0 表示不等待".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
            SettingsField {
                key: "min_heat".to_owned(),
                label: "入库热度阈值".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("热度低于该值的影片不入库，0 表示不过滤".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
            SettingsField {
                key: "javdb_api_host".to_owned(),
                label: "JavDB API 地址".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("默认 api.javdb.com，可指向镜像站（不用写 https://）".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
        ]
    }
}

/// 榜单插件的配置（上游无私有配置；只有 HTTP 超时与请求间隔可调）。
#[derive(Debug, Clone)]
pub struct RankMoviesSettings {
    pub timeout: Duration,
    /// 站点请求间隔（上游两家客户端默认都是 1 秒）。
    pub request_interval: Duration,
}

impl Default for RankMoviesSettings {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            request_interval: Duration::from_secs(1),
        }
    }
}

impl RankMoviesSettings {
    pub fn load() -> Self {
        let path = std::env::var(SETTINGS_FILE_ENV).unwrap_or_default();
        if path.is_empty() {
            return Self::default();
        }
        let text = std::fs::read_to_string(Path::new(&path)).unwrap_or_default();
        if text.is_empty() {
            return Self::default();
        }
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let mut s = Self::default();
        if let Some(secs) = value.get("timeout_seconds").and_then(Value::as_u64) {
            s.timeout = Duration::from_secs(secs.clamp(1, 120));
        }
        if let Some(secs) = value
            .get("request_interval_seconds")
            .and_then(Value::as_u64)
        {
            s.request_interval = Duration::from_secs(secs);
        }
        s
    }

    pub fn schema() -> Vec<SettingsField> {
        vec![
            SettingsField {
                key: "timeout_seconds".to_owned(),
                label: "请求超时（秒）".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("榜单页面与详情页的 HTTP 超时".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
            SettingsField {
                key: "request_interval_seconds".to_owned(),
                label: "请求间隔（秒）".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("对榜单站点的连续请求间隔，避免被限流".to_owned()),
                multiline: false,
                hint: None,
                default: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let s = MoreMoviesSettings::default();
        assert_eq!(s.page_delay, Duration::from_millis(350));
        assert_eq!(s.detail_delay, Duration::ZERO);
        assert_eq!(s.min_heat, 100);
    }

    #[test]
    fn missing_file_gives_defaults() {
        std::env::remove_var(SETTINGS_FILE_ENV);
        let s = MoreMoviesSettings::load();
        assert_eq!(s.min_heat, 100);
    }

    #[test]
    fn timeout_clamped() {
        let s = MoreMoviesSettings::from_value(&serde_json::json!({"timeout_seconds": 500}));
        assert_eq!(s.timeout, Duration::from_secs(120));
    }
}
