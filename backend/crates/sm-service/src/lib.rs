//! 业务逻辑层，对应 Python 的 114 个 service 文件（25,174 行）。
//!
//! # 从哪里开始
//!
//! 上游 `src/service/` 分七个子域，按代码量：
//!
//! | 子域 | 文件 | 行数 |
//! |---|---|---|
//! | `catalog` | 27 | 7,556 |
//! | `discovery` | 16 | 4,485 |
//! | `transfers` | 23 | 4,235 |
//! | `playback` | 19 | 3,738 |
//! | `system` | 19 | 2,935 |
//! | `collections` | 5 | **1,292** |
//! | `videos` | 4 | 927 |
//!
//! **从 `collections` 开始**（[`crate::collections`]），不是因为它最重要，
//! 而是因为它最小且完整：一个垂直切片能把模块结构、错误契约、测试形态
//! 一次定型，之后六个域照此推进。
//!
//! # 这一层放什么、不放什么
//!
//! **放**：业务规则 —— 名称唯一性、系统保留名、状态机推进、去重语义。
//! 这些是 API 行为的分叉点，也是唯一值得写测试的东西。
//!
//! **不放**：查询编排。上游大量 service 方法其实是 Peewee 表达式树的
//! 组装（`Case(...)`、子查询、`fn.MAX(...)`），用于列表页排序与聚合。
//! 那部分在 Rust 里应当直接写 SQL，而不是照搬 Python 的表达式树形状 ——
//! 搬过来的结果是可读性差且无法优化。
//!
//! # 错误契约
//!
//! 见 [`crate::error::ServiceError`]：状态码与错误码绑在一起，逐条对齐
//! 上游 `ApiError(status, code, message, details)`。
pub mod catalog;
pub mod collections;
pub mod discovery;
pub mod error;
pub mod movie_numbers;
pub mod playback;
pub mod system;
pub mod transfers;
pub mod videos;
