//! 取配置快照的**唯一**入口。
//!
//! # 为什么要有这个模块
//!
//! 以前每个路由各写一份 `state.config().snapshot().unwrap_or_default()`，
//! 而 `unwrap_or_default()` 把**「配置文件读不了」**和「配置文件里没这个键」
//! 变成同一件事：全默认配置。真实后果：
//!
//! ```text
//! clip_collections.rs / media_clips.rs
//!   media_clip_root_path 变空串 -> clip_root 退化成 PathBuf::from(".")
//!   -> 产物路径相对进程工作目录解析 -> has_valid_artifact 恒为 false
//!   -> clip_count 恒为 0
//! jobs.rs
//!   调度间隔/并发全用默认值 -> 用户改了配置却不生效，且没有任何提示
//! signing.rs
//!   file_signature_secret 变空串 -> 签名校验失效
//! ```
//!
//! 触发条件很小：Windows 上一个手写的转义反斜杠就能让整个 TOML 解析失败。
//! **而这类失败在 Linux CI 上是看不见的**（路径里没有反斜杠）。
//!
//! # 现在的契约
//!
//! - **配置读不了 / 不是合法 TOML** -> [`snapshot_or_500`] 返回 500
//!   `config_invalid`，`details` 带 `config_path` 与底层原因。
//! - **文件不存在** -> 正常返回默认值（首次启动的路径，不是错误）。
//! - **配置合法但没有某个键** -> 调用方自己决定默认值，那属于业务判断。
//!
//! 组合根另有一次启动期校验（`ConfigService::validate`），所以真实部署里
//! 坏配置根本到不了请求期；这里是第二道防线。

use serde_json::Value;

use crate::error::ErrorResponse;
use crate::state::AppState;

/// 取配置快照；读不了就 500，**不退回默认值**。
///
/// 错误体里 `details.config_path` 指出是哪个文件，`details.cause` 是底层原因
/// （TOML 报错原文或 IO 错误），`details.fields` 在「能解析但字段不合法」时
/// 逐条列出 `section.field` 与原因。
pub fn snapshot_or_500(state: &AppState) -> Result<Value, ErrorResponse> {
    state.config().snapshot().map_err(ErrorResponse::from)
}

/// 从快照里取 `section.field` 的字符串值。
///
/// 与 [`snapshot_or_500`] 配对使用。键不存在时返回 `None` —— 那是**合法的**
/// 配置状态（功能没开），与「配置坏了」不同，所以这里不报错。
pub fn string_at<'a>(snapshot: &'a Value, section: &str, field: &str) -> Option<&'a str> {
    snapshot
        .get(section)
        .and_then(|value| value.get(field))
        .and_then(Value::as_str)
}
