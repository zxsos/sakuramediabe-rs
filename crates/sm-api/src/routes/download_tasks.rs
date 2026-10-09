//! `/download-tasks*` 与 `POST /download-requests` —— 任务台账四个端点。
//!
//! # 与上游 `src/api/routers/transfers/downloads.py` 的对应
//!
//! | 上游端点 | 状态码 | 状态 |
//! |---|---|---|
//! | `POST /download-requests` | 200 | ⏳ 等 provider seam（要提交给下载器）|
//! | `GET /download-tasks` | 200（`PageResponse`） | ✅ 已接 |
//! | `DELETE /download-tasks/{task_id}` | **204** | ⏳ 等 provider seam（服务层 `delete_task`）|
//! | `POST /download-tasks/{task_id}/import` | **202 Accepted** | ✅ 已接（**202 路径依赖 `import_task::enqueue`，它仍是 `todo!()`**；两道门的 404/422/409 可用）|
//!
//! # 响应形状 = 上游 `DownloadTaskResource`，一个字段都不多不少
//!
//! `{id, client_id, movie_number, name, remote_id, state, progress, import_status,
//! movie_title, movie_cover, movie_thin_cover, created_at, updated_at,
//! import_status_label}`（`schema/transfers/downloads.py:112-131`）。
//!
//! ⚠️ 骨架期这里是**自造形状**（`{id: i64, movie_number: String, state,
//! progress: Option<f64>, client_id: Option<i32>, created_at}`），缺了
//! `name` / `remote_id` / `import_status` / `movie_title` / 两个封面 ——
//! 客户端的 `DownloadTaskDto` 会把这些全解析成 `null`。见
//! `sm_service::transfers::download_task` 模块文档里的对照表。
//!
//! # 最重要的一处：`DELETE` 的**两步确认**
//!
//! 上游：
//!
//! ```python
//! if delete_files and not confirm_delete_files:
//!     raise ApiError(
//!         422,
//!         "download_task_delete_confirmation_required",
//!         "Deleting downloaded files requires explicit confirmation",
//!         {"task_id": task_id},
//!     )
//! ```
//!
//! 三个要素都要照抄：
//!
//! | 要素 | 值 | 照抄的理由 |
//! |---|---|---|
//! | 状态码 | **422**（不是 400、不是 409） | 上游用 422 |
//! | code | `download_task_delete_confirmation_required` | 客户端要靠它区分「要二次确认」与其他失败 |
//! | details | `{"task_id": task_id}` | 客户端要能告诉用户**是哪个任务**要确认 |
//!
//! # 为什么这个设计值得单独写一段
//!
//! `delete_files=true` 会**真删磁盘文件**。一个手滑或一个被构造的请求就能让
//! 下载好的影片消失，而任务台账里可能还留着记录。两个布尔参数把「我确认」
//! 与「我要删」分开，客户端必须发两次请求 —— 第一次 `delete_files=true`
//! 拿到 422，弹确认框；用户确认后再带 `confirm_delete_files=true` 重发。
//!
//! **不要「优化」成一步。** 合并成单参数等于取消确认，而确认的价值恰恰在于
//! 「客户端必须先收到拒绝才知道要弹框」。
//!
//! # `state` 是**重复 query 参数**，不是 CSV
//!
//! `GET /download-tasks` 的 `state: list[str] | None = Query(default=None)` ——
//! FastAPI 对 `list[str]` 会展开成重复参数：`?state=downloading&state=failed`。
//!
//! ⚠️ 所以这条路由**不能**用 [`crate::extract::Query`]：它走
//! `serde_urlencoded`，而后者把「字段期待序列」直接转发成 `visit_str` ——
//! 重复键与单值**都会 422**。必须用 [`HtmlFormQuery`]（`serde_html_form`）。
//!
//! **这与本仓库别处的 CSV 约定不同**（`actor_ids`、`movie_ids` 都是 CSV）。
//! 两处形态都照上游，不要「统一」成一种 —— 统一了就有一边对不上。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;

use sm_core::pagination::Paginated;
use sm_service::transfers::download_task::DownloadTaskService;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::extract::{HtmlFormQuery, Query as EnvelopeQuery};
use crate::routes::method_not_allowed;
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

use sm_service::transfers::download_task::{ensure_delete_confirmed, DownloadTaskListItem};
/// 删除参数与「两步确认」的校验都在 service 层，**不在路由层复制一份**
/// （`handoff.md` 纪律第 7 条）。
pub use sm_service::transfers::download_task::{
    DeleteTaskQuery, DownloadTaskImportResponse, ListTasksQuery,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/download-requests",
            post(create_download_request).fallback(method_not_allowed),
        )
        .route(
            "/download-tasks",
            get(list_download_tasks).fallback(method_not_allowed),
        )
        .route(
            "/download-tasks/{task_id}",
            delete(delete_download_task).fallback(method_not_allowed),
        )
        .route(
            "/download-tasks/{task_id}/import",
            post(trigger_download_task_import).fallback(method_not_allowed),
        )
}

/// 任务条目（响应体）。上游 `DownloadTaskResource`
/// （`schema/transfers/downloads.py:112-131`）。
///
/// 字段顺序与上游一致，`import_status_label` 排在最后 —— 上游它是
/// `@computed_field`（定义在 `updated_at` 之后）。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTaskResource {
    pub id: i32,
    pub client_id: i32,
    /// 可为空：任务可以早于影片入库（搜索结果先到是正常流程）。
    pub movie_number: Option<String>,
    pub name: String,
    pub remote_id: String,
    /// 远端状态。取值由 provider 决定，**不要建模成 enum**。
    pub state: String,
    /// 必填（上游 `float`）。
    pub progress: f64,
    pub import_status: String,
    /// 影片标题（trim 后为空则 `null`）。
    pub movie_title: Option<String>,
    pub movie_cover: Option<crate::dto::ImageResource>,
    pub movie_thin_cover: Option<crate::dto::ImageResource>,
    pub created_at: Option<chrono::NaiveDateTime>,
    pub updated_at: Option<chrono::NaiveDateTime>,
    /// computed：导入状态的中文说明（未知取值回退原值）。
    pub import_status_label: String,
}

impl DownloadTaskResource {
    /// 由服务层投影构造。`secret` / `now` 用于给封面签名 —— 与
    /// [`crate::dto::sign_image_origin`] 同一个理由（签名要密钥，而密钥是
    /// 运行期配置，序列化时拿不到）。
    fn from_item(item: DownloadTaskListItem, secret: &str, now: i64) -> Self {
        // 先算 label：它借用 `item.import_status`，而下面要把该字段 move 进结构体。
        let import_status_label = crate::dto::describe_import_status(&item.import_status);
        let signed = |image: &sm_db::Image| crate::dto::ImageResource {
            id: image.id,
            origin: crate::dto::sign_image_origin(secret, &image.origin, now),
        };
        Self {
            id: item.id,
            client_id: item.client_id,
            movie_number: item.movie_number,
            name: item.name,
            remote_id: item.remote_id,
            state: item.state,
            progress: item.progress,
            import_status: item.import_status,
            movie_title: item.movie_title,
            movie_cover: item.movie_cover.as_ref().map(signed),
            movie_thin_cover: item.movie_thin_cover.as_ref().map(signed),
            created_at: item.created_at,
            updated_at: item.updated_at,
            import_status_label,
        }
    }
}

/// `GET /download-tasks`
///
/// 查询参数走 [`HtmlFormQuery`]（**重复键**，见模块文档）；分页参数、
/// `state` / `sort` 的白名单与错误码全在服务层。
async fn list_download_tasks(
    _user: CurrentUser,
    State(state): State<AppState>,
    HtmlFormQuery(query): HtmlFormQuery<ListTasksQuery>,
) -> Result<Json<Paginated<DownloadTaskResource>>, ErrorResponse> {
    let secret = signing_secret(&state)?;
    let now = now_seconds();

    let page = DownloadTaskService::new(state.db())
        .list_tasks(&query)
        .await?;
    let items = page
        .items
        .into_iter()
        .map(|item| DownloadTaskResource::from_item(item, &secret, now))
        .collect();
    // `page` / `page_size` 由服务层按其校验过的分页参数回显（缺省 1 / 20）。
    Ok(Json(Paginated::new(
        items,
        page.page,
        page.page_size,
        page.total,
    )))
}

/// `DELETE /download-tasks/{task_id}`
///
/// 上游用 `Query` 参数而非请求体 —— 因为它要能被浏览器与 curl 直接调。
/// 所以这里注册成 `delete(handler)` 而非 `delete(handler, body)`。
async fn delete_download_task(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(task_id): Path<i32>,
    EnvelopeQuery(query): EnvelopeQuery<DeleteTaskQuery>,
) -> Result<StatusCode, ErrorResponse> {
    // 顺序不能反：先查确认，再动手。确认过了才允许碰磁盘。
    //
    // 契约本体在 service 层（`sm_service::transfers::download_task::
    // ensure_delete_confirmed`）—— 这里是**调用**它而不是重写一遍，
    // 因为错误码、状态码与 `details.task_id` 三者是一个整体，两处各写一份
    // 迟早会漂移。
    ensure_delete_confirmed(task_id, &query)?;
    todo!("阶段二：接任务台账删除（要调下载器 provider 删远端任务；成功 204 不带 body）")
}

/// `POST /download-requests` —— 创建下载请求。
///
/// **200**（不是 201、不是 202）—— 上游如此。任务本身是异步的，但这个响应
/// 返回的是「已登记的候选 + client id」，不是任务句柄。
async fn create_download_request(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接下载请求创建（注意 200 而非 201/202）")
}

/// `POST /download-tasks/{task_id}/import` —— **202 Accepted**。
///
/// 202 是对的：导入是**长任务**，立即返回结果不可能。响应体给出任务句柄供轮询。
///
/// 三道门（404 / 422 / 409）在 service 里，**不经过** enqueue；202 那条会真的
/// 建出 TaskRun。
///
/// ⚠️ 但任务**跑不起来**：`library_import` 的 worker 处理器还没注册，
/// 领到之后会以 `NoHandler` 判失败（见
/// `sm_service::transfers::import_task` 的模块文档）。
async fn trigger_download_task_import(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(task_id): Path<i32>,
) -> Result<(StatusCode, Json<DownloadTaskImportResponse>), ErrorResponse> {
    let accepted = DownloadTaskService::new(state.db())
        // `allowed_statuses` 缺省 = 服务层的 `DEFAULT_IMPORTABLE_STATUSES`
        // （上游路由不传这一项）。
        .trigger_import(task_id, None)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(accepted)))
}
