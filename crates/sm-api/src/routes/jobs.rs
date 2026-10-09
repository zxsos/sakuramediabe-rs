//! `GET /system/jobs` 与 `POST /system/jobs/{task_key}/run` —— 任务目录与手动触发。
//!
//! # 上游对应
//!
//! `src/api/routers/system/jobs.py` 的 `list_jobs` 与 `trigger_job`，后者调
//! `aps.py:submit_manual_job`（`conflict="raise"`）。
//!
//! # 目录是**快照**，不是活注册表
//!
//! 插件的注册表在 `sm-plugins` 里、由组合根持有，而 `sm-api` 不能依赖
//! `sm-server`（反向依赖会成环）。所以 `AppState` 拿的是一份纯数据
//! 目录，组合根每次加载或重建插件表之后换一份新的。代价是插件崩溃重启后
//! **新增**的任务不会出现在目录里，要等进程重启 —— 与 `sm-plugins` 模块文档
//! 里记的那个调度表局限是同一个成因。
//!
//! # `manual_trigger_allowed` 在响应里被改写
//!
//! 目录里存的是任务**声明**；响应里给的是「声明允许 **且** 当前未被停用」
//! （上游 `jobs.py:46`）。前端据此决定按钮是否置灰 —— 直接透传声明值会让
//! 能力关闭时按钮仍可点，点下去吃 409。
//!
//! # `params_schema` 当前恒为 `null`
//!
//! 上游给的是 Pydantic 模型的 `model_json_schema()`。插件任务的 schema 正文
//! 在 proto 里是 `google.protobuf.Struct`，转成 `serde_json::Value` 的那一层
//! 还没写；目录里只有 `has_params_schema`（有没有）这个布尔。**不要**拿它
//! 拼一个假 schema —— 前端会照着渲染表单，然后提交一份服务端不认的参数。
//!
//! # 三条错误分别是三种意思
//!
//! | 状态 | 码 | 含义 |
//! |---|---|---|
//! | 404 | `job_not_found` | 目录里没有这个 key（拼错 / 插件没加载） |
//! | 403 | `manual_trigger_forbidden` | 任务在，但不允许经接口触发 |
//! | 409 | `feature_disabled` | 任务在，但它依赖的能力没开（如向量服务） |
//! | 409 | `task_conflict` | 同 key 已有排队/在跑的行 —— 冲突是谁、什么状态都要带出来 |
//! | 422 | `invalid_job_params` | 该带 body 没带 / 不该带带了 |

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_db::repo::BackgroundTaskRunRepository;
use sm_service::system::{
    job_disabled_reason, require_job_enabled, ConflictPolicy, EnqueueOutcome, JobCatalogEntry,
    TaskQueueService, TaskRunService, FEATURE_DISABLED,
};

use crate::auth::CurrentUser;
use crate::dto::{JobMetadataResource, TaskRunResource};
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

/// 手动触发的响应。字段与上游 `ManualJobTriggerResponse` 一致。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualJobTriggerResponse {
    pub task_run_id: i32,
    pub task_key: String,
    pub state: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/system/jobs", get(list_jobs).fallback(method_not_allowed))
        .route(
            "/system/jobs/{task_key}/run",
            post(trigger).fallback(method_not_allowed),
        )
}

/// 任务目录。对应上游 `list_jobs`（`jobs.py:56-59`）。
///
/// 每项的 `last_task_run` 来自**一次**子查询（`MAX(id) GROUP BY task_key`），
/// 不是 N+1 —— 21 个任务就是 21 次往返，逐项查会变成 42 次。
async fn list_jobs(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<JobMetadataResource>>, ErrorResponse> {
    let config = state.config().snapshot().unwrap_or_default();
    let entries = state.jobs().entries().to_vec();
    let task_keys: Vec<String> = entries.iter().map(|entry| entry.task_key.clone()).collect();

    // 走服务层而不是直接调仓储：`sm-api::error` 没有 `From<DbError>`，
    // 而且 `api → service → db` 才是分层该有的方向。
    let task_runs = TaskRunService::new(state.db());
    let latest = task_runs.latest_runs_by_task_key(&task_keys).await?;

    let items = entries
        .iter()
        .map(|entry| {
            // 上游 `jobs.py:36` 在**构建每项时**各算一次，不要提到循环外 ——
            // 一次快照算一次就够，同一轮里配置不会变。
            let disabled_reason = job_disabled_reason(&entry.task_key, &config);
            JobMetadataResource {
                task_key: entry.task_key.clone(),
                plugin_id: entry.plugin_id.clone(),
                log_name: entry.log_name.clone(),
                cli_name: entry.cli_name.clone(),
                cli_help: entry.cli_help.clone(),
                cron_setting: entry.cron_setting.clone(),
                cron_expr: entry.cron_expr.clone(),
                disabled_reason: disabled_reason.clone(),
                // 「声明允许」且「当前没被停用」—— 见模块文档。
                manual_trigger_allowed: entry.manual_trigger_allowed
                    && disabled_reason.is_none(),
                params_schema: None,
                last_task_run: latest.get(&entry.task_key).map(TaskRunResource::from),
            }
        })
        .collect();

    Ok(Json(items))
}

async fn trigger(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(task_key): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<ManualJobTriggerResponse>), ErrorResponse> {
    let Some(entry) = state.jobs().get(&task_key) else {
        return Err(ErrorResponse::new(
            StatusCode::NOT_FOUND,
            "job_not_found",
            format!("未知任务 task_key={task_key}"),
        ));
    };
    if !entry.manual_trigger_allowed {
        return Err(ErrorResponse::new(
            StatusCode::FORBIDDEN,
            "manual_trigger_forbidden",
            format!("任务 {task_key} 不允许通过接口手动触发"),
        ));
    }
    // 能力开关（上游 `require_job_enabled`）：409 而不是 422 —— 该去开配置，
    // 不是改请求。
    let config = state.config().snapshot().unwrap_or_default();
    if let Some(reason) = job_disabled_reason(&task_key, &config) {
        return Err(ErrorResponse::new(
            StatusCode::CONFLICT,
            FEATURE_DISABLED,
            reason,
        ));
    }
    require_job_enabled(&task_key, &config)
        .map_err(|_| ErrorResponse::new(StatusCode::CONFLICT, FEATURE_DISABLED, "任务未启用"))?;

    let params = params_of(entry, &task_key, parse_body(&body)?)?;

    let service = TaskQueueService::new(state.db());
    let outcome = service
        .enqueue(
            &task_key,
            "manual",
            Some(&entry.cli_help),
            params,
            ConflictPolicy::Raise,
        )
        .await?;

    let run = match &outcome {
        EnqueueOutcome::Enqueued(run) => run.as_ref(),
        EnqueueOutcome::Skipped {
            blocking_task_run_id,
        } => return Err(conflict_response(state, &task_key, *blocking_task_run_id).await),
    };

    Ok((
        StatusCode::ACCEPTED,
        Json(ManualJobTriggerResponse {
            task_run_id: run.id,
            task_key: task_key.clone(),
            state: run.state.clone(),
        }),
    ))
}

/// 请求体。**不带 body 是合法的** —— 上游 `payload: dict | None = None`。
///
/// 走 `Bytes` 自己解析而不是 `Json<...>`：后者对空 body 直接拒（422），而
/// 「不带参跑一次」正是有 cron 的任务最常走的那条路。
fn parse_body(body: &[u8]) -> Result<Option<serde_json::Value>, ErrorResponse> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Null) => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(err) => Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            format!("请求体不是合法 JSON：{err}"),
        )),
    }
}

/// 上游的参数三条规则。
///
/// 与上游的一处**刻意差异**：显式给了 body 且任务声明了 schema 时，上游按
/// schema 校验；这里**原样透传** —— schema 正文还没进目录（见
/// [`sm_service::system::jobs`] 的模块文档），而「拒绝一个其实合法的参数」比
/// 「把校验交给执行侧」更难排查。
fn params_of(
    entry: &JobCatalogEntry,
    task_key: &str,
    payload: Option<serde_json::Value>,
) -> Result<Option<serde_json::Value>, ErrorResponse> {
    match payload {
        None => {
            // 上游只对「声明了参数模型的 manual_only 任务」强制要求 body：
            // 有 cron 的任务不带参触发就是按默认参数跑一次。
            if entry.has_params_schema && entry.cron_expr.is_none() {
                return Err(ErrorResponse::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_job_params",
                    format!("任务 {task_key} 必须提供请求参数"),
                ));
            }
            Ok(None)
        }
        Some(_) if !entry.has_params_schema => Err(ErrorResponse::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_job_params",
            format!("任务 {task_key} 不支持请求参数"),
        )),
        Some(value) => Ok(Some(value)),
    }
}

/// 409 `task_conflict`：把挡住的是谁一并带出来（上游 details 三件套）。
async fn conflict_response(
    state: AppState,
    task_key: &str,
    blocking_task_run_id: Option<i32>,
) -> ErrorResponse {
    let repo = BackgroundTaskRunRepository::new(state.db().clone());
    let blocking = match blocking_task_run_id {
        Some(id) => repo.find_by_id(id).await.ok().flatten(),
        None => None,
    };
    let mut response = ErrorResponse::new(
        StatusCode::CONFLICT,
        "task_conflict",
        format!("任务 {task_key} 已在队列或执行中"),
    );
    if let Some(run) = blocking {
        let mut details = serde_json::Map::new();
        details.insert("blocking_task_run_id".to_owned(), serde_json::json!(run.id));
        details.insert(
            "blocking_trigger_type".to_owned(),
            serde_json::json!(run.trigger_type),
        );
        details.insert("blocking_state".to_owned(), serde_json::json!(run.state));
        response = response.with_details(details);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_service::system::JobCatalog;

    fn entry(task_key: &str, cron: Option<&str>, params: bool) -> JobCatalogEntry {
        JobCatalogEntry {
            task_key: task_key.to_owned(),
            log_name: task_key.to_owned(),
            cli_name: task_key.to_owned(),
            cli_help: "一个任务".to_owned(),
            plugin_id: None,
            cron_setting: None,
            cron_expr: cron.map(str::to_owned),
            manual_trigger_allowed: true,
            has_params_schema: params,
        }
    }

    #[test]
    fn an_absent_body_means_no_params() {
        // 完全不带 body、空白、显式 null 都是「不带参」 —— 上游
        // `payload: dict | None = None` 的三态。
        for body in ["", "   ", "null"] {
            assert_eq!(
                parse_body(body.as_bytes()).expect("应当放行"),
                None,
                "{body:?}"
            );
        }
        // 空对象 **是** 一次带参调用（上游明说）。
        assert_eq!(
            parse_body(b"{}").expect("应当放行"),
            Some(serde_json::json!({}))
        );
    }

    #[test]
    fn a_malformed_body_is_rejected() {
        let err = parse_body(b"{").expect_err("非法 JSON");
        assert_eq!(err.error.code, "validation_error");
    }

    #[test]
    fn a_manual_only_task_without_params_is_rejected() {
        let entry = entry("only", None, true);
        let err = params_of(&entry, "only", None).expect_err("必须给参数");
        assert_eq!(err.error.code, "invalid_job_params");
    }

    #[test]
    fn a_task_without_a_schema_refuses_an_explicit_body() {
        // 无参任务不接受 body：否则调用方会以为参数生效了。
        let entry = entry("plain", Some("0 3 * * *"), false);
        let err =
            params_of(&entry, "plain", Some(serde_json::json!({}))).expect_err("不该接受 body");
        assert_eq!(err.error.code, "invalid_job_params");
    }

    #[test]
    fn a_cron_task_may_be_triggered_without_a_body() {
        // 有 cron 的任务手动触发一次 = 按默认参数跑一次。
        let entry = entry("nightly", Some("0 3 * * *"), true);
        assert_eq!(params_of(&entry, "nightly", None).expect("应当放行"), None);
    }

    #[test]
    fn params_are_passed_through_when_the_task_declares_a_schema() {
        let entry = entry("only", None, true);
        let payload = serde_json::json!({"reset": true});
        assert_eq!(
            params_of(&entry, "only", Some(payload.clone())).expect("应当放行"),
            Some(payload)
        );
    }

    #[test]
    fn an_unknown_task_key_is_not_in_the_catalog() {
        let catalog = JobCatalog::new(vec![entry("known", None, false)]);
        assert!(catalog.get("unknown").is_none());
    }
}
