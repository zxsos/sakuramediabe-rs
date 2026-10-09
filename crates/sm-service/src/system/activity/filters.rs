//! 查询参数的归一化与白名单校验，对应上游 `activity/filters.py`（22 行）。
//!
//! # 空白与 `None` 是两件事
//!
//! [`normalize_string_filter`] 把纯空白折叠成 `None`，因为上游 SQL 是
//! `WHERE col = %s` —— 传空串会匹配到「值正好是空串的行」，而不是「不过滤」。
//! 这两种语义的差别在 `task_key` 上尤其致命：空串匹配不到任何任务，
//! 表现为「筛选没生效」而不是「筛选报错」。
//!
//! # 大小写：状态与分类要折叠，任务键不要
//!
//! [`normalize_allowed_filter`] 会 `lower()` 后再判白名单，因为
//! `state` / `trigger_type` / `category` 的字面量都是小写，而 HTTP 查询参数
//! 由用户随手输入。`task_key` 走 [`normalize_string_filter`] ——
//! 任务键**区分大小写**，折叠会把一个真实存在的任务键改成另一个不存在的。

use serde_json::Value;

use crate::error::{details_of, ServiceError};

/// 折叠纯空白为 `None`。
pub fn normalize_string_filter(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(ToOwned::to_owned)
}

/// 折叠纯空白为 `None`，非空则 `lower()` 后校验白名单。
///
/// 非法值报 422 `invalid_activity_filter` —— 与上游一致（`filters.py:20-26`）。
/// **不回落默认值**：筛选条件写错应当报错，静默忽略会让人以为筛了却没有。
pub fn normalize_allowed_filter(
    value: Option<&str>,
    field_name: &str,
    allowed_values: &[&str],
) -> Result<Option<String>, ServiceError> {
    let Some(normalized) = normalize_string_filter(value) else {
        return Ok(None);
    };
    let lowered = normalized.to_lowercase();
    if allowed_values.contains(&lowered.as_str()) {
        return Ok(Some(lowered));
    }
    // details 带上出错值与允许值：客户端据此高亮对应控件并给出可选列表。
    let mut allowed: Vec<&str> = allowed_values.to_vec();
    allowed.sort_unstable();
    let mut details = details_of("field_name", field_name);
    details.insert("value".to_owned(), Value::String(normalized));
    details.insert(
        "allowed_values".to_owned(),
        Value::Array(allowed.into_iter().map(|item| Value::String(item.to_owned())).collect()),
    );
    Err(ServiceError::validation_with(
        "invalid_activity_filter",
        format!("{field_name} is invalid"),
        details,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [&str; 4] = ["pending", "running", "completed", "failed"];

    #[test]
    fn blanks_become_none_rather_than_matching_empty_strings() {
        assert_eq!(normalize_string_filter(None), None);
        assert_eq!(normalize_string_filter(Some("")), None);
        assert_eq!(normalize_string_filter(Some("   \t ")), None);
        assert_eq!(
            normalize_string_filter(Some("  movie_heat_update  ")),
            Some("movie_heat_update".to_owned())
        );
    }

    #[test]
    fn allowed_values_are_lowercased_before_matching() {
        assert_eq!(
            normalize_allowed_filter(Some(" RUNNING "), "state", &STATES).expect("合法"),
            Some("running".to_owned())
        );
        assert_eq!(
            normalize_allowed_filter(Some("   "), "state", &STATES).expect("空白视作未给"),
            None
        );
    }

    #[test]
    fn an_unknown_value_is_an_error_not_a_silent_no_op() {
        let err = normalize_allowed_filter(Some("succeeded"), "state", &STATES)
            .expect_err("未知状态必须报错");
        // 曾经把成功态写成 succeeded 的那笔历史事故就在这里 —— 静默忽略会
        // 让查询返回「已完成」之外的空集，而不是告诉用户状态名写错了。
        //
        // `ServiceError` 没有实现 Display，所以断言机器可读的 code 与
        // details 键，而不是错误正文。
        assert_eq!(err.code(), "invalid_activity_filter");
        let details = err.details().expect("带 details");
        assert_eq!(details.get("field_name"), Some(&serde_json::json!("state")));
        assert_eq!(details.get("value"), Some(&serde_json::json!("succeeded")));
        // allowed_values 必须排过序，否则客户端拿到的候选列表顺序不稳定。
        assert_eq!(
            details.get("allowed_values"),
            Some(&serde_json::json!(["completed", "failed", "pending", "running"]))
        );
    }
}
