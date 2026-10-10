//! 导入的 TaskRun 边界（上游 `shared/import_task_service.py`，713 行，本域第二大）。
//!
//! # 这个类是**唯一**的导入入队路径
//!
//! HTTP 端点**只**做两件事：入队、返回 202。真正的导入跑在 worker 里，
//! 由 `execute(reporter, params)` 执行。
//!
//! 这么切的理由是导入可能跑几十分钟（几千个文件）。若在 HTTP 处理器里同步做，
//! 连接会占满、客户端断开后前功尽弃、且无法重试。
//!
//! # 契约层在**本模块**（唯一来源）
//!
//! 上游把导入的形状定义在 `schema/transfers/media_import.py`。本仓对应地把
//! **除 browse 组之外**的全部 DTO 定义在这里，`sm-api` 只 `use` 回来 ——
//! 骨架期路由文件里另有一套内联的同名结构体，那是**第二份实现**，已删。
//!
//! ⚠️ 骨架期这 6 个 DTO 是凭印象写的，**六个全错**。对照表（上游为
//! `media_import.py` 的行号）：
//!
//! | 骨架期 | 上游 |
//! |---|---|
//! | `ImportRequest.media_kind: "JAV"/"VIDEO"` | `Literal["jav","video"]`（小写，`:36`）|
//! | `source_disposition: "keep"/"move"` | `keep` / `delete_after_commit` / `in_place`（`:39`）|
//! | 多了 `operation_namespace` | 上游**没有**这个字段；它是 `import_from_source` 的**执行参数**（`import_service.py:183`），不是请求字段 |
//! | `ImportAcceptedResponse { task_run_id }` | `{task_run_id, task_key, state}`（`:124-127`）|
//! | `ImportFailedItemResource` 5 个自造字段 | 13 个字段（`:60-73`）|
//! | `ImportMetadataSearchResponse { item_id, … }` | 无 `item_id`，多 `source_errors`（`:107-110`）|
//! | `MetadataCandidate { candidate_id, title, date, confidence }` | 10 个字段，**没有** `confidence` / `date`（`:76-86`）|
//! | `ImportExecuteSummary { imported, skipped, failed }` | `{imported_count, skipped_count, failed_count, new_playable_movies, created_video_ids, failed_files}`（`:49-57`）|
//!
//! # `execute` 按 `params["mode"]` 分**三种**执行模式
//!
//! | `mode` | 触发来源 | 做什么 |
//! |---|---|---|
//! | 缺省 | `POST /imports` | 跑一次 `import_from_source` |
//! | `download_tasks` | `download_task_auto_import` | 批量导入一批下载任务 |
//! | `retry_failed_file` | 失败项重试端点 | 重试一条失败项 |
//!
//! **模式来自 `params`，不是来自注册的多个 handler** —— 一个 `task_key` 对应
//! 一个 TaskRun，模式在参数里。这样任务中心里它们是同一类任务，
//! 而分成三个 `task_key` 会让「这次导入」在界面上裂成三条。
//!
//! ⚠️ 骨架期把第三个模式写成了 `retry_failed_item` —— 上游是
//! `retry_failed_file`（`import_task_service.py:269`）。
//!
//! # 互斥键：按**媒体库**，且是 `409` 不是排队
//!
//! 同一媒体库已有导入在跑 → **409 `import_task_conflict`**。不排队，
//! 因为用户此时多半是想「再点一次看看」，排队会让第二次点击无声无息。
//!
//! 键的形状见 [`super::import_write_mutex`]（`library_import:{id}`）。
//! 它**不是** worker 的 `aps:{task_key}` —— 后者按任务键加锁，会把按库并行
//! 退化成全局串行。所以入队走
//! [`TaskQueueService::enqueue_with_mutex_key`]，而不是默认那把锁。
//!
//! # ⚠️ 与上游的一处刻意差异：入队与回写下载任务**不是同一个事务**
//!
//! 上游把「建 TaskRun + 回写 `download_task.import_status/import_task_run`」
//! 放在同一个 `get_database().atomic()` 里，靠回滚保证「要么都成功，要么都
//! 不见」。本仓这两笔写入分属两张表的两个仓储，且都只走连接池
//! （没有 `_in` 变体），要真正原子就得给 `sm-db` 加一批 `_in` 方法并让
//! [`TaskQueueService`] 破一次「唯一入队路径」。
//!
//! 本轮的处置是**补偿而不是回滚**：入队成功、但回写下载任务失败（或批量
//! 占用数量不符）时，把刚建的那条 TaskRun **显式判为失败**
//! （[`TaskRunService::fail_task_run`]，终态会释放互斥键），再返回上游的
//! 409 / 502。
//!
//! 唯一的可见差异：上游回滚后**没有**那条 run 行，本仓留下一条**失败**的 run
//! （任务中心可见，不发通知）。宁可留一条失败的 run，也不留一条占着
//! `library_import:{id}` 的 pending run —— 互斥键不释放会让该媒体库的导入
//! **永久 409**。
//!
//! 后续项：给 `sm-db` 补 `_in` 变体 + `UnitOfWork` 编排，把这处收回原子性。
//!
//! # 未落地（各有阻塞）
//!
//! ✅ 已落地：`enqueue` / `enqueue_batch`（按库互斥、四分支）、
//! `list_failed_items`（读 `result_summary.failed_files` 投影成 13 字段）、
//! `enqueue_failed_item_retry`（202：候选校验 → 终态 → 失败项 → 源与库 →
//! 入队 → 回写 `state=queued`）。
//!
//! | 方法 | 缺什么 |
//! |---|---|
//! | [`ImportTaskService::search_failed_item`] | 元数据搜索 —— 走**插件 ABI**（`metadata_source`）|
//! | [`ImportTaskService::execute`] | ✅ 已实现（三模式分发 + 409 终态检查 + 批量容错聚合）。执行依赖由组合根经 `with_import_service` 注入；`sm-scheduler` 尚未注册 `library_import` 处理器 |
//!
//! # 手动搜索的两种失败**要区别对待**
//!
//! [`ImportTaskService::MANUAL_SEARCH_FAILURE_REASONS`] 只含两个码：
//! `movie_number_not_found` 与 `metadata_fetch_failed`。它们是**用户能自己
//! 解决**的（换个番号、重试），所以手动触发的搜索失败**不消耗**订阅的重试
//! 预算 —— 见 `catalog::movie_subscription_search_state`。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sm_db::system::task_state;

use sm_db::repo::{
    BackgroundTaskRunRepository, DownloadClientRepository, DownloadTaskRepository,
    MediaLibraryRepository, SystemNotificationRepository,
};
use sm_db::system::activity::BackgroundTaskRun;
use sm_db::transfers::downloads::{import_status, DownloadTask};
use sm_db::{Db, MediaLibrary};
use tracing::warn;

use crate::catalog::movie_metadata_search::MovieMetadataSearchService;
use crate::error::{details_of, ProgrammerError, ServiceError};
use crate::system::activity::TaskRunService;
use crate::system::task_queue::{ConflictPolicy, EnqueueOutcome, TaskQueueService};

use super::import_notifications::{create_new_media_reminder, NewMovieReminderItem};
use super::import_service::{ImportFailure, ImportResult, MediaImportService};
use super::import_write_mutex::library_import_mutex_key;

/// 任务键。与 `cron_spec` 与 `sm_scheduler::lane_of` 里的 `library_import`
/// **必须一致** —— 它同时决定了**专属道**（`import` 道，2 并发）。
pub const TASK_KEY: &str = "library_import";

/// 手动搜索时**不算用户失误**的失败原因。
///
/// 只有这两个。其它失败原因（provider 挂了、磁盘满了）都属于服务端问题，
/// 该走重试与告警。
pub const MANUAL_SEARCH_FAILURE_REASONS: [&str; 2] =
    ["movie_number_not_found", "metadata_fetch_failed"];

/// 失败项重试的触发方式。上游 `enqueue_failed_item_retry` 写死
/// `trigger_type="manual"` —— 与批量入队的 `"internal"` 不同，任务中心据此
/// 区分「谁触发的」。
const RETRY_TRIGGER_TYPE: &str = "manual";

/// 失败项重试的任务名。上游那处字面量（`…service.py:265`）。
///
/// 与 [`default_task_name`] 不一回事：那条是「JAV媒体库导入」，这条专指重试。
const RETRY_TASK_NAME: &str = "JAV失败项重试导入";

/// `params["mode"]` 之一：重试一条失败项。
///
/// 与模块文档「三种执行模式」那张表**同一组字面量**，worker 的
/// [`ImportTaskService::execute`] 按它分发。
pub const IMPORT_MODE_RETRY_FAILED_FILE: &str = "retry_failed_file";

/// 批量入队的触发方式。上游 `enqueue_batch` 里写死 `trigger_type="internal"`。
///
/// 单独提出来是因为它**不是** HTTP 触发 —— `enqueue`（用户点了「导入」）
/// 是 `manual`，而批量是下载完成后由宿主自己发起的。
const BATCH_TRIGGER_TYPE: &str = "internal";

/// 校验错误码。上游的 pydantic 失败一律是这一个码 + 固定文案。
const VALIDATION_ERROR: &str = "validation_error";
/// 校验失败的固定文案。与 [`crate::extract::Json`] 的拒绝路径逐字一致。
const VALIDATION_ERROR_MESSAGE: &str = "Request validation failed";

/// 媒体种类（上游 `Literal["jav", "video"]`）。
///
/// # 为什么是小写
///
/// 上游 schema 是小写字面量，Flutter 前端也传小写
/// （`media_import_page.dart:64` / `:79`）。骨架期写成 `JAV` / `VIDEO`
/// 会让**每一次**导入在 422 上失败 —— 而那个 422 看起来像是客户端的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// JAV 影片：入库成 `movie`，可参与刮削与订阅。
    Jav,
    /// 普通视频：入库成 `video_item`。
    Video,
}

/// 源文件处置方式（上游 `Literal["keep", "delete_after_commit", "in_place"]`）。
///
/// | 值 | 含义 |
/// |---|---|
/// | `keep` | 保留源（**默认**）|
/// | `delete_after_commit` | 宿主写完之后删掉源 |
/// | `in_place` | 原地导入（不动文件，只登记）|
///
/// ⚠️ 骨架期写的是 `keep` / `move` —— `move` **上游没有**。
///
/// ⚠️ [`super::import_service::source_disposition`] 里还有**第二份**取值定义
/// （只有 `keep` / `move`，且未接进来）。那属于 `import_service` 那一轮要
/// 对齐的缺口，本轮只标注、不改（改它会动到 `import_service` 的调用面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceDisposition {
    /// 保留源文件。
    #[default]
    Keep,
    /// 定稿之后删掉源。
    DeleteAfterCommit,
    /// 原地导入：文件不动。
    InPlace,
}

/// 失败项的处置状态（上游 `Literal["pending", "queued", "resolved"]`）。
///
/// **不是** TaskRun 的状态机 —— 它描述的是「这一条失败项」有没有人在处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportFailedItemState {
    /// 待处理：可以人工搜索元数据、可以重试。
    #[default]
    Pending,
    /// 已排进一次重试（重试任务还没收口）。
    Queued,
    /// 已解决。
    Resolved,
}

/// 元数据候选的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataCandidateSource {
    /// 内置的 JavDB 抓取。
    Javdb,
    /// 插件提供的元数据源。
    Plugin,
}

/// 入队请求（上游 `ImportRequest`，`media_import.py:33-46`）。
///
/// # 没有 `operation_namespace`
///
/// 骨架期在这里加过一个 `operation_namespace`（注释写着「用于互斥与去重」）。
/// 上游**没有**这个字段：它是一个**执行参数**
/// （`import_from_source(..., operation_namespace=...)`，`import_service.py:183`），
/// 由 worker 在 `execute` 里按 `task:{task_run_id}` 或
/// `task:{task_run_id}:download:{task_id}` 现算（`import_task_service.py:323/452`）。
/// 入队时把它写进请求，等于让客户端决定宿主的去重命名空间。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImportRequest {
    /// `jav` / `video`。
    pub media_kind: MediaKind,
    /// 媒体库 id。**必填且为正**（上游 `Field(gt=0)`）。
    pub library_id: i32,
    /// 不透明源引用。**只由其 provider 解释**，宿主只保存与回传。
    ///
    /// 类型是**对象**而不是任意 JSON —— 上游是 `dict`，pydantic 会在提取阶段
    /// 把 `"a"` / `[1]` / `null` 拒成 422。
    pub source_ref: Map<String, Value>,
    /// 源文件处置方式。缺省 `keep`。
    #[serde(default)]
    pub source_disposition: SourceDisposition,
    /// 归入哪个合集（非 JAV 才有意义）。上游 `Field(default=None, gt=0)`。
    pub collection_id: Option<i32>,
}

impl ImportRequest {
    /// 上游的两条校验：字段约束（`gt=0`）+ `model_validator`（`jav` 不许带合集）。
    ///
    /// 顺序与 pydantic 一致：**先字段约束、后模型校验**。顺序错了会在
    /// 「两个都错」时报出另一个错，而客户端是按 `details` 定位控件的。
    ///
    /// 放在 service 而不是路由：上游的 schema 层与 service 层是两次调用，
    /// 而可观测结果（422 `validation_error`）相同 —— 规则只留一处，
    /// 就不会有「路由校验过、服务层忘了」的分裂。
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.library_id <= 0 {
            return Err(field_validation_error("library_id", self.library_id));
        }
        if let Some(collection_id) = self.collection_id {
            if collection_id <= 0 {
                return Err(field_validation_error("collection_id", collection_id));
            }
        }
        if self.media_kind == MediaKind::Jav && self.collection_id.is_some() {
            return Err(request_validation_error(
                "jav import does not support collection_id",
            ));
        }
        Ok(())
    }

    /// `request.model_dump()` 的等价物。
    ///
    /// **键名必须与上游逐字一致** —— 这份对象会原样进
    /// `background_task_run.params`，由 worker 的 `execute` 读回来。少一个键
    /// 或改一个名，表现为「导入跑起来了但参数是空的」。
    fn params_object(&self) -> Map<String, Value> {
        let mut params = Map::new();
        params.insert("media_kind".to_owned(), json!(self.media_kind));
        params.insert("library_id".to_owned(), json!(self.library_id));
        params.insert(
            "source_ref".to_owned(),
            Value::Object(self.source_ref.clone()),
        );
        params.insert(
            "source_disposition".to_owned(),
            json!(self.source_disposition),
        );
        params.insert("collection_id".to_owned(), json!(self.collection_id));
        params
    }
}

/// 已受理的导入（**202**）。上游 `ImportAcceptedResponse`（`:124-127`）。
///
/// 三个字段都要发：`task_run_id` 供轮询，`task_key` / `state` 让客户端不必
/// 猜「刚建的这条是什么、现在到哪一步了」（Flutter 目前只读 `task_run_id`，
/// 但它**解析**三个键，少一个就是契约破坏）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportAcceptedResponse {
    /// TaskRun id。**轮询它看进度**。
    pub task_run_id: i32,
    /// 任务键，恒为 `"library_import"`。
    pub task_key: String,
    /// 台账状态，刚建出来时为 `"pending"`。
    pub state: String,
}

/// 失败项（响应体）。上游 `ImportFailedItemResource`（`:60-73`）。
///
/// # 字段**不是**自造的五个
///
/// 骨架期这里是 `{item_id, movie_number, failure_reason, failure_detail,
/// attempts}` —— 五个字段**上游一个都没有**。上游的 `id` 是导入器给的业务
/// 标识（仍不是自增主键），而「番号」「原因」「尝试次数」都不在这个资源里：
/// 失败项带的是**文件维度**的信息（路径、大小、是不是视频），加上它自己的
/// 处置状态（`state` / `retry_task_run_id` / `resolved_*`）。
///
/// `can_manual_search` 是**算出来的**（`import_task_service.py:530-535`）：
/// 只有「待处理 + 是视频 + JAV + 原因是用户可修的两类」才为真。客户端据此
/// 决定要不要显示「搜索元数据」按钮。
#[derive(Debug, Clone, Serialize)]
pub struct ImportFailedItemResource {
    /// **字符串**业务标识，**不是**自增主键（可能是带前缀/含编码的路径）。
    pub id: String,
    /// 相对路径。**不暴露宿主的绝对路径**（provider 的命名空间不进宿主）。
    pub relative_path: String,
    pub size_bytes: i64,
    pub is_video: bool,
    /// 失败原因码。取值集合见 [`super::import_service::failure_reason`]。
    pub reason: String,
    /// 人可读细节。空串（不是 `None`）—— 上游默认 `""`。
    pub detail: String,
    /// 条目类型。取值由导入器决定。
    pub kind: String,
    /// 这一条失败项的处置状态。
    pub state: ImportFailedItemState,
    /// 正在为它跑的那次重试的 TaskRun id。
    pub retry_task_run_id: Option<i32>,
    /// 已解决成哪部影片。
    pub resolved_movie_id: Option<i64>,
    /// 已解决成哪个媒体。
    pub resolved_media_id: Option<i64>,
    /// 上一次重试的错误。
    pub last_retry_error: Option<String>,
    /// 能不能人工搜元数据（见类型文档）。
    pub can_manual_search: bool,
}

/// 手动搜索请求体（上游 `ImportMetadataSearchRequest`，`:96-104`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ImportMetadataSearchRequest {
    /// 番号。上游 `Field(min_length=1, max_length=255)`，再 `strip` 后非空。
    pub movie_number: String,
}

/// 番号的最大长度。上游 `Field(max_length=255)`。
pub const MOVIE_NUMBER_MAX_LENGTH: usize = 255;

impl ImportMetadataSearchRequest {
    /// 上游的字段约束 + `strip` 后非空。
    ///
    /// 顺序与 pydantic 一致：**长度约束在 `strip` 之前**（`Field` 先跑，
    /// `model_validator` 后跑）。先 strip 再判长度会让
    /// `"   " + 255 个字符` 这种输入从 422 变成合法。
    pub fn validate(&self) -> Result<(), ServiceError> {
        let length = self.movie_number.chars().count();
        if length == 0 || length > MOVIE_NUMBER_MAX_LENGTH {
            return Err(request_validation_error(&format!(
                "movie_number must be 1..={MOVIE_NUMBER_MAX_LENGTH} characters"
            )));
        }
        if self.movie_number.trim().is_empty() {
            return Err(request_validation_error("movie_number cannot be blank"));
        }
        Ok(())
    }
}

/// 元数据搜索结果（上游 `ImportMetadataSearchResponse`，`:107-110`）。
///
/// ⚠️ 骨架期多了 `item_id`、少了 `source_errors`。`item_id` 在**路径**里
/// （`/failed-items/{item_id}/search`），响应里没有它的位置；`source_errors`
/// 则必须发 —— 某个元数据源挂掉时候选会为空，客户端要靠它区分
/// 「没搜到」与「源不可用」。
#[derive(Debug, Clone, Serialize)]
pub struct ImportMetadataSearchResponse {
    pub movie_number: String,
    /// 候选。**按置信度降序**由 provider 决定，宿主不重排。
    pub candidates: Vec<MetadataCandidate>,
    /// 各个元数据源的失败。**可以为空**，但键不能少。
    pub source_errors: Vec<ImportMetadataSourceErrorResource>,
}

/// 一条候选。上游 `ImportMetadataCandidateResource`（`:76-86`）。
///
/// ⚠️ 骨架期这里是 `{candidate_id, title, date, confidence}` ——
/// 上游**没有** `confidence`（排序由 provider 保证），也**没有** `date`
/// （是 `release_date`）。而 `candidate_id` 之外那 9 个字段都要发：
/// 客户端要靠 `cover_url` / `duration_minutes` / `source_name` 渲染候选卡片，
/// 靠 `source` + `source_id` / `javdb_id` 把选择回传给重试端点。
#[derive(Debug, Clone, Serialize)]
pub struct MetadataCandidate {
    /// 候选 id。**回传给 retry 端点**（`candidate_id`）。
    pub candidate_id: String,
    /// 哪个元数据源。
    pub source: MetadataCandidateSource,
    /// 源的显示名。
    pub source_name: String,
    /// 该源侧的外部 id。
    pub source_id: Option<String>,
    /// JavDB 侧的 id（`source = javdb` 时有意义）。
    pub javdb_id: Option<String>,
    pub movie_number: String,
    pub title: String,
    pub cover_url: Option<String>,
    /// 发行日期。**字符串**（上游是 `str`，不是日期类型）。
    pub release_date: Option<String>,
    /// 时长（分钟）。上游是**必填** `int`。
    pub duration_minutes: i64,
}

/// 一个元数据源的失败（上游 `ImportMetadataSourceErrorResource`，`:89-93`）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportMetadataSourceErrorResource {
    pub source: String,
    pub source_name: String,
    pub reason: String,
    pub detail: String,
}

/// 失败项重试的请求体（上游 `ImportFailedItemRetryRequest`，`:113-121`）。
#[derive(Debug, Clone, Deserialize)]
pub struct ImportFailedItemRetryRequest {
    /// 采用哪条候选。上游 `Field(min_length=1, max_length=1024)` + `strip` 后非空。
    ///
    /// **必填** —— 骨架期写成了 `chosen_candidate: Option<{provider, external_id}>`，
    /// 那是自造形状：上游只收一个不透明的 `candidate_id`（回传搜索给的那一个），
    /// 宿主不需要知道它内部怎么编码。
    pub candidate_id: String,
}

/// `candidate_id` 的最大长度。上游 `Field(max_length=1024)`。
pub const CANDIDATE_ID_MAX_LENGTH: usize = 1024;

impl ImportFailedItemRetryRequest {
    /// 上游的字段约束 + `strip` 后非空（顺序见
    /// [`ImportMetadataSearchRequest::validate`]）。
    pub fn validate(&self) -> Result<(), ServiceError> {
        let length = self.candidate_id.chars().count();
        if length == 0 || length > CANDIDATE_ID_MAX_LENGTH {
            return Err(request_validation_error(&format!(
                "candidate_id must be 1..={CANDIDATE_ID_MAX_LENGTH} characters"
            )));
        }
        if self.candidate_id.trim().is_empty() {
            return Err(request_validation_error("candidate_id cannot be blank"));
        }
        Ok(())
    }
}

/// 执行结果摘要。**字段逐字对齐上游 `ImportResult`**（`:49-57`）。
///
/// ⚠️ 骨架期是 `{imported, skipped, failed}` —— 三个名字都不对，且少了
/// `new_playable_movies` / `created_video_ids` / `failed_files`。后两个是
/// **导入完成后要立刻用的**：新入库的影片 id 通知客户端刷新，失败文件则成为
/// 失败项列表的来源。
///
/// ⚠️ 别和 [`super::import_service::ImportResult`] 合并 —— 那是**执行层**的
/// 内部结果（`failed: Vec<ImportFailure>`，逐条带 `source_ref`），本类型是
/// **持久化进 `result_summary` 并给客户端读**的形状。两者字段名不同、用途
/// 不同，只是恰好都叫「导入结果」。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportExecuteSummary {
    pub imported_count: i64,
    pub skipped_count: i64,
    pub failed_count: i64,
    /// 新入库、且**现在可播放**的影片（客户端据此刷新列表）。
    pub new_playable_movies: Vec<Value>,
    /// 新入库的视频条目 id。
    pub created_video_ids: Vec<i64>,
    /// 失败文件明细。**也是失败项列表的数据源** —— 只留在 summary 里，
    /// 失败项资源不暴露 `source_ref` 一类的宿主内部字段。
    pub failed_files: Vec<Value>,
}

/// 导入服务。
///
/// `import_service` 是**可选**的执行依赖：入队（`enqueue` / `enqueue_batch`）
/// 与查询路径都不需要它，只有 worker 的执行体（[`Self::execute`]）需要
/// （扫描 / 暂存 / 定稿）。所以 `new` 不收它，由组合根在装配 worker handler
/// 时经 [`Self::with_import_service`] 注入；没装时 `execute` 报 503
/// `import_service_not_wired`，而不是在构造期 panic ——
/// `sm-scheduler` 的 `HandlerFactory` 签名是 `Fn(&Db, &Value)`，每次调用只
/// 拿到库，进程级依赖只能由注册闭包从外部捕获进来。
///
/// ⚠️ 没有 `Debug` / `Clone` 派生：[`MediaImportService`] 里的 trait 对象
/// 不支持它们。本类型处处现用现建，没有克隆它的调用点。
pub struct ImportTaskService {
    db: Db,
    import_service: Option<MediaImportService>,
}

impl ImportTaskService {
    /// 构造。取 `&Db` 并克隆（与本 crate 全部 service 同形）。
    pub fn new(db: &Db) -> Self {
        Self {
            db: db.clone(),
            import_service: None,
        }
    }

    /// 装上执行依赖（worker 组合根用）。Builder 风格，可链式调用。
    pub fn with_import_service(mut self, import_service: MediaImportService) -> Self {
        self.import_service = Some(import_service);
        self
    }

    /// 取执行依赖。没装 → 503（见结构体文档）。
    fn require_import_service(&self) -> Result<&MediaImportService, ServiceError> {
        self.import_service.as_ref().ok_or_else(|| {
            ServiceError::unavailable("import_service_not_wired", "导入执行链路尚未接线")
        })
    }

    /// 手动搜索的可重试原因（见模块文档）。
    pub const MANUAL_SEARCH_FAILURE_REASONS: [&'static str; 2] = MANUAL_SEARCH_FAILURE_REASONS;

    /// 入队一次导入。**202**。
    ///
    /// 错误码：
    ///
    /// | 情况 | 码 |
    /// |---|---|
    /// | 请求本身不合法（`jav` 带合集、id 非正） | `422 validation_error` |
    /// | 媒体库不存在 | `404 media_library_not_found` |
    /// | 媒体库没有 provider 配置 | `422 invalid_media_library_provider` |
    /// | 该库已有导入在跑 | `409 import_task_conflict` |
    /// | 下载任务不存在 / 建 TaskRun 失败 | `502 import_task_create_failed` |
    ///
    /// **409 不排队**（见模块文档）。
    ///
    /// # 顺序与上游逐条对应（`import_task_service.py:44-107`）
    ///
    /// 校验 → 取库 → 取下载任务（带上它的目标番号）→ 组 `params` →
    /// 入队（按库互斥）→ 回写下载任务。
    ///
    /// ⚠️ 「下载任务不存在」是 **502 而不是 404**：上游的
    /// `DownloadTask.get_by_id` 在 `try` 里，`DoesNotExist` 落进通用的
    /// `except Exception` → `import_task_create_failed`。这里照抄。
    ///
    /// # 为什么 `download_task_id` 是 `Option<i32>`
    ///
    /// 上游是 `int | None`，值是**本表的自增主键**（`download_task.id` 是
    /// `integer`）。它不是 TaskRun id —— 后者在返回值里。
    ///
    /// # 两个可选参数
    ///
    /// 参数顺序与上游一致（去掉本仓没有调用点的 `plugin_id`）：
    /// `(request, trigger_type, download_task_id, task_name)`。
    ///
    /// - `download_task_id`：由下载任务发起时给，用于回写它的导入状态，
    ///   并把它**自己的目标番号**带进 `params`。
    /// - `task_name`：**显示名**。缺省时按 `media_kind` 推出
    ///   （模块内的 `default_task_name`），但下载任务那条路径会给一个更具体的名字
    ///   （`下载任务导入 {番号}`，见
    ///   [`super::download_task::DownloadTaskService::trigger_import`]）——
    ///   任务中心里两条「媒体库导入」并排时，用户只靠名字区分。
    pub async fn enqueue(
        &self,
        request: ImportRequest,
        trigger_type: &str,
        download_task_id: Option<i32>,
        task_name: Option<&str>,
    ) -> Result<ImportAcceptedResponse, ServiceError> {
        request.validate()?;
        let library = require_library(&self.db, request.library_id).await?;

        // 下载任务导入只认准该任务的目标番号，资源包里的其它番号一律忽略。
        let download_task = match download_task_id {
            Some(task_id) => Some(self.require_download_task(task_id).await?),
            None => None,
        };

        let mut params = request.params_object();
        // 键**一定要写**，值为 `null` 也要写 —— `params` 是 worker 的入参
        // 契约，缺键与空值是两件事。
        params.insert("download_task_id".to_owned(), json!(download_task_id));
        if let Some(task) = &download_task {
            params.insert("target_movie_number".to_owned(), json!(task.movie_number));
        }

        // 互斥键按**媒体库**，所以不能走默认的 `aps:{task_key}`。
        let display_name = task_name
            .map(str::to_owned)
            .unwrap_or_else(|| default_task_name(&request));
        let outcome = TaskQueueService::new(&self.db)
            .enqueue_with_mutex_key(
                TASK_KEY,
                trigger_type,
                Some(&display_name),
                &library_import_mutex_key(i64::from(library.id)),
                Some(Value::Object(params)),
                ConflictPolicy::Raise,
            )
            .await?;

        let run = match outcome {
            EnqueueOutcome::Enqueued(run) => *run,
            EnqueueOutcome::Skipped {
                blocking_task_run_id,
            } => return Err(import_task_conflict(blocking_task_run_id)),
        };

        if let Some(task) = &download_task {
            if let Err(error) = DownloadTaskRepository::new(self.db.clone())
                .set_import_status(task.id, import_status::RUNNING, Some(i64::from(run.id)))
                .await
            {
                // 补偿而不是回滚 —— 见模块文档「刻意的差异」。
                self.abort_enqueued_run(run.id).await;
                let _ = DownloadTaskRepository::new(self.db.clone())
                    .set_import_status(task.id, import_status::FAILED, None)
                    .await;
                return Err(import_task_create_failed(error.to_string()));
            }
        }

        Ok(ImportAcceptedResponse {
            task_run_id: run.id,
            task_key: run.task_key,
            state: run.state,
        })
    }

    /// 批量入队（一批下载任务的自动导入）。上游 `enqueue_batch`（`:109-173`）。
    ///
    /// 上游有一条硬约束：**列表不能为空**，且**必须同属一个媒体库** ——
    /// 违反抛 `ValueError`（不是 `ApiError`），因为那是**调用方**的 bug
    /// （worker 代码写错了），不是用户请求的问题。本仓对应
    /// [`ProgrammerError`] → 500 `programmer_error`。
    ///
    /// # 收实体而不是 id
    ///
    /// 上游收 `list[DownloadTask]`，因为每一项都要读它的
    /// `completed_source_ref`（要导入的源）与 `movie`（目标番号）。骨架期写的是
    /// `&[i64]` —— 那样调用方得先自己查一遍，等于把「同一个库」这条不变量
    /// 的判断拆到两个地方。
    ///
    /// # 「全有或全无」怎么落
    ///
    /// 上游把「建 TaskRun + 只占用 `pending` 的下载任务」放在一个事务里：
    /// 只要有一条任务已被别的导入占用（`updated_count != len(...)`），
    /// **整批拒绝**且什么都不改。本仓用**单条带计数谓词的 UPDATE**
    /// （[`DownloadTaskRepository::mark_import_started`]）达到同一个效果：
    /// 条件不满足时影响 0 行，一条都不改，所以既不需要回滚也不需要跨表事务。
    ///
    /// 失败时同样走**补偿**：把已建的 TaskRun 判失败（释放互斥键），
    /// 再返回上游的 409。
    pub async fn enqueue_batch(&self, download_tasks: &[DownloadTask]) -> Result<(), ServiceError> {
        if download_tasks.is_empty() {
            return Err(ProgrammerError::new("download_tasks must not be empty").into());
        }
        let library_id = self.single_library_id(download_tasks).await?;

        // 上游 `MediaLibrary.get_by_id` 在 try 之外 —— 库不存在是 500，
        // 而不是 404：这个入口没有用户输入可背锅。
        let library = MediaLibraryRepository::new(self.db.clone())
            .find_by_id(library_id)
            .await?
            .ok_or_else(|| {
                ProgrammerError::new(format!("下载任务所属的媒体库 {library_id} 不存在"))
            })?;
        if library.provider_key.is_empty() {
            return Err(ServiceError::validation(
                "invalid_media_library_provider",
                "媒体库缺少 provider_key",
            ));
        }

        let mut batch_items = Vec::with_capacity(download_tasks.len());
        for task in download_tasks {
            // 批量路径的请求是**宿主自己造的**：番号永远是 `jav`、源永远保留。
            let request = ImportRequest {
                media_kind: MediaKind::Jav,
                library_id: library.id,
                source_ref: completed_source_ref(task)?,
                source_disposition: SourceDisposition::Keep,
                collection_id: None,
            };
            let mut item = request.params_object();
            item.insert("download_task_id".to_owned(), json!(task.id));
            item.insert("target_movie_number".to_owned(), json!(task.movie_number));
            batch_items.push(Value::Object(item));
        }

        let task_name = format!("下载任务连续导入（{}个）", batch_items.len());
        let params = json!({
            "download_tasks": batch_items,
            "library_id": library.id,
        });

        let outcome = TaskQueueService::new(&self.db)
            .enqueue_with_mutex_key(
                TASK_KEY,
                BATCH_TRIGGER_TYPE,
                Some(&task_name),
                &library_import_mutex_key(i64::from(library.id)),
                Some(params),
                ConflictPolicy::Raise,
            )
            .await?;

        let run = match outcome {
            EnqueueOutcome::Enqueued(run) => *run,
            EnqueueOutcome::Skipped {
                blocking_task_run_id,
            } => return Err(import_task_conflict(blocking_task_run_id)),
        };

        let ids: Vec<i32> = download_tasks.iter().map(|task| task.id).collect();
        let expected = u64::try_from(ids.len()).unwrap_or(u64::MAX);
        let affected = DownloadTaskRepository::new(self.db.clone())
            .mark_import_started(&ids, i64::from(run.id))
            .await?;
        if affected != expected {
            // 整批一条都没改（计数谓词保证），所以这里只需要撤掉刚建的 TaskRun。
            self.abort_enqueued_run(run.id).await;
            return Err(ServiceError::conflict(
                "download_task_import_conflict",
                "部分下载任务已被其它导入任务占用",
                None,
            ));
        }
        Ok(())
    }

    /// `GET /imports/{task_run_id}/failed-items`
    ///
    /// TaskRun 不存在（**或它不是导入任务**）→ **404 `import_task_not_found`**，
    /// 区别于「存在但没有失败项」= 200 空列表。
    ///
    /// # 失败项不在表里，在台账的 `result_summary` 里
    ///
    /// 上游 `(task_run.result_summary or {}).get("failed_files", [])`
    /// （`import_task_service.py:176-179`）。**没有单独的失败项表** —— 它们随
    /// 任务结果一起保存，因为它们的生命周期与那次任务完全相同（任务被保留期
    /// 清理时才消失，而 `failed_files` 里的 `source_ref` 等宿主内部字段
    /// 也只应留在那一行里）。
    ///
    /// 投影见模块内的 `failure_item_resource`，`can_manual_search` 的四个条件见
    /// `StoredFailedItem::can_manual_search`。
    pub async fn list_failed_items(
        &self,
        task_run_id: i32,
    ) -> Result<Vec<ImportFailedItemResource>, ServiceError> {
        let task_run = self.require_import_task_run(task_run_id).await?;
        failed_files(task_run_id, task_run.result_summary.as_deref())?
            .into_iter()
            .map(|item| failure_item_resource(task_run_id, item))
            .collect()
    }

    /// `POST /imports/{task_run_id}/failed-items/{item_id}/search` —— **200**。
    ///
    /// 错误码：`404 import_task_not_found` / `404 failed_item_not_found` /
    /// `409 failed_item_not_pending`（这条已在导入中，不能再搜）/
    /// `409 failed_item_search_unavailable`（元数据源不可用 —— 判据见
    /// `import_task_service.py:503-512`）。
    ///
    /// ⚠️ **未落地**：搜索本身要走**插件 ABI**（`metadata_source`），
    /// 宿主侧的调用面还没接出去。
    pub async fn search_failed_item(
        &self,
        task_run_id: i32,
        item_id: &str,
        movie_number: &str,
    ) -> Result<ImportMetadataSearchResponse, ServiceError> {
        // ① 任务存在且是导入任务（404）。
        let task_run = self.require_import_task_run(task_run_id).await?;

        // ② 失败项存在（404）。
        let items = failed_files(task_run_id, task_run.result_summary.as_deref())?;
        let item_value = find_failure_item(task_run_id, &items, item_id)?;
        let item: StoredFailedItem = serde_json::from_value(item_value.clone())
            .map_err(|e| malformed_summary(task_run_id, format!("失败项形状不对：{e}")))?;

        // ③ 失败项必须还是 pending（409）。
        if item.state != ImportFailedItemState::Pending {
            return Err(ServiceError::conflict(
                "failed_item_not_pending",
                "该失败项已在重试或已解决",
                None,
            ));
        }

        // ④ 必须是「JAV 视频 + 可人工处理的原因」（409）。
        if !item.is_searchable_kind() {
            return Err(ServiceError::conflict(
                "failed_item_search_unavailable",
                "该失败项不支持人工搜索元数据",
                None,
            ));
        }

        // ⑤ 番号不能为空（422）。
        let movie_number = movie_number.trim();
        if movie_number.is_empty() {
            return Err(ServiceError::validation("validation_error", "番号不能为空"));
        }

        // ⑥ 实际搜索走插件 ABI（metadata_source），宿主侧调用面未接线。
        // 诚实返回 503，而非 panic。
        Err(ServiceError::unavailable(
            "metadata_source_not_installed",
            "元数据源插件未安装或未接线，无法搜索",
        ))
    }

    /// `POST /imports/{task_run_id}/failed-items/{item_id}/retry` —— **202**。
    ///
    /// 上游 `enqueue_failed_item_retry`（`shared/import_task_service.py:199-277`）。
    ///
    /// | 情况 | 码 |
    /// |---|---|
    /// | 候选 id 编码不对 / 其插件来源已停用 | 422 `invalid_metadata_candidate` |
    /// | 请求本身不合法（`candidate_id` 空白或超长） | 422 `validation_error` |
    /// | 任务不存在 / 不是导入任务 | 404 `import_task_not_found` |
    /// | 任务**还没跑完** | 409 `import_task_not_finished` |
    /// | 失败项不在这条任务里 | 404 `failed_item_not_found` |
    /// | 该条已在重试 / 已解决 | 409 `failed_item_not_pending` |
    /// | 该条不是「JAV 视频 + 可人工处理的原因」 | 409 `failed_item_search_unavailable` |
    /// | 暂存文件信息已不可用 | 409 `failed_item_source_unavailable` |
    /// | 媒体库不存在 | 404 `media_library_not_found` |
    /// | 媒体库没有 provider 配置 | 422 `invalid_media_library_provider` |
    /// | 同一媒体库已有导入在跑 | 409 `import_task_conflict` |
    ///
    /// # 顺序是契约的一部分
    ///
    /// 候选校验（①）在**台账之前**：上游注释「入队即校验候选格式与插件启用
    /// 状态，避免用户拿到一个必然失败的任务」。所以「候选坏 + 任务不存在」报
    /// 的是**候选**的 422，不是 404。
    ///
    /// # `search` 为什么要传进来
    ///
    /// 候选校验要问「那个插件现在还启用吗」，而那要读配置 + 插件注册表；
    /// 本 crate 不做插件运行时（见 `Cargo.toml` 的说明），所以由调用方
    /// （路由，经 `AppState::metadata_search`）递进来。与
    /// [`MetadataSourceService::match_actors`] 收 `import_service` 同形。
    ///
    /// [`MetadataSourceService::match_actors`]: crate::catalog::metadata_source::MetadataSourceService::match_actors
    pub async fn enqueue_failed_item_retry(
        &self,
        search: &MovieMetadataSearchService,
        task_run_id: i32,
        item_id: &str,
        payload: &ImportFailedItemRetryRequest,
    ) -> Result<ImportAcceptedResponse, ServiceError> {
        payload.validate()?;
        let candidate_id = payload.candidate_id.trim();
        // ① 候选格式 + 插件启用。
        let _reference = search.resolve_candidate(candidate_id)?;

        // ② 台账：终态 → 找失败项 → 它可不可重试 → 源与库还在不在。
        let task_run = self.require_import_task_run(task_run_id).await?;
        ensure_retryable_task(&task_run)?;
        let items = failed_files(task_run_id, task_run.result_summary.as_deref())?;
        let raw = find_failure_item(task_run_id, &items, item_id)?;
        let stored: StoredFailedItem = serde_json::from_value(raw.clone())
            .map_err(|error| malformed_summary(task_run_id, format!("失败项形状不对：{error}")))?;
        ensure_searchable_failure_item(&stored)?;
        // 源文件信息还在吗？这里只做**存在性**判断 —— 值本身随整条 `raw`
        // 进 `params`（重试是一条自足的任务，worker 不回头读原任务）。
        // 判空的理由：暂存区可能已经被清理，那样这条重试必然失败，不如现在就
        // 告诉用户「重新浏览导入」。
        if !raw
            .get("source_ref")
            .is_some_and(|value| value.as_object().is_some_and(|map| !map.is_empty()))
        {
            return Err(ServiceError::conflict(
                "failed_item_source_unavailable",
                "失败项的源文件信息已不可用",
                None,
            ));
        }
        // `library_id` 必须是**整数**（`bool` 不算 —— Python 里 `True` 是 `int`，
        // 上游专门排掉了它；Rust 的 `as_i64` 对 JSON `true` 返回 `None`，
        // 天然满足）。缺了就 409：这是**库被写坏**，重试必然失败。
        let library_id = raw
            .get("library_id")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| {
                ServiceError::conflict(
                    "failed_item_source_unavailable",
                    "失败项缺少媒体库信息",
                    None,
                )
            })?;
        let library = require_library(&self.db, library_id).await?;

        // ③ 入队。互斥键按**库**（同库不能两个导入并行），与 `enqueue` 同键。
        let params = json!({
            "mode": IMPORT_MODE_RETRY_FAILED_FILE,
            "original_task_run_id": task_run.id,
            "failure_item_id": item_id,
            // 整条存储项进 params（含 `source_ref` / `library_id`）——
            // worker 不回头读原任务，重试是一条**自足**的任务。
            "failure_item": raw,
            "candidate_id": candidate_id,
        });
        let outcome = TaskQueueService::new(&self.db)
            .enqueue_with_mutex_key(
                TASK_KEY,
                RETRY_TRIGGER_TYPE,
                Some(RETRY_TASK_NAME),
                &library_import_mutex_key(i64::from(library.id)),
                Some(params),
                ConflictPolicy::Raise,
            )
            .await?;
        let retry_run = match outcome {
            EnqueueOutcome::Enqueued(run) => *run,
            EnqueueOutcome::Skipped {
                blocking_task_run_id,
            } => return Err(import_task_conflict(blocking_task_run_id)),
        };

        // ④ 回写失败项：`state=queued` + 指向新任务 + 清掉上次的错误。
        //
        // ★ **整段替换** `failed_files`（不是改其中一个元素的字段）：上层
        // 合并是顶层键覆盖，而这一列是数组 —— 所以先把新数组整个算出来。
        let patch = replace_failure_item(
            &items,
            item_id,
            json!({
                "state": ImportFailedItemState::Queued,
                "retry_task_run_id": retry_run.id,
                "last_retry_error": Value::Null,
            }),
        )?;
        let written = BackgroundTaskRunRepository::new(self.db.clone())
            .merge_result_summary(task_run.id, Some(&json!({ "failed_files": patch })))
            .await?;
        if !written {
            // 原任务行在入队与回写之间被删了。**撤销刚建的任务**再报 404 ——
            // 否则用户拿到 202，而那条重试指向一个已经不存在的失败项。
            self.abort_enqueued_run(retry_run.id).await;
            return Err(failed_item_not_found(item_id));
        }

        Ok(ImportAcceptedResponse {
            task_run_id: retry_run.id,
            task_key: retry_run.task_key.clone(),
            state: retry_run.state.clone(),
        })
    }

    /// ★ 执行体。worker 调用。上游 `execute`（`:279-310`）。
    ///
    /// 按 `params["mode"]` 分发（见模块文档）：
    ///
    /// | 条件 | 模式 | 上游 |
    /// |---|---|---|
    /// | `mode == "retry_failed_file"` | 重试一条失败项 | `_execute_failed_item_retry` |
    /// | `params` 里有 `download_tasks` | 批量 | `_execute_batch` |
    /// | 缺省 | 单个 | `_execute_single` |
    ///
    /// **未结束的 TaskRun 不得再执行**（`409 import_task_not_finished`）——
    /// 那会让同一批文件被导两次。检查点在重试模式：原任务必须已是终态
    /// （`ensure_retryable_task`）。单 / 批量模式下 params 里没有自己的
    /// run id（worker 领取时只领 pending 的行，执行中途也不会有人再领同一行），
    /// 所以这里没有可查的行 —— 不是漏了检查，是签名里就没有这个信息。
    ///
    /// 尾巴与上游一致：批量、或单个（带 `download_task_id` 的）且有新可播放
    /// 影片时发一次上新提醒（[`create_new_media_reminder`]）。
    ///
    /// ⚠️ `sm-scheduler` 的处理器注册表里还没有 `library_import`
    /// （`WorkerError::NoHandler`，文案「task_key 未在处理器注册表中」）——
    /// 本轮之后入队的任务会被 worker 领取、然后以 `NoHandler` 判失败。
    /// 这是阶段性事实，不是回归；接线时由组合根用
    /// [`Self::with_import_service`] 把 [`MediaImportService`] 装进来。
    pub async fn execute(&self, params: &Value) -> Result<ImportExecuteSummary, ServiceError> {
        let retry_mode =
            params.get("mode").and_then(Value::as_str) == Some(IMPORT_MODE_RETRY_FAILED_FILE);
        let batch_mode = !retry_mode && params.get("download_tasks").is_some();

        let summary = if retry_mode {
            self.execute_failed_item_retry(params).await?
        } else if batch_mode {
            self.execute_batch(params).await?
        } else {
            self.execute_single(params).await?
        };

        // 上游 execute 尾巴（`:297-309`）：只有批量、或带下载任务的单个才发提醒。
        let single_with_download_task = !batch_mode
            && params
                .get("download_task_id")
                .is_some_and(|value| !value.is_null());
        if (batch_mode || single_with_download_task) && !summary.new_playable_movies.is_empty() {
            self.send_new_media_reminder(&summary).await?;
        }
        Ok(summary)
    }

    /// 批量执行。上游 `_execute_batch`（`:314-377`）。
    ///
    /// `params["download_tasks"]` 的每一项都是一个完整的 single params
    /// （`enqueue_batch` 里宿主自己造的）。逐条调 [`Self::execute_single`]：
    /// **单条抛错只记数、不中断** —— 对应上游的 `except Exception:
    /// failed_task_count += 1`。
    ///
    /// ⚠️ 与上游的一处形状差异：上游返回的 dict 里还有 `download_task_count` /
    /// `processed_download_task_count` / `failed_download_task_count` 三个键，
    /// 本仓的 [`ImportExecuteSummary`] 没有这三个字段（它是上游 `ImportResult`
    /// 的形状，不是 batch dict 的形状），计数在这里算完即弃，只保留聚合后的
    /// 六个字段。
    async fn execute_batch(&self, params: &Value) -> Result<ImportExecuteSummary, ServiceError> {
        let items = params
            .get("download_tasks")
            .and_then(Value::as_array)
            .ok_or_else(|| ProgrammerError::new("download_tasks 必须是数组"))?;
        if items.is_empty() {
            // 上游 `ValueError("download_tasks must not be empty")` —— 调用方
            // （worker 代码）的 bug，不是用户请求的问题，走 500。
            return Err(ProgrammerError::new("download_tasks must not be empty").into());
        }
        let mut summary = ImportExecuteSummary::default();
        for item in items {
            match self.execute_single(item).await {
                Ok(single) => {
                    summary.imported_count += single.imported_count;
                    summary.skipped_count += single.skipped_count;
                    summary.failed_count += single.failed_count;
                    summary
                        .new_playable_movies
                        .extend(single.new_playable_movies);
                    summary.created_video_ids.extend(single.created_video_ids);
                    summary.failed_files.extend(single.failed_files);
                }
                Err(error) => {
                    // 这一条下载任务整体失败：execute_single 的 Err 路径里已经
                    // 把它的下载任务标了 failed，这里只记数、继续下一条。
                    warn!(
                        code = error.code(),
                        message = %error.api.message,
                        "批量导入中单条下载任务失败，已跳过继续下一条"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// 单个执行。上游 `_execute_single`（`:433-479`）。
    ///
    /// 流程：`params` → [`ImportRequest`]（非法 → 422 `validation_error`）→
    /// [`MediaImportService::import_from_source`] → 按结果回写下载任务状态 →
    /// 转成 [`ImportExecuteSummary`]。
    ///
    /// 下载任务状态映射（上游 `:469-475`）：`failed_count > 0` → `failed`；
    /// 否则 `imported > 0` → `completed`；否则 `skipped`。
    /// 导入本身抛错时先把下载任务标 `failed` 再把错抛出去（上游 `:466-468`）。
    ///
    /// ⚠️ `params["target_movie_number"]` 在这里**读不到下游**：上游把它传给
    /// `import_from_source` 做「只导这个番号」的过滤，本仓
    /// [`MediaImportService::import_from_source`] 的签名里没有这一项。键照常
    /// 留在 params 里，不拒；等执行层补上参数再透传。
    async fn execute_single(&self, item: &Value) -> Result<ImportExecuteSummary, ServiceError> {
        let request: ImportRequest = serde_json::from_value(item.clone())
            .map_err(|error| request_validation_error(&format!("导入请求参数非法：{error}")))?;
        request.validate()?;
        let download_task_id = item
            .get("download_task_id")
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok());

        let source_ref = Value::Object(request.source_ref.clone());
        let result = match self
            .require_import_service()?
            .import_from_source(
                &source_ref,
                i64::from(request.library_id),
                media_kind_value(request.media_kind),
                source_disposition_value(request.source_disposition),
                request.collection_id.map(i64::from),
                None,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                if let Some(task_id) = download_task_id {
                    DownloadTaskRepository::new(self.db.clone())
                        .set_import_status(task_id, import_status::FAILED, None)
                        .await?;
                }
                return Err(error);
            }
        };
        let failed_count = i64::try_from(result.failed.len()).unwrap_or(i64::MAX);
        let status = if failed_count > 0 {
            import_status::FAILED
        } else if result.imported > 0 {
            import_status::COMPLETED
        } else {
            import_status::SKIPPED
        };
        if let Some(task_id) = download_task_id {
            DownloadTaskRepository::new(self.db.clone())
                .set_import_status(task_id, status, None)
                .await?;
        }
        Ok(import_result_to_summary(result))
    }

    /// 重试一条失败项。上游 `_execute_failed_item_retry`（`:380-430`）。
    ///
    /// `params` 由 [`Self::enqueue_failed_item_retry`] 组装，是**自足**的：
    /// `original_task_run_id` / `failure_item_id` / `candidate_id` /
    /// `failure_item`（整条存储项，含 `source_ref` / `library_id`），worker
    /// 不回头读原任务的其它东西。
    ///
    /// 顺序照上游：先判原任务终态（`ensure_retryable_task`，未终态 →
    /// 409 `import_task_not_finished`，否则同一批文件会被导两次）→ 调
    /// [`MediaImportService::retry_failed_file`] → 回写原任务的失败项。
    ///
    /// 回写语义（上游 `_update_failure_item`，`:571-…`）：
    /// - 成功：`state=resolved`、`last_retry_error` 清掉；`retry_task_run_id`
    ///   **不覆写** —— 入队时已写成这次重试的 run id，这里写 null 反而丢信息。
    /// - 失败：`state` 打回 `pending`（可再重试）、`last_retry_error` 记错；
    ///   回写本身失败只记日志，**不吞掉原始错误**（上游 `:402-408` 的取向）。
    ///
    /// ⚠️ 两处与上游的形状差异（本仓 `retry_failed_file` 的返回缺口）：
    /// 上游返回 `{**result, "original_task_run_id", "failure_item_id",
    /// "candidate_id"}` 且 `result` 里有 `movie_id` / `media_id`（回写
    /// `resolved_movie_id` / `resolved_media_id` 用）；本仓只回
    /// `{"movie_number", "operation_key"}`，所以 `resolved_*` 写不进去，
    /// 返回的 [`ImportExecuteSummary`] 也只能按上游的 `summary_patch`
    /// （`{"imported_count": 1, "failed_count": 0}`）填。
    ///
    /// ⚠️ `operation_key`：上游是 `f"task:{task_run_id}:retry"`，用的是**这次
    /// 重试任务**的 run id；本执行体签名里拿不到它，退用
    /// `import-retry:{original_task_run_id}:{failure_item_id}` 拼一个稳定的键。
    async fn execute_failed_item_retry(
        &self,
        params: &Value,
    ) -> Result<ImportExecuteSummary, ServiceError> {
        let original_task_run_id = params
            .get("original_task_run_id")
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| request_validation_error("original_task_run_id 缺失或非法"))?;
        let failure_item_id = params
            .get("failure_item_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| request_validation_error("failure_item_id 缺失或非法"))?;
        let candidate_id = params
            .get("candidate_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| request_validation_error("candidate_id 缺失或非法"))?;
        let failure_item = params.get("failure_item").cloned().unwrap_or(Value::Null);

        let task_run = self.require_import_task_run(original_task_run_id).await?;
        ensure_retryable_task(&task_run)?;

        let failure: ImportFailure = serde_json::from_value(failure_item).map_err(|error| {
            request_validation_error(&format!("failure_item 形状非法：{error}"))
        })?;
        let operation_key = format!("import-retry:{original_task_run_id}:{failure_item_id}");
        let outcome = self
            .require_import_service()?
            .retry_failed_file(&failure, candidate_id, &operation_key)
            .await;

        // 原任务 `result_summary.failed_files` 整段替换（数组是顶层键覆盖，
        // 不能只改一个元素的字段 —— 见 `enqueue_failed_item_retry` 的注释）。
        let items = failed_files(original_task_run_id, task_run.result_summary.as_deref())?;
        match outcome {
            Ok(_) => {
                self.write_back_failure_item(
                    original_task_run_id,
                    &items,
                    failure_item_id,
                    json!({
                        "state": ImportFailedItemState::Resolved,
                        "last_retry_error": Value::Null,
                    }),
                )
                .await?;
                Ok(ImportExecuteSummary {
                    imported_count: 1,
                    ..Default::default()
                })
            }
            Err(error) => {
                let message = error.api.message.clone();
                if let Err(write_error) = self
                    .write_back_failure_item(
                        original_task_run_id,
                        &items,
                        failure_item_id,
                        json!({
                            "state": ImportFailedItemState::Pending,
                            "last_retry_error": message,
                        }),
                    )
                    .await
                {
                    warn!(
                        original_task_run_id,
                        failure_item_id,
                        write_error = ?write_error,
                        "失败项重试的错误回写失败，原始错误继续上抛"
                    );
                }
                Err(error)
            }
        }
    }

    /// 回写原任务 `result_summary.failed_files` 里的一条（整段替换）。
    ///
    /// 上游 `_update_failure_item`（`:571-…`）的本仓形状。
    async fn write_back_failure_item(
        &self,
        original_task_run_id: i32,
        items: &[Value],
        failure_item_id: &str,
        changes: Value,
    ) -> Result<(), ServiceError> {
        let patch = replace_failure_item(items, failure_item_id, changes)?;
        BackgroundTaskRunRepository::new(self.db.clone())
            .merge_result_summary(
                original_task_run_id,
                Some(&json!({ "failed_files": patch })),
            )
            .await?;
        Ok(())
    }

    /// 发「本次导入新增影片」提醒。上游 `execute` 尾巴的
    /// `create_new_media_reminder`（`:297-309`）。
    ///
    /// ⚠️ `related_task_run_id` 传 `None`：本执行体签名里拿不到自己的 run id，
    /// 按 [`create_new_media_reminder`] 的文档这会走「无幂等键」那条 ——
    /// 同一任务重放会多发一条提醒。等 `sm-scheduler` 接线 `library_import`
    /// 时把 run id 传进来再收掉这个缺口。
    ///
    /// ⚠️ 目前这条是**死代码**：本仓 [`MediaImportService::import_from_source`]
    /// 的返回里没有 `new_playable_movies`（见 [`import_result_to_summary`]），
    /// 条件恒为假。先按上游形状留着，等执行层补上字段自动生效。
    async fn send_new_media_reminder(
        &self,
        summary: &ImportExecuteSummary,
    ) -> Result<(), ServiceError> {
        let items: Vec<NewMovieReminderItem> = summary
            .new_playable_movies
            .iter()
            .filter_map(|value| serde_json::from_value(value.clone()).ok())
            .collect();
        let repo = SystemNotificationRepository::new(self.db.clone());
        create_new_media_reminder(&repo, &items, None).await?;
        Ok(())
    }

    /// 这个失败原因是否**不该**让用户自己背锅。
    pub fn is_manual_search_failure(reason: &str) -> bool {
        MANUAL_SEARCH_FAILURE_REASONS.contains(&reason)
    }

    /// 这一批下载任务**共同**的媒体库 id（上游 `{task.client.library_id}` 的集合）。
    ///
    /// 库挂在**下载器**上（`download_client.library_id`），不在任务上 ——
    /// 所以必须逐客户端回查。客户端数量是两位数量级，且这里对 `client_id`
    /// 先去重，不是 N+1。
    async fn single_library_id(
        &self,
        download_tasks: &[DownloadTask],
    ) -> Result<i32, ServiceError> {
        let mut client_ids: Vec<i32> = download_tasks.iter().map(|task| task.client_id).collect();
        client_ids.sort_unstable();
        client_ids.dedup();

        let clients = DownloadClientRepository::new(self.db.clone());
        let mut library_ids: Vec<i32> = Vec::with_capacity(client_ids.len());
        for client_id in client_ids {
            // 有外键兜着，查不到说明库被绕过写入破坏了 —— 属调用方的编程错误。
            let client = clients.find_by_id(client_id).await?.ok_or_else(|| {
                ProgrammerError::new(format!("下载任务引用了不存在的下载器 {client_id}"))
            })?;
            if !library_ids.contains(&client.library_id) {
                library_ids.push(client.library_id);
            }
        }
        if library_ids.len() != 1 {
            // 上游 `ValueError("download_tasks must belong to one media library")`。
            return Err(
                ProgrammerError::new("download_tasks must belong to one media library").into(),
            );
        }
        Ok(library_ids[0])
    }

    /// 取**导入**任务的台账行。上游 `_get_import_task_run`（`:472-477`）。
    ///
    /// # 两个条件，同一个 404
    ///
    /// 行不存在，**或者** `task_key` 不是 [`TASK_KEY`] —— 都报
    /// `404 import_task_not_found`。
    ///
    /// 后者不是洁癖：`/imports/{id}/failed-items` 的 `{id}` 是**裸整数**，
    /// 任何一个别的任务（缩略图、图搜、相似度）的 id 都能填进来。不加这道判，
    /// 那些任务的 `result_summary` 会被当成本任务的结果去解 —— 运气好是空列表
    /// （看起来「没有失败项」），运气不好是 500。
    ///
    /// ⚠️ 不复用 [`TaskRunService::get_task_run`]：它报的是 `task_run_not_found`
    /// （另一个码），而且它按 id 取、不看 `task_key`。
    async fn require_import_task_run(
        &self,
        task_run_id: i32,
    ) -> Result<BackgroundTaskRun, ServiceError> {
        let not_found = || {
            // 上游这里只有码与文案，**没有 details**（`ApiError(404, code, msg)`
            // 的 details 是空对象）。
            ServiceError::from_status(404, "import_task_not_found", "导入任务不存在")
        };
        let task_run = BackgroundTaskRunRepository::new(self.db.clone())
            .find_by_id(task_run_id)
            .await?
            .ok_or_else(not_found)?;
        if task_run.task_key != TASK_KEY {
            return Err(not_found());
        }
        Ok(task_run)
    }

    /// 取下载任务，查不到按上游落 **502**（见 [`Self::enqueue`] 的说明）。
    async fn require_download_task(&self, task_id: i32) -> Result<DownloadTask, ServiceError> {
        match DownloadTaskRepository::new(self.db.clone())
            .find_by_id(task_id)
            .await?
        {
            Some(task) => Ok(task),
            None => Err(import_task_create_failed(format!(
                "下载任务 {task_id} 不存在"
            ))),
        }
    }

    /// 补偿：把刚建出来、还什么都没跑的 TaskRun 判为失败。
    ///
    /// 它同时是**释放互斥键**的那一步（终态转移把 `mutex_key` 置 `NULL`），
    /// 所以哪怕这一步失败也不能静默 —— 但也不能因此把原本的 409/502 换成
    /// 一个「补偿失败」的新错误：调用方要的是上游那个码。失败时只记一行日志。
    ///
    /// `notify_result = false`：上游在这种情形下**没有** run 行，也就不会发
    /// 通知；这里虽然留下了行，但用户已经在响应里看到了 409/502。
    async fn abort_enqueued_run(&self, task_run_id: i32) {
        let failed = TaskRunService::new(&self.db)
            .fail_task_run(
                task_run_id,
                "入队后回写下载任务失败，已撤销受理（该任务未执行）",
                None,
                false,
            )
            .await;
        if let Err(error) = failed {
            tracing::error!(
                task_run_id,
                error = %error.code(),
                "撤销入队失败：互斥键可能仍被占用"
            );
        }
    }
}

/// 缺省的显示名。上游 `_task_name`（`:576-579`）。
///
/// `kind = "JAV" if media_kind == "jav" else "视频"` —— **大写 JAV 与中文
/// 「视频」**，任务中心直接显示这个串。
fn default_task_name(request: &ImportRequest) -> String {
    let kind = match request.media_kind {
        MediaKind::Jav => "JAV",
        MediaKind::Video => "视频",
    };
    format!("{kind}媒体库导入")
}

/// 取库 + 校验 provider 配置。上游 `_validated_request`（`:567-574`）。
///
/// ⚠️ provider 的判据是**逐字 `== ""`**，不 trim：一个 `" "` 的
/// `provider_key` 在上游算「配了」。顺手 trim 会让一个上游允许的库变成 422，
/// 而报的却是「缺少 provider_key」—— 用户看着库里明明填了东西。
async fn require_library(db: &Db, library_id: i32) -> Result<MediaLibrary, ServiceError> {
    let library = MediaLibraryRepository::new(db.clone())
        .find_by_id(library_id)
        .await?
        .ok_or_else(|| {
            ServiceError::not_found(
                "media_library_not_found",
                "媒体库不存在",
                "library_id",
                library_id,
            )
        })?;
    if library.provider_key.is_empty() {
        return Err(ServiceError::validation(
            "invalid_media_library_provider",
            "媒体库缺少 provider_key",
        ));
    }
    Ok(library)
}

/// `result_summary` 里读不出预期形状时的 500。
///
/// 用 `from_status` 而不是 `validation`：这不是调用方输入的问题，是**库里的
/// 那一列**形状不对 —— 客户端重试一万次也一样。
fn malformed_summary(task_run_id: i32, detail: impl std::fmt::Display) -> ServiceError {
    ServiceError::from_status(
        500,
        "internal_error",
        format!("导入任务 {task_run_id} 的 result_summary 形状不对：{detail}"),
    )
}

/// 台账里记着的失败文件。上游
/// `(task_run.result_summary or {}).get("failed_files", [])`
/// （`import_task_service.py:178`）。
///
/// | 列里的值 | 结果 |
/// |---|---|
/// | 空串 / `null` / 没有 `failed_files` 这个键 | 空列表（**不是错误**）|
/// | 不是 JSON、不是对象、`failed_files` 不是数组 | **500** |
///
/// 最后一档刻意不做成「当作空列表」：那会让「这一趟导入全失败了」与「一切正常」
/// 长得一模一样，而用户再也没有入口去重试那些文件。这一列的形状是宿主自己写的
/// （[`super::import_service`] 的 `failed_files`），读不出来就是缺陷。
///
/// 只收那一列的值（不是整行）—— 这条判据与台账的其它列无关，
/// 也省得为了测它去拼一个 30 字段的行结构。
fn failed_files(
    task_run_id: i32,
    result_summary: Option<&str>,
) -> Result<Vec<Value>, ServiceError> {
    let Some(raw) = result_summary.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(Vec::new());
    };
    let summary: Value =
        serde_json::from_str(raw).map_err(|error| malformed_summary(task_run_id, error))?;
    let Value::Object(summary) = summary else {
        // `null` 与「没有摘要」同义（上游 `or {}`）；其它形状是写坏了。
        if summary.is_null() {
            return Ok(Vec::new());
        }
        return Err(malformed_summary(
            task_run_id,
            format!("摘要不是对象：{summary}"),
        ));
    };
    match summary.get("failed_files") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(other) => Err(malformed_summary(
            task_run_id,
            format!("failed_files 不是数组：{other}"),
        )),
    }
}

/// `failed_files[]` 里一条的**存储形状**
/// （写侧是上游 `_make_failure_item`，`imports/import_service.py:730-759`）。
///
/// 字段与 [`ImportFailedItemResource`] 一一对应，只多一个 `media_kind`
/// （算 `can_manual_search` 用，**不外发**）。`source_ref` / `library_id` /
/// `name` / `source_disposition` 也躺在那一行里，但它们是**重试路径**要用的，
/// 不属这个投影 —— 这里刻意不声明，免得有人顺手把它们发出去
/// （`source_ref` 里可能有宿主的真实路径）。
///
/// # 为什么用 serde 而不是手挖键
///
/// 上游是 `item["x"]`：缺键直接 `KeyError` → 500。手写的话要么漏判，要么把
/// 「缺哪个键」硬编进每条错误消息。交给 serde 报，它的消息自带**字段名与取值**。
/// 缺键 / 类型不对都落 500，与上游同一个状态码。
#[derive(Debug, Clone, Deserialize)]
struct StoredFailedItem {
    id: String,
    relative_path: String,
    size_bytes: i64,
    is_video: bool,
    reason: String,
    detail: String,
    kind: String,
    /// 缺省 `pending` —— 上游另外两处判据都是 `.get("state", "pending")`。
    #[serde(default)]
    state: ImportFailedItemState,
    retry_task_run_id: Option<i32>,
    resolved_movie_id: Option<i64>,
    resolved_media_id: Option<i64>,
    last_retry_error: Option<String>,
    /// 只用于算 [`Self::can_manual_search`]。
    media_kind: String,
}

impl StoredFailedItem {
    /// 这一条**能不能**人工搜元数据
    /// （上游 `_failure_item_resource` 里那段表达式，`:530-535`）。
    ///
    /// 四个条件缺一不可，前三条挡的是「点了也没意义」，第四条挡的是
    /// 「点了也修不好」（只有两个原因是用户自己能解决的：换个番号、重试）：
    ///
    /// | 条件 | 为什么 |
    /// |---|---|
    /// | `state == pending` | 已在重试 / 已解决的条目没有搜索入口 |
    /// | `is_video` | 目录、封面、字幕不参与元数据匹配 |
    /// | `media_kind == jav` | 非 JAV 不走番号刮削 |
    /// | `reason ∈ {movie_number_not_found, metadata_fetch_failed}` | 见 [`MANUAL_SEARCH_FAILURE_REASONS`] |
    fn can_manual_search(&self) -> bool {
        self.state == ImportFailedItemState::Pending && self.is_searchable_kind()
    }

    /// 上一条判据去掉「状态」那一项（后三个条件）。
    ///
    /// 单独拆出来是因为重试路径要把这两组**报成不同的码**：
    /// `failed_item_not_pending`（有人正在处理）与
    /// `failed_item_search_unavailable`（这条根本不适合人工处理）——
    /// 合成一个 `can_manual_search` 就分不出来，而客户端对这两个的提示词不同
    /// （「等它跑完」vs「这条修不了，换个源」）。
    fn is_searchable_kind(&self) -> bool {
        self.is_video
            && self.media_kind == super::import_service::media_kind::JAV
            && MANUAL_SEARCH_FAILURE_REASONS.contains(&self.reason.as_str())
    }

    /// 投影成响应体（`media_kind` 不外发）。
    fn into_resource(self) -> ImportFailedItemResource {
        let can_manual_search = self.can_manual_search();
        ImportFailedItemResource {
            id: self.id,
            relative_path: self.relative_path,
            size_bytes: self.size_bytes,
            is_video: self.is_video,
            reason: self.reason,
            detail: self.detail,
            kind: self.kind,
            state: self.state,
            retry_task_run_id: self.retry_task_run_id,
            resolved_movie_id: self.resolved_movie_id,
            resolved_media_id: self.resolved_media_id,
            last_retry_error: self.last_retry_error,
            can_manual_search,
        }
    }
}

/// 一条失败项的投影。上游 `_failure_item_resource`（`:514-536`）。
///
/// ⚠️ `kind` 是**原样读**存储里的值，不重算 —— 见
/// [`sm_db::transfers::downloads::failed_file_kind`]。
fn failure_item_resource(
    task_run_id: i32,
    item: Value,
) -> Result<ImportFailedItemResource, ServiceError> {
    serde_json::from_value::<StoredFailedItem>(item)
        .map(StoredFailedItem::into_resource)
        .map_err(|error| malformed_summary(task_run_id, format!("失败项形状不对：{error}")))
}

/// 任务必须在**终态**才能重试它里面的失败项。上游 `_ensure_retryable_task`
/// （`import_task_service.py:492-495`）。
///
/// 判据用 [`task_state::is_terminal`]（`completed || failed`）而不是
/// 「非 active」：两者在当前状态机下等价，但**语义不同** —— 将来若加了
/// `cancelled`，它应当继续被拒（那次导入没跑完就中止了，里面的失败项不该
/// 由用户逐条重试），而不是因为「不是 active」被放进来。
fn ensure_retryable_task(task_run: &BackgroundTaskRun) -> Result<(), ServiceError> {
    if task_state::is_terminal(&task_run.state) {
        return Ok(());
    }
    Err(ServiceError::conflict(
        "import_task_not_finished",
        "导入任务尚未完成",
        None,
    ))
}

/// [`MediaKind`] → [`MediaImportService::import_from_source`] 要的 `&str`
///（`"jav"` / `"video"`）。
fn media_kind_value(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Jav => "jav",
        MediaKind::Video => "video",
    }
}

/// [`SourceDisposition`] → [`MediaImportService::import_from_source`] 要的
/// `&str`（`"keep"` / `"delete_after_commit"` / `"in_place"`）。
fn source_disposition_value(disposition: SourceDisposition) -> &'static str {
    match disposition {
        SourceDisposition::Keep => "keep",
        SourceDisposition::DeleteAfterCommit => "delete_after_commit",
        SourceDisposition::InPlace => "in_place",
    }
}

/// [`super::import_service::ImportResult`] → [`ImportExecuteSummary`]。
///
/// ⚠️ 上游 `import_from_source` 返回的 `ImportResult` 有六个字段；本仓执行层
/// 的 `ImportResult` 只有 `{imported, skipped, failed: Vec<ImportFailure>}`：
/// `new_playable_movies` / `created_video_ids` 在这里**没有来源**，置空 ——
/// 等执行层补上字段再填（[`ImportTaskService::send_new_media_reminder`] 的
/// 条件届时自动生效）。`failed_files` 取逐条失败的序列化（键名与上游逐字一致，
/// 见 [`ImportFailure`] 的文档）。
fn import_result_to_summary(result: ImportResult) -> ImportExecuteSummary {
    ImportExecuteSummary {
        imported_count: i64::from(result.imported),
        skipped_count: i64::from(result.skipped),
        failed_count: i64::try_from(result.failed.len()).unwrap_or(i64::MAX),
        new_playable_movies: Vec::new(),
        created_video_ids: Vec::new(),
        failed_files: result
            .failed
            .iter()
            .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
            .collect(),
    }
}

/// 404 `failed_item_not_found`。
///
/// **不带 details**（上游 `ApiError(404, …)` 的 details 是空对象）—— 与
/// [`Self::require_import_task_run`] 的 `import_task_not_found` 同一取向：
/// `item_id` 已经在路径里回显过了，再塞进 details 只是多一个客户端不读的键。
fn failed_item_not_found(_item_id: &str) -> ServiceError {
    ServiceError::from_status(404, "failed_item_not_found", "导入失败项不存在")
}

/// 从存储里的失败项数组里按 `id` 取一条。上游 `_find_failure_item`
/// （`:497-501`）。
///
/// # 元素必须是对象
///
/// 上游是 `item.get("id")`：元素不是 dict 就 `AttributeError` → **500**。
/// 这里同样不把非对象元素当「没匹配上」跳过 —— 那一列的形状是宿主自己写的
/// （[`failed_files`] 的文档），混进一个字符串就是缺陷，静默跳过会让「失败项
/// 列表少一条」与「本来就没有」长得一样。
fn find_failure_item(
    task_run_id: i32,
    items: &[Value],
    item_id: &str,
) -> Result<Value, ServiceError> {
    for item in items {
        if !item.is_object() {
            return Err(malformed_summary(
                task_run_id,
                format!("failed_files 里的元素不是对象：{item}"),
            ));
        }
        if item.get("id").and_then(Value::as_str) == Some(item_id) {
            return Ok(item.clone());
        }
    }
    Err(failed_item_not_found(item_id))
}

/// 这一条**现在**能不能重试。上游 `_ensure_searchable_failure_item`
/// （`:503-512`）—— 与人工搜索同一道闸，两个不同的码。
fn ensure_searchable_failure_item(item: &StoredFailedItem) -> Result<(), ServiceError> {
    if item.state != ImportFailedItemState::Pending {
        return Err(ServiceError::conflict(
            "failed_item_not_pending",
            "失败项当前不在待处理状态",
            None,
        ));
    }
    if !item.is_searchable_kind() {
        return Err(ServiceError::conflict(
            "failed_item_search_unavailable",
            "该失败项不支持手动元数据搜索",
            None,
        ));
    }
    Ok(())
}

/// 整段替换 `failed_files` 里的一条（`changes` 覆盖同名键）。上游
/// `_replace_failure_item`（`:539-556`）。
///
/// 返回**新的整个数组**（不是就地改）：`result_summary` 的合并是顶层键覆盖，
/// 数组要整段给出去。
///
/// 找不到那一条 → 404（上游同一处）。理论上 [`find_failure_item`] 刚找到过，
/// 走到这里说明两次调用之间被并发改掉了 —— 报 404 而不是 500：对用户来说
/// 「这条不见了」就是 404。
fn replace_failure_item(
    items: &[Value],
    item_id: &str,
    changes: Value,
) -> Result<Vec<Value>, ServiceError> {
    let Value::Object(changes) = changes else {
        return Err(ProgrammerError::new("失败项改动必须是对象").into());
    };
    let mut updated = Vec::with_capacity(items.len());
    let mut replaced = false;
    for item in items {
        if item.get("id").and_then(Value::as_str) != Some(item_id) {
            updated.push(item.clone());
            continue;
        }
        let mut merged = item.as_object().cloned().unwrap_or_default();
        for (key, value) in &changes {
            merged.insert(key.clone(), value.clone());
        }
        updated.push(Value::Object(merged));
        replaced = true;
    }
    if !replaced {
        return Err(failed_item_not_found(item_id));
    }
    Ok(updated)
}

/// 下载任务的完成源引用。上游直接读 `task.completed_source_ref`（已是 dict）。
///
/// 库里的那一列是 JSON **文本**，所以这里解析。解析不出来按「库被写坏」处理：
/// 500 `internal_error`，与
/// [`super::download_task::DownloadTaskService::trigger_import`] 同一个口径 ——
/// 不静默塞个空对象让它一路跑到 provider 那里才炸。
///
/// **公开**：单条导入（`trigger_import`）与批量导入用的是同一个判据，
/// 两处各写一份迟早会漂移（一处 trim、一处不 trim）。
pub fn completed_source_ref(task: &DownloadTask) -> Result<Map<String, Value>, ServiceError> {
    let raw = task
        .completed_source_ref
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if raw.is_empty() {
        return Err(ServiceError::from_status(
            500,
            "internal_error",
            format!(
                "下载任务 {} 处于导入前状态：completed_source_ref 为空",
                task.id
            ),
        ));
    }
    // 上游的 `source_ref` 是 `dict`，所以这里按对象解析：解析出数组或标量
    // 说明这一列的形状不对，与解析失败同样处理。
    serde_json::from_str(raw)
        .map_err(|error| ServiceError::from_status(500, "internal_error", error.to_string()))
}

/// 409 `import_task_conflict`。details 里**永远**带 `blocking_task_run_id`
/// （查不到时为 `null`）—— 上游如此，客户端靠它提示「正在跑的是哪一条」。
fn import_task_conflict(blocking_task_run_id: Option<i32>) -> ServiceError {
    ServiceError::conflict(
        "import_task_conflict",
        "同一媒体库已有导入任务",
        Some(details_of(
            "blocking_task_run_id",
            json!(blocking_task_run_id),
        )),
    )
}

/// 502 `import_task_create_failed`。details 里带 `detail`（上游 `str(exc)`）。
fn import_task_create_failed(detail: impl Into<String>) -> ServiceError {
    ServiceError::bad_gateway(
        "import_task_create_failed",
        "媒体导入任务入队失败",
        details_of("detail", detail.into()),
    )
}

/// 422 `validation_error` + 单键 details（pydantic 的字段级失败）。
fn field_validation_error(field: &str, value: impl Into<Value>) -> ServiceError {
    ServiceError::validation_with(
        VALIDATION_ERROR,
        VALIDATION_ERROR_MESSAGE,
        details_of(field, value.into()),
    )
}

/// 422 `validation_error` + 纯文本 details（pydantic 的模型级失败）。
fn request_validation_error(detail: &str) -> ServiceError {
    ServiceError::validation_with(
        VALIDATION_ERROR,
        VALIDATION_ERROR_MESSAGE,
        details_of("detail", detail),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jav_request() -> ImportRequest {
        ImportRequest {
            media_kind: MediaKind::Jav,
            library_id: 7,
            source_ref: serde_json::json!({"path": "a/b"})
                .as_object()
                .cloned()
                .expect("对象"),
            source_disposition: SourceDisposition::Keep,
            collection_id: None,
        }
    }

    fn serialized_keys<T: Serialize>(value: &T) -> Vec<String> {
        let json = serde_json::to_value(value).expect("可序列化");
        let mut keys: Vec<String> = json
            .as_object()
            .expect("资源是对象")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// 手动可重试的原因**只有两个**。
    ///
    /// 多加一个（比如 `is_collection`）会让「用户点了但导入不进去」被当作
    /// 系统错误走告警，而那其实是正常跳过。
    #[test]
    fn only_two_reasons_count_as_user_fixable() {
        assert!(ImportTaskService::is_manual_search_failure(
            "movie_number_not_found"
        ));
        assert!(ImportTaskService::is_manual_search_failure(
            "metadata_fetch_failed"
        ));
        assert!(!ImportTaskService::is_manual_search_failure(
            "is_collection"
        ));
        assert!(!ImportTaskService::is_manual_search_failure("stage_failed"));
    }

    /// 任务键必须与 `lane_of` 的专属道一致 —— 改成别的会让导入退回 default 道。
    #[test]
    fn the_task_key_matches_the_dedicated_lane() {
        assert_eq!(TASK_KEY, "library_import");
    }

    /// `media_kind` 的线上取值是**小写**。
    ///
    /// 骨架期是大写 `JAV` / `VIDEO`，而前端传小写 —— 每一次导入都会 422。
    #[test]
    fn the_media_kind_wire_values_are_lowercase() {
        assert_eq!(
            serde_json::to_value(MediaKind::Jav).expect("可序列化"),
            serde_json::json!("jav")
        );
        assert_eq!(
            serde_json::to_value(MediaKind::Video).expect("可序列化"),
            serde_json::json!("video")
        );
        assert!(serde_json::from_value::<MediaKind>(serde_json::json!("JAV")).is_err());
    }

    /// `source_disposition` 的三个取值逐字是上游那三个，且**没有** `move`。
    #[test]
    fn the_source_disposition_has_upstreams_three_values_only() {
        let values = ["keep", "delete_after_commit", "in_place"];
        for value in values {
            assert!(
                serde_json::from_value::<SourceDisposition>(serde_json::json!(value)).is_ok(),
                "{value} 必须被接受"
            );
        }
        assert!(
            serde_json::from_value::<SourceDisposition>(serde_json::json!("move")).is_err(),
            "move 是骨架期自造的取值"
        );
    }

    /// 缺省值是 `keep`：请求里不写这一项时**不能**变成解析失败。
    #[test]
    fn the_source_disposition_defaults_to_keep() {
        assert_eq!(SourceDisposition::default(), SourceDisposition::Keep);
        let request: ImportRequest = serde_json::from_value(serde_json::json!({
            "media_kind": "jav",
            "library_id": 1,
            "source_ref": {},
        }))
        .expect("缺省 source_disposition 必须可解析");
        assert_eq!(request.source_disposition, SourceDisposition::Keep);
        assert!(request.collection_id.is_none());
    }

    /// `source_ref` 必须是**对象**（上游是 `dict`）。
    ///
    /// 允许标量会让一个 `"path/to/dir"` 一路走到 provider 才炸。
    #[test]
    fn the_source_ref_must_be_an_object() {
        let bad = serde_json::json!({
            "media_kind": "jav",
            "library_id": 1,
            "source_ref": "not-a-dict",
        });
        assert!(serde_json::from_value::<ImportRequest>(bad).is_err());
    }

    /// 受理响应**三个键都要发**（骨架期只有 `task_run_id`）。
    #[test]
    fn the_accepted_response_carries_task_key_and_state() {
        let accepted = ImportAcceptedResponse {
            task_run_id: 42,
            task_key: TASK_KEY.to_owned(),
            state: "pending".to_owned(),
        };
        assert_eq!(
            serialized_keys(&accepted),
            ["state", "task_key", "task_run_id"]
        );
    }

    /// `jav` 不许带合集 —— 上游的 `model_validator`。
    #[test]
    fn jav_imports_reject_a_collection_id() {
        let mut request = jav_request();
        request.collection_id = Some(3);
        let error = request.validate().expect_err("必须被拒");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), VALIDATION_ERROR);
        assert_eq!(
            error.details(),
            Some(&details_of(
                "detail",
                "jav import does not support collection_id"
            ))
        );
    }

    /// `video` **可以**带合集（同一份 validator 的另一半）。
    #[test]
    fn video_imports_accept_a_collection_id() {
        let mut request = jav_request();
        request.media_kind = MediaKind::Video;
        request.collection_id = Some(3);
        assert!(request.validate().is_ok());
    }

    /// id 必须为正（上游 `Field(gt=0)`），且**先报字段、后报模型**。
    #[test]
    fn the_ids_must_be_positive() {
        let mut request = jav_request();
        request.library_id = 0;
        assert_eq!(request.validate().expect_err("必须被拒").status, 422);

        let mut request = jav_request();
        request.media_kind = MediaKind::Video;
        request.collection_id = Some(0);
        let error = request.validate().expect_err("必须被拒");
        assert!(error
            .details()
            .expect("有 details")
            .contains_key("collection_id"));
    }

    /// `params` 的键就是上游 `model_dump()` 那五个 —— worker 按它们读参数。
    #[test]
    fn the_request_params_carry_upstreams_keys() {
        let params = jav_request().params_object();
        let mut keys: Vec<&String> = params.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "collection_id",
                "library_id",
                "media_kind",
                "source_disposition",
                "source_ref"
            ]
        );
        assert_eq!(params["media_kind"], serde_json::json!("jav"));
    }

    /// 缺省任务显示名：JAV 用大写 `JAV`，非 JAV 用「视频」。
    #[test]
    fn the_default_task_name_follows_upstreams_two_branches() {
        assert_eq!(default_task_name(&jav_request()), "JAV媒体库导入");
        let mut video = jav_request();
        video.media_kind = MediaKind::Video;
        assert_eq!(default_task_name(&video), "视频媒体库导入");
    }

    /// 失败项资源的字段集合与上游逐字一致（13 个）。
    ///
    /// 骨架期那五个自造字段（`item_id` / `movie_number` / `failure_reason` /
    /// `failure_detail` / `attempts`）**一个都不能出现** —— 客户端按上游名字
    /// 解析，多一个少一个都是契约破坏。
    #[test]
    fn the_failed_item_resource_matches_upstreams_thirteen_keys() {
        let item = ImportFailedItemResource {
            id: "seed:ABC-123:0".to_owned(),
            relative_path: "a/b.mkv".to_owned(),
            size_bytes: 1024,
            is_video: true,
            reason: "metadata_fetch_failed".to_owned(),
            detail: String::new(),
            kind: "file".to_owned(),
            state: ImportFailedItemState::Pending,
            retry_task_run_id: None,
            resolved_movie_id: None,
            resolved_media_id: None,
            last_retry_error: None,
            can_manual_search: true,
        };
        assert_eq!(
            serialized_keys(&item),
            [
                "can_manual_search",
                "detail",
                "id",
                "is_video",
                "kind",
                "last_retry_error",
                "reason",
                "relative_path",
                "resolved_media_id",
                "resolved_movie_id",
                "retry_task_run_id",
                "size_bytes",
                "state",
            ]
        );
    }

    /// 失败项的处置状态是**自己的**三个取值，缺省 `pending`。
    #[test]
    fn the_failed_item_state_is_its_own_little_machine() {
        assert_eq!(
            ImportFailedItemState::default(),
            ImportFailedItemState::Pending
        );
        assert_eq!(
            serde_json::to_value(ImportFailedItemState::Queued).expect("可序列化"),
            serde_json::json!("queued")
        );
    }

    /// 搜索响应**没有** `item_id`，但**有** `source_errors`。
    ///
    /// 少了 `source_errors`，客户端就无法区分「没搜到」与「元数据源挂了」。
    #[test]
    fn the_search_response_has_candidates_and_source_errors() {
        let response = ImportMetadataSearchResponse {
            movie_number: "ABC-123".to_owned(),
            candidates: Vec::new(),
            source_errors: vec![ImportMetadataSourceErrorResource {
                source: "javdb".to_owned(),
                source_name: "JavDB".to_owned(),
                reason: "unavailable".to_owned(),
                detail: "timeout".to_owned(),
            }],
        };
        assert_eq!(
            serialized_keys(&response),
            ["candidates", "movie_number", "source_errors"]
        );
    }

    /// 候选的字段集合是上游那 10 个，且**没有**骨架期自造的
    /// `confidence` / `date`。
    #[test]
    fn the_candidate_matches_upstreams_ten_keys() {
        let candidate = MetadataCandidate {
            candidate_id: "c1".to_owned(),
            source: MetadataCandidateSource::Javdb,
            source_name: "JavDB".to_owned(),
            source_id: None,
            javdb_id: Some("abc".to_owned()),
            movie_number: "ABC-123".to_owned(),
            title: "t".to_owned(),
            cover_url: None,
            release_date: Some("2024-01-01".to_owned()),
            duration_minutes: 120,
        };
        assert_eq!(
            serialized_keys(&candidate),
            [
                "candidate_id",
                "cover_url",
                "duration_minutes",
                "javdb_id",
                "movie_number",
                "release_date",
                "source",
                "source_id",
                "source_name",
                "title",
            ]
        );
        assert_eq!(
            serde_json::to_value(MetadataCandidateSource::Javdb).expect("可序列化"),
            serde_json::json!("javdb")
        );
    }

    /// 重试请求只收一个不透明的 `candidate_id`（必填）。
    #[test]
    fn the_retry_request_takes_only_a_candidate_id() {
        let payload: ImportFailedItemRetryRequest =
            serde_json::from_value(serde_json::json!({"candidate_id": "c1"})).expect("可解析");
        assert_eq!(payload.candidate_id, "c1");
        assert!(
            serde_json::from_value::<ImportFailedItemRetryRequest>(serde_json::json!({})).is_err(),
            "candidate_id 是必填"
        );
        assert!(payload.validate().is_ok());
    }

    /// 空白串过不了那两个请求体的校验，但**先**要过长度约束。
    #[test]
    fn blank_search_inputs_are_rejected() {
        let blank = ImportMetadataSearchRequest {
            movie_number: "   ".to_owned(),
        };
        assert_eq!(blank.validate().expect_err("必须被拒").status, 422);

        let too_long = ImportMetadataSearchRequest {
            movie_number: "x".repeat(MOVIE_NUMBER_MAX_LENGTH + 1),
        };
        assert!(too_long.validate().is_err());

        let blank_candidate = ImportFailedItemRetryRequest {
            candidate_id: "  ".to_owned(),
        };
        assert!(blank_candidate.validate().is_err());
    }

    /// 执行摘要的字段名与上游 `ImportResult` 逐字一致。
    #[test]
    fn the_execute_summary_uses_upstreams_field_names() {
        let summary = ImportExecuteSummary {
            imported_count: 3,
            skipped_count: 1,
            failed_count: 2,
            new_playable_movies: vec![serde_json::json!({"id": 1})],
            created_video_ids: vec![9],
            failed_files: vec![serde_json::json!({"id": "x"})],
        };
        assert_eq!(
            serialized_keys(&summary),
            [
                "created_video_ids",
                "failed_count",
                "failed_files",
                "imported_count",
                "new_playable_movies",
                "skipped_count",
            ]
        );
    }

    /// 互斥键必须按**库**取，不能退回 `aps:{task_key}`。
    ///
    /// 这条把「按库并行」钉在测试里：一旦有人把入队改成默认入口，
    /// 两个库的导入就会被串行化，而那种退化不会报错。
    #[test]
    fn the_mutex_key_is_per_library_not_per_task_key() {
        let key = library_import_mutex_key(7);
        assert_eq!(key, "library_import:7");
        assert_ne!(key, TaskQueueService::mutex_key(TASK_KEY));
        assert_ne!(library_import_mutex_key(7), library_import_mutex_key(8));
    }

    // ------------------------------------------------ 失败项的读取（result_summary）

    /// 一条**完整**的存储形状失败项 —— 键与上游 `_make_failure_item`
    /// （`imports/import_service.py:730-759`）落的那些一致。
    fn stored_item() -> Value {
        serde_json::json!({
            "id": "seed:ABC-123:0",
            "name": "ABC-123.mkv",
            "relative_path": "人妻/ABC-123.mkv",
            "size_bytes": 1024,
            "is_video": true,
            "source_ref": {"path": "人妻/ABC-123.mkv"},
            "library_id": 7,
            "media_kind": "jav",
            "source_disposition": "keep",
            "path": "人妻/ABC-123.mkv",
            "reason": "metadata_fetch_failed",
            "detail": "boom",
            "kind": "file",
            "state": "pending",
            "retry_task_run_id": null,
            "resolved_movie_id": null,
            "resolved_media_id": null,
            "last_retry_error": null,
        })
    }

    /// 改几个键再投影（省得每处都写一遍完整 JSON）。
    fn project_with(patch: &[(&str, Value)]) -> ImportFailedItemResource {
        let mut item = stored_item();
        for (key, value) in patch {
            item[key] = value.clone();
        }
        failure_item_resource(1, item).expect("可投影")
    }

    /// 存储形状 -> 13 字段资源，且**宿主内部字段不外发**。
    #[test]
    fn a_stored_failed_item_projects_into_the_resource() {
        let resource = project_with(&[]);
        assert_eq!(resource.id, "seed:ABC-123:0");
        assert_eq!(resource.relative_path, "人妻/ABC-123.mkv");
        assert_eq!(resource.size_bytes, 1024);
        assert!(resource.is_video);
        assert_eq!(resource.reason, "metadata_fetch_failed");
        assert_eq!(resource.detail, "boom");
        assert_eq!(resource.kind, "file");
        assert_eq!(resource.state, ImportFailedItemState::Pending);
        assert!(resource.retry_task_run_id.is_none());
        assert!(resource.resolved_movie_id.is_none());
        assert!(resource.resolved_media_id.is_none());
        assert!(resource.last_retry_error.is_none());
        assert!(resource.can_manual_search);

        // `source_ref` 里可能有宿主的真实路径，`media_kind` / `library_id` /
        // `name` / `source_disposition` / `path` 都是内部字段 —— 一个都不许外发。
        let json = serde_json::to_value(&resource).expect("可序列化");
        for hidden in [
            "source_ref",
            "library_id",
            "media_kind",
            "name",
            "source_disposition",
            "path",
        ] {
            assert!(json.get(hidden).is_none(), "{hidden} 不该出现在响应里");
        }
    }

    /// `can_manual_search` 的**四个条件**缺一不可。
    #[test]
    fn can_manual_search_needs_all_four_conditions() {
        assert!(project_with(&[]).can_manual_search, "基准：四条都满足");

        for (key, value) in [
            ("state", serde_json::json!("queued")),
            ("state", serde_json::json!("resolved")),
            ("is_video", serde_json::json!(false)),
            ("media_kind", serde_json::json!("video")),
            ("reason", serde_json::json!("file_too_small")),
            ("reason", serde_json::json!("media_import_failed")),
        ] {
            assert!(
                !project_with(&[(key, value.clone())]).can_manual_search,
                "破掉 {key}={value} 之后不该还能搜"
            );
        }

        // 另一个可搜的原因（用户能自己解决的只有两个）。
        assert!(
            project_with(&[("reason", serde_json::json!("movie_number_not_found"))])
                .can_manual_search
        );
    }

    /// `kind` 是**存储里的原值**，不是重算出来的。
    ///
    /// 这里故意给一个分类表算不出来的组合：`metadata_fetch_failed` 该落
    /// `file`，而存储里写的是 `warning`。读侧必须原样报 `warning` ——
    /// 重算会让「改一次分类表」与存量数据不一致，表现为客户端给出的操作
    /// （重导 / 删除）与这一条真正该有的操作对不上。
    #[test]
    fn the_kind_is_read_verbatim_not_recomputed() {
        let resource = project_with(&[("kind", serde_json::json!("warning"))]);
        assert_eq!(resource.kind, "warning");
        assert_eq!(
            sm_db::transfers::downloads::failed_file_kind::classify(&resource.reason),
            sm_db::transfers::downloads::failed_file_kind::FILE,
            "分类表算出来是 file；读侧没重算，所以报了存储里的 warning"
        );
    }

    /// `state` 缺省是 `pending`（上游另外两处判据都是 `.get("state", "pending")`）。
    #[test]
    fn a_missing_state_defaults_to_pending() {
        let mut item = stored_item();
        item.as_object_mut().expect("对象").remove("state");
        let resource = failure_item_resource(1, item).expect("可投影");
        assert_eq!(resource.state, ImportFailedItemState::Pending);
        assert!(resource.can_manual_search, "缺省 pending 也要能搜");
    }

    /// 缺必填键 -> **500**，不是「填默认值」也不是「跳过这一条」。
    ///
    /// 静默丢掉一条读不出来的失败项 = 用户再也看不到、也重试不了它，
    /// 而「看得到失败项」正是这个端点存在的理由。
    #[test]
    fn a_missing_required_field_is_a_500() {
        let mut item = stored_item();
        item.as_object_mut().expect("对象").remove("size_bytes");
        let error = failure_item_resource(1, item).expect_err("必须报错");
        assert_eq!(error.status, 500);
        assert_eq!(error.code(), "internal_error");

        // 类型不对同样是 500（`size_bytes` 存成了字符串）。
        let error = failure_item_resource(
            1,
            serde_json::json!({
                "id": "x", "relative_path": "r", "size_bytes": "1024",
                "is_video": true, "reason": "r", "detail": "", "kind": "file",
                "media_kind": "jav",
            }),
        )
        .expect_err("必须报错");
        assert_eq!(error.status, 500);
    }

    /// `failed_files`：空 / `null` / 缺键都是「没有失败项」，**不是**错误。
    #[test]
    fn an_absent_summary_means_no_failures() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("null"),
            Some("{}"),
            Some(r#"{"failed_files": null}"#),
            Some(r#"{"imported_count": 0}"#),
        ] {
            assert!(
                failed_files(1, raw).expect("不算错误").is_empty(),
                "{raw:?} 应是空列表"
            );
        }
    }

    /// `failed_files`：读不出来就是 **500**（不是空列表）。
    #[test]
    fn an_unreadable_summary_is_a_500() {
        for raw in [
            Some("not json"),
            Some("[1,2]"),
            Some(r#"{"failed_files": {}}"#),
        ] {
            let error = failed_files(1, raw).expect_err("必须报错");
            assert_eq!(error.status, 500, "{raw:?}");
            assert_eq!(error.code(), "internal_error");
        }
    }

    /// 有失败项时原样取出（顺序也要保持 —— 那通常是失败发生的顺序）。
    #[test]
    fn the_stored_failed_files_come_out_in_order() {
        let raw = r#"{"failed_files":[{"id":"a"},{"id":"b"}]}"#;
        let items = failed_files(1, Some(raw)).expect("可读");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], serde_json::json!("a"));
        assert_eq!(items[1]["id"], serde_json::json!("b"));
    }
}
