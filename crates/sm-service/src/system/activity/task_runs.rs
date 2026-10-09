//! `background_task_run` 的状态转移服务层，对应上游
//! `activity/task_runs.py`（377 行）里**除查询之外**的部分。
//!
//! 上游那个类叫 `TaskRunService`，与 `TaskExecutionService` /
//! `NotificationService` / `ActivityBootstrapService` 一起被 `ActivityService`
//! 多继承组合。Rust 没有 mixin，所以按职责拆成模块、由
//! [`super::ActivityService`] 持有一份仓储并转发。
//!
//! # 为什么这一层不直接调 `finish` / `fail`
//!
//! 队列路径的 `BackgroundTaskRunRepository::finish` / `fail` 与本模块要的语义
//! **不同**，差别有三处，每一处都会让 worker 判错：
//!
//! | | `finish` / `fail` | 本模块 |
//! |---|---|---|
//! | 合法来源 | 仅 `running` | `pending` + `running` |
//! | `result_summary` | 覆盖 | 行锁内合并 |
//! | 输掉 CAS | 报 `business` 错误 | 返回 `transitioned = false` |
//!
//! 第三处是**正确性的关键**：两个执行器可能同时收口同一行（领取超时被回收
//! 后重领、或手动触发与 cron 撞在一起）。行锁裁决出唯一赢家，输的一方
//! **必须读到已持久化的终态并服从它**，而不是抛错让 worker 以为任务崩了。
//! 上游 `task_execution.py` 正是据此决定「本地执行成功也不能覆盖已持久化的
//! 失败终态」。
//!
//! # 读侧（列表 / 排序 / 分页）不在本模块
//!
//! `build_task_runs_query` / `page_task_runs` / `list_task_runs` /
//! `list_active_task_runs` 服务于 `GET /system/task-runs` 那批端点，
//! 随 activity 的 HTTP 层一起落地。

use sm_db::repo::{BackgroundTaskRunRepository, SystemNotificationRepository, TaskProgress};
use sm_db::system::activity::BackgroundTaskRun;
use sm_db::Db;
use serde_json::Value;

use crate::error::ServiceError;

use super::notifications::notify_task_result;
use super::task_catalog::resolve_task_name;

/// 台账行不存在的 404。集中构造，别在五处重复写 code 与 details 键名。
fn task_run_not_found(task_run_id: i32) -> ServiceError {
    ServiceError::not_found(
        "task_run_not_found",
        format!("任务 {task_run_id} 不存在"),
        "task_run_id",
        task_run_id,
    )
}

/// 一次终态转移的结果。
///
/// `transitioned = false` 表示**本调用没有赢得状态转移** —— 行已是终态。
/// 此时 `run` 是锁内读到的既有终态，调用方必须服从它。
#[derive(Debug, Clone)]
pub struct TaskRunTransition {
    pub run: BackgroundTaskRun,
    pub transitioned: bool,
}

impl TaskRunTransition {
    fn is_completed(&self) -> bool {
        sm_db::system::activity::task_state::is_terminal(&self.run.state)
    }
}

/// 任务台账的状态转移。
#[derive(Debug, Clone)]
pub struct TaskRunService {
    runs: BackgroundTaskRunRepository,
    notifications: SystemNotificationRepository,
}

impl TaskRunService {
    /// 构造服务。
    pub fn new(db: &Db) -> Self {
        Self {
            runs: BackgroundTaskRunRepository::new(db.clone()),
            notifications: SystemNotificationRepository::new(db.clone()),
        }
    }

    /// 仅执行 `pending -> running`；其它状态原样返回且零写入。
    ///
    /// 对应上游 `mark_task_run_running`（`task_runs.py:166`）。
    pub async fn mark_task_run_running(
        &self,
        task_run_id: i32,
    ) -> Result<BackgroundTaskRun, ServiceError> {
        let run = self
            .runs
            .mark_running(task_run_id)
            .await?
            .ok_or_else(|| task_run_not_found(task_run_id))?;
        Ok(run)
    }

    /// 更新进行中任务的显式进度字段。终态行原样返回且零写入。
    ///
    /// 对应上游 `update_task_run_progress`（`task_runs.py:180`）。摘要补丁在
    /// 行锁内合并 —— 否则两个并发 reporter 会互相覆盖键位。
    pub async fn update_task_run_progress(
        &self,
        task_run_id: i32,
        progress: &TaskProgress,
        summary_patch: Option<&Value>,
    ) -> Result<BackgroundTaskRun, ServiceError> {
        let run = self
            .runs
            .report_progress_active(task_run_id, progress, summary_patch)
            .await?
            .ok_or_else(|| task_run_not_found(task_run_id))?;
        Ok(run)
    }

    /// 把进行中的任务收口为 `completed`。
    ///
    /// 对应上游 `complete_task_run` 及其内部的 `_complete_task_run_transition`
    /// （`task_runs.py:224-269`）。返回值把「是否赢得转移」一并带出来 ——
    /// 上游拆成两个方法（公开的丢 bool、内部私有的不丢）是为了让 HTTP 层
    /// 拿不到那个 bool；这里没有那个顾虑，一个方法就够。
    pub async fn complete_task_run(
        &self,
        task_run_id: i32,
        result_summary: Option<&Value>,
        result_text: Option<&str>,
        notify_result: bool,
    ) -> Result<TaskRunTransition, ServiceError> {
        let (row, transitioned) = self
            .runs
            .complete_active(task_run_id, result_summary, result_text)
            .await?
            .ok_or_else(|| task_run_not_found(task_run_id))?;

        // 只有赢得转移的那一方发通知：CAS 落败说明别人已经收过口并发过了，
        // 再发一次就是刷屏。
        if transitioned && notify_result {
            notify_task_result(&self.notifications, &row, false).await?;
        }
        Ok(TaskRunTransition {
            run: row,
            transitioned,
        })
    }

    /// 把进行中的任务收口为 `failed`。
    ///
    /// 对应上游 `fail_task_run` / `_fail_task_run_transition` /
    /// `_fail_locked_task_run`（`task_runs.py:272-332`）。上游那三个方法最终
    /// 汇到一处，本模块对应一个。
    pub async fn fail_task_run(
        &self,
        task_run_id: i32,
        error_message: &str,
        result_summary: Option<&Value>,
        notify_result: bool,
    ) -> Result<TaskRunTransition, ServiceError> {
        let (row, transitioned) = self
            .runs
            .fail_active(task_run_id, error_message, result_summary)
            .await?
            .ok_or_else(|| task_run_not_found(task_run_id))?;
        if transitioned && notify_result {
            notify_task_result(&self.notifications, &row, true).await?;
        }
        Ok(TaskRunTransition {
            run: row,
            transitioned,
        })
    }

    /// 每个 `task_key` 各自最新一条运行记录。
    ///
    /// 供 `GET /system/jobs` 的目录项填 `last_task_run`。走服务层而不是让
    /// `sm-api` 直接调仓储，有两个好处：分层不破（`api → service → db`），
    /// 以及 `DbError → ServiceError → ErrorResponse` 的转换链是现成的 ——
    /// `sm-api::error` 没有 `From<DbError>`。
    pub async fn latest_runs_by_task_key(
        &self,
        task_keys: &[String],
    ) -> Result<std::collections::HashMap<String, BackgroundTaskRun>, ServiceError> {
        Ok(self.runs.latest_by_task_keys(task_keys).await?)
    }

    /// 按互斥键取**最早**的一行。
    ///
    /// 对应上游 `find_task_run_by_mutex_key`（`task_runs.py:335-344`）。空白键
    /// 归一为「不查询」—— 空白串不参与互斥，拿它查只会查到一个「永不被占用」
    /// 的值。
    pub async fn find_task_run_by_mutex_key(
        &self,
        mutex_key: &str,
    ) -> Result<Option<BackgroundTaskRun>, ServiceError> {
        let Some(key) = super::filters::normalize_string_filter(Some(mutex_key)) else {
            return Ok(None);
        };
        Ok(self.runs.find_by_mutex_key(&key).await?)
    }

    /// 取一行。找不到时返回 404。
    pub async fn get_task_run(&self, task_run_id: i32) -> Result<BackgroundTaskRun, ServiceError> {
        self.runs
            .find_by_id(task_run_id)
            .await?
            .ok_or_else(|| task_run_not_found(task_run_id))
    }

    /// 任务显示名。转发 [`resolve_task_name`]，让入队路径不必知道
    /// `task_catalog` 这个模块。
    pub fn resolve_task_name(&self, task_key: &str, task_name: Option<&str>) -> String {
        resolve_task_name(task_key, task_name)
    }
}

impl TaskRunTransition {
    /// 行是否已是终态（不论是本调用收的还是别人收的）。
    pub fn is_terminal(&self) -> bool {
        self.is_completed()
    }
}
