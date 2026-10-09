//! 卸载插件前的**占用检查** + 删除（上游 `system/plugin_removal_service.py`，95 行）。
//!
//! # ⚠️ 这一节整体重写过：骨架期把它的职责建错了
//!
//! 骨架期的模块文档写的是「卸载一个插件要做**四件事**：停进程 → 释放字段主权 →
//! 清扩展点数据 → 删配置行」，并据此定义了一个七字段的 `PluginRemovalReport`
//!（含 `incomplete` 与 `mark_incomplete`）与两条单元测试。
//!
//! **那四步在上游不存在。** 逐条核对（`plugin_removal_service.py:43-48`）：
//!
//! ```python
//! @classmethod
//! def remove(cls, plugin_id: str) -> None:
//!     manager = PluginManager()
//!     cls._ensure_not_in_use(manager, plugin_id)   # 1. 占用检查 → 409
//!     manager.remove(plugin_id)                    # 2. 停用 + 删代码（保留 data/）
//! ```
//!
//! - **「停进程」不是卸载的一部分**：`PluginManager.remove` 只删文件、写
//!   `enabled`，进程要等重启（路由回 `pending_restart: ["api","aps"]`）。
//! - **「释放字段主权」不在这个调用链上**：`release_plugin_owners` 全仓只有
//!   **一个**调用点 —— `commands.py:524`，一个 CLI 子命令
//!   （`movie_ownership_gateway.py:189` 的文档写着「清理端点，CLI 调用」）。
//!   没有 HTTP 路由，也没有任何地方在卸载时调它。
//! - **「清扩展点数据 / 删配置行」**：`ranking_source` / `metadata_source` 的
//!   删除在卸载路径上**零命中**；配置只剩 `manager.remove` 内部那次
//!   `_set_enabled(plugin_id, False)`。
//!
//! 所以本文件按上游重写成两步。**这不是「简化」**：多做的那些事会改变行为
//! （比如卸载时顺手释放字段主权，等于替管理员做了一个他没要求的、影响所有
//! 影片的动作）。CLI 那条路仍然是它自己的入口，将来若要做，应该照上游做成
//! 独立命令，而不是塞进 `DELETE /system/plugins/{id}`。
//!
//! # 占用检查为什么是必要的
//!
//! 一个 provider 插件可能还在给媒体库供数据。删掉它的代码之后，那些库的
//! `provider_key` 就指向一个不存在的插件 —— 表现为「媒体库页面报 503
//! `provider_not_installed`」，而用户已经不知道是为什么。所以宁可拒删，
//! 并把「谁在引用它」原样报出来。

use std::collections::HashSet;

use serde_json::{Map, Value};

use sm_db::repo::{DownloadClientRepository, MediaLibraryRepository, MediaRepository};
use sm_db::Db;

use crate::error::ServiceError;
use crate::system::plugins::PluginAdmin;

/// 插件仍被引用的错误码（上游 `plugin_in_use`，409）。
pub const PLUGIN_IN_USE: &str = "plugin_in_use";

/// 「plugin_id → provider_key」的**反向索引**。注入 seam。
///
/// # 上游有两条来源，Rust 侧两条都还没有
///
/// 上游 `_provider_keys`（`plugin_removal_service.py:74-93`）先问活跃注册表
/// `MEDIA_PROVIDER_REGISTRY.provider_keys_for_plugin(plugin_id)`；拿不到就
/// 回落到 `check_plugin_dir()` —— **试加载插件目录**、扫它的扩展点里的
/// `MEDIA_PROVIDER_EXTENSION_KEY`。
///
/// Rust 侧：
///
/// | 来源 | 现状 |
/// |---|---|
/// | 活跃注册表 | `MediaLibraryRegistry` **全仓没有实现**（`docs/handoff.md` §7.2），它现在连 `library_for(provider_key)` 这一向都没人接，更没有反向索引 |
/// | 试加载插件目录 | Rust 侧没有「import 插件」这回事；要拿扩展点得**拉起进程**问它 `Register` —— 那比这个安全检查值得的开销大 |
///
/// 所以它是一个**显式 seam**，而不是藏在 `unwrap_or_default()` 里：组合根
/// 在拿到 provider 注册表之后把它接上（那时也才谈得上「插件正在提供哪些
/// provider_key」）。现在传 [`NoProviderKeys`]。
///
/// # 现在传 [`NoProviderKeys`] 意味着什么
///
/// **这一条安全检查不生效**：`provider_keys` 恒空 → 上游的两处 `if not ...:
/// return` 会立刻放行 → 一个仍被媒体库引用的插件**能被删掉**。
///
/// 这与上游**不完全**等价：上游在「插件目录加载失败」时返回 `()` 也会放行
/// （它的 `except (OSError, PluginLoadError, ValueError)`），但一个**正在
/// 服役**的插件在上游是走第一支的，能查到键。所以这是一个**已知缺口**，
/// 记在这里与 `docs/handoff.md`，不是「上游也这样」。
pub trait ProviderKeyIndex: Send + Sync {
    /// 该插件提供了哪些 provider key。拿不到就返回空表。
    fn provider_keys_for_plugin(&self, plugin_id: &str) -> Vec<String>;
}

/// 恒空的 [`ProviderKeyIndex`]：provider 注册表还没接上时用它。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProviderKeys;

impl ProviderKeyIndex for NoProviderKeys {
    fn provider_keys_for_plugin(&self, _plugin_id: &str) -> Vec<String> {
        Vec::new()
    }
}

/// 插件卸载。上游 `PluginRemovalService`。
pub struct PluginRemovalService;

impl PluginRemovalService {
    /// 卸载一个插件。上游 `PluginRemovalService.remove`（`:43-48`）。
    ///
    /// 两步：占用检查（可能 409）→ 删代码 + 停用。
    ///
    /// # 顺序不能换
    ///
    /// 先检查再删。反过来（先删再检查）在冲突时已经删掉了代码，而客户端拿到的
    /// 是一个「409 但插件已经没了」的状态 —— 比拒绝删除糟糕得多。
    pub async fn remove(
        db: &Db,
        admin: &dyn PluginAdmin,
        provider_keys: &dyn ProviderKeyIndex,
        plugin_id: &str,
    ) -> Result<(), ServiceError> {
        let keys = provider_keys.provider_keys_for_plugin(plugin_id);
        Self::ensure_not_in_use(db, plugin_id, &keys).await?;
        // 删代码**保留 `data/`**，并把插件从 `plugins.enabled` 摘掉。
        admin.remove_code(plugin_id)
    }

    /// 该插件是否仍在被媒体库引用。被引用 → **409 [`PLUGIN_IN_USE`]**。
    ///
    /// 上游 `_ensure_not_in_use`（`:50-72`）。三个提前返回点，语义各不相同：
    ///
    /// | 条件 | 上游 | 含义 |
    /// |---|---|---|
    /// | 没有 provider key | `return` | 不是 provider 插件，与本检查无关 |
    /// | 没有引用它的库 | `return` | 有 key 但没配库 —— 可以删 |
    /// | 有库 | **抛** `PluginInUseError` | 拒删，并把详情报出来 |
    pub async fn ensure_not_in_use(
        db: &Db,
        plugin_id: &str,
        provider_keys: &[String],
    ) -> Result<(), ServiceError> {
        if provider_keys.is_empty() {
            return Ok(());
        }
        let library_ids = MediaLibraryRepository::new(db.clone())
            .ids_by_provider_keys(provider_keys)
            .await?;
        if library_ids.is_empty() {
            return Ok(());
        }

        let media_count = MediaRepository::new(db.clone())
            .count_in_libraries(&library_ids)
            .await?;
        let download_client_count = DownloadClientRepository::new(db.clone())
            .count_in_libraries(&library_ids)
            .await?;

        Err(in_use(
            plugin_id,
            provider_keys,
            &library_ids,
            media_count,
            download_client_count,
        ))
    }
}

/// 409 `plugin_in_use`。details 的五个键与上游 `PluginInUseError.details`
/// 逐字一致（`plugin_removal_service.py:23-29`）。
fn in_use(
    plugin_id: &str,
    provider_keys: &[String],
    library_ids: &[i32],
    media_count: i64,
    download_client_count: i64,
) -> ServiceError {
    let mut details = Map::new();
    details.insert("plugin_id".to_owned(), Value::from(plugin_id));
    details.insert("provider_keys".to_owned(), json_strings(provider_keys));
    details.insert(
        "library_ids".to_owned(),
        Value::Array(library_ids.iter().map(|id| Value::from(*id)).collect()),
    );
    details.insert("media_count".to_owned(), Value::from(media_count));
    details.insert(
        "download_client_count".to_owned(),
        Value::from(download_client_count),
    );

    // 文案照上游：**两个计数都要报**。只报媒体数会让「库下只有下载器」的
    // 实例看到 `0 个媒体`，读起来像「没有东西引用它」。
    ServiceError::conflict(
        PLUGIN_IN_USE,
        format!(
            "插件仍被 {} 个媒体库引用（{} 个媒体、{} 个下载客户端），无法删除；\
             请先迁移或删除相关媒体库。",
            library_ids.len(),
            media_count,
            download_client_count
        ),
        Some(details),
    )
}

/// `&[String]` → JSON 数组。
fn json_strings(values: &[String]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|value| Value::String(value.clone()))
            .collect(),
    )
}

/// 去掉重复的 provider key（上游 `tuple(sorted(...))`）。
///
/// 上游在 `_provider_keys` 的第二支里排序去重；第一支信任注册表。这里提供
/// 同一个工具，供将来的实现在把键交给本服务之前归一化。
pub fn normalize_provider_keys(keys: impl IntoIterator<Item = String>) -> Vec<String> {
    let unique: HashSet<String> = keys.into_iter().collect();
    let mut sorted: Vec<String> = unique.into_iter().collect();
    sorted.sort();
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sorted_and_deduplicated() {
        let keys =
            normalize_provider_keys(["zeta".to_owned(), "alpha".to_owned(), "zeta".to_owned()]);
        assert_eq!(keys, vec!["alpha".to_owned(), "zeta".to_owned()]);
    }

    /// 恒空实现存在，且**真的恒空** —— 它是「注册表没接上」那个状态的
    /// 显式表达，不是占位。
    #[test]
    fn the_no_op_key_index_returns_nothing() {
        assert!(NoProviderKeys.provider_keys_for_plugin("local").is_empty());
    }
}
