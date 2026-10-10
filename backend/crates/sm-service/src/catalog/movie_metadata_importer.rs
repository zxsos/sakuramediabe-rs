//! 「按番号 / 按候选 id 取元数据并入库」的真实现 —— 接进 `transfers` 的窄接口。
//!
//! # 为什么这个文件在 `catalog/`，而不是放进 `transfers/`
//!
//! `transfers` 侧那个 [`MovieMetadataImporter`] 是**刻意的窄接口**（它自己的
//! 文档写着「避免本模块依赖 `metadata_source` 的实现」）。真正知道「怎么取
//! 元数据、怎么入库」的是 catalog 侧的服务，所以实现落在这一边，依赖方向保持：
//!
//! ```text
//!   transfers（只声明要什么）← catalog（知道怎么做）← 组合根（接线）
//! ```
//!
//! # 上游没有这一层
//!
//! 上游 `imports/import_service.py` 直接调 `MetadataSourceService` 与
//! `CatalogImportService`（同一个进程里）。本仓加这一层**唯一**的原因是模块
//! 依赖方向。所以这里只做转发与翻译、不含业务判断 —— 上游在两条路（批量导入 /
//! 手动重试）上调的是同一份实现，这里也必须是。
//!
//! # 一次「按候选导入」的完整链路
//!
//! ```text
//!   retry_failed_file(source_kind = "plugin")
//!     -> MovieMetadataImporter::import_by_candidate(candidate_id, force = true)
//!       -> resolve_candidate_reference        分支在这里定（插件 / JavDB）
//!       -> MovieMetadataSearchService::fetch_candidate
//!         -> CatalogImport::import_plugin_movie | import_movie_if_missing
//! ```
//!
//! `force = true` 是**调用方**的决定（上游：用户手动重试 = 明确要它进订阅），
//! 这里只透传。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::catalog::catalog_import::CatalogImport;
use crate::catalog::metadata_source::{MetadataSourceError, MetadataSourceService};
use crate::catalog::movie_metadata_search::MovieMetadataSearchService;
use crate::error::ServiceError;
use crate::system::ConfigService;
use crate::transfers::import_service::{
    CatalogImport as TransfersCatalogImport, MovieMetadataImporter,
};
use crate::transfers::import_task::MetadataCandidateSource;

/// 见模块文档。
pub struct CatalogMovieMetadataImporter {
    /// 配置来源。`import_by_number` 要一份 `plugins.enabled` 快照；候选那条路
    /// 还要再读一次（`fetch_candidate` 内部自己读 —— 它读的是**当下**的值）。
    config: ConfigService,
    /// 元数据来源（JavDB + 插件）。
    source: Arc<MetadataSourceService>,
    /// 候选那条路（取详情 + 校验番号）。
    search: Arc<MovieMetadataSearchService>,
    /// 目录写入。
    ///
    /// `dyn ... + Send + Sync`：catalog 侧的 [`CatalogImport`] 没有
    /// `Send + Sync` 超 trait，而本结构必须 `Send + Sync`
    /// （[`MovieMetadataImporter`] 的约束）。在 `dyn` 这一层补上，不去动那个
    /// trait 的声明 —— 它目前的唯一实现（`CatalogImportService`）本来就是
    /// `Send + Sync`。
    catalog: Arc<dyn CatalogImport + Send + Sync>,
}

impl CatalogMovieMetadataImporter {
    /// 构造。
    ///
    /// `source` 与 `search` 里那份必须是**同一个** `Arc<MetadataSourceService>`
    /// —— 两条路共用同一个 provider 实例（测试替身尤其在意这点：各建一份，
    /// 「问了几次来源」的计数就对不上了）。
    pub fn new(
        config: ConfigService,
        source: Arc<MetadataSourceService>,
        search: Arc<MovieMetadataSearchService>,
        catalog: Arc<dyn CatalogImport + Send + Sync>,
    ) -> Self {
        Self {
            config,
            source,
            search,
            catalog,
        }
    }
}

impl MovieMetadataImporter for CatalogMovieMetadataImporter {
    fn import_by_number<'a>(
        &'a self,
        movie_number: &'a str,
        _import: &'a dyn TransfersCatalogImport,
        force_subscribed: bool,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ServiceError>> + Send + 'a>> {
        Box::pin(async move {
            let config = self.config.snapshot()?;
            // 上游 `metadata_import_batch` / `retry_failed_file` 从这一步拿到的
            // 就是「是否新建」——中间那个 `movie_id` 由调用方自己再查一次
            // （`import_service.rs:609` 就是这么写的）。
            let (_movie_id, created) = self
                .source
                .import_by_number(
                    &config,
                    movie_number,
                    self.catalog.as_ref(),
                    force_subscribed,
                )
                .await
                .map_err(metadata_error)?;
            Ok(created)
        })
    }

    fn import_by_candidate<'a>(
        &'a self,
        candidate_id: &'a str,
        _import: &'a dyn TransfersCatalogImport,
        force_subscribed: bool,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ServiceError>> + Send + 'a>> {
        Box::pin(async move {
            // ★ 分支在**取详情之前**定，且用与 `fetch_candidate` **同一个**判据
            // —— 那个判据只在一个地方（`MovieMetadataSearchService::resolve_candidate`），
            // 这里与失败项重试/人工搜索三处共用。**不**从闭包第二个参数是不是
            // `Null` 反推来源（见 `fetch_candidate` 的文档）。
            let reference = self.search.resolve_candidate(candidate_id)?;
            // 闭包返回 `Result` 而不是直接返回值：两个导入方法都会失败，而
            // `fetch_candidate` 的闭包是**不可失败**的形状 —— 失败沿它外层那个
            // `Result` 出去，与「取详情失败」同一条路。
            let imported = self
                .search
                .fetch_candidate(candidate_id, |detail, source| async move {
                    if matches!(reference.source, MetadataCandidateSource::Plugin) {
                        self.catalog
                            .import_plugin_movie(&detail, &source, force_subscribed)
                            .await
                    } else {
                        // JavDB 支：番号取 id 里那一段 —— `fetch_candidate` 已经
                        // 校验过它与详情里的番号归一等价，所以这里不会写错行。
                        self.catalog
                            .import_movie_if_missing(&reference.movie_number, &detail)
                            .await
                    }
                })
                .await?;
            let (_movie_id, created) = imported?;
            Ok(created)
        })
    }
}

/// [`MetadataSourceError`] → [`ServiceError`]。
///
/// # 错误码取的是**上游异常类的名字**
///
/// 上游 `metadata/_providers/exceptions.py` 有三个类，`start/commands.py:121`
/// 与 `:132` 把它们各自的名字直接作为 `error.type` 交给用户：
/// `MetadataNotFoundError` → `metadata_not_found`、`MetadataRequestError` →
/// `metadata_request_error`。
///
/// 这几个码在本仓是**会被读到的**：`MetadataImportResult.failure_detail`
/// （`import_service.rs:626`）与 `invalid_retry_file` 的 message
/// （`import_service.rs:684`）都取 `error.code()`。所以必须是能读懂的、
/// 上游用过的词 —— 自造一个，用户看到的就是黑话。
///
/// 后两个变体（插件 ABI 侧的失败）在上游都在 `MetadataSourceError` 这个
/// `RuntimeError` 里，没有独立类名 —— 就用基类的名字 `metadata_source_error`。
fn metadata_error(error: MetadataSourceError) -> ServiceError {
    match error {
        // 404：「没收录」是**正常结果**，不是故障（上游把它与请求失败分成两类，
        // 下游处置相反：一个提示用户换候选，一个该重试）。
        MetadataSourceError::NotFound => ServiceError::not_found_with(
            "metadata_not_found",
            "没有来源收录这部影片",
            serde_json::Map::new(),
        ),
        MetadataSourceError::RequestFailed(detail) => {
            tracing::warn!(detail, "元数据来源请求失败");
            ServiceError::bad_gateway(
                "metadata_request_error",
                "元数据来源请求失败",
                serde_json::Map::new(),
            )
        }
        MetadataSourceError::InvalidDelivery(problem) => {
            tracing::warn!(problem, "元数据来源交付不合法");
            ServiceError::bad_gateway(
                "metadata_source_error",
                "元数据来源交付不合法",
                serde_json::Map::new(),
            )
        }
        MetadataSourceError::Disabled(plugin_id) => {
            tracing::warn!(plugin_id, "元数据来源插件未启用");
            ServiceError::bad_gateway(
                "metadata_source_error",
                "元数据来源插件未启用",
                serde_json::Map::new(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 错误码 = **上游异常类的名字**。
    ///
    /// 这条会被用户看到（进 `failure_detail` / `invalid_retry_file` 的正文），
    /// 所以它值一个测试。
    #[test]
    fn source_errors_map_to_upstream_exception_names() {
        assert_eq!(
            metadata_error(MetadataSourceError::NotFound).code(),
            "metadata_not_found"
        );
        assert_eq!(
            metadata_error(MetadataSourceError::RequestFailed("连不上".to_owned())).code(),
            "metadata_request_error"
        );
        // 这两个没有上游对应类名（都在 `MetadataSourceError` 里），用基类名。
        assert_eq!(
            metadata_error(MetadataSourceError::InvalidDelivery("越界".to_owned())).code(),
            "metadata_source_error"
        );
        assert_eq!(
            metadata_error(MetadataSourceError::Disabled("javbus".to_owned())).code(),
            "metadata_source_error"
        );
    }
}
