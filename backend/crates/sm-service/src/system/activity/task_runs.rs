//! `background_task_run` 的状态转移服务层，对应上游
//! `activity/task_runs.py`（377 行）里**除查询之外**的部分。
//!
//! 上游那个类叫 `TaskRunService`，与 `TaskExecutionService` /
//! `NotificationService` / `ActivityBootstrapService` 一起被 `ActivityService`
//! 多继承组合。Rust 没有 mixin，所以按职责拆成模块、由
//! `super::ActivityService` 持有一份仓储并转发。
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

use serde_json::Value;
use sm_core::pagination::Paginated;
use sm_db::common::page::PageRequest;
use sm_db::repo::{BackgroundTaskRunRepository, SystemNotificationRepository, TaskProgress};
use sm_db::system::activity::BackgroundTaskRun;
use sm_db::Db;

use crate::error::{details_of, ServiceError};

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

/// `trigger_type` 的合法取值。对应上游 `ALLOWED_TASK_TRIGGER_TYPES`
/// （`task_runs.py:20`）。
///
/// 数据库对该列**没有 CHECK 约束** —— 上游也是。所以白名单只在这层生效，
/// 绕过 service 直写库能塞进任何字面量，而按白名单筛选时它会静默消失。
pub const ALLOWED_TASK_TRIGGER_TYPES: [&str; 5] =
    ["scheduled", "manual", "startup", "internal", "plugin"];

/// 合法排序规则。对应上游 `TASK_RUN_SORT_FIELDS`（`task_runs.py:23-30`）。
///
/// 每个值是 `字段:方向`。`id` 次级键由仓储固定追加，不在这里列出 ——
/// 它不是可选维度。
pub const TASK_RUN_SORT_FIELDS: [&str; 6] = [
    "started_at:desc",
    "started_at:asc",
    "created_at:desc",
    "created_at:asc",
    "updated_at:desc",
    "updated_at:asc",
];

/// 缺省排序。上游 `(sort or "started_at:desc")`（`task_runs.py:105`）。
pub const DEFAULT_TASK_RUN_SORT: &str = "started_at:desc";

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

    // ------------------------------------------------------------ 读侧

    /// 分页查询任务运行记录。
    ///
    /// 对应上游 `TaskRunService.list_task_runs`（`task_runs.py:347-367`）。
    /// 三个筛选与排序都在这里**先校验再下发** —— 仓储层不认白名单，传非法值
    /// 会退化成「只按 id DESC 排」，那是个稳定但无意义的顺序。
    pub async fn list_task_runs(
        &self,
        state: Option<&str>,
        trigger_type: Option<&str>,
        task_key: Option<&str>,
        sort: Option<&str>,
        page: i64,
        page_size: i64,
    ) -> Result<Paginated<BackgroundTaskRun>, ServiceError> {
        let state = super::filters::normalize_allowed_filter(
            state,
            "state",
            &sm_db::system::activity::task_state::ALL,
        )?;
        let trigger_type = super::filters::normalize_allowed_filter(
            trigger_type,
            "trigger_type",
            &ALLOWED_TASK_TRIGGER_TYPES,
        )?;
        // 任务键**区分大小写**，只折叠空白不 lower —— 见 filters 模块文档。
        let task_key = super::filters::normalize_string_filter(task_key);

        // 排序要 lower + trim（上游 `(sort or "started_at:desc").strip().lower()`）
        let sort = super::filters::normalize_string_filter(sort)
            .unwrap_or_else(|| DEFAULT_TASK_RUN_SORT.to_owned())
            .to_lowercase();
        if !TASK_RUN_SORT_FIELDS.contains(&sort.as_str()) {
            return Err(ServiceError::validation_with(
                "invalid_task_run_sort",
                "任务排序规则不合法",
                {
                    let mut details = details_of("sort", sort);
                    let mut allowed = TASK_RUN_SORT_FIELDS.to_vec();
                    allowed.sort_unstable();
                    details.insert("allowed_values".to_owned(), Value::from(allowed));
                    details
                },
            ));
        }

        let request = PageRequest::new(page, page_size)?;
        // `paged_list!` 把 `page: PageRequest` 追加在声明的参数之后。
        let result = self
            .runs
            .list_runs(state, trigger_type, task_key, sort, request)
            .await?;
        Ok(result.into_paginated(&request))
    }

    /// 列出**进行中**（`pending` + `running`）的任务运行，新的在前。
    ///
    /// 对应上游 `list_active_task_runs`（`task_runs.py:370-376`）。**不
    /// 分页** —— 语义是「现在有什么在跑」，分页会把在跑的任务挤到第二页。
    pub async fn list_active_task_runs(&self) -> Result<Vec<BackgroundTaskRun>, ServiceError> {
        Ok(self.runs.list_active_runs().await?)
    }
}

impl TaskRunTransition {
    /// 行是否已是终态（不论是本调用收的还是别人收的）。
    pub fn is_terminal(&self) -> bool {
        self.is_completed()
    }
}
