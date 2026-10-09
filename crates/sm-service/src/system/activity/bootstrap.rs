//! 首屏聚合查询，对应上游 `activity/bootstrap.py`（49 行）。
//!
//! # 为什么首屏要一个端点而不是四个
//!
//! 活动中心首屏要同时显示：未读数、最近通知、进行中的任务、最近任务运行。
//! 分成四个端点的话客户端要发四个请求，而**这四份数据必须来自同一时刻** ——
//! 「未读 3 条」与列表里躺着 2 条对不上时，用户点进去会发现数字变了。
//!
//! 上游用 `isolation_level="repeatable read"` 的事务把四份读固定在一个快照里
//! （`bootstrap.py:12-15`）。
//!
//! ## 一处**未对齐**的地方：没有包在同一个快照里
//!
//! 这里的四个查询各自开自己的事务（`paged_list!` 宏内部走 `in_snapshot_tx`），
//! **四段之间可能跨时间点**。
//!
//! 为什么没照做：要包成一个快照，四段查询必须共用同一个连接，而仓储方法的
//! 签名是 `&self`（内部自己从 `self.pool` 取连接），没有接收外部连接的变体。
//! 改签名会波及全部 `paged_list!` 调用方 —— 那是本批之外的事。
//!
//! **症状**：在两次查询之间恰好完成一次任务收口时，客户端会看到
//! `unread_count` 与 `notifications` 对不上。表现为红点数差 1，且**刷新后自愈**。
//! 上游那四个独立端点有同样的窗口，只是首屏一次性取数的窗口更小。
//!
//! # 五个筛选参数只有一个作用在通知上
//!
//! `notification_category` 只筛通知；`task_state` / `task_key` /
//! `task_trigger_type` / `task_sort` 只筛任务运行。这是上游的形状 ——
//! 两份数据没有共同维度，硬凑一个 `category` 给两边会让一半参数名不副实。

use sm_core::pagination::Paginated;
use sm_db::system::activity::{BackgroundTaskRun, SystemNotification};

use crate::error::ServiceError;

use super::notifications::NotificationService;
use super::task_runs::TaskRunService;

/// 首屏每页的条数。对应上游 `ACTIVITY_BOOTSTRAP_PAGE_SIZE = 20`。
pub const ACTIVITY_BOOTSTRAP_PAGE_SIZE: i64 = 20;

/// 首屏聚合查询的参数。
#[derive(Debug, Clone, Default)]
pub struct ActivityBootstrapQuery<'a> {
    pub notification_category: Option<&'a str>,
    pub task_state: Option<&'a str>,
    pub task_key: Option<&'a str>,
    pub task_trigger_type: Option<&'a str>,
    pub task_sort: Option<&'a str>,
}

/// 首屏聚合结果。
#[derive(Debug, Clone)]
pub struct ActivityBootstrap {
    pub notifications: Paginated<SystemNotification>,
    pub unread_count: i64,
    pub active_task_runs: Vec<BackgroundTaskRun>,
    pub task_runs: Paginated<BackgroundTaskRun>,
}

/// 首屏聚合服务。
pub struct ActivityBootstrapService;

impl ActivityBootstrapService {
    /// 取首屏。
    ///
    /// 四个查询**顺序**与上游一致（通知 → 任务运行 → 未读数 → 进行中），
    /// 便于和上游日志逐条对照。
    pub async fn get_activity_bootstrap(
        db: &sm_db::Db,
        query: &ActivityBootstrapQuery<'_>,
    ) -> Result<ActivityBootstrap, ServiceError> {
        let notifications = NotificationService::new(db)
            .list_notifications(
                query.notification_category,
                None,
                1,
                ACTIVITY_BOOTSTRAP_PAGE_SIZE,
            )
            .await?;
        let task_runs = TaskRunService::new(db)
            .list_task_runs(
                query.task_state,
                query.task_trigger_type,
                query.task_key,
                query.task_sort,
                1,
                ACTIVITY_BOOTSTRAP_PAGE_SIZE,
            )
            .await?;
        let unread_count = NotificationService::new(db).unread_count().await?;
        let active_task_runs = TaskRunService::new(db).list_active_task_runs().await?;

        Ok(ActivityBootstrap {
            notifications,
            unread_count,
            active_task_runs,
            task_runs,
        })
    }
}
