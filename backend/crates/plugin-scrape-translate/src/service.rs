//! gRPC 服务面：`PluginControl`（本插件是纯任务型，没有扩展点）。
//!
//! # 三个 job（上游 `jobs.py:build_jobs`）
//!
//! | task_key | 上游 | 说明 |
//! |---|---|---|
//! | `sakuramedia_movie_scrape_translate_sync` | `run_manual` | 手动：按番号抓取并翻译 |
//! | `sakuramedia_movie_scrape_translate_sync_subscribed` | `run_daily` | 定时（每天 04:10）：按优先级全量 |
//! | `sakuramedia_movie_scrape_translate_translate_cached` | `run_translate_cached` | 手动：只翻译已有缓存，不请求 DMM |
//!
//! # 影片存取走宿主的 `PluginHost`
//!
//! 上游是进程内插件，`context.movies` 是同步对象。拆成 gRPC 插件后，影片的
//! 列举 / 读取 / 写回都得反向调宿主（[`GrpcMovieStore`]，实现
//! [`jobs::MovieStore`]）：
//!
//! | 上游 `context.movies` | 宿主 rpc |
//! |---|---|
//! | `find_by_numbers([...])` | `FindMoviesByNumbers` |
//! | `list_page(after_id, limit)` | `ListMovies` |
//! | `get(movie_id)`（`_writable` 的复核） | `GetMovie` |
//! | `patch(id, fields, expected_revision)` | `PatchMovie` |
//!
//! 端点由宿主给（进程式走 `SAKURAMEDIA_HOST_GRPC_ADDR`，进程内由组合根传）。
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control`] 的
//! `register` 只回声明；宿主连接、状态库、锁文件都是**每次 `RunJob` 时**才建的。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use sm_plugin_api::v1::job_event::Event as JobEventKind;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::{
    FindByNumbersRequest, GetMovieRequest, JobDefinition, JobEvent, ListMoviesRequest,
    MovieSnapshot, PatchMovieRequest, ProgressEvent, RegisterRequest, RegisterResponse,
    RunJobRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use crate::jobs::{self, MovieRef, MovieStore, PipelineError, PipelineStats, Progress};
use crate::settings::{self, Settings};

/// 手动：按番号抓取并翻译。
pub const TASK_SYNC: &str = "sakuramedia_movie_scrape_translate_sync";
/// 定时：每天按优先级抓取并翻译。
pub const TASK_SYNC_SUBSCRIBED: &str = "sakuramedia_movie_scrape_translate_sync_subscribed";
/// 手动：只翻译已有 DMM 缓存。
pub const TASK_TRANSLATE_CACHED: &str = "sakuramedia_movie_scrape_translate_translate_cached";

/// 宿主能力出口的端点（`sm-plugins` 的 `supervisor::HOST_ADDR_ENV`）。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

/// 连一次宿主的超时上限（宿主就在本机，正常是毫秒级；给上限是为了让「宿主没
/// 起来」表现成一次可读的失败，而不是任务永远挂着）。
const HOST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 三个任务的定义（上游 `build_jobs`）。
pub fn job_definitions() -> Vec<JobDefinition> {
    vec![
        JobDefinition {
            task_key: TASK_SYNC.to_owned(),
            log_name: "movie-scrape-translate".to_owned(),
            cli_name: "scrape-translate-movies".to_owned(),
            cli_help: "抓取并翻译影片标题和简介".to_owned(),
            default_cron: String::new(),
            manual_only: true,
            params_schema: Some(params_schema()),
            required_capabilities: Vec::new(),
        },
        JobDefinition {
            task_key: TASK_SYNC_SUBSCRIBED.to_owned(),
            log_name: "movie-scrape-translate-subscribed".to_owned(),
            cli_name: "scrape-translate-subscribed-movies".to_owned(),
            cli_help: "每天按优先级抓取并翻译影片文案".to_owned(),
            default_cron: "10 4 * * *".to_owned(),
            manual_only: false,
            params_schema: None,
            required_capabilities: Vec::new(),
        },
        JobDefinition {
            task_key: TASK_TRANSLATE_CACHED.to_owned(),
            log_name: "movie-translate-cached".to_owned(),
            cli_name: "translate-cached-movies".to_owned(),
            cli_help: "仅翻译已有 DMM 缓存原文的影片文案，不请求 DMM".to_owned(),
            default_cron: String::new(),
            manual_only: true,
            params_schema: None,
            required_capabilities: Vec::new(),
        },
    ]
}

/// 手动任务的参数 schema（上游 `DmmSyncParams`：`movie_number` 必填）。
fn params_schema() -> prost_types::Struct {
    use prost_types::value::Kind;
    use prost_types::{Struct, Value};
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "type".to_owned(),
        Value {
            kind: Some(Kind::StringValue("object".to_owned())),
        },
    );
    let mut props = std::collections::BTreeMap::new();
    let mut movie_number = std::collections::BTreeMap::new();
    movie_number.insert(
        "type".to_owned(),
        Value {
            kind: Some(Kind::StringValue("string".to_owned())),
        },
    );
    movie_number.insert(
        "minLength".to_owned(),
        Value {
            kind: Some(Kind::NumberValue(1.0)),
        },
    );
    movie_number.insert(
        "maxLength".to_owned(),
        Value {
            kind: Some(Kind::NumberValue(128.0)),
        },
    );
    props.insert(
        "movie_number".to_owned(),
        Value {
            kind: Some(Kind::StructValue(Struct {
                fields: movie_number,
            })),
        },
    );
    fields.insert(
        "properties".to_owned(),
        Value {
            kind: Some(Kind::StructValue(Struct { fields: props })),
        },
    );
    fields.insert(
        "required".to_owned(),
        Value {
            kind: Some(Kind::ListValue(prost_types::ListValue {
                values: vec![Value {
                    kind: Some(Kind::StringValue("movie_number".to_owned())),
                }],
            })),
        },
    );
    Struct { fields }
}

/// 控制面。
pub struct Control {
    plugin_id: String,
    settings: Settings,
    /// 宿主能力出口端点。`None` = 宿主没暴露 `PluginHost`，任务直接失败。
    host_endpoint: Option<String>,
}

impl Control {
    /// 无宿主端点。单测与「只想注册一下」的场合用。
    pub fn new(plugin_id: String, settings: Settings) -> Self {
        Self {
            plugin_id,
            settings,
            host_endpoint: None,
        }
    }

    /// 进程式与进程内**共用**的构造：配置与宿主端点显式传入。
    pub fn with_runtime(
        plugin_id: String,
        settings: Settings,
        host_endpoint: Option<String>,
    ) -> Self {
        Self {
            plugin_id,
            settings,
            host_endpoint,
        }
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
        let task_key = inner.task_key.clone();
        let translate_only = task_key == TASK_TRANSLATE_CACHED;
        // 参数校验在建流之前（见 `plugin-subtitlecat/src/service.rs` 同一纪律）。
        let movie_number = match task_key.as_str() {
            TASK_SYNC => Some(extract_movie_number(&inner)?),
            TASK_SYNC_SUBSCRIBED | TASK_TRANSLATE_CACHED => None,
            other => return Err(Status::invalid_argument(format!("未知 task_key: {other}"))),
        };
        if translate_only && !self.settings.translation_enabled {
            // 上游在 `run_pipeline` 里 raise；这里提前一步，能让调用方在
            // 「流已经开了又立刻报错」之前就拿到原因。
            return Err(Status::failed_precondition("仅翻译任务需要启用翻译"));
        }

        let settings = self.settings.clone();
        let host_endpoint = self.host_endpoint.clone();
        let data_dir = inner.data_dir.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);
        // 任务在后台跑，流只负责把事件搬出去 —— 宿主断开流即取消。
        tokio::spawn(async move {
            let outcome = run_job_once(
                &settings,
                host_endpoint.as_deref(),
                &data_dir,
                movie_number.as_deref(),
                translate_only,
                tx.clone(),
            )
            .await;
            match outcome {
                Ok(stats) => {
                    let event = JobEvent {
                        event: Some(JobEventKind::Result(stats.to_struct())),
                    };
                    let _ = tx.send(Ok(event)).await;
                }
                Err(err) => {
                    let _ = tx.send(Err(status_of(&err))).await;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
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
            // 纯任务型插件：不声明扩展点能力（Capability 只有存储/播放/转存/
            // 扩展点/下载几类，任务不需要声明）。
            capabilities: Vec::new(),
            extensions: Vec::new(),
            jobs: job_definitions(),
            settings_schema: settings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 跑一次任务（上游三个 handler 的公共外壳）。
async fn run_job_once(
    settings: &Settings,
    host_endpoint: Option<&str>,
    data_dir: &str,
    movie_number: Option<&str>,
    translate_only: bool,
    tx: mpsc::Sender<Result<JobEvent, Status>>,
) -> Result<PipelineStats, PipelineError> {
    let store = host_client(host_endpoint).await?;
    let mut progress = ChannelProgress::new(tx);
    jobs::run_pipeline(
        &store,
        settings,
        Path::new(data_dir),
        movie_number,
        translate_only,
        &mut progress,
    )
    .await
}

/// 从 `RunJobRequest` 里取 `movie_number` 参数（上游 `DmmSyncParams`）。
fn extract_movie_number(request: &RunJobRequest) -> Result<String, Status> {
    let params = request
        .params
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("缺少任务参数：需要 movie_number"))?;
    let value = params
        .fields
        .get("movie_number")
        .ok_or_else(|| Status::invalid_argument("缺少任务参数：需要 movie_number"))?;
    let raw = match value.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(text)) => text.clone(),
        _ => return Err(Status::invalid_argument("movie_number 必须是字符串")),
    };
    // 上游 `str_strip_whitespace=True` + `min_length=1`。
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument("movie_number 不能为空"));
    }
    Ok(trimmed.to_owned())
}

/// 连一次宿主，包成 [`MovieStore`]。
async fn host_client(host_endpoint: Option<&str>) -> Result<GrpcMovieStore, PipelineError> {
    let host_addr = host_endpoint.ok_or_else(|| {
        PipelineError::Host(format!(
            "宿主没有暴露 PluginHost（{HOST_ADDR_ENV} / 进程内 host_endpoint 都没给）"
        ))
    })?;
    // 端点由宿主给：`serve_for`（进程内）与宿主注入的环境变量（进程式）交出来的
    // 都是带 scheme 的完整 URL（`http://127.0.0.1:<port>`）。这里**只补不拼**：
    // 早期版本无条件写 `format!("http://{host_addr}")`，遇到完整 URL 就成了
    // `http://http://…` —— 报错是 `host:connect:transport error`，看不出原因。
    let endpoint = if host_addr.starts_with("http://") || host_addr.starts_with("https://") {
        host_addr.to_owned()
    } else {
        format!("http://{host_addr}")
    };
    let channel = tokio::time::timeout(HOST_CONNECT_TIMEOUT, async {
        Channel::from_shared(endpoint)
            .map_err(|e| PipelineError::Host(format!("host:bad_addr:{e}")))?
            .connect()
            .await
            .map_err(|e| PipelineError::Host(format!("host:connect:{e}")))
    })
    .await
    .map_err(|_| PipelineError::Host("host:connect:timeout".to_owned()))??;
    Ok(GrpcMovieStore {
        client: tokio::sync::Mutex::new(PluginHostClient::new(channel)),
    })
}

/// [`MovieStore`] 的生产实现：把 trait 的方法翻成 `PluginHost` 的调用。
pub struct GrpcMovieStore {
    /// `PluginHostClient` 的方法是 `&mut self`，而 trait 给的是 `&self` —— 用
    /// 异步锁桥一下。管线是顺序跑的，锁不会成为瓶颈。
    client: tokio::sync::Mutex<PluginHostClient<Channel>>,
}

impl GrpcMovieStore {
    /// 用现成的客户端建（单测可以直接塞一个假的连接）。
    pub fn new(client: PluginHostClient<Channel>) -> Self {
        Self {
            client: tokio::sync::Mutex::new(client),
        }
    }
}

#[async_trait]
impl MovieStore for GrpcMovieStore {
    async fn find_by_number(&self, number: &str) -> Result<Option<MovieRef>, PipelineError> {
        let mut client = self.client.lock().await;
        let response = client
            .find_movies_by_numbers(FindByNumbersRequest {
                movie_numbers: vec![number.to_owned()],
            })
            .await
            .map_err(|e| PipelineError::Host(format!("host:find_movies_by_numbers:{e}")))?
            .into_inner();
        // 上游取第一条（`find_by_numbers` 找不到就不出现在结果里）。
        Ok(response.movies.into_iter().next().map(movie_ref))
    }

    async fn list_page(
        &self,
        after_id: i64,
        limit: usize,
    ) -> Result<(Vec<MovieRef>, Option<i64>), PipelineError> {
        let mut client = self.client.lock().await;
        let response = client
            .list_movies(ListMoviesRequest {
                after_id,
                limit: limit as i32,
                // 上游按 `is_subscribed` / 年份等在**插件侧**筛；宿主的 `filters`
                // 还没映射（传了会直接 `unimplemented`），所以这里也不传。
                filters: None,
            })
            .await
            .map_err(|e| PipelineError::Host(format!("host:list_movies:{e}")))?
            .into_inner();
        Ok((
            response.movies.into_iter().map(movie_ref).collect(),
            response.next_cursor,
        ))
    }

    async fn reload(&self, movie_id: i64) -> Result<Option<MovieRef>, PipelineError> {
        let mut client = self.client.lock().await;
        match client.get_movie(GetMovieRequest { movie_id }).await {
            Ok(response) => Ok(response.into_inner().movie.map(movie_ref)),
            // 影片没了：上游 `_writable` 里同样是「返回 None」（不报错）。
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(PipelineError::Host(format!("host:get_movie:{status}"))),
        }
    }

    async fn patch(
        &self,
        movie_id: i64,
        title: Option<&str>,
        summary: Option<&str>,
        expected_revision: i64,
    ) -> Result<bool, PipelineError> {
        let mut fields = HashMap::new();
        if let Some(title) = title {
            fields.insert("title".to_owned(), string_value(title));
        }
        if let Some(summary) = summary {
            fields.insert("summary".to_owned(), string_value(summary));
        }
        let mut client = self.client.lock().await;
        let response = client
            .patch_movie(PatchMovieRequest {
                movie_id,
                fields,
                expected_revision,
            })
            .await
            .map_err(|e| PipelineError::Host(format!("host:patch_movie:{e}")))?
            .into_inner();
        Ok(response.updated)
    }
}

/// 影片快照 → 管线要的 [`MovieRef`]。
fn movie_ref(snapshot: MovieSnapshot) -> MovieRef {
    MovieRef {
        movie_id: snapshot.movie_id,
        revision: snapshot.revision,
        movie_number: value_string(&snapshot.values, "movie_number").unwrap_or_default(),
        title: value_string(&snapshot.values, "title").unwrap_or_default(),
        summary: value_string(&snapshot.values, "summary").unwrap_or_default(),
        // 有主的字段才在 map 里；值是 owner 字符串。
        owners: snapshot
            .field_owners
            .into_iter()
            .map(|(field, owner)| (field, Some(owner)))
            .collect(),
        release_year: value_string(&snapshot.values, "release_date")
            .and_then(|date| date.get(..4).and_then(|year| year.parse::<i32>().ok())),
        is_subscribed: value_bool(&snapshot.values, "is_subscribed").unwrap_or(false),
        interaction_heat: interaction_heat(&snapshot.values),
        has_subscribed_actress: snapshot.actors.iter().any(|actor| {
            // 上游 `_has_subscribed_actress`：`gender == 1` 且订阅了的女演员。
            value_number(&actor.values, "gender") == Some(1.0)
                && value_bool(&actor.values, "is_subscribed").unwrap_or(false)
        }),
    }
}

/// 互动热度（上游 `_interaction_heat`）：四个键的和，缺键当 0。
///
/// ⚠️ 宿主的 `movie_values` 目前只给 `score_number`；`watched_count` /
/// `want_watch_count` / `comment_count` 还不在快照里（宿主侧没有这三列），
/// 所以现在热量实际等于 `score_number`。缺键当 0 与上游 `int(... or 0)` 同义，
/// 补上宿主字段后这一项自动变准。
fn interaction_heat(values: &HashMap<String, prost_types::Value>) -> i64 {
    ["watched_count", "want_watch_count", "comment_count", "score_number"]
        .iter()
        .map(|key| value_number(values, key).unwrap_or(0.0) as i64)
        .sum()
}

/// 取快照里的字符串（键不在、类型不对都算没有）。
fn value_string(values: &HashMap<String, prost_types::Value>, key: &str) -> Option<String> {
    match values.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::StringValue(text) => Some(text.clone()),
        _ => None,
    }
}

/// 取快照里的布尔。
fn value_bool(values: &HashMap<String, prost_types::Value>, key: &str) -> Option<bool> {
    match values.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::BoolValue(flag) => Some(*flag),
        _ => None,
    }
}

/// 取快照里的数字。
fn value_number(values: &HashMap<String, prost_types::Value>, key: &str) -> Option<f64> {
    match values.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::NumberValue(number) => Some(*number),
        _ => None,
    }
}

/// 造一个字符串 `Value`（写回用）。
fn string_value(text: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(text.to_owned())),
    }
}

/// 把进度事件推进 mpsc（[`Progress`] 的通道实现）。
struct ChannelProgress {
    tx: mpsc::Sender<Result<JobEvent, Status>>,
}

impl ChannelProgress {
    fn new(tx: mpsc::Sender<Result<JobEvent, Status>>) -> Self {
        Self { tx }
    }
}

impl Progress for ChannelProgress {
    fn emit(&mut self, current: usize, total: usize, text: &str) {
        let event = JobEvent {
            event: Some(JobEventKind::Progress(ProgressEvent {
                current: current as i32,
                total: total as i32,
                text: text.to_owned(),
            })),
        };
        // 接收端走了就不用再报了；blocking_send 会卡住任务，用 try_send。
        let _ = self.tx.try_send(Ok(event));
    }
}

/// 把任务失败翻成 gRPC 状态。
fn status_of(err: &PipelineError) -> Status {
    match err {
        // 「已有任务在跑」与「翻译没启用」都是调用方能修的前置条件。
        PipelineError::Busy | PipelineError::TranslationDisabled => {
            Status::failed_precondition(err.to_string())
        }
        // 存在失败项目：任务跑了，但有片子没走完 —— 是个结果，不是崩溃。
        PipelineError::Failed(_) => Status::internal(err.to_string()),
        PipelineError::Host(_) => Status::unavailable(err.to_string()),
        // DMM / 翻译服务都在外面，连不上是常态。
        PipelineError::Dmm(_) | PipelineError::Translation(_) => {
            Status::unavailable(err.to_string())
        }
        PipelineError::State(_) | PipelineError::Io(_) => Status::internal(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(task_key: &str, params: Option<serde_json::Value>, data_dir: &str) -> RunJobRequest {
        RunJobRequest {
            run_id: "run-1".to_owned(),
            task_key: task_key.to_owned(),
            params: params.map(|value| {
                sm_plugin_api::json_struct::json_to_struct(&value).expect("参数必是对象")
            }),
            data_dir: data_dir.to_owned(),
        }
    }

    #[test]
    fn three_jobs_are_declared() {
        let jobs = job_definitions();
        assert_eq!(jobs.len(), 3);
        let keys: Vec<&str> = jobs.iter().map(|j| j.task_key.as_str()).collect();
        assert_eq!(
            keys,
            vec![TASK_SYNC, TASK_SYNC_SUBSCRIBED, TASK_TRANSLATE_CACHED]
        );
    }

    #[test]
    fn the_daily_job_has_a_cron_and_the_others_are_manual_only() {
        let jobs = job_definitions();
        let daily = jobs
            .iter()
            .find(|j| j.task_key == TASK_SYNC_SUBSCRIBED)
            .unwrap();
        assert_eq!(daily.default_cron, "10 4 * * *");
        assert!(!daily.manual_only);
        for j in jobs.iter().filter(|j| j.task_key != TASK_SYNC_SUBSCRIBED) {
            assert!(j.manual_only, "{}", j.task_key);
        }
    }

    #[test]
    fn the_manual_job_requires_a_movie_number() {
        let jobs = job_definitions();
        let manual = jobs.iter().find(|j| j.task_key == TASK_SYNC).unwrap();
        let schema = manual
            .params_schema
            .as_ref()
            .expect("手动任务要有参数 schema");
        let required = schema.fields.get("required").expect("要有 required");
        let list = match required.kind.as_ref() {
            Some(prost_types::value::Kind::ListValue(l)) => l,
            _ => panic!("required 不是 list"),
        };
        assert_eq!(list.values.len(), 1);
    }

    #[test]
    fn a_missing_or_wrong_typed_movie_number_is_rejected() {
        assert!(extract_movie_number(&request(TASK_SYNC, None, "/tmp")).is_err());
        assert!(extract_movie_number(&request(
            TASK_SYNC,
            Some(serde_json::json!({"movie_number": 7})),
            "/tmp"
        ))
        .is_err());
        assert!(extract_movie_number(&request(
            TASK_SYNC,
            Some(serde_json::json!({"movie_number": "   "})),
            "/tmp"
        ))
        .is_err());
        let number = extract_movie_number(&request(
            TASK_SYNC,
            Some(serde_json::json!({"movie_number": " SSNI-888 "})),
            "/tmp"
        ))
        .expect("该收下");
        assert_eq!(number, "SSNI-888", "两头的空白要剥掉");
    }

    #[tokio::test]
    async fn an_unknown_task_key_is_an_invalid_argument() {
        let control = Control::new(crate::PLUGIN_ID.to_owned(), Settings::default());
        let Err(err) = control
            .run_job(Request::new(request("nope", None, "/tmp")))
            .await
        else {
            panic!("未知 task_key 该被拒");
        };
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn bad_params_fail_before_any_stream_is_created() {
        let control = Control::new(crate::PLUGIN_ID.to_owned(), Settings::default());
        let Err(err) = control
            .run_job(Request::new(request(TASK_SYNC, None, "/tmp")))
            .await
        else {
            panic!("缺参数该被拒");
        };
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn translate_only_needs_translation_enabled_before_any_stream() {
        let control = Control::new(crate::PLUGIN_ID.to_owned(), Settings::default());
        let Err(err) = control
            .run_job(Request::new(request(TASK_TRANSLATE_CACHED, None, "/tmp")))
            .await
        else {
            panic!("翻译没启用该被拒");
        };
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn a_busy_pipeline_or_a_full_scan_is_reported_as_a_precondition() {
        assert_eq!(
            status_of(&PipelineError::Busy).code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            status_of(&PipelineError::Host("连不上".to_owned())).code(),
            tonic::Code::Unavailable
        );
    }

    #[test]
    fn a_snapshot_maps_to_a_movie_ref() {
        use prost_types::value::Kind;
        use prost_types::Value;

        let snapshot = MovieSnapshot {
            movie_id: 7,
            revision: 3,
            values: HashMap::from([
                (
                    "movie_number".to_owned(),
                    Value {
                        kind: Some(Kind::StringValue("SSNI-888".to_owned())),
                    },
                ),
                (
                    "title".to_owned(),
                    Value {
                        kind: Some(Kind::StringValue("标题".to_owned())),
                    },
                ),
                (
                    "release_date".to_owned(),
                    Value {
                        kind: Some(Kind::StringValue("2023-05-06".to_owned())),
                    },
                ),
                (
                    "is_subscribed".to_owned(),
                    Value {
                        kind: Some(Kind::BoolValue(true)),
                    },
                ),
                (
                    "score_number".to_owned(),
                    Value {
                        kind: Some(Kind::NumberValue(4.0)),
                    },
                ),
            ]),
            owners: vec!["host:manual".to_owned()],
            // 字段级归属：只有 title 有主。
            field_owners: HashMap::from([("title".to_owned(), "host:manual".to_owned())]),
            actors: Vec::new(),
            tags: Vec::new(),
        };
        let movie = movie_ref(snapshot);
        assert_eq!(movie.movie_id, 7);
        assert_eq!(movie.revision, 3);
        assert_eq!(movie.movie_number, "SSNI-888");
        assert_eq!(movie.title, "标题");
        assert_eq!(movie.summary, "", "缺失的字段读成空串");
        assert_eq!(movie.release_year, Some(2023));
        assert!(movie.is_subscribed);
        assert_eq!(movie.interaction_heat, 4);
        assert_eq!(
            movie.owners.get("title").and_then(|o| o.as_deref()),
            Some("host:manual"),
            "有主的字段要带过来"
        );
        assert!(
            !movie.owners.contains_key("summary"),
            "无主的字段不进 owners —— 那正是「可写」"
        );
    }

    #[test]
    fn a_release_date_that_is_not_a_year_is_ignored() {
        use prost_types::value::Kind;
        use prost_types::Value;

        let snapshot = MovieSnapshot {
            movie_id: 1,
            revision: 1,
            values: HashMap::from([(
                "release_date".to_owned(),
                Value {
                    kind: Some(Kind::StringValue("待定".to_owned())),
                },
            )]),
            owners: Vec::new(),
            field_owners: HashMap::new(),
            actors: Vec::new(),
            tags: Vec::new(),
        };
        assert_eq!(movie_ref(snapshot).release_year, None);
    }

    #[test]
    fn a_subscribed_female_actor_raises_the_priority_tier() {
        use prost_types::value::Kind;
        use prost_types::Value;
        use sm_plugin_api::v1::ActorSnapshot;

        let actor = |gender: f64, subscribed: bool| ActorSnapshot {
            actor_id: 1,
            revision: 1,
            values: HashMap::from([
                (
                    "gender".to_owned(),
                    Value {
                        kind: Some(Kind::NumberValue(gender)),
                    },
                ),
                (
                    "is_subscribed".to_owned(),
                    Value {
                        kind: Some(Kind::BoolValue(subscribed)),
                    },
                ),
            ]),
            owners: Vec::new(),
            field_owners: HashMap::new(),
        };
        let snapshot_with = |actors: Vec<ActorSnapshot>| MovieSnapshot {
            movie_id: 1,
            revision: 1,
            values: HashMap::new(),
            owners: Vec::new(),
            field_owners: HashMap::new(),
            actors,
            tags: Vec::new(),
        };

        assert!(movie_ref(snapshot_with(vec![actor(1.0, true)])).has_subscribed_actress);
        // 男演员（gender != 1）不算。
        assert!(!movie_ref(snapshot_with(vec![actor(0.0, true)])).has_subscribed_actress);
        // 没订阅的不算。
        assert!(!movie_ref(snapshot_with(vec![actor(1.0, false)])).has_subscribed_actress);
        assert!(!movie_ref(snapshot_with(vec![])).has_subscribed_actress);
    }
}
