//! 任务终态通知，对应上游 `activity/notifications.py` 的**写侧**。
//!
//! 上游那个文件 276 行，其中 `build_notification_query` / `list_notifications`
//! / `mark_notifications_read` / `get_unread_count` 等读侧方法服务于
//! `GET /system/notifications` 那批端点 —— 它们属于 HTTP 层，随 activity
//! 的端点一起落地。本模块只保留**收口时**被调用的部分。
//!
//! # 一条 task_run 终身只有一条终态通知
//!
//! 幂等键是 `task_run_result:{id}`。两道防线各管一件事：
//!
//! 1. `background_task_run` 的 active → terminal 行锁裁决**状态转移**的赢家；
//! 2. 这里的持久幂等键再防**调用重放**（同一执行体重试、事务重试、
//!    迟到的一方也调了收口）重复插入通知。
//!
//! 只靠第 1 条不够：赢家收口后输的一方仍会走
//! `complete_task_run(..., notify_result=...)` 的通知分支。

use sm_db::repo::{NewNotification, SystemNotificationRepository};
use sm_db::system::activity::{notification_category, BackgroundTaskRun, SystemNotification};

use crate::error::ServiceError;

/// 幂等键的前缀。键的**全文**是 `task_run_result:{task_run_id}`。
pub const TASK_RESULT_EVENT: &str = "task_run_result";
const DEDUPE_PREFIX: &str = "task_run_result:";

/// 摘要里带失败计数时，返回真。
///
/// 对应上游 `_detect_failed_summary`（`notifications.py:41-47`）：
/// 存在某个键名含 `failed`、值是大于 0 的数值的键。
///
/// # 布尔也算数
///
/// 上游写的是 `isinstance(value, (int, float))` —— Python 里 `bool` 是
/// `int` 的子类，所以 `{"failed": true}` 会命中。Rust 没有这个隐式转换，
/// 得显式把 `Value::Bool` 也算进去，否则同一份摘要在两侧行为不同。
/// 键名仍然要求含 `failed`：`{"error": true}` 不该被当成失败计数。
fn detect_failed_summary(summary: &serde_json::Value) -> bool {
    let Some(object) = summary.as_object() else {
        return false;
    };
    object.iter().any(|(key, value)| {
        if !key.contains("failed") {
            return false;
        }
        match value {
            serde_json::Value::Bool(flag) => *flag,
            serde_json::Value::Number(number) => number
                .as_f64()
                .is_some_and(|n| n > 0.0),
            _ => false,
        }
    })
}

/// 任务终态通知的幂等键。
pub fn task_result_dedupe_key(task_run_id: i32) -> String {
    format!("{DEDUPE_PREFIX}{task_run_id}")
}

/// 按终态发通知。
///
/// 对应上游 `NotificationService.notify_task_result`
/// （`notifications.py:143-175`）。**幂等**：重复调用不会产生第二条。
///
/// 三条分支的语义各不相同，别简化：
///
/// | 情形 | 行为 |
/// |---|---|
/// | 失败 | `error` 通知，**总是**发 |
/// | 成功且摘要无失败计数 | **不发** |
/// | 成功但摘要带失败计数 | `warning` 通知（部分成功） |
///
/// 第二条是「常态成功不发通知」—— 每天几十次任务成功，刷屏等于没有通知。
pub async fn notify_task_result(
    repo: &SystemNotificationRepository,
    task_run: &BackgroundTaskRun,
    failed: bool,
) -> Result<Option<SystemNotification>, ServiceError> {
    let dedupe_key = task_result_dedupe_key(task_run.id);

    if failed {
        return create_task_run_notification(
            repo,
            task_run,
            &dedupe_key,
            notification_category::ERROR,
            format!("{}执行失败", task_run.task_name),
            task_run
                .error_message
                .as_deref()
                .unwrap_or("后台任务执行失败"),
        )
        .await;
    }

    let summary = sm_db::system::activity::result_summary::from_column_text(
        task_run.result_summary.as_deref(),
    );
    if !detect_failed_summary(&summary) {
        return Ok(None);
    }

    create_task_run_notification(
        repo,
        task_run,
        &dedupe_key,
        notification_category::WARNING,
        format!("{}已完成", task_run.task_name),
        task_run.result_text.as_deref().unwrap_or("后台任务已完成"),
    )
    .await
}

async fn create_task_run_notification(
    repo: &SystemNotificationRepository,
    task_run: &BackgroundTaskRun,
    dedupe_key: &str,
    category: &str,
    title: String,
    content: &str,
) -> Result<Option<SystemNotification>, ServiceError> {
    let draft = NewNotification {
        category: category.to_owned(),
        title,
        content: content.to_owned(),
        event_type: Some(TASK_RESULT_EVENT.to_owned()),
        dedupe_key: Some(dedupe_key.to_owned()),
        resource_type: Some("task_run".to_owned()),
        resource_id: Some(task_run.id),
        related_task_run_id: Some(task_run.id),
        // 遗留展示关联字段不填 —— 走新事件身份模型。
        related_resource_type: None,
        related_resource_id: None,
    };
    // `create_once` 重复调用返回既有行，所以这里不会重复插入。
    let row = repo.create_once(&draft).await?;
    Ok(Some(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_positive_failed_count_triggers_the_warning_branch() {
        assert!(detect_failed_summary(&json!({"failed": 3})));
        assert!(detect_failed_summary(&json!({"subsets_failed": 1})));
        assert!(
            detect_failed_summary(&json!({"ok": 9, "thumbnail_failed": 2})),
            "只要有一个失败计数就是部分成功"
        );
    }

    #[test]
    fn a_zero_count_is_not_a_failure() {
        assert!(
            !detect_failed_summary(&json!({"failed": 0})),
            "0 个失败是成功，不要发 warning"
        );
        assert!(!detect_failed_summary(&json!({"failed": 0.0})));
    }

    #[test]
    fn booleans_count_because_python_int_is_a_superset_of_bool() {
        // 上游 `isinstance(value, (int, float))` 对 bool 也成立。
        assert!(detect_failed_summary(&json!({"failed": true})));
        assert!(!detect_failed_summary(&json!({"failed": false})));
    }

    #[test]
    fn the_key_must_contain_failed_not_merely_be_numeric() {
        assert!(
            !detect_failed_summary(&json!({"error": 5})),
            "键名不含 failed 的计数不是失败计数"
        );
        assert!(!detect_failed_summary(&json!({"skipped": 5})));
    }

    #[test]
    fn containers_and_empty_summaries_never_trigger() {
        assert!(!detect_failed_summary(&json!({})));
        assert!(!detect_failed_summary(&json!({"failed": [1]})));
        assert!(!detect_failed_summary(&json!({"failed": {"n": 1}})));
        assert!(!detect_failed_summary(&json!("failed")));
        assert!(!detect_failed_summary(&json!(null)));
    }

    #[test]
    fn the_dedupe_key_is_derived_from_the_run_id_alone() {
        assert_eq!(task_result_dedupe_key(42), "task_run_result:42");
    }
}
