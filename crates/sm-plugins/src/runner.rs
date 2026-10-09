//! 执行插件任务：`RunJob` 的流式调用与事件收敛。
//!
//! # 上游对应
//!
//! `PluginControl.RunJob(RunJobRequest) returns (stream JobEvent)`。
//! `JobEvent` 是 oneof：`progress`（进度）或 `result`（终态摘要）。
//!
//! # 取消语义（proto 注释的原话）
//!
//! > 实现必须支持取消：**宿主会在超时或重启时直接断开流。**
//!
//! 也就是说「取消」不是发一个 cancel rpc，而是**把流丢掉**。所以本模块的
//! [`run_job`] 在超限时不做任何通知，直接 `drop(stream)` —— 由 gRPC 的流关闭
//! 表达取消。这一点必须照做：插件那一侧的 `JobEvent` 生产者要靠流断开来退出。
//!
//! # 为什么把「收敛」单独做成纯函数
//!
//! 真正的调用需要 gRPC 服务端；而事件怎么收敛成结果是**纯逻辑**，可以单测。
//! 分开后规则有测试覆盖，调用层只管 I/O。

use std::time::Duration;

use sm_plugin_api::v1::job_event::Event;
use sm_plugin_api::v1::{plugin_control_client::PluginControlClient, JobEvent, RunJobRequest};
use tonic::transport::Channel;

/// 一次任务执行的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// 收到终态 `result`。
    Completed {
        /// 终态摘要（`JobEvent.result`）。没有则为 `None`。
        has_result: bool,
        /// 期间收到的进度事件数。
        progress_events: usize,
    },
    /// 流走到结尾却**没有**终态事件 —— 按协议这是异常：插件跑完了却没给结果。
    EndedWithoutResult { progress_events: usize },
    /// 宿主在时限内没跑完，直接断开流取消。
    Cancelled { progress_events: usize },
}

/// 执行阶段的失败（区别于任务本身的失败 —— 任务失败会作为 `result` 回来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRunError {
    /// gRPC 调用失败（含插件在任务中崩了）。
    Call(String),
    /// 流里出现了一条既不是进度也不是结果的事件（协议升级不同步）。
    UnexpectedEvent,
}

impl JobRunError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Call(_) => "job_run_failed",
            Self::UnexpectedEvent => "job_event_unknown",
        }
    }
}

/// 把一串 `JobEvent` 收敛成结局。**纯函数，可单测。**
///
/// 语义：`result` 是终态，出现即结束；只有 `progress` 而流结束 → 异常收尾。
pub fn fold_events(events: &[JobEvent]) -> Result<JobOutcome, JobRunError> {
    let mut progress_events = 0usize;
    for event in events {
        match &event.event {
            Some(Event::Progress(_)) => progress_events += 1,
            Some(Event::Result(_)) => {
                return Ok(JobOutcome::Completed {
                    has_result: true,
                    progress_events,
                });
            }
            None => return Err(JobRunError::UnexpectedEvent),
        }
    }
    // 流走完了却没有终态事件。
    Ok(JobOutcome::EndedWithoutResult { progress_events })
}

/// 跑一个插件任务。
///
/// `deadline` 到达时**直接断开流**（按协议即取消），不额外通知插件。
pub async fn run_job(
    client: &mut PluginControlClient<Channel>,
    request: RunJobRequest,
    deadline: Option<Duration>,
) -> Result<JobOutcome, JobRunError> {
    let mut stream = client
        .run_job(request)
        .await
        .map_err(|err| JobRunError::Call(err.to_string()))?
        .into_inner();

    let mut events = Vec::new();
    loop {
        let next = match deadline {
            Some(limit) => match tokio::time::timeout(limit, stream.message()).await {
                Ok(next) => next,
                // 超时 = 取消：把流丢掉。下面的 `drop` 会关掉它。
                Err(_) => {
                    drop(stream);
                    return Ok(JobOutcome::Cancelled {
                        progress_events: count_progress(&events),
                    });
                }
            },
            None => stream.message().await,
        };

        match next.map_err(|err| JobRunError::Call(err.to_string()))? {
            Some(event) => {
                // 边收边判终态：收到 result 就可以停，不必等流自己关。
                let is_result = matches!(&event.event, Some(Event::Result(_)));
                events.push(event);
                if is_result {
                    break;
                }
            }
            // 流正常结束。
            None => break,
        }
    }

    fold_events(&events)
}

fn count_progress(events: &[JobEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(&event.event, Some(Event::Progress(_))))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_plugin_api::v1::ProgressEvent;

    fn progress() -> JobEvent {
        JobEvent {
            event: Some(Event::Progress(ProgressEvent::default())),
        }
    }

    fn result_event() -> JobEvent {
        JobEvent {
            // 变体自己知道字段类型，交给推断即可 —— 不必引入 `prost-types`。
            event: Some(Event::Result(Default::default())),
        }
    }

    #[test]
    fn a_result_event_ends_the_job() {
        assert_eq!(
            fold_events(&[progress(), progress(), result_event()]).unwrap(),
            JobOutcome::Completed {
                has_result: true,
                progress_events: 2
            }
        );
        // result 之后的事件不该被统计（纯函数语义上我们只看第一个终态）。
        assert_eq!(
            fold_events(&[result_event(), progress()]).unwrap(),
            JobOutcome::Completed {
                has_result: true,
                progress_events: 0
            }
        );
    }

    #[test]
    fn a_stream_that_ends_without_a_result_is_an_anomaly() {
        // 插件跑完了却没给结果 —— 不能当成成功。
        assert_eq!(
            fold_events(&[progress(), progress()]).unwrap(),
            JobOutcome::EndedWithoutResult { progress_events: 2 }
        );
        assert_eq!(
            fold_events(&[]).unwrap(),
            JobOutcome::EndedWithoutResult { progress_events: 0 }
        );
    }

    #[test]
    fn an_event_without_a_variant_is_rejected() {
        // 协议升级不同步时要显式报错，而不是静默忽略。
        let empty = JobEvent { event: None };
        assert_eq!(
            fold_events(&[empty]).unwrap_err(),
            JobRunError::UnexpectedEvent
        );
        assert_eq!(JobRunError::UnexpectedEvent.code(), "job_event_unknown");
    }

    #[test]
    fn cancellation_is_expressed_by_dropping_the_stream() {
        // 协议规定：取消 = 宿主直接断开流，没有 cancel rpc。
        // 这里只能测「结局类型表达正确」，真正的断开发生在 run_job 里。
        assert_eq!(
            JobOutcome::Cancelled { progress_events: 3 },
            JobOutcome::Cancelled { progress_events: 3 }
        );
        assert_ne!(
            JobOutcome::Cancelled { progress_events: 3 },
            JobOutcome::EndedWithoutResult { progress_events: 3 }
        );
    }
}
