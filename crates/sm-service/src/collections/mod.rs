//! `collections` 域的 service，对应上游 `src/service/collections/`（5 个文件）。
//!
//! 上层是 `sm-api` 的路由，下层是 [`sm_db::repo::collection`] 的仓储。
//! 本模块只放**业务规则** —— 查询编排与 Peewee 表达式树不属于这里。
//!
//! # 为什么先做这个域
//!
//! 它是最小的域（1,292 行 / 5 个文件），且完全建立在刚落地的合集族仓储
//! 之上。先做一个完整的垂直切片，比横向铺开七个域更有价值：模块结构、
//! 错误契约、测试形态都在第一批里定型，后续批次照此机械推进。

pub mod playlist;

pub use playlist::{PlaylistService, PlaylistUpdate};
