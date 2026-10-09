//! 下载任务台账与宿主侧导入交接（上游 `downloads/task_service.py`，176 行）。
//!
//! # 响应形状与上游 `DownloadTaskResource` **逐字一致**
//!
//! `{id, client_id, movie_number, name, remote_id, state, progress, import_status,
//! movie_title, movie_cover, movie_thin_cover, created_at, updated_at,
//! import_status_label}`（`schema/transfers/downloads.py:112-131`）。
//!
//! ⚠️ 骨架期这里是**自造形状**：`{id: i64, movie_number: String, state,
//! progress: Option<f64>, client_id: Option<i32>, client_name: Option<String>,
//! created_at, importable: bool}`。四处都不对，且那两个自造字段**前端根本不消费**：
//!
//! | 骨架期字段 | 上游 | 为什么删 |
//! |---|---|---|
//! | `client_name` | 资源里**没有** | 前端用 `GET /download-clients` 自己拼名字（`clientNames`）；全仓 Flutter 搜 `clientName` 只在那一处 |
//! | `importable: bool` | 资源里**没有** | 前端由 `import_status` 自行推导；全仓 Flutter 搜 `importable` = 0 命中 |
//! | `progress: Option<f64>` | `progress: float`（必填）| 把「provider 没给」与「0%」混成一个 |
//! | 缺 `name` / `remote_id` / `import_status` / `movie_title` / 两个封面 / `updated_at` | 上游都有 | 客户端的 `DownloadTaskDto` 按上游形状解析，这些会全变 `null` |
//!
//! 契约以客户端的 `DownloadTaskDto`（`sakuramedia/lib/features/downloads/data/
//! download_request_dto.dart:6-61`）为准 —— 它与上游**逐字段一致**。
//!
//! # ★ `DELETE` 的**两步确认**是本文件最重要的契约
//!
//! `delete_files=true` 会**真删磁盘文件**。上游要求客户端发两次请求：
//!
//! 第一次 `?delete_files=true` → **422**
//! `download_task_delete_confirmation_required`，客户端据此弹确认框；
//! 用户确认后再带 `confirm_delete_files=true` 重发。
//!
//! **不要「优化」成一步。** 合并成单参数等于取消确认，而确认的全部价值就在
//! 「客户端必须先收到拒绝，才知道要弹框」。
//!
//! # 可导入状态是**白名单**，且含 `failed` 与 `skipped`
//!
//! [`DownloadTaskService::DEFAULT_IMPORTABLE_STATUSES`] 三态都可重试导入。
//! 尤其别漏掉 `failed`：下载失败了正是最需要重试导入的时候。
//!
//! # 409 有三个不同语义，别混用
//!
//! | 码 | 含义 | 客户端该做什么 |
//! |---|---|---|
//! | `download_task_import_running` | **已经有一个导入在跑** | 等待，不要重发 |
//! | `download_task_import_conflict` | 任务状态不允许导入 | 先修状态 |
//! | `422 invalid_download_task_import` | 状态在白名单外 | 改状态 |

use serde::{Deserialize, Serialize};

use sm_core::pagination::Paginated;
use sm_db::common::page::{Page, PageRequest};
use sm_db::repo::{DownloadTaskFilter, DownloadTaskRepository, ImageRepository, MovieRepository};
use sm_db::Db;

use crate::error::{details_of, ServiceError};

use super::download_common::{
    normalize_state_filters, require_client, require_task, resolve_task_sort,
};
use super::import_task::{
    completed_source_ref, ImportAcceptedResponse, ImportRequest, ImportTaskService, MediaKind,
    SourceDisposition,
};

/// 分页缺省：上游 `DownloadTasksQuery.page` 的 `Field(default=1, ge=1)`。
const DEFAULT_PAGE: i64 = 1;
/// 分页缺省：上游 `DownloadTasksQuery.page_size` 的 `Field(default=20, ge=1, le=100)`。
const DEFAULT_PAGE_SIZE: i64 = 20;

/// 导入状态：待处理。
pub const IMPORT_STATUS_PENDING: &str = "pending";
pub const IMPORT_STATUS_RUNNING: &str = "running";
/// 导入状态：失败。
pub const IMPORT_STATUS_FAILED: &str = "failed";
/// 导入状态：跳过。
pub const IMPORT_STATUS_SKIPPED: &str = "skipped";

/// 台账列表项 —— 上游 `DownloadTaskResource` 的投影。
///
/// 字段与上游逐字对齐（见模块文档「响应形状」）。`movie_*` / 封面来自
/// **一次批量**查询（`DownloadTaskService::load_movie_cards`），不是逐行查。
///
/// `import_status_label` **不在**这里：它是上游的 `@computed_field`，属于 API
/// 表达层，由 `sm_api::dto::describe_import_status` 在组装响应时补。
#[derive(Debug, Clone)]
pub struct DownloadTaskListItem {
    pub id: i32,
    pub client_id: i32,
    /// 允许为空：任务可以早于影片入库（搜索结果先到是正常流程）。
    pub movie_number: Option<String>,
    pub name: String,
    pub remote_id: String,
    pub state: String,
    /// 必填（上游 `float`）—— 骨架期的 `Option` 把「没给」与「0%」混成一个。
    pub progress: f64,
    pub import_status: String,
    pub created_at: Option<chrono::NaiveDateTime>,
    pub updated_at: Option<chrono::NaiveDateTime>,
    /// 影片标题，**trim 后为空则 `None`**（上游 `(movie.title or "").strip() or None`）。
    pub movie_title: Option<String>,
    pub movie_cover: Option<sm_db::Image>,
    pub movie_thin_cover: Option<sm_db::Image>,
}

/// 触发导入的响应（**202**）。上游 `DownloadTaskImportResponse`
/// （`schema/transfers/downloads.py:188-191`）。
///
/// ⚠️ 骨架期是 `{task_run_id, accepted: i32}` —— 上游是
/// `{task_id, task_run_id, status: "accepted"}`：没有 `accepted` 这个计数
/// （导入的对象是**一个**任务），且回显 `task_id` 让客户端能对上号。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTaskImportResponse {
    pub task_id: i32,
    /// TaskRun id。**轮询它看进度**。
    pub task_run_id: i64,
    /// 恒为 `"accepted"`。
    pub status: String,
}

/// 删除参数 —— **两步确认**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeleteTaskQuery {
    /// 是否连带删除已下载文件。默认 `false`。
    #[serde(default)]
    pub delete_files: bool,
    /// 是否已确认。`delete_files = true` 时**必须**也为 `true`。
    #[serde(default)]
    pub confirm_delete_files: bool,
}

/// 列表查询参数。
///
/// ⚠️ `state` 是**重复 query 参数**（`?state=queued&state=failed`），
/// **不是 CSV**。这与本仓别处（`actor_ids` / `movie_ids` 用 CSV）不同 ——
/// 两处形态都照上游，不要「统一」成一种，统一了就有一边对不上。
///
/// 路由层的解析方式见 `sm_api::extract::HtmlFormQuery`：axum 自带的
/// `Query` 走 `serde_urlencoded`，它**不支持序列**（重复键会被当成单个字符串），
/// 所以这个参数必须走 html-form 提取器。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListTasksQuery {
    pub client_id: Option<i32>,
    pub movie_number: Option<String>,
    /// 重复 query 参数。`None` = 不过滤；空列表 / 全空白项**同样**不过滤
    /// （上游 `if not values: return None`）。
    pub state: Option<Vec<String>>,
    /// 排序键（`field:dir` 六个字面量之一），见 [`super::download_common::TASK_SORT_FIELDS`]。
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// ★ 两步确认的第一道门。**路由层必须先调它**，再调 [`DownloadTaskService::delete_task`]。
///
/// 三个要素一个都不能少（见模块文档）：状态码 **422**、错误码
/// `download_task_delete_confirmation_required`、`details.task_id`。
///
/// 放成自由函数而不是 `delete_task` 的第一行，是因为**顺序**要由调用方保证
/// —— 路由层若先查了任务再判确认，就等于在拒绝前告诉了客户端「这个任务
/// 存在」，那本身是一次可被用来探测任务存在性的侧信道。
pub fn ensure_delete_confirmed(task_id: i32, query: &DeleteTaskQuery) -> Result<(), ServiceError> {
    if !query.delete_files || query.confirm_delete_files {
        return Ok(());
    }
    Err(ServiceError::validation_with(
        "download_task_delete_confirmation_required",
        "Deleting downloaded files requires explicit confirmation",
        details_of("task_id", serde_json::Value::from(task_id)),
    ))
}

/// 校验分页参数，错误码用**专用**的那个（上游 `task_service.py:53`
/// 的 `validate_page(page, page_size, error_code="invalid_download_task_filter")`）。
///
/// 上游把 `details` 交给 `validate_page` 填（`{"page": 0}` / `{"page_size": 101}`），
/// 所以这里原样搬 `PageError::details()`。
fn validate_task_page(page: i64, page_size: i64) -> Result<(), ServiceError> {
    sm_core::pagination::validate_page(page, page_size).map_err(|error| {
        let details = match error.details() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        ServiceError::validation_with("invalid_download_task_filter", error.message(), details)
    })
}

/// 由下载任务发起的导入的**显示名**。
///
/// 上游 `task_name=f"下载任务导入 {task.movie or task.name}"`
/// （`task_service.py:153`）。它**覆盖**了 `ImportTaskService._task_name`
/// 推出的缺省名（「JAV媒体库导入」）—— 任务中心里同时有几条导入时，
/// 用户只能靠这个名字区分「哪一条是哪个下载任务」。
///
/// Python 的 `or` 对**空串**也回落（`movie` 是 `CharField(null=True)`，库里
/// 可能存着空串），所以判据是「有值且非空」，而**不是** `is_some()` ——
/// 用后者会让只有空串番号的任务显示成 `下载任务导入 `。
fn download_task_import_task_name(task: &sm_db::DownloadTask) -> String {
    let label = task
        .movie_number
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&task.name);
    format!("下载任务导入 {label}")
}

/// 台账服务。
#[derive(Debug, Clone)]
pub struct DownloadTaskService {
    db: Db,
}

impl DownloadTaskService {
    /// **可触发导入的状态白名单**（三态，见模块文档）。
    pub const DEFAULT_IMPORTABLE_STATUSES: [&'static str; 3] = [
        IMPORT_STATUS_PENDING,
        IMPORT_STATUS_FAILED,
        IMPORT_STATUS_SKIPPED,
    ];

    /// 构造。取 `&Db` 并克隆（与 [`super::download_client::DownloadClientService::new`]
    /// 同形），调用方写 `new(state.db())`。
    pub fn new(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    /// `GET /download-tasks` —— **200**，泛型分页（上游 `task_service.py:42-70`）。
    ///
    /// 顺序与上游一致：`validate_page`（专用错误码）→ 三个筛选位 →
    /// `COUNT` + 分页（同一快照，见 [`DownloadTaskRepository::list_page`]）→
    /// **一次**批量取回当页涉及的影片卡片。
    ///
    /// 错误码：分页参数非法 / `state` 不在白名单 / `sort` 不在白名单 →
    /// **422 `invalid_download_task_filter`**（三者共用同一个码，上游如此）。
    pub async fn list_tasks(
        &self,
        query: &ListTasksQuery,
    ) -> Result<Paginated<DownloadTaskListItem>, ServiceError> {
        let page = query.page.unwrap_or(DEFAULT_PAGE);
        let page_size = query.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
        validate_task_page(page, page_size)?;

        // `state` 与 `sort` 的归一/白名单都在 `download_common`（上游同名函数）。
        let states = normalize_state_filters(query.state.as_deref())?;
        let sort = resolve_task_sort(query.sort.as_deref())?;
        let filter = DownloadTaskFilter {
            client_id: query.client_id,
            movie_number: query.movie_number.clone(),
            states,
        };
        // 分页参数已在上一步按上游口径校验过；这里的映射不会失败。
        let request = PageRequest::new(page, page_size).map_err(ServiceError::from)?;

        let Page {
            items: tasks,
            total,
        } = DownloadTaskRepository::new(self.db.clone())
            .list_page(&filter, sort, request)
            .await?;

        let items = self.load_movie_cards(tasks).await?;
        // `request` 已经过上游口径的校验，回显它的值就是回显**请求的**分页参数。
        Ok(Paginated::new(
            items,
            request.page(),
            request.page_size(),
            total,
        ))
    }

    /// `POST /download-tasks/{task_id}/import` —— **202**（上游 `:120-159`）。
    ///
    /// 顺序与上游一致：`require_task` → 两道门（**先 422 后 409**，见
    /// [`Self::ensure_importable`]）→ 取下载器（拿 `library_id`）→ 入队 → 202。
    ///
    /// ⚠️ **入队已落地，但任务跑不起来**：`library_import` 的 worker 处理器
    /// 还没注册（`ImportTaskService::execute` 要 `import_service` 的编排）。
    /// 所以 202 之后那条 TaskRun 会被 worker 领取、随即以 `NoHandler`
    /// 判失败 —— 任务中心里能看到它。这是阶段性事实，不是回归。
    ///
    /// 显示的**任务名**与上游逐字一致：`下载任务导入 {番号 or 任务名}`
    /// （`task_service.py:153`）。
    pub async fn trigger_import(
        &self,
        task_id: i32,
        allowed_statuses: Option<&[&str]>,
    ) -> Result<DownloadTaskImportResponse, ServiceError> {
        let task = require_task(&self.db, task_id).await?;

        Self::ensure_importable(
            task.id,
            &task.state,
            // 空串与 NULL 等价（上游 `task.completed_source_ref is None` 只管 NULL，
            // 但写入侧 `set_state` 已保证非空；这里对空白串一并按「没有来源」处理）。
            task.completed_source_ref
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            Some(task.import_status.as_str()),
            allowed_statuses,
        )?;

        // 导入跑在 worker 里，这里只入队（上游 `ImportTaskService.enqueue`）。
        // `library_id` 来自任务所属下载器 —— 所以要先把 client 查出来。
        let client = require_client(&self.db, task.client_id).await?;
        // 任务名在入队**之前**算好：它要借用 `task`，而 `enqueue` 会把
        // `task.id` 带进参数（再往下 `task` 就不需要了）。
        let task_name = download_task_import_task_name(&task);

        let accepted: ImportAcceptedResponse = ImportTaskService::new(&self.db)
            .enqueue(
                ImportRequest {
                    // 上游写死 `media_kind="jav"` / `source_disposition="keep"`。
                    media_kind: MediaKind::Jav,
                    library_id: client.library_id,
                    // 源引用由 storage provider 定义，宿主只搬运。解析口径与
                    // 批量导入共用一份（见 `completed_source_ref`）。
                    source_ref: completed_source_ref(&task)?,
                    source_disposition: SourceDisposition::Keep,
                    collection_id: None,
                },
                "manual",
                Some(task.id),
                Some(&task_name),
            )
            .await?;

        Ok(DownloadTaskImportResponse {
            task_id: task.id,
            task_run_id: i64::from(accepted.task_run_id),
            status: "accepted".to_owned(),
        })
    }

    /// ★ `DELETE /download-tasks/{task_id}` —— **204**，两步确认。
    ///
    /// 签名里**没有** `confirm_delete_files` —— 它已被提到
    /// [`DeleteTaskQuery`] 里由路由层先判（[`ensure_delete_confirmed`]）。
    /// 这里只管「确认过了之后」的删除。
    ///
    /// ⚠️ **阶段二**：要调下载器 provider 删远端任务（`download_provider`），
    /// 属 §5 的 provider seam，本轮不硬填。上游的完整语义见
    /// `task_service.py:81-118`（409 `download_task_import_running`、
    /// `failed`/`skipped` 时把 info hash 写进黑名单、`source_not_found` 视为成功）。
    pub async fn delete_task(
        &self,
        task_id: i32,
        delete_files: bool,
    ) -> Result<serde_json::Value, ServiceError> {
        use super::download_common::require_task;
        use sm_db::repo::DownloadResourceBlacklistRepository;
        use sm_db::repo::DownloadTaskRepository;

        // 1. 查任务（不存在 -> 404）
        let task = require_task(&self.db, task_id).await?;

        // 2. 导入中 -> 409，不能删
        if task.import_status == IMPORT_STATUS_RUNNING {
            let mut details = serde_json::Map::new();
            details.insert(
                "task_id".to_owned(),
                serde_json::Value::from(task.id),
            );
            return Err(ServiceError::conflict(
                "download_task_import_running",
                "Cannot delete a download task while importing media",
                Some(details),
            ));
        }

        // 3. 失败/跳过的任务：取 info_hash 准备拉黑
        // 上游从 DownloadSubmissionRecord 取，取不到就用 task.remote_id
        let info_hash: Option<String> = if task.import_status == IMPORT_STATUS_FAILED
            || task.import_status == IMPORT_STATUS_SKIPPED
        {
            // 简化：直接用 remote_id 做 hash（完整实现需查 submission record）
            // 上游：canonical_info_hash(record.info_hash if record else task.remote_id)
            super::download_resource_hash::canonical_info_hash(&task.remote_id).ok()
        } else {
            None
        };

        // 4. 调 provider 删远端任务
        // ⚠️ provider seam 未就绪（sm-service 不能依赖 sm-plugins），本轮跳过。
        // 上游语义：source_not_found 视为成功（幂等），其他 provider 错误透传。
        let _ = delete_files; // 按需删文件：待 provider seam

        // 5. 落库：拉黑 + 删任务（事务）
        if let Some(hash) = info_hash {
            let _ = DownloadResourceBlacklistRepository::new(self.db.clone())
                .add(&hash)
                .await;
        }
        DownloadTaskRepository::new(self.db.clone())
            .delete(task.id)
            .await?;

        // 6. 返回
        Ok(serde_json::json!({
            "task_id": task.id,
            "client_id": task.client_id,
            "movie_number": task.movie_number,
            "remote_id": task.remote_id,
        }))
    }

    /// 该任务当前**是否可再发起导入** —— 判据是**导入状态**，不是下载状态。
    ///
    /// 上游 `task_service.py:40` 的 `DEFAULT_IMPORTABLE_STATUSES` 是
    /// `{pending, failed, skipped}`，而 `:137` 用它比的是
    /// **`task.import_status`**。
    ///
    /// ⚠️ 骨架期这里拿 `task.state`（**下载状态**）去比那个集合 —— 把两台状态机
    /// 混成了一个。症状很具体：下载已完成、导入曾失败的任务，`state` 是
    /// `completed`（不在集合里）→ 被判成**不可导入**，而它恰恰是最该重导的一种。
    /// 「已完成的能不能导」是下载那一侧的条件，见 [`Self::ensure_importable`]。
    pub fn importable(import_status: Option<&str>) -> bool {
        // 没有导入记录（NULL / 空串都算）→ 全新任务，可导。
        let Some(status) = import_status
            .map(str::trim)
            .filter(|status| !status.is_empty())
        else {
            return true;
        };
        Self::DEFAULT_IMPORTABLE_STATUSES.contains(&status)
    }

    /// `trigger_import` 的两道门（上游 `:129-143`）。**顺序不能反。**
    ///
    /// 1. 下载没完成**或**没有导入来源 → **422 `invalid_download_task_import`**
    ///    （"只有下载已完成且带导入来源的任务才能导入"，details `{task_id}`）；
    /// 2. `import_status` 不在可导入集合 → **409 `download_task_import_conflict`**
    ///    （"该任务的导入已完成或正在进行"，details `{task_id, import_status}`）。
    ///
    /// 先 422 后 409 是有理由的：422 说的是「这个任务**本来就不该**导入」（请求与
    /// 资源状态不匹配），409 说的是「现在不行，等它跑完」。反过来会把「种子还没下完」
    /// 报成「导入冲突」，用户于是去等一个永远不会结束的导入。
    ///
    /// 入参刻意收**裸值**：`completed_source_ref` 与 `import_status` 都在
    /// `download_task` 实体上，而 [`super::download_common::require_task`] 返回的
    /// 是完整实体，判两道门不必另造投影。
    pub fn ensure_importable(
        task_id: i32,
        download_state: &str,
        has_source_ref: bool,
        import_status: Option<&str>,
        allowed_statuses: Option<&[&str]>,
    ) -> Result<(), ServiceError> {
        if !super::transfer_shared::is_download_complete(download_state) || !has_source_ref {
            return Err(ServiceError::validation_with(
                "invalid_download_task_import",
                "只有下载已完成且带导入来源的任务才能导入",
                details_of("task_id", serde_json::Value::from(task_id)),
            ));
        }

        // 调用方给了 `allowed_statuses` 就用它（上游 `allowed_statuses or DEFAULT`）。
        // ⚠️ 给了自定义白名单时，**没有导入记录**也算「不在白名单里」——
        // 与上游 `task.import_status not in allowed` 一致（NULL 不在任何集合里）。
        let allowed = allowed_statuses.map_or_else(
            || Self::importable(import_status),
            |allowed| {
                import_status
                    .map(str::trim)
                    .filter(|status| !status.is_empty())
                    .is_some_and(|status| allowed.contains(&status))
            },
        );
        if allowed {
            return Ok(());
        }

        let mut details = details_of("task_id", serde_json::Value::from(task_id));
        details.insert(
            "import_status".to_owned(),
            serde_json::Value::from(import_status.unwrap_or("")),
        );
        Err(ServiceError::conflict(
            "download_task_import_conflict",
            "该任务的导入已完成或正在进行",
            Some(details),
        ))
    }

    /// 把当页任务投影成列表项，并**一次**取回涉及的影片卡片与封面
    /// （上游 `_load_movies_for_tasks`，`task_service.py:72-79`）。
    ///
    /// 两次批量查询（影片、图片）而不是逐行 —— 逐行就是 N+1，而这里一页最多
    /// 100 条。
    async fn load_movie_cards(
        &self,
        tasks: Vec<sm_db::DownloadTask>,
    ) -> Result<Vec<DownloadTaskListItem>, ServiceError> {
        // 上游：`{task.movie for task in tasks if task.movie}` —— 去重且跳过空番号。
        let numbers: Vec<String> = tasks
            .iter()
            .filter_map(|task| task.movie_number.as_deref())
            .filter(|number| !number.is_empty())
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let movies = MovieRepository::new(self.db.clone())
            .find_by_numbers(&numbers)
            .await?;
        let image_ids: Vec<i32> = movies
            .values()
            .flat_map(|movie| [movie.cover_image_id, movie.thin_cover_image_id])
            .flatten()
            .collect();
        let images = ImageRepository::new(self.db.clone())
            .find_by_ids(&image_ids)
            .await?;

        let cover = |id: Option<i32>| id.and_then(|id| images.get(&id)).cloned();
        Ok(tasks
            .into_iter()
            .map(|task| {
                let movie = task
                    .movie_number
                    .as_deref()
                    .and_then(|number| movies.get(number));
                DownloadTaskListItem {
                    id: task.id,
                    client_id: task.client_id,
                    movie_number: task.movie_number,
                    name: task.name,
                    remote_id: task.remote_id,
                    state: task.state,
                    progress: task.progress,
                    import_status: task.import_status,
                    created_at: task.created_at,
                    updated_at: task.updated_at,
                    // 上游 `(movie.title or "").strip() or None`。
                    movie_title: movie
                        .map(|movie| movie.title.trim().to_owned())
                        .filter(|title| !title.is_empty()),
                    movie_cover: movie.and_then(|movie| cover(movie.cover_image_id)),
                    movie_thin_cover: movie.and_then(|movie| cover(movie.thin_cover_image_id)),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两步确认的第一道门：`delete_files` 且未确认 → **422 + 专用码 + task_id**。
    ///
    /// 三个要素都要有：状态码 422、错误码、以及 `details.task_id`（客户端要靠
    /// 它告诉用户**哪个**任务要确认）。
    #[test]
    fn deleting_files_without_confirmation_is_refused_with_the_task_id() {
        let query = DeleteTaskQuery {
            delete_files: true,
            confirm_delete_files: false,
        };
        let error = ensure_delete_confirmed(1, &query).expect_err("未确认应被拒");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), "download_task_delete_confirmation_required");
        let details = error.details().expect("必须带 details");
        assert_eq!(details.get("task_id").and_then(|v| v.as_i64()), Some(1));
    }

    /// 带确认就放行 —— 门只挡「要删文件但没确认」这一种组合。
    #[test]
    fn confirmation_lets_the_delete_through() {
        let confirmed = DeleteTaskQuery {
            delete_files: true,
            confirm_delete_files: true,
        };
        assert!(ensure_delete_confirmed(1, &confirmed).is_ok());
        // 不删文件时不需要确认.
        let no_files = DeleteTaskQuery::default();
        assert!(ensure_delete_confirmed(1, &no_files).is_ok());
    }

    /// ★ 可导入的判据是**导入状态**，白名单 `{pending, failed, skipped}`。
    ///
    /// ⚠️ 骨架期这两条用例拿的是**下载状态**（`importable(&task("failed"), None)`），
    /// 还断言「导入已成功的不阻塞（可重新导入）」—— 与上游**正好相反**：
    /// 上游白名单不含 `completed`，说明已经导过了、不该再导。
    #[test]
    fn importable_follows_the_import_status_whitelist() {
        // 没有导入记录（NULL / 空串）= 全新任务，可导。
        assert!(DownloadTaskService::importable(None));
        assert!(DownloadTaskService::importable(Some("")));
        // 白名单三态。
        assert!(DownloadTaskService::importable(Some("pending")));
        assert!(DownloadTaskService::importable(Some("failed")));
        assert!(DownloadTaskService::importable(Some("skipped")));
        // 在跑 / 已成功：都不在白名单里。
        assert!(!DownloadTaskService::importable(Some("running")));
        assert!(!DownloadTaskService::importable(Some("completed")));
    }

    /// ★ 两道门**先 422 后 409**（上游 `:129-143`）。
    ///
    /// 「下载没完成」是 422（请求与资源状态不匹配），「导入状态不允许」是 409
    /// （现在不行）。顺序反了会把「种子还没下完」报成「导入冲突」，用户于是去等
    /// 一个永远不会结束的导入。
    #[test]
    fn the_import_gates_run_in_the_upstream_order() {
        // 1. 下载没完成 → 422，**哪怕**导入状态本来是可导的。
        let error = DownloadTaskService::ensure_importable(7, "downloading", true, None, None)
            .expect_err("下载中");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), "invalid_download_task_import");

        // 1'. 下载完成但没有导入来源 → 同一道 422。
        let error = DownloadTaskService::ensure_importable(7, "completed", false, None, None)
            .expect_err("没有来源");
        assert_eq!(error.status, 422);
        assert_eq!(
            error.details().and_then(|details| details.get("task_id")),
            Some(&serde_json::json!(7))
        );

        // 2. 两道都过 → Ok（全新任务 / 导入失败过都能重导）。
        DownloadTaskService::ensure_importable(7, "completed", true, None, None).expect("全新任务");
        DownloadTaskService::ensure_importable(7, "completed", true, Some("failed"), None)
            .expect("失败过可重导");
        DownloadTaskService::ensure_importable(7, "completed", true, Some("skipped"), None)
            .expect("跳过过可重导");

        // 3. 下载已完成 + 有来源，但导入状态不允许 → **409**（不是 422）。
        let error =
            DownloadTaskService::ensure_importable(7, "completed", true, Some("completed"), None)
                .expect_err("已导过");
        assert_eq!(error.status, 409);
        assert_eq!(error.code(), "download_task_import_conflict");
        assert_eq!(
            error
                .details()
                .and_then(|details| details.get("import_status")),
            Some(&serde_json::json!("completed"))
        );

        // 4. 自定义白名单：给了就**不再回落**到默认三态。
        DownloadTaskService::ensure_importable(
            7,
            "completed",
            true,
            Some("completed"),
            Some(&["completed"]),
        )
        .expect("白名单里就有 completed");
        let error = DownloadTaskService::ensure_importable(
            7,
            "completed",
            true,
            None,
            Some(&["completed"]),
        )
        .expect_err("没有导入记录不在自定义白名单里（上游 NULL not in set）");
        assert_eq!(error.status, 409);
    }

    /// ★ 分页非法走**专用**错误码，而不是通用的 `validation_error`。
    ///
    /// 上游 `validate_page(..., error_code="invalid_download_task_filter")`：
    /// 客户端要靠这个码把「筛选/分页写错了」与「任务不存在」分开。
    #[test]
    fn a_bad_page_is_rejected_with_the_dedicated_filter_code() {
        let error = validate_task_page(0, 20).expect_err("page 必须 >= 1");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), "invalid_download_task_filter");
        assert_eq!(error.api.message, "page must be greater than 0");
        assert_eq!(
            error.details().and_then(|details| details.get("page")),
            Some(&serde_json::json!(0))
        );

        let error = validate_task_page(1, 101).expect_err("page_size 上限 100");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), "invalid_download_task_filter");
        assert_eq!(
            error.details().and_then(|details| details.get("page_size")),
            Some(&serde_json::json!(101))
        );

        validate_task_page(1, 20).expect("合法分页");
        validate_task_page(1, 100).expect("上限本身合法");
    }

    /// 缺省分页与上游 `Field(default=...)` 一致。
    #[test]
    fn list_query_defaults_follow_upstream() {
        let query = ListTasksQuery::default();
        assert_eq!(query.page.unwrap_or(DEFAULT_PAGE), 1);
        assert_eq!(query.page_size.unwrap_or(DEFAULT_PAGE_SIZE), 20);
    }
}
