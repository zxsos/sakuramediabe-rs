//! 插件包清单（`manifest.json`）的解析与校验。
//!
//! 上游对应 `src/plugins/manifest.py`（69 行）。
//!
//! # 只读四个字段，且**允许别的字段存在**
//!
//! 上游那个模型有十个字段并带 `extra="forbid"`（未知字段直接拒绝）。Rust 侧
//! 只读这四个：
//!
//! | 字段 | Rust 侧为什么需要 |
//! |---|---|
//! | `plugin_id` | 目录名 / 环境变量 / `Register` 回显三者要对上 |
//! | `display_name` | UI 与日志（上游同样是必需字段） |
//! | `version` | 升级判定（「新版必须更高」）与遥测心跳 |
//! | `release_api_url` | 前端检查更新 |
//!
//! **不读** `requires_python` / `dependencies` / `settings_model` /
//! `author` / `homepage` —— 它们是 Python 侧的概念（包管理器、pydantic
//! 配置模型、PyPI 元数据），Rust 插件没有对应物。
//!
//! # `host_api_version` 仍然可缺，且**不参与校验**
//!
//! 上游把它定为必需（`ge=1`），存进 [`crate::manifest::PluginManifest`]
//! 只为**展示**与排障。
//! **不要拿它去判兼容性**：它是 Python 侧的 ABI 版本（javbus 那个包里写着
//! `6`），而 Rust 侧的 ABI 版本是 `sm_plugin_api::ABI_MAJOR`（当前 2，
//! 由 `Register` 回显并在 `registration::validate_registration` 里比对）。
//! 拿 `6` 去比 `2` 会把一个**正确的包**判成不兼容。
//!
//! # 与上游相反：未知字段**不拒绝**
//!
//! 上游 `extra="forbid"` 抓的是「作者把字段名拼错了」。但那要求宿主与插件包
//! **同时**演进；而 Rust 宿主面对的是上游生态里**已经存在**的包（以及上游将
//! 来新增字段的包）—— 拒装全部插件比漏掉一个拼写错误贵得多。
//!
//! 所以这里宽松：已知字段校验类型，未知字段**记一条 warn 然后继续**。
//! 代价写在明处：`displan_name`（拼错）不会被拒，只会静默地没有展示名。

use std::path::Path;

use sm_core::config_schema::is_valid_plugin_id;

/// 清单文件名。上游 `MANIFEST_FILENAME`（`manifest.py:14`）。
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// 已知字段。**不在这份名单里的字段不报错**，只记 warn（见模块文档）。
const KNOWN_FIELDS: [&str; 10] = [
    "plugin_id",
    "display_name",
    "version",
    "host_api_version",
    "settings_model",
    "requires_python",
    "dependencies",
    "author",
    "homepage",
    "release_api_url",
];

/// 清单解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestProblem {
    /// `<dir>/manifest.json` 不存在。
    Missing,
    /// 读不出来（IO / 非法 JSON / 顶层不是对象）。
    NotJson(String),
    /// 缺必需字段。
    MissingField(&'static str),
    /// 字段在但值不合法（空串、类型不符、`plugin_id` 格式不对）。
    InvalidField { field: &'static str, reason: String },
}

impl ManifestProblem {
    /// 稳定的错误标识，进 API 的 `error.code` 与日志。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "plugin_manifest_missing",
            Self::NotJson(_) => "plugin_manifest_invalid_json",
            Self::MissingField(_) => "plugin_manifest_missing_field",
            Self::InvalidField { .. } => "plugin_manifest_invalid_field",
        }
    }

    /// 人类可读的说明（含字段名，便于作者直接改）。
    pub fn message(&self) -> String {
        match self {
            Self::Missing => format!("插件缺少 {MANIFEST_FILENAME}"),
            Self::NotJson(reason) => format!("{MANIFEST_FILENAME} 无法解析：{reason}"),
            Self::MissingField(field) => format!("{MANIFEST_FILENAME} 缺少字段 {field}"),
            Self::InvalidField { field, reason } => {
                format!("{MANIFEST_FILENAME} 的 {field} 不合法：{reason}")
            }
        }
    }
}

/// 插件包清单。**只保留 Rust 侧要用的字段**（见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    /// 插件 id。目录名、`SAKURAMEDIA_PLUGIN_ID`、`Register` 回显三者都是它。
    pub plugin_id: String,
    /// 展示名。
    pub display_name: String,
    /// 版本。升级时要求「新包 > 已装」（比较规则见 service 层）。
    pub version: String,
    /// 上游字段：**不参与兼容性判定**，只留作展示与排障（见模块文档）。
    pub host_api_version: Option<i32>,
    /// 检查更新的 Release API 地址（前端的插件页在用它）。
    pub release_api_url: Option<String>,
    /// 上游字段，只供详情接口回显（宿主不读它）。
    ///
    /// 上游另有两个字段（`settings_model` / `dependencies`）本仓**没有**解析：
    /// 前者是 pydantic 模型名、后者是 PEP 508 依赖列表，都只在 Python 侧有意义。
    /// 它们仍然会出现在 [`Self::raw`] 里，所以详情接口不会丢信息。
    pub requires_python: Option<String>,
    /// 上游字段，只供详情接口回显。
    pub author: Option<String>,
    /// 上游字段，只供详情接口回显。
    pub homepage: Option<String>,
    /// 清单的**原始 JSON 对象**。
    ///
    /// 详情接口（`GET /system/plugins/{id}`）的 `manifest` 字段是
    /// 「整份清单」，而上面那些具名字段只是其中宿主关心的几个 —— 用 `raw`
    /// 回显才能把 `settings_model` / `dependencies` 也带给客户端。
    pub raw: serde_json::Value,
}

impl PluginManifest {
    /// 解析 `manifest.json` 的字节（从 zip 条目里读出来的就是它）。
    pub fn parse(raw: &[u8]) -> Result<Self, ManifestProblem> {
        let text = std::str::from_utf8(raw)
            .map_err(|error| ManifestProblem::NotJson(format!("不是 UTF-8：{error}")))?;
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| ManifestProblem::NotJson(error.to_string()))?;
        Self::from_value(&value)
    }

    /// 从插件目录读 `<dir>/manifest.json`。
    pub fn read_from_dir(dir: &Path) -> Result<Self, ManifestProblem> {
        let path = dir.join(MANIFEST_FILENAME);
        let raw = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ManifestProblem::Missing
            } else {
                ManifestProblem::NotJson(format!("读 {} 失败：{error}", path.display()))
            }
        })?;
        Self::parse(&raw)
    }

    /// 从已解析的 JSON 构造。
    ///
    /// **未知字段只记 warn**（模块文档解释了为什么与上游的 `extra="forbid"`
    /// 相反）。
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ManifestProblem> {
        let object = value
            .as_object()
            .ok_or_else(|| ManifestProblem::NotJson("顶层不是对象".to_owned()))?;

        for key in object.keys() {
            if !KNOWN_FIELDS.contains(&key.as_str()) {
                tracing::warn!(
                    field = key.as_str(),
                    "插件清单里有本仓不认识的字段（已忽略；见 manifest 模块文档）"
                );
            }
        }

        let plugin_id = required_text(object, "plugin_id")
            .or_else(|_| required_text(object, "id"))
            .map_err(|_| ManifestProblem::InvalidField {
                field: "plugin_id",
                reason: "缺少字段 plugin_id（或别名 id）".to_owned(),
            })?;
        if !is_valid_plugin_id(&plugin_id) {
            return Err(ManifestProblem::InvalidField {
                field: "plugin_id",
                reason: "只能是小写字母、数字、下划线，且以字母开头".to_owned(),
            });
        }
        // 上游 `max_length=64` —— 它要当目录名与环境变量值，长到一定程度
        // 会撞文件系统与 env 的长度限制。这里保留同一条。
        if plugin_id.len() > 64 {
            return Err(ManifestProblem::InvalidField {
                field: "plugin_id",
                reason: format!("最长 64 个字符，收到 {}", plugin_id.len()),
            });
        }

        Ok(Self {
            plugin_id,
            display_name: required_text(object, "display_name")?,
            version: required_text(object, "version")?,
            host_api_version: object
                .get("host_api_version")
                .and_then(serde_json::Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
            release_api_url: optional_text(object, "release_api_url"),
            requires_python: optional_text(object, "requires_python"),
            author: optional_text(object, "author"),
            homepage: optional_text(object, "homepage"),
            raw: value.clone(),
        })
    }
}

/// 取一个**必需的**非空字符串字段。
///
/// 空白串按「没给」处理：上游 pydantic 有 `str_strip_whitespace=True`，
/// `" "` 过 `min_length=1` 会失败 —— 两边行为一致。
fn required_text(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<String, ManifestProblem> {
    match object.get(field) {
        None => Err(ManifestProblem::MissingField(field)),
        Some(value) => match value.as_str() {
            None => Err(ManifestProblem::InvalidField {
                field,
                reason: "不是字符串".to_owned(),
            }),
            Some(text) if text.trim().is_empty() => Err(ManifestProblem::InvalidField {
                field,
                reason: "是空串".to_owned(),
            }),
            Some(text) => Ok(text.trim().to_owned()),
        },
    }
}

/// 取一个可选的非空字符串字段。空白串按「没给」处理。
fn optional_text(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<String> {
    object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "plugin_id": "local",
            "display_name": "本地存储",
            "version": "1.2.3",
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        base
    }

    #[test]
    fn the_four_read_fields_are_parsed() {
        let manifest = PluginManifest::from_value(&manifest_json(serde_json::json!({
            "release_api_url": "https://api.github.com/repos/x/y/releases/latest",
            "host_api_version": 6,
        })))
        .expect("应当解析成功");
        assert_eq!(manifest.plugin_id, "local");
        assert_eq!(manifest.display_name, "本地存储");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.host_api_version, Some(6));
        assert!(manifest.release_api_url.is_some());
    }

    /// ★ 上游的十个字段里，Rust 不读的那六个**存在时也不报错**。
    ///
    /// 反过来的写法（照抄上游的 `extra="forbid"`）会让上游生态里**已经存在**
    /// 的包一个都装不上 —— 那些包里必然带 `requires_python` 之类的字段。
    #[test]
    fn upstream_only_fields_are_accepted_and_ignored() {
        let manifest = PluginManifest::from_value(&manifest_json(serde_json::json!({
            "settings_model": "Settings",
            "requires_python": ">=3.10,<3.11",
            "dependencies": ["libtorrent>=2.1.1,<3.0.0"],
            "author": "SakuraMedia",
            "homepage": "https://example.test",
        })))
        .expect("上游专有字段不该让解析失败");
        assert_eq!(manifest.plugin_id, "local");
    }

    /// `plugin_id` 的格式与上游 `PLUGIN_ID_PATTERN` 一致。
    #[test]
    fn a_malformed_plugin_id_is_rejected() {
        for bad in ["Local", "1local", "local-provider", "local.provider", ""] {
            let error = PluginManifest::from_value(&manifest_json(serde_json::json!({
                "plugin_id": bad,
            })))
            .expect_err("{bad} 应当被拒");
            assert!(
                matches!(
                    error,
                    ManifestProblem::InvalidField {
                        field: "plugin_id",
                        ..
                    } | ManifestProblem::MissingField("plugin_id")
                ),
                "{bad} 的错误类型不对：{error:?}"
            );
        }
    }

    #[test]
    fn required_fields_cannot_be_missing_or_blank() {
        for field in ["plugin_id", "display_name", "version"] {
            let mut value = manifest_json(serde_json::json!({}));
            value.as_object_mut().expect("对象").remove(field);
            let error = PluginManifest::from_value(&value).expect_err("缺字段该报错");
            assert_eq!(error, ManifestProblem::MissingField(field));

            let blank = manifest_json(serde_json::json!({}));
            let mut blank = blank;
            blank
                .as_object_mut()
                .expect("对象")
                .insert(field.to_owned(), serde_json::json!("   "));
            let error = PluginManifest::from_value(&blank).expect_err("空白串该报错");
            assert!(
                matches!(error, ManifestProblem::InvalidField { field: got, .. } if got == field),
                "{field} 的错误类型不对：{error:?}"
            );
        }
    }

    /// `host_api_version` 缺失是**正常**的（Rust 插件不需要它），
    /// 而且它不参与任何兼容性判断。
    #[test]
    fn host_api_version_is_optional_and_never_compared() {
        let manifest =
            PluginManifest::from_value(&manifest_json(serde_json::json!({}))).expect("应当成功");
        assert_eq!(manifest.host_api_version, None);
        // 一个「Python 侧版本号」不会让解析失败 —— 它只是被记下来。
        let with_python_version = PluginManifest::from_value(&manifest_json(serde_json::json!({
            "host_api_version": 6,
        })))
        .expect("应当成功");
        assert_eq!(with_python_version.host_api_version, Some(6));
    }

    /// 顶层不是对象 / 不是合法 JSON → 各自的错误，**不是 panic**。
    #[test]
    fn non_object_and_broken_json_are_reported() {
        assert!(matches!(
            PluginManifest::parse(b"[1,2,3]"),
            Err(ManifestProblem::NotJson(_))
        ));
        assert!(matches!(
            PluginManifest::parse(b"{not json"),
            Err(ManifestProblem::NotJson(_))
        ));
        assert!(matches!(
            PluginManifest::parse(&[0xff, 0xfe]),
            Err(ManifestProblem::NotJson(_))
        ));
    }

    /// 每个错误都有稳定的 code（进 API 的 `error.code`）。
    #[test]
    fn problems_carry_stable_codes() {
        assert_eq!(ManifestProblem::Missing.code(), "plugin_manifest_missing");
        assert_eq!(
            PluginManifest::from_value(&serde_json::json!({}))
                .expect_err("空对象该报缺字段")
                .code(),
            "plugin_manifest_missing_field"
        );
    }
}
