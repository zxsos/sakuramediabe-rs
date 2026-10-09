//! 插件宿主：进程管理、gRPC 客户端、能力注册表
pub mod error;
pub mod extension_calls;
pub mod extensions;
pub mod jobs;
pub mod loader;
// 交付校验**不在**本 crate：它是插件与宿主双方都要遵守的契约，住在
// `sm_plugin_api::movie_delivery`（见那里的模块文档）。这里再导出一次，
// 让宿主侧既有引用点保持不变。
pub use sm_plugin_api::movie_delivery;
/// provider 数据面（`StorageProvider` / `DownloadProvider`）的调用面。
pub mod provider_calls;
pub mod registration;
pub mod registry;
pub mod runner;
pub mod supervisor;
