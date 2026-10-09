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

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 下载任务的合法状态集合。
///
/// ⚠️ **由 provider 决定，不要建模成 enum。** 新增一个下载器就可能带来
/// 新状态 —— 写死 enum 会让它在反序列化时炸掉。
pub const DOWNLOAD_STATES: [&str; 6] = [
    "queued",
    "downloading",
    "completed",
    "failed",
    "seeding",
    "done",
];

/// 视为「已下载完成」的状态（白名单，见 [`super::transfer_shared::is_download_complete`]）。
pub const DOWNLOAD_COMPLETE_STATES: [&str; 4] = ["completed", "done", "seeding", "finished"];

/// `GET /download-tasks` 的 `sort` 可选值。
///
/// 白名单而非自由字符串 —— 否则拼进 SQL 的排序字段名可被注入。
pub const TASK_SORT_FIELDS: [&str; 4] = ["created_at", "updated_at", "state", "movie_number"];

/// provider 操作失败。**由插件 ABI 抛出**，本域只做翻译。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOperationError {
    /// provider 自带的错误码。
    pub code: String,
    pub message: String,
    /// 出错的操作名（`browse` / `scan` / `submit` …），用于日志。
    pub operation: Option<String>,
}

/// 查不到下载器客户端。
pub fn require_client(client_id: i32) -> Result<DownloadClientRow, ServiceError> {
    let _ = client_id;
    todo!("骨架：查 download_client 表；查不到 -> 404 download_client_not_found")
}

/// 查不到媒体库。
pub fn require_library(library_id: i64) -> Result<MediaLibraryRow, ServiceError> {
    let _ = library_id;
    todo!("骨架：查 media_library 表；查不到 -> 404 media_library_not_found")
}

/// 查不到下载任务。
pub fn require_task(task_id: i64) -> Result<DownloadTaskRow, ServiceError> {
    let _ = task_id;
    todo!("骨架：查 download_task 表；查不到 -> 404 download_task_not_found")
}

/// 查不到索引器（按名字）。
pub fn require_indexer(indexer_name: &str) -> Result<IndexerRow, ServiceError> {
    let _ = indexer_name;
    todo!("骨架：按 name 查 indexer；查不到 -> 422 download_request_indexer_not_found")
}

/// 取某个下载器客户端的 provider 句柄。
pub fn download_provider(
    client: &DownloadClientRow,
) -> Result<PluginDownloadProvider, ServiceError> {
    let _ = client;
    todo!("骨架：经 sm-plugins 的 download_client 能力取句柄；未装 -> 503 provider_not_installed")
}

/// 取媒体库的存储 provider 句柄。
pub fn library_provider(library: &MediaLibraryRow) -> Result<PluginStorageProvider, ServiceError> {
    let _ = library;
    todo!("骨架：经 sm-plugins 的 media.provider 能力取句柄；未装 -> 503 provider_not_installed")
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

/// 归一 `state` 过滤条件。`None` → `None`（不过滤）；空列表 → `Some(∅)`（无结果）。
///
/// # `None` 与空列表**语义不同**，别归一
///
/// `?state=`（空值）上游给的是 `None`，也就是**不过滤**。而显式传空列表在
/// 其它端点（如 `movie_ids`）里表示「什么都不选」。混淆这两者会让「清空筛选」
/// 变成「看全部」。
pub fn normalize_state_filters(
    values: Option<&[String]>,
) -> Result<Option<Vec<String>>, ServiceError> {
    let Some(values) = values else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !DOWNLOAD_STATES.contains(&trimmed) {
            return Err(ServiceError::validation(
                "invalid_download_task_filter",
                format!("未知的任务状态：{trimmed}"),
            ));
        }
        if !out.iter().any(|item| item == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    Ok(Some(out))
}

/// 解析 `sort`。`None`/空 → `None`；不在白名单 → **422**（不夹到默认值）。
pub fn resolve_task_sort(value: Option<&str>) -> Result<Option<String>, ServiceError> {
    let Some(raw) = value.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    // 允许 `created_at` 与 `-created_at` 两种写法。
    let field = raw.strip_prefix('-').unwrap_or(raw);
    if !TASK_SORT_FIELDS.contains(&field) {
        return Err(ServiceError::validation(
            "invalid_download_task_filter",
            format!("未知的排序字段：{raw}"),
        ));
    }
    Ok(Some(raw.to_owned()))
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
    let _ = raw;
    todo!("骨架：照上游 validate_remote_download_task 实现（逐字段校验 provider 返回的任务）")
}

/// 列出与某索引器绑定的下载器客户端。**空列表**是合法结果（→ 422
/// `download_request_client_resolution_failed`）。
pub fn list_indexer_clients(indexer: &IndexerRow) -> Result<Vec<DownloadClientRow>, ServiceError> {
    let _ = indexer;
    todo!("骨架：查 indexer_download_client 绑定关系")
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

/// 下载器客户端的最小投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientRow {
    pub id: i32,
    pub name: String,
    /// 下载器种类。**取值由插件决定**，不是 enum。
    pub kind: String,
    pub enabled: bool,
    /// 插件配置。**原样透传** —— 各插件字段不同，宿主不解释。
    pub provider_config: serde_json::Value,
}

/// 媒体库的最小投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaLibraryRow {
    pub id: i64,
    pub name: String,
    /// provider 键（`local` / `115` / …）。
    pub provider_key: String,
    /// 插件配置。**原样透传**。
    pub provider_config: serde_json::Value,
}

/// 下载任务的最小投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTaskRow {
    pub id: i64,
    pub movie_number: String,
    /// 状态。取值由 provider 决定。
    pub state: String,
    pub client_id: Option<i32>,
}

/// 索引器的最小投影。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerRow {
    pub id: i32,
    pub name: String,
    pub enabled: bool,
}

/// provider 侧的远端任务（已校验）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDownloadTask {
    /// provider 侧的任务标识，删任务时要用。
    pub remote_task_id: String,
    pub state: String,
    pub progress: Option<f64>,
    /// 下载完成的文件路径（provider 侧）。**宿主不解释它** —— 路径是 provider
    /// 的命名空间，宿主要转存时得再经 provider 解析。
    pub download_path: Option<String>,
}

/// 插件的下载器句柄。**形状待插件 ABI 定型**，这里只占位。
pub struct PluginDownloadProvider;

/// 插件的存储句柄。**形状待插件 ABI 定型**，这里只占位。
pub struct PluginStorageProvider;

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
        assert_eq!(error.code(), "invalid_download_task_filter");
    }

    /// `None` = 不过滤；空列表 = 筛出空集。**两者不能混。**
    #[test]
    fn absent_and_empty_filters_mean_different_things() {
        assert_eq!(normalize_state_filters(None).expect("不过滤"), None);
        assert_eq!(
            normalize_state_filters(Some(&[])).expect("空列表合法"),
            Some(Vec::new()),
            "显式空列表表示「什么都不选」，不是「不过滤」"
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

    /// 排序字段是**白名单**：自由字符串会被拼进 SQL。
    #[test]
    fn sort_must_come_from_the_allow_list() {
        assert_eq!(resolve_task_sort(None).expect("缺省不排序"), None);
        assert_eq!(resolve_task_sort(Some("  ")).expect("空串不排序"), None);
        assert_eq!(
            resolve_task_sort(Some("-created_at")).expect("允许降序"),
            Some("-created_at".to_owned())
        );
        let error = resolve_task_sort(Some("id; DROP TABLE")).expect_err("注入应被拒");
        assert_eq!(error.code(), "invalid_download_task_filter");
    }

    /// 绑定多个客户端时**不猜**：报 422 让用户去配置。
    ///
    /// 猜错的后果是种子被提交到错误的下载器，而全程无报错。
    #[test]
    fn multiple_bound_clients_are_refused_rather_than_guessed() {
        let client = DownloadClientRow {
            id: 1,
            name: "qb".to_owned(),
            kind: "qbittorrent".to_owned(),
            enabled: true,
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
}
