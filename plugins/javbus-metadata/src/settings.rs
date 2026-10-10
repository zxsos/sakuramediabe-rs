//! 插件配置。
//!
//! # 上游对应：`settings.py`
//!
//! 上游是 pydantic 的 `Settings`（`timeout_seconds: int = Field(default=20,
//! ge=1, le=120)`），值从 `PluginContext.settings` 来 —— 那是**进程内插件**的
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
//!    看门狗会按退避反复重拉 —— 为了一个「超时写了 200 秒」把插件打成反复重启
//!    不值得，所以夹到 1..120。
//! 2. **多一个 `base_url`**。上游把 `BASE_URL` 写死在 `javbus.py` 里
//!    （`https://www.javbus.com`）。这里要能打本地假服务（测试不许联网）也要能
//!    指向镜像站，所以提到配置里，默认仍是上游那个值，并在 `settings_schema`
//!    的 `hint` 里写明用途。

use std::path::Path;

use serde_json::Value;
use sm_plugin_api::v1::SettingsField;
use url::Url;

/// 宿主写好的配置文件（`docs/adr/2026-10-05-plugin-lifecycle.md`）。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 上游 `javbus.py` 里的 `BASE_URL`。
pub const DEFAULT_BASE_URL: &str = "https://www.javbus.com";

/// 超时的下界 / 上界 / 缺省（上游 `Field(default=20, ge=1, le=120)`）。
pub const TIMEOUT_MIN_SECONDS: u64 = 1;
pub const TIMEOUT_MAX_SECONDS: u64 = 120;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 20;

/// 插件配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 单次 HTTP 请求的超时（秒）。
    pub timeout_seconds: u64,
    /// 站点基址。`Url` 而非 `String`：详情页与图片都要按它做 `join`，解析一次
    /// 就能保证「拼出来的地址一定合法」。
    pub base_url: Url,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            base_url: parse_base(DEFAULT_BASE_URL).expect("缺省基址是常量，必能解析"),
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

    /// 只取用得到的两个键，其余忽略 —— 宿主版本比插件新时，文件里带未知键
    /// 是常态，不该让插件起不来。
    pub fn from_json(value: &Value) -> Self {
        let mut settings = Self::default();
        if let Some(seconds) = value.get("timeout_seconds").and_then(Value::as_u64) {
            settings.timeout_seconds = seconds.clamp(TIMEOUT_MIN_SECONDS, TIMEOUT_MAX_SECONDS);
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
            key: "timeout_seconds".to_owned(),
            label: "请求超时（秒）".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!(
                "{TIMEOUT_MIN_SECONDS} 到 {TIMEOUT_MAX_SECONDS}，默认 \
                 {DEFAULT_TIMEOUT_SECONDS}；越界按边界取值"
            )),
            multiline: false,
            hint: None,
        },
        SettingsField {
            key: "base_url".to_owned(),
            label: "站点基址".to_owned(),
            input: "text".to_owned(),
            required: false,
            description: Some(format!("默认 {DEFAULT_BASE_URL}")),
            multiline: false,
            // 这一条是对上游的偏离，写清楚它是给谁用的：上游把这个值写死在
            // 代码里，而测试不许联网、镜像站也要能指。
            hint: Some("供测试与镜像站使用".to_owned()),
        },
    ]
}

/// 解析基址。**结尾补一个 `/`**：详情页是 `base.join(番号)`、图片是
/// `base.join(href)`，两者都要求基址的 path 以 `/` 收尾才等价于上游的
/// `f"{BASE_URL}/{番号}"`。
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

    use std::sync::atomic::{AtomicU64, Ordering};

    /// 每次调用一个不重名的目录，测试之间互不干扰。
    fn scratch(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "plugin-javbus-settings-{tag}-{nanos}-{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn the_defaults_are_the_upstream_ones() {
        let settings = Settings::default();
        assert_eq!(settings.timeout_seconds, 20);
        assert_eq!(settings.base_url.as_str(), "https://www.javbus.com/");
    }

    #[test]
    fn a_timeout_outside_the_range_is_clamped_to_it() {
        // 上游是 pydantic 的 ge/le，越界即加载失败；这里夹到边界 ——
        // 注册期抛错会让看门狗反复重拉插件。
        let settings = Settings::from_json(&serde_json::json!({"timeout_seconds": 0}));
        assert_eq!(settings.timeout_seconds, TIMEOUT_MIN_SECONDS);
        let settings = Settings::from_json(&serde_json::json!({"timeout_seconds": 999}));
        assert_eq!(settings.timeout_seconds, TIMEOUT_MAX_SECONDS);
        let settings = Settings::from_json(&serde_json::json!({"timeout_seconds": 8}));
        assert_eq!(settings.timeout_seconds, 8);
    }

    #[test]
    fn a_non_integer_timeout_falls_back_to_the_default() {
        for bad in [
            serde_json::json!(null),
            serde_json::json!(-5),
            serde_json::json!("20"),
        ] {
            let value = serde_json::json!({"timeout_seconds": bad});
            assert_eq!(
                Settings::from_json(&value).timeout_seconds,
                DEFAULT_TIMEOUT_SECONDS,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_base_url_keeps_its_path_and_gets_a_trailing_slash() {
        let settings = Settings::from_json(&serde_json::json!({"base_url": "http://127.0.0.1:9"}));
        assert_eq!(settings.base_url.as_str(), "http://127.0.0.1:9/");
        // 带路径的基址：结尾补 `/`，`join` 才不会把最后一段吃掉。
        let settings =
            Settings::from_json(&serde_json::json!({"base_url": "https://mirror.example/javbus/"}));
        assert_eq!(settings.base_url.as_str(), "https://mirror.example/javbus/");
    }

    #[test]
    fn an_unusable_base_url_falls_back_to_the_default() {
        for bad in ["", "   ", "not a url"] {
            let settings = Settings::from_json(&serde_json::json!({"base_url": bad}));
            assert_eq!(
                settings.base_url.as_str(),
                "https://www.javbus.com/",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let settings =
            Settings::from_json(&serde_json::json!({"future_key": 1, "timeout_seconds": 3}));
        assert_eq!(settings.timeout_seconds, 3);
    }

    #[test]
    fn the_settings_file_is_read_when_the_host_wrote_one() {
        let dir = scratch("file");
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"timeout_seconds": 7}"#).expect("写配置");

        assert_eq!(Settings::from_file(&path).timeout_seconds, 7);
        // 文件不在 / 内容不是 JSON：按默认跑，而不是让插件起不来。
        assert_eq!(
            Settings::from_file(&dir.join("missing")).timeout_seconds,
            20
        );
        std::fs::write(&path, "not json").expect("写坏");
        assert_eq!(Settings::from_file(&path).timeout_seconds, 20);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_schema_declares_both_keys() {
        let fields = schema();
        let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["timeout_seconds", "base_url"]);
        // `input` 只能是 text / secret / path（proto 写在字段上）。
        assert!(fields.iter().all(|f| f.input == "text"));
        assert!(
            fields
                .iter()
                .any(|f| f.key == "base_url" && f.hint.as_deref() == Some("供测试与镜像站使用")),
            "偏离上游的那一项要把用途写在 hint 里"
        );
    }
}
