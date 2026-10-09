//! 查询参数的**契约形状**。
//!
//! # 为什么需要它：FastAPI 的 `bool` 比 serde 的 `bool` 宽松得多
//!
//! 上游端点写的是 `include_system: bool = Query(default=True)`。FastAPI
//! 把它交给 pydantic，而 **pydantic 的 lax 模式**接受 12 个字面量
//! （不区分大小写）：
//!
//! ```text
//! 真：1 / on / t / true / y / yes
//! 假：0 / off / f / false / n / no
//! ```
//!
//! serde 的 `bool` 只认 `true` / `false`。所以 `?include_system=1` 在上游是
//! `True`，在 Rust 侧会**解析失败**。
//!
//! 这不是洁癖：客户端（Flutter）里 `include_system` 这类开关通常来自
//! 持久化偏好或深链，值形态不受我们控制。而失败方式很隐蔽 ——
//! `QueryRejection` 走的是 axum 的默认路径，客户端拿到 **400 + 纯文本**，
//! 既不是上游的 422 信封，也不是「这个开关为真」。
//!
//! # 只在**默认值为真**的开关上用
//!
//! `#[serde(default = "...")]` 走的是默认值路径，不经过这个函数；
//! 只有客户端**显式传了**这个键才会走。而本模块的 [`deser_bool`] 对
//! 无法识别的值返回**错误**（→ 422），不返回 `false` —— 把拼错的
//! `?include_system=ture` 当成「关掉」会让用户拿到一个看起来正常但
//! 少了系统列表的页面，且没有任何提示。

use serde::{Deserialize, Deserializer};

/// pydantic lax 布尔：真值字面量。全部小写，匹配前先转小写。
const TRUE_LITERALS: [&str; 6] = ["1", "on", "t", "true", "y", "yes"];

/// pydantic lax 布尔：假值字面量。
const FALSE_LITERALS: [&str; 6] = ["0", "off", "f", "false", "n", "no"];

/// 按 pydantic 的 lax 规则解析查询参数里的布尔。
///
/// 用法：
///
/// ```ignore
/// #[derive(Deserialize)]
/// struct Q {
///     #[serde(default = "default_true", deserialize_with = "deser_bool")]
///     include_system: bool,
/// }
/// ```
pub fn deser_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse(&raw).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "不能解析为布尔值: {raw:?}（接受 1/0、true/false、yes/no、on/off、t/f、y/n，不区分大小写）"
        ))
    })
}

/// 与上游 `Query(default=True)` 的缺省值。
///
/// **不能**写成 `#[serde(default)]` —— 那给的是 `false`，而端点要的是
/// `true`。这个差异会让「不传 `include_system`」的请求少返回系统列表。
pub fn default_true() -> bool {
    true
}

/// 纯函数形式，便于单元测试与复用。
pub fn parse(raw: &str) -> Option<bool> {
    // 上游 pydantic 不 trim：`" true"` 不是合法布尔。这里照做 ——
    // 宽松到接受 12 个字面量已经是对齐，额外的 trim 会让某些本该 422
    // 的输入变成静默成功。
    let lowered = raw.to_ascii_lowercase();
    if TRUE_LITERALS.contains(&lowered.as_str()) {
        Some(true)
    } else if FALSE_LITERALS.contains(&lowered.as_str()) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_every_literal_pydantic_accepts() {
        for raw in ["1", "on", "t", "true", "y", "yes"] {
            assert_eq!(parse(raw), Some(true), "{raw:?} 应为真");
        }
        for raw in ["0", "off", "f", "false", "n", "no"] {
            assert_eq!(parse(raw), Some(false), "{raw:?} 应为假");
        }
    }

    #[test]
    fn is_case_insensitive_like_pydantic() {
        for raw in ["TRUE", "True", "tRuE", "YES", "On", "Y"] {
            assert_eq!(parse(raw), Some(true), "{raw:?} 应为真");
        }
        for raw in ["FALSE", "Off", "N", "NO"] {
            assert_eq!(parse(raw), Some(false), "{raw:?} 应为假");
        }
    }

    #[test]
    fn serde_alone_would_have_rejected_these() {
        // 这三个是本模块存在的理由：serde 的 bool 只认 true/false
        for raw in ["1", "yes", "on"] {
            assert!(
                serde_json::from_value::<bool>(serde_json::Value::String(raw.to_owned())).is_err(),
                "{raw:?} 不该被 serde 的 bool 接受 —— 那就没有本模块的理由"
            );
        }
    }

    #[test]
    fn rejects_near_misses_instead_of_defaulting_to_false() {
        // 拼错 / 空串 / 带空白 —— 全部 None（→ 422）
        for raw in [
            "", " ", "ture", "2", "true ", " yes", "null", "None", "yEs ",
        ] {
            assert_eq!(parse(raw), None, "{raw:?} 应被拒绝");
        }
    }

    #[test]
    fn the_default_is_true_not_false() {
        // 端点缺省是 Query(default=True)
        assert!(default_true());
    }
}
