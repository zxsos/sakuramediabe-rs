//! `activity` 子包：任务台账的**执行侧**。
//!
//! 上游 `src/service/system/activity/` 共 7 个文件，由 `facade.py` 的
//! `ActivityService` 多继承组合四个服务（`TaskExecutionService` /
//! `TaskRunService` / `NotificationService` / `ActivityBootstrapService`）。
//! Rust 没有 mixin，所以按职责拆成模块。
//!
//! | 本包模块 | 上游对应 | 本批 |
//! |---|---|---|
//! | [`task_runs`] | `task_runs.py` 的**写侧** | ✅ 状态转移 + 收口 + 通知联动 |
//! | [`task_execution`] | `task_execution.py` | ✅ `run_task` 与进度上报 |
//! | [`notifications`] | `notifications.py` 的**写侧** | ✅ `notify_task_result` |
//! | [`task_catalog`] | `task_catalog.py` | ✅ 任务显示名注册表 |
//! | [`filters`] | `filters.py` | ✅ 查询参数归一化 |
//! | — | `task_runs.py` 的读侧（列表 / 排序 / 分页） | ⏳ 随 HTTP 端点落地 |
//! | — | `notifications.py` 的读侧（列表 / 已读 / 未读计数） | ⏳ 随 HTTP 端点落地 |
//! | — | `bootstrap.py` | ⏳ 随 `GET /system/activity/bootstrap` 落地 |
//!
//! # 读侧为什么不在本批
//!
//! 读侧方法（`build_task_run_query` / `list_notifications` /
//! `get_unread_count`）只被 HTTP 端点调用，没有 worker 路径的调用方。它们的
//! 存在意义是响应 `GET /system/task-runs` 与 `GET /system/notifications`
//! —— 写一个没有调用方的函数只会掩盖「端点还没接」这件事。
//!
//! # 为什么没有 `ActivityService` 这个门面
//!
//! 上游那个门面存在的唯一理由是 Python 的多继承与方法解析顺序。本包的服务
//! 已经是平铺的模块，worker 直接组合 `TaskRunService` 与
//! [`run_task`] 即可。等 worker（块 2）真的需要单一入口时再加门面。
//!
//! [`filters`] 里的筛选函数本批没有调用方 —— 它们是给读侧用的，**先于**读侧
//! 落地，因为它们是纯函数、可以独立断言，且能挡住「空白与 `None` 混淆」
//! 这类错误在读侧落地时才被发现。

pub mod bootstrap;
pub mod filters;
pub mod notifications;
pub mod task_catalog;
pub mod task_execution;
pub mod task_runs;

pub use bootstrap::{
    ActivityBootstrap, ActivityBootstrapQuery, ActivityBootstrapService,
    ACTIVITY_BOOTSTRAP_PAGE_SIZE,
};
pub use filters::{normalize_allowed_filter, normalize_string_filter};
pub use notifications::{
    notify_task_result, task_result_dedupe_key, BatchReadResult, NotificationService,
    TASK_RESULT_EVENT,
};
pub use task_catalog::{lookup_task_name, resolve_task_name, TASK_NAME_REGISTRY};
pub use task_execution::{run_task, TaskHandler, TaskHandlerResult, TaskRunError, TaskRunReporter};
pub use task_runs::{
    TaskRunService, TaskRunTransition, ALLOWED_TASK_TRIGGER_TYPES, DEFAULT_TASK_RUN_SORT,
    TASK_RUN_SORT_FIELDS,
};

/// `background_task_run.state` 的合法取值。
///
/// 转发 `sm_db` 的白名单，让调用方不必为了判一个状态而依赖 `sm_db` 路径。
pub use sm_db::system::activity::task_state;

/// 通知分类白名单。转发理由同上。
pub use sm_db::system::activity::notification_category;
