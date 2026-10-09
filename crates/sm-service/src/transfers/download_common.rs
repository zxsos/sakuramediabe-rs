//! 下载器与媒体库的公共取数、校验与 provider 错误映射（上游 `downloads/common.py`，229 行）。
//!
//! # 这个文件是本域的**错误码翻译层**
//!
//! `ProviderOperationError` 带的是 provider 自己的错误码（`invalid_config`
//! / `authentication_failed` / `source_not_found` / `task_not_managed` …），
//! 而 HTTP 层要的是带状态码的 `ApiError`。映射表在
//! [`provider_error`]。**照抄那张表**，特别是：
//!
//! | provider 码 | HTTP |
//! |---|---|
//! | `authentication_failed` | 401 |
//! | `source_not_found` | **404** |
//! | `task_not_managed` | **409** |
//! | `source_blacklisted` / `unsupported` / `invalid_config` | 422 |
//! | `unavailable` | 503 |
//! | **其它** | **502** |
//!
//! 「其它 → 502」是关键：provider 报了一个我们不认识的码，说明**它那边出错了**
//! 而不是用户请求有问题，所以是 502 而不是 4xx。
//!
//! # `require_*` 系列返回 404 而不是 500
//!
//! `require_client` / `require_library` / `require_task` 查不到就是 404。
//! 别把它们写成「查不到就返回 `Option` 让调用方自己判」—— 那样每个调用点
//! 都要重复一遍同样的判空，而漏一个就变成 panic。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
    IndexerRepository, MediaLibraryRepository,
};
use sm_db::Db;
use sm_plugin_api::host::{HostProviderFactory, HostStorageProvider};

use crate::error::{details_of, ServiceError};

/// 下载任务**筛选**允许的状态集合（上游 `downloads/common.py:35` 的 `DOWNLOAD_STATES`）。
///
/// ⚠️ **只有四个。** 骨架期这里是六个（多了 `seeding` / `done`），于是
/// `?state=seeding` 被放行，而上游会给 422 `invalid_download_task_filter` ——
/// 筛选器比上游宽会让前端以为「存在一个实际不可能出现的状态」。
///
/// 取值集合由下载器 provider 决定，**不要建模成 enum** —— 新增一个下载器
/// 就可能带来新状态。
///
/// ⚠️ 与 [`super::transfer_shared::ACTIVE_DOWNLOAD_STATES`] 不是一回事：那个是
/// 「进行中」（`queued` / `downloading`），这里是**筛选白名单**（含终态）。
pub const DOWNLOAD_STATES: [&str; 4] = ["queued", "downloading", "completed", "failed"];

/// 视为「已下载完成」的状态白名单。上游 `downloads/common.py:36`。
///
/// 唯一实现住在 [`super::transfer_shared::DOWNLOAD_COMPLETE_STATES`]（与
/// [`is_download_complete`] 同处，避免两处判据漂移）；这里**再导出**，
/// 让上游同名路径可用。
pub use super::transfer_shared::DOWNLOAD_COMPLETE_STATES;

/// `GET /download-tasks` 的 `sort` 白名单 —— **六个 `field:dir` 字面量**
/// （上游 `downloads/common.py:37-44` 的 `TASK_SORT_FIELDS`）。
///
/// ⚠️ 骨架期这里是四个裸字段名（`created_at` / `updated_at` / `state` /
/// `movie_number`）加 `-field` 降序语法 —— 与上游三处不符：
///
/// | 骨架期 | 上游 | 后果 |
/// |---|---|---|
/// | 语法 `-created_at` | `created_at:desc` | 上游合法的 `sort=progress:desc` 被我们拒 422 |
/// | `state` 可排 | **不可排** | 上游没有这个键 |
/// | `movie_number` 可排 | **不可排** | 同上 |
///
/// 白名单而非自由字符串 —— 否则拼进 SQL 的排序字段名可被注入。
pub const TASK_SORT_FIELDS: [&str; 6] = [
    "created_at:desc",
    "created_at:asc",
    "updated_at:desc",
    "updated_at:asc",
    "progress:desc",
    "progress:asc",
];

/// 台账排序键（上游 `TASK_SORT_FIELDS` 的六个键之一）。
///
/// 类型本尊定义在 `sm-db`（[`sm_db::repo::DownloadTaskSort`]）—— 因为
/// `ORDER BY` 片段必须与仓储里的查询写死在同一处，分两层迟早漂移。这里
/// **再导出**，服务层照常用 `TaskSort` 这个名字。
pub use sm_db::repo::DownloadTaskSort as TaskSort;

/// provider 操作失败。**由插件 ABI 抛出**，本域只做翻译。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOperationError {
    /// provider 自带的错误码。
    pub code: String,
    pub message: String,
    /// 出错的操作名（`browse` / `scan` / `submit` …），用于日志。
    pub operation: Option<String>,
}

/// 查下载器客户端。查不到 → **404 `download_client_not_found`**。
///
/// 上游 `require_client`（`downloads/common.py:47-54`）走 `require_by_id`
/// （`common/service_helpers.py`）：**404**（不是 422）+ details `{client_id}`，
/// 文案 "Download client not found"。
///
/// # 为什么签名是 `async` + `&Db`（骨架期是同步的）
///
/// 骨架期这里是 `pub fn require_client(client_id: i32)` —— 一个**同步函数**却要查库，
/// 根本落不了地。这五个函数当时都长这样（另四个同因）。实测它们**零调用点**，
/// 所以一次性改成 `async` + `&Db` 是安全的。
pub async fn require_client(db: &Db, client_id: i32) -> Result<DownloadClientRow, ServiceError> {
    DownloadClientRepository::new(db.clone())
        .find_by_id(client_id)
        .await?
        .map(|row| DownloadClientRow::from_entity(&row))
        .ok_or_else(|| {
            ServiceError::not_found_with(
                "download_client_not_found",
                "Download client not found",
                details_of("client_id", serde_json::Value::from(client_id)),
            )
        })
}

/// 查媒体库。查不到 → **404 `media_library_not_found`**（details `{library_id}`）。
///
/// 上游 `require_library`（`:57-64`），文案 "Media library not found"。
pub async fn require_library(db: &Db, library_id: i32) -> Result<MediaLibraryRow, ServiceError> {
    MediaLibraryRepository::new(db.clone())
        .find_by_id(library_id)
        .await?
        .map(|row| MediaLibraryRow::from_entity(&row))
        .ok_or_else(|| {
            ServiceError::not_found_with(
                "media_library_not_found",
                "Media library not found",
                details_of("library_id", serde_json::Value::from(library_id)),
            )
        })
}

/// 查下载任务。查不到 → **404 `download_task_not_found`**（details `{task_id}`）。
///
/// 上游 `require_task`（`:67-74`），文案 "Download task not found"。
///
/// # 返回**实体**而不是投影
///
/// 上游返回的是整个 `DownloadTask` 模型，调用方各取所需（`trigger_import` 要
/// `state` / `completed_source_ref` / `import_status` / `movie` / `client`，
/// `delete_task` 还要 `remote_id`）。骨架期这里返回一个只有四个字段的
/// `DownloadTaskRow`，于是 `trigger_import` 判两道门时**拿不到需要的列** ——
/// 那个投影后来谁也不用了。现在直接给实体，投影类型一并删掉。
pub async fn require_task(db: &Db, task_id: i32) -> Result<sm_db::DownloadTask, ServiceError> {
    DownloadTaskRepository::new(db.clone())
        .find_by_id(task_id)
        .await?
        .ok_or_else(|| {
            ServiceError::not_found_with(
                "download_task_not_found",
                "Download task not found",
                details_of("task_id", serde_json::Value::from(task_id)),
            )
        })
}

/// 按名字查索引器。查不到 → **422 `download_request_indexer_not_found`**。
///
/// 上游 `require_indexer`（`:171-188`）有三点容易写错：
///
/// 1. 码是 **422 而不是 404** —— 这是「提交下载」入口的前置校验，不是资源查询；
/// 2. **空/纯空白名字先报同一个 422**，`details` 里放**原始**入参（用户填了什么
///    就回什么，前端据此高亮那个输入框）；
/// 3. 查库用**归一化后**（`strip()`）的名字，`details` 也放归一化后的值。
pub async fn require_indexer(db: &Db, indexer_name: &str) -> Result<IndexerRow, ServiceError> {
    let normalized = indexer_name.trim();
    if normalized.is_empty() {
        return Err(indexer_not_found(indexer_name));
    }
    IndexerRepository::new(db.clone())
        .find_by_name(normalized)
        .await?
        .map(|row| IndexerRow::from_entity(&row))
        .ok_or_else(|| indexer_not_found(normalized))
}

/// 422 `download_request_indexer_not_found` 的唯一出口。
fn indexer_not_found(shown: &str) -> ServiceError {
    ServiceError::validation_with(
        "download_request_indexer_not_found",
        "Indexer not found",
        details_of("indexer_name", serde_json::Value::from(shown)),
    )
}

/// 取某个下载器客户端的 provider 能力。
///
/// # 为什么按 `provider_key` 而不是「下载器种类」
///
/// ⚠️ 骨架期这个函数收的是 **`&DownloadClientRow` 而直接 `todo!()`** —— 但真正
/// 的决定因素是客户端**所属媒体库的 `provider_key`**（上游
/// `_bundle(library)`，`client_config_service.py:52-70`）。把 row 传进来就
/// 还要再查一次库，而这个帮助函数放在这里本来就是为了让**调用方不必关心查库**。
///
/// 两处 `None` / `Err` 的区别照上游：
///
/// | 返回 | 含义 | HTTP |
/// |---|---|---|
/// | `Err` | provider **没安装** | 503 `provider_not_installed` |
/// | `Ok(None)` | 装了但该库**没有下载能力** | 422 `provider_download_unsupported` |
pub fn download_provider(
    registry: &dyn super::download_client::DownloadCapabilityRegistry,
    provider_key: &str,
) -> Result<Option<Box<dyn super::download_client::DownloadClientCapability>>, ServiceError> {
    match registry.download_client_for(provider_key) {
        Ok(capability) => Ok(capability),
        Err(failure) => Err(ServiceError::unavailable(
            format!("provider_{}", failure.code),
            failure.message,
        )),
    }
}

/// 取媒体库的存储 provider 句柄。
///
/// 经 [`HostProviderFactory`] 按 `library.provider_key` 取 ——
//  与 `ProviderBrowseService::browse` 同一条链路（查注册表 → 建 gRPC client）。
///
/// | 情况 | 结果 |
/// |---|---|
/// | `factory` 为 `None`（组合根没注入） | 503 `provider_not_installed` |
/// | 注册表里没有这个 provider / 连不上 | 503 `provider_not_installed` |
/// | provider 操作失败 | `provider_{code}`（按码分状态，见 [`provider_error`]） |
pub async fn library_provider(
    library: &MediaLibraryRow,
    factory: Option<&dyn HostProviderFactory>,
) -> Result<Arc<dyn HostStorageProvider>, ServiceError> {
    let factory = factory
        .ok_or_else(|| ServiceError::unavailable("provider_not_installed", "媒体提供方未安装"))?;
    factory
        .for_provider_key(&library.provider_key)
        .await
        .map_err(|err| {
            if err.is_not_installed() {
                ServiceError::unavailable("provider_not_installed", "媒体提供方未安装")
            } else {
                // `HostProviderError` → 本域的 `ProviderOperationError` 镜像，
                // 复用 `provider_error` 的码表（`provider_{code}`）。
                provider_error(&ProviderOperationError {
                    code: err.code,
                    message: err.safe_message,
                    operation: Some(err.operation),
                })
            }
        })
}

/// **provider 错误码 → HTTP 状态码**。见模块文档的映射表。
///
/// 「未识别的码 → 502」是刻意的：那是 provider 侧的意外，不是用户请求的问题。
pub fn provider_error(error: &ProviderOperationError) -> ServiceError {
    let status = match error.code.as_str() {
        "invalid_config" | "source_blacklisted" | "unsupported" => 422,
        "authentication_failed" => 401,
        "source_not_found" => 404,
        "task_not_managed" => 409,
        "unavailable" => 503,
        // 未知码 = provider 出了问题 = 上游依赖故障。
        _ => 502,
    };
    ServiceError::from_status(
        status,
        format!("provider_{}", error.code),
        error.message.clone(),
    )
}

/// 非空字符串校验。**空串与纯空白都算空**（上游 `value.strip()`）。
///
/// 错误码由调用方给 —— 同一个校验在不同端点用不同码，客户端要靠码区分。
pub fn validate_non_empty(value: &str, code: &str, message: &str) -> Result<String, ServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::validation(code, message));
    }
    Ok(trimmed.to_owned())
}

/// 归一 `state` 过滤条件（上游 `normalize_state_filters`，`downloads/common.py:191-212`）。
///
/// # ⚠️ `None` 与空列表**都表示「不过滤」**
///
/// 上游是 `if not values: return None` —— 空列表是**假值**，与 `None` 走同一
/// 分支；末尾又是 `return normalized or None`，所以「全部是空白项」也归 `None`。
///
/// 骨架期这里把空列表当成 `Some(∅)`（「筛出空集」），并在注释里称这是上游
/// 语义 —— **那是不对的**。`?state=`（空值）与显式空列表在上游都不含筛选条件。
/// Flutter 客户端也从不发空列表（`states.isNotEmpty` 才拼该参数），所以这个
/// 偏差在真机上不可见，只是与上游的筛选口径不一致。
///
/// 每一项先 `strip().lower()` 再查白名单，所以 `?state=COMPLETED` 合法；
/// 不在白名单 → **422 `invalid_download_task_filter`**，文案 "Invalid state"，
/// details 放**原始**入参（不是归一小写后的），前端据此高亮。
pub fn normalize_state_filters(
    values: Option<&[String]>,
) -> Result<Option<Vec<String>>, ServiceError> {
    let Some(values) = values.filter(|values| !values.is_empty()) else {
        return Ok(None);
    };
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let item = value.trim().to_lowercase();
        if item.is_empty() {
            continue;
        }
        if !DOWNLOAD_STATES.contains(&item.as_str()) {
            return Err(ServiceError::validation_with(
                "invalid_download_task_filter",
                "Invalid state",
                details_of("state", value.as_str()),
            ));
        }
        if !out.iter().any(|existing| existing == &item) {
            out.push(item);
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// 解析 `sort`（上游 `resolve_sort`，`service_helpers.py:179-194`）。
///
/// 上游语义有三点容易写错：
///
/// 1. `None` / 空白 → **回落默认键** `created_at:desc`（不是「不排序」——
///    台账必须有稳定顺序，否则翻页会重复或漏行）；
/// 2. 先 `strip().lower()` 再查表，所以 `ASC` / `Progress:Desc` 都合法；
/// 3. 不在白名单 → **422 `invalid_download_task_filter`**，文案
///    "Invalid sort expression"，details 放**原始**入参 `{sort: value}`。
///
/// ⚠️ 骨架期这里 `None`/空白返回 `None`（「不排序」）且只认 `field` / `-field`，
/// 与上游两条都不符。
pub fn resolve_task_sort(value: Option<&str>) -> Result<TaskSort, ServiceError> {
    let Some(raw) = value else {
        return Ok(TaskSort::default());
    };
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return Ok(TaskSort::default());
    }
    TaskSort::from_key(&normalized).ok_or_else(|| {
        ServiceError::validation_with(
            "invalid_download_task_filter",
            "Invalid sort expression",
            details_of("sort", raw),
        )
    })
}

/// 任务是否已完成。见 [`super::transfer_shared::is_download_complete`]。
pub fn is_download_complete(state: &str) -> bool {
    super::transfer_shared::is_download_complete(state)
}

/// 校验 provider 返回的远端任务对象。
///
/// 远端返回的字段类型不可信（JSON 解出来可能是字符串或 null），
/// 逐个字段校验后才落库 —— 宁可这一条导入失败，也不要写进半截数据。
pub fn validate_remote_download_task(
    raw: &serde_json::Value,
) -> Result<RemoteDownloadTask, ServiceError> {
    // 上游这一层只做两件事（`:138-144`）：类型对不上、或「completed 却没有
    // completed_source_ref」→ 502 `provider_invalid_response`。其余字段级规则在
    // `RemoteDownloadTask.__post_init__`（`plugins/provider_protocol.py`）里，
    // 构造时就炸 —— 我们这边没有那个构造时机，所以一起在这儿判。
    let invalid =
        || ServiceError::from_status(502, "provider_invalid_response", "下载提供方返回了无效任务");
    let object = raw.as_object().ok_or_else(invalid)?;

    // `remote_id`：非空字符串（`strip()` 之后判）。
    let remote_id = object
        .get("remote_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(invalid)?;
    // `name`：字符串（上游不判空）。
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    // `state`：**provider 侧**的四种取值之一（不是宿主台账那六个）。
    let state = object
        .get("state")
        .and_then(serde_json::Value::as_str)
        .filter(|value| REMOTE_TASK_STATES.contains(value))
        .ok_or_else(invalid)?;
    // `progress`：必填，且必须在 `0..=1`。上游是 `float`，没有默认值。
    let progress = object
        .get("progress")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| (0.0..=1.0).contains(value))
        .ok_or_else(invalid)?;
    // `completed_source_ref`：缺省/`null` 都可以；给了就必须是**非空对象**。
    let completed_source_ref = match object.get("completed_source_ref") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            if !value.as_object().is_some_and(|map| !map.is_empty()) {
                return Err(invalid());
            }
            Some(value.clone())
        }
    };
    // 两个方向都要判：完成必须有来源引用；**没完成也不许带**（否则下游会拿它
    // 当成「已经下好了」，而任务其实还在跑）。
    if state == "completed" {
        if completed_source_ref.is_none() {
            return Err(invalid());
        }
    } else if completed_source_ref.is_some() {
        return Err(invalid());
    }

    Ok(RemoteDownloadTask {
        remote_id: remote_id.to_owned(),
        name: name.to_owned(),
        state: state.to_owned(),
        progress,
        completed_source_ref,
    })
}

/// 列出与某索引器绑定的下载器客户端。**空列表**是合法结果（调用方 → 422
/// `download_request_client_resolution_failed`，见 [`resolve_preferred_client`]）。
///
/// 上游 `list_indexer_clients`（`:147-156`）：JOIN 取**完整客户端行**，按关联行
/// id 升序（顺序就是「提交时选哪个下载器」的行为，见仓储侧文档）。
pub async fn list_indexer_clients(
    db: &Db,
    indexer: &IndexerRow,
) -> Result<Vec<DownloadClientRow>, ServiceError> {
    Ok(IndexerDownloadClientRepository::new(db.clone())
        .list_clients_by_indexer(indexer.id)
        .await?
        .iter()
        .map(DownloadClientRow::from_entity)
        .collect())
}

/// 选一个客户端。**唯一的那个**；多于一个 → 422。
///
/// 上游 `resolve_preferred_client`：多于一个时**不猜**，报错让用户去配置里
/// 指定。猜错的后果是把种子提交到错误的下载器上，而那不会有任何报错。
pub fn resolve_preferred_client(
    clients: &[DownloadClientRow],
) -> Result<DownloadClientRow, ServiceError> {
    match clients {
        [] => Err(ServiceError::validation(
            "download_request_client_resolution_failed",
            "该索引器没有绑定下载器客户端",
        )),
        [only] => Ok(only.clone()),
        many => Err(ServiceError::validation(
            "download_request_client_resolution_failed",
            format!(
                "该索引器绑定了 {} 个下载器客户端，请先在配置中指定",
                many.len()
            ),
        )),
    }
}

/// provider 侧远端任务的合法状态。
///
/// 上游 `provider_protocol.py` 的 `RemoteDownloadTask.state`：
/// `Literal["queued", "downloading", "completed", "failed"]`。
///
/// ⚠️ 与 [`DOWNLOAD_STATES`]（**宿主台账**的状态白名单）**不是同一个集合** ——
/// 台账里还有 `seeding` / `done` 这类由宿主记的状态。合并两者会让 provider 报
/// `seeding` 时被判成「无效响应」。
pub const REMOTE_TASK_STATES: [&str; 4] = ["queued", "downloading", "completed", "failed"];

/// 下载器客户端的最小投影。
///
/// 字段与上游响应资源同源（`schema/transfers/downloads.py:14-33` 的
/// `DownloadClientResource`：`{id, name, library_id, provider_config,
/// created_at, updated_at}`）。
///
/// ⚠️ 骨架期这里多两个字段 —— **都没有数据来源**：
///
/// | 字段 | 为什么删 |
/// |---|---|
/// | `kind`（下载器种类） | `download_client` 表与上游模型
///   （`model/transfers/downloads.py:9-13`）都**没有这列**。「哪种下载器」是
///   **provider 自己解释 `provider_config`** 的事，宿主不建模 |
/// | `enabled` | 同样没有这列。上游的启停语义在索引器绑定上，不在客户端 |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientRow {
    pub id: i32,
    pub name: String,
    /// 归属库。构造插件句柄要用（`:89-94` 的 `library_handle_for(client.library)`）。
    pub library_id: i32,
    /// 插件配置。**原样透传** —— 各插件字段不同，宿主不解释。
    pub provider_config: serde_json::Value,
}

impl DownloadClientRow {
    /// 从库里的行投影。
    fn from_entity(row: &sm_db::DownloadClient) -> Self {
        Self {
            id: row.id,
            name: row.name.clone(),
            library_id: row.library_id,
            provider_config: provider_config_object(row.provider_config.as_deref()),
        }
    }
}

/// 媒体库的最小投影。
///
/// ⚠️ `id` 骨架期写成 `i64`，而 `media_library.id` 是 `integer`（i32）——
/// 类型对不上会在用到的地方逼出一堆 `as` 转换，而那正是漏判溢出的地方。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaLibraryRow {
    pub id: i32,
    pub name: String,
    /// provider 键（`local` / `115` / …）。决定谁来解释 `provider_config`。
    pub provider_key: String,
    /// 插件配置。**原样透传**。
    pub provider_config: serde_json::Value,
}

impl MediaLibraryRow {
    /// 从库里的行投影。
    ///
    /// `pub(crate)`：`media_transfer_task` 的候选列表也要把 `MediaLibrary`
    /// 转成行投影（`provider_config` 的归一化逻辑只应有一处）。
    pub(crate) fn from_entity(row: &sm_db::MediaLibrary) -> Self {
        Self {
            id: row.id,
            name: row.name.clone(),
            provider_key: row.provider_key.clone(),
            provider_config: provider_config_object(row.provider_config.as_deref()),
        }
    }
}

/// 索引器的最小投影。
///
/// ⚠️ 骨架期多一个 `enabled` —— `indexer` 表与实体都没有这一列
/// （实体字段：`id` / `name` / `url` / `kind` / `api_key` / 时间戳）。
/// 反过来 `kind`（`pt` / `bt`）是**真列**，下游要拿它填候选资源的
/// `indexer_kind`，所以留在这儿。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerRow {
    pub id: i32,
    pub name: String,
    /// `pt` / `bt`。数据库无 CHECK 约束，取值由上游约定。
    pub kind: String,
}

impl IndexerRow {
    /// 从库里的行投影。
    fn from_entity(row: &sm_db::Indexer) -> Self {
        Self {
            id: row.id,
            name: row.name.clone(),
            kind: row.kind.clone(),
        }
    }
}

/// provider 侧的远端任务（**已校验**，见 [`validate_remote_download_task`]）。
///
/// 字段名与上游 `plugins/provider_protocol.py` 的 dataclass **逐字对齐**。
/// 骨架期这里是 `remote_task_id` / `download_path` 且 `progress` 可空 —— 三处都不对：
///
/// | 骨架期 | 上游 | 后果 |
/// |---|---|---|
/// | `remote_task_id` | `remote_id` | 与库里的 `download_task.remote_id` 不同名，
///   同步时得记两条名字的对应关系 |
/// | `progress: Option<f64>` | `progress: float`（必填，`0..=1`）| 把「provider 没给」
///   与「0%」混成一个 |
/// | `download_path` | **不存在**；完成时给的是 `completed_source_ref` | 路径是
///   provider 命名空间里的东西，宿主拿到它也不知道怎么用 |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDownloadTask {
    /// provider 侧的任务标识，删任务时要用。
    pub remote_id: String,
    /// 显示名。
    pub name: String,
    /// 取值见 [`REMOTE_TASK_STATES`]。
    pub state: String,
    /// `0.0..=1.0`。
    pub progress: f64,
    /// **只有** `completed` 才允许有值，且必须是非空对象（`:142-143`）。
    /// 里面放什么由 provider 决定，宿主只原样存下来给导入流程用。
    pub completed_source_ref: Option<serde_json::Value>,
}

/// 库里的 `provider_config`（不透明 JSON **文本**）→ 交给插件的对象。
///
/// 上游是 `client.provider_config or {}`（`downloads/common.py:93`）：**NULL 当空
/// 对象**，不是 `null` —— 插件侧统一按「拿到一个对象」处理。
///
/// 文本不是合法 JSON 时按空对象处理并记 warning：那是脏数据，但它不该让
/// 「列出下载器」这种读操作整个 500。
pub(crate) fn provider_config_object(raw: Option<&str>) -> serde_json::Value {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return serde_json::json!({});
    };
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            tracing::warn!("provider_config 不是 JSON 对象，按空对象处理");
            serde_json::json!({})
        }
        Err(error) => {
            tracing::warn!(%error, "provider_config 不是合法 JSON，按空对象处理");
            serde_json::json!({})
        }
    }
}

/// ⚠️ 这里**曾经**有一个占位的 `PluginDownloadProvider`（unit struct）。它已删除：
/// 下载能力的真实形状是 [`super::download_client::DownloadClientCapability`]
/// 那个 trait（有 `config_fields` / `prepare_client` / `test_client` 三个动作），
/// 空壳留着只会让人以为「句柄已经存在」。
///
/// 同理，占位的 `PluginStorageProvider`（unit struct）也已删除：
/// 存储 provider 的真实形状是 `sm_plugin_api::host::HostStorageProvider`
/// 那个 trait（宿主侧调用抽象），取句柄走 [`library_provider`]。
#[cfg(test)]
mod tests {
    use super::*;

    /// 未知状态 → 422，且错误码是**专用**的那个（不是 `validation_error`）。
    ///
    /// 客户端要靠 `invalid_download_task_filter` 区分「筛选写错了」与「任务不存在」。
    #[test]
    fn an_unknown_state_filter_is_rejected_with_the_dedicated_code() {
        let error =
            normalize_state_filters(Some(&["nope".to_owned()])).expect_err("未知状态应被拒");
        assert_eq!(error.status, 422);
        assert_eq!(error.code(), "invalid_download_task_filter");
        assert_eq!(error.api.message, "Invalid state");
        // details 放**原始**入参（未归一小写）。
        assert_eq!(
            error.details().and_then(|details| details.get("state")),
            Some(&serde_json::json!("nope"))
        );
    }

    /// ★ `seeding` / `done` **不在**筛选白名单里（上游只有四个状态）。
    ///
    /// 骨架期放行它们，而上游给 422 —— 筛选器比上游宽是契约偏差。
    #[test]
    fn only_the_four_upstream_states_are_filterable() {
        assert_eq!(DOWNLOAD_STATES.len(), 4);
        for valid in ["queued", "downloading", "completed", "failed"] {
            assert!(normalize_state_filters(Some(&[valid.to_owned()])).is_ok());
        }
        for bogus in ["seeding", "done", "submitted"] {
            let error = normalize_state_filters(Some(&[bogus.to_owned()]))
                .expect_err("这些状态不在上游白名单里");
            assert_eq!(error.code(), "invalid_download_task_filter");
        }
    }

    /// 状态先 `strip().lower()` 再查表 —— `COMPLETED` 与 ` completed ` 都合法。
    #[test]
    fn state_filters_are_lowercased_and_trimmed() {
        assert_eq!(
            normalize_state_filters(Some(&["  COMPLETED ".to_owned()])).expect("合法"),
            Some(vec!["completed".to_owned()])
        );
    }

    /// ★ `None` 与空列表、以及「全部空白项」**都归 `None`**（上游 `not values`）。
    #[test]
    fn absent_blank_and_empty_filters_all_mean_no_filter() {
        assert_eq!(normalize_state_filters(None).expect("不过滤"), None);
        assert_eq!(
            normalize_state_filters(Some(&[])).expect("空列表"),
            None,
            "空列表上游走 not values 分支，等于不过滤"
        );
        assert_eq!(
            normalize_state_filters(Some(&["".to_owned(), "  ".to_owned()])).expect("全空白"),
            None,
            "全空白项归 None（上游 `normalized or None`）"
        );
    }

    /// 重复的状态要去重 —— `?state=queued&state=queued` 不该让 SQL 里出现两遍。
    #[test]
    fn duplicate_states_are_collapsed() {
        let filters = normalize_state_filters(Some(&[
            "queued".to_owned(),
            "queued".to_owned(),
            "downloading".to_owned(),
        ]))
        .expect("合法");
        assert_eq!(
            filters,
            Some(vec!["queued".to_owned(), "downloading".to_owned()])
        );
    }

    /// ★ `sort` 是**六个 `field:dir`** 的白名单（上游 `TASK_SORT_FIELDS`）。
    ///
    /// - `None` / 空白 → 回落默认键 `created_at:desc`（**不是「不排序」**）；
    /// - `field:dir` 大小写不敏感；
    /// - 骨架期的 `-created_at` 语法**不再合法**（上游没有这个写法）；
    /// - `state` / `movie_number` **不可排序**（上游白名单里没有）。
    #[test]
    fn sort_must_come_from_the_six_field_dir_keys() {
        assert_eq!(
            resolve_task_sort(None).expect("缺省"),
            TaskSort::CreatedAtDesc
        );
        assert_eq!(
            resolve_task_sort(Some("   ")).expect("空白"),
            TaskSort::CreatedAtDesc
        );
        assert_eq!(
            resolve_task_sort(Some("progress:desc")).expect("合法"),
            TaskSort::ProgressDesc
        );
        // 大小写不敏感（上游 strip().lower()）。
        assert_eq!(
            resolve_task_sort(Some("  UPDATED_AT:ASC ")).expect("合法"),
            TaskSort::UpdatedAtAsc
        );
        // 骨架期的 `-field` 语法上游并不认。
        let error = resolve_task_sort(Some("-created_at")).expect_err("-field 不是上游语法");
        assert_eq!(error.code(), "invalid_download_task_filter");
        assert_eq!(error.api.message, "Invalid sort expression");
        assert_eq!(
            error.details().and_then(|details| details.get("sort")),
            Some(&serde_json::json!("-created_at"))
        );
        for bogus in ["state", "movie_number", "state:desc", "id; DROP TABLE"] {
            let error = resolve_task_sort(Some(bogus)).expect_err("这些排序键不在上游白名单里");
            assert_eq!(error.code(), "invalid_download_task_filter");
        }
    }

    /// 白名单常量与 enum 集合必须一致（`from_key` 是唯一解析入口）。
    #[test]
    fn sort_allow_list_and_enum_stay_in_sync() {
        for sort in TaskSort::ALL {
            assert!(
                TASK_SORT_FIELDS.contains(&sort.key()),
                "{} 不在白名单常量里",
                sort.key()
            );
        }
        assert_eq!(TASK_SORT_FIELDS.len(), TaskSort::ALL.len());
    }

    /// `ORDER BY` 的次级排序恒为 `id` 同向 —— 否则翻页顺序不稳定。
    #[test]
    fn order_by_has_a_stable_id_tiebreaker_in_the_same_direction() {
        for sort in TaskSort::ALL {
            let sql = sort.order_by();
            assert!(sql.ends_with("DESC") || sql.ends_with("ASC"), "{sql}");
            let direction = if sql.ends_with("DESC") { "DESC" } else { "ASC" };
            assert!(
                sql.contains(&format!("id {direction}")),
                "{sql} 的次级排序应与主序同向"
            );
        }
    }

    /// 绑定多个客户端时**不猜**：报 422 让用户去配置。
    ///
    /// 猜错的后果是种子被提交到错误的下载器，而全程无报错。
    #[test]
    fn multiple_bound_clients_are_refused_rather_than_guessed() {
        let client = DownloadClientRow {
            id: 1,
            name: "qb".to_owned(),
            library_id: 1,
            provider_config: serde_json::json!({}),
        };
        let error = resolve_preferred_client(&[client.clone(), client.clone()])
            .expect_err("多于一个应报错");
        assert_eq!(error.code(), "download_request_client_resolution_failed");
        assert_eq!(
            resolve_preferred_client(&[client])
                .expect("唯一绑定可用")
                .id,
            1
        );
        assert!(resolve_preferred_client(&[]).is_err(), "零绑定也要报 422");
    }

    /// provider 错误码 → 状态码的映射逐条断言，尤其「未知码 → 502」。
    #[test]
    fn provider_codes_map_to_the_documented_status_codes() {
        let cases = [
            ("invalid_config", 422),
            ("source_blacklisted", 422),
            ("unsupported", 422),
            ("authentication_failed", 401),
            ("source_not_found", 404),
            ("task_not_managed", 409),
            ("unavailable", 503),
            // 未知码：provider 侧的意外 -> 502 而不是 4xx。
            ("something_new", 502),
        ];
        for (code, expected) in cases {
            let error = provider_error(&ProviderOperationError {
                code: code.to_owned(),
                message: "x".to_owned(),
                operation: Some("browse".to_owned()),
            });
            assert_eq!(
                error.status, expected,
                "provider 码 {code} 应映射到 {expected}"
            );
        }
    }

    /// 错误码前缀固定为 `provider_`。
    #[test]
    fn the_error_code_is_prefixed_with_provider() {
        let error = provider_error(&ProviderOperationError {
            code: "source_not_found".to_owned(),
            message: "x".to_owned(),
            operation: None,
        });
        assert_eq!(error.code(), "provider_source_not_found");
    }

    /// 纯空白也要被拒 —— `" "` 拼进路径或查库都会出问题。
    #[test]
    fn blank_values_are_rejected() {
        assert!(validate_non_empty("x", "c", "m").is_ok());
        let error = validate_non_empty("  \t ", "c", "m").expect_err("空白应被拒");
        assert_eq!(error.code(), "c");
    }

    /// ★ 远端任务的**五项**规则逐条钉住（上游 `provider_protocol.py` 的
    /// `RemoteDownloadTask.__post_init__`）。违反任一条都是同一个 **502**。
    ///
    /// 为什么值得逐条写：这些字段来自**插件**，类型完全不可信。放到库里以后
    /// 才发现的代价是「同步流程写进半截数据」，而那时已经分不清是谁写坏的。
    #[test]
    fn remote_task_validation_matches_upstream() {
        let good = serde_json::json!({
            "remote_id": "abc",
            "name": "some release",
            "state": "downloading",
            "progress": 0.5,
        });
        let parsed = validate_remote_download_task(&good).expect("合法");
        assert_eq!(parsed.remote_id, "abc");
        assert_eq!(parsed.name, "some release");
        assert_eq!(parsed.state, "downloading");
        assert_eq!(parsed.progress, 0.5);
        assert!(parsed.completed_source_ref.is_none());

        for bad in [
            // 1. `remote_id` 必须是**非空**字符串（strip 之后判）。
            serde_json::json!({"remote_id": "   ", "name": "n", "state": "queued", "progress": 0.0}),
            serde_json::json!({"name": "n", "state": "queued", "progress": 0.0}),
            serde_json::json!({"remote_id": 7, "name": "n", "state": "queued", "progress": 0.0}),
            // 2. `state` 取 provider 那四种（`seeding` 是**台账**状态，不算）。
            serde_json::json!({"remote_id": "a", "name": "n", "state": "seeding", "progress": 0.0}),
            // 3. `progress` 必填且在 0..=1。
            serde_json::json!({"remote_id": "a", "name": "n", "state": "queued"}),
            serde_json::json!({"remote_id": "a", "name": "n", "state": "queued", "progress": 1.5}),
            serde_json::json!({"remote_id": "a", "name": "n", "state": "queued", "progress": -0.1}),
            // 4. `completed` 必须带**非空对象**的来源引用。
            serde_json::json!({"remote_id": "a", "name": "n", "state": "completed", "progress": 1.0}),
            serde_json::json!({"remote_id": "a", "name": "n", "state": "completed", "progress": 1.0, "completed_source_ref": {}}),
            serde_json::json!({"remote_id": "a", "name": "n", "state": "completed", "progress": 1.0, "completed_source_ref": "x"}),
            // 5. 没完成**不许**带来源引用（带了说明 provider 自相矛盾）。
            serde_json::json!({"remote_id": "a", "name": "n", "state": "downloading", "progress": 0.1, "completed_source_ref": {"x": 1}}),
            // 6. 根本不是对象。
            serde_json::json!(["not", "an", "object"]),
        ] {
            let error = validate_remote_download_task(&bad).expect_err("应被判无效");
            assert_eq!(error.status, 502, "{bad}");
            assert_eq!(error.code(), "provider_invalid_response", "{bad}");
        }
    }

    /// 完成的任务：来源引用**原样保留**（宿主不解释它的内容）。
    #[test]
    fn a_completed_remote_task_keeps_its_source_ref() {
        let raw = serde_json::json!({
            "remote_id": "abc",
            "name": "n",
            "state": "completed",
            "progress": 1.0,
            "completed_source_ref": {"path": "/downloads/x.mkv", "size": 42},
        });
        let parsed = validate_remote_download_task(&raw).expect("合法");
        assert_eq!(
            parsed.completed_source_ref,
            Some(serde_json::json!({"path": "/downloads/x.mkv", "size": 42}))
        );
    }

    /// ★ `provider_config` 文本 → **对象**：NULL / 空串 / 非法 JSON 都当空对象。
    ///
    /// 上游是 `client.provider_config or {}`（`common.py:93`）—— 插件侧统一按
    /// 「拿到一个对象」处理；给它 `null` 会让插件里那句 `config["x"]` 炸掉。
    #[test]
    fn provider_config_text_becomes_an_object() {
        assert_eq!(provider_config_object(None), serde_json::json!({}));
        assert_eq!(provider_config_object(Some("  ")), serde_json::json!({}));
        assert_eq!(provider_config_object(Some("{oops")), serde_json::json!({}));
        // 合法对象原样透传（字段由插件解释，宿主不改写）。
        assert_eq!(
            provider_config_object(Some(r#"{"host":"127.0.0.1","port":8080}"#)),
            serde_json::json!({"host": "127.0.0.1", "port": 8080})
        );
        // 非对象（数组/标量）也退回空对象 —— 句柄那边只接受对象。
        assert_eq!(provider_config_object(Some("[1,2]")), serde_json::json!({}));
    }

    /// ★ 索引器那两条 422 的 details **用哪个值**（原始 / 归一化）要分清。
    #[test]
    fn the_indexer_not_found_error_carries_the_right_details() {
        let blank = indexer_not_found("   ");
        assert_eq!(blank.status, 422);
        assert_eq!(blank.code(), "download_request_indexer_not_found");
        assert_eq!(
            blank
                .details()
                .and_then(|details| details.get("indexer_name")),
            Some(&serde_json::json!("   ")),
            "空名字那次要把**原始**入参回给前端去高亮输入框"
        );

        let missing = indexer_not_found(" abc ");
        assert_eq!(
            missing
                .details()
                .and_then(|details| details.get("indexer_name")),
            Some(&serde_json::json!(" abc ")),
            "查不到时上游放的是**归一化后**的名字（这里入参已由调用方 trim 过）"
        );
    }
}
