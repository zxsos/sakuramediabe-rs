//! 仓储层共用基础设施。
//!
//! 这些模块解决的是**横切关注点**：时间戳语义、JSON 列的两种存储形态、
//! UPDATE 语句的强制构造、字段写入护栏。放在 `common` 而不是各 Repository
//! 内部，是因为它们对全部 40 张表都成立 —— 任何一处特例化都会让「漏了
//! `updated_at`」这类静默错误重新出现。
//!
//! | 模块 | 解决什么 |
//! |---|---|
//! | [`time`] | naive UTC 时间戳（列是 `timestamp without time zone`） |
//! | [`json_text`] | `JsonTextField`：TEXT 列装 JSON 文本，空串 == NULL |
//! | [`update`] | 把 `SET updated_at = now()` 固化进 API 形状 |
//! | [`guard`] | 字段护栏：受保护字段与插件白名单 |
//! | [`advisory_lock`] | 会话级 advisory lock：媒体 I/O 的短时独占 |

pub mod advisory_lock;
pub mod guard;
pub mod json_text;
pub mod page;
pub mod time;
pub mod update;

pub use advisory_lock::{AdvisoryLock, LockUnavailable};
pub use guard::{FieldGuard, WriteSource};
pub use json_text::{decode as decode_json_text, encode as encode_json_text};
pub use page::{Page, PageRequest};
pub use time::now_utc;
pub use update::{UpdateSet, Value as UpdateValue};
