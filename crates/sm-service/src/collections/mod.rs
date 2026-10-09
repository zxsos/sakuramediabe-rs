//! `collections` 域的 service，对应上游 `src/service/collections/`（5 个文件）。
//!
//! 上层是 `sm-api` 的路由，下层是 `sm_db::repo::collection` 的仓储。
//! 本模块只放**业务规则** —— 查询编排与 Peewee 表达式树不属于这里。
//!
//! # 三个文件、两个入口
//!
//! | 文件 | 模块 | 面向 |
//! |---|---|---|
//! | `playlist_service.py` | [`playlist`] | 终端用户 |
//! | `moment_/clip_collection_service.py` | [`ordered`] | 终端用户 |
//! | `plugin_collection_service.py` | [`plugin`] | 插件 facade |
//!
//! 用户侧与插件侧的规则**不同**（定位方式、找不到时的行为、参数校验失败
//! 的状态码），所以是两套入口而不是一个带参数开关的入口。

pub mod ordered;
pub mod playlist;
pub mod plugin;

pub use ordered::{
    ClipCollectionService, CollectionUpdate, MomentCollectionService, MomentCollectionSummary,
};
pub use playlist::{PlaylistService, PlaylistUpdate};
pub use plugin::PluginCollectionService;
