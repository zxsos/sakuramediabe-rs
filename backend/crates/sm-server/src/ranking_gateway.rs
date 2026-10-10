//! 排行取数网关：把 `sm-service` 定义的 [`RankingGateway`] 接到插件进程。
//!
//! # 为什么实现必须在这里
//!
//! `sm-plugins` 依赖 `sm-scheduler`，`sm-scheduler` 依赖 `sm-service` ——
//! `sm-service` 依赖 `sm-plugins` 就成环。所以 trait 在 `sm-service`、实现在
//! 组合根。同 [`crate::provider_gateway`]（`StorageGateway` / `PlaybackGateway`）。
//!
//! # 端点是**现取**的
//!
//! 排行榜扩展点服务跑在插件的**控制面**上（proto 里
//! `RankingSourceExtensionService` 与 `MetadataSourceExtensionService` 是同一
//! 进程上的两个 service，只有数据面才有独立的 `data_plane_endpoint`），
//! 而插件重启会**换端口**（`supervisor` 每次向内核要新地址）。所以这里每次
//! 调用都从注册表现取 `plugin_endpoint`，不缓存 channel —— 缓存会在重启后
//! 指向一个没人监听的端口，表现为「注册表看起来一切正常，取数全部失败」。
//!
//! # 不设调用时限
//!
//! 上游是同进程同步调用，无时限；本仓的 `sm_plugins::extension_calls` 也支持
//! `deadline = None`。这里给 `None`：抓一页榜单是本就要联网的活儿，挂一个
//! 猜出来的秒数只会在慢线路上把成功的抓取误判成超时。真要加上限，位置是
//! [`RankingPluginGateway::with_deadline`]。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sm_plugin_api::v1::{FetchRankingRequest, ResolveRankingPeriodsRequest};
use sm_plugins::extension_calls;
use sm_plugins::extensions::ExtensionRegistry;
use sm_service::discovery::ranking::{RankingCallError, RankingGateway, RankingSyncService};

/// 排行取数适配器。
pub struct RankingPluginGateway {
    /// 活的注册表句柄（理由见模块文档「端点是现取的」）。
    extensions: Arc<Mutex<ExtensionRegistry>>,
    /// 单次调用上限。`None` = 不限（默认）。见模块文档。
    deadline: Option<Duration>,
}

impl RankingPluginGateway {
    /// 用活的扩展点注册表造一个网关，不带调用时限。
    pub fn new(extensions: Arc<Mutex<ExtensionRegistry>>) -> Self {
        Self {
            extensions,
            deadline: None,
        }
    }

    /// 设一个单次调用上限。见模块文档「不设调用时限」。
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    fn registry(&self) -> MutexGuard<'_, ExtensionRegistry> {
        self.extensions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 取某个排行源所在插件的**控制面**端点。源不在注册表 → `ranking_source_not_found`。
    fn endpoint_for(&self, source_key: &str) -> Result<String, RankingCallError> {
        self.registry()
            .ranking_source(source_key)
            .map(|source| source.plugin_endpoint.clone())
            .ok_or_else(|| {
                RankingCallError::new(
                    "ranking_source_not_found",
                    format!("排行源 {source_key} 不在注册表里（插件没加载或已被拒）"),
                )
            })
    }
}

/// 把 `sm_plugins` 的调用错误收敛成 `sm-service` 的错误码。
///
/// `ExtensionCallError` 的两个变体本身就是稳定的机读码
/// （`extension_call_failed` / `extension_call_timeout`），原样传下去 ——
/// 上层的重试/告警判断要按它分流。
fn to_call_error(error: extension_calls::ExtensionCallError) -> RankingCallError {
    // 措辞留在这一层：`ExtensionCallError` 的 `Call(..)` 里装的是 gRPC 的原始
    // 报文，`Timeout` 没有附文 —— 补一句人话，别让调用方看见一个空 message。
    let message = match &error {
        extension_calls::ExtensionCallError::Call(detail) => detail.clone(),
        extension_calls::ExtensionCallError::Timeout => "扩展点调用超时".to_owned(),
    };
    RankingCallError::new(error.code(), message)
}

impl RankingGateway for RankingPluginGateway {
    fn fetch_ranking<'a>(
        &'a self,
        source_key: &'a str,
        board_key: &'a str,
        period: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<String>, RankingCallError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let endpoint = self.endpoint_for(source_key)?;
            let mut client = extension_calls::connect_ranking(&endpoint)
                .await
                .map_err(to_call_error)?;
            let response = extension_calls::fetch_ranking(
                &mut client,
                FetchRankingRequest {
                    board_key: board_key.to_owned(),
                    period: period.to_owned(),
                },
                self.deadline,
            )
            .await
            .map_err(to_call_error)?;
            // 顺序即排名 —— 中间不做任何排序或去重（上游也是原样交给写侧）。
            Ok(response.movie_numbers)
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
            let endpoint = self.endpoint_for(source_key)?;
            let mut client = extension_calls::connect_ranking(&endpoint)
                .await
                .map_err(to_call_error)?;
            let response = extension_calls::resolve_ranking_periods(
                &mut client,
                ResolveRankingPeriodsRequest {
                    board_key: board_key.to_owned(),
                    periods_with_items: periods_with_items.to_vec(),
                },
                self.deadline,
            )
            .await
            .map_err(to_call_error)?;
            // 空列表是「本次不抓」（正常结果），不是错误 —— 原样返回。
            Ok(response.periods)
        })
    }
}

/// 排行同步服务的**延迟填槽**。
///
/// # 为什么需要它
///
/// 装配时序是死的（见 `crate::lib.rs`）：`PluginHost` 端点必须**在插件进程起来
/// 之前**就绪（端点串要通过环境变量注入插件），而排行源目录要等插件注册完才有。
/// 也就是说端点比目录**先出生**。
///
/// 直接塞一个空目录进去的话，`sync_ranking_sources` 会永远算出「0 个目标」并
/// 返回成功 —— 又一例「接口成功但没数据」。所以这里留一个可后填的槽：装配时建
/// 空的，插件加载完之后填真货。
///
/// 没填时两个 rpc 都明确失败（`Unavailable` / `ranking_sync_unavailable`）。
#[derive(Clone, Default)]
pub struct RankingSyncSlot {
    inner: Arc<Mutex<Option<Arc<RankingSyncService>>>>,
}

impl RankingSyncSlot {
    /// 造一个空槽。
    pub fn new() -> Self {
        Self::default()
    }

    /// 填上真正的服务。组合根在插件加载后调用**一次**。
    ///
    /// 幂等（后填的覆盖先填的）—— 热重载场景下会再填一次。
    pub fn fill(&self, service: Arc<RankingSyncService>) {
        *self.lock() = Some(service);
    }

    /// 取当前的服务。还没填 → `None`。
    pub fn get(&self) -> Option<Arc<RankingSyncService>> {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Arc<RankingSyncService>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
