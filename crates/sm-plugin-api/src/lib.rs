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

/// 结构化错误的过线方式（见模块文档：为什么用 `Status::details`）。
pub mod error;

/// `serde_json::Value` 与 `google.protobuf.Struct` 的互转。**两侧都要用**，
/// 所以规则只有一份（见模块文档）。
pub mod json_struct;

/// 元数据交付校验。
///
/// **插件与宿主双方**都要遵守的规则，所以放在契约仓而不是宿主实现里：
/// 作者在插件自己的测试里就能验一遍交出去的文件。见模块文档。
pub mod movie_delivery;

pub use provider::{DownloadProviderExt, StorageProviderExt};

/// 契约包名，供代码生成与文档引用。
pub const PACKAGE: &str = "sakuramedia.v1";

/// 插件 ABI 主版本。不兼容变更时递增，宿主据此拒绝加载旧插件。
///
/// # 2 —— `GenerateThumbnails` 的流换了消息类型（P1-1）
///
/// 旧契约是 `returns (stream ProgressEvent)`，新契约是
/// `returns (stream GenerateThumbnailsResponse)`（`oneof { progress, done }`）。
/// rpc 的方法名与路径**没变**，但线格式不兼容：旧二进制发来的帧在新宿主上会以
/// 「解码失败」收场，而那个错误指向不了真正的原因（两仓契约不同步）。
///
/// 所以这是**不兼容变更**，必须递增 —— 让宿主在**注册阶段**就拒掉旧二进制，
/// 而不是等到调用缩略图时才报一个指错方向的错。
///
/// 后果与修复步骤见 `docs/tasks/proto-p1-gaps.md` §二。
pub const ABI_MAJOR: i32 = 2;
