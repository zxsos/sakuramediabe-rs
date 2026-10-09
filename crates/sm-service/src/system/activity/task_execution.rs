//! 任务的执行体与进度上报，对应上游 `activity/task_execution.py`（162 行）。
//!
//! # 这个模块解决的是「谁有权写终态」
//!
//! 上游 `run_task` 的核心不是「调用 handler」，而是**终态只能由赢得状态转移的
//! 那一路写**。两个执行器可能同时收口同一行 —— 领取租约到期被回收后重领、
//! 手动触发与 cron 撞在一起、worker 重启后重入。四种情形下：
//!
//! | 本地执行结果 | CAS | 上游行为 |
//! |---|---|---|
//! | 抛错 | 赢 | 写 failed，**向上抛** |
//! | 抛错 | 输，且对方已 **completed** | **吞掉异常**，返回对方的 summary |
//! | 抛错 | 输，且对方是其它终态 | 抛 `TaskRunFinalizedError` |
//! | 成功 | 输，且对方已 **failed** | 抛 `TaskRunFinalizedError`（本地成功不能覆盖失败） |
//! | 成功 | 输，且对方已 completed | 返回对方的 summary |
//!
//! 第二行最反直觉：**本地抛了异常，但数据库里已经是成功收口** —— 此时
//! 持久状态是真相，异常必须吞掉。反过来会让 worker 把一个已成功的任务
//! 报成失败，而任务中心里显示的是成功，两边对不上。
//!
//! # 与上游的三处形态差异
//!
//! **① handler 是 async 的 boxed 闭包**，不是同步 `Callable`。上游的 handler
//! 是同步函数（内部自己跑事件循环），Rust 侧处理器要 `await`，所以签名是
//! `FnOnce(TaskRunReporter) -> BoxFuture`。
//!
//! **② `TaskRunReporter` 内部用 `Arc<Mutex<_>>`**。上游是 Pydantic model 直接
//! 改字段；Rust 里 `emit` 是 `&self` 而要改 summary，得靠内部可变性。锁是
//! `std::sync::Mutex` 而非 `tokio::sync` —— 临界区里不做 `.await`。
//!
//! **③ 错误是一个枚举**而不是异常类。上游靠 `raise` 与异常类型区分，本模块
//! 用 [`TaskRunError`] 的三个变体，见其文档。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sm_db::repo::{BackgroundTaskRunRepository, TaskProgress};
use sm_db::system::activity::{result_summary, task_state};
use sm_db::Db;

use crate::error::ServiceError;

use super::task_runs::TaskRunService;

/// handler 的返回值。`Err` 是失败原因文本，语义同上游的 `Exception`。
pub type TaskHandlerResult = Result<Value, String>;

/// 任务执行体。返回一个 boxed future —— handler 需要 `await`。
pub type TaskHandler = Box<
    dyn FnOnce(TaskRunReporter) -> Pin<Box<dyn Future<Output = TaskHandlerResult> + Send>> + Send,
>;

/// 进度与摘要的上报句柄。
///
/// 上游同名类继承自 `BaseModel`，`emit` 直接改字段再调
/// `ActivityService.update_task_run_progress`。这里 `emit` 是 `&self`，因此
/// summary 走 `Arc<Mutex<_>>`。
#[derive(Clone)]
pub struct TaskRunReporter {
    task_run_id: i32,
    runs: BackgroundTaskRunRepository,
    summary: Arc<Mutex<Value>>,
}

impl TaskRunReporter {
    /// 新建上报句柄。**只有 `run_task` 该调它** —— handler 拿到的实例必须
    /// 与执行的是同一行。
    pub fn new(db: &Db, task_run_id: i32) -> Self {
        Self {
            task_run_id,
            runs: BackgroundTaskRunRepository::new(db.clone()),
            summary: Arc::new(Mutex::new(Value::Object(serde_json::Map::new()))),
        }
    }

    /// 本次执行对应的台账行 id。handler 需要它时用。
    pub fn task_run_id(&self) -> i32 {
        self.task_run_id
    }

    /// 上报进度。
    ///
    /// 对应上游 `TaskRunReporter.emit`（`task_execution.py:52-70`）。四个字段
    /// 全部可选，只有 `summary_patch` 非空时才合并进累计摘要。
    ///
    /// 累计摘要是**内存态**：调用方紧接着还会把它合并进 `result_summary`。
    /// 中间态落库只为了任务中心能看到实时进度。
    pub async fn emit(
        &self,
        current: Option<i32>,
        total: Option<i32>,
        text: Option<&str>,
        summary_patch: Option<&Value>,
    ) -> Result<(), ServiceError> {
        {
            let mut guard = self.summary.lock().map_err(|_| {
                ServiceError::from(crate::error::ProgrammerError::new(
                    "TaskRunReporter 的 summary 锁已中毒",
                ))
            })?;
            *guard = result_summary::merge(Some(&guard), summary_patch);
        }
        let snapshot = self.snapshot();
        self.runs
            .report_progress_active(
                self.task_run_id,
                &TaskProgress {
                    current,
                    total,
                    text: text.map(ToOwned::to_owned),
                },
                Some(&snapshot),
            )
            .await?;
        Ok(())
    }

    /// 当前累计摘要。
    ///
    /// `run_task` 在 handler 返回后读它，与 handler 的返回值合并后写进
    /// `result_summary`。锁中毒时返回空对象而不是 panic —— 那意味着某个
    /// handler 在 `emit` 的临界区里 panic 了，任务已经会被记为失败，
    /// 不该在这里二次 panic 把 worker 线程带走。
    pub fn snapshot(&self) -> Value {
        self.summary
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
    }
}

/// `run_task` 的失败。
#[derive(Debug)]
pub enum TaskRunError {
    /// handler 抛错，本执行器赢得转移，已写 `failed`。文本是失败原因。
    Failed(String),
    /// **本执行器没赢得状态转移**，持久终态是唯一可信结果。
    ///
    /// 上游 `TaskRunFinalizedError`（`task_execution.py:34-43`）。这不是
    /// 「执行失败」而是「执行结果已由别人决定」—— worker 应当重新读那一行
    /// 并服从它，不该重试。
    Finalized {
        state: String,
        task_run_id: i32,
        detail: String,
    },
    /// 服务层自身出错（数据库、通知写入等）。
    Service(ServiceError),
}

impl std::fmt::Display for TaskRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(message) => write!(f, "任务执行失败：{message}"),
            Self::Finalized {
                state,
                task_run_id,
                detail,
            } => write!(
                f,
                "task_run 已终态化 task_run_id={task_run_id} state={state} detail={detail}"
            ),
            // `ServiceError` 只暴露机器可读的 `code()`，没有实现 Display ——
            // 错误正文要走 HTTP 层的错误信封才能拿到。这里只报 code，
            // 足够定位，又不会把「没有 Display」误当成「没有信息」。
            Self::Service(error) => write!(f, "任务服务错误（{}）", error.code()),
        }
    }
}

impl std::error::Error for TaskRunError {}

impl From<ServiceError> for TaskRunError {
    fn from(value: ServiceError) -> Self {
        Self::Service(value)
    }
}

/// 执行一个任务：标记运行 → 跑 handler → 收口。
///
/// 对应上游 `TaskExecutionService.run_task`（`task_execution.py:86-161`）。
/// 分工：调用方（worker）负责**领取**与**租约**，本方法负责从「拿到行」到
/// 「写终态」这一段。
///
/// `log_task_name` 只进日志。上游用它起 per-task loguru logger
/// （`scheduler/logging.py`）并把日志写进 `scheduler.log_dir`；本仓库
/// `sm-server::logging` 已有落盘能力，这里只把名字透给调用方做上下文。
pub async fn run_task(
    db: &Db,
    handler: TaskHandler,
    task_run_id: i32,
    log_task_name: Option<&str>,
    notify_result: bool,
) -> Result<Value, TaskRunError> {
    let task_runs = TaskRunService::new(db);

    // 领取时已置 running，所以这一步通常是原样返回 —— 但手动触发 / 预领取
    // 路径下行仍是 pending，必须在这里推一把。
    let running = task_runs
        .mark_task_run_running(task_run_id)
        .await
        .map_err(TaskRunError::Service)?;
    if running.state != task_state::RUNNING {
        // 终态行绝不能再次进入执行体。
        return Err(TaskRunError::Finalized {
            state: running.state,
            task_run_id,
            detail: terminal_detail(&running.error_message, &running.result_text),
        });
    }

    let reporter = TaskRunReporter::new(db, task_run_id);
    let outcome = handler(reporter.clone()).await;

    match outcome {
        Err(error_message) => {
            let reporter_summary = reporter.snapshot();
            let failure = task_runs
                .fail_task_run(
                    task_run_id,
                    &error_message,
                    Some(&reporter_summary),
                    notify_result,
                )
                .await
                .map_err(TaskRunError::Service)?;

            if !failure.transitioned {
                if failure.run.state == task_state::COMPLETED {
                    // 本地抛错但对方已成功收口：持久状态是真相，吞掉异常。
                    tracing::warn!(
                        task = log_task_name,
                        task_run_id,
                        "任务异常已被持久化的成功收口覆盖，忽略这条迟到异常"
                    );
                    return Ok(result_summary::from_column_text(
                        failure.run.result_summary.as_deref(),
                    ));
                }
                return Err(TaskRunError::Finalized {
                    state: failure.run.state,
                    task_run_id,
                    detail: terminal_detail(&failure.run.error_message, &failure.run.result_text),
                });
            }

            tracing::error!(
                task = log_task_name,
                task_run_id,
                error = %error_message,
                "任务执行失败，已收口为 failed"
            );
            Err(TaskRunError::Failed(error_message))
        }
        Ok(result) => {
            let mut merged = reporter.snapshot();
            if result.is_object() {
                merged = result_summary::merge(Some(&merged), Some(&result));
            }
            let completion = task_runs
                .complete_task_run(task_run_id, Some(&merged), None, notify_result)
                .await
                .map_err(TaskRunError::Service)?;

            if !completion.transitioned {
                if completion.run.state == task_state::FAILED {
                    // 本地成功也不能覆盖已持久化的失败终态。
                    return Err(TaskRunError::Finalized {
                        state: completion.run.state,
                        task_run_id,
                        detail: terminal_detail(
                            &completion.run.error_message,
                            &completion.run.result_text,
                        ),
                    });
                }
                return Ok(result_summary::from_column_text(
                    completion.run.result_summary.as_deref(),
                ));
            }

            tracing::info!(task = log_task_name, task_run_id, "任务执行完成");
            Ok(result)
        }
    }
}

fn terminal_detail(error_message: &Option<String>, result_text: &Option<String>) -> String {
    error_message
        .clone()
        .or_else(|| result_text.clone())
        .unwrap_or_else(|| "任务已由其它执行器收口".to_owned())
}
