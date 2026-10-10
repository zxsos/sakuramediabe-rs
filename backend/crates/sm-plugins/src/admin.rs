//! 插件管理的**宿主实现**（`sm_service::system::plugins::PluginAdmin`）。
//!
//! 上游对应 `PluginManager`（`manager.py:49-347`）里除 zip 解包之外的编排部分。
//!
//! # 为什么实现放在 `sm-plugins` 而不是 `sm-service`
//!
//! 它要同时用到「插件目录的布局」（[`crate::inventory`] / [`crate::installer`]）
//! 与「配置写入」（`ConfigService`）—— 前者只有本 crate 有。契约（trait 与 DTO）
//! 在 `sm-service`，组合根把实现注入 `AppState`：与 `StorageGateway` /
//! `DownloadCapabilityRegistry` / `MediaLibraryRegistry` 同一套做法。
//!
//! # 每次操作都重新读盘
//!
//! `root_dir` 与 `enabled` 都从**当前磁盘快照**读，进程内不缓存：
//!
//! - 插件目录是**多进程共享**的（宿主、将来的 CLI、运维手工拷贝）；
//! - 配置也可能被运维手工改过（`plugins` 是只读键，改它只能改 toml）。
//!
//! 缓存任何一样，都会让「另一个进程刚装好的插件」看不见 —— 而那正是运维
//! 装完插件后最想确认的事。
//!
//! # 本批**未做**的一件事（`load_status` 的运行时那一半）
//!
//! 上游 `load_status` 有三个来源：清单本身坏掉、import 阶段加载失败
//! （`PLUGIN_LOAD_ERRORS`）、声明依赖装不上。本模块现在只覆盖**第一个** ——
//! 后面两个来自「插件进程没起来」，那要读 supervisor 的运行时状态，而它
//! 还没有一条暴露到管理接口的通道。
//!
//! 所以清单读得出来时 `load_status` 一律 `"ok"`。**这与上游不同**，且不同在
//! 一个用户能看见的地方（插件页不会显示「加载失败」），所以记在这里而不是
//! 悄无声息。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sm_core::config_schema::is_valid_plugin_id;
use sm_service::error::ServiceError;
use sm_service::system::config::ConfigService;
use sm_service::system::plugins::{
    PluginAdmin, PluginDetail, PluginInstallOutcome, PluginSummary, PLUGIN_INSTALL_FAILED,
    PLUGIN_NOT_FOUND, PLUGIN_UPGRADE_FAILED, RESTART_API_AND_APS, RESTART_CONTAINER,
};

use crate::installer;
use crate::inventory::{self, ScannedPlugin};
use crate::manifest::PluginManifest;
use crate::versions;

/// 上传暂存子目录（插件根下）。上游 `_upload_temp_path`：`.staging/uploads`。
pub const UPLOAD_SUBDIR: &str = "uploads";

/// 残留上传的保留时长（秒）。上游 `_UPLOAD_STALE_SECONDS = 24 * 3600`。
pub const UPLOAD_STALE_SECONDS: u64 = 24 * 3600;

/// 插件管理实现。只持有配置服务 —— `root_dir` 每次从配置里读。
#[derive(Debug, Clone)]
pub struct PluginAdminService {
    config: ConfigService,
}

impl PluginAdminService {
    pub fn new(config: ConfigService) -> Self {
        Self { config }
    }

    /// 插件根目录（`plugins.root_dir`）。
    ///
    /// 配置快照已经叠加过模式默认值，所以这个键**总是存在** —— 读不到就说明
    /// 配置文件被写坏了，那是 500 而不是「当成空目录」。
    fn root_dir(&self) -> Result<PathBuf, ServiceError> {
        let snapshot = self.config.snapshot()?;
        let raw = snapshot
            .get("plugins")
            .and_then(|plugins| plugins.get("root_dir"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if raw.is_empty() {
            return Err(ServiceError::from_status(
                500,
                "config_invalid",
                "plugins.root_dir 为空 —— 配置被写坏了",
            ));
        }
        // 上游做 `expanduser()`（`~` 展开）。容器里 root_dir 是绝对路径，
        // 而 `~` 在 TOML 里也要用户自己写对，所以本仓不实现展开 —— 换成
        // 「原样当作路径」，行为对容器部署完全一致。
        Ok(PathBuf::from(raw))
    }

    /// 已启用的插件 id 集合（`plugins.enabled`）。
    fn enabled_ids(&self) -> Result<HashSet<String>, ServiceError> {
        let snapshot = self.config.snapshot()?;
        let list = snapshot
            .get("plugins")
            .and_then(|plugins| plugins.get("enabled"))
            .and_then(Value::as_array);
        Ok(list
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// 概要：清单 + 是否启用。
    fn summary_of(&self, scanned: &ScannedPlugin, enabled: bool) -> PluginSummary {
        let manifest = scanned.manifest.as_ref();
        PluginSummary {
            plugin_id: scanned.plugin_id.clone(),
            display_name: manifest.map_or_else(
                || scanned.plugin_id.clone(),
                |manifest| manifest.display_name.clone(),
            ),
            // 上游：读不出清单时是字面量 `"unknown"`（不是空串）——
            // 客户端据此显示「版本未知」。
            version: manifest.map_or_else(|| "unknown".to_owned(), |m| m.version.clone()),
            host_api_version: manifest
                .and_then(|m| m.host_api_version)
                .unwrap_or_default(),
            enabled,
            // 见模块文档：现在只有「清单坏掉」这一种来源。
            load_status: if scanned.manifest_error.is_some() {
                "error".to_owned()
            } else {
                "ok".to_owned()
            },
            load_error: scanned.manifest_error.clone(),
            release_api_url: manifest.and_then(|m| m.release_api_url.clone()),
        }
    }

    fn detail_of(&self, scanned: ScannedPlugin, enabled: bool) -> PluginDetail {
        let manifest: Option<&PluginManifest> = scanned.manifest.as_ref();
        PluginDetail {
            summary: self.summary_of(&scanned, enabled),
            requires_python: manifest.and_then(|m| m.requires_python.clone()),
            author: manifest.and_then(|m| m.author.clone()),
            homepage: manifest.and_then(|m| m.homepage.clone()),
            // 读不出清单时给空对象（上游同样）—— 而不是 `null`：
            // 客户端的字段类型是 `dict`。
            manifest: manifest
                .map(|m| m.raw.clone())
                .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
            data_dir: scanned
                .dir
                .join(crate::installer::DATA_DIR_NAME)
                .to_string_lossy()
                .into_owned(),
        }
    }

    /// 该插件是否存在。不存在就报 404（上游 `get_plugin` 返回 None → 路由 404）。
    ///
    /// # 先查 id 格式，再看目录
    ///
    /// 不查格式直接拼路径的话，`PATCH /system/plugins/..%2F..%2Fetc?enabled=true`
    /// 会让 `root.join("../../etc")` **跑到插件根目录之外**去读文件。
    /// 这同样是上游 `_set_enabled_checked` 先过 `PLUGIN_ID_PATTERN` 的理由。
    fn require_installed(&self, plugin_id: &str) -> Result<ScannedPlugin, ServiceError> {
        if !is_valid_plugin_id(plugin_id) {
            return Err(unknown_plugin(plugin_id));
        }
        let root = self.root_dir()?;
        inventory::read_one(&root, plugin_id).ok_or_else(|| unknown_plugin(plugin_id))
    }

    /// 写 `plugins.enabled`。
    fn write_enabled(&self, plugin_id: &str, enabled: bool) -> Result<(), ServiceError> {
        self.config.update_plugins_section(|plugins| {
            let mut list: Vec<String> = plugins
                .get("enabled")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if enabled {
                // 幂等：已启用时重复启用不产生重复项（上游 `enabled` 无重复是
                // 配置校验的一部分，重复了会被 `validate_strict` 拒掉）。
                if !list.iter().any(|id| id == plugin_id) {
                    list.push(plugin_id.to_owned());
                }
            } else {
                list.retain(|id| id != plugin_id);
            }
            plugins.insert("enabled".to_owned(), Value::Array(json_strings(&list)));
            Ok(())
        })?;
        Ok(())
    }
}

impl PluginAdmin for PluginAdminService {
    /// 读插件私有配置。上游 `get_plugin_settings`（`manager.py:368-373`）。
    fn get_plugin_settings(
        &self,
        plugin_id: &str,
    ) -> Result<sm_service::system::plugins::PluginSettingsBundle, ServiceError> {
        self.require_installed(plugin_id)?;
        let snapshot = self.config.snapshot()?;
        let settings = snapshot
            .get("plugins")
            .and_then(|plugins| plugins.get("settings"))
            .and_then(|settings| settings.get(plugin_id))
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        // schema：注册时由宿主落盘到 `<data_dir>/settings-schema.json`
        // （sm-server 的 `admit`），读不到 = 插件没声明或没落成 —— 键省略。
        let schema_path = self
            .root_dir()?
            .join(plugin_id)
            .join("data")
            .join("settings-schema.json");
        let schema = std::fs::read_to_string(&schema_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        // defaults：从 schema 里带 `default` 的字段提取 `{key: default}` ——
        // 上游 `settings_defaults(model)` 的对位物（那边从 pydantic 模型读，
        // 这边从落盘的字段清单读，同一个信息源）。
        let defaults = schema.as_ref().and_then(|fields| {
            let map: serde_json::Map<String, Value> = fields
                .as_array()?
                .iter()
                .filter_map(|field| {
                    let key = field.get("key")?.as_str()?;
                    let default = field.get("default")?;
                    if default.is_null() {
                        return None;
                    }
                    Some((key.to_owned(), default.clone()))
                })
                .collect();
            (!map.is_empty()).then_some(Value::Object(map))
        });
        Ok(sm_service::system::plugins::PluginSettingsBundle {
            settings,
            schema,
            defaults,
        })
    }

    /// 整体替换插件私有配置并落盘。上游 `set_plugin_settings`（`manager.py:393-401`）。
    fn set_plugin_settings(&self, plugin_id: &str, values: &Value) -> Result<Value, ServiceError> {
        self.require_installed(plugin_id)?;
        let Some(values) = values.as_object() else {
            // 上游 Body(...) 直接收 dict；非对象 = 调用方 bug 级别的形状错误。
            return Err(ServiceError::from_status(
                422,
                "invalid_plugin_settings",
                "插件设置必须是对象",
            ));
        };
        let values = values.clone();
        self.config.update_plugins_section(|plugins| {
            let settings = plugins
                .entry("settings")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            settings
                .as_object_mut()
                .expect("settings 段是对象（schema 约定）")
                .insert(plugin_id.to_owned(), Value::Object(values.clone()));
            Ok(())
        })?;
        Ok(Value::Object(values))
    }
    fn list(&self) -> Result<Vec<PluginSummary>, ServiceError> {
        let root = self.root_dir()?;
        let enabled = self.enabled_ids()?;
        Ok(inventory::scan(&root)
            .iter()
            .map(|scanned| self.summary_of(scanned, enabled.contains(&scanned.plugin_id)))
            .collect())
    }

    fn detail(&self, plugin_id: &str) -> Result<Option<PluginDetail>, ServiceError> {
        // 与 `require_installed` 不同：这里「不存在」是 `Ok(None)`（由路由转
        // 404），而不是错误 —— 上游 `get_plugin` 也是返回 None。
        if !is_valid_plugin_id(plugin_id) {
            return Ok(None);
        }
        let root = self.root_dir()?;
        let Some(scanned) = inventory::read_one(&root, plugin_id) else {
            return Ok(None);
        };
        let enabled = self.enabled_ids()?.contains(&scanned.plugin_id);
        Ok(Some(self.detail_of(scanned, enabled)))
    }

    fn set_enabled(&self, plugin_id: &str, enabled: bool) -> Result<PluginSummary, ServiceError> {
        // 先确认装了（不装就 404），再写配置 —— 反过来会往配置里塞一个
        // 根本不存在的插件 id，而它下次启动时会被静默忽略。
        let scanned = self.require_installed(plugin_id)?;
        self.write_enabled(&scanned.plugin_id, enabled)?;
        Ok(self.summary_of(&scanned, enabled))
    }

    fn prepare_upload_slot(&self) -> Result<PathBuf, ServiceError> {
        let dir = self
            .root_dir()?
            .join(installer::STAGING_DIR_NAME)
            .join(UPLOAD_SUBDIR);
        // 如果目录已存在且可写，直接用；只有不存在时才创建
        // 避免 create_dir_all 在某些环境下对已存在目录误报权限错误
        if !dir.is_dir() {
            std::fs::create_dir_all(&dir).map_err(|error| {
                ServiceError::from_status(
                    500,
                    "internal_error",
                    format!("创建上传暂存目录失败 {}: {error}", dir.display()),
                )
            })?;
        }
        prune_stale_uploads(&dir);
        // 上游用 `uuid.uuid4().hex` —— 并发上传必须互不撞名。
        Ok(dir.join(format!("{}.zip", uuid::Uuid::new_v4().simple())))
    }

    fn archive_size_limit(&self) -> u64 {
        // 单一真相在 `Limits`（上游 `MAX_ARCHIVE_BYTES = 100 MiB`）。
        installer::Limits::default().archive_bytes
    }

    fn install_zip(
        &self,
        zip_path: &Path,
        sha256: Option<&str>,
        enable: bool,
    ) -> Result<PluginInstallOutcome, ServiceError> {
        let root = self.root_dir()?;
        let (manifest, staging) =
            installer::unpack(&root, zip_path, sha256).map_err(install_failed)?;
        let restart = pending_restart_for(Some(&manifest));
        // 校验（含入口文件）在 `unpack` 里已经做完 —— 上游那一步是 Python 侧的
        // `check_plugin_dir`（import 试加载），Rust 侧没有 import，对应物是
        // `unpack` 的 Package 阶段（缺入口可执行文件就失败）。
        installer::publish(&root, &staging, &manifest.plugin_id).map_err(install_failed)?;
        if enable {
            self.write_enabled(&manifest.plugin_id, true)?;
        }
        Ok(outcome(manifest.plugin_id, manifest.version, restart))
    }

    fn upgrade_zip(
        &self,
        plugin_id: &str,
        zip_path: &Path,
        sha256: Option<&str>,
    ) -> Result<PluginInstallOutcome, ServiceError> {
        // 不在装的插件不能「升级」：上游先 `get_plugin` 判 404
        // （`plugins.py:150-151`），否则升级包会被当成一次全新安装。
        let current = self.require_installed(plugin_id)?;
        let Some(current_manifest) = current.manifest.as_ref() else {
            return Err(upgrade_failed(
                plugin_id,
                "已安装插件的清单无效，无法比较版本",
            ));
        };
        let current_version = current_manifest.version.clone();

        let root = self.root_dir()?;
        let (manifest, staging) = installer::unpack(&root, zip_path, sha256)
            .map_err(|error| upgrade_failed(plugin_id, &error.message))?;

        // 下面两条失败都要**清掉暂存目录** —— 上游 `except` 里的
        // `shutil.rmtree(staging)`。不清的话，下一次安装会先删它；而「下一次」
        // 可能永远不来，`.staging/` 就一直留着半个包。
        if manifest.plugin_id != plugin_id {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(upgrade_failed(
                plugin_id,
                &format!(
                    "升级包 plugin_id 不匹配：期望 {plugin_id}，实际 {}",
                    manifest.plugin_id
                ),
            ));
        }
        if !versions::is_newer(&manifest.version, &current_version) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(upgrade_failed(
                plugin_id,
                &format!(
                    "升级包版本必须高于当前版本：当前 {current_version}，升级包 {}",
                    manifest.version
                ),
            ));
        }

        let restart = pending_restart_for(Some(&manifest));
        installer::publish(&root, &staging, plugin_id)
            .map_err(|error| upgrade_failed(plugin_id, &error.message))?;
        // 启停状态**保持不变**：上游传的是 `plugin_id in enabled`
        // （`manager.py:273`）—— 升级不该把一个被停用的插件顺带启用。
        Ok(outcome(plugin_id.to_owned(), manifest.version, restart))
    }

    fn remove_code(&self, plugin_id: &str) -> Result<(), ServiceError> {
        let scanned = self.require_installed(plugin_id)?;
        // **先停用，再删代码**：反过来的话，删到一半失败会在配置里留下一个
        // 「启用中但目录已残缺」的插件 —— 下次启动会照 enabled 去找它。
        self.write_enabled(&scanned.plugin_id, false)?;
        remove_code_in(&scanned.dir).map_err(|error| {
            ServiceError::from_status(
                500,
                "internal_error",
                format!("删除插件代码失败 {}: {error}", scanned.dir.display()),
            )
        })
    }
}

/// 删掉插件目录里除 `data/` 之外的一切（上游 `PluginManager._remove_locked`，
/// `manager.py:327-343`）。
///
/// # 两个分支的差别在「有没有 data/」
///
/// | 有 `data/` | 动作 |
/// |---|---|
/// | 有 | 只删**目录里的其它东西**，目录本身与 `data/` 留着 |
/// | 没有 | 连目录一起删 |
///
/// 留着空壳目录不是疏忽：重装时 `publish` 正是靠 `<root>/<id>/data` 把用户数据
/// 接回去的。而它**不会被误认为「装了」** —— [`crate::inventory`] 要求目录里
/// 有 `manifest.json` 才算装了，那个已经被删掉了。
fn remove_code_in(dir: &Path) -> std::io::Result<()> {
    let data = dir.join(installer::DATA_DIR_NAME);
    if !data.is_dir() {
        return std::fs::remove_dir_all(dir);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == data {
            continue;
        }
        // 符号链接按**文件**删（`remove_dir_all` 会跟着链接走进去，把包外的
        // 东西删掉）。上游同样先判 `is_symlink()`。
        let metadata = entry.metadata()?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// 安装结果。
fn outcome(
    plugin_id: String,
    version: String,
    pending_restart: Vec<String>,
) -> PluginInstallOutcome {
    PluginInstallOutcome {
        plugin_id,
        version,
        pending_restart,
    }
}

/// 安装失败 → **422** `plugin_install_failed`（上游路由的 `except Exception`）。
fn install_failed(error: installer::InstallError) -> ServiceError {
    ServiceError::validation(
        PLUGIN_INSTALL_FAILED,
        format!("插件安装失败: {}", describe(&error)),
    )
}

/// 升级失败 → **422** `plugin_upgrade_failed`。
fn upgrade_failed(plugin_id: &str, reason: &str) -> ServiceError {
    ServiceError::validation(
        PLUGIN_UPGRADE_FAILED,
        format!("插件升级失败 plugin_id={plugin_id}: {reason}"),
    )
}

/// 带上失败环节（`zip` / `manifest` / `extract` / `package`）。
///
/// 上游的错误消息也是这个形状（`PluginInstallError.__init__` 拼了 stage），
/// 而用户在「包有问题」时有用的信息正是**哪一环节**：是压缩包坏了，还是包里
/// 少了入口文件。
fn describe(error: &installer::InstallError) -> String {
    format!("{}（{} 阶段）", error.message, error.stage.as_str())
}

/// 清理超过 [`UPLOAD_STALE_SECONDS`] 的残留上传。
///
/// **失败一律忽略**：这只是顺手打扫。因为它失败而让一次新的上传被拒，是把
/// 「磁盘上有点垃圾」升级成了「装不了插件」。
fn prune_stale_uploads(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        // 时钟回拨（`modified` 在未来）时 `duration_since` 会 Err —— 那正是
        // 「这个文件不该被当垃圾删掉」，所以直接跳过。
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age.as_secs() > UPLOAD_STALE_SECONDS {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `manifest.dependencies` 非空时的重启目标。供将来的升级端点使用。
///
/// 上游 `pending_restart_for`：声明了依赖的插件要**整个容器**重建，其余只需
/// 重启 api 与 aps 两个进程。本仓只有一个进程，但语义照抄 —— 它是
/// 「这个包需要完整启动流程」的标记，而不是「重启几个进程」的字面意思。
pub fn pending_restart_for(manifest: Option<&PluginManifest>) -> Vec<String> {
    let declares_dependencies = manifest
        .and_then(|manifest| manifest.raw.get("dependencies"))
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty());
    if declares_dependencies {
        return RESTART_CONTAINER.iter().map(|s| (*s).to_owned()).collect();
    }
    RESTART_API_AND_APS
        .iter()
        .map(|s| (*s).to_owned())
        .collect()
}

/// 未知插件的 404（上游 `plugin_not_found`）。
fn unknown_plugin(plugin_id: &str) -> ServiceError {
    ServiceError::from_status(
        404,
        PLUGIN_NOT_FOUND,
        format!("未知插件 plugin_id={plugin_id}"),
    )
}

/// `Vec<String>` → JSON 数组。
fn json_strings(values: &[String]) -> Vec<Value> {
    values.iter().map(|v| Value::String(v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 临时插件根 + 临时配置文件，返回 `(root, config)`。
    ///
    /// 配置**手工拼 TOML 字符串**而不是引 `toml` crate 序列化：路径里的
    /// 反斜杠在 TOML 基本字符串里是转义符，所以统一换成 `/`（Windows 也认）。
    fn fixture(tag: &str, plugins_toml: &str) -> (PathBuf, ConfigService) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let base =
            std::env::temp_dir().join(format!("sm-plugins-admin-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("plugins");
        std::fs::create_dir_all(&root).expect("建插件根");
        let config_path = base.join("config.toml");
        let root_toml = root.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &config_path,
            format!("[plugins]\nroot_dir = \"{root_toml}\"\n{plugins_toml}"),
        )
        .expect("写配置");
        (root, ConfigService::new(&config_path))
    }

    fn install(root: &std::path::Path, plugin_id: &str) {
        let dir = root.join(plugin_id);
        std::fs::create_dir_all(&dir).expect("建插件目录");
        std::fs::write(
            dir.join("manifest.json"),
            json!({
                "plugin_id": plugin_id,
                "display_name": "测试插件",
                "version": "1.2.3",
                "author": "SakuraMedia",
                "dependencies": [],
            })
            .to_string(),
        )
        .expect("写清单");
    }

    fn service(tag: &str, plugins_toml: &str) -> (PathBuf, PluginAdminService) {
        let (root, config) = fixture(tag, plugins_toml);
        (root, PluginAdminService::new(config))
    }

    #[test]
    fn the_list_reflects_the_enabled_configuration() {
        let (root, admin) = service("list", "enabled = [\"local\"]\n");
        install(&root, "local");
        install(&root, "other");

        let list = admin.list().expect("列插件");
        assert_eq!(list.len(), 2);
        let local = list.iter().find(|p| p.plugin_id == "local").expect("local");
        assert!(local.enabled, "配置里启用了");
        assert_eq!(local.display_name, "测试插件");
        assert_eq!(local.version, "1.2.3");
        assert_eq!(local.load_status, "ok");
        assert!(local.load_error.is_none());
        let other = list.iter().find(|p| p.plugin_id == "other").expect("other");
        assert!(!other.enabled, "没在 enabled 里");
    }

    #[test]
    fn an_empty_plugin_root_is_an_empty_list() {
        let (root, admin) = service("empty", "");
        let _ = root;
        assert!(admin.list().expect("列插件").is_empty());
    }

    /// ★ 坏清单要出现在列表里且 `load_status = "error"`（用户得能看见它）。
    #[test]
    fn a_broken_manifest_shows_up_as_a_load_error() {
        let (root, admin) = service("broken", "");
        let dir = root.join("local");
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(dir.join("manifest.json"), "{ not json").expect("写坏清单");

        let list = admin.list().expect("列插件");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].plugin_id, "local", "读不出清单时用目录名");
        assert_eq!(list[0].version, "unknown");
        assert_eq!(list[0].host_api_version, 0);
        assert_eq!(list[0].load_status, "error");
        assert!(list[0].load_error.is_some());
    }

    #[test]
    fn detail_carries_the_whole_manifest_and_the_data_directory() {
        let (root, admin) = service("detail", "");
        install(&root, "local");

        let detail = admin.detail("local").expect("查详情").expect("应当存在");
        assert_eq!(detail.summary.plugin_id, "local");
        assert_eq!(detail.author.as_deref(), Some("SakuraMedia"));
        // 用 `Path` 比较而不是字符串后缀：Windows 上分隔符是 `\`。
        let data_dir = std::path::Path::new(&detail.data_dir);
        assert_eq!(data_dir.file_name().and_then(|n| n.to_str()), Some("data"));
        assert_eq!(
            data_dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("local"),
            "data 目录必须挂在插件目录下"
        );
        // 整份清单原文都在（含宿主不解析的那些字段）。
        assert_eq!(detail.manifest["author"], "SakuraMedia");
        assert!(detail.manifest["dependencies"].is_array());
    }

    #[test]
    fn detail_of_an_unknown_plugin_is_none_not_an_error() {
        let (root, admin) = service("detail-missing", "");
        install(&root, "local");
        assert!(admin.detail("nope").expect("查详情").is_none());
    }

    /// ★ 停用要**写进配置**，而且立刻能从列表里看出来。
    #[test]
    fn disabling_writes_the_config_and_is_visible_immediately() {
        let (root, admin) = service("disable", "enabled = [\"local\"]\n");
        install(&root, "local");

        let summary = admin.set_enabled("local", false).expect("停用");
        assert!(!summary.enabled);
        assert!(
            !admin.list().expect("列插件")[0].enabled,
            "停用后列表必须跟着变（每次操作都重读配置）"
        );

        // 再启用回来。
        let summary = admin.set_enabled("local", true).expect("启用");
        assert!(summary.enabled);
        assert!(admin.list().expect("列插件")[0].enabled);
    }

    /// 重复启用**不产生重复项** —— 重复项会被 `validate_strict` 拒掉，
    /// 于是「再点一次启用」会报一个看起来莫名其妙的校验错。
    #[test]
    fn enabling_twice_does_not_duplicate_the_entry() {
        let (root, admin) = service("enable-twice", "enabled = [\"local\"]\n");
        install(&root, "local");

        admin.set_enabled("local", true).expect("第一次");
        admin.set_enabled("local", true).expect("第二次应当幂等");

        let raw = admin.config.snapshot().expect("读配置")["plugins"]["enabled"].clone();
        assert_eq!(raw, json!(["local"]), "配置里只该有一份");
    }

    /// ★ 不存在的插件 → 404（上游 `plugin_not_found`），且**不写配置**。
    #[test]
    fn enabling_an_unknown_plugin_is_a_404_and_leaves_the_config_alone() {
        let (root, admin) = service("unknown", "");
        install(&root, "local");

        let error = admin.set_enabled("nope", true).expect_err("应当 404");
        assert_eq!(error.status, 404);
        assert_eq!(error.code(), PLUGIN_NOT_FOUND);
        assert_eq!(
            admin.config.snapshot().expect("读配置")["plugins"]["enabled"],
            json!([]),
            "404 时不能往配置里塞东西"
        );
    }

    /// ★ 非法插件 id 也走 404，**绝不能拼进路径**。
    ///
    /// `root.join("../../etc")` 会跑到插件根目录之外 —— 虽然只是读，但那已经
    /// 是路径穿越（能读到宿主上任意一个 `manifest.json`）。
    #[test]
    fn a_malformed_plugin_id_is_rejected_before_touching_the_filesystem() {
        let (_, admin) = service("malformed", "");
        for bad in ["../../etc", "Local", "a/b", "..", ""] {
            let error = admin.set_enabled(bad, true).expect_err("应当 404");
            assert_eq!(error.status, 404, "{bad} 应当是 404");
            assert!(admin.detail(bad).expect("查详情").is_none(), "{bad}");
        }
    }

    /// `dependencies` 非空 → 重启目标是整个容器（上游 `pending_restart_for`）。
    #[test]
    fn declaring_dependencies_asks_for_a_container_restart() {
        let manifest = PluginManifest {
            plugin_id: "local".to_owned(),
            display_name: "本地".to_owned(),
            version: "1.0.0".to_owned(),
            host_api_version: None,
            release_api_url: None,
            requires_python: None,
            author: None,
            homepage: None,
            raw: json!({ "dependencies": ["libtorrent>=2.1.1,<3.0.0"] }),
        };
        assert_eq!(pending_restart_for(Some(&manifest)), vec!["container"]);

        let plain = PluginManifest {
            raw: json!({ "dependencies": [] }),
            ..manifest
        };
        assert_eq!(pending_restart_for(Some(&plain)), vec!["api", "aps"]);
        assert_eq!(
            pending_restart_for(None),
            vec!["api", "aps"],
            "读不出清单时走默认分支"
        );
    }

    /// 空 `root_dir` 是 500（配置坏了），**不是**「空插件列表」。
    ///
    /// 当成空目录的话，用户会看到「插件页什么都没有」而以为插件没装 ——
    /// 实际是配置被写坏了、每一个插件端点都在一个奇怪的位置找插件。
    #[test]
    fn an_empty_root_dir_in_config_is_a_server_error() {
        let (_, config) = fixture("bad-root", "");
        let broken = config.path().with_file_name("broken.toml");
        std::fs::write(&broken, "[plugins]\nroot_dir = \"   \"\n").expect("写坏配置");
        let admin = PluginAdminService::new(ConfigService::new(&broken));

        let error = admin.list().expect_err("空 root_dir 该报 500");
        assert_eq!(error.status, 500);
        assert_eq!(error.code(), "config_invalid");
    }

    // ============================================================ 安装 / 升级

    /// 造一个合法插件包：根部清单 + 入口可执行文件。
    ///
    /// 入口用**无扩展名**的那个名字 —— `entry_point_of` 两个都认，这样测试在
    /// Windows 与 Linux 上是同一条路径。
    fn make_package(path: &Path, plugin_id: &str, version: &str) {
        use std::io::Write as _;

        let file = std::fs::File::create(path).expect("建 zip");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("manifest.json", options).expect("写清单");
        writer
            .write_all(
                json!({
                    "plugin_id": plugin_id,
                    "display_name": "测试插件",
                    "version": version,
                })
                .to_string()
                .as_bytes(),
            )
            .expect("写清单内容");
        writer.start_file(plugin_id, options).expect("写入口");
        writer.write_all(b"binary").expect("写入口内容");
        writer.finish().expect("收尾");
    }

    fn package_in(dir: &Path, name: &str, plugin_id: &str, version: &str) -> PathBuf {
        let path = dir.join(format!("{name}.zip"));
        make_package(&path, plugin_id, version);
        path
    }

    /// ★ 装一个包：目录出现、写进 `enabled`、结果里的版本与重启目标正确。
    #[test]
    fn installing_a_package_publishes_it_and_enables_it() {
        let (root, admin) = service("install", "");
        let zip_path = package_in(&root, "local-1.0.0", "local", "1.0.0");

        let outcome = admin
            .install_zip(&zip_path, None, true)
            .expect("安装应当成功");

        assert_eq!(outcome.plugin_id, "local");
        assert_eq!(outcome.version, "1.0.0");
        // 没有 `dependencies` → 只重启 api 与 aps（上游 `pending_restart_for`）。
        assert_eq!(outcome.pending_restart, vec!["api", "aps"]);
        assert!(
            root.join("local").join("manifest.json").is_file(),
            "包应当被发布到插件根下"
        );
        assert!(root.join("local").join("local").is_file(), "入口文件也在");
        assert!(
            admin.list().expect("列插件")[0].enabled,
            "enable=true 要写进配置"
        );
    }

    #[test]
    fn installing_with_enable_false_leaves_the_plugin_disabled() {
        let (root, admin) = service("install-no-enable", "");
        let zip_path = package_in(&root, "local-1.0.0", "local", "1.0.0");

        admin
            .install_zip(&zip_path, None, false)
            .expect("安装应当成功");

        assert!(
            !admin.list().expect("列插件")[0].enabled,
            "enable=false 不该写进 enabled"
        );
    }

    /// 坏包 → 422 `plugin_install_failed`，且消息里带**失败环节**。
    #[test]
    fn a_broken_package_reports_install_failed_with_the_stage() {
        let (root, admin) = service("install-broken", "");
        let zip_path = root.join("broken.zip");
        std::fs::write(&zip_path, b"not a zip at all").expect("写垃圾文件");

        let error = admin
            .install_zip(&zip_path, None, true)
            .expect_err("坏包该拒");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), PLUGIN_INSTALL_FAILED);
        // 用户需要知道是「压缩包坏了」还是「包里少了东西」。
        assert!(
            error.api.message.contains("zip 阶段"),
            "消息里要带环节：{}",
            error.api.message
        );
    }

    /// sha256 不匹配 → 422（上游 `_validate_archive`）。
    #[test]
    fn a_sha256_mismatch_is_refused() {
        let (root, admin) = service("install-sha", "");
        let zip_path = package_in(&root, "local-1.0.0", "local", "1.0.0");

        let error = admin
            .install_zip(&zip_path, Some(&"0".repeat(64)), true)
            .expect_err("摘要不符该拒");
        assert_eq!(error.code(), PLUGIN_INSTALL_FAILED);
        assert!(!root.join("local").exists(), "拒绝时不该留下插件目录");
    }

    /// ★ 升级要**保留 `data/`**（发布那一步的职责，这里验跨层行为没被破坏）。
    #[test]
    fn upgrading_keeps_the_data_directory_and_bumps_the_version() {
        let (root, admin) = service("upgrade", "enabled = [\"local\"]\n");
        let first = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin.install_zip(&first, None, true).expect("首次安装");

        let data = root.join("local").join(installer::DATA_DIR_NAME);
        std::fs::create_dir_all(&data).expect("建 data 目录");
        std::fs::write(data.join("state.json"), b"user data").expect("写用户数据");

        let second = package_in(&root, "local-2.0.0", "local", "2.0.0");
        let outcome = admin
            .upgrade_zip("local", &second, None)
            .expect("升级应当成功");

        assert_eq!(outcome.version, "2.0.0");
        assert_eq!(
            std::fs::read(data.join("state.json")).expect("data 必须还在"),
            b"user data"
        );
        assert_eq!(admin.list().expect("列插件")[0].version, "2.0.0");
        assert!(
            admin.list().expect("列插件")[0].enabled,
            "升级不该改启停状态"
        );
    }

    /// ★ 升级**不改变**启停状态：被停用的插件升级后还是停用的。
    ///
    /// 上游传的是 `plugin_id in enabled`（`manager.py:273`）—— 若这里写成
    /// `enable=true`，升级会把一个**故意停用**的插件悄悄启用起来。
    #[test]
    fn upgrading_leaves_a_disabled_plugin_disabled() {
        let (root, admin) = service("upgrade-disabled", "");
        let first = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin
            .install_zip(&first, None, false)
            .expect("首次安装（不启用）");

        let second = package_in(&root, "local-2.0.0", "local", "2.0.0");
        admin.upgrade_zip("local", &second, None).expect("升级");

        assert!(
            !admin.list().expect("列插件")[0].enabled,
            "升级不该顺带启用它"
        );
    }

    /// ★ 版本不高于当前 → 422，且**不覆盖**现有代码。
    #[test]
    fn upgrading_to_a_version_that_is_not_higher_is_refused() {
        let (root, admin) = service("upgrade-downgrade", "");
        let first = package_in(&root, "local-2.0.0", "local", "2.0.0");
        admin.install_zip(&first, None, true).expect("首次安装");

        let older = package_in(&root, "local-1.0.0", "local", "1.0.0");
        let error = admin
            .upgrade_zip("local", &older, None)
            .expect_err("降级该拒");
        assert_eq!(error.code(), PLUGIN_UPGRADE_FAILED);
        assert!(
            error.api.message.contains("必须高于当前版本"),
            "{}",
            error.api.message
        );

        let same = package_in(&root, "local-2.0.0-again", "local", "2.0.0");
        assert!(
            admin.upgrade_zip("local", &same, None).is_err(),
            "同版本不算升级"
        );
        assert_eq!(
            admin.list().expect("列插件")[0].version,
            "2.0.0",
            "代码没被旧包覆盖"
        );
    }

    /// 升级包属于另一个插件 → 422（上游 `manager.py:241-244`）。
    #[test]
    fn upgrading_with_a_package_for_another_plugin_is_refused() {
        let (root, admin) = service("upgrade-mismatch", "");
        let first = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin.install_zip(&first, None, true).expect("首次安装");

        let wrong = package_in(&root, "other-2.0.0", "other", "2.0.0");
        let error = admin
            .upgrade_zip("local", &wrong, None)
            .expect_err("plugin_id 不匹配该拒");
        assert_eq!(error.code(), PLUGIN_UPGRADE_FAILED);
        assert!(error.api.message.contains("plugin_id 不匹配"));
        assert!(!root.join("other").exists(), "不该顺手把 other 装进来");
    }

    /// 没装的插件不能「升级」→ **404**（上游先 `get_plugin` 判存在）。
    #[test]
    fn upgrading_an_unknown_plugin_is_a_404() {
        let (root, admin) = service("upgrade-unknown", "");
        let zip_path = package_in(&root, "ghost-1.0.0", "ghost", "1.0.0");

        let error = admin
            .upgrade_zip("ghost", &zip_path, None)
            .expect_err("没装该 404");
        assert_eq!(error.status, 404);
        assert_eq!(error.code(), PLUGIN_NOT_FOUND);
        assert!(!root.join("ghost").exists(), "404 时不该把它当新安装装进来");
    }

    /// 升级校验失败时**清掉暂存目录**（上游 `shutil.rmtree(staging)`）。
    #[test]
    fn a_failed_upgrade_cleans_up_its_staging_directory() {
        let (root, admin) = service("upgrade-cleanup", "");
        let first = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin.install_zip(&first, None, true).expect("首次安装");

        let older = package_in(&root, "local-0.9.0", "local", "0.9.0");
        assert!(admin.upgrade_zip("local", &older, None).is_err());

        let staging = root.join(installer::STAGING_DIR_NAME).join("local");
        assert!(
            !staging.exists(),
            "校验失败后暂存目录必须被清掉，否则 .staging 会累积半个包"
        );
    }

    /// 上传槽位在 `<root>/.staging/uploads/` 下，且每次名字不同。
    #[test]
    fn the_upload_slot_lives_under_the_plugin_root_and_is_unique() {
        let (root, admin) = service("upload-slot", "");
        let first = admin.prepare_upload_slot().expect("申请槽位");
        let second = admin.prepare_upload_slot().expect("再申请一个");

        let uploads = root.join(installer::STAGING_DIR_NAME).join(UPLOAD_SUBDIR);
        assert!(uploads.is_dir(), "目录要被建出来");
        assert_eq!(first.parent(), Some(uploads.as_path()));
        assert_ne!(first, second, "并发上传必须互不撞名");
        assert!(
            first.extension().is_some_and(|ext| ext == "zip"),
            "扩展名要是 .zip：{first:?}"
        );
    }

    // ============================================================ 卸载

    /// ★ 删代码但**保留 `data/`**，并把它从 `enabled` 摘掉。
    ///
    /// 「保留 data/」是这步的全部要点：用户的数据不该因为一次误删消失，而重装时
    /// `publish` 会把它接回去。
    #[test]
    fn removing_a_plugin_keeps_the_data_directory_and_disables_it() {
        let (root, admin) = service("remove", "enabled = [\"local\"]\n");
        let zip_path = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin.install_zip(&zip_path, None, true).expect("安装");

        let data = root.join("local").join(installer::DATA_DIR_NAME);
        std::fs::create_dir_all(&data).expect("建 data 目录");
        std::fs::write(data.join("state.json"), b"user data").expect("写用户数据");

        admin.remove_code("local").expect("卸载");

        assert!(
            !root.join("local").join("manifest.json").exists(),
            "代码要删"
        );
        assert!(!root.join("local").join("local").exists(), "入口文件也要删");
        assert!(data.join("state.json").is_file(), "★ data/ 必须留下");
        assert!(
            admin.list().expect("列插件").is_empty(),
            "删掉 manifest 之后就不再算「装了」"
        );
        assert_eq!(
            admin.config.snapshot().expect("读配置")["plugins"]["enabled"],
            json!([]),
            "要从 enabled 里摘掉"
        );
    }

    /// 没有 `data/` 时整个目录都删掉（不留空壳）。
    #[test]
    fn removing_a_plugin_without_data_takes_the_whole_directory() {
        let (root, admin) = service("remove-nodata", "");
        let zip_path = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin.install_zip(&zip_path, None, true).expect("安装");

        admin.remove_code("local").expect("卸载");

        assert!(!root.join("local").exists(), "没有 data/ 就该整个删掉");
    }

    /// 删了再装回来：用户数据接回新安装。
    ///
    /// 这是「保留 `data/`」的**目的** —— 单看上一个测试只是「没删 data 目录」，
    /// 这一条才证明它有用。
    #[test]
    fn reinstalling_after_removal_picks_the_data_back_up() {
        let (root, admin) = service("remove-reinstall", "");
        let first = package_in(&root, "local-1.0.0", "local", "1.0.0");
        admin.install_zip(&first, None, true).expect("首次安装");
        let data = root.join("local").join(installer::DATA_DIR_NAME);
        std::fs::create_dir_all(&data).expect("建 data 目录");
        std::fs::write(data.join("state.json"), b"user data").expect("写用户数据");

        admin.remove_code("local").expect("卸载");
        let again = package_in(&root, "local-1.0.0-again", "local", "1.0.0");
        admin.install_zip(&again, None, true).expect("重装");

        assert_eq!(
            std::fs::read(data.join("state.json")).expect("data 必须接回来"),
            b"user data"
        );
    }

    /// 没装的插件 → 404（上游 `_remove_locked` 开头就判 `manifest.json` 在不在）。
    #[test]
    fn removing_an_unknown_plugin_is_a_404() {
        let (root, admin) = service("remove-unknown", "");
        install(&root, "local");

        let error = admin.remove_code("ghost").expect_err("没装该 404");
        assert_eq!(error.status, 404);
        assert_eq!(error.code(), PLUGIN_NOT_FOUND);
    }

    /// 非法插件 id 同样 404（不拼路径出去）。
    #[test]
    fn removing_a_malformed_plugin_id_is_a_404() {
        let (_, admin) = service("remove-malformed", "");
        for bad in ["../../etc", "Local", "..", ""] {
            let error = admin.remove_code(bad).expect_err("非法 id 该 404");
            assert_eq!(error.status, 404, "{bad}");
        }
    }

    /// ★ 超过 24 小时的残留上传会被清掉，新的不会被清。
    #[test]
    fn stale_uploads_are_pruned_but_fresh_ones_survive() {
        let (_, admin) = service("upload-prune", "");
        let slot = admin.prepare_upload_slot().expect("申请槽位");
        let uploads = slot.parent().expect("有父目录").to_path_buf();

        let stale = uploads.join("stale.zip");
        std::fs::write(&stale, b"half a package").expect("写残留");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("打开残留")
            .set_modified(old)
            .expect("改 mtime");

        // 下一次申请槽位时顺手打扫。
        let fresh = admin.prepare_upload_slot().expect("再申请槽位");

        assert!(!stale.exists(), "超过 24 小时的残留该被清掉");
        assert!(fresh.parent().is_some(), "新槽位仍然可用");
        assert!(
            !uploads.join("stale.zip").exists(),
            "不该只清一半 —— 残留必须真的没了"
        );
    }
}
