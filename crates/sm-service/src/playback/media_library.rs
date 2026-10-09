//! 媒体库 CRUD（上游 `playback/media_library_service.py`，332 行）。
//!
//! # ★ 本轮照**上游原文**重做：骨架期的类型/文档有四处不符
//!
//! | 骨架期 | 上游（权威） | 为什么重要 |
//! |---|---|---|
//! | `MediaLibraryResource { enabled, space_usage }` | `schema/playback/media_libraries.py:24-31` `{ id, name, provider_key, provider_config, account_key, supports_in_place_import, created_at, updated_at }` | **`enabled` 库里没有这一列**；`space_usage` 不在响应里（容量走独立端点） |
//! | `CreateRequest { enabled? }` | `:34-36` `{ name, provider_key, provider_config={} }` | 同上：多一个不存在的列 |
//! | `UpdateRequest { name?, enabled? }` 且注明「`provider_config` 不可改」 | `:39-41` `{ name?, provider_config? }` | ★ **`provider_config` 恰恰是可改的**，而且是 `update_library` 的主干分支（带着 `previous` 重跑 `prepare_library`） |
//! | 缓存按 `library_id` | `:229-232` key = `{provider_key}:{account_key or "library:{id}"}` | 同一 115 账号可能挂**多个库**，按账号去重才能少一次远程查询 |
//!
//! 真正不可改的只有 **`provider_key`**：它决定 `provider_config` 的 schema，
//! 而 `storage_ref` 里的路径是按旧配置生成的。
//!
//! # 空间占用有 **300 秒缓存**
//!
//! [`SPACE_USAGE_CACHE_TTL_SECONDS`] = 300。`get_space_usage` 常常被前端
//! 仪表盘**轮询**，而它要向 provider 发请求（115 网盘那种尤其慢）。
//!
//! ⚠️ 缓存失败**不写**：provider 挂了不能缓存 300 秒，否则用户会盯着一个假的
//! 数据看五分钟。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use sm_db::repo::{
    DownloadClientRepository, MediaLibraryRepository, MediaRepository, NewMediaLibrary,
};
use sm_db::Db;

use super::provider_helpers::SpaceUsage;
use crate::error::{details_of, ServiceError};
use crate::transfers::download_client::ProviderFailureInfo;

/// 空间占用缓存 TTL（秒）。
pub const SPACE_USAGE_CACHE_TTL_SECONDS: i64 = 300;

/// 媒体库（响应体）。上游 `MediaLibraryResource`
/// （`schema/playback/media_libraries.py:24-31`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaLibraryResource {
    pub id: i32,
    pub name: String,
    /// provider 键。**创建后不可改**（改它等于让已有媒体指向不存在的位置）。
    pub provider_key: String,
    /// **剥掉 secret 之后**的插件配置。拿不到插件声明的字段表时是 `{}` ——
    /// 不知道哪些字段是 secret 时，原样发出去等于泄漏。
    pub provider_config: serde_json::Value,
    /// 多账号存储（115 等）用它区分 cookie 归属。**来自 `prepare_library` 的返回值**，
    /// 不是请求体里的字段。
    pub account_key: Option<String>,
    /// 该 provider 是否支持原地导入。** Inventory 页据此决定要不要给那个选项。**
    pub supports_in_place_import: bool,
    pub created_at: Option<chrono::NaiveDateTime>,
    pub updated_at: Option<chrono::NaiveDateTime>,
}

/// 创建请求。上游 `MediaLibraryCreateRequest`（`:34-36`）。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaLibraryCreateRequest {
    pub name: String,
    pub provider_key: String,
    #[serde(default)]
    pub provider_config: serde_json::Value,
}

/// 更新请求 —— **部分更新**。上游 `MediaLibraryUpdateRequest`（`:39-41`）。
///
/// ★ `provider_config` **可以改**（会重跑 `prepare_library` 并带上旧值）；
/// `provider_key` **不能**（本结构里根本没有它）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaLibraryUpdateRequest {
    pub name: Option<String>,
    pub provider_config: Option<serde_json::Value>,
}

/// provider 声明的一个配置项。宿主用它做白名单，**自己不维护这份表**。
///
/// 与下载器那份是同一个形状（见
/// [`crate::transfers::download_client::DownloadClientConfigField`]）——
/// 两边都来自 `MediaProviderConfigFieldResource`（`:10-19`）。刻意**没有**合并成一个
/// 类型：它们分属两个 seam，合并会让「下载能力」与「库能力」的边界变模糊。
#[derive(Debug, Clone)]
pub struct LibraryConfigField {
    pub key: String,
    /// `"secret"`：不出现在响应里，且更新时**从旧值回填**。
    pub input: String,
    pub read_only: bool,
}

/// `prepare_library` 的结果。上游 `:156-179`。
#[derive(Debug, Clone)]
pub struct PreparedLibrary {
    /// 归一化后的配置。**必须是对象**，否则 502 `provider_invalid_response`。
    pub provider_config: serde_json::Value,
    /// provider 派生的账号键，要**跟着落库**。
    pub account_key: Option<String>,
}

/// 传给 `prepare_library` 的「旧库」句柄。
#[derive(Debug, Clone)]
pub struct PreviousLibraryHandle {
    pub library_id: i32,
    pub provider_config: serde_json::Value,
}

/// 插件的**媒体库**能力。注入 seam —— 组合根实现，本模块不认识 gRPC。
pub trait MediaLibraryCapability: Send + Sync {
    fn library_config_fields(&self) -> Vec<LibraryConfigField>;
    fn prepare_library(
        &self,
        submitted: &serde_json::Value,
        previous: Option<&PreviousLibraryHandle>,
    ) -> Result<PreparedLibrary, ProviderFailureInfo>;
}

/// 插件目录里的一项。`GET /media-libraries/provider-catalog` 的输出，
/// 字段与上游 `list_provider_catalog`（`:240-259`）逐字对齐。
#[derive(Debug, Clone, Serialize)]
pub struct ProviderCatalogEntry {
    pub provider_key: String,
    pub display_name: String,
    pub library_config_fields: Vec<serde_json::Value>,
    pub playback_deliveries: Vec<String>,
    /// **没有下载能力时是 `null`**（不是空数组）—— 前端据此判断要不要显示
    /// 「下载器」那一节。这个 `Option` 的有无就是那个区别。
    pub download_config_fields: Option<Vec<serde_json::Value>>,
}

/// 媒体 provider 注册表。**组合根实现。**
pub trait MediaLibraryRegistry: Send + Sync {
    /// `Err` = 没安装 → 503 `provider_not_installed`；`Ok(None)` = 装了但**没有库能力**。
    fn library_for(
        &self,
        provider_key: &str,
    ) -> Result<Option<Box<dyn MediaLibraryCapability>>, ProviderFailureInfo>;
    /// 该 provider 是否支持原地导入。查不到按 `false` 处理（不阻断）。
    fn supports_in_place_import(&self, provider_key: &str) -> bool;
    /// 已装插件的目录。**逐个插件失败不影响其它**（由实现保证跳过坏的那个）。
    fn list_bundles(&self) -> Vec<ProviderCatalogEntry>;
    /// 问 provider 要容量。`None` = **不支持这个能力 / 查询失败** —— 上游把两者
    /// 都当「不在结果里」，从不让状态页整体失败。
    fn space_usage(
        &self,
        library_id: i32,
        provider_key: &str,
        provider_config: &serde_json::Value,
    ) -> Option<SpaceUsage>;
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
pub struct MediaLibraryService {
    db: Db,
    /// 插件注册表。`None` = 未注入，见
    /// [`Self::new_with_registry`] 上的说明。
    registry: Option<Arc<dyn MediaLibraryRegistry>>,
    /// ★ key 是 `{provider_key}:{account_key or "library:{id}"}`（**不是** library_id）
    /// —— 同一账号可能挂多个库，按账号去重才少一次远程查询。
    cache: Mutex<HashMap<String, CachedUsage>>,
}

impl MediaLibraryService {
    /// 构造（纯库能力）。
    pub fn new(db: &Db) -> Self {
        Self {
            db: db.clone(),
            registry: None,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 构造（带上插件注册表）。**写路径与容量必须由组合根用这个构造**：
    /// 未注入时 `create_library` 返回 503（宁可报错，也不落一份没校验过的配置）。
    pub fn new_with_registry(db: &Db, registry: Arc<dyn MediaLibraryRegistry>) -> Self {
        Self {
            db: db.clone(),
            registry: Some(registry),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn repo(&self) -> MediaLibraryRepository {
        MediaLibraryRepository::new(self.db.clone())
    }

    /// 404 `media_library_not_found` 的唯一出口。
    async fn require(
        &self,
        library_id: i32,
    ) -> Result<sm_db::playback::media::MediaLibrary, ServiceError> {
        self.repo().find_by_id(library_id).await?.ok_or_else(|| {
            ServiceError::not_found(
                "media_library_not_found",
                "Media library not found",
                "library_id",
                library_id,
            )
        })
    }

    fn bundle_for(
        &self,
        provider_key: &str,
    ) -> Result<Box<dyn MediaLibraryCapability>, ServiceError> {
        let Some(registry) = self.registry.as_deref() else {
            return Err(ServiceError::unavailable(
                "provider_not_installed",
                "媒体提供方未安装",
            ));
        };
        let capability = registry.library_for(provider_key).map_err(|failure| {
            ServiceError::unavailable(format!("provider_{}", failure.code), failure.message)
        })?;
        capability.ok_or_else(|| {
            ServiceError::validation_with(
                "provider_library_unsupported",
                "该 provider 未提供媒体库能力",
                details_of("provider_key", serde_json::Value::from(provider_key)),
            )
        })
    }

    /// 投影响应体 —— 与下载客户端同一个规则：**剥掉** `input == "secret"` 的字段，
    /// 拿不到字段表时整个 `provider_config` 发 `{}`（上游 `:70-88`）。
    fn resource_of(&self, library: &sm_db::playback::media::MediaLibrary) -> MediaLibraryResource {
        let raw = parse_provider_config(library.provider_config.as_deref());
        let supports_in_place_import = self
            .registry
            .as_deref()
            .is_some_and(|registry| registry.supports_in_place_import(&library.provider_key));
        let provider_config = self
            .registry
            .as_deref()
            .and_then(|registry| registry.library_for(&library.provider_key).ok().flatten())
            .map(|capability| {
                let secret_keys: std::collections::BTreeSet<String> = capability
                    .library_config_fields()
                    .into_iter()
                    .filter(|field| field.input == "secret")
                    .map(|field| field.key)
                    .collect();
                serde_json::Value::Object(
                    raw.as_object()
                        .map(|map| {
                            map.iter()
                                .filter(|(key, _)| !secret_keys.contains(*key))
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            })
            // ★ 拿不到字段表就发空对象：不知道哪些是 secret 时，原样发出去=泄漏。
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        MediaLibraryResource {
            id: library.id,
            name: library.name.clone(),
            provider_key: library.provider_key.clone(),
            provider_config,
            account_key: library.account_key.clone(),
            supports_in_place_import,
            created_at: library.created_at,
            updated_at: library.updated_at,
        }
    }

    /// `GET /media-libraries` —— 上游 `:182-186`：`created_at DESC, id DESC`。
    pub async fn list_libraries(&self) -> Result<Vec<MediaLibraryResource>, ServiceError> {
        let rows = self.repo().list_ordered().await?;
        Ok(rows.iter().map(|row| self.resource_of(row)).collect())
    }

    /// ★ 空间占用，**带 300 秒缓存**。上游 `storage_space_usages`（`:188-196`）。
    ///
    /// 不支持 / 失败的库**不出现在结果里** —— 那是「可选指标」，不能让状态页
    /// 整体失败。
    pub async fn storage_space_usages(
        &self,
        now: i64,
    ) -> Result<HashMap<i32, SpaceUsage>, ServiceError> {
        let rows = self.repo().list_ordered().await?;
        let mut usages = HashMap::new();
        for library in rows {
            let usage = self.space_usage_of(&library, now);
            if let Some(usage) = usage {
                usages.insert(library.id, usage);
            }
        }
        Ok(usages)
    }

    /// 单个库的容量：命中未过期缓存就用，否则问 provider。**失败不写缓存。**
    fn space_usage_of(
        &self,
        library: &sm_db::playback::media::MediaLibrary,
        now: i64,
    ) -> Option<SpaceUsage> {
        let registry = self.registry.as_deref()?;
        let key = space_cache_key(library);
        if let Some(entry) = self.cache.lock().ok()?.get(&key) {
            if !entry.is_stale(now) {
                return Some(entry.usage);
            }
        }
        let raw = parse_provider_config(library.provider_config.as_deref());
        let usage = registry.space_usage(library.id, &library.provider_key, &raw)?;
        let mut cache = self.cache.lock().ok()?;
        cache.insert(
            key,
            CachedUsage {
                usage,
                cached_at: now,
            },
        );
        Some(usage)
    }

    /// `GET /media-libraries/provider-catalog`。**纯插件目录**，没有注入就是空表
    /// （「没装插件」与「装了但一个都没注册」表现一致，上游亦然）。
    pub async fn list_provider_catalog(&self) -> Result<Vec<serde_json::Value>, ServiceError> {
        let Some(registry) = self.registry.as_deref() else {
            return Ok(Vec::new());
        };
        Ok(registry
            .list_bundles()
            .into_iter()
            .map(|entry| serde_json::to_value(entry).unwrap_or(serde_json::Value::Null))
            .collect())
    }

    /// `POST /media-libraries`。上游 `create_library`（`:262-280`）。
    ///
    /// ★ **`prepare_library` 在落库之前**：provider 侧建目录失败时不该留下
    /// 「库存在但用不了」的记录。
    pub async fn create_library(
        &self,
        payload: MediaLibraryCreateRequest,
    ) -> Result<MediaLibraryResource, ServiceError> {
        let name = crate::transfers::download_common::validate_non_empty(
            &payload.name,
            "invalid_media_library_name",
            "Media library name cannot be empty",
        )?;
        let provider_key = crate::transfers::download_common::validate_non_empty(
            &payload.provider_key,
            "invalid_media_library_provider",
            "provider_key cannot be empty",
        )?;
        let capability = self.bundle_for(&provider_key)?;
        let submitted = Self::submitted_config(&payload.provider_config)?;
        Self::validate_fields(capability.as_ref(), &submitted, false)?;
        let prepared = capability
            .prepare_library(&submitted, None)
            .map_err(|failure| self.provider_failure(&failure))?;
        let provider_config = Self::prepared_object(&prepared)?;
        self.ensure_name_available(&name, None).await?;
        let row = self
            .repo()
            .insert(&NewMediaLibrary {
                name,
                provider_key,
                provider_config: Some(provider_config.to_string()),
                account_key: prepared.account_key,
            })
            .await?;
        Ok(self.resource_of(&row))
    }

    /// `PATCH /media-libraries/{id}`。上游 `update_library`（`:282-311`）。
    ///
    /// ★ 两个 cache key 都要失效：改完配置可能连 `account_key` 都变了，
    /// 而缓存是按 `{provider_key}:{account_key}` 编的。
    pub async fn update_library(
        &self,
        library_id: i32,
        payload: MediaLibraryUpdateRequest,
    ) -> Result<MediaLibraryResource, ServiceError> {
        let library = self.require(library_id).await?;
        if payload.name.is_none() && payload.provider_config.is_none() {
            return Err(ServiceError::validation(
                "empty_media_library_update",
                "At least one field must be provided",
            ));
        }
        self.forget_space_usage(&library);

        if let Some(requested_name) = payload.name.as_deref() {
            let name = crate::transfers::download_common::validate_non_empty(
                requested_name,
                "invalid_media_library_name",
                "Media library name cannot be empty",
            )?;
            if name != library.name {
                self.ensure_name_available(&name, Some(library.id)).await?;
                self.repo().rename(library.id, &name).await?;
            }
        }
        if let Some(submitted) = payload.provider_config.as_ref() {
            let capability = self.bundle_for(&library.provider_key)?;
            let submitted = Self::submitted_config(submitted)?;
            Self::validate_fields(capability.as_ref(), &submitted, false)?;
            let previous = PreviousLibraryHandle {
                library_id: library.id,
                provider_config: parse_provider_config(library.provider_config.as_deref()),
            };
            let prepared = capability
                .prepare_library(&submitted, Some(&previous))
                .map_err(|failure| self.provider_failure(&failure))?;
            let provider_config = Self::prepared_object(&prepared)?;
            self.repo()
                .set_provider_config(library.id, &provider_config.to_string())
                .await?;
            self.repo()
                .set_account_key(library.id, prepared.account_key.as_deref())
                .await?;
        }
        let updated = self.require(library_id).await?;
        self.forget_space_usage(&updated);
        Ok(self.resource_of(&updated))
    }

    /// `DELETE /media-libraries/{id}` —— **204**。
    ///
    /// 上游 `delete_library`（`:313-328`）拦的是
    /// 「库里有媒体 **或** 有下载器客户端」 → `409 media_library_in_use`。
    /// 删库不会删里面的媒体，那些媒体会变成「指向不存在的库」的孤儿行。
    pub async fn delete_library(&self, library_id: i32) -> Result<(), ServiceError> {
        let library = self.require(library_id).await?;
        // ★ 上游（`:317-320`）拦两件事：库里有**媒体** *或* 有**下载器客户端**。
        // 只查 Media 会让「仅被下载器引用的库」被删掉，下载器随后指向不存在的库。
        let has_media = !MediaRepository::new(self.db.clone())
            // 只看「有没有」，取第一页 1 条即可。
            .list_by_library(library.id, sm_db::common::page::PageRequest::first_page(1)?)
            .await?
            .items
            .is_empty();
        let has_download_client = !DownloadClientRepository::new(self.db.clone())
            // 与 Media 那个一样是 `paged_list!` 生成的，同样要 page。
            .list_by_library(library.id, sm_db::common::page::PageRequest::first_page(1)?)
            .await?
            .items
            .is_empty();
        if has_media || has_download_client {
            self.forget_space_usage(&library);
            return Err(ServiceError::conflict(
                "media_library_in_use",
                "Media library is still referenced",
                Some(details_of(
                    "library_id",
                    serde_json::Value::from(library.id),
                )),
            ));
        }
        self.repo().delete(library.id).await?;
        self.forget_space_usage(&library);
        Ok(())
    }

    /// provider 这些年 failed → 服务错误。码跟着 provider 走（上游 `:160-172`）。
    fn provider_failure(&self, failure: &ProviderFailureInfo) -> ServiceError {
        let status = match failure.code.as_str() {
            "source_not_found" => 404,
            "authentication_failed" => 401,
            "unavailable" => 503,
            "invalid_config" | "unsupported" => 422,
            _ => 502,
        };
        ServiceError::from_status(
            status,
            format!("provider_{}", failure.code),
            failure.message.clone(),
        )
    }

    fn submitted_config(
        provider_config: &serde_json::Value,
    ) -> Result<serde_json::Value, ServiceError> {
        if !provider_config.is_object() {
            return Err(ServiceError::validation(
                "invalid_media_library_provider_config",
                "provider_config must be an object",
            ));
        }
        Ok(provider_config.clone())
    }

    /// `prepare_library` 的返回**必须是对象**，否则 502（上游 `:173-178`）。
    fn prepared_object(prepared: &PreparedLibrary) -> Result<serde_json::Value, ServiceError> {
        if !prepared.provider_config.is_object() {
            return Err(ServiceError::unavailable(
                "provider_invalid_response",
                "媒体提供方返回了无效配置",
            ));
        }
        Ok(prepared.provider_config.clone())
    }

    /// 未知字段 / 只读字段 → 422（上游 `_prepare_config`，`:121-139`）。
    fn validate_fields(
        capability: &dyn MediaLibraryCapability,
        submitted: &serde_json::Value,
        allow_read_only: bool,
    ) -> Result<(), ServiceError> {
        let Some(map) = submitted.as_object() else {
            return Err(ServiceError::validation(
                "invalid_media_library_provider_config",
                "provider_config must be an object",
            ));
        };
        let fields = capability.library_config_fields();
        let unknown: Vec<String> = map
            .keys()
            .filter(|key| !fields.iter().any(|field| field.key == **key))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(ServiceError::validation_with(
                "invalid_media_library_provider_config",
                "provider_config contains unknown fields",
                details_of("fields", serde_json::Value::from(unknown)),
            ));
        }
        if !allow_read_only {
            let read_only: Vec<String> = fields
                .iter()
                .filter(|field| field.read_only && map.contains_key(&field.key))
                .map(|field| field.key.clone())
                .collect();
            if !read_only.is_empty() {
                return Err(ServiceError::validation_with(
                    "invalid_media_library_provider_config",
                    "provider_config contains read-only fields",
                    details_of("fields", serde_json::Value::from(read_only)),
                ));
            }
        }
        Ok(())
    }

    async fn ensure_name_available(
        &self,
        name: &str,
        exclude_library_id: Option<i32>,
    ) -> Result<(), ServiceError> {
        let hit = self.repo().find_by_name(name).await?;
        let conflict = match (hit, exclude_library_id) {
            (Some(row), Some(exclude)) => row.id != exclude,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if conflict {
            return Err(ServiceError::conflict(
                "media_library_name_conflict",
                "Media library name already exists",
                Some(details_of("name", serde_json::Value::from(name))),
            ));
        }
        Ok(())
    }

    fn forget_space_usage(&self, library: &sm_db::playback::media::MediaLibrary) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(&space_cache_key(library));
        }
    }
}

/// `provider_config` 文本 → 对象。**NULL / 脏数据当空对象**（上游
/// `library.provider_config or {}`）。
fn parse_provider_config(raw: Option<&str>) -> serde_json::Value {
    raw.and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

/// 缓存键。`account_key` 优先 —— 一个账号下挂多个库时省一次远程查询。
fn space_cache_key(library: &sm_db::playback::media::MediaLibrary) -> String {
    match library.account_key.as_deref() {
        Some(account_key) if !account_key.is_empty() => {
            format!("{}:{}", library.provider_key, account_key)
        }
        _ => format!("{}:library:{}", library.provider_key, library.id),
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

    /// ★ 更新请求按上游是 `name` / `provider_config` —— **不是** `enabled`，
    /// 而且 `provider_config` **可以改**（要重跑 `prepare_library`）。
    #[test]
    fn the_update_request_matches_upstream_fields() {
        let empty: MediaLibraryUpdateRequest =
            serde_json::from_value(serde_json::json!({})).expect("空对象合法");
        assert!(empty.name.is_none());
        assert!(empty.provider_config.is_none(), "provider_config 可缺省");

        let full: MediaLibraryUpdateRequest = serde_json::from_value(serde_json::json!({
            "name": "115",
            "provider_config": {"root": "/mnt"}
        }))
        .expect("两个字段都合法");
        assert_eq!(full.name.as_deref(), Some("115"));
        assert!(full.provider_config.is_some());

        // `enabled` 不是上游字段 —— 传了也该被忽略（ Unknown key 默认忽略）。
        let with_enabled: MediaLibraryUpdateRequest =
            serde_json::from_value(serde_json::json!({"enabled": false})).expect("未知键被忽略");
        assert!(with_enabled.name.is_none());
    }

    /// ★ 响应体**没有** `enabled` / `space_usage`（库里没 `enabled` 那列，
    /// 容量走独立端点），但**有** `account_key` 与 `supports_in_place_import`。
    #[test]
    fn the_resource_wire_shape_has_no_invented_fields() {
        let resource = MediaLibraryResource {
            id: 1,
            name: "115".to_owned(),
            provider_key: "115".to_owned(),
            provider_config: serde_json::json!({}),
            account_key: Some("acct-1".to_owned()),
            supports_in_place_import: true,
            created_at: None,
            updated_at: None,
        };
        let json = serde_json::to_value(&resource).expect("序列化");
        assert_eq!(json["account_key"], "acct-1");
        assert_eq!(json["supports_in_place_import"], true);
        for absent in ["enabled", "space_usage"] {
            assert!(json.get(absent).is_none(), "{absent} 是骨架期自造的字段");
        }
    }

    /// ★ 缓存键按 **账号** 去重：同一账号挂两个库只问一次远程。
    #[test]
    fn the_cache_key_dedupes_by_account() {
        let library = sm_db::playback::media::MediaLibrary {
            id: 7,
            name: "115".to_owned(),
            provider_key: "115".to_owned(),
            provider_config: None,
            account_key: Some("acct-1".to_owned()),
            created_at: None,
            updated_at: None,
        };
        assert_eq!(space_cache_key(&library), "115:acct-1");

        let without_account = sm_db::playback::media::MediaLibrary {
            account_key: None,
            ..library.clone()
        };
        assert_eq!(space_cache_key(&without_account), "115:library:7");
    }

    /// 目录条目里 `download_config_fields` 是 **`null` 而不是空数组** ——
    /// 「没有下载能力」与「有但没字段」是两件事。
    #[test]
    fn a_bundle_without_downloads_reports_null_fields() {
        let entry = ProviderCatalogEntry {
            provider_key: "local".to_owned(),
            display_name: "本地盘".to_owned(),
            library_config_fields: Vec::new(),
            playback_deliveries: vec!["proxy".to_owned()],
            download_config_fields: None,
        };
        let json = serde_json::to_value(&entry).expect("序列化");
        assert!(json["download_config_fields"].is_null());
    }
}
