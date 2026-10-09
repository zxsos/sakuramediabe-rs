//! 插件配置。
//!
//! # 上游对应：`settings.py`
//!
//! 上游是 pydantic 的 `Settings`（`retry_days: int = Field(default=3, ge=1,
//! le=365)` 等），值从 `PluginContext.settings` 来 —— 那是**进程内插件**的
//! 通道：宿主把配置对象直接交给 `register()`。
//!
//! 拆成进程后没有这条通道了（`RegisterRequest` 只有 `plugin_id` 与
//! `abi_major`），于是配置走「宿主写文件 + 环境变量指路」
//! （`docs/adr/2026-10-05-plugin-lifecycle.md` 第 3 节）：宿主每次拉起**重写**
//! `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`，插件只读，**不写回**。
//!
//! # 两处对上游的偏离
//!
//! 1. **越界值按边界取值，不报错**。上游 pydantic 越界是 `ValidationError` →
//!    插件加载失败。在 gRPC 插件上，注册期抛错的表现是「进程起来又被判不合规」，
//!    看门狗会按退避反复重拉 —— 为了一个「重试间隔写了 400 天」把插件打成反复
//!    重启不值得，所以夹到合法区间。
//! 2. **多 `javdb_base_url` / `minnanoav_base_url`**。上游把站点基址写死在
//!    `sources.py` 里（`JAVDB` / `MINNANOAV` 常量）。这里要能打本地假服务
//!    （测试不许联网），所以提到配置里，默认仍是上游那两个值，并在
//!    `settings_schema` 的 `hint` 里写明用途。

use std::path::Path;

use serde_json::Value;
use sm_plugin_api::v1::SettingsField;
use url::Url;

/// 宿主写好的配置文件（`docs/adr/2026-10-05-plugin-lifecycle.md`）。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 上游 `sources.py` 里的 `JAVDB`。
pub const DEFAULT_JAVDB_BASE_URL: &str = "https://jdforrepam.com";
/// 上游 `sources.py` 里的 `MINNANOAV`。
pub const DEFAULT_MINNANOAV_BASE_URL: &str = "https://www.minnano-av.com";

/// `retry_days` 的下界 / 上界 / 缺省（上游 `Field(default=3, ge=1, le=365)`）。
pub const RETRY_DAYS_MIN: u64 = 1;
pub const RETRY_DAYS_MAX: u64 = 365;
pub const DEFAULT_RETRY_DAYS: u64 = 3;
/// `max_attempts` 的下界 / 上界 / 缺省（上游 `Field(default=3, ge=1, le=100)`）。
pub const MAX_ATTEMPTS_MIN: u64 = 1;
pub const MAX_ATTEMPTS_MAX: u64 = 100;
pub const DEFAULT_MAX_ATTEMPTS: u64 = 3;
/// `request_interval_seconds` 的下界 / 上界 / 缺省
/// （上游 `Field(default=1, ge=0.2, le=60)`）。
pub const REQUEST_INTERVAL_MIN: f64 = 0.2;
pub const REQUEST_INTERVAL_MAX: f64 = 60.0;
pub const DEFAULT_REQUEST_INTERVAL_SECONDS: f64 = 1.0;
/// `timeout_seconds` 的下界 / 上界 / 缺省（上游 `Field(default=20, ge=1, le=120)`）。
pub const TIMEOUT_MIN_SECONDS: u64 = 1;
pub const TIMEOUT_MAX_SECONDS: u64 = 120;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 20;

/// 插件配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// 失败后隔多少天重试（上游 `retry_days`）。
    pub retry_days: u64,
    /// 每位演员最多尝试次数（上游 `max_attempts`）。
    pub max_attempts: u64,
    /// 只处理已订阅演员（上游 `subscribed_only`）。
    pub subscribed_only: bool,
    /// 站点请求间隔（秒，上游 `request_interval_seconds`）。
    pub request_interval_seconds: f64,
    /// 单次 HTTP 请求超时（秒，上游 `timeout_seconds`）。
    pub timeout_seconds: u64,
    /// JavDB 基址。`Url` 而非 `String`：API 路径要按它做 `join`。
    pub javdb_base_url: Url,
    /// MinnanoAV 基址。搜索与资料页都按它拼。
    pub minnanoav_base_url: Url,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            retry_days: DEFAULT_RETRY_DAYS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            subscribed_only: false,
            request_interval_seconds: DEFAULT_REQUEST_INTERVAL_SECONDS,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            javdb_base_url: parse_base(DEFAULT_JAVDB_BASE_URL).expect("缺省基址是常量，必能解析"),
            minnanoav_base_url: parse_base(DEFAULT_MINNANOAV_BASE_URL)
                .expect("缺省基址是常量，必能解析"),
        }
    }
}

impl Settings {
    /// 读宿主写好的配置文件。
    ///
    /// **任何一个环节出问题都用默认值**，不报出去：配置是可选的（上游
    /// `context.settings` 为空也是照常注册），而「文件坏了」在这条链路上没有
    /// 比「按默认跑」更好的处置 —— 宿主并没有重写的时机。
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

    /// 只取认得的键，其余忽略 —— 宿主版本比插件新时，文件里带未知键是常态，
    /// 不该让插件起不来。
    pub fn from_json(value: &Value) -> Self {
        let mut settings = Self::default();
        if let Some(days) = value.get("retry_days").and_then(Value::as_u64) {
            settings.retry_days = days.clamp(RETRY_DAYS_MIN, RETRY_DAYS_MAX);
        }
        if let Some(attempts) = value.get("max_attempts").and_then(Value::as_u64) {
            settings.max_attempts = attempts.clamp(MAX_ATTEMPTS_MIN, MAX_ATTEMPTS_MAX);
        }
        if let Some(only) = value.get("subscribed_only").and_then(Value::as_bool) {
            settings.subscribed_only = only;
        }
        if let Some(interval) = value
            .get("request_interval_seconds")
            .and_then(Value::as_f64)
        {
            settings.request_interval_seconds =
                interval.clamp(REQUEST_INTERVAL_MIN, REQUEST_INTERVAL_MAX);
        }
        if let Some(seconds) = value.get("timeout_seconds").and_then(Value::as_u64) {
            settings.timeout_seconds = seconds.clamp(TIMEOUT_MIN_SECONDS, TIMEOUT_MAX_SECONDS);
        }
        if let Some(raw) = value.get("javdb_base_url").and_then(Value::as_str) {
            if let Some(base) = parse_base(raw) {
                settings.javdb_base_url = base;
            }
        }
        if let Some(raw) = value.get("minnanoav_base_url").and_then(Value::as_str) {
            if let Some(base) = parse_base(raw) {
                settings.minnanoav_base_url = base;
            }
        }
        // 上游已退役的两个键：静默丢掉（`settings.py` 的 `remove_retired_config`）。
        settings
    }
}

/// 基址必须带 `/` 结尾：`Url::join` 在基址不以 `/` 结尾时会把最后一截当文件
/// 名吃掉（上游 `urljoin` 同理）。
fn parse_base(raw: &str) -> Option<Url> {
    let mut url = Url::parse(raw).ok()?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Some(url)
}

/// 宿主渲染配置表单用的 schema（`RegisterResponse.settings_schema`）。
pub fn schema() -> Vec<SettingsField> {
    vec![
        field(
            "retry_days",
            "重试间隔（天）",
            Some("失败后隔多少天再试一次"),
            Some(&DEFAULT_RETRY_DAYS.to_string()),
        ),
        field(
            "max_attempts",
            "最大尝试次数",
            Some("每位演员最多尝试多少次，耗尽后不再访问来源"),
            Some(&DEFAULT_MAX_ATTEMPTS.to_string()),
        ),
        field("subscribed_only", "仅处理已订阅女优", None, Some("false")),
        field(
            "request_interval_seconds",
            "请求间隔（秒）",
            Some("对 JavDB / MinnanoAV 的请求间隔，避免被限流"),
            Some(&DEFAULT_REQUEST_INTERVAL_SECONDS.to_string()),
        ),
        field(
            "timeout_seconds",
            "请求超时（秒）",
            None,
            Some(&DEFAULT_TIMEOUT_SECONDS.to_string()),
        ),
        field(
            "javdb_base_url",
            "JavDB 基址",
            Some("测试或镜像站时改；默认是上游的 jdforrepam.com"),
            Some(DEFAULT_JAVDB_BASE_URL),
        ),
        field(
            "minnanoav_base_url",
            "MinnanoAV 基址",
            Some("测试时指向本地假服务"),
            Some(DEFAULT_MINNANOAV_BASE_URL),
        ),
    ]
}

fn field(
    key: &str,
    label: &str,
    description: Option<&str>,
    _default: Option<&str>,
) -> SettingsField {
    // v0.2.0 的 SettingsField 还没有 `default` 字段（tag 8 是后加的），
    // 默认值写进 description。
    SettingsField {
        key: key.to_owned(),
        label: label.to_owned(),
        input: "text".to_owned(),
        required: false,
        description: description.map(str::to_owned),
        multiline: false,
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let settings = Settings::from_json(&json!({
            "retry_days": 400,
            "max_attempts": 0,
            "request_interval_seconds": 0.01,
            "timeout_seconds": 200,
        }));
        assert_eq!(settings.retry_days, RETRY_DAYS_MAX);
        assert_eq!(settings.max_attempts, MAX_ATTEMPTS_MIN);
        assert_eq!(settings.request_interval_seconds, REQUEST_INTERVAL_MIN);
        assert_eq!(settings.timeout_seconds, TIMEOUT_MAX_SECONDS);
    }

    #[test]
    fn retired_keys_are_silently_dropped() {
        // 上游 `remove_retired_config` 丢掉的两个键。
        let settings = Settings::from_json(&json!({
            "javdb_mapping": {"a": "b"},
            "target_fields": ["birthday"],
            "retry_days": 5,
        }));
        assert_eq!(settings.retry_days, 5);
    }

    #[test]
    fn invalid_base_url_keeps_the_default() {
        let settings = Settings::from_json(&json!({"javdb_base_url": "not a url"}));
        assert_eq!(
            settings.javdb_base_url.as_str(),
            DEFAULT_JAVDB_BASE_URL.to_owned() + "/"
        );
    }

    #[test]
    fn base_url_always_ends_with_a_slash() {
        assert!(parse_base("https://example.com/api")
            .unwrap()
            .as_str()
            .ends_with('/'));
    }
}
