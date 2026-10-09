//! `api_keys` 表映射。
//!
//! 对应 `src/model/system/api_key.py`。
//!
//! 外部集成（如 MCP server）使用的 API 密钥。只存 SHA-256 哈希，
//! 明文仅在生成响应中出现一次；key_hint 为展示用前缀。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// API 密钥。对应 Python `ApiKey`。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ApiKey {
    pub id: i32,
    pub name: String,
    pub key_hint: String,
    pub key_hash: String,
    pub last_used_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}
