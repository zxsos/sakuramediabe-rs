//! `JsonTextField` 的双向转换。
//!
//! # 列类型是 TEXT，不是 JSON
//!
//! 上游 `JsonTextField` 继承 `TextField`（`JsonbField` 才继承成 JSONB），
//! 所以这些列在 PostgreSQL 里是 `text`，内容是 JSON 字符串。Rust 侧对应
//! `Option<String>`，**不能**用 `serde_json::Value` 直接解码 ——
//! 那需要列类型是 json/jsonb。
//!
//! # 空串 == NULL
//!
//! 上游 `python_value` 的实现：
//!
//! ```python
//! def python_value(self, value: Any) -> Any:
//!     if value is None or value == "":
//!         return None
//!     if isinstance(value, (dict, list)):
//!         return value
//!     return json.loads(value)
//! ```
//!
//! **空串被显式折叠成 `None`**。这不是细节：库里出现空串说明某次写入
//! 绕过了 peewee 的 `db_value`（比如裸 SQL），读取时必须把它当 NULL，
//! 否则上层会拿到 `Some("")` 然后解析失败。

use serde_json::Value;

/// 把 Rust 值序列化成 `JsonTextField` 列的文本内容。
///
/// 返回 `None` 表示写 NULL —— 调用方应把 `None` 显式表达，不要写空串。
/// 空串在读取时会被折叠成 `None`，写进去等于丢数据。
pub fn encode(value: &Value) -> Option<String> {
    // ensure_ascii=false 与上游 db_value 一致：中文直接存 UTF-8，
    // 不转成 \uXXXX 转义，否则库里可读性差且体积变大。
    Some(value.to_string())
}

/// 解析 `JsonTextField` 列的文本内容。
///
/// 复刻 `python_value` 的空串语义：**空串与纯空白都返回 `None`**。
/// 非法 JSON 同样返回 `None` 而不是 Err —— 上游 `json.loads` 抛异常，
/// 但那会让一次脏数据把整个列表接口打断；这里选择与
/// [`parse_or_default`] 相同的宽松策略，由调用方决定是否需要严格模式。
pub fn decode(raw: Option<&str>) -> Option<Value> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// 严格版 [`decode`]：非法 JSON 返回 `Err`。
///
/// 写路径校验用户输入时用这个 —— 静默吞掉非法 JSON 会让脏数据落库，
/// 而读路径用 [`decode`] 则是为了不让一行脏数据打断整个列表。
pub fn decode_strict(raw: Option<&str>) -> Result<Option<Value>, serde_json::Error> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => serde_json::from_str(text).map(Some),
    }
}

/// 解析并在缺失时回退到默认值。
///
/// 用于「本该有值但可能为空」的列，例如 `Image.origin` 的
/// `JsonTextField(default=dict)`。
pub fn parse_or_default(raw: Option<&str>, default: Value) -> Value {
    decode(raw).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_preserves_unicode_unescaped() {
        // ensure_ascii=false：中文不应变成 \uXXXX
        let value = json!({"title": "电影"});
        let text = encode(&value).unwrap();
        assert!(text.contains("电影"), "got {text}");
        assert_eq!(decode(Some(&text)), Some(value));
    }

    #[test]
    fn empty_and_blank_string_become_none() {
        // 这是 python_value 的明确语义：空串 == NULL
        assert_eq!(decode(Some("")), None);
        assert_eq!(decode(Some("   ")), None);
        assert_eq!(decode(None), None);
        // 空白也要折叠 —— 数据库里存空格与存空串语义相同.
        assert_eq!(decode_strict(Some("   ")).unwrap(), None);
    }

    #[test]
    fn encode_never_produces_empty_string() {
        // 写空串等于丢数据：读取时会被折叠成 None。
        let text = encode(&json!({})).unwrap();
        assert_eq!(text, "{}");
        assert!(!text.is_empty());
        assert!(!text.trim().is_empty());
    }

    #[test]
    fn strict_mode_surfaces_malformed_json() {
        // 写路径要能发现非法 JSON，读路径不应被一行脏数据打断。
        assert!(decode_strict(Some("{not json")).is_err());
        // 宽松模式静默跳过
        assert_eq!(decode(Some("{not json")), None);
    }

    #[test]
    fn scalars_and_arrays_roundtrip() {
        // JsonTextField 能装任意 JSON，不只是对象。
        for value in [json!(null), json!(1), json!("s"), json!([]), json!([1, 2])] {
            let text = encode(&value).unwrap();
            assert_eq!(decode(Some(&text)), Some(value.clone()), "value={value}");
        }
    }

    #[test]
    fn default_applies_only_when_absent_or_malformed() {
        let fallback = json!({"k": "v"});
        assert_eq!(parse_or_default(None, fallback.clone()), fallback);
        assert_eq!(parse_or_default(Some(""), fallback.clone()), fallback);
        assert_eq!(parse_or_default(Some("{bad"), fallback.clone()), fallback);
        assert_eq!(
            parse_or_default(Some(r#"{"a":1}"#), fallback),
            json!({"a": 1})
        );
    }
}
