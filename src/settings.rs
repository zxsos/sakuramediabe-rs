//! 插件配置。
//!
//! # 上游对应：`settings.py` 的 `DmmSettings`
//!
//! 上游是 pydantic 模型，值从 `PluginContext.settings` 来 —— 那是**进程内插件**
//! 的通道：宿主把配置对象直接交给 `register()`。
//!
//! 拆成进程后没有这条通道了（`RegisterRequest` 只有 `plugin_id` 与
//! `abi_major`），于是配置走「宿主写文件 + 环境变量指路」：宿主每次拉起
//! **重写** `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`，插件只读，**不写回**。
//!
//! # 对上游的偏离
//!
//! **越界值按边界取值，不报错**。上游 pydantic 越界是 `ValidationError` →
//! 插件加载失败。在 gRPC 插件上，注册期抛错的表现是「进程起来又被判不合规」
//! —— 为了一个「超时写了 200 秒」把插件打成反复重启不值得，所以夹到合法区间。

use std::path::Path;

use serde_json::Value;
use sm_plugin_api::v1::SettingsField;

/// 宿主写好的配置文件。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 请求超时的下界 / 上界 / 缺省（上游 `Field(default=20.0, gt=0, le=120)`）。
pub const REQUEST_TIMEOUT_MIN: f64 = 0.1;
pub const REQUEST_TIMEOUT_MAX: f64 = 120.0;
pub const DEFAULT_REQUEST_TIMEOUT: f64 = 20.0;

/// 请求间隔的下界 / 上界 / 缺省（上游 `Field(default=1.0, ge=0, le=60)`）。
pub const REQUEST_INTERVAL_MIN: f64 = 0.0;
pub const REQUEST_INTERVAL_MAX: f64 = 60.0;
pub const DEFAULT_REQUEST_INTERVAL: f64 = 1.0;

/// 翻译超时的下界 / 上界 / 缺省（上游 `Field(default=60.0, gt=0, le=300)`）。
pub const TRANSLATION_TIMEOUT_MIN: f64 = 0.1;
pub const TRANSLATION_TIMEOUT_MAX: f64 = 300.0;
pub const DEFAULT_TRANSLATION_TIMEOUT: f64 = 60.0;

/// API 类型（上游 `Literal["chat_completions", "responses"]`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiType {
    #[default]
    ChatCompletions,
    Responses,
}

impl ApiType {
    fn from_str(raw: &str) -> Self {
        match raw.trim() {
            "responses" => Self::Responses,
            _ => Self::ChatCompletions,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

/// 插件配置（上游 `DmmSettings`）。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 单次 DMM 请求的超时（秒）。
    pub request_timeout_seconds: f64,
    /// DMM 请求之间的最小间隔（秒），礼貌爬取。
    pub request_interval_seconds: f64,
    /// 是否启用翻译。
    pub translation_enabled: bool,
    /// 翻译服务基址（OpenAI 兼容，不带 `/v1` 也行，构造客户端时补）。
    pub base_url: String,
    /// 翻译服务 API 密钥。
    pub api_key: String,
    /// 翻译模型名。
    pub model: String,
    /// 翻译 API 类型。
    pub api_type: ApiType,
    /// 单次翻译请求的超时（秒）。
    pub translation_timeout_seconds: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT,
            request_interval_seconds: DEFAULT_REQUEST_INTERVAL,
            translation_enabled: false,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            api_type: ApiType::ChatCompletions,
            translation_timeout_seconds: DEFAULT_TRANSLATION_TIMEOUT,
        }
    }
}

impl Settings {
    /// 读宿主写好的配置文件。
    ///
    /// **任何一个环节出问题都用默认值**，不报出去：配置是可选的，而「文件坏
    /// 了」在这条链路上没有比「按默认跑」更好的处置。
    pub fn load() -> Self {
        let Ok(path) = std::env::var(SETTINGS_FILE_ENV) else {
            return Self::default();
        };
        Self::from_file(Path::new(&path))
    }

    /// 从一个配置文件读。拆出来是为了能单测（环境变量是进程级的，单测改它
    /// 会互相干扰）。
    pub fn from_file(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        Self::from_json(&value)
    }

    /// 只取认识的键，其余忽略。越界数值夹到合法区间；`translation_enabled`
    /// 为真但缺 `base_url`/`model` 时**保持启用**（上游此时是加载失败；这里
    /// 让任务在运行时报配置错误，而不是让整个插件起不来）。
    pub fn from_json(value: &Value) -> Self {
        let mut s = Self::default();
        if let Some(v) = value.get("request_timeout_seconds").and_then(Value::as_f64) {
            s.request_timeout_seconds = v.clamp(REQUEST_TIMEOUT_MIN, REQUEST_TIMEOUT_MAX);
        }
        if let Some(v) = value
            .get("request_interval_seconds")
            .and_then(Value::as_f64)
        {
            s.request_interval_seconds = v.clamp(REQUEST_INTERVAL_MIN, REQUEST_INTERVAL_MAX);
        }
        if let Some(v) = value.get("translation_enabled").and_then(Value::as_bool) {
            s.translation_enabled = v;
        }
        if let Some(v) = value.get("base_url").and_then(Value::as_str) {
            s.base_url = v.trim().to_owned();
        }
        if let Some(v) = value.get("api_key").and_then(Value::as_str) {
            s.api_key = v.trim().to_owned();
        }
        if let Some(v) = value.get("model").and_then(Value::as_str) {
            s.model = v.trim().to_owned();
        }
        if let Some(v) = value.get("api_type").and_then(Value::as_str) {
            s.api_type = ApiType::from_str(v);
        }
        if let Some(v) = value
            .get("translation_timeout_seconds")
            .and_then(Value::as_f64)
        {
            s.translation_timeout_seconds =
                v.clamp(TRANSLATION_TIMEOUT_MIN, TRANSLATION_TIMEOUT_MAX);
        }
        s
    }

    /// 归一化后的翻译服务基址：保证以 `/v1` 结尾（上游 `translation.py` 的逻辑）。
    pub fn translation_base_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.is_empty() {
            return String::new();
        }
        if base.ends_with("/v1") {
            base.to_owned()
        } else {
            format!("{base}/v1")
        }
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
                "{REQUEST_TIMEOUT_MIN} 到 {REQUEST_TIMEOUT_MAX}，默认 {DEFAULT_REQUEST_TIMEOUT}"
            )),
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "request_interval_seconds".to_owned(),
            label: "请求间隔（秒）".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!(
                "{REQUEST_INTERVAL_MIN} 到 {REQUEST_INTERVAL_MAX}，默认 {DEFAULT_REQUEST_INTERVAL}"
            )),
            multiline: false,
            hint: Some("礼貌爬取，避免被 DMM 限流".to_owned()),
        },
        SettingsField {
            key: "translation_enabled".to_owned(),
            label: "启用翻译".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some("true/false".to_owned()),
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "base_url".to_owned(),
            label: "翻译服务地址".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some("OpenAI 兼容服务的根地址".to_owned()),
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "api_key".to_owned(),
            label: "API 密钥".to_owned(),
            input: "secret".to_owned(),
            required: false,
            description: None,
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "model".to_owned(),
            label: "翻译模型".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: None,
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "api_type".to_owned(),
            label: "API 类型".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some("chat_completions 或 responses".to_owned()),
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "translation_timeout_seconds".to_owned(),
            label: "翻译超时（秒）".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!(
                "{TRANSLATION_TIMEOUT_MIN} 到 {TRANSLATION_TIMEOUT_MAX}，默认 {DEFAULT_TRANSLATION_TIMEOUT}"
            )),
            multiline: false,
            hint: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_match_upstream() {
        let s = Settings::default();
        assert_eq!(s.request_timeout_seconds, 20.0);
        assert_eq!(s.request_interval_seconds, 1.0);
        assert!(!s.translation_enabled);
        assert_eq!(s.translation_timeout_seconds, 60.0);
        assert_eq!(s.api_type, ApiType::ChatCompletions);
    }

    #[test]
    fn out_of_range_numbers_are_clamped() {
        let s = Settings::from_json(&serde_json::json!({"request_timeout_seconds": 999.0}));
        assert_eq!(s.request_timeout_seconds, REQUEST_TIMEOUT_MAX);
        let s = Settings::from_json(&serde_json::json!({"request_timeout_seconds": -1.0}));
        assert_eq!(s.request_timeout_seconds, REQUEST_TIMEOUT_MIN);
        let s = Settings::from_json(&serde_json::json!({"request_interval_seconds": 100.0}));
        assert_eq!(s.request_interval_seconds, REQUEST_INTERVAL_MAX);
    }

    #[test]
    fn the_translation_base_url_gets_a_v1_suffix() {
        let s = Settings {
            base_url: "https://api.example.com".to_owned(),
            ..Default::default()
        };
        assert_eq!(s.translation_base_url(), "https://api.example.com/v1");
        let s = Settings {
            base_url: "https://api.example.com/v1".to_owned(),
            ..Default::default()
        };
        assert_eq!(s.translation_base_url(), "https://api.example.com/v1");
        s.base_url = "https://api.example.com/v1/".to_owned();
        assert_eq!(s.translation_base_url(), "https://api.example.com/v1");
    }

    #[test]
    fn api_type_parsing() {
        assert_eq!(ApiType::from_str("responses"), ApiType::Responses);
        assert_eq!(
            ApiType::from_str("chat_completions"),
            ApiType::ChatCompletions
        );
        assert_eq!(ApiType::from_str("bogus"), ApiType::ChatCompletions);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let s = Settings::from_json(&serde_json::json!({
            "future_key": 1,
            "request_timeout_seconds": 7.0,
        }));
        assert_eq!(s.request_timeout_seconds, 7.0);
    }

    #[test]
    fn the_schema_covers_all_keys() {
        let fields = schema();
        let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "request_timeout_seconds",
                "request_interval_seconds",
                "translation_enabled",
                "base_url",
                "api_key",
                "model",
                "api_type",
                "translation_timeout_seconds",
            ]
        );
        // api_key 必须是 secret 输入。
        let key_field = schema()
            .into_iter()
            .find(|f| f.key == "api_key")
            .expect("api_key");
        assert_eq!(key_field.input, "secret");
    }
}
