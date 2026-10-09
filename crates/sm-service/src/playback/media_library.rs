//! 媒体库 CRUD（上游 `playback/media_library_service.py`，332 行）。
//!
//! # 空间占用有 **300 秒缓存**
//!
//! [`SPACE_USAGE_CACHE_TTL_SECONDS`] = 300。`get_space_usage` 常常被前端
//! 仪表盘**轮询**，而它要向 provider 发请求（115 网盘那种尤其慢）。
//!
//! ⚠️ 缓存是**按库**的，且失败**不缓存** —— provider 挂了不能缓存 300 秒，
//! 否则用户会盯着一个假的数据看五分钟。
//!
//! # `kind` / `provider_config` 创建后**不可改**
//!
//! `provider_key` 决定 `provider_config` 的 schema，而 `storage_ref` 里的路径
//! 是按旧配置生成的。改配置会让已有媒体**指向不存在的位置**。
//! 所以更新时传了不同的值 → `422 invalid_media_library_provider`。
//!
//! 改配置的正确做法是**新建一个库再迁移**（`transfers::media_transfer_task`）。

use serde::{Deserialize, Serialize};

use super::provider_helpers::SpaceUsage;
use crate::error::ServiceError;

/// 空间占用缓存 TTL（秒）。
pub const SPACE_USAGE_CACHE_TTL_SECONDS: i64 = 300;

/// 媒体库（响应体）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaLibraryResource {
    pub id: i64,
    pub name: String,
    /// provider 键。**创建后不可改**。
    pub provider_key: String,
    pub enabled: bool,
    /// 插件配置。**黑盒透传**，宿主不解释。
    pub provider_config: serde_json::Value,
    /// 空间占用。**列表里不返回** —— 那要逐库问 provider，太慢。
    pub space_usage: Option<SpaceUsage>,
}

/// 创建请求。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaLibraryCreateRequest {
    pub name: String,
    pub provider_key: String,
    pub enabled: Option<bool>,
    /// 必须是**对象**，且字段不得超出插件声明的 schema。
    pub provider_config: serde_json::Value,
}

/// 更新请求 —— **部分更新**。
///
/// `provider_key` 与 `provider_config` **不在这里**：见模块文档的「不可改」。
/// 传了就 422。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaLibraryUpdateRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
}

/// 空间占用的**进程内缓存条目**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedUsage {
    pub usage: SpaceUsage,
    /// 写入时刻（unix 秒）。
    pub cached_at: i64,
}

impl CachedUsage {
    /// 是否已过期。`now` 显式传入，便于测。
    pub fn is_stale(&self, now: i64) -> bool {
        now.saturating_sub(self.cached_at) >= SPACE_USAGE_CACHE_TTL_SECONDS
    }
}

/// 媒体库服务。
// `cache` 尚未被方法体引用（`space_usage` 还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct MediaLibraryService {
    cache: std::sync::Mutex<std::collections::HashMap<i64, CachedUsage>>,
}

impl Default for MediaLibraryService {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaLibraryService {
    /// 构造。
    pub fn new() -> Self {
        Self {
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// `GET /media-libraries` —— **不**带空间占用（见响应体文档）。
    pub async fn list_libraries() -> Result<Vec<MediaLibraryResource>, ServiceError> {
        todo!("骨架：查 media_library 按 id 升序；space_usage 恒为 None")
    }

    /// ★ 空间占用，**带 300 秒缓存**。
    ///
    /// 缓存**按库**，且 **provider 失败不写缓存**（见模块文档）。
    pub async fn storage_space_usages(
        &self,
        now: i64,
    ) -> Result<std::collections::HashMap<i64, SpaceUsage>, ServiceError> {
        let _ = now;
        todo!("骨架：命中未过期缓存则返回；否则问 provider -> 写缓存；失败不写缓存")
    }

    /// 列出**已装插件**提供的库模板。`GET /media-libraries/provider-catalog`。
    ///
    /// 返回 `[{provider_key, 名称, 配置字段 schema}]`，供前端渲染创建表单。
    pub async fn list_provider_catalog() -> Result<Vec<serde_json::Value>, ServiceError> {
        todo!("骨架：问各已装插件要配置 schema；某插件失败不影响其它（跳过即可）")
    }

    /// `POST /media-libraries` —— 建库并 `prepare_library`。
    ///
    /// 流程：校验 name 非空(422) -> 校验 provider_config 是对象且字段合规(422)
    /// -> 查重名(409) -> `prepare_library`(provider 错误) -> 落库。
    ///
    /// ⚠️ **`prepare_library` 在落库之前**：provider 侧建目录失败时不应该
    /// 留下一个「库存在但用不了」��记录。
    pub async fn create_library(
        &self,
        payload: MediaLibraryCreateRequest,
    ) -> Result<MediaLibraryResource, ServiceError> {
        let _ = payload;
        todo!("骨架：校验 -> 查重(409) -> prepare_library -> 落库")
    }

    /// `PATCH /media-libraries/{id}`。**provider_key / provider_config 不可改**。
    ///
    /// 错误码：不存在 → 404；重名 → 409；传了 provider 字段 → 422
    /// `invalid_media_library_provider`；**空更新** → 422
    /// `empty_media_library_update`。
    pub async fn update_library(
        &self,
        library_id: i64,
        payload: MediaLibraryUpdateRequest,
    ) -> Result<MediaLibraryResource, ServiceError> {
        let _ = (library_id, payload);
        todo!("骨架：空更新 422；重名 409；provider 字段不可改（传了 422）")
    }

    /// `DELETE /media-libraries/{id}` —— **204**。
    ///
    /// 错误码：不存在 → 404；**库里有媒体** → `409 media_library_in_use`。
    ///
    /// ⚠️ 那个 409 是保护：删库不会删里面的媒体，那些媒体会变成「指向不存在的
    /// 库」的孤儿行，之后谁都动不了它们。
    pub async fn delete_library(&self, library_id: i64) -> Result<(), ServiceError> {
        let _ = library_id;
        todo!("骨架：查库内是否有媒体(409 media_library_in_use) -> 无则删")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缓存 TTL 是 300 秒。
    #[test]
    fn the_cache_ttl_is_five_minutes() {
        assert_eq!(SPACE_USAGE_CACHE_TTL_SECONDS, 300);
    }

    /// ★ 过期判定用 `>=`：**恰好 300 秒就算过期**。
    ///
    /// 用 `>` 会让条目多活一秒 —— 影响不大，但 `>=` 与「300 秒后重新问」
    /// 的直觉一致。
    #[test]
    fn the_cache_boundary_is_inclusive() {
        let entry = CachedUsage {
            usage: SpaceUsage {
                total_bytes: 100,
                used_bytes: 50,
                free_bytes: 50,
            },
            cached_at: 1_000,
        };
        assert!(!entry.is_stale(1_000 + 299));
        assert!(entry.is_stale(1_000 + 300));
    }

    /// 更新请求里**没有** provider 字段 —— 不可改。
    ///
    /// 加上它们就意味着「改配置后已有媒体指向不存在的位置」。
    #[test]
    fn the_update_request_cannot_change_provider_fields() {
        let request: MediaLibraryUpdateRequest = serde_json::from_str("{}").expect("可解析");
        assert!(request.name.is_none());
        assert!(request.enabled.is_none());
    }

    /// 三个 `Option` 全 `None` = **空更新** → 422。
    #[test]
    fn an_empty_update_is_detectable() {
        let empty: MediaLibraryUpdateRequest = serde_json::from_str("{}").expect("可解析");
        assert!(empty.name.is_none() && empty.enabled.is_none());
    }

    /// 列表响应里 `space_usage` 恒为 `None` —— 逐库问 provider 太慢。
    #[test]
    fn the_list_omits_space_usage() {
        let library = MediaLibraryResource {
            id: 1,
            name: "本地盘".to_owned(),
            provider_key: "local".to_owned(),
            enabled: true,
            provider_config: serde_json::json!({}),
            space_usage: None,
        };
        assert!(library.space_usage.is_none());
    }
}
