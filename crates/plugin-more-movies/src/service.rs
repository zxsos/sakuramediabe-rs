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
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::job_event::Event as JobEventKind;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionService;
use sm_plugin_api::v1::{
    Capability, Extension, FetchRankingRequest, FetchRankingResponse, FindByNumbersRequest,
    ImportMovieByNumberRequest, JobDefinition, JobEvent, ProgressEvent, RankingBoard,
    RankingSourceExtension, RegisterRequest, RegisterResponse, ResolveRankingPeriodsRequest,
    ResolveRankingPeriodsResponse, RunJobRequest, SyncRankingSourcesRequest,
};
use tonic::{Request, Response, Status};

use crate::javdb::{self, LatestItem, LatestPageError, SAFETY_MAX_PAGES};
use crate::javlibrary::{self, JavLibraryClient, JavLibraryError};
use crate::minnano::{MinnanoAvClient, MinnanoError};
use crate::settings::{MoreMoviesSettings, RankMoviesSettings};

/// 宿主 `PluginHost` 服务的地址（宿主注入）。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

// ── 任务 key（上游各 plugin.py 的 task_key）─────────────────────────────

pub const MORE_MOVIES_SYNC_TASK: &str = "sakuramedia_more_movies_sync";
pub const RANK_SYNC_TASK: &str = "sakuramedia_more_rank_movies_sync";
// 注意：上游还有 `sakuramedia_more_rank_movies_sync_board`（手动单榜同步），
// 但 v0.2.0 契约没有对应的宿主 RPC（`SyncRankingBoard` 是后加的），所以这里
// 不声明 —— 契约升级后再补。

// ── 榜单 key（上游 boards.py）────────────────────────────────────────────

pub const MINNANO_SOURCE_KEY: &str = "minnano_av";
pub const JAVLIBRARY_SOURCE_KEY: &str = "javlibrary";
pub const MINNANO_BOARD_KEY: &str = "minnano_av";
pub const JAVLIBRARY_BESTRATED_BOARD_KEY: &str = "javlibrary_bestrated";
pub const JAVLIBRARY_MOSTWANTED_BOARD_KEY: &str = "javlibrary_mostwanted";

/// 榜单声明（上游 `boards.py:build_ranking_sources`）。
///
/// v0.2.0 的 `RankingBoard` 只有 `board_key` + `display_name`（周期集合是后加的），
/// 周期合法性在 `fetch_ranking` 里按上游的 `MINNANO_PERIODS` /
/// `JAVLIBRARY_PERIODS` 校验，不支持的周期回 `invalid_argument`。
pub fn ranking_sources() -> Vec<RankingSourceExtension> {
    vec![
        RankingSourceExtension {
            source_key: MINNANO_SOURCE_KEY.to_owned(),
            boards: vec![RankingBoard {
                board_key: MINNANO_BOARD_KEY.to_owned(),
                display_name: "Minnano AV".to_owned(),
                supported_periods: Vec::new(),
                default_period: String::new(),
                dynamic_periods: false,
            }],
        },
        RankingSourceExtension {
            source_key: JAVLIBRARY_SOURCE_KEY.to_owned(),
            boards: vec![
                RankingBoard {
                    board_key: JAVLIBRARY_BESTRATED_BOARD_KEY.to_owned(),
                    display_name: "JavLibrary 高评价".to_owned(),
                    supported_periods: Vec::new(),
                    default_period: String::new(),
                    dynamic_periods: false,
                },
                RankingBoard {
                    board_key: JAVLIBRARY_MOSTWANTED_BOARD_KEY.to_owned(),
                    display_name: "JavLibrary 最想要".to_owned(),
                    supported_periods: Vec::new(),
                    default_period: String::new(),
                    dynamic_periods: false,
                },
            ],
        },
    ]
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
async fn host_client() -> Result<PluginHostClient<tonic::transport::Channel>, Status> {
    let addr = std::env::var(HOST_ADDR_ENV)
        .map_err(|_| Status::failed_precondition(format!("缺少环境变量 {HOST_ADDR_ENV}")))?;
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
    http: reqwest::Client,
}

impl MoreMoviesControl {
    pub fn new(plugin_id: String, settings: MoreMoviesSettings) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(settings.timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent("sakuramedia-more-movies/0.1.0")
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        Ok(Self {
            plugin_id,
            settings,
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
            return Err(Status::not_found(format!("未知任务: {}", req.task_key)));
        }
        let settings = self.settings.clone();
        let http = self.http.clone();
        // 事件先攒成 Vec 再转成流：不用 async-stream（离线缓存里没有它）。
        let events = run_more_movies_sync(&settings, &http).await;
        Ok(Response::new(Box::pin(futures::stream::iter(events))))
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
/// 返回事件序列：进度事件 + 最后的结果事件。HTTP 失败按页中断该类型
/// （上游 `LatestPageError` 的语义）；宿主 RPC 失败则整个任务失败。
async fn run_more_movies_sync(
    settings: &MoreMoviesSettings,
    http: &reqwest::Client,
) -> Vec<Result<JobEvent, Status>> {
    let mut out = Vec::new();
    let mut stats = SyncStats::default();
    // 进度事件 helper（不用闭包：闭包会借住 stats/out，与后面的可变使用冲突）
    macro_rules! emit {
        ($text:expr) => {
            out.push(Ok(progress_event($text, stats.scanned as i32, 0)))
        };
    }

    let mut host = match host_client().await {
        Ok(c) => c,
        Err(e) => {
            out.push(Err(e));
            return out;
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
                    out.push(Err(Status::unavailable(format!("查重失败: {e}"))));
                    return out;
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
                process_item(&mut host, http, settings, item, &mut stats, &mut out).await;
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
    out.push(Ok(progress_event(
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
    )));
    out.push(Ok(result_event(final_stats)));
    out
}

/// 处理单个影片：详情 → 热度门槛 → 入库（上游 `sync.py:_process_item`）。
async fn process_item(
    host: &mut PluginHostClient<tonic::transport::Channel>,
    http: &reqwest::Client,
    settings: &MoreMoviesSettings,
    item: &LatestItem,
    stats: &mut SyncStats,
    out: &mut Vec<Result<JobEvent, Status>>,
) {
    // 详情抓取（上游 `provider.get_movie_by_javdb_id`）
    let detail_url = javdb::detail_url(&settings.javdb_api_host, &item.javdb_id);
    let detail: serde_json::Value = match javdb_get(http, &detail_url).await {
        Ok(v) => v,
        Err(e) => {
            stats.detail_failed += 1;
            out.push(Ok(progress_event(
                format!("详情抓取失败 number={} err={e}", item.number),
                stats.scanned as i32,
                0,
            )));
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
            out.push(Ok(progress_event(
                format!("已入库 number={} heat={heat}", item.number),
                stats.scanned as i32,
                0,
            )));
        }
        Err(e) => {
            stats.import_failed += 1;
            out.push(Ok(progress_event(
                format!("入库失败 number={} err={e}", item.number),
                stats.scanned as i32,
                0,
            )));
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
}

impl RankMoviesControl {
    pub fn new(plugin_id: String) -> Self {
        Self {
            plugin_id,
            settings: RankMoviesSettings::load(),
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
        match req.task_key.as_str() {
            RANK_SYNC_TASK => {
                let events = run_rank_sync_all().await;
                Ok(Response::new(Box::pin(futures::stream::iter(events))))
            }
            other => Err(Status::not_found(format!("未知任务: {other}"))),
        }
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
                // 注意：上游还有 `sakuramedia_more_rank_movies_sync_board`
                // （手动单榜同步），但 v0.2.0 契约没有对应的宿主 RPC
                // （`SyncRankingBoard` 是后加的），所以这里不声明 ——
                // 契约升级后再补。
            ],
            settings_schema: RankMoviesSettings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 全量榜单同步（上游 `sync_jobs.py:run_sync_all`）：调宿主的一次性全量接口。
async fn run_rank_sync_all() -> Vec<Result<JobEvent, Status>> {
    let mut out = vec![Ok(progress_event("开始同步全部榜单".to_owned(), 0, 0))];
    let mut host = match host_client().await {
        Ok(c) => c,
        Err(e) => {
            out.push(Err(e));
            return out;
        }
    };
    match host
        .sync_ranking_sources(SyncRankingSourcesRequest {})
        .await
    {
        Ok(resp) => {
            let r = resp.into_inner();
            let value = serde_json::json!({
                "synced_count": r.synced_count,
            });
            out.push(Ok(progress_event(
                format!("榜单同步完成 · 成功 {} 个目标", r.synced_count),
                r.synced_count,
                0,
            )));
            out.push(Ok(result_event(value)));
        }
        Err(e) => out.push(Err(Status::unavailable(format!("同步榜单失败: {e}")))),
    }
    out
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
