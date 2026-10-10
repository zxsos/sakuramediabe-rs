//! API 密钥的列表 / 生成 / 删除，对应上游 `src/service/system/api_key_service.py`（55 行）。
//!
//! # 这一层只做三件事
//!
//! | 方法 | 上游 | 规则 |
//! |---|---|---|
//! | [`ApiKeyService::list`] | `list_api_keys` | 纯投影，排序交给仓储（`created_at DESC, id DESC`） |
//! | [`ApiKeyService::create`] | `create_api_key` | 生成明文 → 落哈希与 `key_hint` → **明文只在返回值里出现一次** |
//! | [`ApiKeyService::delete`] | `delete_api_key` | 删不到 → 404 `api_key_not_found` |
//!
//! # 鉴权**不在这里**
//!
//! 「Bearer `sk-...` → 按 sha256 查库 → 取单账号用户」是**请求期**的行为，
//! 落在 `sm_api::auth::CurrentUser` 的提取器里（`via_api_key`）。本 service
//! 只管密钥的**生命周期**。两处共用 [`sm_core::api_key`] 的前缀与哈希定义。
//!
//! # 明文为什么由 service 返回而不是写进实体
//!
//! [`ApiKey`] 实体对应数据库行，而行里**没有**明文字段。让 `create` 返回
//! `(行, 明文)` 这个二元组，调用方（路由层）就会**显式**决定把它放进创建响应；
//! 若把明文塞进实体，它就有机会被列表响应的投影顺手带出去。
//!
//! # 一处刻意偏差：`name` 超长给 422 而不是让数据库报错
//!
//! 上游靠 pydantic 的 `Field(max_length=64)` 在**入口**拦下超长名。本仓的
//! 请求 DTO 不做长度校验，若不在这一层补上，`varchar(64)` 会在 INSERT 时
//! 抛 `value too long` —— 对一个「名字写长了」的输入回 **500** 是错的。
//! 所以这里比照上游在 422 上拦，`details` 用 `{"name": <原因>}`（与
//! `catalog::actor` 的 `invalid_field` 同形）。
//!
//! 校验的是**原始输入**（trim 之前）：上游的 `max_length` 在 strip 之前生效，
//! 对 `"  <64 字符>  "` 同样回 422。

use sm_core::api_key::{ApiKeyMaterial, NAME_MAX_LEN};
use sm_db::repo::ApiKeyRepository;
use sm_db::system::api_key::ApiKey;
use sm_db::Db;

use crate::error::{details_of, ServiceError};

/// API 密钥 service。
pub struct ApiKeyService {
    keys: ApiKeyRepository,
}

impl ApiKeyService {
    pub fn new(db: &Db) -> Self {
        Self {
            keys: ApiKeyRepository::new(db.clone()),
        }
    }

    /// 列出全部密钥，最新创建在前。
    ///
    /// **不含明文** —— 返回的是实体，而行里本来就没有明文字段。
    pub async fn list(&self) -> Result<Vec<ApiKey>, ServiceError> {
        Ok(self.keys.list_all().await?)
    }

    /// 生成一条密钥。
    ///
    /// 返回 `(落库后的行, 明文)`。**明文仅此一次** —— 库里只有它的 sha256，
    /// 之后任何端点都无法再取回。
    pub async fn create(&self, raw_name: &str) -> Result<(ApiKey, String), ServiceError> {
        // 长度校验用**原始**输入，见模块文档的「一处刻意偏差」。
        if raw_name.chars().count() > NAME_MAX_LEN {
            return Err(ServiceError::validation_with(
                "validation_error",
                "Request validation failed",
                details_of("name", format!("长度不能超过 {NAME_MAX_LEN} 个字符")),
            ));
        }
        // 上游 `name=name.strip()`；备注名允许为空串（列默认值就是 `''`）。
        let name = raw_name.trim();

        let material = ApiKeyMaterial::generate();
        let row = self
            .keys
            .insert(name, &material.key_hint, &material.key_hash)
            .await?;
        tracing::info!(api_key_id = row.id, "已生成 API 密钥");
        Ok((row, material.plain_key))
    }

    /// 删除（吊销）一条密钥。删不到 → 404 `api_key_not_found`。
    pub async fn delete(&self, key_id: i32) -> Result<(), ServiceError> {
        if self.keys.delete_by_id(key_id).await? {
            tracing::info!(key_id, "已删除 API 密钥");
            Ok(())
        } else {
            Err(Self::not_found())
        }
    }

    /// 上游抛 `ApiError(404, "api_key_not_found", "API key not found")` ——
    /// 注意**没有 details**（不是 `require_by_id` 那条 `{entity}_id` 约定）。
    fn not_found() -> ServiceError {
        ServiceError::from_status(404, "api_key_not_found", "API key not found")
    }
}
