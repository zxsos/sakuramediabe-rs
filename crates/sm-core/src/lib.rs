//! SakuraMedia 后端共享类型。
//!
//! # 契约边界
//!
//! 客户端是 Flutter，API 契约**必须逐字节不变**。本 crate 承载其中最基础、
//! 也最容易被忽视的两块：错误信封与分页响应壳。
//!
//! 宽松解析（[`json`] 模块）不是偷懒，而是对齐客户端 `json_parse.dart` 的
//! 历史行为 —— 后端某些字段会以字符串形式下发数字、把空串当 null。
//! 两端宽容度不一致时，同一份响应会解析出不同结果，且极难在集成测试里暴露。

#![forbid(unsafe_code)]

pub mod auth;
pub mod config_schema;
pub mod crontab;
pub mod error;
pub mod hashing_support;
pub mod json;
pub mod jwt;
pub mod pagination;
pub mod password;
pub mod refresh_token;
pub mod signing;

pub use auth::{AuthTokens, AuthUser, InvalidAuthResponse};
pub use error::{ApiError, ErrorEnvelope};
pub use pagination::Paginated;
pub use signing::SignatureError;
