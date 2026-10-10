//! 插件任务 → worker 处理器：把插件声明的任务接进 `HandlerRegistry`。
//!
//! # 它补的是哪条缝
//!
//! worker 的处理器注册表默认只有 6 个内建任务（`builtin_handlers`）。插件在
//! 注册响应里声明的任务（`JobDefinition`）此前**只进了任务目录与调度器**
//! （[`Plugins::catalog`] / [`Plugins::scheduler_specs`]）—— 于是任务会到点
//! 入队、能被手动触发，但 worker 在 `handlers.build()` 一步查不到处理器，
//! 按「未知任务键」收口为 failed（见 `sm_scheduler::worker` 模块文档）。
//! 本模块把这一步接上：**每个插件任务注册一个处理器，执行时把 `RunJob`
//! 发给归属插件。**
//!
//! # 端点必须现取
//!
//! 处理器构造（工厂被调用）时向 [`Plugins::job_target`] 要「活端点」——
//! 插件重启会换端口，启动期快照会带着旧端口，表现为「任务全部失败而插件
//! 看起来正常」。这与 provider / 排行写侧那条「活的注册表」纪律是同一条
//! （见 `Plugins::provider_registry` 的文档）。
//!
//! # 一次执行的生命周期
//!
//! 1. `job_target(task_key)` → 插件不在线就直接失败。**不重试**：重试要等
//!    下一次入队，而 `mutex_key` 保证同一任务不会并发重跑；失败信息会让
//!    任务中心显示「插件不可用」，比转圈好。
//! 2. `connect` 建通道 → `RunJob` 发流。**每次执行新建通道**：插件重启换
//!    端口，缓存 channel 就要配失效逻辑；回环连接的开销对分钟级任务可忽略。
//! 3. 插件每发一条 `progress` 就 `reporter.emit(...)` 落一次进度 ——
//!    进度是旁路，落库失败只 warn，不打断任务。
//! 4. 终态三态映射（见 [`execute_plugin_job`] 的 `match`）：
//!    `result` → handler 返回值；流断而无终态 → 失败；超时 → 失败（流已断，
//!    按协议即取消）。
//!
//! # 超时值写死在常量里（**登记**）
//!
//! `scheduler` 配置节没有任务执行超时键，上游也没有（进程内调用，不设防）。
//! 跨进程后不设不行：插件挂死时流会永远挂着，worker 的一个并发位被永久
//! 占住（`in_flight` 集合里的行还会被 housekeeper 无限续租）。[`JOB_DEADLINE`]
//! 是拍的量级 —— 值得升级成 `scheduler.job_timeout_seconds` 一类的配置键。
//!
//! # 为什么不注册 `BusinessRecovery`
//!
//! 内建任务的收口钩子收的是**宿主侧领域状态**（比如图搜索引的半成品）。插件
//! 任务的半成品在**插件自己**手里（`data_dir` / 它自己的库），而 proto 没有
//! 「取消 / 收口」rpc —— 宿主能做的（断流）在超时时已经做了。
//! `background_task_run` 侧的状态由 `run_task` 收口，不归 recovery 管；插件
//! 需要的自愈（下次执行时清理）是插件自己的职责。所以这里**刻意不带**
//! recovery —— 缺省就是无操作（`HandlerRegistry::run_recovery` 查不到即返回）。

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sm_db::Db;
use sm_plugin_api::json_struct::json_to_struct;
use sm_plugin_api::v1::job_event::Event;
use sm_plugin_api::v1::RunJobRequest;
use sm_plugins::loader::connect;
use sm_plugins::runner::{run_job_with_progress, JobOutcome, JobRunError};
use sm_scheduler::worker::HandlerRegistry;
use sm_service::system::activity::{TaskHandler, TaskHandlerResult, TaskRunReporter};
use tokio::sync::Mutex;
use tonic::transport::Endpoint;

use crate::plugins::Plugins;

/// 单次插件任务执行的时限（总时长，含建流）。
///
/// 量级取「长任务（全量抓取 / 翻译扫描）也跑得完、挂死不会过夜」。到期宿主
/// 直接断开调用流（按协议即取消），任务收口为 failed —— 插件能不能中途收到
/// 取消由它自己靠流断开感知（proto 的取消语义，见 `sm_plugins::runner`）。
const JOB_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// 为**当前注册表里的全部插件任务**注册处理器。
///
/// 在组合根、`Plugins::load` 之后调用一次。`Arc<tokio::sync::Mutex<Plugins>>`
/// 与看门狗共用同一个句柄 —— 处理器执行时必须看**活**的插件表。
///
/// 任务清单在这里取一次快照：清单的增删只发生在插件重启 + `rebuild` 时，
/// 而 `HandlerRegistry` 在 worker 构造时就 `Arc` 固化（`TaskWorker` 不做
/// 运行时增量注册）。看门狗重建注册表后**新出现**的任务要等下次进程启动才
/// 有处理器 —— 与「新 cron 要等重启才生效」是同一条边界（见 `plugins`
/// 模块文档的看门狗一节）。
pub async fn plugin_handlers(plugins: Arc<Mutex<Plugins>>) -> HandlerRegistry {
    let entries = plugins.lock().await.plugin_job_entries();
    let mut registry = HandlerRegistry::new();
    tracing::info!(count = entries.len(), "注册插件任务处理器");

    for (task_key, plugin_id) in entries {
        let plugins_for_factory = Arc::clone(&plugins);
        let task_key_for_factory = task_key.clone();
        let plugin_id_for_factory = plugin_id.clone();
        registry.register(
            &task_key,
            Box::new(move |_db: &Db, params: &Value| {
                // 工厂是 `Fn`（每个任务运行调一次），捕获的东西每次都要 clone，
                // 不能 move —— 同 `builtin_handlers` 里那条注释。
                let plugins = Arc::clone(&plugins_for_factory);
                let task_key = task_key_for_factory.clone();
                let plugin_id = plugin_id_for_factory.clone();
                let params = params.clone();
                let handler: TaskHandler = Box::new(move |reporter| {
                    Box::pin(execute_plugin_job(plugins, plugin_id, task_key, params, reporter))
                });
                Ok(handler)
            }),
        );
    }
    registry
}

/// 执行一次插件任务。失败原因会进 `background_task_run.error`，写具体些。
async fn execute_plugin_job(
    plugins: Arc<Mutex<Plugins>>,
    plugin_id: String,
    task_key: String,
    params: Value,
    reporter: TaskRunReporter,
) -> TaskHandlerResult {
    // ① 现取目标（活端点 + 数据目录）。锁只在这条查询期间持有。
    let Some(target) = plugins.lock().await.job_target(&task_key) else {
        return Err(format!(
            "插件任务 {task_key} 当前无人可跑：归属插件 {plugin_id} 未加载或已退出"
        ));
    };

    // ② 建通道。
    let endpoint = Endpoint::from_shared(target.endpoint.clone())
        .map_err(|error| format!("插件端点不合法（{}）：{error}", target.endpoint))?;
    let mut client = connect(endpoint)
        .await
        .map_err(|error| format!("连接插件失败（{}）：{error:?}", error.code()))?;

    // ③ 发 `RunJob`，进度实时转给 reporter。
    let request = RunJobRequest {
        // proto：「本次执行的唯一 id，插件用它关联 task_run 记录」。
        run_id: reporter.task_run_id().to_string(),
        task_key: task_key.clone(),
        // `run_id` 之外的形参 —— `json_to_struct` 只收对象根（见其文档），
        // params 不是对象时不传（`None` = 字段缺省）。
        params: json_to_struct(&params),
        data_dir: target.data_dir.display().to_string(),
    };
    let progress_reporter = reporter.clone();
    let outcome = run_job_with_progress(&mut client, request, Some(JOB_DEADLINE), move |event| {
        let reporter = progress_reporter.clone();
        async move {
            if let Some(Event::Progress(progress)) = event.event {
                // proto 的 `current` / `total` 不是 optional，0 义为「没给」——
                // 两个都没给就传 `None`（「只说了一句话」），别在任务中心里
                // 显示成「0 / 0」。
                let (current, total) = if progress.current == 0 && progress.total == 0 {
                    (None, None)
                } else {
                    (Some(progress.current), Some(progress.total))
                };
                let text = (!progress.text.is_empty()).then_some(progress.text.as_str());
                if let Err(error) = reporter.emit(current, total, text, None).await {
                    // 进度是旁路：落不进去（比如那一行已被收口）只记 warn，
                    // 不打断任务本身。
                    tracing::warn!(code = error.code(), "插件任务进度落库失败");
                }
            }
        }
    })
    .await
    .map_err(|error| match error {
        JobRunError::Call(message) => format!("调用插件失败：{message}"),
        JobRunError::UnexpectedEvent => {
            "插件发出了未知类型的事件（协议版本不同步）".to_owned()
        }
    })?;

    // ④ 三态映射。
    match outcome {
        // 终态摘要就是 handler 的返回值 → `run_task` 拿它当 `result_summary`。
        JobOutcome::Completed { result, .. } => Ok(result),
        JobOutcome::EndedWithoutResult { progress_events } => Err(format!(
            "插件在发完 {progress_events} 条进度后结束了调用流，却没有给出终态结果（协议违约）"
        )),
        JobOutcome::Cancelled { progress_events } => Err(format!(
            "插件任务超过时限（{} 秒），宿主已断开调用流；断开前收到 {progress_events} 条进度",
            JOB_DEADLINE.as_secs()
        )),
    }
}
