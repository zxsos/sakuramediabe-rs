//! `api_keys` 表仓储。
//!
//! 对应上游 `src/service/system/api_key_service.py`。
//! API key 以 `sk-` 为前缀，与 JWT 共用 `Authorization: Bearer` 头。
//!
//! # 前缀与哈希定义在 `sm_core`，这里只再导出
//!
//! 生成（前缀 + 随机 + `key_hint`）与哈希是**同一条不变量**的两半 ——
//! 生成出来的明文必须能被 [`hash_key`] 哈希后按 `key_hash` 查回。把两半
//! 分在两个 crate 里会留出「改了前缀、忘了改校验」的口子。所以定义上移到
//! [`sm_core::api_key`]，这里保留同名再导出，让 `sm_api::auth` 的引用点不动。

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::error::DbError;
use crate::system::api_key::ApiKey;

/// API key 前缀。上游 `API_KEY_PREFIX = "sk-"`。
pub const API_KEY_PREFIX: &str = sm_core::api_key::API_KEY_PREFIX;

/// 对明文 key 做 SHA-256 哈希。对应上游 `_hash_key`。
pub use sm_core::api_key::hash_key;

/// `last_used_at` 更新节流：5 分钟内不重复写库。
/// 对应上游 `_LAST_USED_REFRESH_INTERVAL = timedelta(minutes=5)`。
const LAST_USED_REFRESH_SECS: i64 = 300;

/// 实体名，用于错误分类。
const ENTITY: &str = "ApiKey";

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
        .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(row)
    }

    /// 列出全部密钥，最新创建在前。
    ///
    /// 排序 `created_at DESC, id DESC` 与上游 `order_by(ApiKey.created_at.desc(),
    /// ApiKey.id.desc())` 一致。第二个键不是装饰：`created_at` 由应用写入、
    /// 同一毫秒内可以相同，只按它排序会让「刚生成的那条排在第几」不确定 ——
    /// 而客户端**期望新生成的排在最前**（生成后就地插到列表头部）。
    ///
    /// ⚠️ 列名逐字写出而不是拼一个 `COLUMNS` 常量：sqlx 0.9 只接受
    /// `&'static str`（`SqlSafeStr`），`&format!(...)` 会被编译期挡下 ——
    /// 这正是它想要的，别用 `AssertSqlSafe` 绕过去。
    pub async fn list_all(&self) -> Result<Vec<ApiKey>, DbError> {
        let rows = sqlx::query_as::<_, ApiKey>(
            "SELECT id, name, key_hint, key_hash, last_used_at, created_at, updated_at
             FROM api_keys ORDER BY created_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(rows)
    }

    /// 插入一条密钥，返回落库后的整行。
    ///
    /// `created_at` / `updated_at` 由这里写死为同一时刻 —— 表的这两列没有
    /// 数据库默认值（上游迁移里是 `NOT NULL` 且无 `DEFAULT`），不显式写会
    /// 插入失败。两列同值也是上游 `TimestampedMixin` 的语义：新建即「未修改」。
    pub async fn insert(
        &self,
        name: &str,
        key_hint: &str,
        key_hash: &str,
    ) -> Result<ApiKey, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, ApiKey>(
            "INSERT INTO api_keys (name, key_hint, key_hash, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $4) \
             RETURNING id, name, key_hint, key_hash, last_used_at, created_at, updated_at",
        )
        .bind(name)
        .bind(key_hint)
        .bind(key_hash)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 按 id 删除。返回**是否删到了行** —— 调用方据此区分 204 与 404。
    ///
    /// 不用「先查再删」：那是两次往返之间的竞态窗口，而 `rows_affected`
    /// 本来就是数据库给出的权威答案。
    pub async fn delete_by_id(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 更新 `last_used_at`（节流由调用方判断）。
    pub async fn touch_last_used(&self, id: i32, now: NaiveDateTime) -> Result<(), DbError> {
        sqlx::query("UPDATE api_keys SET last_used_at = $1, updated_at = $1 WHERE id = $2")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;
        Ok(())
    }
}

/// 判断 `last_used_at` 是否需要刷新（超过 5 分钟或从未设置）。
pub fn needs_touch(last_used_at: Option<NaiveDateTime>, now: NaiveDateTime) -> bool {
    match last_used_at {
        None => true,
        Some(t) => (now - t).num_seconds() > LAST_USED_REFRESH_SECS,
    }
}
