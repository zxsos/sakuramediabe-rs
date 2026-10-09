//! `api_keys` 表仓储。
//!
//! 对应上游 `src/service/system/api_key_service.py`。
//! API key 以 `sk-` 为前缀，与 JWT 共用 `Authorization: Bearer` 头。

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::error::DbError;
use crate::system::api_key::ApiKey;

/// API key 前缀。上游 `API_KEY_PREFIX = "sk-"`。
pub const API_KEY_PREFIX: &str = "sk-";

/// `last_used_at` 更新节流：5 分钟内不重复写库。
/// 对应上游 `_LAST_USED_REFRESH_INTERVAL = timedelta(minutes=5)`。
const LAST_USED_REFRESH_SECS: i64 = 300;

/// API key 仓储。
pub struct ApiKeyRepository {
    pool: PgPool,
}

impl ApiKeyRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 按 key_hash 查找。用于鉴权。
    pub async fn find_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, DbError> {
        let row = sqlx::query_as::<_, ApiKey>(
            "SELECT id, name, key_hint, key_hash, last_used_at, created_at, updated_at
             FROM api_keys WHERE key_hash = $1",
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::query("ApiKey", e.to_string()))?;
        Ok(row)
    }

    /// 更新 `last_used_at`（节流由调用方判断）。
    pub async fn touch_last_used(&self, id: i32, now: NaiveDateTime) -> Result<(), DbError> {
        sqlx::query("UPDATE api_keys SET last_used_at = $1, updated_at = $1 WHERE id = $2")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::query("ApiKey", e.to_string()))?;
        Ok(())
    }
}

/// 对明文 key 做 SHA-256 哈希。对应用游 `_hash_key`。
pub fn hash_key(raw_key: &str) -> String {
    sm_core::hashing_support::hex(&sm_core::hashing_support::sha256(raw_key.as_bytes()))
}

/// 判断 `last_used_at` 是否需要刷新（超过 5 分钟或从未设置）。
pub fn needs_touch(last_used_at: Option<NaiveDateTime>, now: NaiveDateTime) -> bool {
    match last_used_at {
        None => true,
        Some(t) => (now - t).num_seconds() > LAST_USED_REFRESH_SECS,
    }
}
