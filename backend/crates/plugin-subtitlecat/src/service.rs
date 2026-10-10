//! gRPC 服务面：`PluginControl`（注册 + 两个任务）。
//!
//! # 上游对应
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `plugin.py:register` | [`Control::register`]：声明两个 job |
//! | `jobs.py:build_jobs` | [`Control::run_job`]：按 `task_key` 分发 |
//! | `context.movies` / `context.import_subtitle` | [`GrpcHost`]（[`jobs::SubtitleHost`] 的生产实现） |
//! | `context.data_dir` | `RunJobRequest.data_dir`（宿主给的插件数据目录） |
//!
//! # 两个任务
//!
//! | task_key | 上游 | 说明 |
//! |---|---|---|
//! | `sakuramedia_subtitlecat_fetch` | `run_fetch` | 手动抓取单部影片 |
//! | `sakuramedia_subtitlecat_fetch_subscribed` | `run_subscribed` | 定时抓取已订阅影片 |
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control`] 的
//! `register` 只回声明；HTTP 客户端、宿主连接、状态文件都是**每次 `RunJob`
//! 时**才建的（状态文件落在 `data_dir` 里，`data_dir` 也由那次请求给）。
//!
//! # 参数校验在建流之前
//!
//! `run_job` 先校验 `task_key` 与参数，再开流。这样「参数写错了」以带 code 的
//! `Status` 返回给调用方，而不是变成流里的一条错误事件 —— 后者宿主必须先把
//! 流读到那一行才知道，日志里也只剩一句「任务失败」。与
//! `plugin-javdb-ranking/src/service.rs` 同一纪律。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use sm_plugin_api::v1::job_event::Event as JobEventKind;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::{
    Extension, FindByNumbersRequest, ImportSubtitleRequest, JobDefinition, JobEvent,
    ListMoviesRequest, MovieSnapshot, ProgressEvent, RegisterRequest, RegisterResponse,
    RunJobRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use crate::jobs::{
    self, ErrorCode, HostMovie, ImportStatus, JobError, ProgressReporter, SubtitleHost,
};
use crate::settings::{self, Settings};
use crate::state::FetchState;
use crate::subtitlecat::SubtitleCatClient;

/// 手动抓取单部影片的 task_key（上游 `jobs.py` 的 `task_key`）。
pub const TASK_FETCH: &str = "sakuramedia_subtitlecat_fetch";
/// 定时抓取已订阅影片的 task_key。
pub const TASK_FETCH_SUBSCRIBED: &str = "sakuramedia_subtitlecat_fetch_subscribed";

/// 宿主能力出口的端点（`sm-plugins` 的 `supervisor::HOST_ADDR_ENV`）。
///
/// **进程式**由可执行文件从这里取；**进程内**由组合根显式传进
/// [`Control::with_runtime`] —— 进程内多插件共用一份进程环境，从那里读会互相
/// 覆盖。公开它是为了让两处用同一个常量。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

/// 连一次宿主的超时上限（宿主就在本机，正常是毫秒级；给上限是为了让「宿主没
/// 起来」表现成一次可读的失败，而不是任务永远挂着）。
const HOST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 抓取状态文件的名字（放在宿主给的 `data_dir` 里）。
const STATE_FILE_NAME: &str = "fetch_state.sqlite3";

/// 控制面。
pub struct Control {
    plugin_id: String,
    /// 插件配置。**构造时定死**，任务里不再读进程环境 —— 进程内多插件共用
    /// 一份进程环境，从那里读 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 会互相覆盖。
    settings: Settings,
    /// 宿主能力出口端点。`None` = 宿主没暴露 `PluginHost`，任务直接失败。
    host_endpoint: Option<String>,
}

/// 一次 `RunJob` 要跑哪个任务（参数已校验好）。
enum TaskRequest {
    Fetch(String),
    FetchSubscribed,
}

impl Control {
    /// 缺省配置、无宿主端点。单测与「只想注册一下」的场合用。
    pub fn new(plugin_id: String) -> Self {
        Self {
            plugin_id,
            settings: Settings::default(),
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

    /// 构造两个任务的声明（`RegisterResponse.jobs`）。
    fn job_definitions() -> Vec<JobDefinition> {
        vec![
            JobDefinition {
                task_key: TASK_FETCH.to_owned(),
                log_name: "subtitlecat-fetch".to_owned(),
                cli_name: "fetch-subtitlecat".to_owned(),
                cli_help: "手动抓取单部影片的中文字幕".to_owned(),
                // 上游 `manual_only=True`。
                manual_only: true,
                // 上游 `default_cron` 没给这个任务。
                default_cron: String::new(),
                params_schema: Some(fetch_params_schema()),
                required_capabilities: Vec::new(),
            },
            JobDefinition {
                task_key: TASK_FETCH_SUBSCRIBED.to_owned(),
                log_name: "subtitlecat-subscribed-fetch".to_owned(),
                cli_name: "fetch-subscribed-subtitlecat".to_owned(),
                cli_help: "定时抓取所有已订阅影片的中文字幕".to_owned(),
                manual_only: false,
                // 上游 `default_cron="0 3 * * *"`。
                default_cron: "0 3 * * *".to_owned(),
                params_schema: None,
                required_capabilities: Vec::new(),
            },
        ]
    }
}

/// `FetchSubtitleParams` 的 schema（上游 pydantic 模型的手写投影）。
///
/// 宿主拿它渲染手动触发的参数表单 / 校验请求体 —— 字段与
/// [`extract_movie_number`] 认的必须一致，所以两边都从这里取。
fn fetch_params_schema() -> prost_types::Struct {
    use prost_types::value::Kind;
    use prost_types::{Struct, Value};

    let movie_number = Value {
        kind: Some(Kind::StructValue(Struct {
            fields: std::collections::BTreeMap::from([
                (
                    "type".to_owned(),
                    Value {
                        kind: Some(Kind::StringValue("string".to_owned())),
                    },
                ),
                // 上游 `min_length=1, max_length=64`。
                (
                    "minLength".to_owned(),
                    Value {
                        kind: Some(Kind::NumberValue(1.0)),
                    },
                ),
                (
                    "maxLength".to_owned(),
                    Value {
                        kind: Some(Kind::NumberValue(64.0)),
                    },
                ),
            ]),
        })),
    };
    Struct {
        fields: std::collections::BTreeMap::from([("movie_number".to_owned(), movie_number)]),
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
        // 先校验：校验不过就是调用错误，不该先给一条流再在流里报错。
        let task = match inner.task_key.as_str() {
            TASK_FETCH => TaskRequest::Fetch(extract_movie_number(&inner)?),
            TASK_FETCH_SUBSCRIBED => TaskRequest::FetchSubscribed,
            other => return Err(Status::invalid_argument(format!("未知 task_key: {other}"))),
        };

        let settings = self.settings.clone();
        let host_endpoint = self.host_endpoint.clone();
        let data_dir = inner.data_dir.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);
        // 任务在后台跑，流只负责把事件搬出去 —— 宿主断开流即取消。
        tokio::spawn(async move {
            let mut reporter = ChannelReporter::new(tx.clone());
            let outcome = run_job_once(
                &task,
                &data_dir,
                &settings,
                host_endpoint.as_deref(),
                &mut reporter,
            )
            .await;
            match outcome {
                Ok(result) => {
                    let event = JobEvent {
                        event: Some(JobEventKind::Result(result)),
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
            // 字幕插件不声明扩展点能力，只跑任务（jobs 不需要 capability）。
            capabilities: Vec::new(),
            extensions: Vec::<Extension>::new(),
            jobs: Self::job_definitions(),
            settings_schema: settings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 跑一次任务并给出终态 `Struct`（上游两个 handler 的公共外壳）。
async fn run_job_once(
    task: &TaskRequest,
    data_dir: &str,
    settings: &Settings,
    host_endpoint: Option<&str>,
    reporter: &mut ChannelReporter,
) -> Result<prost_types::Struct, JobError> {
    let client = SubtitleCatClient::new(settings)?;
    let state = FetchState::open(&PathBuf::from(data_dir).join(STATE_FILE_NAME))?;
    let mut host = host_client(host_endpoint).await?;
    let now = jobs::utc_now();

    match task {
        TaskRequest::Fetch(movie_number) => {
            let stats =
                jobs::run_fetch(&mut host, &client, &state, movie_number, reporter, now).await?;
            Ok(stats.to_struct())
        }
        TaskRequest::FetchSubscribed => {
            let stats =
                jobs::run_subscribed(&mut host, &client, &state, settings, reporter, now).await?;
            Ok(stats.to_struct())
        }
    }
}

/// 从 `RunJobRequest` 里取 `movie_number` 参数。
fn extract_movie_number(request: &RunJobRequest) -> Result<String, Status> {
    // 参数以 JSON Struct 形式传（与上游 `FetchSubtitleParams` 对齐）。
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
    // 上游 `FetchSubtitleParams._normalize_number`：先归一化，再看空不空。
    let normalized = crate::subtitlecat::normalize_movie_number(&raw);
    if normalized.is_empty() {
        return Err(Status::invalid_argument("movie_number 不能为空"));
    }
    Ok(normalized)
}

/// 连一次宿主，包成 [`SubtitleHost`]。
async fn host_client(host_endpoint: Option<&str>) -> Result<GrpcHost, JobError> {
    let host_addr = host_endpoint.ok_or_else(|| {
        JobError::host(format!(
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
            .map_err(|e| JobError::host(format!("host:bad_addr:{e}")))?
            .connect()
            .await
            .map_err(|e| JobError::host(format!("host:connect:{e}")))
    })
    .await
    .map_err(|_| JobError::host("host:connect:timeout"))??;
    Ok(GrpcHost {
        client: PluginHostClient::new(channel),
    })
}

/// [`SubtitleHost`] 的生产实现：把 trait 的四个方法翻成 `PluginHost` 的调用。
struct GrpcHost {
    client: PluginHostClient<Channel>,
}

#[async_trait]
impl SubtitleHost for GrpcHost {
    async fn find_movies_by_numbers(
        &mut self,
        movie_numbers: &[String],
    ) -> Result<Vec<HostMovie>, JobError> {
        let response = self
            .client
            .find_movies_by_numbers(FindByNumbersRequest {
                movie_numbers: movie_numbers.to_vec(),
            })
            .await
            .map_err(|e| JobError::host(format!("host:find_movies_by_numbers:{e}")))?
            .into_inner();
        Ok(response.movies.into_iter().map(host_movie).collect())
    }

    async fn list_movies(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<(Vec<HostMovie>, Option<i64>), JobError> {
        let response = self
            .client
            .list_movies(ListMoviesRequest {
                after_id,
                limit,
                // 上游按 `is_subscribed` 在**插件侧**筛；宿主的 `filters` 还没映射
                // （传了会直接 `unimplemented`），所以这里也不传。
                filters: None,
            })
            .await
            .map_err(|e| JobError::host(format!("host:list_movies:{e}")))?
            .into_inner();
        Ok((
            response.movies.into_iter().map(host_movie).collect(),
            response.next_cursor,
        ))
    }

    async fn import_subtitle(
        &mut self,
        movie_number: &str,
        content: &[u8],
        file_name: &str,
        language: &str,
    ) -> Result<ImportStatus, JobError> {
        let response = self
            .client
            .import_subtitle(ImportSubtitleRequest {
                movie_number: movie_number.to_owned(),
                content: content.to_vec(),
                file_name: file_name.to_owned(),
                language: Some(language.to_owned()),
            })
            .await
            .map_err(|e| JobError::host(format!("host:import_subtitle:{e}")))?
            .into_inner();
        Ok(ImportStatus::parse(&response.status))
    }
}

/// 影片快照 → 本插件要的那几项。
fn host_movie(snapshot: MovieSnapshot) -> HostMovie {
    HostMovie {
        movie_number: value_string(&snapshot.values, "movie_number").unwrap_or_default(),
        is_subscribed: value_bool(&snapshot.values, "is_subscribed").unwrap_or(false),
        release_date: value_string(&snapshot.values, "release_date"),
    }
}

/// 取快照里的字符串（键不在、类型不对都算没有）。
fn value_string(values: &HashMap<String, prost_types::Value>, key: &str) -> Option<String> {
    match values.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::StringValue(text) => Some(text.clone()),
        _ => None,
    }
}

/// 取快照里的布尔（同上）。
fn value_bool(values: &HashMap<String, prost_types::Value>, key: &str) -> Option<bool> {
    match values.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::BoolValue(flag) => Some(*flag),
        _ => None,
    }
}

/// 把进度事件推进 mpsc（[`ProgressReporter`] 的通道实现）。
struct ChannelReporter {
    tx: mpsc::Sender<Result<JobEvent, Status>>,
}

impl ChannelReporter {
    fn new(tx: mpsc::Sender<Result<JobEvent, Status>>) -> Self {
        Self { tx }
    }
}

impl ProgressReporter for ChannelReporter {
    fn report(&mut self, current: i64, total: i64, text: String) {
        let event = JobEvent {
            event: Some(JobEventKind::Progress(ProgressEvent {
                current: current as i32,
                total: total as i32,
                text,
            })),
        };
        // 接收端走了就不用再报了；blocking_send 会卡住任务，用 try_send。
        let _ = self.tx.try_send(Ok(event));
    }
}

/// 把任务失败翻成 gRPC 状态（类别由 [`JobError::code`] 带过来）。
fn status_of(err: &JobError) -> Status {
    let code = match err.code {
        ErrorCode::InvalidArgument => tonic::Code::InvalidArgument,
        ErrorCode::Unavailable => tonic::Code::Unavailable,
        ErrorCode::Internal => tonic::Code::Internal,
    };
    Status::new(code, err.message.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitlecat::SubtitleCatError;

    fn request(task_key: &str, params: Option<serde_json::Value>) -> RunJobRequest {
        RunJobRequest {
            run_id: "run-1".to_owned(),
            task_key: task_key.to_owned(),
            params: params.map(|value| {
                sm_plugin_api::json_struct::json_to_struct(&value).expect("参数必是对象")
            }),
            data_dir: "/tmp/plugin".to_owned(),
        }
    }

    #[test]
    fn the_two_task_keys_are_distinct() {
        assert_ne!(TASK_FETCH, TASK_FETCH_SUBSCRIBED);
    }

    #[test]
    fn the_params_schema_lists_what_we_read() {
        let schema = fetch_params_schema();
        assert!(schema.fields.contains_key("movie_number"));
    }

    #[test]
    fn a_missing_or_wrong_typed_movie_number_is_rejected() {
        // 完全没有 params。
        assert!(extract_movie_number(&request(TASK_FETCH, None)).is_err());
        // 有 params，但没有 movie_number。
        assert!(extract_movie_number(&request(
            TASK_FETCH,
            Some(serde_json::json!({"other": "x"}))
        ))
        .is_err());
        // 类型不对（早期实现会静默忽略，等于「参数没生效但任务照跑」）。
        assert!(extract_movie_number(&request(
            TASK_FETCH,
            Some(serde_json::json!({"movie_number": 888}))
        ))
        .is_err());
        // 归一化后为空。
        assert!(extract_movie_number(&request(
            TASK_FETCH,
            Some(serde_json::json!({"movie_number": "  "}))
        ))
        .is_err());
    }

    #[test]
    fn a_movie_number_is_normalized_before_use() {
        let number = extract_movie_number(&request(
            TASK_FETCH,
            Some(serde_json::json!({"movie_number": "ssni 888"})),
        ))
        .expect("该收下");
        assert_eq!(number, "SSNI-888");
    }

    #[tokio::test]
    async fn an_unknown_task_key_is_an_invalid_argument() {
        let control = Control::new(crate::PLUGIN_ID.to_owned());
        // 不用 `expect_err`：成功那一支是 `Response<BoxStream>`，不为 `Debug`。
        let Err(err) = control.run_job(Request::new(request("nope", None))).await else {
            panic!("未知 task_key 该被拒");
        };
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn bad_params_fail_before_any_stream_is_created() {
        let control = Control::new(crate::PLUGIN_ID.to_owned());
        let Err(err) = control
            .run_job(Request::new(request(TASK_FETCH, None)))
            .await
        else {
            panic!("缺参数该被拒");
        };
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn the_subscribed_task_declares_no_params() {
        // 订阅任务没有参数：给了 params 也照样进（上游 `_params` 直接忽略）。
        let control = Control::new(crate::PLUGIN_ID.to_owned());
        let stream = control
            .run_job(Request::new(request(TASK_FETCH_SUBSCRIBED, None)))
            .await;
        assert!(stream.is_ok(), "订阅任务该能起流");
    }

    #[test]
    fn a_client_error_is_invalid_argument() {
        let status = status_of(&JobError::from(SubtitleCatError::ClientError(
            "HTTP 404".to_owned(),
        )));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_request_error_is_unavailable() {
        let status = status_of(&JobError::from(SubtitleCatError::Request(
            "timeout".to_owned(),
        )));
        assert_eq!(status.code(), tonic::Code::Unavailable);
    }

    #[test]
    fn a_missing_host_endpoint_is_unavailable() {
        let err = JobError::host("宿主没有暴露 PluginHost");
        assert_eq!(status_of(&err).code(), tonic::Code::Unavailable);
    }

    #[test]
    fn host_movies_read_the_three_fields_we_care_about() {
        use prost_types::value::Kind;
        use prost_types::Value;

        let snapshot = MovieSnapshot {
            movie_id: 7,
            revision: 1,
            values: HashMap::from([
                (
                    "movie_number".to_owned(),
                    Value {
                        kind: Some(Kind::StringValue("SSNI-888".to_owned())),
                    },
                ),
                (
                    "is_subscribed".to_owned(),
                    Value {
                        kind: Some(Kind::BoolValue(true)),
                    },
                ),
                (
                    "release_date".to_owned(),
                    Value {
                        kind: Some(Kind::StringValue("2026-01-02".to_owned())),
                    },
                ),
            ]),
            owners: Vec::new(),
            field_owners: HashMap::new(),
            actors: Vec::new(),
            tags: Vec::new(),
        };
        let movie = host_movie(snapshot);
        assert_eq!(movie.movie_number, "SSNI-888");
        assert!(movie.is_subscribed);
        assert_eq!(movie.release_date.as_deref(), Some("2026-01-02"));
    }

    #[test]
    fn missing_values_read_as_absent() {
        let snapshot = MovieSnapshot {
            movie_id: 7,
            revision: 1,
            values: HashMap::new(),
            owners: Vec::new(),
            field_owners: HashMap::new(),
            actors: Vec::new(),
            tags: Vec::new(),
        };
        let movie = host_movie(snapshot);
        assert_eq!(movie.movie_number, "");
        assert!(!movie.is_subscribed);
        assert_eq!(movie.release_date, None);
    }
}
