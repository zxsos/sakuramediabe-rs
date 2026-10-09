//! 插件管理的**契约层**：路由要的数据形状 + 宿主实现的操作接口。
//!
//! 上游对应 `src/api/routers/system/plugins.py` + `src/plugins/manager.py`。
//!
//! # 为什么是 trait，而不是在这里直接实现
//!
//! `sm-api` 与 `sm-service` 都**不能**依赖 `sm-plugins`（插件宿主实现）：
//!
//! | 谁 | 为什么不能 |
//! |---|---|
//! | `sm-api` | 分层：API 层不该看得见插件运行时 |
//! | `sm-service` | 会闭掉一条**将来**的环 —— 插件任务要接 cron 触发，那时 `sm-plugins → sm-scheduler → sm-service`，反向再依赖一次就成环（这条写在 `crates/sm-service/Cargo.toml` 里那条「刻意不依赖」的注释上） |
//!
//! 所以按本仓既有做法：**契约放在这里，实现在 `sm-plugins`，由组合根 `sm-server`
//! 注入 `AppState`** —— 与 `StorageGateway` / `DownloadCapabilityRegistry` /
//! `MediaLibraryRegistry` 三个能力注册表完全同一套。
//!
//! # 未注入时**不能**假装「一个插件都没装」
//!
//! `AppState` 里持有的是 `Option<Arc<dyn PluginAdmin>>`。`None` 只该出现在
//! 单测没注入、或组合根忘了接这两种场合 —— 而它们与「真的没装插件」是**三件
//! 不同的事**，却都会让插件页面空着。所以路由层用
//! [`plugin_admin_unavailable`] 明确报错，而不是返回空列表。

use serde::Serialize;

use crate::error::ServiceError;

/// 未注入插件管理实现时的错误码。
pub const PLUGIN_ADMIN_UNAVAILABLE: &str = "plugin_admin_unavailable";

/// 未知插件的错误码（上游 `plugin_not_found`）。
///
/// 放在契约层是因为**两侧都要用**：实现报它（服务层/宿主）、路由转 404 时
/// 也要认它。各写一份字面量，改一处就会漏一处。
pub const PLUGIN_NOT_FOUND: &str = "plugin_not_found";

/// 安装失败（422）。上游路由的 `except Exception` 分支（`plugins.py:129-130`）。
pub const PLUGIN_INSTALL_FAILED: &str = "plugin_install_failed";
/// 升级失败（422）。上游（`plugins.py:159-160`）。
///
/// 与安装**不共用**一个码：客户端对「装一个新的失败」与「把在用的升级坏了」
/// 的提示完全不同（后者常常要引导用户去回滚/手工恢复）。
pub const PLUGIN_UPGRADE_FAILED: &str = "plugin_upgrade_failed";
/// 上传超限（413）。上游 `_check_upload_size`（`plugins.py:43-55`）。
///
/// ⚠️ 与通用提取器超限用的 `http_error` 不是同一个码 —— 见
/// `sm_api::extract::receive_to_file` 的 `too_large_code` 参数文档。
pub const PLUGIN_TOO_LARGE: &str = "plugin_too_large";

/// 插件管理不可用的错误。
///
/// 用 500 而不是 503：这不是「依赖方暂时不可用」（那类错误的语义是「等会儿
/// 再试」），而是**服务装配漏了** —— 重试多少次结果都一样。
pub fn plugin_admin_unavailable() -> ServiceError {
    ServiceError::from_status(
        500,
        PLUGIN_ADMIN_UNAVAILABLE,
        "插件管理未注入：组合根必须调 AppState::with_plugin_admin",
    )
}

/// 插件的**概要**。上游 `PluginSummaryResource`（`schema/system/plugins.py`）。
///
/// # 字段名就是响应体的键
///
/// 客户端按这些键读，改名不会报错、只会让某一块显示成空白。`tests` 里有一条
/// 断言把键集合钉住。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginSummary {
    pub plugin_id: String,
    pub display_name: String,
    /// 读不出清单时是 `"unknown"`（上游同样处理）。
    pub version: String,
    /// `host_api_version`。**读不出清单时是 0**，不是 `null` —— 上游字段声明是
    /// `int`，客户端按整数格式化。
    ///
    /// ⚠️ 它是 **Python 侧的 ABI 版本**，不参与本仓的兼容性判定（那看
    /// `Register` 回显的 `abi_major`）。见 `sm_plugins::manifest` 的模块文档。
    pub host_api_version: i32,
    pub enabled: bool,
    /// `"ok"` 或 `"error"`（上游是 `str`，取值只有这两个）。
    pub load_status: String,
    /// 加载失败的原因。`load_status == "ok"` 时是 `None`。
    pub load_error: Option<String>,
    pub release_api_url: Option<String>,
}

/// 插件的**详情**。上游 `PluginDetailResource`。
///
/// `#[serde(flatten)]`：上游是继承（detail 就是 summary 多几个字段），
/// 客户端读的是同一个平铺对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginDetail {
    #[serde(flatten)]
    pub summary: PluginSummary,
    pub requires_python: Option<String>,
    pub author: Option<String>,
    pub homepage: Option<String>,
    /// **整份清单原文**。读不出清单时是 `{}`（上游同样回空对象）。
    pub manifest: serde_json::Value,
    /// 数据目录的绝对路径（宿主托管，重装保留）。
    pub data_dir: String,
}

/// 安装 / 升级 / 卸载的结果。上游 `PluginInstallResponse`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginInstallOutcome {
    pub plugin_id: String,
    pub version: String,
    /// 让这次变更生效需要重启哪些目标。
    ///
    /// 上游只可能有两种取值：`["api", "aps"]`（普通插件）或 `["container"]`
    /// （清单里声明了 `dependencies` 的插件 —— 它要在完整容器启动时先同步
    /// 依赖）。本仓没有 Python 依赖可同步，但**照抄这个语义**：声明了
    /// `dependencies` 的包在 Rust 侧同样意味着「作者认为它需要整个容器重建」。
    pub pending_restart: Vec<String>,
}

/// 普通插件变更后的重启目标。上游 `pending_restart_for` 的默认分支。
pub const RESTART_API_AND_APS: [&str; 2] = ["api", "aps"];
/// 声明了 `dependencies` 的插件的重启目标。
pub const RESTART_CONTAINER: [&str; 1] = ["container"];

/// 插件管理操作（宿主实现，组合根注入）。
///
/// # 为什么方法收 `&self` 而不是 `&mut self`
///
/// 实现里每次操作都重新读磁盘（配置与目录），进程内不缓存任何东西 ——
/// 缓存会让「另一个进程刚装了插件」看不见，而插件目录是**多进程共享**的
/// （宿主、将来的 CLI 工具、运维手工拷贝）。
pub trait PluginAdmin: Send + Sync {
    /// 列出插件根目录下所有**装了**的插件。上游 `PluginManager.list_plugins`。
    ///
    /// 根目录不存在时返回空表（新装实例的正常状态）。
    fn list(&self) -> Result<Vec<PluginSummary>, ServiceError>;

    /// 一个插件的详情。**不存在（或目录里没有清单）返回 `Ok(None)`** ——
    /// 由路由层转 404，而不是在这里报错。
    fn detail(&self, plugin_id: &str) -> Result<Option<PluginDetail>, ServiceError>;

    /// 启用 / 停用。上游 `PluginManager.set_enabled`。
    ///
    /// **写配置，不拉起或杀掉进程**：改动要重启才生效（与上游一致，也解释了
    /// 响应里的 `pending_restart`）。
    fn set_enabled(&self, plugin_id: &str, enabled: bool) -> Result<PluginSummary, ServiceError>;

    /// 为一次上传准备一个**暂存槽位**，返回可写的文件路径。
    ///
    /// 上游 `_upload_temp_path`（`plugins.py:58-69`）：在
    /// `<root>/.staging/uploads/` 下取一个随机文件名，并顺手清理超过 24 小时
    /// 的残留（上一次异常退出留下的半个包，不清就会一直累积）。
    ///
    /// # 为什么不是「路由自己拼一个 `temp_dir()`」
    ///
    /// 两个理由：`<root>/.staging` 与插件目录**同一个文件系统**（发布时
    /// `rename` 才可能是原子的），以及「哪些是残留上传」这个判断需要知道
    /// 插件根在哪 —— 那是配置里的值，路由层不该自己去读。
    ///
    /// 调用方负责在结束后删掉它（成功失败都要）。
    fn prepare_upload_slot(&self) -> Result<std::path::PathBuf, ServiceError>;

    /// 单个插件包的大小上限（字节）。
    ///
    /// 路由要在**读 body 之前**用它拦超限请求（`Content-Length` 预检），
    /// 而那个时刻它还没碰到任何与 zip 有关的东西 —— 所以这个数字必须由实现
    /// 提供，不能在路由里写字面量：`100 MiB` 是 `sm_plugins::installer::Limits`
    /// 的一部分，两处各写一份就会漂。
    fn archive_size_limit(&self) -> u64;

    /// 从 zip 安装一个插件。上游 `PluginManager.install_zip`（`manager.py:193-219`）。
    ///
    /// `enable` 为真时顺带把插件写进 `plugins.enabled`（上游 `_publish_staging`
    /// 的 `if enable`）。
    ///
    /// 失败一律是 **422 [`PLUGIN_INSTALL_FAILED`]**（上游路由的
    /// `except Exception`），而不是各种细分错误码。
    fn install_zip(
        &self,
        zip_path: &std::path::Path,
        sha256: Option<&str>,
        enable: bool,
    ) -> Result<PluginInstallOutcome, ServiceError>;

    /// 用 zip 升级已安装的插件。上游 `PluginManager.upgrade_zip`
    /// （`manager.py:221-274`）。
    ///
    /// 三条前置：插件已安装（否则 **404 [`PLUGIN_NOT_FOUND`]**）、包里的
    /// `plugin_id` 与它一致、包版本**严格高于**当前版本。启停状态**保持不变**
    /// （上游传的是 `plugin_id in enabled`）。
    fn upgrade_zip(
        &self,
        plugin_id: &str,
        zip_path: &std::path::Path,
        sha256: Option<&str>,
    ) -> Result<PluginInstallOutcome, ServiceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> PluginSummary {
        PluginSummary {
            plugin_id: "local".to_owned(),
            display_name: "本地存储".to_owned(),
            version: "1.2.3".to_owned(),
            host_api_version: 0,
            enabled: true,
            load_status: "ok".to_owned(),
            load_error: None,
            release_api_url: None,
        }
    }

    /// ★ 键名与上游 `PluginSummaryResource` 逐字一致。
    ///
    /// 少一个键或多一个键都不会报错 —— 只会让客户端上某一块显示成空白，
    /// 或者被「严格校验字段」的客户端直接拒收。这条把它钉住。
    #[test]
    fn the_summary_serializes_with_the_upstream_field_names() {
        let value = serde_json::to_value(summary()).expect("序列化");
        let keys: Vec<&str> = value
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "plugin_id",
                "display_name",
                "version",
                "host_api_version",
                "enabled",
                "load_status",
                "load_error",
                "release_api_url",
            ]
        );
    }

    /// 详情是**平铺**的（上游 detail 继承 summary，客户端读同一个对象）。
    #[test]
    fn the_detail_is_a_flattened_summary_plus_its_own_fields() {
        let detail = PluginDetail {
            summary: summary(),
            requires_python: None,
            author: Some("SakuraMedia".to_owned()),
            homepage: None,
            manifest: serde_json::json!({ "settings_model": "Settings" }),
            data_dir: "/data/plugins/local/data".to_owned(),
        };
        let value = serde_json::to_value(detail).expect("序列化");
        let object = value.as_object().expect("对象");

        // summary 的键在顶层，不在 `summary` 子对象里。
        assert!(object.contains_key("plugin_id"));
        assert!(!object.contains_key("summary"));
        for key in [
            "display_name",
            "version",
            "host_api_version",
            "enabled",
            "load_status",
            "load_error",
            "release_api_url",
            "requires_python",
            "author",
            "homepage",
            "manifest",
            "data_dir",
        ] {
            assert!(object.contains_key(key), "缺 {key}");
        }
    }

    /// `load_error` 为 `None` 时输出 **`null` 而不是省略**。
    ///
    /// 两个 settings 端点用 `exclude_none`，但**概要/详情不用** —— 上游只有那
    /// 两处声明了 `response_model_exclude_none=True`。差别在客户端侧是可见的：
    /// 「键不存在」和「键是 null」在强类型客户端里是两个不同的分支。
    #[test]
    fn absent_optional_fields_serialize_as_null_not_missing() {
        let value = serde_json::to_value(summary()).expect("序列化");
        let object = value.as_object().expect("对象");
        assert!(object.contains_key("load_error"));
        assert!(object["load_error"].is_null());
        assert!(object["release_api_url"].is_null());
    }

    /// `host_api_version` 读不出清单时是 **0**，不是 null（上游字段是 `int`）。
    #[test]
    fn a_missing_host_api_version_is_zero_not_null() {
        let value = serde_json::to_value(summary()).expect("序列化");
        assert_eq!(value["host_api_version"], 0);
        assert!(value["host_api_version"].is_i64());
    }
}
