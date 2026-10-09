//! 插件宿主：进程管理、gRPC 客户端、能力注册表
/// 插件管理的**宿主实现**（`PluginAdmin` trait 的 impl）。
///
/// 契约在 `sm_service::system::plugins`，由组合根注入 `AppState` ——
/// 理由见模块文档。
pub mod admin;
pub mod error;
pub mod extension_calls;
pub mod extensions;
/// 插件包安装：完整性校验 + 安全解压到 `<root>/.staging/<plugin_id>`。
///
/// **只到暂存为止** —— 发布（换成正式目录、保留 `data/`）与启停由 service 层
/// 的插件管理器负责，与上游 `installer.py` / `manager.py` 的切分一致。
pub mod installer;
/// 插件目录的**盘点**：扫 `<root_dir>` 列出已装的插件。
///
/// 只管文件系统那一半（装了哪些）；「有没有启用」「加载有没有失败」要读配置
/// 与运行时状态，在 `sm_service::system::plugins`。
pub mod inventory;
pub mod jobs;
pub mod loader;
/// 插件包清单 `manifest.json` 的解析与校验。
pub mod manifest;
// 交付校验**不在**本 crate：它是插件与宿主双方都要遵守的契约，住在
// `sm_plugin_api::movie_delivery`（见那里的模块文档）。这里再导出一次，
// 让宿主侧既有引用点保持不变。
pub use sm_plugin_api::movie_delivery;
/// provider 数据面（`StorageProvider` / `DownloadProvider`）的调用面。
pub mod provider_calls;
/// `sm_plugin_api::host` trait 的 gRPC 实现（给 `sm-server` 注入用）。
pub mod host_impl;
pub mod registration;
pub mod registry;
pub mod runner;
pub mod supervisor;
/// 插件版本号比较（PEP 440 的**常用子集**），服务「升级包必须更高版本」。
pub mod versions;
