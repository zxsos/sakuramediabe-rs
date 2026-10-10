//! 按番号入库（`PluginHost::ImportMovieByNumber`）的**延迟填槽**。
//!
//! # 为什么需要它
//!
//! 与 [`crate::ranking_gateway::RankingSyncSlot`] 同一个时序问题：`PluginHost`
//! 端点必须在插件进程起来**之前**就绪（端点串靠环境变量注入插件），而
//! `ImportMovieByNumber` 依赖的两件东西 —— 元数据来源服务（要等插件注册完才有
//! `catalog.metadata_source` 扩展点）与目录写入（要等图片根目录解析出来）——
//! 都比端点**晚出生**。
//!
//! 直接在装配点构造不出来的话，剩下的选择就是把整个 `MetadataSourceService`
//! 的构造提前，而它离不开插件注册表 —— 死循环。所以这里留一个可后填的槽：
//! 装配 4a 建**空槽**，组合根在元数据来源服务建成之后填真货。
//!
//! 没填时 `ImportMovieByNumber` 明确回 `unimplemented` —— 不退化成假成功
//! （「接口成功但什么都没发生」比失败难查得多）。

use std::sync::{Arc, Mutex, MutexGuard};

use sm_service::catalog::catalog_import::CatalogImport;
use sm_service::catalog::metadata_source::MetadataSourceService;

/// 按番号入库要用的两件东西。
pub struct MetadataImportDeps {
    /// 元数据来源（JavDB 优先，启用的插件来源兜底）。
    pub source: Arc<MetadataSourceService>,
    /// 目录写入（影片 + 演员 + 图片落盘的唯一入口）。
    pub catalog: Arc<dyn CatalogImport + Send + Sync>,
}

/// [`MetadataImportDeps`] 的延迟填槽。
///
/// `Clone` 是有意的：组合根要留一份原柄填槽，各端点实例拿的是共享句柄的克隆
/// （`Arc<Mutex<…>>`，填一次全体可见）。
#[derive(Clone, Default)]
pub struct MetadataImportSlot {
    inner: Arc<Mutex<Option<Arc<MetadataImportDeps>>>>,
}

impl MetadataImportSlot {
    /// 造一个空槽。
    pub fn new() -> Self {
        Self::default()
    }

    /// 填上真正的两件套。组合根在元数据来源服务建成后调用**一次**。
    ///
    /// 幂等（后填的覆盖先填的）—— 热重载场景下会再填一次。
    pub fn fill(&self, deps: Arc<MetadataImportDeps>) {
        *self.lock() = Some(deps);
    }

    /// 取当前的两件套。还没填 → `None`。
    pub fn get(&self) -> Option<Arc<MetadataImportDeps>> {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Arc<MetadataImportDeps>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_service::error::ServiceError;

    /// ★ 空槽取出来是 `None`；填了之后同一个句柄的克隆也能看到 ——
    /// 「端点先出生、货后填」的整条语义就押在这两行上。
    #[test]
    fn the_slot_stays_empty_until_filled_and_clones_share_the_fill() {
        let slot = MetadataImportSlot::new();
        assert!(slot.get().is_none(), "空槽必须真的空");
        let shared = slot.clone();
        assert!(shared.get().is_none(), "克隆看到的也是空");

        slot.fill(Arc::new(MetadataImportDeps {
            source: Arc::new(MetadataSourceService::new(Vec::new(), None)),
            catalog: dummy_catalog(),
        }));

        assert!(slot.get().is_some());
        assert!(shared.get().is_some(), "填槽对克隆可见");
        // 重复填是覆盖，不是 panic（热重载会再填一次）。
        slot.fill(Arc::new(MetadataImportDeps {
            source: Arc::new(MetadataSourceService::new(Vec::new(), None)),
            catalog: dummy_catalog(),
        }));
        assert!(slot.get().is_some());
    }

    /// `CatalogImport` 的最小替身：槽不调用它的任何方法，只需要一个非 `None`
    /// 的值。全部方法都返回「没动过」的缺省结果。
    fn dummy_catalog() -> Arc<dyn CatalogImport + Send + Sync> {
        Arc::new(NoopCatalog)
    }

    struct NoopCatalog;

    #[tonic::async_trait]
    impl CatalogImport for NoopCatalog {
        async fn import_movie_if_missing(
            &self,
            _movie_number: &str,
            _detail: &serde_json::Value,
        ) -> Result<(i32, bool), ServiceError> {
            Ok((0, false))
        }

        async fn find_movie_id(&self, _movie_number: &str) -> Result<Option<i32>, ServiceError> {
            Ok(None)
        }

        async fn import_plugin_movie(
            &self,
            _detail: &serde_json::Value,
            _source: &serde_json::Value,
            _force_subscribed: bool,
        ) -> Result<(i32, bool), ServiceError> {
            Ok((0, false))
        }

        async fn upsert_actor(&self, _actor_resource: &serde_json::Value) -> Result<i32, ServiceError> {
            Ok(0)
        }
    }
}
