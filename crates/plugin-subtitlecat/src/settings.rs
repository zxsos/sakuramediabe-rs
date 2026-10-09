//! 插件配置。
//!
//! # 上游对应：`settings.py`
//!
//! 上游是 pydantic 的 `SubtitleCatSettings`：
//! - `request_timeout_seconds: float = Field(default=20.0, gt=0, le=120)`
//! - `request_retries: int = Field(default=2, ge=0, le=3)`
//! - `release_age_months: int = Field(default=3, ge=1, le=120)`
//!
//! 值从宿主写好的配置文件读（`SAKURAMEDIA_PLUGIN_SETTINGS_FILE`），
//! 不是 `context.settings` —— 进程内插件才有后者那条通道。
//!
//! # 两处对上游的偏离
//!
//! 1. **越界值按边界取值，不报错**。上游 pydantic 越界是 `ValidationError` →
//!    插件加载失败。在 gRPC 插件上，注册期抛错的表现是「进程起来又被判不合规」。
//! 2. **多一个 `base_url`**。上游把 `SUBTITLECAT_BASE_URL` 写死在
//!    `subtitlecat.py` 里（`https://subtitlecat.com/`）。这里要能打本地假服务
//!    （测试不许联网），所以提到配置里，默认仍是上游那个值。

use std::path::Path;

use serde_json::Value;
use sm_plugin_api::v1::SettingsField;
use url::Url;

/// 宿主写好的配置文件（`docs/adr/2026-10-05-plugin-lifecycle.md`）。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 上游 `subtitlecat.py` 里的 `SUBTITLECAT_BASE_URL`。
pub const DEFAULT_BASE_URL: &str = "https://subtitlecat.com/";

/// 超时的下界 / 上界 / 缺省（上游 `gt=0, le=120, default=20.0`）。
pub const TIMEOUT_MIN_SECONDS: f64 = 0.0;
pub const TIMEOUT_MAX_SECONDS: f64 = 120.0;
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 20.0;

/// 重试次数的下界 / 上界 / 缺省（上游 `ge=0, le=3, default=2`）。
pub const RETRIES_MIN: u32 = 0;
pub const RETRIES_MAX: u32 = 3;
pub const DEFAULT_RETRIES: u32 = 2;

/// 月龄上限的下界 / 上界 / 缺省（上游 `ge=1, le=120, default=3`）。
pub const AGE_MIN_MONTHS: u32 = 1;
pub const AGE_MAX_MONTHS: u32 = 120;
pub const DEFAULT_AGE_MONTHS: u32 = 3;

/// 插件配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// 单次 HTTP 请求的超时（秒）。
    pub request_timeout_seconds: f64,
    /// 请求重试次数。
    pub request_retries: u32,
    /// 定时补抓影片月龄上限。
    pub release_age_months: u32,
    /// 站点基址。
    pub base_url: Url,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            request_timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            request_retries: DEFAULT_RETRIES,
            release_age_months: DEFAULT_AGE_MONTHS,
            base_url: parse_base(DEFAULT_BASE_URL).expect("缺省基址是常量，必能解析"),
        }
    }
}

impl Settings {
    /// 读宿主写好的配置文件。
    ///
    /// **任何一个环节出问题都用默认值**，不报出去。
    pub fn load() -> Self {
        let Ok(path) = std::env::var(SETTINGS_FILE_ENV) else {
            return Self::default();
        };
        Self::from_file(Path::new(&path))
    }

    /// 从一个配置文件读。拆出来是为了能单测。
    pub fn from_file(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        Self::from_json(&value)
    }

    /// 只取用得到的键，其余忽略。
    pub fn from_json(value: &Value) -> Self {
        let mut settings = Self::default();
        if let Some(seconds) = value.get("request_timeout_seconds").and_then(Value::as_f64) {
            settings.request_timeout_seconds =
                seconds.clamp(TIMEOUT_MIN_SECONDS, TIMEOUT_MAX_SECONDS);
            // gt=0：0 按下界处理，上游是 ValidationError，这里夹到最小正值。
            if settings.request_timeout_seconds <= 0.0 {
                settings.request_timeout_seconds = f64::MIN_POSITIVE;
            }
        }
        if let Some(retries) = value.get("request_retries").and_then(Value::as_u64) {
            settings.request_retries = (retries as u32).clamp(RETRIES_MIN, RETRIES_MAX);
        }
        if let Some(months) = value.get("release_age_months").and_then(Value::as_u64) {
            settings.release_age_months = (months as u32).clamp(AGE_MIN_MONTHS, AGE_MAX_MONTHS);
        }
        if let Some(raw) = value.get("base_url").and_then(Value::as_str) {
            if let Some(base) = parse_base(raw) {
                settings.base_url = base;
            }
        }
        settings
    }
}

/// 给宿主渲染配置表单用的 schema（`RegisterResponse.settings_schema`）。
pub fn schema() -> Vec<SettingsField> {
    vec![
        SettingsField {
            key: "request_timeout_seconds".to_owned(),
            label: "请求超时（秒）".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!(
                "大于 0，到 {TIMEOUT_MAX_SECONDS}，默认 {DEFAULT_TIMEOUT_SECONDS}；越界按边界取值"
            )),
            multiline: false,
            hint: None,
            default: None,
        },
        SettingsField {
            key: "request_retries".to_owned(),
            label: "请求重试次数".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!(
                "{RETRIES_MIN} 到 {RETRIES_MAX}，默认 {DEFAULT_RETRIES}"
            )),
            multiline: false,
            hint: None,
            default: None,
        },
        SettingsField {
            key: "release_age_months".to_owned(),
            label: "定时补抓影片月龄上限".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!(
                "{AGE_MIN_MONTHS} 到 {AGE_MAX_MONTHS}，默认 {DEFAULT_AGE_MONTHS}；已抓过的影片发布时间超过此月数后不再重复抓取"
            )),
            multiline: false,
            hint: None,
            default: None,
        },
        SettingsField {
            key: "base_url".to_owned(),
            label: "站点基址".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!("默认 {DEFAULT_BASE_URL}")),
            multiline: false,
            hint: Some("供测试与镜像站使用".to_owned()),
            default: None,
        },
    ]
}

/// 解析基址。**结尾补一个 `/`**。
fn parse_base(raw: &str) -> Option<Url> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Url::parse(&format!("{trimmed}/")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_upstream_ones() {
        let settings = Settings::default();
        assert_eq!(settings.request_timeout_seconds, 20.0);
        assert_eq!(settings.request_retries, 2);
        assert_eq!(settings.release_age_months, 3);
        assert_eq!(settings.base_url.as_str(), "https://subtitlecat.com/");
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings = Settings::from_json(&serde_json::json!({"request_timeout_seconds": 999.0}));
        assert_eq!(settings.request_timeout_seconds, TIMEOUT_MAX_SECONDS);
        let settings = Settings::from_json(&serde_json::json!({"request_retries": 99}));
        assert_eq!(settings.request_retries, RETRIES_MAX);
        let settings = Settings::from_json(&serde_json::json!({"release_age_months": 0}));
        assert_eq!(settings.release_age_months, AGE_MIN_MONTHS);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let settings =
            Settings::from_json(&serde_json::json!({"future_key": 1, "request_retries": 1}));
        assert_eq!(settings.request_retries, 1);
    }

    #[test]
    fn the_schema_declares_all_keys() {
        let fields = schema();
        let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "request_timeout_seconds",
                "request_retries",
                "release_age_months",
                "base_url"
            ]
        );
    }
}
