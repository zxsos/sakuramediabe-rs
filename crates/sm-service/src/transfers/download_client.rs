//! 下载器客户端配置（上游 `downloads/client_config_service.py`，351 行）。
//!
//! # 这里**只**做库侧那两件事，其余三条卡在 provider seam
//!
//! | 入口 | 上游 | 现在 |
//! |---|---|---|
//! | [`DownloadClientService::list_clients`] | `:257-265` | ✅ 已落地（纯库）|
//! | [`DownloadClientService::delete_client`] | `:333-351` | ✅ 已落地（纯库）|
//! | [`DownloadClientService::create_client`] | `:266-278` | ✅ 已落地（经注入的 [`DownloadCapabilityRegistry`]）|
//! | [`DownloadClientService::update_client`] | `:279-331` | ✅ 已落地 |
//! | [`DownloadClientService::test_client`] | `:199-256` | ✅ 已落地（连不上也是 200，`status="failed"`）|
//!
//! # ★ 响应体要剥掉 secret 字段（上游 `_resource`，`:72-91`）
//!
//! `provider_config` 里可能有密码 / 令牌。判据是**插件声明**的
//! `input == "secret"`：拿得到字段表才判得了，拿不到（插件没装 / 没有下载
//! 能力）时上游直接发 `{}` —— 不知道哪些是 secret 的时候，原样发出去就等于泄漏。
//!
//! # 为什么 `create` / `update` 不能「先落库、以后补校验」
//!
//! 上游那两条都要先 `_bundle(library)`（注册表里取 provider bundle）再做两件事：
//!
//! 1. `_validate_config`（`:94-128`）拿**插件声明**的 `config_fields` 判「未知字段 /
//!    只读字段」—— 宿主**没有**那份表；
//! 2. `_prepare`（`:144-172`）直接调 `bundle.downloads.prepare_client(...)`
//!    （合并 secret、派生/归一化配置），失败按 `provider_error` 映射。
//!
//! 宿主自己编一份字段表就是**把插件 schema 抄进宿主** —— 插件升版新增配置项时，
//! 老宿主会把新字段拒掉，那个插件就完全不能用（上游注释里说的「向前兼容」正是
//! 要防这个）。所以宁可留 `todo!()` + 上游行号，也不做半截实现。
//!
//! # ★ 形状按上游 `schema/transfers/downloads.py` 逐字对齐
//!
//! ⚠️ 骨架期这三种类型都是自造的，差异如下（都已按上游改回）：
//!
//! | 骨架期 | 上游 | 为什么 |
//! |---|---|---|
//! | `Resource { id, name, kind, enabled, config }` | `:14-33` `{ id, name, library_id, provider_config, created_at, updated_at }` | `kind` / `enabled` **库里没有这两列**；`config` 的真名是 `provider_config` |
//! | `CreateRequest { name, kind, config }` | `:35-38` `{ name, library_id(>0), provider_config(默认 `{}`) }` | 同上；`library_id` 是必填且必须为正 |
//! | `UpdateRequest { name, enabled, config }`（注释还写着「`kind` 刻意不提供，不可改」）| `:41-44` `{ name?, library_id?, provider_config? }` | 上游**根本没有 `kind` 字段**，也不存在 `download_client_library_change_forbidden` 这个码 —— 那张「kind 不可改」的表是骨架期编的 |
//! | `TestRequest { kind, config, library_id? }` | `:47-50` `{ library_id(>0), provider_config(默认 `{}`), client_id?(>0) }` | 探测要的是「库 + 配置（+ 可选已有客户端）」，不是「种类」 |
//!
//! # ★ 删除的两道 409：**先任务、后绑定**
//!
//! | 顺序 | 检查 | 码 |
//! |---|---|---|
//! | 1 | 该客户端名下**有没有任务行**（含历史） | `409 download_client_in_use` |
//! | 2 | 有没有索引器还绑着它 | `409 download_client_in_use_by_indexers` |
//!
//! ⚠️ 骨架期的文档把顺序写反了（「先查绑定再查任务」），实际上游是
//! `:335` 查任务、`:342` 查绑定。两道的**判据也不同**：第一道是「有没有任何任务」
//! （不是「有没有在跑的」），因为任务行会随下载器 `CASCADE` 删掉 —— 拦的是
//! 「你的下载历史会一起没」，不是「任务还在跑」。
//!
//! # `provider_config` 是**透传的黑盒**，但白名单校验在插件侧
//!
//! 宿主不解释里面有哪些字段，但「未知字段 / 只读字段」的判据来自**插件声明的
//! schema**（`_validate_config`），所以这条校验只能跟 provider seam 一起做。

use serde::{Deserialize, Serialize};

use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
    MediaLibraryRepository,
};
use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::transfers::download_common::{require_client, require_library};

/// 客户端配置（响应体）。上游 `DownloadClientResource`
/// （`schema/transfers/downloads.py:14-33`）。
///
/// `created_at` / `updated_at` 在实体里是 `Option`（迁移期可能有 NULL），
/// 上游模型继承 `TimestampedMixin` 所以是必填 —— 差异只在历史数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientResource {
    pub id: i32,
    pub name: String,
    /// 归属库。`library_id` **必填且 > 0**（上游 `:38`）。
    pub library_id: i32,
    /// 插件配置。**原样透传** —— 字段由插件解释。
    pub provider_config: serde_json::Value,
    pub created_at: Option<chrono::NaiveDateTime>,
    pub updated_at: Option<chrono::NaiveDateTime>,
}

/// 创建请求。上游 `DownloadClientCreateRequest`（`:35-38`）。
///
/// `provider_config` 缺省是空对象；`library_id` 上游带 `gt=0` 校验。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientCreateRequest {
    pub name: String,
    pub library_id: i32,
    #[serde(default)]
    pub provider_config: serde_json::Value,
}

/// 更新请求 —— **部分更新**。上游 `DownloadClientUpdateRequest`（`:41-44`）。
///
/// ⚠️ 字段全 `Option` 带来 serde 的固有局限：`None` **不区分**「没传这个键」
/// 与「显式传了 `null`」。上游 FastAPI 用的是同一套 `exclude_unset` 语义，
/// 所以这不是移植引入的偏差。要真正区分得用 `Option<Option<T>>` + 自定义
/// 反序列化，上游没做，**照抄**。
///
/// 三个字段都可以改（`library_id` 也在内）—— 骨架期那条「`kind` 不可改」的规则
/// 上游并不存在，因为**上游没有 `kind` 字段**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DownloadClientUpdateRequest {
    pub name: Option<String>,
    pub library_id: Option<i32>,
    pub provider_config: Option<serde_json::Value>,
}

/// 探测请求。**不落库**。上游 `DownloadClientTestRequest`（`:47-50`）。
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadClientTestRequest {
    /// 用哪个库的 provider 来探测。**必填且 > 0**。
    pub library_id: i32,
    #[serde(default)]
    pub provider_config: serde_json::Value,
    /// 要测的是**已存在**的客户端（会带上库里存的配置）。
    pub client_id: Option<i32>,
}

/// 诊断中的一项检查。上游 `DownloadClientDiagnosticCheckResource`（`:53-58`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientDiagnosticCheck {
    pub key: String,
    /// `ok` / `warning` / `failed` / `skipped`。
    pub status: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// 探测诊断结果。上游 `DownloadClientDiagnosticResource`（`:60-64`）。
///
/// ⚠️ 骨架期这里是 `{ reachable, latency_ms, version, error }` —— **自造的四项**。
/// 上游给的是 `{ status, checks[], checked_at, elapsed_ms }`：一次探测会跑多项
/// 检查，每项各有自己的码与文案，客户端按 `checks[].status` 显示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadClientDiagnostic {
    /// `ok` / `warning` / `failed`。
    pub status: String,
    pub checks: Vec<DownloadClientDiagnosticCheck>,
    pub checked_at: chrono::NaiveDateTime,
    pub elapsed_ms: i64,
}

/// 插件声明的一个配置项。宿主用它做白名单，**自己不维护这份表**。
///
/// 上游 `bundle.downloads.config_fields` 的元素（`client_config_service.py:107`）。
/// ⚠️ 别在宿主里抄一份字段表：插件升版新增配置项时，老宿主会把新字段判成
/// 「未知字段」而拒掉整个请求 —— 那个插件就完全不能用了。
#[derive(Debug, Clone)]
pub struct DownloadClientConfigField {
    pub key: String,
    /// 输入种类。`"secret"` 有两个后果：不出现在响应里（`_resource`），
    /// 更新时**从旧值回填**（`_prepare`）。
    pub input: String,
    /// 只读字段：只能由 provider 派生，**用户提交不得包含**（除非允许）。
    pub read_only: bool,
}

/// 已有的那个客户端 —— `prepare_client` 要它来合并 secret / 只读字段。
///
/// 上游 `DownloadClientHandle`（`:157-161`）。
#[derive(Debug, Clone)]
pub struct PreviousClientHandle {
    pub client_id: i32,
    pub library_id: i32,
    pub provider_config: serde_json::Value,
}

/// provider 侧的失败。**宿主据此构造失败项，不是把它吞掉。**
#[derive(Debug, Clone)]
pub struct ProviderFailureInfo {
    /// `invalid_config` / `authentication_failed` … 与 `ProviderOperationError.code` 同源。
    pub code: String,
    pub message: String,
}

/// 插件的下载器能力。**注入 seam** —— 组合根（sm-server）实现它，
/// 本模块不认识 gRPC。
pub trait DownloadClientCapability: Send + Sync {
    /// 插件声明的配置字段表。白名单 / secret / 只读的**唯一**判据来源。
    fn config_fields(&self) -> Vec<DownloadClientConfigField>;
    /// 合并 secret、派生并归一化配置。**返回必须能当配置对象用的东西。**
    fn prepare_client(
        &self,
        submitted: &serde_json::Value,
        library_id: i32,
        previous: Option<&PreviousClientHandle>,
    ) -> Result<serde_json::Value, ProviderFailureInfo>;
    /// 探测。**失败由宿主转成失败诊断**（HTTP 仍是 200）。
    fn test_client(
        &self,
        submitted: &serde_json::Value,
        library_id: i32,
    ) -> Result<DownloadClientDiagnostic, ProviderFailureInfo>;
}

/// 按 `provider_key` 取下载能力。**未注入 → 无能力**（`Ok(None)`）。
pub trait DownloadCapabilityRegistry: Send + Sync {
    /// `Err` = provider **没安装** → 503 `provider_not_installed`；
    /// `Ok(None)` = 装了但没下载能力 → 422 `provider_download_unsupported`。
    fn download_client_for(
        &self,
        provider_key: &str,
    ) -> Result<Option<Box<dyn DownloadClientCapability>>, ProviderFailureInfo>;
}

/// 下载器客户端服务（**库侧**）。
///
/// # 为什么要有状态
///
/// 骨架期这三个函数是**无状态的自由 `async fn`**（连 `Db` 都没有）—— 落不了地。
/// `list_clients` / `delete_client` 只查库，所以现在持 `Db` 就够；阶段二注入
/// provider seam 时再加一个网关字段（与 `playback::media::MediaService` 同款）。
pub struct DownloadClientService {
    db: Db,
    /// 插件能力注册表。`None` = 未注入 —— 那时三个写方法 RETURN **503**，
    /// 而不是假装配置合法。
    downloads: Option<std::sync::Arc<dyn DownloadCapabilityRegistry>>,
}

impl DownloadClientService {
    /// 构造（库侧能力）。`list_clients` / `delete_client` 只查库，这样就够。
    pub fn new(db: &Db) -> Self {
        Self {
            db: db.clone(),
            downloads: None,
        }
    }

    /// 构造（带上插件能力）。**写方法必须由组合根用这个构造**，
    /// 否则三个写方法会一律返回 503（刻意如此：宁可报错，也不放行未校验的配置）。
    pub fn new_with_downloads(
        db: &Db,
        downloads: std::sync::Arc<dyn DownloadCapabilityRegistry>,
    ) -> Self {
        Self {
            db: db.clone(),
            downloads: Some(downloads),
        }
    }

    /// 取某个库的下载能力。上游 `_bundle(library)`（`:52-70`）。
    fn bundle_for(
        &self,
        provider_key: &str,
    ) -> Result<Box<dyn DownloadClientCapability>, ServiceError> {
        let Some(registry) = self.downloads.as_deref() else {
            return Err(ServiceError::unavailable(
                "provider_not_installed",
                "媒体提供方未安装",
            ));
        };
        let capability =
            crate::transfers::download_common::download_provider(registry, provider_key)?;
        capability.ok_or_else(|| {
            ServiceError::validation_with(
                "provider_download_unsupported",
                "该媒体库未提供下载能力",
                details_of("provider_key", serde_json::Value::from(provider_key)),
            )
        })
    }

    /// 投影成响应体 —— **剥掉 `input == "secret"` 的字段**。
    ///
    /// 上游 `_resource`（`:72-91`）：provider 不可用或没有下载能力时把
    /// `provider_config` 整个换成 `{}`。这是**数据最小化**，不是「省略卫语句」——
    /// 不知道哪些字段是 secret 时，把原始配置原样发出去等于泄漏凭据。
    async fn resource_of(
        &self,
        client: &sm_db::DownloadClient,
    ) -> Result<DownloadClientResource, ServiceError> {
        let raw = crate::transfers::download_common::provider_config_object(
            client.provider_config.as_deref(),
        );
        let Ok(library) = MediaLibraryRepository::new(self.db.clone())
            .find_by_id(client.library_id)
            .await
        else {
            return Ok(Self::masked(
                client,
                serde_json::Value::Object(Default::default()),
            ));
        };
        let Some(library) = library else {
            return Ok(Self::masked(
                client,
                serde_json::Value::Object(Default::default()),
            ));
        };
        let Ok(capability) = self.bundle_for(&library.provider_key) else {
            return Ok(Self::masked(
                client,
                serde_json::Value::Object(Default::default()),
            ));
        };
        let secret_keys: std::collections::BTreeSet<String> = capability
            .config_fields()
            .into_iter()
            .filter(|field| field.input == "secret")
            .map(|field| field.key)
            .collect();
        let visible: serde_json::Map<String, serde_json::Value> = raw
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(key, _)| !secret_keys.contains(*key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self::masked(client, serde_json::Value::Object(visible)))
    }

    fn masked(
        client: &sm_db::DownloadClient,
        provider_config: serde_json::Value,
    ) -> DownloadClientResource {
        DownloadClientResource {
            id: client.id,
            name: client.name.clone(),
            library_id: client.library_id,
            provider_config,
            created_at: client.created_at,
            updated_at: client.updated_at,
        }
    }

    /// `# ★ 白名单校验：判据来自插件，不是宿主`
    ///
    /// 上游 `_validate_config`（`:93-128`）：不是对象 → 422；未知字段 → 422
    /// （`details.fields`）；只读字段 → 422。
    /// `allow_read_only` 只在「用户**没有**提交 `provider_config`、只是改了别的
    /// 字段而需要把旧配置带上去略一遍」时才放行（`:324`）。
    fn validate_config(
        capability: &dyn DownloadClientCapability,
        submitted: &serde_json::Value,
        allow_read_only: bool,
    ) -> Result<serde_json::Value, ServiceError> {
        let Some(map) = submitted.as_object() else {
            return Err(ServiceError::validation(
                "invalid_download_client_provider_config",
                "provider_config must be an object",
            ));
        };
        let fields = capability.config_fields();
        let known: std::collections::BTreeSet<String> =
            fields.iter().map(|field| field.key.clone()).collect();
        let unknown: Vec<String> = map
            .keys()
            .filter(|key| !known.contains(*key))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Err(ServiceError::validation_with(
                "invalid_download_client_provider_config",
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
                    "invalid_download_client_provider_config",
                    "provider_config contains read-only fields",
                    details_of("fields", serde_json::Value::from(read_only)),
                ));
            }
        }
        Ok(submitted.clone())
    }

    /// 名字可用性。上游 `_ensure_name_available`（`:130-141`）→ **409**，
    /// `details.name`。排除自己（`exclude_client_id`）用于更新。
    async fn ensure_name_available(
        &self,
        name: &str,
        exclude_client_id: Option<i32>,
    ) -> Result<(), ServiceError> {
        let hit = DownloadClientRepository::new(self.db.clone())
            .find_by_name(name)
            .await?;
        let conflict = match (hit, exclude_client_id) {
            (Some(client), Some(exclude)) => client.id != exclude,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if conflict {
            return Err(ServiceError::conflict(
                "download_client_name_conflict",
                "Download client name already exists",
                Some(details_of("name", serde_json::Value::from(name))),
            ));
        }
        Ok(())
    }

    /// `GET /download-clients` —— **裸数组**（上游没有分页信封）。
    ///
    /// 上游 `list_clients`（`:257-265`）：`created_at DESC, id DESC`，最新在前。
    pub async fn list_clients(&self) -> Result<Vec<DownloadClientResource>, ServiceError> {
        let rows = DownloadClientRepository::new(self.db.clone())
            .list_ordered()
            .await?;
        let mut resources = Vec::with_capacity(rows.len());
        for row in rows {
            resources.push(self.resource_of(&row).await?);
        }
        Ok(resources)
    }

    /// `DELETE /download-clients/{client_id}` —— **204**，无 body。
    ///
    /// 上游 `delete_client`（`:333-351`）。两道 409 **有先后**：
    ///
    /// 1. 有任务行（**含历史**）→ `download_client_in_use` + details `{client_id}`；
    /// 2. 有索引器绑定 → `download_client_in_use_by_indexers` + details `{client_id}`。
    ///
    /// 顺序照抄会更好用：先报「有任务」时用户删掉任务，再删会看到「被绑定」——
    /// 两步都能修完。反过来（骨架期文档写的那种顺序）也能修完，但与上游的
    /// **错误码**就不一致了，客户端按码分流的提示会错位。
    ///
    /// ⚠️ 删除会 `CASCADE` 掉该客户端的**全部任务历史**与索引器绑定，
    /// 所以这两道检查拦的正是「你确定要连历史一起删吗」。
    pub async fn delete_client(&self, client_id: i32) -> Result<(), ServiceError> {
        let client = require_client(&self.db, client_id).await?;

        if DownloadTaskRepository::new(self.db.clone())
            .exists_for_client(client.id)
            .await?
        {
            return Err(ServiceError::conflict(
                "download_client_in_use",
                "Download client is still referenced by download tasks",
                Some(details_of("client_id", serde_json::Value::from(client.id))),
            ));
        }
        if IndexerDownloadClientRepository::new(self.db.clone())
            .exists_for_client(client.id)
            .await?
        {
            return Err(ServiceError::conflict(
                "download_client_in_use_by_indexers",
                "Download client is still referenced by indexers",
                Some(details_of("client_id", serde_json::Value::from(client.id))),
            ));
        }

        DownloadClientRepository::new(self.db.clone())
            .delete(client.id)
            .await?;
        Ok(())
    }

    /// `POST /download-clients` —— **201**。**阶段二**（要 provider seam）。
    pub async fn create_client(
        &self,
        payload: DownloadClientCreateRequest,
    ) -> Result<DownloadClientResource, ServiceError> {
        let name = crate::transfers::download_common::validate_non_empty(
            &payload.name,
            "invalid_download_client_name",
            "Download client name cannot be empty",
        )?;
        let library = require_library(&self.db, payload.library_id).await?;
        let capability = self.bundle_for(&library.provider_key)?;
        // 顺序照上游：先 `_bundle` → `_validate_config` → `_prepare` →
        // **名字查重** → 落库。名字冲突放在最后是为了让「插件没装 /
        // 配置不合法」先报错（那些用户改不了请求体也解决不了）。
        let prepared = Self::validate_config(capability.as_ref(), &payload.provider_config, false)
            .and_then(|submitted| {
                capability
                    .prepare_client(&submitted, library.id, None)
                    .map_err(provider_config_failed)
            })?;
        self.ensure_name_available(&name, None).await?;
        let row = DownloadClientRepository::new(self.db.clone())
            .insert(&sm_db::repo::NewDownloadClient {
                name,
                library_id: library.id,
                provider_config: Some(prepared.to_string()),
            })
            .await?;
        self.resource_of(&row).await
    }

    /// `PATCH /download-clients/{client_id}` —— 不存在 → **404**。**阶段二**。
    ///
    /// 顺序照上游 `:279-331`：改名查重 → **改库要先判有没有任务** → 再动配置。
    ///
    /// ⚠️ 骨架期那条「`kind` 不可改」的规则上游并**不存在**（上游没有 `kind`）；
    /// 真正被禁的是「**有任务时不能换库**」，码是
    /// `409 download_client_library_change_forbidden`（`:297-302`）。
    pub async fn update_client(
        &self,
        client_id: i32,
        payload: DownloadClientUpdateRequest,
    ) -> Result<DownloadClientResource, ServiceError> {
        let client = require_client(&self.db, client_id).await?;
        if payload.name.is_none()
            && payload.library_id.is_none()
            && payload.provider_config.is_none()
        {
            return Err(ServiceError::validation(
                "empty_download_client_update",
                "At least one field must be provided",
            ));
        }
        if let Some(requested) = payload.name.as_deref() {
            let name = crate::transfers::download_common::validate_non_empty(
                requested,
                "invalid_download_client_name",
                "Download client name cannot be empty",
            )?;
            if name != client.name {
                self.ensure_name_available(&name, Some(client.id)).await?;
                DownloadClientRepository::new(self.db.clone())
                    .rename(client.id, &name)
                    .await?;
            }
        }
        let mut target_library_id = client.library_id;
        if let Some(requested_library_id) = payload.library_id {
            if requested_library_id != client.library_id {
                if DownloadTaskRepository::new(self.db.clone())
                    .exists_for_client(client.id)
                    .await?
                {
                    return Err(ServiceError::conflict(
                        "download_client_library_change_forbidden",
                        "Download client library cannot change while tasks exist",
                        Some(details_of("client_id", serde_json::Value::from(client.id))),
                    ));
                }
                let _ = require_library(&self.db, requested_library_id).await?;
                target_library_id = requested_library_id;
            }
        }
        // ★ 只有动了 `library_id` 或 `provider_config` 才重跑 `_prepare`
        // —— 改个名字不该触发一次 provider 往返。
        if payload.library_id.is_some() || payload.provider_config.is_some() {
            let Some(submitted) = payload.provider_config.as_ref() else {
                return Err(ServiceError::validation(
                    "invalid_download_client_provider_config",
                    "provider_config must be an object",
                ));
            };
            if !submitted.is_object() {
                return Err(ServiceError::validation(
                    "invalid_download_client_provider_config",
                    "provider_config must be an object",
                ));
            }
            let library = require_library(&self.db, target_library_id).await?;
            let capability = self.bundle_for(&library.provider_key)?;
            // 没传 `provider_config` 只是改别的字段 → 用旧配置过一遍，
            // 此时**允许**只读字段（它们本来就躺在库里）。
            let config_submitted = payload.provider_config.is_some();
            let submitted_value = payload.provider_config.clone().unwrap_or_else(|| {
                crate::transfers::download_common::provider_config_object(
                    client.provider_config.as_str(),
                )
            });
            // 用户提交了配置 → 只读字段不许出现；只是改别的字段 → 放行
            // （库里本来就有那些派生值）。
            let submitted =
                Self::validate_config(capability.as_ref(), &submitted_value, !config_submitted)?;
            let previous = PreviousClientHandle {
                client_id: client.id,
                library_id: client.library_id,
                provider_config: submitted.clone(),
            };
            let prepared = capability
                .prepare_client(&submitted, library.id, Some(&previous))
                .map_err(provider_config_failed)?;
            let repo = DownloadClientRepository::new(self.db.clone());
            repo.set_provider_config(client.id, &prepared.to_string())
                .await?;
            if target_library_id != client.library_id {
                repo.move_to_library(client.id, target_library_id).await?;
            }
        }
        let row = DownloadClientRepository::new(self.db.clone())
            .find_by_id(client_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "download_client_not_found",
                    "Download client not found",
                    "client_id",
                    client_id,
                )
            })?;
        self.resource_of(&row).await
    }

    /// `POST /download-clients/test` —— **无副作用**，成功与失败都 200。**阶段二**。
    ///
    /// ★ 失败**不返回 `Err`**：连不上也要 200，只不过 `status = "failed"`
    /// 且 `checks[]` 里带着原因 —— 这本来就是「诊断」接口。
    pub async fn test_client(
        &self,
        payload: DownloadClientTestRequest,
    ) -> Result<DownloadClientDiagnostic, ServiceError> {
        let started = std::time::Instant::now();
        let existing = match payload.client_id {
            Some(client_id) => Some(require_client(&self.db, client_id).await?),
            None => None,
        };
        let library = require_library(&self.db, payload.library_id).await?;
        // ⚠️ 已存在的客户端只能用它**自己所属**的库来测 —— 否则插件会拿着
        // A 库的凭据去连 B 库的下载器。
        if let Some(client) = existing.as_ref() {
            if client.library_id != library.id {
                return Err(ServiceError::validation_with(
                    "download_client_test_library_mismatch",
                    "下载器测试必须使用当前下载器绑定的媒体库",
                    details_of("client_id", serde_json::Value::from(client.id)),
                ));
            }
        }
        let capability = self.bundle_for(&library.provider_key)?;
        // 没给配置就用库里存的那份（上游 `:214-215`）。
        let submitted_value = if payload.provider_config.is_object()
            && payload
                .provider_config
                .as_object()
                .is_some_and(|map| !map.is_empty())
        {
            payload.provider_config.clone()
        } else {
            crate::transfers::download_common::provider_config_object(
                existing
                    .as_ref()
                    .and_then(|client| client.provider_config.as_str()),
            )
        };
        let submitted = Self::validate_config(capability.as_ref(), &submitted_value, false)?;
        let previous = existing.as_ref().map(|client| PreviousClientHandle {
            client_id: client.id,
            library_id: client.library_id,
            provider_config: submitted.clone(),
        });
        let prepared = match capability.prepare_client(&submitted, library.id, previous.as_ref()) {
            Ok(prepared) => prepared,
            Err(failure) => return Ok(failed_diagnostic(&failure.code, &failure.message, started)),
        };
        // ★ provider 的失败（含连接不上）在**这里**转成失败诊断。
        let diagnostic = match capability.test_client(&prepared, library.id) {
            Ok(diagnostic) => diagnostic,
            Err(failure) => failed_diagnostic(&failure.code, &failure.message, started),
        };
        let _ = started;
        Ok(diagnostic)
    }
}

/// provider 失败 → 失败诊断。**HTTP 仍是 200**。
fn failed_diagnostic(
    code: &str,
    message: &str,
    started: std::time::Instant,
) -> DownloadClientDiagnostic {
    DownloadClientDiagnostic {
        status: "failed".to_owned(),
        checks: vec![DownloadClientDiagnosticCheck {
            key: "provider".to_owned(),
            status: "failed".to_owned(),
            code: code.to_owned(),
            message: message.to_owned(),
            details: None,
        }],
        checked_at: chrono::Utc::now().naive_utc(),
        elapsed_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    }
}

/// provider 的 `prepare_client` 失败 → 服务错误。**配置不合法不能放行**：
/// 落到这里说明插件拒绝了这份配置（错误码跟着 provider 走）。
fn provider_config_failed(failure: ProviderFailureInfo) -> ServiceError {
    ServiceError::unavailable(format!("provider_{}", failure.code), failure.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 更新请求按上游三个字段反序列化；`{}` → 三个 `None`（「空更新」的判据）。
    ///
    /// ⚠️ 骨架期这里断言的是 `enabled` / `config` —— 那两个字段上游没有。
    #[test]
    fn the_update_request_matches_upstream_fields() {
        let full: DownloadClientUpdateRequest = serde_json::from_value(serde_json::json!({
            "name": "qb",
            "library_id": 7,
            "provider_config": {"host": "127.0.0.1"}
        }))
        .expect("三个字段都合法");
        assert_eq!(full.name.as_deref(), Some("qb"));
        assert_eq!(full.library_id, Some(7));
        assert!(full.provider_config.is_some());

        // 空对象 = 空更新（由 `update_client` 报 422 `empty_download_client_update`）。
        let empty: DownloadClientUpdateRequest =
            serde_json::from_value(serde_json::json!({})).expect("空对象合法");
        assert!(empty.name.is_none() && empty.library_id.is_none());
        assert!(empty.provider_config.is_none());

        // `kind` 不是上游字段 —— 传了也该被忽略，而不是当成未知字段报错
        // （serde 默认忽略未知键；这条用例把它钉住）。
        let with_kind: DownloadClientUpdateRequest =
            serde_json::from_value(serde_json::json!({"kind": "qbittorrent"}))
                .expect("未知键被忽略");
        assert!(with_kind.name.is_none());
    }

    /// ★ 创建请求：`library_id` 必填、`provider_config` 可缺省。
    #[test]
    fn the_create_request_matches_upstream() {
        let minimal: DownloadClientCreateRequest =
            serde_json::from_value(serde_json::json!({"name": "qb", "library_id": 3}))
                .expect("缺 provider_config 应合法");
        assert_eq!(minimal.library_id, 3);
        assert_eq!(
            minimal.provider_config,
            serde_json::json!(null),
            "serde 的缺省是 Null；上游是 `{{}}`，由服务层归一"
        );

        assert!(
            serde_json::from_value::<DownloadClientCreateRequest>(
                serde_json::json!({"name": "qb"})
            )
            .is_err(),
            "library_id 必填"
        );
    }

    /// ★ 响应体的**线上字段名**照上游：`library_id` / `provider_config`，
    /// 且**没有** `kind` / `enabled` / `config`。
    #[test]
    fn the_resource_wire_shape_has_no_invented_fields() {
        let resource = DownloadClientResource {
            id: 1,
            name: "qb".to_owned(),
            library_id: 2,
            provider_config: serde_json::json!({"host": "h"}),
            created_at: None,
            updated_at: None,
        };
        let json = serde_json::to_value(&resource).expect("序列化");
        assert_eq!(json["library_id"], 2);
        assert_eq!(json["provider_config"]["host"], "h");
        for absent in ["kind", "enabled", "config"] {
            assert!(json.get(absent).is_none(), "{absent} 不该出现在响应里");
        }
    }

    /// ★ 诊断结果的形状照上游：一次探测多项 `checks`。
    #[test]
    fn the_diagnostic_shape_is_the_upstream_one() {
        let diagnostic = DownloadClientDiagnostic {
            status: "warning".to_owned(),
            checks: vec![DownloadClientDiagnosticCheck {
                key: "connection".to_owned(),
                status: "ok".to_owned(),
                code: "ok".to_owned(),
                message: "reachable".to_owned(),
                details: None,
            }],
            checked_at: chrono::NaiveDate::from_ymd_opt(2026, 10, 6)
                .expect("日期")
                .and_hms_opt(0, 0, 0)
                .expect("时刻"),
            elapsed_ms: 12,
        };
        let json = serde_json::to_value(&diagnostic).expect("序列化");
        assert_eq!(json["status"], "warning");
        assert_eq!(json["checks"][0]["key"], "connection");
        assert!(json["checks"][0].get("details").is_none(), "None 不序列化");
        for absent in ["reachable", "latency_ms", "version", "error"] {
            assert!(json.get(absent).is_none(), "{absent} 是骨架期自造的字段");
        }
    }
}
