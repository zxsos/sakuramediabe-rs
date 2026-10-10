//! 活动中心：`/system/activity/bootstrap`、`/system/notifications*`、
//! `/system/task-runs*`。
//!
//! 对应上游 `src/api/routers/system/activity.py`（**6 个端点**）。
//!
//! # 鉴权挂在每个 handler 上
//!
//! 上游 router 级声明了 `dependencies=[Depends(db_deps), Depends(get_current_user)]`
//! （`activity.py:14-18`），是**六个端点共用**一次鉴权。这里逐个写
//! `CurrentUser` 提取器 —— 与 `routes/config.rs` 同一取舍：靠 router 级 layer
//! 悄悄生效会让「这个端点其实没鉴权」变得看不见。
//!
//! 代价要写清楚：上游是 router 级依赖，本模块是逐 handler。行为等价，
//! 但**新增端点时必须记得写 `CurrentUser`**，忘了编译器不会报错（参数只是
//! 没被使用），而端点会静默变成匿名可调。这是本仓库所有路由模块的共同
//! 约定，不是本模块独有。
//!
//! # 六个端点的形状各不相同
//!
//! | 端点 | 响应 |
//! |---|---|
//! | `GET /system/activity/bootstrap` | 两份分页 + 两个标量的聚合体 |
//! | `GET /system/notifications` | 分页 |
//! | `POST /system/notifications/read` | `{updated_count, unread_count}` |
//! | `POST /system/notifications/read-all` | 同上（共用 resource） |
//! | `GET /system/task-runs/active` | **裸数组**，不分页 |
//! | `GET /system/task-runs` | 分页 |
//!
//! `read` 与 `read-all` 共用一个响应体是上游的选择（`activity.py:49-58`）——
//! 客户端处理「批量点已读」和「全部已读」两条路径可以复用同一段解析。

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use sm_service::system::activity::{
    ActivityBootstrapQuery, ActivityBootstrapService, NotificationService, TaskRunService,
};

use crate::auth::CurrentUser;
use crate::dto::{
    ActivityBootstrapResource, NotificationBatchReadResponse, NotificationReadBatchRequest,
    NotificationResource, TaskRunResource,
};
use crate::error::ErrorResponse;
use crate::extract::{Json as EnvelopeJson, Query as EnvelopeQuery};
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/system/activity/bootstrap",
            get(bootstrap).fallback(method_not_allowed),
        )
        .route(
            "/system/notifications",
            get(list_notifications).fallback(method_not_allowed),
        )
        .route(
            "/system/notifications/read",
            post(mark_read).fallback(method_not_allowed),
        )
        .route(
            "/system/notifications/read-all",
            post(mark_all_read).fallback(method_not_allowed),
        )
        .route(
            "/system/task-runs/active",
            get(list_active_task_runs).fallback(method_not_allowed),
        )
        .route(
            "/system/task-runs",
            get(list_task_runs).fallback(method_not_allowed),
        )
}

/// 首屏聚合。参数与上游 `get_activity_bootstrap`（`activity.py:20-34`）一致。
///
/// 五个参数里只有 `notification_category` 筛通知，其余四个筛任务运行 ——
/// 上游的形状，两份数据没有共同维度。
#[derive(Debug, Deserialize)]
struct BootstrapQuery {
    #[serde(default)]
    notification_category: Option<String>,
    #[serde(default)]
    task_state: Option<String>,
    #[serde(default)]
    task_key: Option<String>,
    #[serde(default)]
    task_trigger_type: Option<String>,
    #[serde(default)]
    task_sort: Option<String>,
}

async fn bootstrap(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<BootstrapQuery>,
) -> Result<Json<ActivityBootstrapResource>, ErrorResponse> {
    let result = ActivityBootstrapService::get_activity_bootstrap(
        state.db(),
        &ActivityBootstrapQuery {
            notification_category: query.notification_category.as_deref(),
            task_state: query.task_state.as_deref(),
            task_key: query.task_key.as_deref(),
            task_trigger_type: query.task_trigger_type.as_deref(),
            task_sort: query.task_sort.as_deref(),
        },
    )
    .await?;
    Ok(Json(ActivityBootstrapResource {
        notifications: map_page(result.notifications, |row| NotificationResource::from(row)),
        unread_count: result.unread_count,
        active_task_runs: result
            .active_task_runs
            .iter()
            .map(TaskRunResource::from)
            .collect(),
        task_runs: map_page(result.task_runs, |row| TaskRunResource::from(row)),
    }))
}

fn default_page() -> i64 {
    1
}

fn default_page_size() -> i64 {
    20
}

/// `GET /system/notifications` 的查询参数。
///
/// ⚠️ `page` / `page_size` **必须内联**，不能抽成结构体再 `#[serde(flatten)]`。
///
/// `#[serde(flatten)]` 会迫使 serde 先把整个 query 缓冲成 `Content`，而
/// `serde_urlencoded` 产出的值**全是字符串**（它的 `deserialize_any` 走
/// `visit_str`）。于是 `Content::Str("1")` 反序列化到 `i64` 必然失败：
///
/// ```text
/// ?page=1  ->  invalid type: string "1", expected i64
/// ```
///
/// 症状很刁：**不带参数时正常**（默认值不经过 `Content`），一带上 `page=` /
/// `page_size=` 就 422。这不是值的问题，是字段类型与 flatten 的组合问题。
/// 本仓其余路由一律内联书写（见 `routes::movies`、`routes::actors`）。
#[derive(Debug, Deserialize)]
struct ListNotificationsQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    /// 分类。非法值 422 `invalid_activity_filter`。
    #[serde(default)]
    category: Option<String>,
    /// 已读状态。**不校验** —— 它是布尔，不是字符串白名单。
    #[serde(default)]
    is_read: Option<bool>,
}

async fn list_notifications(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListNotificationsQuery>,
) -> Result<Json<sm_core::pagination::Paginated<NotificationResource>>, ErrorResponse> {
    let page = NotificationService::new(state.db())
        .list_notifications(
            query.category.as_deref(),
            query.is_read,
            query.page,
            query.page_size,
        )
        .await?;
    Ok(Json(map_page(page, |row| NotificationResource::from(row))))
}

/// `POST /system/notifications/read`
///
/// **不存在的 id 被静默忽略**（仓储层 WHERE 带 `is_read = false`）——
/// 上游同义。客户端传回一串 id，其中一个已被保留期清理掉，不该让整批失败。
async fn mark_read(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(body): EnvelopeJson<NotificationReadBatchRequest>,
) -> Result<Json<NotificationBatchReadResponse>, ErrorResponse> {
    let result = NotificationService::new(state.db())
        .mark_read(&body.ids)
        .await?;
    Ok(Json(result.into()))
}

/// `POST /system/notifications/read-all`
async fn mark_all_read(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<NotificationBatchReadResponse>, ErrorResponse> {
    let result = NotificationService::new(state.db()).mark_all_read().await?;
    Ok(Json(result.into()))
}

/// `GET /system/task-runs/active` —— **裸数组，不分页**。
///
/// 对应上游 `list_active_task_runs`（`activity.py:68-70`）。语义是「现在有
/// 什么在跑」而不是「翻页看历史上跑过什么」——分页会把在跑的任务挤到第二页，
/// 而那正是最需要被看到的那批。
async fn list_active_task_runs(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<TaskRunResource>>, ErrorResponse> {
    let runs = TaskRunService::new(state.db())
        .list_active_task_runs()
        .await?;
    Ok(Json(runs.iter().map(TaskRunResource::from).collect()))
}

/// `GET /system/task-runs` 的查询参数。
///
/// 同 [`ListNotificationsQuery`]：`page` / `page_size` **必须内联**，
/// `#[serde(flatten)]` 配 `serde_urlencoded` 会让带数字参数的请求必 422。
#[derive(Debug, Deserialize)]
struct ListTaskRunsQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    /// 任务状态。非法值 422 `invalid_activity_filter`。
    #[serde(default)]
    state: Option<String>,
    /// 任务键。**区分大小写**，只折叠空白。
    #[serde(default)]
    task_key: Option<String>,
    /// 触发方式。非法值 422 `invalid_activity_filter`。
    #[serde(default)]
    trigger_type: Option<String>,
    /// `字段:方向`，六个合法值见 `TASK_RUN_SORT_FIELDS`。
    /// 非法值 422 `invalid_task_run_sort`（details 回显原始输入与允许值）。
    #[serde(default)]
    sort: Option<String>,
}

async fn list_task_runs(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListTaskRunsQuery>,
) -> Result<Json<sm_core::pagination::Paginated<TaskRunResource>>, ErrorResponse> {
    let page = TaskRunService::new(state.db())
        .list_task_runs(
            query.state.as_deref(),
            query.trigger_type.as_deref(),
            query.task_key.as_deref(),
            query.sort.as_deref(),
            query.page,
            query.page_size,
        )
        .await?;
    Ok(Json(map_page(page, |row| TaskRunResource::from(row))))
}

/// 把 `Paginated<Row>` 换成 `Paginated<Dto>`，映射由闭包给出。
///
/// # 为什么用闭包而不是 `AsRef`
///
/// 两个 `Paginated`（`sm_db` 与 `sm_core` 各有一个）字段相同但没有 `From`
/// 转换，而 `From<&Row> for Dto` 产生的是**值**、借不出 `&Dto` —— 写
/// `AsRef` impl 就得 `unimplemented!()` 或另存一份缓存，两条都是埋雷。
///
/// # 为什么调用点写 `|row| Dto::from(row)` 而不是直接传 `Dto::from`
///
/// `impl Fn(&Row) -> Dto` 是**高阶生命周期**约束，而函数项
/// `NotificationResource::from` 的 `From<&NotificationResource>` 实现不带
/// 那个界 —— 直接传会报 `implementation of Fn is not general enough`
/// （一个能过但难懂的报错）。包一层闭包把生命周期固定住。
fn map_page<Row, Dto>(
    page: sm_core::pagination::Paginated<Row>,
    map: impl Fn(&Row) -> Dto,
) -> sm_core::pagination::Paginated<Dto> {
    sm_core::pagination::Paginated::new(
        page.items.iter().map(map).collect(),
        page.page,
        page.page_size,
        page.total,
    )
}
