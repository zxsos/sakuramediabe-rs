//! gRPC 服务面：`PluginControl`（注册 + 两个同步任务）与
//! `RankingSourceExtensionService`（榜单取数）。
//!
//! # 活都在宿主那边，插件只转发
//!
//! 上游 `sync_jobs.py` 的两个 handler 只做一件事：调 `context.sync_ranking_sources`
//! / `context.sync_ranking_board`。本仓的对应物就是宿主的 `SyncRankingSources` /
//! `SyncRankingBoard` 两个 rpc —— 所以 `run_job` 里没有抓取逻辑，只有「转发 + 把
//! 返回的统计搬进终态摘要」。
//!
//! 同理 `FetchRanking`：番号由宿主的 `GetJavdbRankNumbers` 取（JavDB 的出网细节
//! 与登录态归宿主一处管，见 `host.proto` 的注释），本插件只把
//! [`boards::Board::query`] 算出的请求形状递过去。
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control::register`]
//! 只回声明；宿主客户端是在**每次任务/取数时**才连的 —— 与上游「每个任务运行
//! 开始时新建 provider」同一条纪律（`PerRunProviderHolder`：登录失败不跨运行
//! 污染，这里每次新建连接同样是这个理由）。

use std::time::Duration;

use chrono::Datelike;
use futures::stream::{BoxStream, StreamExt};
use sm_plugin_api::v1::get_javdb_rank_numbers_request::Query;
use sm_plugin_api::v1::job_event::Event as JobEventKind;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionService;
use sm_plugin_api::v1::{
    Extension, FetchRankingRequest, FetchRankingResponse, GetJavdbRankNumbersRequest, JobDefinition,
    JobEvent, ProgressEvent, RegisterRequest, RegisterResponse, ResolveRankingPeriodsRequest,
    ResolveRankingPeriodsResponse, RunJobRequest, SyncRankingBoardRequest,
    SyncRankingSourcesRequest,
};
// 三种取数形状的载荷类型（oneof 的变体在 `get_javdb_rank_numbers_request` 里，
// 载荷结构体本身在 `v1` 下）。
use sm_plugin_api::v1::{JavdbPlaybackRankQuery, JavdbTopQuery, JavdbVideoTypeRankQuery};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use crate::boards::{self, BoardQuery};
use crate::settings::Settings;

/// 定时全量同步（上游 `sync_jobs.py` 的 `run_sync_all`）。
pub const TASK_SYNC: &str = "sakuramedia_javdb_ranking_sync";
/// 手动单榜同步（上游 `run_sync_board`）。
pub const TASK_SYNC_BOARD: &str = "sakuramedia_javdb_ranking_sync_board";

/// 宿主能力出口的端点（`sm-plugins::supervisor::HOST_ADDR_ENV`）。
///
/// **进程式**由可执行文件从这里取；**进程内**由组合根显式传进
/// [`Control::with_runtime`] —— 进程内多插件共用一份进程环境，从那里读会互相
/// 覆盖。公开它是为了让两处用同一个常量。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

/// 连一次宿主的超时上限。
///
/// 宿主就在本机（进程内形态下就是本进程），正常是毫秒级；给上限是为了让
/// 「宿主没起来」表现成一次可读的失败，而不是任务永远挂着。
const HOST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 控制面。
///
/// **不带账号**：两个同步任务只是转发（账号在宿主问 `FetchRanking` 时由
/// [`Ranking`] 透传），所以这里持账号只会有两份状态。
pub struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
    plugin_id: String,
    /// 宿主能力出口端点。`None` = 宿主没暴露 `PluginHost`，任务直接失败。
    host_endpoint: Option<String>,
}

impl Control {
    /// 只有 id（无宿主端点）。单测与「只想注册一下」的场合用。
    pub fn new(plugin_id: String) -> Self {
        Self {
            plugin_id,
            host_endpoint: None,
        }
    }

    /// 进程式与进程内**共用**的构造：宿主端点显式传入。
    pub fn with_runtime(plugin_id: String, host_endpoint: Option<String>) -> Self {
        Self {
            plugin_id,
            host_endpoint,
        }
    }

    /// 两个任务的声明（上游 `build_jobs`，逐字段对齐）。
    pub fn jobs() -> Vec<JobDefinition> {
        vec![
            JobDefinition {
                task_key: TASK_SYNC.to_owned(),
                log_name: "javdb-ranking-sync".to_owned(),
                cli_name: "sync-javdb-ranking".to_owned(),
                cli_help: "同步 JavDB 排行榜（热播/高评分/有码/无码/FC2/TOP250）".to_owned(),
                // 上游 `default_cron="45 1 * * *"`（与 more-rank-movies 同点，
                // 但两个插件的 `run_sync_all` 是各自独立的请求）。
                default_cron: "45 1 * * *".to_owned(),
                manual_only: false,
                params_schema: None,
                required_capabilities: Vec::new(),
            },
            JobDefinition {
                task_key: TASK_SYNC_BOARD.to_owned(),
                log_name: "javdb-ranking-sync-board".to_owned(),
                cli_name: "sync-javdb-ranking-board".to_owned(),
                cli_help: "手动同步单个 JavDB 榜单".to_owned(),
                manual_only: true,
                default_cron: String::new(),
                params_schema: Some(board_params_schema()),
                required_capabilities: Vec::new(),
            },
        ]
    }
}

/// `SyncBoardParams` 的 JSON Schema（上游 pydantic 模型的手写投影）。
///
/// 宿主拿它渲染手动触发的参数表单 / 校验请求体 —— 字段与
/// [`extract_board_params`] 认的必须一致，所以两边都从这里取。
fn board_params_schema() -> prost_types::Struct {
    use prost_types::value::Kind;
    use prost_types::{Struct, Value};

    /// 一个 `type: string` 属性（可带长度约束）。
    fn string_prop(min: Option<f64>, max: Option<f64>) -> Value {
        let mut fields = std::collections::BTreeMap::from([(
            "type".to_owned(),
            Value {
                kind: Some(Kind::StringValue("string".to_owned())),
            },
        )]);
        for (key, value) in [("minLength", min), ("maxLength", max)] {
            if let Some(value) = value {
                fields.insert(
                    key.to_owned(),
                    Value {
                        kind: Some(Kind::NumberValue(value)),
                    },
                );
            }
        }
        Value {
            kind: Some(Kind::StructValue(Struct { fields })),
        }
    }

    let properties = std::collections::BTreeMap::from([
        (
            "board_key".to_owned(),
            // 榜单 key 是 slug：1..=64（上游 `SyncBoardParams` 的约束；是否
            // 属于本插件由 `extract_board_params` 查表决定）。
            string_prop(Some(1.0), Some(64.0)),
        ),
        // 空串 = 该榜的默认周期，所以**不设 minLength**。
        ("period".to_owned(), string_prop(None, Some(32.0))),
    ]);
    Struct {
        fields: std::collections::BTreeMap::from([
            (
                "type".to_owned(),
                Value {
                    kind: Some(Kind::StringValue("object".to_owned())),
                },
            ),
            (
                "properties".to_owned(),
                Value {
                    kind: Some(Kind::StructValue(Struct { fields: properties })),
                },
            ),
            (
                "required".to_owned(),
                Value {
                    kind: Some(Kind::ListValue(prost_types::ListValue {
                        values: vec![Value {
                            kind: Some(Kind::StringValue("board_key".to_owned())),
                        }],
                    })),
                },
            ),
        ]),
    }
}

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let inner = request.into_inner();
        // 参数校验放在**建流之前**：参数错了要回一个明确的 invalid_argument，
        // 而不是一条「先成功建流、再在流里报错」的路径 —— 后者在任务中心里
        // 显示成「跑过了」。
        let board_params = match inner.task_key.as_str() {
            TASK_SYNC => None,
            TASK_SYNC_BOARD => Some(extract_board_params(&inner)?),
            other => {
                return Err(Status::invalid_argument(format!(
                    "未知 task_key: {other}（本插件只有 {TASK_SYNC} / {TASK_SYNC_BOARD}）"
                )))
            }
        };
        let host_endpoint = self.host_endpoint.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);
        // 任务在后台跑，流只负责把事件搬出去 —— 宿主断开流即取消（协议原话）。
        tokio::spawn(async move {
            match board_params {
                None => run_sync_all(&tx, host_endpoint.as_deref()).await,
                Some(params) => run_sync_board(&tx, host_endpoint.as_deref(), params).await,
            }
        });
        Ok(Response::new(ReceiverStream::new(rx).boxed()))
    }

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let injected = request.into_inner().plugin_id;
        if injected != self.plugin_id {
            return Err(Status::invalid_argument(format!(
                "宿主注入的 plugin_id 与启动参数不一致：注入 {injected}，本进程 {}",
                self.plugin_id
            )));
        }
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: crate::DISPLAY_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            capabilities: vec![sm_plugin_api::v1::Capability::ExtensionRankingSource as i32],
            extensions: vec![Extension {
                key: crate::RANKING_SOURCE_KEY.to_owned(),
                data: Some(sm_plugin_api::v1::extension::Data::RankingSource(
                    sm_plugin_api::v1::RankingSourceExtension {
                        source_key: crate::SOURCE_KEY.to_owned(),
                        // 榜单声明与取数映射读**同一份**表（`boards::BOARDS`）。
                        boards: boards::BOARDS
                            .iter()
                            .map(|board| sm_plugin_api::v1::RankingBoard {
                                board_key: board.key.to_owned(),
                                display_name: board.display_name.to_owned(),
                                supported_periods: board
                                    .periods
                                    .iter()
                                    .map(|period| (*period).to_owned())
                                    .collect(),
                                default_period: board.default_period.to_owned(),
                                dynamic_periods: board.dynamic_periods,
                            })
                            .collect(),
                    },
                )),
            }],
            jobs: Self::jobs(),
            settings_schema: Settings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 手动单榜同步的参数（上游 `SyncBoardParams`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardParams {
    /// 本插件的榜单 key（校验过：不在 [`boards::BOARDS`] 里直接拒）。
    pub board_key: String,
    /// 空串 = 该榜的默认周期（宿主那边同一语义）。
    pub period: String,
}

/// 从 `RunJobRequest` 里取单榜同步参数。
///
/// `board_key` 必须是我们自己的榜单：宿主那边的 `FetchRanking` 也会拒未知榜单，
/// 但那时已经建立了一次同步、失败被记成「目标失败」；在这里拒能让调用方直接
/// 看到「参数写错了」。
///
/// 字段**存在但类型不对**（`period: 1`）同样要拒：静默忽略会让调用方以为
/// 「周期按我说的办了」。
pub fn extract_board_params(request: &RunJobRequest) -> Result<BoardParams, Status> {
    let fields = request.params.as_ref().map(|params| &params.fields);
    let read = |key: &str| -> Result<Option<String>, Status> {
        let Some(value) = fields.and_then(|fields| fields.get(key)) else {
            return Ok(None);
        };
        match value.kind.as_ref() {
            Some(prost_types::value::Kind::StringValue(text)) => Ok(Some(text.clone())),
            _ => Err(Status::invalid_argument(format!("参数 {key} 必须是字符串"))),
        }
    };
    let board_key = read("board_key")?
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Status::invalid_argument("缺少任务参数：需要 board_key（榜单 key，见 GET /ranking-sources）")
        })?;
    if boards::find(&board_key).is_none() {
        let known: Vec<&str> = boards::BOARDS.iter().map(|board| board.key).collect();
        return Err(Status::invalid_argument(format!(
            "未知榜单：{board_key}（本插件有 {known:?}）"
        )));
    }
    Ok(BoardParams {
        board_key,
        period: read("period")?.unwrap_or_default().trim().to_owned(),
    })
}

/// 全量同步：转发宿主的 `SyncRankingSources`。
async fn run_sync_all(tx: &mpsc::Sender<Result<JobEvent, Status>>, host_endpoint: Option<&str>) {
    let _ = tx
        .send(Ok(progress_event("开始同步 JavDB 全部榜单".to_owned(), 0, 0)))
        .await;
    let mut host = match host_client(host_endpoint).await {
        Ok(host) => host,
        Err(error) => {
            let _ = tx.send(Err(error)).await;
            return;
        }
    };
    match host.sync_ranking_sources(SyncRankingSourcesRequest {}).await {
        Ok(response) => {
            let stats = response.into_inner();
            let _ = tx
                .send(Ok(progress_event(
                    format!(
                        "同步完成 · 成功 {}/{} 个目标（失败 {}）",
                        stats.synced_count, stats.total_targets, stats.failed_targets
                    ),
                    stats.synced_count,
                    stats.total_targets,
                )))
                .await;
            let _ = tx
                .send(Ok(result_event(serde_json::json!({
                    "synced_count": stats.synced_count,
                    "total_targets": stats.total_targets,
                    "failed_targets": stats.failed_targets,
                    "fetched_numbers": stats.fetched_numbers,
                    "imported_movies": stats.imported_movies,
                    "local_hit_movies": stats.local_hit_movies,
                    "skipped_movies": stats.skipped_movies,
                    "stored_items": stats.stored_items,
                }))))
                .await;
        }
        Err(error) => {
            // `failed_targets` 记的是「某个榜没同步成」；整批调用失败（插件没声明
            // 排行源、宿主侧服务不可用）是另一回事，要整条任务失败。
            let _ = tx
                .send(Err(Status::unavailable(format!("同步榜单失败: {error}"))))
                .await;
        }
    }
}

/// 单榜同步：转发宿主的 `SyncRankingBoard`。
async fn run_sync_board(
    tx: &mpsc::Sender<Result<JobEvent, Status>>,
    host_endpoint: Option<&str>,
    params: BoardParams,
) {
    let _ = tx
        .send(Ok(progress_event(
            format!("开始同步榜单 {}", params.board_key),
            0,
            0,
        )))
        .await;
    let mut host = match host_client(host_endpoint).await {
        Ok(host) => host,
        Err(error) => {
            let _ = tx.send(Err(error)).await;
            return;
        }
    };
    let request = SyncRankingBoardRequest {
        // 归属由宿主按端点身份核：插件只能同步自己的源（越界回 permission_denied）。
        source_key: crate::SOURCE_KEY.to_owned(),
        board_key: params.board_key.clone(),
        period: params.period,
    };
    match host.sync_ranking_board(request).await {
        Ok(response) => {
            let stats = response.into_inner();
            let _ = tx
                .send(Ok(progress_event(
                    format!(
                        "{}（{}）同步完成 · 取到 {} 个番号，入库 {} 条",
                        stats.board_key, stats.period, stats.fetched_numbers, stats.stored_items
                    ),
                    stats.stored_items,
                    stats.stored_items,
                )))
                .await;
            let _ = tx
                .send(Ok(result_event(serde_json::json!({
                    "source_key": stats.source_key,
                    "board_key": stats.board_key,
                    "period": stats.period,
                    "fetched_numbers": stats.fetched_numbers,
                    "imported_movies": stats.imported_movies,
                    "local_hit_movies": stats.local_hit_movies,
                    "skipped_movies": stats.skipped_movies,
                    "stored_items": stats.stored_items,
                }))))
                .await;
        }
        Err(error) => {
            let _ = tx
                .send(Err(Status::unavailable(format!("同步榜单失败: {error}"))))
                .await;
        }
    }
}

/// `discovery.ranking_source` 扩展点：把榜单取数转发给宿主。
pub struct Ranking {
    settings: Settings,
    host_endpoint: Option<String>,
}

impl Ranking {
    /// 进程式与进程内**共用**的构造。
    pub fn with_runtime(settings: Settings, host_endpoint: Option<String>) -> Self {
        Self {
            settings,
            host_endpoint,
        }
    }
}

#[tonic::async_trait]
impl RankingSourceExtensionService for Ranking {
    /// 这个榜、这个周期有哪些番号（顺序即排名）。
    async fn fetch_ranking(
        &self,
        request: Request<FetchRankingRequest>,
    ) -> Result<Response<FetchRankingResponse>, Status> {
        let inner = request.into_inner();
        let board = boards::find(&inner.board_key)
            .ok_or_else(|| Status::not_found(format!("未知榜单: {}", inner.board_key)))?;
        // 空周期用榜单的默认周期（与读侧 `period_or_all` 同语义）。
        let period = if inner.period.is_empty() {
            board.default_period
        } else {
            inner.period.as_str()
        };

        let mut host = host_client(self.host_endpoint.as_deref()).await?;
        let (username, password) = self.settings.credentials();
        let query = match board.query(period) {
            BoardQuery::Playback { filter_by, period } => {
                Query::Playback(JavdbPlaybackRankQuery {
                    filter_by: filter_by.to_owned(),
                    period,
                })
            }
            BoardQuery::Rank { video_type, period } => {
                Query::VideoTypeRank(JavdbVideoTypeRankQuery {
                    video_type: video_type.to_owned(),
                    period,
                })
            }
            BoardQuery::Top { top_type, type_value } => Query::Top(JavdbTopQuery {
                top_type: top_type.to_owned(),
                type_value,
                // 缺省页数由宿主按上游的 5 页来（`JavdbTopQuery.max_pages`）。
                max_pages: None,
            }),
        };
        let numbers = host
            .get_javdb_rank_numbers(GetJavdbRankNumbersRequest {
                // 账号**只有 TOP250 会读**：另两个榜未登录即可取，传了也不被使用。
                // 没配账号时传空串（宿主 `unwrap_or_default` 与 `None` 等价）。
                username: Some(username),
                password: Some(password),
                query: Some(query),
            })
            .await
            .map_err(|error| Status::unavailable(format!("宿主取榜失败: {error}")))?
            .into_inner()
            .movie_numbers;
        Ok(Response::new(FetchRankingResponse {
            movie_numbers: numbers,
        }))
    }

    /// 本次**真正要抓**的周期。
    ///
    /// 静态榜单回它声明的三个周期；TOP250 现算（固定子榜 + 年份），并且：
    /// - 没配账号 → 空数组（本次不抓，**正常结果**不是错误）；
    /// - 历史年份已有条目 → 跳过（否则榜单停在第一次同步的结果上）。
    async fn resolve_ranking_periods(
        &self,
        request: Request<ResolveRankingPeriodsRequest>,
    ) -> Result<Response<ResolveRankingPeriodsResponse>, Status> {
        let inner = request.into_inner();
        let board = boards::find(&inner.board_key)
            .ok_or_else(|| Status::not_found(format!("未知榜单: {}", inner.board_key)))?;
        let periods = boards::periods_to_fetch(
            board,
            &inner.periods_with_items,
            self.settings.account_configured(),
            chrono::Local::now().year(),
        );
        Ok(Response::new(ResolveRankingPeriodsResponse { periods }))
    }
}

/// 连回宿主的 `PluginHost`。
///
/// 没有端点就 `failed_precondition` —— 静默跳过会让任务「看起来跑了但什么都没干」。
async fn host_client(host_endpoint: Option<&str>) -> Result<PluginHostClient<Channel>, Status> {
    let endpoint = host_endpoint.ok_or_else(|| {
        Status::failed_precondition(format!(
            "缺少宿主端点（{HOST_ADDR_ENV}）—— 宿主没暴露 PluginHost 时榜单同步无从谈起"
        ))
    })?;
    tokio::time::timeout(HOST_CONNECT_TIMEOUT, PluginHostClient::connect(endpoint.to_owned()))
        .await
        .map_err(|_| Status::unavailable("连接宿主超时"))?
        .map_err(|error| Status::unavailable(format!("连接宿主失败: {error}")))
}

/// 构造进度事件。
fn progress_event(text: String, current: i32, total: i32) -> JobEvent {
    JobEvent {
        event: Some(JobEventKind::Progress(ProgressEvent {
            text,
            current,
            total,
        })),
    }
}

/// 构造终态事件（摘要经 `json_struct` 走 `Struct`）。
fn result_event(value: serde_json::Value) -> JobEvent {
    JobEvent {
        event: Some(JobEventKind::Result(
            sm_plugin_api::json_struct::json_to_struct(&value).unwrap_or_default(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(task_key: &str, manual_only: bool) -> JobDefinition {
        JobDefinition {
            task_key: task_key.to_owned(),
            manual_only,
            ..JobDefinition::default()
        }
    }

    /// 两个任务、键与上游逐字相同 —— 任务键是**配置键**（`plugins.job_crons`），
    /// 写错了只会表现成「定时任务没跑 / 手动触发 404」。
    #[test]
    fn jobs_are_the_upstream_two() {
        let jobs = Control::jobs();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].task_key, TASK_SYNC);
        assert!(!jobs[0].manual_only, "定时全量同步要挂 cron");
        assert_eq!(jobs[0].default_cron, "45 1 * * *");
        assert_eq!(jobs[1].task_key, TASK_SYNC_BOARD);
        assert!(jobs[1].manual_only, "单榜同步只能手动触发");
        assert!(jobs[1].default_cron.is_empty());
        for (definition, expected) in jobs.iter().zip([false, true]) {
            assert_eq!(definition.manual_only, job(&definition.task_key, expected).manual_only);
        }
    }

    /// ★ 单榜同步的参数：`board_key` 必填且必须是**我们自己的**榜单。
    #[test]
    fn board_params_are_validated_before_the_stream_starts() {
        let request = |params: Option<serde_json::Value>| RunJobRequest {
            run_id: "t".to_owned(),
            task_key: TASK_SYNC_BOARD.to_owned(),
            params: params.and_then(|value| sm_plugin_api::json_struct::json_to_struct(&value)),
            data_dir: String::new(),
        };

        let parsed = extract_board_params(&request(Some(serde_json::json!({
            "board_key": " playback_all ",
            "period": "daily",
        }))))
        .expect("正常参数");
        assert_eq!(parsed.board_key, "playback_all", "首尾空白要裁掉");
        assert_eq!(parsed.period, "daily");

        // 缺 board_key / 空串 / 别人的榜单，都要在**建流之前**拒。
        for bad in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "board_key": "  " })),
            Some(serde_json::json!({ "board_key": "hot" })),
            Some(serde_json::json!({ "board_key": "playback_all", "period": 1 })),
        ] {
            let error = extract_board_params(&request(bad.clone()))
                .expect_err(&format!("{bad:?} 该被拒"));
            assert_eq!(error.code(), tonic::Code::InvalidArgument, "{bad:?}");
        }

        // 空周期是允许的：宿主按该榜的默认周期算。
        let parsed = extract_board_params(&request(Some(serde_json::json!({
            "board_key": "top250",
        }))))
        .expect("period 可缺省");
        assert_eq!(parsed.period, "");
    }

    /// 参数 schema 认的字段与 `extract_board_params` 认的必须一致 —— 分叉一次
    /// 就会出现「表单能填、提交说缺参数」。
    #[test]
    fn the_params_schema_lists_what_we_read() {
        let schema = board_params_schema();
        let properties = schema
            .fields
            .get("properties")
            .and_then(|value| value.kind.as_ref())
            .and_then(|kind| match kind {
                prost_types::value::Kind::StructValue(fields) => Some(fields),
                _ => None,
            })
            .expect("properties");
        let keys: Vec<&str> = properties.fields.keys().map(String::as_str).collect();
        assert_eq!(keys, ["board_key", "period"]);
        let required = schema.fields.get("required").expect("required");
        assert!(matches!(
            required.kind.as_ref(),
            Some(prost_types::value::Kind::ListValue(_))
        ));
    }

    /// 终态摘要必须是**对象根** —— `json_to_struct` 只收对象根，别的形状会被
    /// 静默换成空 `Struct`（宿主侧看起来就是「任务成功了但没结果」）。
    #[test]
    fn result_payloads_round_trip_through_struct() {
        let event = result_event(serde_json::json!({"board_key": "playback_all"}));
        let Some(JobEventKind::Result(payload)) = event.event else {
            panic!("该是终态事件");
        };
        assert_eq!(payload.fields.len(), 1);
    }
}
