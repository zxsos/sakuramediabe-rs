//! `system` 域：鉴权、账号、状态、配置、任务。
//!
//! 上游 `src/service/system/` 共 11 个文件 / 1,996 行。本批落了 `auth`、
//! `account`、`activity_cleanup`、`config` 与 `task_queue` 五个。
//!
//! # `task_queue` 是 worker 的地基
//!
//! 调度器只**入队**，执行在 worker 侧（见 `sm_scheduler` 的 crate 文档）。
//! 本模块的 [`task_queue`] 是队列本体：互斥（coalesce）、领取、租约、回收。
//! 阶段 7 的 worker 只缺「handler 分发」—— 队列侧的地基已经在这了。
//!
//! 其余按体量从小到大排：`optional_services`(38，功能开关) →
//! `plugin_removal`(95) → `telemetry`(188) → `indexer_settings`(304) →
//! `status`(602)。
//!
//! `optional_services`(38) 是功能开关（`job_disabled_reason` /
//! `movie_similarity_enabled`），不落它会让「任务为什么没跑」无法解释 ——
//! 排在能落地业务的那几个之后。

pub mod account;
pub mod activity;
pub mod activity_cleanup;
pub mod auth;
pub mod config;
pub mod indexer_settings;
pub mod jobs;
pub mod optional_services;
pub mod status;
pub mod task_queue;

pub use account::AccountService;
pub use activity::{
    run_task, normalize_allowed_filter, normalize_string_filter, notification_category,
    notify_task_result, resolve_task_name, task_result_dedupe_key, task_state, TaskHandler,
    TaskHandlerResult, TaskRunError, TaskRunReporter, TaskRunService, TaskRunTransition,
    TASK_NAME_REGISTRY, TASK_RESULT_EVENT,
};
pub use activity_cleanup::{ActivityCleanupService, ActivityCleanupStats, RetentionPolicy};
pub use config::ConfigService;
pub use jobs::{JobCatalog, JobCatalogEntry};
pub use optional_services::{
    capabilities, capabilities_of, image_search_enabled, job_disabled_reason,
    movie_similarity_enabled, require_image_search, require_job_enabled, Capabilities,
    FeatureDisabled, FEATURE_DISABLED,
};
pub use task_queue::{
    ConflictPolicy, EnqueueOutcome, TaskQueueService, BOOTSTRAP_QUEUE_TASK_KEYS,
    DEFAULT_LEASE_SECONDS, FAILURE_CODE_QUEUE_LEASE_EXPIRED, INTERNAL_FAILURE_CODE_KEY,
    INTERRUPTED_ERROR_MESSAGE, LEASE_EXPIRED_ERROR_MESSAGE,
};
