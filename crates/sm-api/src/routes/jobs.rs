//! `POST /system/jobs/{task_key}/run` —— 手动触发一个任务。
//!
//! # 上游对应
//!
//! `src/api/routers/system/jobs.py` 的 `trigger_job`，以及它调的
//! `aps.py:submit_manual_job`（`conflict="raise"`）。
//!
//! # 只落了「触发」这一条，`GET /system/jobs` 还没落
//!
//! 列表端点要吐 `last_task_run`（每 key 最新一条运行记录）与 `params_schema`
//! 正文，两样都还缺（见 [`sm_service::system::jobs`] 的模块文档）。先把「能跑
//! 起来」落地 —— `manual_only` 的插件任务**只有**这条路能触发，没有 cron。
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
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sm_db::repo::BackgroundTaskRunRepository;
use sm_service::system::{
    job_disabled_reason, require_job_enabled, ConflictPolicy, EnqueueOutcome, JobCatalogEntry,
    TaskQueueService, FEATURE_DISABLED,
};

use crate::auth::CurrentUser;
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
    Router::new().route(
        "/system/jobs/{task_key}/run",
        post(trigger).fallback(method_not_allowed),
    )
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
