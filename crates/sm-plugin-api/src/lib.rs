//! SakuraMedia 插件 gRPC 契约。
//!
//! 由 `proto/` 下四个文件生成，统一 package `sakuramedia.v1`：
//!
//! - `common.proto`   共享类型（句柄、结果、枚举、错误）
//! - `storage.proto`  存储与下载 Provider（30 个方法全覆盖）
//! - `plugin.proto`   插件生命周期、任务、扩展点
//! - `host.proto`     宿主提供给插件的能力（PluginContext）
//!
//! 统一 package 而非分包，是为了让 prost 生成同包引用：
//!! 跨包引用会要求调用方提供额外的模块层级，
//! 而这层��在 build.rs 里无法可靠表达。
//!
//! 设计说明见仓库根 `docs/plugin-abi.md`。
//!
/// 自动生成的 prost/tonic 类型。
pub mod v1 {
    #![allow(clippy::all)]
    tonic::include_proto!("sakuramedia.v1");
}

/// 插件侧默认实现层：把生成的 trait 的 37 个方法都变成有默认体的。
///
/// 见 [`provider`] 的模块文档（gRPC 报告的 P1-4）。
pub mod provider;

pub use provider::{DownloadProviderExt, StorageProviderExt};

/// 契约包名，供代码生成与文档引用。
pub const PACKAGE: &str = "sakuramedia.v1";

/// 插件 ABI 主版本。不兼容变更时递增，宿主据此拒绝加载旧插件。
pub const ABI_MAJOR: i32 = 1;
