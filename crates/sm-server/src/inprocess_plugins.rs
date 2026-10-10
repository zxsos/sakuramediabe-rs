//! 进程内插件调用：直接实例化 vendored 插件的 service，不起进程、不走 gRPC。
//!
//! # 为什么需要这个模块
//!
//! 9 个插件已 vendoring 进 workspace（`crates/plugin-*`），与后端共用同一份
//! `sm-plugin-api`。本模块提供直接调用它们的能力，省掉 9 个进程的开销
//! （约 50M 内存）。
//!
//! # 调用方式
//!
//! 插件的 service 实现了 tonic 生成的 trait（如 `RankingSourceExtensionService`）。
//! 直接构造 `tonic::Request`，调用 trait 方法，从 `tonic::Response` 取结果。
//! 与走 gRPC 的效果完全一致，只是跳过了网络序列化。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sm_plugin_api::v1::metadata_source_extension_service_server::MetadataSourceExtensionService;
use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionService;
use sm_plugin_api::v1::{
    FetchMovieRequest, FetchMovieResponse, FetchRankingRequest, ResolveRankingPeriodsRequest,
};
use sm_service::catalog::metadata_source::{InProcessMetadataFetch, MetadataSourceError};
use sm_service::discovery::ranking::{RankingCallError, RankingGateway};

/// 进程内排行插件网关。
///
/// 与 `RankingPluginGateway` 同样的接口，但直接调用 vendored 插件，
/// 不经过 gRPC。
pub struct InProcessRankingGateway {
    /// plugin_id -> settings JSON
    settings: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl InProcessRankingGateway {
    pub fn new() -> Self {
        Self {
            settings: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_settings(settings: HashMap<String, serde_json::Value>) -> Self {
        Self {
            settings: Arc::new(Mutex::new(settings)),
        }
    }

    fn settings_for(&self, plugin_id: &str) -> serde_json::Value {
        self.settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(plugin_id)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

impl Default for InProcessRankingGateway {
    fn default() -> Self {
        Self::new()
    }
}

/// 复合排行网关：优先进程内，兜底 gRPC。
///
/// javdb 走进程内（已 vendoring），其他源走原有的 gRPC 路径。
pub struct CompositeRankingGateway {
    inprocess: InProcessRankingGateway,
    grpc: crate::ranking_gateway::RankingPluginGateway,
}

impl CompositeRankingGateway {
    pub fn new(
        inprocess: InProcessRankingGateway,
        grpc: crate::ranking_gateway::RankingPluginGateway,
    ) -> Self {
        Self { inprocess, grpc }
    }
}

impl RankingGateway for CompositeRankingGateway {
    fn fetch_ranking<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        period: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>,
    > {
        // javdb 已 vendoring，走进程内；其他走 gRPC。
        if source_key == "javdb" {
            self.inprocess.fetch_ranking(source_key, board_key, period)
        } else {
            self.grpc.fetch_ranking(source_key, board_key, period)
        }
    }

    fn resolve_periods<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        periods_with_items: &'a [String],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>,
    > {
        if source_key == "javdb" {
            self.inprocess.resolve_periods(source_key, board_key, periods_with_items)
        } else {
            self.grpc.resolve_periods(source_key, board_key, periods_with_items)
        }
    }
}

impl RankingGateway for InProcessRankingGateway {
    fn fetch_ranking<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        period: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // 目前只支持 javdb，其他源走 gRPC 路径（由调用方分流）。
            if source_key != "javdb" {
                return Err(RankingCallError::new(
                    "inprocess_not_supported",
                    format!("进程内网关不支持排行源 {source_key}"),
                ));
            }

            // 从 settings 构造 JavDbSource。
            let settings_value = self.settings_for("sakuramedia_javdb_ranking");
            let settings: plugin_javdb_ranking::settings::Settings =
                serde_json::from_value(settings_value).unwrap_or_default();
            let source = plugin_javdb_ranking::javdb::JavDbSource::new(&settings).map_err(|e| {
                RankingCallError::new("inprocess_init_failed", format!("JavDbSource 初始化失败：{e}"))
            })?;
            let ranking = plugin_javdb_ranking::service::Ranking::new(source);

            // 直接调用 trait 方法，不走 gRPC。
            let request = tonic::Request::new(FetchRankingRequest {
                board_key: board_key.to_owned(),
                period: period.to_owned(),
            });
            let response = ranking.fetch_ranking(request).await.map_err(|s| {
                RankingCallError::new("inprocess_call_failed", format!("fetch_ranking 失败：{s}"))
            })?;
            Ok(response.into_inner().movie_numbers)
        })
    }

    fn resolve_periods<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        periods_with_items: &'a [String],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if source_key != "javdb" {
                return Err(RankingCallError::new(
                    "inprocess_not_supported",
                    format!("进程内网关不支持排行源 {source_key}"),
                ));
            }

            let settings_value = self.settings_for("sakuramedia_javdb_ranking");
            let settings: plugin_javdb_ranking::settings::Settings =
                serde_json::from_value(settings_value).unwrap_or_default();
            let source = plugin_javdb_ranking::javdb::JavDbSource::new(&settings).map_err(|e| {
                RankingCallError::new("inprocess_init_failed", format!("JavDbSource 初始化失败：{e}"))
            })?;
            let ranking = plugin_javdb_ranking::service::Ranking::new(source);

            let request = tonic::Request::new(ResolveRankingPeriodsRequest {
                board_key: board_key.to_owned(),
                periods_with_items: periods_with_items.to_vec(),
            });
            let response = ranking.resolve_ranking_periods(request).await.map_err(|s| {
                RankingCallError::new(
                    "inprocess_call_failed",
                    format!("resolve_ranking_periods 失败：{s}"),
                )
            })?;
            Ok(response.into_inner().periods)
        })
    }
}

/// 进程内元数据网关。
///
/// 直接调用 vendored 的 `plugin-javbus-metadata` 的 `Metadata` service，
/// 不经过 gRPC。目前仅支持 `sakuramedia_javbus_metadata`。
pub struct InProcessMetadataGateway {
    /// plugin_id -> settings JSON
    settings: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl InProcessMetadataGateway {
    pub fn new() -> Self {
        Self {
            settings: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_settings(settings: HashMap<String, serde_json::Value>) -> Self {
        Self {
            settings: Arc::new(Mutex::new(settings)),
        }
    }

    fn settings_for(&self, plugin_id: &str) -> serde_json::Value {
        self.settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(plugin_id)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

impl Default for InProcessMetadataGateway {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl InProcessMetadataFetch for InProcessMetadataGateway {
    fn supports(&self, plugin_id: &str) -> bool {
        // 目前只有 javbus-metadata 实现了 MetadataSourceExtensionService。
        plugin_id == "sakuramedia_javbus_metadata"
    }

    async fn fetch_movie(
        &self,
        plugin_id: &str,
        request: FetchMovieRequest,
    ) -> Result<FetchMovieResponse, MetadataSourceError> {
        if plugin_id != "sakuramedia_javbus_metadata" {
            return Err(MetadataSourceError::Disabled(format!(
                "进程内元数据网关不支持插件 {plugin_id}"
            )));
        }

        // 从 settings 构造 JavBusSource。
        let settings_value = self.settings_for("sakuramedia_javbus_metadata");
        let settings = plugin_javbus_metadata::settings::Settings::from_json(&settings_value);
        let source = plugin_javbus_metadata::javbus::JavBusSource::new(&settings).map_err(|e| {
            MetadataSourceError::RequestFailed(format!("JavBusSource 初始化失败：{e}"))
        })?;
        let metadata = plugin_javbus_metadata::service::Metadata::new(source);

        // 直接调用 trait 方法，不走 gRPC。
        let tonic_request = tonic::Request::new(request);
        let response = metadata.fetch_movie(tonic_request).await.map_err(|s| {
            // 与 gRPC 路径的错误分类保持一致：
            // DeadlineExceeded -> RequestFailed，其他 -> RequestFailed。
            // `found=false` 的情况插件返回 Ok，由调用方判 NotFound。
            match s.code() {
                tonic::Code::DeadlineExceeded => {
                    MetadataSourceError::RequestFailed("插件索取超时".to_owned())
                }
                _ => MetadataSourceError::RequestFailed(s.to_string()),
            }
        })?;
        Ok(response.into_inner())
    }
}
