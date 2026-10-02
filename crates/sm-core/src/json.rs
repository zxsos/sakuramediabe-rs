//! 宽松 JSON 字段解析助手。
//!
//! 对应客户端 `lib/core/json/json_parse.dart`。那份文件记录了后端的历史行为：
//! 「后端字段类型偶有漂移（数字以字符串下发、空串当 null 等）」。
//!
//! Rust 侧重写时必须保留同等宽松度，否则同一份响应在两端会解析出不同结果，
//! 这类漂移最难在集成测试里发现。

use serde_json::Value;

/// 宽松转 `i64`：接受整数、浮点（截断）、数字字符串。
///
/// 对应 Dart 的 `asIntOrNull`。
pub fn as_int_or_null(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 宽松转 `i64`，失败返回 `fallback`。对应 Dart 的 `asInt(value, fallback:)`。
pub fn as_int(value: &Value, fallback: i64) -> i64 {
    as_int_or_null(value).unwrap_or(fallback)
}

/// 宽松转布尔；接受布尔、0/1 数字、"true"/"false"/"yes"/"no" 字符串。
///
/// Dart 侧没有这个助手，但 Rust 侧需要它来放宽 serde 对 `bool` 的严格性 ——
/// 否则后端输出 `1` 而字段声明为 `bool` 时会直接反序列化失败。
pub fn as_bool_or_null(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 解析 ISO-8601 时间戳；非字符串或 trim 后为空时返回 `None`。
///
/// 对应 Dart 的 `asDateTime`。**解析失败同样返回 `None`** —— 这一点很关键：
/// 后端偶尔下发格式非法的历史时间，客户端会静默忽略，重写后必须一致。
pub fn as_datetime(value: &Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed);
    }
    // 退而接受无时区的朴素格式；后端统一用 UTC。
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").ok()
        })
        .map(|naive| {
            let offset = chrono::FixedOffset::east_opt(0).expect("UTC 偏移恒合法");
            chrono::DateTime::from_naive_utc_and_offset(naive, offset)
        })
}

/// 取字符串；`trim` 为 true 时先 trim，且 trim 后为空返回 `None`。
pub fn as_string_or_null(value: &Value, trim: bool) -> Option<String> {
    let text = value.as_str()?;
    if !trim {
        return Some(text.to_owned());
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 宽松转对象；非对象返回 `None`。对应 Dart 的 `asMapOrNull`。
pub fn as_object_or_null(value: &Value) -> Option<serde_json::Map<String, Value>> {
    value.as_object().cloned()
}

/// 宽松转对象；非对象返回空对象。对应 Dart 的 `asMap`。
pub fn as_object(value: &Value) -> serde_json::Map<String, Value> {
    as_object_or_null(value).unwrap_or_default()
}

/// 宽松转字符串数组；非数组返回空列表，只保留字符串项。
pub fn as_string_list(value: &Value, trim: bool) -> Vec<String> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| as_string_or_null(item, trim))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn int_accepts_number_string_and_float() {
        assert_eq!(as_int_or_null(&json!(42)), Some(42));
        assert_eq!(as_int_or_null(&json!("42")), Some(42));
        assert_eq!(as_int_or_null(&json!(42.9)), Some(42));
        assert_eq!(as_int_or_null(&json!(" 42 ")), Some(42));
        assert_eq!(as_int_or_null(&json!("abc")), None);
        assert_eq!(as_int_or_null(&json!(null)), None);
        assert_eq!(as_int_or_null(&json!([1])), None);
        assert_eq!(as_int(&json!("bad"), 7), 7);
    }

    #[test]
    fn bool_accepts_number_and_string() {
        assert_eq!(as_bool_or_null(&json!(true)), Some(true));
        assert_eq!(as_bool_or_null(&json!(1)), Some(true));
        assert_eq!(as_bool_or_null(&json!(0)), Some(false));
        assert_eq!(as_bool_or_null(&json!("true")), Some(true));
        assert_eq!(as_bool_or_null(&json!("False")), Some(false));
        assert_eq!(as_bool_or_null(&json!("maybe")), None);
        assert_eq!(as_bool_or_null(&json!(null)), None);
    }

    #[test]
    fn datetime_rejects_empty_and_malformed() {
        assert!(as_datetime(&json!("2026-01-02T03:04:05Z")).is_some());
        assert!(as_datetime(&json!("2026-01-02T03:04:05+08:00")).is_some());
        assert!(as_datetime(&json!("2026-01-02 03:04:05")).is_some());
        // 空串与纯空白必须视为 null。
        assert!(as_datetime(&json!("")).is_none());
        assert!(as_datetime(&json!("   ")).is_none());
        // 非法格式返回 None 而非报错，与 Dart 的 DateTime.tryParse 一致。
        assert!(as_datetime(&json!("not-a-date")).is_none());
        assert!(as_datetime(&json!(20260102)).is_none());
    }

    #[test]
    fn string_trim_semantics() {
        assert_eq!(as_string_or_null(&json!(" x "), false), Some(" x ".to_owned()));
        assert_eq!(as_string_or_null(&json!(" x "), true), Some("x".to_owned()));
        assert_eq!(as_string_or_null(&json!("  "), true), None);
        assert_eq!(as_string_or_null(&json!("  "), false), Some("  ".to_owned()));
        assert_eq!(as_string_or_null(&json!(7), true), None);
    }

    #[test]
    fn object_and_list_tolerate_wrong_types() {
        assert!(as_object_or_null(&json!({"a": 1})).is_some());
        assert!(as_object_or_null(&json!([1])).is_none());
        assert!(as_object(&json!("nope")).is_empty());

        assert_eq!(as_string_list(&json!(["a", "b"]), false), vec!["a", "b"]);
        // 非字符串项被丢弃，而不是报错。
        assert_eq!(as_string_list(&json!(["a", 1, null]), false), vec!["a"]);
        assert_eq!(as_string_list(&json!("x"), false), Vec::<String>::new());
        assert_eq!(as_string_list(&json!([" a ", " "]), true), vec!["a"]);
    }
}
