//! 时间戳工具。
//!
//! # 全部是 naive UTC，不是 `DateTime<Utc>`
//!
//! 既有列类型是 `timestamp without time zone`，上游注释写明「全项目统一
//! 使用 naive datetime 与数据库交互」。用 `DateTime<Utc>` 会按 `timestamptz`
//! 语义解码，与既有列不匹配。
//!
//! 需要 UTC 语义时在边界处 `.and_utc()`，不要把 `DateTime<Utc>` 存进列。

use chrono::{DateTime, NaiveDateTime, Utc};

/// 当前 UTC 时间，naive 表示。
///
/// 直接取 `Utc::now()` 截断时区。纳秒精度保留 —— PostgreSQL 的
/// `timestamp` 默认微秒精度，超出会被静默截断，所以调用方若要比较
/// 往返一致性应自己按微秒对齐。
pub fn now_utc() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// 把 naive UTC 转成带时区的 `DateTime<Utc>`。
///
/// 仅用于日志与 API 响应等**不进库**的场景。
pub fn to_utc(value: NaiveDateTime) -> DateTime<Utc> {
    value.and_utc()
}

/// 把 `DateTime<Utc>` 压成 naive UTC，用于写库。
pub fn to_naive(value: DateTime<Utc>) -> NaiveDateTime {
    value.naive_utc()
}
