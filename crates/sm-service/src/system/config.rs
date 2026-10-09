//! 配置读写：白名单校验、局部合并、严格校验、原子写盘。
//!
//! 对应上游 `src/service/system/config_service.py`（153 行，**本域里最小的一个**
//! 文件 —— 与 `account_service` 的 307 行同属 system 子域）。
//!
//! # 契约：五个错误码，一个都不多
//!
//! | 码 | 触发条件 | details |
//! |---|---|---|
//! | `empty_config_update` | patch 为空对象 | — |
//! | `readonly_config_key` | patch 命中 `auth` / `enable_docs` / `plugins` | `{"field": "<key>"}` |
//! | `unknown_config_field` | 顶层键不在白名单，或子节里的键不在该节字段表 | `{"field": "<key>" \| "<key>.<sub>"}` |
//! | `invalid_config_value` | 子节收到非对象；或严格校验不过；或 `image_search` 开了而 `qdrant` 没开 | `{"field": ...}` 或 `{"errors": [...]}` |
//!
//! # 有一条校验器从本 API 不可达
//!
//! 上游 `Plugins` 的两个校验器（`enabled` 不许重复、`job_crons`/`settings` 的
//! 键必须是合法插件 ID）在配置 API 上**永远不生效** —— `plugins` 是只读键，
//! 请求在白名单那一步就以 `readonly_config_key` 被挡掉了。它们只在**启动加载
//! 手工改过的 TOML** 时有用。
//!
//! 这不是 bug，是只读设计的必然结果。`tests/config_service.rs` 里有断言把
//! 这个事实钉住：哪天有人把 `plugins` 从 `READONLY_KEYS` 挪走，重复插件 ID
//! 就能从 API 写进去，而那是「启用两份同一插件」。
//!
//! # 三条容易搞反的语义
//!
//! **① 每次 PATCH 都从**当前磁盘快照**开始合并，不是从进程内的启动快照。**
//! 上游注释写得很直接：`load_persisted_settings()` 而不是 `settings` ——
//! 否则连续两次局部 PATCH，第二次会把第一次的结果用旧快照覆盖掉。
//!
//! **② 写盘不生效于当前进程。** 返回的 `restart_required` 恒为
//! `["api", "aps"]`：普通配置只落盘，进程继续用启动时快照，重启后统一生效。
//! 这是「配置改动为什么不立刻起作用」的答案，也是客户端弹提示的依据。
//!
//! **③ `updates` 与 `existing_config` 是可 PATCH 的。** 见
//! [`sm_core::config_schema::EXTRA_WRITABLE_KEYS`] 的说明 —— 上游把它们
//! 声明成了 `Settings` 字段，于是就在白名单里。照抄是对齐，收紧会让原本
//! 200 的请求变 422。
//!
//! # 合并只深一层，且不限于声明过的子节
//!
//! `{"scheduler": {"log_dir": "/x"}}` 会并进现有的 `scheduler` 对象，
//! 其余字段保留。**再深一层不合并** —— `{"plugins": {"settings": {...}}}`
//! 整体替换 `plugins.settings`，而 `plugins` 是只读键，所以这条路走不到。
//!
//! 「不限于声明过的子节」这一点容易被漏掉：上游的判据只是
//! `isinstance(value, dict) and isinstance(merged.get(key), dict)`，它**不**查
//! 这个 key 是不是 `Settings` 里声明过的节。所以自由形式的 `updates` 也是
//! 深一层合并 —— `{"updates": {"b": "2"}}` 会保留原有的 `{"a": "1"}`。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sm_core::config_schema as schema;

use crate::error::{details_of, ProgrammerError, ServiceError};

/// 空 patch 的错误码。
pub const EMPTY_CONFIG_UPDATE: &str = "empty_config_update";
/// 命中只读键的错误码。
pub const READONLY_CONFIG_KEY: &str = "readonly_config_key";
/// 未知键的错误码。
pub const UNKNOWN_CONFIG_FIELD: &str = "unknown_config_field";
/// 值非法（含类型、范围、跨节不变式）的错误码。
pub const INVALID_CONFIG_VALUE: &str = "invalid_config_value";

/// `ConfigUpdateResource.restart_required` 的恒定值。
///
/// 只有「api」与「aps」两个进程会读配置（上游是两个独立部署），所以恒为这两个。
pub const RESTART_REQUIRED: [&str; 2] = ["api", "aps"];

/// 配置服务。持有一个 TOML 文件路径。
#[derive(Debug, Clone)]
pub struct ConfigService {
    path: PathBuf,
}

impl ConfigService {
    /// 以给定路径创建服务。
    ///
    /// 文件**可以不存在** —— 上游首次启动时也是这样：先落默认值与自举出的
    /// secrets，配置文件才出现（`ensure_runtime_secrets`）。缺文件时
    /// [`Self::snapshot`] 退回全部默认值。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 配置文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读当前值：磁盘上有就以它为底，否则退回默认值。
    ///
    /// 上游 `load_persisted_settings()` 的行为：**缺文件不是错误**，那正是
    /// 首次启动的路径。
    pub fn snapshot(&self) -> Result<Value, ServiceError> {
        if !self.path.exists() {
            return Ok(schema::defaults_json());
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| io_error(&self.path, "读取配置失败", e))?;
        let from_disk: Value = toml::from_str(&text)
            .map_err(|e| ProgrammerError::new(format!("配置不是合法 TOML: {e}")))?;
        Ok(schema::overlay_defaults(from_disk))
    }

    /// 读当前值的**公开**快照（剔除只读键）。
    ///
    /// 只读键包括 `auth`（含 secret_key 与 file_signature_secret）——
    /// 把它们塞进响应体就是凭据外流。`plugins` 同理，它可能含插件私有配置。
    pub fn get(&self) -> Result<Value, ServiceError> {
        Ok(schema::public_json(&self.snapshot()?))
    }

    /// 局部更新并落盘。
    ///
    /// 顺序很重要，与上游一致：**先白名单、再合并、再严格校验、最后写盘**。
    /// 反过来（先合并再查未知键）会让一个拼错的键被合并进快照后才被发现，
    /// 而那时已经改过了内存状态。
    pub fn update(&self, patch: &Map<String, Value>) -> Result<Value, ServiceError> {
        // 上游的第一条规则，且在白名单校验**之前**：空对象连「键认不认识」
        // 都没资格问。它有自己的错误码而不是复用 `unknown_config_field`，
        // 因为客户端要区分「你什么都没改」与「你改了个不存在的键」。
        if patch.is_empty() {
            return Err(ServiceError::validation(
                EMPTY_CONFIG_UPDATE,
                "At least one field must be provided",
            ));
        }
        reject_unknown_fields(patch)?;

        // 每次从磁盘快照起算：连续两次局部 PATCH 不能互相覆盖。
        let mut merged = self.snapshot()?;
        deep_merge_one_level(&mut merged, patch);

        let object = merged.as_object().ok_or_else(|| {
            ServiceError::from(ProgrammerError::new("配置快照不是对象 —— 配置文件被写坏了"))
        })?;

        // 跨节不变式先于逐字段校验：它是「语义」错误，报出来的 message 比
        // 「字段类型不对」更接近用户的真实意图。
        if schema::image_search_requires_qdrant(object) {
            return Err(ServiceError::validation(
                INVALID_CONFIG_VALUE,
                "启用图片与文字搜图需要先启用 Qdrant",
            ));
        }

        let errors = schema::validate_strict(object);
        if !errors.is_empty() {
            return Err(ServiceError::validation_with(
                INVALID_CONFIG_VALUE,
                "Configuration validation failed",
                details_of_errors(&errors),
            ));
        }

        self.persist(&merged)?;
        Ok(schema::public_json(&merged))
    }

    /// 原子写盘。
    ///
    /// 「原子」在这里有具体含义：先写同目录下的隐藏临时文件，`fsync`，再
    /// `rename` 覆盖目标。`rename` 在同一文件系统内是原子的，所以**不存在**
    /// 「读到半个配置」的状态 —— 而配置文件在进程启动时被读，半个 TOML 会让
    /// 新进程直接起不来。上游 `persist_settings` 用的是同一套三步
    /// （`tempfile` → `flush` + `fsync` → `os.replace`）。
    ///
    /// 临时文件是**隐藏的**（`.config.toml.<pid>.tmp`）：运维 `ls` 配置目录时
    /// 不该看到它，也不该有 IDE / 备份脚本去同步它。任何失败路径都会清掉它
    /// —— 残留的 `.tmp` 会让「配置有两份」变成可能。
    fn persist(&self, values: &Value) -> Result<(), ServiceError> {
        let text = toml::to_string_pretty(&strip_nulls(values)).map_err(|e| {
            ServiceError::from(ProgrammerError::new(format!("配置无法序列化为 TOML: {e}")))
        })?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_error(parent, "创建配置目录失败", e))?;
        }
        // 临时文件与目标同目录 —— 跨文件系统的 rename 不是原子的。
        let name = self.path.file_name().map_or_else(
            || "config.toml".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let temp = self
            .path
            .with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(text.as_bytes())?;
            // 不 fsync 的话，rename 可能先于内容落盘 —— 那就白改了。
            file.sync_all()
        };
        if let Err(err) = write() {
            let _ = std::fs::remove_file(&temp);
            return Err(io_error(&temp, "写临时配置失败", err));
        }
        if let Err(err) = std::fs::rename(&temp, &self.path) {
            let _ = std::fs::remove_file(&temp);
            return Err(io_error(&self.path, "替换配置文件失败", err));
        }
        Ok(())
    }
}

/// 白名单校验。逐条对应上游 `_reject_unknown_fields`。
///
/// 顺序刻意是「只读键 → 未知顶层键 → 子节形状 → 未知子键」，与上游一致：
/// 只读键单独报错是为了不与 `unknown_config_field` 混淆 —— 前者有替代路径
/// （改 toml 或 `/account`），后者是拼错了。
pub fn reject_unknown_fields(patch: &Map<String, Value>) -> Result<(), ServiceError> {
    for (key, value) in patch {
        if schema::is_readonly_key(key) {
            return Err(field_error(
                READONLY_CONFIG_KEY,
                format!("Config key '{key}' is not modifiable via this API"),
                key,
            ));
        }
        if !schema::is_known_top_level_key(key) {
            return Err(field_error(
                UNKNOWN_CONFIG_FIELD,
                format!("Unknown config field: {key}"),
                key,
            ));
        }
        if !schema::is_section_key(key) {
            // `enable_docs` 已在上面被只读键拦掉；这里是标量键
            // （`updates` / `existing_config`），值直接替换。
            continue;
        }
        let section = schema::section(key).expect("刚判过是子节");
        let Some(object) = value.as_object() else {
            return Err(field_error(
                INVALID_CONFIG_VALUE,
                format!("Config section '{key}' must be an object"),
                key,
            ));
        };
        for sub_key in object.keys() {
            if !section.fields.iter().any(|f| f.name == sub_key) {
                let dotted = format!("{key}.{sub_key}");
                return Err(field_error(
                    UNKNOWN_CONFIG_FIELD,
                    format!("Unknown config field: {dotted}"),
                    &dotted,
                ));
            }
        }
    }
    Ok(())
}

/// 把 patch 并进 base：字典且 base 对应位置也是字典时**深一层**合并，否则整体替换。
pub fn deep_merge_one_level(base: &mut Value, patch: &Map<String, Value>) {
    for (key, value) in patch {
        let merged = match (value.as_object(), base.get_mut(key)) {
            (Some(incoming), Some(existing)) if existing.is_object() => {
                let mut section = existing.as_object().cloned().unwrap_or_default();
                for (sub_key, sub_value) in incoming {
                    section.insert(sub_key.clone(), sub_value.clone());
                }
                Value::Object(section)
            }
            _ => value.clone(),
        };
        if let Some(root) = base.as_object_mut() {
            root.insert(key.clone(), merged);
        }
    }
}

/// 写盘前剥掉所有 null 值。
///
/// # 为什么需要
///
/// TOML **没有 null**。全表唯一的 `null` 默认值是
/// `image_search.inference_api_key`（`str | None = None`），直接序列化会
/// 让 `toml` 报 `unsupported unit type` —— 于是「默认值」永远写不出去。
///
/// 上游靠 Python `toml.dumps` 的一个隐式行为解决：它的编码器**静默跳过
/// `None`**。所以磁盘上「这个键不存在」与「这个键是 null」是同一件事，
/// 读回时都由默认值补上（见 [`schema::overlay_defaults`]）。
///
/// 这里显式做同一件事，而不是依赖库的宽容 —— 依赖「写不出来就当没写」这种
/// 隐式行为，等于把一个契约寄托在实现细节上。
pub fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

fn field_error(code: &str, message: String, field: &str) -> ServiceError {
    ServiceError::validation_with(code, message, details_of("field", field))
}

fn details_of_errors(errors: &[schema::FieldError]) -> Map<String, Value> {
    let list: Vec<Value> = errors
        .iter()
        .map(|e| serde_json::json!({ "loc": e.loc, "msg": e.reason }))
        .collect();
    let mut details = Map::new();
    details.insert("errors".to_owned(), Value::Array(list));
    details
}

fn io_error(path: &Path, what: &str, err: std::io::Error) -> ServiceError {
    ServiceError::from(ProgrammerError::new(format!(
        "{what}（{}）: {err}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_section_patch_merges_rather_than_replaces() {
        let mut base = serde_json::json!({
            "scheduler": {"enabled": true, "log_dir": "/data/logs"}
        });
        let patch = serde_json::json!({"scheduler": {"log_dir": "/new"}})
            .as_object()
            .cloned()
            .expect("对象");
        deep_merge_one_level(&mut base, &patch);
        assert_eq!(
            base["scheduler"],
            serde_json::json!({"enabled": true, "log_dir": "/new"}),
            "只该替换 log_dir，enabled 应保留"
        );
    }

    #[test]
    fn merging_applies_to_any_dict_not_only_declared_sections() {
        // 上游的判据是 `isinstance(value, dict) and isinstance(merged.get(key), dict)`
        // —— **不**查这个 key 是不是声明过的子节。所以 `updates` 这种自由
        // 形式的字典也是深一层合并，而不是整体替换。
        let mut base = serde_json::json!({"enable_docs": false, "updates": {"a": "1"}});
        let patch = serde_json::json!({"updates": {"b": "2"}})
            .as_object()
            .cloned()
            .expect("对象");
        deep_merge_one_level(&mut base, &patch);
        assert_eq!(base["updates"], serde_json::json!({"a": "1", "b": "2"}));
        assert_eq!(base["enable_docs"], serde_json::json!(false));
    }

    #[test]
    fn a_dict_where_the_base_is_scalar_replaces_outright() {
        // 合并只在两边**都**是 dict 时发生；类型不同就整体替换。
        let mut base = serde_json::json!({"qdrant": "http://旧"});
        let patch = serde_json::json!({"qdrant": {"url": "http://新"}})
            .as_object()
            .cloned()
            .expect("对象");
        deep_merge_one_level(&mut base, &patch);
        assert_eq!(base["qdrant"], serde_json::json!({"url": "http://新"}));
    }

    #[test]
    fn disk_values_override_defaults_and_missing_ones_fall_back() {
        use sm_core::config_schema::overlay_defaults;
        let disk = serde_json::json!({
            "logging": {"level": "DEBUG"},
            "scheduler": {"log_dir": "/custom"},
        });
        let merged = overlay_defaults(disk);
        assert_eq!(merged["logging"]["level"], serde_json::json!("DEBUG"));
        assert_eq!(merged["scheduler"]["log_dir"], serde_json::json!("/custom"));
        // 同节里没写的字段回落默认值
        assert_eq!(
            merged["scheduler"]["worker_default_concurrency"],
            serde_json::json!(4)
        );
        // 完全没出现的节整体用默认值
        assert_eq!(
            merged["qdrant"]["url"],
            serde_json::json!("http://qdrant:6333")
        );
    }

    #[test]
    fn unknown_disk_keys_are_kept_not_pruned() {
        use sm_core::config_schema::overlay_defaults;
        // 更新版本写入的键本进程不认识，但删掉它就是数据丢失。
        let merged = overlay_defaults(serde_json::json!({"future_section": {"x": 1}}));
        assert!(merged.get("future_section").is_some());
    }
}
