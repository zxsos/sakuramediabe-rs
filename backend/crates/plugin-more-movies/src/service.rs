//! gRPC 服务面：`PluginControl`（两个插件）与 `RankingSourceExtensionService`。
//!
//! # 为什么两个 service 在同一个进程、同一个端口
//!
//! 与 `plugin-javbus-metadata` 同一理由：proto 里它们各是一个 service，但没有任何
//! 字段声明另一个端口 —— 只有数据面有 `data_plane_endpoint`，而它留给字节搬运。
//! 所以控制面与扩展点共用宿主分配的那个地址。
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以两个 `Control` 的
//! `register` 只回声明；HTTP 客户端是在 `main` 里建好的（建客户端不发请求）。
//!
//! # 回调宿主
//!
//! 任务执行时插件是宿主 `PluginHost` 服务的客户端，地址走环境变量
//! `SAKURAMEDIA_HOST_GRPC_ADDR`（宿主 `sm-plugins` 的 `supervisor.rs` 注入）。
//! 变量缺失时任务直接失败 —— 静默跳过会让定时任务「看起来跑了但什么都没干」。

use std::collections::HashSet;

use futures::stream::BoxStream;
use futures::StreamExt;
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::job_event::Event as JobEventKind;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionService;
use sm_plugin_api::v1::{
    Capability, Extension, FetchRankingRequest, FetchRankingResponse, FindByNumbersRequest,
    ImportMovieByNumberRequest, JobDefinition, JobEvent, ProgressEvent, RankingBoard,
    RankingSourceExtension, RegisterRequest, RegisterResponse, ResolveRankingPeriodsRequest,
    ResolveRankingPeriodsResponse, RunJobRequest, SyncRankingBoardRequest,
    SyncRankingSourcesRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::javdb::{self, LatestItem, LatestPageError, SAFETY_MAX_PAGES};
use crate::javlibrary::{self, JavLibraryClient, JavLibraryError};
use crate::minnano::{MinnanoAvClient, MinnanoError};
use crate::settings::{MoreMoviesSettings, RankMoviesSettings};

/// 宿主 `PluginHost` 服务的地址（宿主注入）。
///
/// **进程式**从进程环境读（`sm_plugins::supervisor::HOST_ADDR_ENV`）；
/// **进程内**由组合根把每个插件自己的端点显式传进 control —— 进程内每个插件
/// 各有一份 `PluginHost` 端点（身份 = `plugin:{plugin_id}`），读一份共享的进程
/// 环境必然串身份。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

// ── 任务 key（上游各 plugin.py 的 task_key）─────────────────────────────

pub const MORE_MOVIES_SYNC_TASK: &str = "sakuramedia_more_movies_sync";
pub const RANK_SYNC_TASK: &str = "sakuramedia_more_rank_movies_sync";
/// 手动单榜同步（上游 `sync_jobs.py` 的第二个任务）。
pub const RANK_SYNC_BOARD_TASK: &str = "sakuramedia_more_rank_movies_sync_board";

// ── 榜单 key（上游 boards.py）────────────────────────────────────────────

pub const MINNANO_SOURCE_KEY: &str = "minnano_av";
pub const JAVLIBRARY_SOURCE_KEY: &str = "javlibrary";
pub const MINNANO_BOARD_KEY: &str = "minnano_av";
pub const JAVLIBRARY_BESTRATED_BOARD_KEY: &str = "javlibrary_bestrated";
pub const JAVLIBRARY_MOSTWANTED_BOARD_KEY: &str = "javlibrary_mostwanted";

/// 榜单声明（上游 `boards.py:build_ranking_sources`）。
///
/// 周期集合照上游每个 board 上的 `supported_periods` / `default_period` 填：
/// Minnano AV 三档（`daily` / `weekly` / `monthly`，缺省 `daily`），
/// JavLibrary 两档（`monthly` / `all`，缺省 `monthly`）。宿主拿它们判
/// 「这个周期合不合法」「不给周期时按哪个跑」——留给宿主判而不是在
/// `fetch_ranking` 里判，是为了让 `SyncRankingBoard` 的裁决与读侧一致。
pub fn ranking_sources() -> Vec<RankingSourceExtension> {
    vec![
        RankingSourceExtension {
            source_key: MINNANO_SOURCE_KEY.to_owned(),
            boards: vec![RankingBoard {
                board_key: MINNANO_BOARD_KEY.to_owned(),
                display_name: "Minnano AV".to_owned(),
                supported_periods: periods(&crate::minnano::PERIODS),
                default_period: MINNANO_DEFAULT_PERIOD.to_owned(),
                dynamic_periods: false,
            }],
        },
        RankingSourceExtension {
            source_key: JAVLIBRARY_SOURCE_KEY.to_owned(),
            boards: vec![
                RankingBoard {
                    board_key: JAVLIBRARY_BESTRATED_BOARD_KEY.to_owned(),
                    display_name: "JavLibrary 高评价".to_owned(),
                    supported_periods: periods(&crate::javlibrary::PERIODS),
                    default_period: JAVLIBRARY_DEFAULT_PERIOD.to_owned(),
                    dynamic_periods: false,
                },
                RankingBoard {
                    board_key: JAVLIBRARY_MOSTWANTED_BOARD_KEY.to_owned(),
                    display_name: "JavLibrary 最想要".to_owned(),
                    supported_periods: periods(&crate::javlibrary::PERIODS),
                    default_period: JAVLIBRARY_DEFAULT_PERIOD.to_owned(),
                    dynamic_periods: false,
                },
            ],
        },
    ]
}

/// 榜单周期表（上游 `MINNANO_PERIODS` / `JAVLIBRARY_PERIODS`）。
pub const MINNANO_PERIODS: [&str; 3] = crate::minnano::PERIODS;
/// 上游 `PluginRankingBoard(default_period="daily")`。
pub const MINNANO_DEFAULT_PERIOD: &str = "daily";
/// 榜单周期表（上游 `JAVLIBRARY_PERIODS`）。
pub const JAVLIBRARY_PERIODS: [&str; 2] = crate::javlibrary::PERIODS;
/// 上游 `PluginRankingBoard(default_period="monthly")`。
pub const JAVLIBRARY_DEFAULT_PERIOD: &str = "monthly";

/// 本插件声明的**全部**榜单 key（顺序即声明顺序）。
pub fn board_keys() -> Vec<&'static str> {
    vec![
        MINNANO_BOARD_KEY,
        JAVLIBRARY_BESTRATED_BOARD_KEY,
        JAVLIBRARY_MOSTWANTED_BOARD_KEY,
    ]
}

/// 某个榜单属于哪个源。
pub fn source_of(board_key: &str) -> Option<&'static str> {
    match board_key {
        MINNANO_BOARD_KEY => Some(MINNANO_SOURCE_KEY),
        JAVLIBRARY_BESTRATED_BOARD_KEY | JAVLIBRARY_MOSTWANTED_BOARD_KEY => {
            Some(JAVLIBRARY_SOURCE_KEY)
        }
        _ => None,
    }
}

fn periods(list: &[&'static str]) -> Vec<String> {
    list.iter().map(|period| (*period).to_owned()).collect()
}

fn check_plugin_id(injected: &str, expected: &str) -> Result<(), Status> {
    if injected != expected {
        return Err(Status::invalid_argument(format!(
            "宿主注入的 plugin_id 与启动参数不一致：注入 {injected}，本进程 {expected}"
        )));
    }
    Ok(())
}

/// 连回宿主的 `PluginHost`。
///
/// `host_endpoint` 是**这个插件自己的**宿主端点：进程式由组合根从进程环境读来
/// 传进来，进程内由组合根直接传。没有它就回 `failed_precondition` —— 静默
/// 跳过会让任务「看起来跑了但什么都没干」。
async fn host_client(
    host_endpoint: Option<&str>,
) -> Result<PluginHostClient<tonic::transport::Channel>, Status> {
    let addr = host_endpoint
        .map(str::to_owned)
        .ok_or_else(|| Status::failed_precondition(format!("缺少宿主端点（{HOST_ADDR_ENV}）")))?;
    PluginHostClient::connect(addr)
        .await
        .map_err(|e| Status::unavailable(format!("连接宿主失败: {e}")))
}

fn progress_event(text: String, current: i32, total: i32) -> JobEvent {
    JobEvent {
        event: Some(JobEventKind::Progress(ProgressEvent {
            text,
            current,
            total,
        })),
    }
}

fn result_event(value: serde_json::Value) -> JobEvent {
    JobEvent {
        event: Some(JobEventKind::Result(
            sm_plugin_api::json_struct::json_to_struct(&value).unwrap_or_default(),
        )),
    }
}

// ════════════════════════════════════════════════════════════════════
// 更多影片：控制面
// ════════════════════════════════════════════════════════════════════

/// `sakuramedia_more_movies` 的控制面。
pub struct MoreMoviesControl {
    plugin_id: String,
    settings: MoreMoviesSettings,
    /// 本插件自己的宿主 `PluginHost` 端点（进程式从环境读，进程内显式传）。
    host_endpoint: Option<String>,
    http: reqwest::Client,
}

impl MoreMoviesControl {
    pub fn new(
        plugin_id: String,
        settings: MoreMoviesSettings,
        host_endpoint: Option<String>,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(settings.timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent("sakuramedia-more-movies/0.1.0")
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        Ok(Self {
            plugin_id,
            settings,
            host_endpoint,
            http,
        })
    }
}

#[tonic::async_trait]
impl PluginControl for MoreMoviesControl {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let req = request.into_inner();
        if req.task_key != MORE_MOVIES_SYNC_TASK {
            return Err(Status::invalid_argument(format!(
                "未知 task_key: {}（本插件只有 {MORE_MOVIES_SYNC_TASK}）",
                req.task_key
            )));
        }
        let settings = self.settings.clone();
        let http = self.http.clone();
        let host_endpoint = self.host_endpoint.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);
        // 任务在后台跑，流只负责把事件搬出去 —— 宿主断开流即取消（协议原话）。
        tokio::spawn(async move {
            run_more_movies_sync(&tx, &settings, &http, host_endpoint.as_deref()).await;
        });
        Ok(Response::new(ReceiverStream::new(rx).boxed()))
    }

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let injected = request.into_inner().plugin_id;
        check_plugin_id(&injected, &self.plugin_id)?;
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: "SakuraMedia 更多影片".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            // 只有后台任务，不声明扩展点能力
            capabilities: vec![],
            extensions: vec![],
            jobs: vec![JobDefinition {
                task_key: MORE_MOVIES_SYNC_TASK.to_owned(),
                log_name: "more-movies-sync".to_owned(),
                cli_name: "sync-more-movies".to_owned(),
                cli_help: "抓取 JavDB 最新影片，热度达标且主库缺失的入库".to_owned(),
                default_cron: "0 6 * * *".to_owned(),
                manual_only: false,
                params_schema: None,
                required_capabilities: vec![],
            }],
            settings_schema: MoreMoviesSettings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 同步统计（上游 `sync.py:_empty_stats`）。
#[derive(Debug, Default, Clone)]
struct SyncStats {
    pages: u64,
    scanned: u64,
    skipped: u64,
    low_heat: u64,
    imported: u64,
    detail_failed: u64,
    import_failed: u64,
    page_failed: u64,
}

impl SyncStats {
    fn failed_total(&self) -> u64 {
        self.detail_failed + self.import_failed
    }

    fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "pages": self.pages,
            "scanned": self.scanned,
            "skipped": self.skipped,
            "low_heat": self.low_heat,
            "imported": self.imported,
            "detail_failed": self.detail_failed,
            "import_failed": self.import_failed,
            "page_failed": self.page_failed,
            "failed": self.failed_total(),
        })
    }
}

/// 更多影片同步主流程（上游 `sync.py:run_sync`）。
///
/// 事件边跑边发：一页可能翻很久，攒成 `Vec` 再发等于「任务期间进度条不动」。
/// HTTP 失败按页中断该类型（上游 `LatestPageError` 的语义）；宿主 RPC 失败则
/// 整个任务失败。
async fn run_more_movies_sync(
    tx: &mpsc::Sender<Result<JobEvent, Status>>,
    settings: &MoreMoviesSettings,
    http: &reqwest::Client,
    host_endpoint: Option<&str>,
) {
    let mut stats = SyncStats::default();
    // 进度事件 helper（不用闭包：闭包会借住 stats，与后面的可变使用冲突）
    macro_rules! emit {
        ($text:expr) => {
            let _ = tx
                .send(Ok(progress_event($text, stats.scanned as i32, 0)))
                .await;
        };
    }

    let mut host = match host_client(host_endpoint).await {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(e)).await;
            return;
        }
    };

    emit!("开始同步 JavDB 最新影片".to_owned());
    for (movie_type, label) in javdb::MOVIE_TYPES {
        let mut page: u32 = 0;
        loop {
            page += 1;
            if page > SAFETY_MAX_PAGES {
                emit!(format!("超过安全翻页上限 type={movie_type} page={page}"));
                break;
            }
            let items = match fetch_latest_page(http, settings, movie_type, page).await {
                Ok(items) => items,
                Err(e) => {
                    stats.page_failed += 1;
                    emit!(format!(
                        "列表翻页中断 type={movie_type} page={page} err={e}"
                    ));
                    break;
                }
            };
            if items.is_empty() {
                break;
            }
            stats.pages += 1;

            // 批量查主库已有番号（上游 `_existing_numbers`）
            let numbers: Vec<String> = items.iter().map(|i| i.number.clone()).collect();
            let existing: HashSet<String> = match host
                .find_movies_by_numbers(FindByNumbersRequest {
                    movie_numbers: numbers,
                })
                .await
            {
                Ok(resp) => resp
                    .into_inner()
                    .movies
                    .iter()
                    .filter_map(|m| {
                        let v = m.values.get("movie_number")?;
                        match &v.kind {
                            Some(prost_types::value::Kind::StringValue(s)) => {
                                Some(s.to_uppercase())
                            }
                            _ => None,
                        }
                    })
                    .collect(),
                Err(e) => {
                    let _ = tx
                        .send(Err(Status::unavailable(format!("查重失败: {e}"))))
                        .await;
                    return;
                }
            };

            for item in &items {
                stats.scanned += 1;
                if existing.contains(&item.number) {
                    stats.skipped += 1;
                    emit!(format!("{label} 第 {page} 页 · 主库已有 {}", item.number));
                    continue;
                }
                emit!(format!("{label} 第 {page} 页 · 处理 {}", item.number));
                process_item(&mut host, http, settings, item, &mut stats, tx).await;
                if !settings.detail_delay.is_zero() {
                    tokio::time::sleep(settings.detail_delay).await;
                }
            }

            emit!(format!(
                "{label} 第 {page} 页完成 · 已入库 {} · 跳过 {} · 低热度 {}",
                stats.imported, stats.skipped, stats.low_heat
            ));
            if items.len() < javdb::PAGE_SIZE {
                break;
            }
            if !settings.page_delay.is_zero() {
                tokio::time::sleep(settings.page_delay).await;
            }
        }
    }

    let final_stats = stats.as_json();
    let _ = tx
        .send(Ok(progress_event(
            format!(
                "完成 · 检查 {} 部 · 入库 {} 部 · 跳过 {} · 低热度 {} · 失败 {}",
                stats.scanned,
                stats.imported,
                stats.skipped,
                stats.low_heat,
                stats.failed_total()
            ),
            stats.scanned as i32,
            stats.scanned as i32,
        )))
        .await;
    let _ = tx.send(Ok(result_event(final_stats))).await;
}

/// 处理单个影片：详情 → 热度门槛 → 入库（上游 `sync.py:_process_item`）。
async fn process_item(
    host: &mut PluginHostClient<tonic::transport::Channel>,
    http: &reqwest::Client,
    settings: &MoreMoviesSettings,
    item: &LatestItem,
    stats: &mut SyncStats,
    tx: &mpsc::Sender<Result<JobEvent, Status>>,
) {
    // 详情抓取（上游 `provider.get_movie_by_javdb_id`）
    let detail_url = javdb::detail_url(&settings.javdb_api_host, &item.javdb_id);
    let detail: serde_json::Value = match javdb_get(http, &detail_url).await {
        Ok(v) => v,
        Err(e) => {
            stats.detail_failed += 1;
            let _ = tx
                .send(Ok(progress_event(
                    format!("详情抓取失败 number={} err={e}", item.number),
                    stats.scanned as i32,
                    0,
                )))
                .await;
            return;
        }
    };
    let heat = javdb::heat_inputs_from_detail(&detail).heat();
    if settings.min_heat > 0 && heat < settings.min_heat {
        stats.low_heat += 1;
        return;
    }
    // 入库（上游 `importer.import_movie_if_missing`；宿主按番号幂等）
    match host
        .import_movie_by_number(ImportMovieByNumberRequest {
            movie_number: item.number.clone(),
            force_subscribed: false,
        })
        .await
    {
        Ok(_) => {
            stats.imported += 1;
            let _ = tx
                .send(Ok(progress_event(
                    format!("已入库 number={} heat={heat}", item.number),
                    stats.scanned as i32,
                    0,
                )))
                .await;
        }
        Err(e) => {
            stats.import_failed += 1;
            let _ = tx
                .send(Ok(progress_event(
                    format!("入库失败 number={} err={e}", item.number),
                    stats.scanned as i32,
                    0,
                )))
                .await;
        }
    }
}

/// 带 jdsignature 的 JavDB 列表请求（上游 `fetch_latest_page` 的请求部分）。
async fn fetch_latest_page(
    http: &reqwest::Client,
    settings: &MoreMoviesSettings,
    movie_type: u8,
    page: u32,
) -> Result<Vec<LatestItem>, LatestPageError> {
    let url = javdb::latest_page_url(&settings.javdb_api_host, movie_type, page);
    let payload = javdb_get(http, &url)
        .await
        .map_err(|e| LatestPageError::Request {
            movie_type,
            page,
            detail: e,
        })?;
    javdb::parse_latest_page(&payload, movie_type, page)
}

/// 带签名头的 GET → JSON。
async fn javdb_get(http: &reqwest::Client, url: &str) -> Result<serde_json::Value, String> {
    let response = http
        .get(url)
        .header("jdsignature", javdb::jdsignature_now())
        .header("accept-language", "zh-TW")
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("解析 JSON 失败: {e}"))
}

// ════════════════════════════════════════════════════════════════════
// 更多影片榜单：控制面
// ════════════════════════════════════════════════════════════════════

/// `sakuramedia_more_rank_movies` 的控制面。
pub struct RankMoviesControl {
    plugin_id: String,
    settings: RankMoviesSettings,
    /// 本插件自己的宿主 `PluginHost` 端点（进程式从环境读，进程内显式传）。
    host_endpoint: Option<String>,
}

impl RankMoviesControl {
    pub fn new(
        plugin_id: String,
        settings: RankMoviesSettings,
        host_endpoint: Option<String>,
    ) -> Self {
        Self {
            plugin_id,
            settings,
            host_endpoint,
        }
    }

    pub fn settings(&self) -> &RankMoviesSettings {
        &self.settings
    }
}

#[tonic::async_trait]
impl PluginControl for RankMoviesControl {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let req = request.into_inner();
        // 参数校验放在**建流之前**：参数错了要回一个明确的 invalid_argument，
        // 而不是一条「先成功建流、再在流里报错」的路径 —— 后者在任务中心里
        // 显示成「跑过了」。
        let board_params = match req.task_key.as_str() {
            RANK_SYNC_TASK => None,
            RANK_SYNC_BOARD_TASK => Some(extract_board_params(&req)?),
            other => {
                return Err(Status::invalid_argument(format!(
                    "未知 task_key: {other}（本插件只有 {RANK_SYNC_TASK} / {RANK_SYNC_BOARD_TASK}）"
                )))
            }
        };
        let host_endpoint = self.host_endpoint.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);
        // 任务在后台跑，流只负责把事件搬出去 —— 宿主断开流即取消（协议原话）。
        tokio::spawn(async move {
            match board_params {
                None => run_rank_sync_all(&tx, host_endpoint.as_deref()).await,
                Some(params) => run_rank_sync_board(&tx, host_endpoint.as_deref(), params).await,
            }
        });
        Ok(Response::new(ReceiverStream::new(rx).boxed()))
    }

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let injected = request.into_inner().plugin_id;
        check_plugin_id(&injected, &self.plugin_id)?;
        let extensions = ranking_sources()
            .into_iter()
            .map(|source| Extension {
                key: crate::RANKING_SOURCE_KEY.to_owned(),
                data: Some(Data::RankingSource(source)),
            })
            .collect();
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: "SakuraMedia 更多影片榜单".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            capabilities: vec![Capability::ExtensionRankingSource as i32],
            extensions,
            jobs: vec![
                JobDefinition {
                    task_key: RANK_SYNC_TASK.to_owned(),
                    log_name: "more-rank-movies-sync".to_owned(),
                    cli_name: "sync-more-rank-movies".to_owned(),
                    cli_help: "同步更多影片榜单（Minnano AV、JavLibrary）".to_owned(),
                    default_cron: "45 1 * * *".to_owned(),
                    manual_only: false,
                    params_schema: None,
                    required_capabilities: vec![],
                },
                // 上游 `sync_jobs.py` 的第二个任务：手动触发单个榜单。
                JobDefinition {
                    task_key: RANK_SYNC_BOARD_TASK.to_owned(),
                    log_name: "more-rank-movies-sync-board".to_owned(),
                    cli_name: "sync-more-rank-movies-board".to_owned(),
                    cli_help: "手动同步更多影片榜单中的一个榜单".to_owned(),
                    default_cron: String::new(),
                    manual_only: true,
                    params_schema: Some(board_params_schema()),
                    required_capabilities: vec![],
                },
            ],
            settings_schema: RankMoviesSettings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 手动单榜同步的参数（上游 `sync_jobs.py:SyncBoardParams`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardParams {
    /// 榜单 key。**源 key 由榜单推出来**，不吃参数里给的那个 —— 上游两个键
    /// 都要，但两者的对应关系在本插件里是固定的（见 [`source_of`]），
    /// 让调用方再传一遍只会多一处能写错的地方。
    pub board_key: String,
    /// 空串 = 该榜的默认周期（宿主那边同一语义）。
    pub period: String,
}

impl BoardParams {
    /// 该榜单所属的源 key。
    pub fn source_key(&self) -> &'static str {
        source_of(&self.board_key).unwrap_or(MINNANO_SOURCE_KEY)
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
        // 榜单 key 是 slug：1..=64（上游 `SyncBoardParams` 的约束；是否属于
        // 本插件由 `extract_board_params` 查表决定）。
        (
            "board_key".to_owned(),
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
    if source_of(&board_key).is_none() {
        return Err(Status::invalid_argument(format!(
            "未知榜单：{board_key}（本插件有 {:?}）",
            board_keys()
        )));
    }
    Ok(BoardParams {
        board_key,
        period: read("period")?.unwrap_or_default().trim().to_owned(),
    })
}

/// 全量榜单同步（上游 `sync_jobs.py:run_sync_all`）：调宿主的一次性全量接口。
async fn run_rank_sync_all(tx: &mpsc::Sender<Result<JobEvent, Status>>, host_endpoint: Option<&str>) {
    let _ = tx
        .send(Ok(progress_event("开始同步全部榜单".to_owned(), 0, 0)))
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

/// 单榜同步（上游 `sync_jobs.py:run_sync_board`）：转发宿主的 `SyncRankingBoard`。
async fn run_rank_sync_board(
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
        source_key: params.source_key().to_owned(),
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

// ════════════════════════════════════════════════════════════════════
// 更多影片榜单：榜单扩展点
// ════════════════════════════════════════════════════════════════════

/// `discovery.ranking_source` 扩展点：按榜单 key 分发到 Minnano / JavLibrary。
pub struct RankingService {
    settings: RankMoviesSettings,
}

impl RankingService {
    pub fn new(settings: RankMoviesSettings) -> Self {
        Self { settings }
    }
}

#[tonic::async_trait]
impl RankingSourceExtensionService for RankingService {
    async fn resolve_ranking_periods(
        &self,
        request: Request<ResolveRankingPeriodsRequest>,
    ) -> Result<Response<ResolveRankingPeriodsResponse>, Status> {
        // Minnano / JavLibrary 榜单都是静态周期，无需动态解析。
        let _ = request;
        Ok(Response::new(ResolveRankingPeriodsResponse {
            periods: Vec::new(),
        }))
    }

    async fn fetch_ranking(
        &self,
        request: Request<FetchRankingRequest>,
    ) -> Result<Response<FetchRankingResponse>, Status> {
        let req = request.into_inner();
        let numbers = match req.board_key.as_str() {
            MINNANO_BOARD_KEY => {
                let mut client =
                    MinnanoAvClient::new(self.settings.timeout, self.settings.request_interval)
                        .map_err(|e| Status::internal(format!("{e}")))?;
                client
                    .get_rank_numbers(&req.period)
                    .await
                    .map_err(|e| match e {
                        MinnanoError::BadPeriod(_) => Status::invalid_argument(format!("{e}")),
                        _ => Status::unavailable(format!("{e}")),
                    })?
            }
            JAVLIBRARY_BESTRATED_BOARD_KEY => {
                let mut client = JavLibraryClient::new(
                    self.settings.timeout,
                    self.settings.request_interval,
                    javlibrary::MAX_PAGES,
                )
                .map_err(|e| Status::internal(format!("{e}")))?;
                client
                    .get_bestrated_numbers(&req.period)
                    .await
                    .map_err(|e| match e {
                        JavLibraryError::BadPeriod(_) => Status::invalid_argument(format!("{e}")),
                        _ => Status::unavailable(format!("{e}")),
                    })?
            }
            JAVLIBRARY_MOSTWANTED_BOARD_KEY => {
                let mut client = JavLibraryClient::new(
                    self.settings.timeout,
                    self.settings.request_interval,
                    javlibrary::MAX_PAGES,
                )
                .map_err(|e| Status::internal(format!("{e}")))?;
                client
                    .get_mostwanted_numbers(&req.period)
                    .await
                    .map_err(|e| match e {
                        JavLibraryError::BadPeriod(_) => Status::invalid_argument(format!("{e}")),
                        _ => Status::unavailable(format!("{e}")),
                    })?
            }
            other => {
                return Err(Status::not_found(format!("未知榜单: {other}")));
            }
        };
        Ok(Response::new(FetchRankingResponse {
            movie_numbers: numbers,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> RankMoviesControl {
        RankMoviesControl::new(
            "sakuramedia_more_rank_movies".to_owned(),
            RankMoviesSettings::default(),
            None,
        )
    }

    fn run_request(task_key: &str, params: Option<serde_json::Value>) -> RunJobRequest {
        RunJobRequest {
            run_id: "t".to_owned(),
            task_key: task_key.to_owned(),
            params: params.and_then(|value| sm_plugin_api::json_struct::json_to_struct(&value)),
            data_dir: String::new(),
        }
    }

    /// 周期集合照上游 `boards.py` 的三个 board 逐项对齐。
    #[test]
    fn the_three_boards_declare_the_upstream_periods() {
        let sources = ranking_sources();
        assert_eq!(sources.len(), 2, "两个来源：Minnano AV、JavLibrary");

        let minnano = &sources[0];
        assert_eq!(minnano.source_key, MINNANO_SOURCE_KEY);
        assert_eq!(minnano.boards.len(), 1);
        assert_eq!(minnano.boards[0].board_key, MINNANO_BOARD_KEY);
        assert_eq!(minnano.boards[0].supported_periods, periods(&MINNANO_PERIODS));
        assert_eq!(minnano.boards[0].default_period, "daily");

        let javlibrary = &sources[1];
        assert_eq!(javlibrary.source_key, JAVLIBRARY_SOURCE_KEY);
        let keys: Vec<&str> = javlibrary
            .boards
            .iter()
            .map(|board| board.board_key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![JAVLIBRARY_BESTRATED_BOARD_KEY, JAVLIBRARY_MOSTWANTED_BOARD_KEY]
        );
        for board in &javlibrary.boards {
            assert_eq!(board.supported_periods, periods(&JAVLIBRARY_PERIODS));
            assert_eq!(board.default_period, "monthly");
        }
        // 三个榜单的周期都是**静态**的（上游没有 `supported_periods_provider`）。
        for board in sources.iter().flat_map(|source| &source.boards) {
            assert!(!board.dynamic_periods, "{} 不该是动态周期", board.board_key);
            assert!(
                !board.supported_periods.is_empty(),
                "{} 缺周期集合：宿主会当成「没有合法周期」",
                board.board_key
            );
        }
    }

    /// 榜单 key ↔ 源 key 的对应关系：宿主按 source_key 派发，写错就同步不到。
    #[test]
    fn every_board_maps_to_its_source() {
        for board in board_keys() {
            let source = source_of(board).expect(board);
            assert!(
                ranking_sources()
                    .iter()
                    .any(|declared| declared.source_key == source
                        && declared
                            .boards
                            .iter()
                            .any(|entry| entry.board_key == board)),
                "{board} 声明的源 {source} 与榜单表不一致"
            );
        }
        assert_eq!(source_of("minnano_av"), Some(MINNANO_SOURCE_KEY));
        assert_eq!(source_of("javlibrary_bestrated"), Some(JAVLIBRARY_SOURCE_KEY));
        assert_eq!(source_of("javlibrary_mostwanted"), Some(JAVLIBRARY_SOURCE_KEY));
        assert_eq!(source_of("unknown_board"), None);
    }

    /// 两个任务：全量（定时）+ 单榜（手动、要参数）。
    #[tokio::test]
    async fn both_rank_tasks_are_declared() {
        let response = PluginControl::register(
            &control(),
            Request::new(RegisterRequest {
                plugin_id: "sakuramedia_more_rank_movies".to_owned(),
                abi_major: sm_plugin_api::ABI_MAJOR,
            }),
        )
        .await
        .expect("register")
        .into_inner();

        let jobs = response.jobs;
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].task_key, RANK_SYNC_TASK);
        assert!(!jobs[0].manual_only);
        assert_eq!(jobs[0].default_cron, "45 1 * * *");
        assert!(jobs[0].params_schema.is_none(), "全量同步不吃参数");

        assert_eq!(jobs[1].task_key, RANK_SYNC_BOARD_TASK);
        assert!(jobs[1].manual_only, "单榜同步只能手动触发");
        assert!(jobs[1].default_cron.is_empty());
        assert!(jobs[1].params_schema.is_some(), "单榜同步要 board_key");

        // 注册声明的能力与扩展点：一个排行源（两张榜）＋ 能力位。
        assert_eq!(
            response.capabilities,
            vec![Capability::ExtensionRankingSource as i32]
        );
        assert_eq!(response.extensions.len(), 2);
    }

    /// ★ 单榜同步的参数：`board_key` 必填且必须是**我们自己的**榜单。
    #[test]
    fn board_params_are_validated_before_the_stream_starts() {
        let parsed = extract_board_params(&run_request(
            RANK_SYNC_BOARD_TASK,
            Some(serde_json::json!({
                "board_key": " minnano_av ",
                "period": " weekly ",
            })),
        ))
        .expect("正常参数");
        assert_eq!(parsed.board_key, "minnano_av", "首尾空白要裁掉");
        assert_eq!(parsed.period, "weekly");
        assert_eq!(parsed.source_key(), MINNANO_SOURCE_KEY);

        // 缺 params / 空串 / 别人的榜单 / 类型不对，都要在**建流之前**拒。
        for bad in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({ "board_key": "  " })),
            Some(serde_json::json!({ "board_key": "javdb_playback_all" })),
            Some(serde_json::json!({ "board_key": 1 })),
            Some(serde_json::json!({ "board_key": "minnano_av", "period": 1 })),
        ] {
            let error = extract_board_params(&run_request(RANK_SYNC_BOARD_TASK, bad.clone()))
                .expect_err(&format!("{bad:?} 该被拒"));
            assert_eq!(error.code(), tonic::Code::InvalidArgument, "{bad:?}");
        }

        // 空周期是允许的：宿主按该榜的默认周期算。
        let parsed = extract_board_params(&run_request(
            RANK_SYNC_BOARD_TASK,
            Some(serde_json::json!({ "board_key": "javlibrary_bestrated" })),
        ))
        .expect("period 可缺省");
        assert_eq!(parsed.period, "");
        assert_eq!(parsed.source_key(), JAVLIBRARY_SOURCE_KEY);
    }

    /// 未知 task_key 在建流之前就回 `invalid_argument`（不是 `not_found`：
    /// 任务中心把两者都当失败，但前者能让调用方看出「参数写错了」）。
    #[tokio::test]
    async fn an_unknown_task_key_is_an_invalid_argument() {
        let error = PluginControl::run_job(
            &control(),
            Request::new(run_request("sakuramedia_more_movies_sync", None)),
        )
        .await
        .err()
        .expect("未知任务该被拒");
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().contains(RANK_SYNC_BOARD_TASK), "{}", error.message());
    }
}
