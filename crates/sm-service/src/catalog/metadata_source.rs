//! 元数据来源：JavDB 优先，插件结果临时交付（上游 `catalog/metadata_source_service.py`，242 行）。
//!
//! # 核心约束：插件交付的元数据**只在本次调用期间有效**
//!
//! 上游 docstring：「JavDB 优先；插件结果只在本次调用期间交付给宿主。」
//!
//! `fetch_plugin` / `fetch` 都是 **context manager**：进入时向插件索取文件，
//! 退出时清理。宿主**不允许**持有跨越调用边界的插件文件句柄 —— 插件可能在
//! 下一次请求里换掉目录内容。
//!
//! ⚠️ 因此这两个方法**不能**返回 `&Path` 或把路径存进结构体。必须用闭包把
//! 生命周期圈住。
//!
//! # 交付文件必须在插件自己的目录下
//!
//! 上游校验交付文件落在
//! `<plugin>/data/metadata-tmp/<请求目录>/` 内，**用 Pillow 校验后清理**。
//!
//! 三个环节缺一不可：
//!
//! | 环节 | 防的是 |
//! |---|---|
//! | 路径前缀校验 | 插件让宿主读任意文件（`../../etc/passwd`） |
//! | Pillow 校验 | 「这是图片」只是插件的一句话，不验就是信任它 |
//! | 退出时清理 | 临时文件堆积占满磁盘 |
//!
//! # 错误用**自定义异常**而非 `ApiError`
//!
//! `MetadataSourceError` / `MetadataNotFoundError` / `MetadataRequestError`。
//! 因为这个模块被多个端点复用，HTTP 状态码由**调用方**决定 ——
//! 「按番号搜索时找不到」该是 404，「刷新时找不到」该是 502，语义不同。

use std::path::PathBuf;

/// 元数据来源错误基类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataSourceError {
    /// 该来源没有这条记录（**不是**故障）。
    NotFound,
    /// 调来源失败（网络、插件崩了）。
    RequestFailed(String),
    /// 插件交付的文件不合法（路径逃逸 / 不是图片 / 目录不对）。
    InvalidDelivery(String),
    /// 该来源未启用。
    Disabled(String),
}

/// JavDB 查询能力。**出网**。
pub trait MetadataProvider {
    /// 按番号取影片详情。
    fn get_movie_by_number(
        &self,
        movie_number: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError>;
    /// 按 JavDB id 取影片详情。
    fn get_movie_by_javdb_id(
        &self,
        javdb_id: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError>;
    /// 搜演员。
    fn search_actors(&self, keyword: &str) -> Result<Vec<serde_json::Value>, MetadataSourceError>;
}

/// 插件交付的元数据。**只在闭包内有效**。
pub struct PluginDelivery {
    /// 影片详情。
    pub detail: serde_json::Value,
    /// 来源标识（`javdb` / `plugin:xxx`），落进 `movie.metadata_source`。
    pub source: String,
    /// 交付的图片目录。**闭包退出后即失效。**
    pub delivery_dir: PathBuf,
    /// 原始 provider 句柄（调用方可能还要用）。
    pub provider: Option<serde_json::Value>,
}

/// 元数据来源服务。
// `sources` 尚未被方法体引用（`fetch_movie` 还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct MetadataSourceService {
    /// 已启用的插件来源。**从配置读**，不是硬编码列表。
    sources: Vec<RegisteredSource>,
}

/// 一个已启用的插件来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSource {
    pub plugin_id: String,
    /// 插件数据目录（交付文件必须落在它下面）。
    pub data_dir: PathBuf,
}

impl MetadataSourceService {
    /// 构造。
    pub fn new(sources: Vec<RegisteredSource>) -> Self {
        Self { sources }
    }

    /// 读配置里已启用的来源。`None` = 一个都没启用。
    pub fn enabled_plugin_sources(
        config: &serde_json::Value,
    ) -> Result<Vec<RegisteredSource>, MetadataSourceError> {
        let _ = config;
        todo!("骨架：读 plugins.enabled + 每个插件的 data_dir 约定")
    }

    /// 该插件来源是否启用。
    pub fn is_plugin_enabled(sources: &[RegisteredSource], plugin_id: &str) -> bool {
        sources.iter().any(|source| source.plugin_id == plugin_id)
    }

    /// ★ 向某个插件来源索取元数据，**闭包内有效**。
    ///
    /// 上游 `fetch_plugin(cls, plugin_id, movie_number)` 是 contextmanager。
    /// 闭包参数是 [`PluginDelivery`]。
    ///
    /// 三个交付校验都在这里做（见模块文档）：路径前缀、Pillow 校验、退出清理。
    pub async fn fetch_plugin<R>(
        &self,
        plugin_id: &str,
        movie_number: &str,
        consume: impl AsyncFnOnce(PluginDelivery) -> R,
    ) -> Result<R, MetadataSourceError> {
        let _ = (plugin_id, movie_number, consume);
        todo!("骨架：向插件索取 -> 校验交付目录前缀 -> Pillow 校验 -> use(delivery).await -> 清理目录")
    }

    /// ★ 按番号取元数据，**JavDB 优先**。
    ///
    /// 上游 `fetch(cls, movie_number, provider)`，同样是 contextmanager。
    /// 顺序：先 JavDB，**查不到**才走插件。
    pub async fn fetch<R>(
        &self,
        movie_number: &str,
        consume: impl AsyncFnOnce(PluginDelivery) -> R,
    ) -> Result<R, MetadataSourceError> {
        let _ = (movie_number, consume);
        todo!("骨架：先 JavDB(NotFound 不算错) -> 再按顺序试已启用插件 -> 都没有则 NotFound")
    }

    /// 按番号导入（JavDB 优先）。**不返回交付目录** —— 导入过程中用即可。
    pub async fn import_by_number(
        &self,
        movie_number: &str,
        import_service: &mut dyn crate::catalog::catalog_import::CatalogImport,
        force_subscribed: bool,
    ) -> Result<bool, MetadataSourceError> {
        let _ = (movie_number, import_service, force_subscribed);
        todo!("骨架：fetch() 闭包内调 import_movie_if_missing；闭包退出即清理交付文件")
    }

    /// 匹配演员并入库。返回入库的演员数。
    pub async fn match_actors(
        &self,
        keyword: &str,
        import_service: &mut dyn crate::catalog::catalog_import::CatalogImport,
    ) -> Result<usize, MetadataSourceError> {
        let _ = (keyword, import_service);
        todo!("骨架：search_actors -> 逐个 upsert_actor_from_javdb_resource")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(ids: &[&str]) -> Vec<RegisteredSource> {
        ids.iter()
            .map(|id| RegisteredSource {
                plugin_id: (*id).to_owned(),
                data_dir: PathBuf::from("/tmp").join(id),
            })
            .collect()
    }

    /// 只有**已启用**的来源可用。
    #[test]
    fn only_enabled_sources_are_used() {
        let enabled = sources(&["local", "115"]);
        assert!(MetadataSourceService::is_plugin_enabled(&enabled, "local"));
        assert!(!MetadataSourceService::is_plugin_enabled(
            &enabled, "unknown"
        ));
    }

    /// 一个来源都没有时，`is_plugin_enabled` 全部为假 —— 不报错。
    #[test]
    fn an_empty_source_list_disables_everything() {
        assert!(!MetadataSourceService::is_plugin_enabled(&[], "local"));
    }

    /// 「没收录」与「调用失败」是**两个不同的结果**。
    ///
    /// 合成一个会让「按番号搜索没找到」被报成 502 —— 客户端会以为服务端坏了，
    /// 而实际只是这部片不在 JavDB。
    #[test]
    fn not_found_and_failure_are_distinct() {
        let not_found = MetadataSourceError::NotFound;
        let failed = MetadataSourceError::RequestFailed("超时".to_owned());
        assert_ne!(not_found, failed);
    }
}
