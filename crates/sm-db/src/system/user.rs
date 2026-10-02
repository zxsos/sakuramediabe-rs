//! `users` / `user_refresh_tokens` 表映射。
//!
//! 对应 `src/model/system/user.py` 与 `refresh_token.py`。
//!
//! 这两张表是认证链路的唯一持久化状态。Rust 侧没有别的认证后端——
//! 旧的 `POST /auth/token-refreshes` 依赖 `client_ip` 与 `user_agent` 做审计留痕，
//! 因此这两列必须保留，不能因为「日志里也有」就省掉。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// 刷新令牌状态。对应 `src/model/enums.py` 的 `RefreshTokenStatus`。
///
/// 数据库里存的是字符串而非数字，重写时不要改成整数枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTokenStatus {
    Active,
    Revoked,
    Expired,
}

impl RefreshTokenStatus {
    /// 数据库中存储的字面量。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    /// 解析数据库字面量。未知值返回 `None`，不静默降级为 `Active` ——
    /// 那会把一个已失效的令牌当成有效令牌。
    pub fn from_str_lossy(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "revoked" => Some(Self::Revoked),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

impl std::fmt::Display for RefreshTokenStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `users` 表。
#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i32,
    pub username: String,
    /// argon2 哈希。**永不返回给客户端**。
    pub password_hash: String,
    pub last_login_at: Option<NaiveDateTime>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `user_refresh_tokens` 表。
///
/// 刷新令牌采用**轮换**模型：`replaced_by_token_id` 指向接替它的新令牌，
/// `revoked_at` 记录吊销时刻，`client_ip` / `user_agent` 用于审计。
#[derive(Debug, Clone, FromRow)]
pub struct UserRefreshToken {
    pub id: i32,
    /// 对外下发的令牌标识（非哈希值）。唯一。
    pub token_id: String,
    /// 令牌哈希。**永不返回给客户端**。
    pub token_hash: String,
    /// 取值见 [`RefreshTokenStatus`]。列有默认值 `"active"`。
    pub status: String,
    pub expires_at: NaiveDateTime,
    pub revoked_at: Option<NaiveDateTime>,
    /// 轮换后的接替令牌；末代为 `None`。
    pub replaced_by_token_id: Option<String>,
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl UserRefreshToken {
    /// 解析状态列。
    ///
    /// 数据库里存在未知状态时返回 `None`，调用方应视为**拒绝**而非放行。
    pub fn parsed_status(&self) -> Option<RefreshTokenStatus> {
        RefreshTokenStatus::from_str_lossy(&self.status)
    }

    /// 是否可用于刷新。
    ///
    /// 只看状态；`expires_at` 的比较属于 service 层，
    /// 因为过期后的清理节奏由任务中心控制。
    pub fn can_refresh(&self) -> bool {
        matches!(self.parsed_status(), Some(RefreshTokenStatus::Active))
    }

    /// 是否已被轮换掉（有接替者）。
    pub fn is_rotated(&self) -> bool {
        self.replaced_by_token_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_literals_match_backend_enum() {
        assert_eq!(RefreshTokenStatus::Active.as_str(), "active");
        assert_eq!(RefreshTokenStatus::Revoked.as_str(), "revoked");
        assert_eq!(RefreshTokenStatus::Expired.as_str(), "expired");
    }

    #[test]
    fn status_roundtrip() {
        for status in [
            RefreshTokenStatus::Active,
            RefreshTokenStatus::Revoked,
            RefreshTokenStatus::Expired,
        ] {
            assert_eq!(
                RefreshTokenStatus::from_str_lossy(status.as_str()),
                Some(status),
                "status={status}"
            );
        }
    }

    #[test]
    fn unknown_status_is_not_silently_active() {
        // 关键：把未知状态降级成 Active 会让失效令牌被当成有效令牌。
        assert_eq!(RefreshTokenStatus::from_str_lossy("bogus"), None);
        assert_eq!(RefreshTokenStatus::from_str_lossy(""), None);
        assert_eq!(
            RefreshTokenStatus::from_str_lossy("ACTIVE"),
            None,
            "大小写敏感，与数据库字面量一致"
        );
    }

    #[test]
    fn can_refresh_requires_active() {
        let make = |status: &str, replaced: Option<&str>| UserRefreshToken {
            id: 1,
            token_id: "t".to_owned(),
            token_hash: "h".to_owned(),
            status: status.to_owned(),
            expires_at: NaiveDateTime::default(),
            revoked_at: None,
            replaced_by_token_id: replaced.map(str::to_owned),
            client_ip: None,
            user_agent: None,
            created_at: None,
            updated_at: None,
        };

        assert!(make("active", None).can_refresh());
        assert!(!make("revoked", None).can_refresh());
        assert!(!make("expired", None).can_refresh());
        assert!(!make("bogus", None).can_refresh(), "未知状态必须拒绝");

        let rotated = make("active", Some("next-token"));
        assert!(rotated.is_rotated());
    }
}
