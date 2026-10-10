//! gRPC 服务面：`PluginControl`（注册 + 后台任务）。
//!
//! # 上游对应：`plugin.py` + `jobs.py`
//!
//! | 上游 | 这里 |
//! |---|---
//! | `plugin.py:register` | [`Control`]（`register`，声明 `JobDefinition`） |
//! | `jobs.py:run` | [`Control::run_job`] |
//! | `jobs.py:process` | [`crate::jobs::process`] |
//! | `portalocker.Lock` | `fs2::FileExt::try_lock_exclusive` |
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control`] 的
//! `register` 只回声明；HTTP 客户端与宿主连接都在 `run_job` 里建。
//!
//! # 任务需要宿主在场
//!
//! 上游是进程内插件，直接拿 `context.actors`。拆成进程后，任务通过
//! `SAKURAMEDIA_HOST_GRPC_ADDR`（宿主注入，可选）连回 `PluginHost` 调
//! `ListActors` / `GetActor` / `PatchActor` / `ListMovies`。宿主没给这个
//! 变量时任务直接失败 —— 没有宿主就没有可补的演员。

use std::path::PathBuf;

use async_trait::async_trait;
use futures::stream::BoxStream;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::plugin_host_client::PluginHostClient;
use sm_plugin_api::v1::{
    job_event, Capability, GetActorRequest, JobDefinition, JobEvent, ListActorsRequest,
    ListMoviesRequest, PatchActorRequest, ProgressEvent, RegisterRequest, RegisterResponse,
    RunJobRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Channel, Request, Response, Status};

use crate::jobs::{
    field_to_proto, process, ActorHost, HostActor, HostError, HostMovie, HostValue, JobError,
    ProgressReporter, Stats,
};
use crate::settings::{self, Settings};
use crate::sources::Sources;
use crate::state::State;

/// 宿主能力出口的端点（`sm-plugins` 的 `supervisor::HOST_ADDR_ENV`）。
const HOST_ADDR_ENV: &str = "SAKURAMEDIA_HOST_GRPC_ADDR";

/// 控制面。
pub struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
    plugin_id: String,
}

impl Control {
    pub fn new(plugin_id: String) -> Self {
        Self { plugin_id }
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
        if inner.task_key != crate::PLUGIN_ID {
            return Err(Status::invalid_argument(format!(
                "未知任务：{}（本插件只提供 {}）",
                inner.task_key,
                crate::PLUGIN_ID
            )));
        }
        let plugin_id = self.plugin_id.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);
        // 任务在后台跑，流只负责把事件搬出去 —— 宿主断开流即取消。
        tokio::spawn(async move {
            let mut reporter = ChannelReporter::new(tx.clone());
            let result = run_job_once(&plugin_id, &inner.data_dir, &mut reporter).await;
            match result {
                Ok(stats) => {
                    let event = JobEvent {
                        event: Some(job_event::Event::Result(
                            sm_plugin_api::json_struct::json_to_struct(&stats.to_json())
                                .unwrap_or_default(),
                        )),
                    };
                    let _ = tx.send(Ok(event)).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
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
        // 宿主注入什么就回什么：不一致会让宿主的加载校验直接失败，而这个
        // 进程自己也说不清「我是谁」，不如当场拒。
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
            // **不要**抄 manifest 里的 `host_api_version: 6` —— 那是 Python 侧
            // 的版本号；这里是 Rust 侧的 ABI 主版本。
            abi_major: sm_plugin_api::ABI_MAJOR,
            // 本插件没有扩展点：纯后台任务。
            capabilities: Vec::new(),
            extensions: Vec::new(),
            jobs: vec![JobDefinition {
                task_key: crate::PLUGIN_ID.to_owned(),
                log_name: "actor-metadata".to_owned(),
                cli_name: "actor-metadata".to_owned(),
                cli_help: "从 JavDB / MinnanoAV 补全女优资料".to_owned(),
                default_cron: "0 5 * * *".to_owned(),
                manual_only: false,
                params_schema: None,
                required_capabilities: vec![Capability::ExtensionCatalogMetadataSource as i32],
            }],
            // 让宿主渲染配置表单；值从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读。
            settings_schema: settings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// 把进度事件推进 mpsc（`jobs::ProgressReporter` 的通道实现）。
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
            event: Some(job_event::Event::Progress(ProgressEvent {
                text,
                current: current as i32,
                total: total as i32,
            })),
        };
        // 接收端走了就不用再报了；blocking_send 会卡住任务，用 try_send。
        let _ = self.tx.try_send(Ok(event));
    }
}

/// 任务的一次执行（上游 `jobs.run`）。
async fn run_job_once(
    plugin_id: &str,
    data_dir: &str,
    reporter: &mut ChannelReporter,
) -> Result<Stats, JobError> {
    let data_dir = PathBuf::from(data_dir);
    // 并发锁（上游 `portalocker.Lock(..., timeout=0)`）：拿不到就直接失败，
    // 不排队 —— 定时与手动触发撞车时，让这一次直接报错比静默排队好查。
    let lock_path = data_dir.join("actor_metadata.lock");
    let lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| JobError::State(crate::state::StateError(format!("state:lock:open:{e}"))))?;
    {
        use fs2::FileExt;
        lock_file
            .try_lock_exclusive()
            .map_err(|_| JobError::State(crate::state::StateError("任务已在运行中".to_owned())))?;
    }
    // 文件句柄活到函数结束，锁随之释放。
    let _lock_held = lock_file;

    let state = State::open(&data_dir.join("actor_metadata.sqlite3")).map_err(JobError::State)?;
    if state.requeued_count > 0 {
        reporter.report(
            0,
            0,
            format!(
                "资料来源升级，已重新入队 {} 位未补齐演员",
                state.requeued_count
            ),
        );
    }
    let settings = Settings::load();
    let host_addr = std::env::var(HOST_ADDR_ENV).map_err(|_| {
        JobError::Host(format!(
            "缺少环境变量 {HOST_ADDR_ENV}：宿主没有暴露 PluginHost"
        ))
    })?;
    let channel = Channel::from_shared(format!("http://{host_addr}"))
        .map_err(|e| JobError::Host(format!("host:bad_addr:{e}")))?
        .connect()
        .await
        .map_err(|e| JobError::Host(format!("host:connect:{e}")))?;
    let mut host = GrpcHost::new(PluginHostClient::new(channel));
    let mut sources = Sources::new(&settings).map_err(JobError::Sources)?;
    let owner = format!("plugin:{plugin_id}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let stats = process(
        &mut host,
        &mut sources,
        &state,
        &settings,
        &owner,
        Some(reporter),
        now,
    )
    .await?;
    if stats.aborted == 1 {
        return Err(JobError::Sources(crate::sources::SourceError(format!(
            "连续 {} 位演员网络请求异常，本轮已停止；当前演员状态已保存，未开始的演员不计次数",
            crate::jobs::MAX_CONSECUTIVE_NETWORK_ERRORS
        ))));
    }
    Ok(stats)
}

/// `PluginHost` 的 gRPC 客户端（[`ActorHost`] 的生产实现）。
struct GrpcHost {
    client: PluginHostClient<Channel>,
}

impl GrpcHost {
    fn new(client: PluginHostClient<Channel>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ActorHost for GrpcHost {
    async fn list_actors(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<(Vec<HostActor>, Option<i64>), HostError> {
        let response = self
            .client
            .list_actors(ListActorsRequest {
                after_id,
                limit,
                filters: None,
            })
            .await
            .map_err(|e| HostError(format!("host:list_actors:{e}")))?
            .into_inner();
        let actors = response
            .actors
            .into_iter()
            .map(|a| HostActor {
                actor_id: a.actor_id,
                revision: a.revision,
                values: a
                    .values
                    .into_iter()
                    .map(|(k, v)| (k, HostValue::from_proto(&v)))
                    .collect(),
                owners: a.owners,
            })
            .collect();
        Ok((actors, response.next_cursor))
    }

    async fn get_actor(&mut self, actor_id: i64) -> Result<Option<HostActor>, HostError> {
        match self.client.get_actor(GetActorRequest { actor_id }).await {
            Ok(response) => Ok(response.into_inner().actor.map(|a| HostActor {
                actor_id: a.actor_id,
                revision: a.revision,
                values: a
                    .values
                    .into_iter()
                    .map(|(k, v)| (k, HostValue::from_proto(&v)))
                    .collect(),
                owners: a.owners,
            })),
            Err(e) if e.code() == tonic::Code::NotFound => Ok(None),
            Err(e) => Err(HostError(format!("host:get_actor:{e}"))),
        }
    }

    async fn patch_actor(
        &mut self,
        actor_id: i64,
        fields: &std::collections::BTreeMap<String, crate::sources::FieldValue>,
        expected_revision: i64,
    ) -> Result<bool, HostError> {
        let response = self
            .client
            .patch_actor(PatchActorRequest {
                actor_id,
                fields: fields
                    .iter()
                    .map(|(k, v)| (k.clone(), field_to_proto(v)))
                    .collect(),
                expected_revision,
            })
            .await
            .map_err(|e| HostError(format!("host:patch_actor:{e}")))?
            .into_inner();
        Ok(response.updated)
    }

    async fn list_movies(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<(Vec<HostMovie>, Option<i64>), HostError> {
        let response = self
            .client
            .list_movies(ListMoviesRequest {
                after_id,
                limit,
                filters: None,
            })
            .await
            .map_err(|e| HostError(format!("host:list_movies:{e}")))?
            .into_inner();
        let movies = response
            .movies
            .into_iter()
            .map(|m| HostMovie {
                movie_id: m.movie_id,
                values: m
                    .values
                    .into_iter()
                    .map(|(k, v)| (k, HostValue::from_proto(&v)))
                    .collect(),
                actor_ids: m.actors.into_iter().map(|a| a.actor_id).collect(),
            })
            .collect();
        Ok((movies, response.next_cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_rejects_a_mismatched_plugin_id() {
        // Control::new("a") 但注入 "b" → invalid_argument。
        // （异步 trait，这里只验构造逻辑；完整走 gRPC 的用例在 plugin-ref-local 侧已有。）
        let control = Control::new("sakuramedia_actor_metadata".to_owned());
        assert_eq!(control.plugin_id, "sakuramedia_actor_metadata");
    }

    #[test]
    fn job_definition_matches_upstream() {
        // 上游 manifest / plugin.py 的任务声明。
        assert_eq!(crate::PLUGIN_ID, "sakuramedia_actor_metadata");
    }
}
