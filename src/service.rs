//! gRPC 服务面：`PluginControl`（注册 + 两个任务）。
//!
//! # 上游对应
//!
//! - `plugin.py:register` → [`Control::register`]：声明两个 job；
//! - `jobs.py:build_jobs` → [`Control::run_job`]：按 `task_key` 分发。
//!
//! # 两个任务
//!
//! | task_key | 上游 | 说明 |
//! |---|---|
//! | `sakuramedia_subtitlecat_fetch` | `run_fetch` | 手动抓取单部影片 |
//! | `sakuramedia_subtitlecat_fetch_subscribed` | `run_subscribed` | 定时抓取已订阅影片 |
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control`] 的
//! `register` 只回声明；HTTP 客户端是在每次 `RunJob` 时建的。

use futures::stream::{self, BoxStream, StreamExt};
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::{
    Extension, JobDefinition, JobEvent, ProgressEvent, RegisterRequest, RegisterResponse,
    RunJobRequest,
};
use tonic::{Request, Response, Status};

use crate::settings;
use crate::subtitlecat::{SubtitleCatClient, SubtitleCatError};

/// 手动抓取单部影片的 task_key（上游 `jobs.py` 的 `task_key`）。
pub const TASK_FETCH: &str = "sakuramedia_subtitlecat_fetch";
/// 定时抓取已订阅影片的 task_key。
pub const TASK_FETCH_SUBSCRIBED: &str = "sakuramedia_subtitlecat_fetch_subscribed";

/// 控制面。
pub struct Control {
    plugin_id: String,
}

impl Control {
    pub fn new(plugin_id: String) -> Self {
        Self { plugin_id }
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
                params_schema: None,
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

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let inner = request.into_inner();
        let settings = settings::Settings::load();
        let client = SubtitleCatClient::new(&settings)
            .map_err(|e| Status::internal(format!("建 SubtitleCat 客户端失败: {e}")))?;

        match inner.task_key.as_str() {
            TASK_FETCH => {
                // 参数：上游 `FetchSubtitleParams.movie_number`。
                let movie_number = extract_movie_number(&inner)?;
                let events = run_fetch(client, movie_number).await;
                Ok(Response::new(stream::iter(events).boxed()))
            }
            TASK_FETCH_SUBSCRIBED => {
                // 订阅任务需要宿主侧的影片列表与导入回调，当前契约下插件拿不到；
                // 返回 unimplemented，待宿主补齐 host 回调通道后实现。
                let event = JobEvent {
                    event: Some(sm_plugin_api::v1::job_event::Event::Progress(
                        ProgressEvent {
                            current: 0,
                            total: 0,
                            text: "订阅抓取需要宿主提供影片列表，暂未实现".to_owned(),
                            ..Default::default()
                        },
                    )),
                };
                Ok(Response::new(stream::iter(vec![Ok(event)]).boxed()))
            }
            other => Err(Status::invalid_argument(format!("未知 task_key: {other}"))),
        }
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

/// 从 `RunJobRequest` 里取 `movie_number` 参数。
fn extract_movie_number(request: &RunJobRequest) -> Result<String, Status> {
    // 参数以 JSON Struct 形式传（与上游 `FetchSubtitleParams` 对齐）。
    let params = request
        .params
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("缺少任务参数：需要 movie_number"))?;
    let fields = &params.fields;
    let value = fields
        .get("movie_number")
        .ok_or_else(|| Status::invalid_argument("缺少任务参数：需要 movie_number"))?;
    let s = value
        .kind
        .as_ref()
        .and_then(|k| match k {
            prost_types::value::Kind::StringValue(v) => Some(v.clone()),
            _ => None,
        })
        .ok_or_else(|| Status::invalid_argument("movie_number 必须是字符串"))?;
    let normalized = crate::subtitlecat::normalize_movie_number(&s);
    if normalized.is_empty() {
        return Err(Status::invalid_argument("movie_number 不能为空"));
    }
    Ok(normalized)
}

/// 执行单部抓取：事件流（进度 + 终态摘要）。
async fn run_fetch(
    client: SubtitleCatClient,
    movie_number: String,
) -> Vec<Result<JobEvent, Status>> {
    let mut events = Vec::new();

    // 先报进度：开始抓取。
    events.push(Ok(progress_event(
        0,
        0,
        format!("开始抓取 {movie_number} 的中文字幕"),
    )));

    let subtitles = match client.fetch_chinese_subtitles(&movie_number).await {
        Ok(subtitles) => subtitles,
        Err(e) => {
            return vec![Err(status_of(&e))];
        }
    };

    events.push(Ok(progress_event(
        0,
        subtitles.len() as i32,
        format!("找到 {} 份中文字幕", subtitles.len()),
    )));

    // 终态摘要：与上游 `_MANUAL_STAT_KEYS` 对齐。
    // 字幕字节以 base64 放在 result 里，由宿主侧导入（插件不直接写宿主库）。
    let mut result = prost_types::Struct::default();
    result.fields.insert(
        "movie_number".to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(movie_number.clone())),
        },
    );
    result.fields.insert(
        "source_matches".to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(subtitles.len() as f64)),
        },
    );
    // base64 编码的字幕列表。
    let encoded: Vec<String> = subtitles.iter().map(|b| base64_encode(b)).collect();
    let list_values: Vec<prost_types::Value> = encoded
        .into_iter()
        .map(|s| prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s)),
        })
        .collect();
    result.fields.insert(
        "subtitles_base64".to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(
                prost_types::ListValue {
                    values: list_values,
                },
            )),
        },
    );

    events.push(Ok(JobEvent {
        event: Some(sm_plugin_api::v1::job_event::Event::Result(result)),
    }));
    events
}

/// 构造进度事件。
fn progress_event(current: i32, total: i32, text: String) -> JobEvent {
    JobEvent {
        event: Some(sm_plugin_api::v1::job_event::Event::Progress(
            ProgressEvent {
                current,
                total,
                text,
                ..Default::default()
            },
        )),
    }
}

/// 把抓取失败压成 gRPC 状态。
fn status_of(err: &SubtitleCatError) -> Status {
    match err {
        SubtitleCatError::ClientError(_) => Status::invalid_argument(err.to_string()),
        SubtitleCatError::InvalidSubtitle(_) => Status::internal(err.to_string()),
        SubtitleCatError::Request(_) => Status::unavailable(err.to_string()),
    }
}

/// 简单的 base64 编码（不引入新依赖）。
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_task_keys_are_distinct() {
        assert_ne!(TASK_FETCH, TASK_FETCH_SUBSCRIBED);
    }

    #[test]
    fn base64_roundtrip() {
        // 用标准测试向量。
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_client_error_is_invalid_argument() {
        let status = status_of(&SubtitleCatError::ClientError("HTTP 404".to_owned()));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_request_error_is_unavailable() {
        let status = status_of(&SubtitleCatError::Request("timeout".to_owned()));
        assert_eq!(status.code(), tonic::Code::Unavailable);
    }
}
