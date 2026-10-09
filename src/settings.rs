//! 插件配置。
//!
//! # 上游对应：`settings.py`
//!
//! 上游是 pydantic 的 `DurationCollectionSettings`：
//! - `duration_threshold_minutes: int = Field(default=300, ge=1)`
//! - `number_features: set[str]`，默认 `{"OFJE", "CJOB", "DVAJ", "REBD"}`，
//!   存前按 `strip().upper()` 归一化
//! - `suffix_number_features: set[str]`，默认空，同样归一化
//! - `tag_names: set[str]`，默认空，按 `strip().casefold()` 归一化
//!
//! # 对上游的偏离
//!
//! 1. **越界值按边界取值，不报错**。上游 pydantic 越界是 `ValidationError` →
//!    插件加载失败。在 gRPC 插件上，注册期抛错的表现是「进程起来又被判不合规」
//!    （与 `sakuramedia-javbus-metadata` 同一个理由），所以
//!    `duration_threshold_minutes` 夹到 `>= 1`。
//! 2. **配置从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读**，不是 `context.settings`。

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;
use sm_plugin_api::v1::SettingsField;

/// 宿主写好的配置文件。
pub const SETTINGS_FILE_ENV: &str = "SAKURAMEDIA_PLUGIN_SETTINGS_FILE";

/// 上游 `Field(default=300, ge=1)`。
pub const DEFAULT_DURATION_THRESHOLD_MINUTES: u64 = 300;

/// 上游默认的番号前缀特征。
pub fn default_number_features() -> HashSet<String> {
    ["OFJE", "CJOB", "DVAJ", "REBD"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// 插件配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationCollectionSettings {
    /// 合集时长阈值（分钟）。达到或超过即判为合集。
    pub duration_threshold_minutes: u64,
    /// 番号前缀特征（已按 `strip().upper()` 归一化）。
    pub number_features: HashSet<String>,
    /// 番号后缀特征（已按 `strip().upper()` 归一化）。
    pub suffix_number_features: HashSet<String>,
    /// 标签名（已按 `strip().casefold()` 归一化）。
    pub tag_names: HashSet<String>,
}

impl Default for DurationCollectionSettings {
    fn default() -> Self {
        Self {
            duration_threshold_minutes: DEFAULT_DURATION_THRESHOLD_MINUTES,
            number_features: default_number_features(),
            suffix_number_features: HashSet::new(),
            tag_names: HashSet::new(),
        }
    }
}

fn normalize_number_feature(value: &str) -> Option<String> {
    let n = value.trim().to_uppercase();
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

fn normalize_tag_name(value: &str) -> Option<String> {
    let n = value.trim().to_lowercase();
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

fn read_string_set(value: Option<&Value>, normalize: fn(&str) -> Option<String>) -> HashSet<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .filter_map(normalize)
            .collect(),
        _ => HashSet::new(),
    }
}

impl DurationCollectionSettings {
    /// 从宿主写好的 `settings.json` 里读配置。文件不存在或解析失败时
    /// 按上游缺省跑 —— 宿主每次拉起都会重写它，缺文件属于「还没配」，
    /// 不该让插件起不来。
    pub fn load() -> Self {
        let path = std::env::var(SETTINGS_FILE_ENV).unwrap_or_default();
        if path.is_empty() {
            return Self::default();
        }
        Self::load_from(Path::new(&path))
    }

    fn load_from(path: &Path) -> Self {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let mut out = Self::default();
        if let Some(t) = value.get("duration_threshold_minutes").and_then(Value::as_u64) {
            out.duration_threshold_minutes = t.max(1);
        }
        // 上游 validator 里 `None` → 空集；这里缺字段就保留缺省（前缀那组
        // 上游缺省非空），显式给了（哪怕空数组）才覆盖。
        if value.get("number_features").is_some() {
            out.number_features = read_string_set(value.get("number_features"), normalize_number_feature);
        }
        if value.get("suffix_number_features").is_some() {
            out.suffix_number_features =
                read_string_set(value.get("suffix_number_features"), normalize_number_feature);
        }
        if value.get("tag_names").is_some() {
            out.tag_names = read_string_set(value.get("tag_names"), normalize_tag_name);
        }
        out
    }

    /// 宿主渲染配置表单用的 schema。
    pub fn schema() -> Vec<SettingsField> {
        vec![
            SettingsField {
                key: "duration_threshold_minutes".to_owned(),
                label: "合集时长阈值（分钟）".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some(format!(
                    "达到或超过该时长即判为合集；默认 {DEFAULT_DURATION_THRESHOLD_MINUTES}，最小 1"
                )),
                multiline: false,
                hint: None,
            },
            SettingsField {
                key: "number_features".to_owned(),
                label: "番号前缀".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some(
                    "番号归一化后以前缀命中即判为合集；多个用英文逗号分隔".to_owned(),
                ),
                multiline: true,
                hint: None,
            },
            SettingsField {
                key: "suffix_number_features".to_owned(),
                label: "番号后缀".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("番号归一化后以后缀命中即判为合集；多个用英文逗号分隔".to_owned()),
                multiline: true,
                hint: None,
            },
            SettingsField {
                key: "tag_names".to_owned(),
                label: "标签".to_owned(),
                input: "text".to_owned(),
                required: false,
                description: Some("命中标签即判为合集；多个用英文逗号分隔".to_owned()),
                multiline: true,
                hint: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let s = DurationCollectionSettings::default();
        assert_eq!(s.duration_threshold_minutes, 300);
        assert_eq!(s.number_features, default_number_features());
        assert!(s.suffix_number_features.is_empty());
        assert!(s.tag_names.is_empty());
    }

    #[test]
    fn threshold_clamped_to_at_least_one() {
        let dir = std::env::temp_dir();
        let path = dir.join("judge_collection_settings_test.json");
        std::fs::write(&path, r#"{"duration_threshold_minutes": 0}"#).unwrap();
        let s = DurationCollectionSettings::load_from(&path);
        assert_eq!(s.duration_threshold_minutes, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn features_normalized_on_load() {
        let dir = std::env::temp_dir();
        let path = dir.join("judge_collection_settings_test2.json");
        std::fs::write(
            &path,
            r#"{"number_features": [" ofje ", "cjOB", ""], "tag_names": ["合集", " Best "]}"#,
        )
        .unwrap();
        let s = DurationCollectionSettings::load_from(&path);
        assert_eq!(s.number_features, ["OFJE", "CJOB"].into_iter().map(str::to_owned).collect());
        assert_eq!(s.tag_names, ["合集", "best"].into_iter().map(str::to_owned).collect());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let s = DurationCollectionSettings::load_from(Path::new("/nonexistent/settings.json"));
        assert_eq!(s, DurationCollectionSettings::default());
    }
}
