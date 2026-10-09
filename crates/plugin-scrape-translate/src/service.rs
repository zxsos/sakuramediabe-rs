//! gRPC 服务面：`PluginControl`（本插件是纯任务型，没有扩展点）。
//!
//! # 三个 job（上游 `jobs.py:build_jobs`）
//!
//! | task_key | 说明 |
//! |---|---
//! | `sakuramedia_movie_scrape_translate_sync` | 手动：按番号抓取并翻译 |
//! | `sakuramedia_movie_scrape_translate_sync_subscribed` | 定时（每天 04:10）：按优先级全量 |
//! | `sakuramedia_movie_scrape_translate_translate_cached` | 手动：只翻译已有缓存，不请求 DMM |
//!
//! # `run_job` 的说明
//!
//! 任务管线的「影片列举 / 写回」需要宿主的影片存取（上游 `context.movies`），
//! 而 gRPC 插件模型下没有这条宿主 API。因此 `run_job` 目前：
//!
//! 1. 校验 `task_key` 是否是上面三个之一；
//! 2. 校验 `translate_only` 任务要求翻译已启用；
//! 3. 流式回进度事件，最后一个 `JobEvent.result` 带统计摘要；
//! 4. 实际的 DMM 抓取 / 翻译 / 状态逻辑在 [`crate::jobs`] 里是完整可用的，
//!    宿主在任务编排层实现 [`crate::jobs::MovieStore`] 即可跑全流程。

use futures::stream::BoxStream;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::{
    JobDefinition, JobEvent, ProgressEvent, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tonic::{Request, Response, Status};

use crate::settings;

/// 手动：按番号抓取并翻译。
pub const TASK_SYNC: &str = "sakuramedia_movie_scrape_translate_sync";
/// 定时：每天按优先级抓取并翻译。
pub const TASK_SYNC_SUBSCRIBED: &str = "sakuramedia_movie_scrape_translate_sync_subscribed";
/// 手动：只翻译已有 DMM 缓存。
pub const TASK_TRANSLATE_CACHED: &str = "sakuramedia_movie_scrape_translate_translate_cached";

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
    settings: settings::Settings,
}

impl Control {
    pub fn new(plugin_id: String, settings: settings::Settings) -> Self {
        Self {
            plugin_id,
            settings,
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
        let data_dir = inner.data_dir.clone();
        let settings = self.settings.clone();

        // 参数：手动任务的 movie_number。
        let movie_number = inner
            .params
            .as_ref()
            .and_then(|p| p.fields.get("movie_number"))
            .and_then(|v| match &v.kind {
                Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            });

        let translate_only = task_key == TASK_TRANSLATE_CACHED;
        if !matches!(
            task_key.as_str(),
            TASK_SYNC | TASK_SYNC_SUBSCRIBED | TASK_TRANSLATE_CACHED
        ) {
            return Err(Status::not_found(format!("未知任务: {task_key}")));
        }
        if translate_only && !settings.translation_enabled {
            return Err(Status::failed_precondition("仅翻译任务需要启用翻译"));
        }

        let stream = async_stream::try_stream! {
            // 进度事件 1：开始。
            yield JobEvent {
                event: Some(sm_plugin_api::v1::job_event::Event::Progress(ProgressEvent {
                    text: format!("任务 {task_key} 开始"),
                    current: 0,
                    total: 0,
                })),
            };
            // 任务管线的影片存取需要宿主实现（见模块文档）；这里先把
            // 「配置就绪、任务可调度」的状态回给宿主，终态摘要带上任务参数。
            let mut result = std::collections::BTreeMap::new();
            result.insert(
                "task_key".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(task_key.clone())),
                },
            );
            result.insert(
                "data_dir".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(data_dir.clone())),
                },
            );
            if let Some(number) = movie_number {
                result.insert(
                    "movie_number".to_owned(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(number)),
                    },
                );
            }
            result.insert(
                "note".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(
                        "DMM 抓取 / 翻译 / 状态逻辑在 jobs 模块；影片列举与写回由宿主任务编排层实现 MovieStore 后接入"
                            .to_owned(),
                    )),
                },
            );
            let _ = settings;
            yield JobEvent {
                event: Some(sm_plugin_api::v1::job_event::Event::Result(
                    prost_types::Struct { fields: result },
                )),
            };
        };
        Ok(Response::new(Box::pin(stream) as Self::RunJobStream))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
