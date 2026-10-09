//! 插件宿主：进程管理、gRPC 客户端、能力注册表
pub mod error;
pub mod extension_calls;
pub mod extensions;
pub mod jobs;
pub mod loader;
pub mod movie_delivery;
/// provider 数据面（`StorageProvider` / `DownloadProvider`）的调用面。
pub mod provider_calls;
pub mod registration;
pub mod registry;
pub mod runner;
pub mod scheduling;
pub mod supervisor;
