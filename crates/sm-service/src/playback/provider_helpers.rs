//! 插件句柄工厂（上游 `playback/provider_helpers.py`，25 行）。
//!
//! # 只有 25 行，但它是**整个 playback 域的入口**
//!
//! ```python
//! def library_handle_for(library: MediaLibrary) -> LibraryHandle
//! def media_handle_for(media: Media) -> MediaHandle
//! ```
//!
//! 两个函数都**不发任何请求** —— 它们只从 DB 记录构造句柄：`deepcopy` 一份
//! `provider_config` / `storage_ref`，带上 provider key。
//!
//! # ★ `deepcopy` 不是细节，是**安全边界**
//!
//! 上游显式 `deepcopy(provider_config)`。原因是 `provider_config` 来自
//! `jsonb`，而句柄会被交给 provider 插件；若不拷贝，插件一次 in-place 修改
//! 就可能**改到调用方的数据**（而调用方往往还拿着它做别的事）。
//!
//! Rust 侧对应 `Clone`（`serde_json::Value` 的深拷贝）—— 语义一致。
//!
//! # 句柄里**没有**密钥
//!
//! `provider_config` 是插件的**配置**（库 id、路径前缀…），凭据在插件自己的
//! 进程内（见 `docs/adr/2026-10-05-plugin-lifecycle.md`）。所以句柄可以
//! 安全地跨线程传递、日志里打印。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 媒体库句柄。**发给插件**的形态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryHandle {
    /// 库 id（宿主侧主键）。
    pub library_id: i64,
    /// provider 键（`local` / `115` / …）。插件据此选实现。
    pub provider_key: String,
    /// 插件配置。**已深拷贝**（见模块文档）。
    pub provider_config: serde_json::Value,
}

/// 媒体句柄。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaHandle {
    pub media_id: i64,
    pub library_id: i64,
    /// provider 键。**冗余**（也能从 library 查到）—— 但省一次查询，
    /// 且插件处理单个媒体时不该被迫回查库。
    pub provider_key: String,
    /// 宿主侧的存储引用。**宿主不解释它的内容**（是 provider 的命名空间）。
    pub storage_ref: serde_json::Value,
    /// 该媒体的库配置。**深拷贝**。
    pub provider_config: serde_json::Value,
}

/// 媒体库的最小投影（构造句柄所需）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryRecord {
    pub id: i64,
    pub provider_key: String,
    pub provider_config: serde_json::Value,
}

/// 媒体的最小投影（构造句柄所需）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRecord {
    pub id: i64,
    pub library_id: i64,
    pub storage_ref: serde_json::Value,
    /// 所属库的配置。**由调用方 JOIN 出来** —— 构造句柄需要它。
    pub provider_config: serde_json::Value,
    /// 所属库的 provider 键。同上。
    pub provider_key: String,
}

/// 构造媒体库句柄。**纯函数**。
pub fn library_handle_for(library: &LibraryRecord) -> LibraryHandle {
    LibraryHandle {
        library_id: library.id,
        provider_key: library.provider_key.clone(),
        // `Clone` 即深拷贝：插件改不到我们的数据。
        provider_config: library.provider_config.clone(),
    }
}

/// 构造媒体句柄。**纯函数**。
pub fn media_handle_for(media: &MediaRecord) -> MediaHandle {
    MediaHandle {
        media_id: media.id,
        library_id: media.library_id,
        provider_key: media.provider_key.clone(),
        storage_ref: media.storage_ref.clone(),
        provider_config: media.provider_config.clone(),
    }
}

/// 取 provider 能力。未安装该 provider → **503 `provider_not_installed`**。
///
/// ⚠️ 503 而不是 404：库记录**存在**，只是负责它的插件没装。这是配置/
/// 部署问题，客户端该提示「去装插件」，不是「资源不存在」。
pub fn require_provider(provider_key: &str) -> Result<PluginStorageProvider, ServiceError> {
    let _ = provider_key;
    todo!("骨架：经 sm-plugins 取 media.provider 能力；未装 -> 503")
}

/// 插件的存储能力。**形状待插件 ABI 定型**。
///
/// ★ 这里刻意用 **struct** 而不是 `trait`：它出现在
/// [`require_provider`] 的返回类型位置上，而 trait 不能作返回类型
/// （需要 `dyn Trait` 或泛型，而那时我们还没有可用的具体实现）。
///
/// 真正的多态接入（按 `provider_key` 分发到不同插件）是 `sm-plugins` 的事；
/// 那一层落地后这里会换成持有 gRPC client 的 struct。
pub struct PluginStorageProvider {
    pub provider_key: String,
}

/// 空间占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceUsage {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}
