//! 登录与令牌刷新 —— 对应 `src/service/system/auth_service.py`。
//!
//! # 只落两类规则，其余留给 sm-core / sm-db
//!
//! 令牌的**生成与哈希**（`RefreshTokenMaterial`）、**轮换状态机**（仓储的
//! `rotate`，三步在同一事务里）已经分别住在 `sm-core::refresh_token` 与
//! `sm-db::repo::user`。本文件只负责编排与错误码。
//!
//! # 与上游的两处**刻意**差异
//!
//! **① 密码哈希是「Argon2id 写、bcrypt 也读」。** 上游 `bcrypt.checkpw`。
//! 本实现**新密码一律写 Argon2id**（内存硬、抗 GPU/ASIC），但**必须能验
//! 存量 bcrypt 哈希** —— 这个后端是原地替换上游，数据库里的哈希一行没动，
//! 认不得就意味着切换当天所有人登不进来。所以 [`AuthService::login`]
//! 在验证成功后按 [`sm_core::password::HashKind::should_upgrade`] 重哈希回写，
//! **第一次登录即完成无感迁移**，不需要单独脚本或停机窗口。写侧永远不再
//! 生成 bcrypt。
//!
//! **② 刷新时取用户用 `find_primary`。** 上游写的是
//! `User.select().order_by(User.id).first()` —— 单用户部署下的固定写法。
//! 这里照搬，**没有**改成"按令牌反查用户"，因为 `user_refresh_tokens`
//! **没有 `user_id` 列**（见 `sm-db/src/repo/user.rs` 的文档）。改不了的
//! 是 schema，不是这里。
//!
//! # 错误码
//!
//! | 场景 | status | code |
//! |---|---|---|
//! 用户名或密码错 | 401 | `invalid_credentials` |
//! 刷新令牌无效 / 已吊销 / 已过期 | 401 | `invalid_refresh_token` |
//! 刷新时库里没有用户 | 401 | `unauthorized` |

use chrono::{DateTime, Duration, Utc};
use sm_core::jwt::encode_access_token;
use sm_core::password::{hash_password, verify_and_classify};
use sm_core::refresh_token::{hash_token, RefreshRejection, RefreshTokenMaterial};
use sm_db::common::time::now_utc;
use sm_db::repo::{NewRefreshToken, UserRefreshTokenRepository, UserRepository};
use sm_db::Db;

use crate::error::ServiceError;

/// 鉴权配置。
///
/// 默认值抄自上游 `src/config/config.py`：
/// access `60*24*30` 分钟（30 天），refresh `60*24*7` 分钟（7 天）。
/// 放进结构体而不是全局，是为了让测试能构造"刚签发就过期"的场景。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub secret: String,
    pub access_token_expire_minutes: i64,
    pub refresh_token_expire_minutes: i64,
}

impl AuthConfig {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            access_token_expire_minutes: 60 * 24 * 30,
            refresh_token_expire_minutes: 60 * 24 * 7,
        }
    }
}

/// 一次签发的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenPair {
    pub access_token: String,
    /// 明文刷新令牌。**只在响应里出现这一次**，库里只有它的哈希。
    pub refresh_token: String,
    /// 上游固定 `"Bearer"`。
    pub token_type: String,
    /// `access_token_expire_minutes * 60`，是**配置值**而非实际剩余秒数。
    pub expires_in: i64,
    pub expires_at: DateTime<Utc>,
    pub refresh_expires_at: DateTime<Utc>,
    pub username: String,
}

pub struct AuthService {
    users: UserRepository,
    tokens: UserRefreshTokenRepository,
}

impl AuthService {
    pub fn new(db: &Db) -> Self {
        Self {
            users: UserRepository::new(db.clone()),
            tokens: UserRefreshTokenRepository::new(db.clone()),
        }
    }

    /// 验证成功后把哈希升级为当前默认的 Argon2id。
    ///
    /// # 为什么失败只记日志不返回错误
    ///
    /// 密码**已经验证成功了** —— 此时把用户挡在门外，代价（用户登不进来）
    /// 远大于收益（哈希晚一晚升级）。而升级失败只有两种可能：哈希算不出来
    /// （几乎不可能）或数据库写不进去（该修的是数据库）。所以这里
    /// best-effort + `warn`，与上面 [`Self::login`] 里 `touch_last_login`
    /// 的处理是同一条原则。
    ///
    /// # 幂等性
    ///
    /// 重复执行是安全的：升级后 `needs_rehash` 为 false，不会再进这条路径。
    /// 而中途失败时哈希保持原样，下次登录会**再试一次** —— 这正是想要的：
    /// 迁移会随着登录自然推进，而不是需要单独的重试脚本。
    async fn upgrade_password_hash(users: &UserRepository, user_id: i32, password: &str) {
        let fresh = match hash_password(password) {
            Ok(hash) => hash,
            Err(err) => {
                tracing::warn!(user_id, error = %err, "生成 Argon2id 哈希失败，保留原哈希");
                return;
            }
        };
        match users.set_password_hash(user_id, &fresh).await {
            Ok(_) => tracing::info!(user_id, "密码哈希已升级为 Argon2id"),
            Err(err) => {
                tracing::warn!(user_id, error = %err, "回写新哈希失败，保留原哈希（下次登录会重试）")
            }
        }
    }

    /// 用户名 + 密码 → 令牌对。
    pub async fn login(
        &self,
        config: &AuthConfig,
        username: &str,
        password: &str,
        client_ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<TokenPair, ServiceError> {
        let user = match self.users.find_by_username(username.trim()).await? {
            Some(user) => user,
            // 用户名不存在与密码错误**必须**返回同一个错误码 ——
            // 区分二者等于告诉攻击者哪些用户名是有效的。
            None => return Err(Self::invalid_credentials()),
        };

        // 两种算法都收（存量 bcrypt + 本实现写的 Argon2id），并在验证成功后
        // 按需升级哈希 —— 见 `sm_core::password` 的模块文档。
        let hash_kind = verify_and_classify(password, &user.password_hash)
            .map_err(|_| Self::invalid_credentials())?;
        if hash_kind.should_upgrade() {
            Self::upgrade_password_hash(&self.users, user.id, password).await;
        }

        if let Err(err) = self.users.touch_last_login(user.id).await {
            // 登录时间写不进去不该让用户登不上 —— 但这必须**可见**，
            // 否则审计字段会长期静默为空。
            tracing::warn!(user_id = user.id, error = %err, "写入 last_login_at 失败");
        }

        let now = Utc::now();
        let material = RefreshTokenMaterial::generate();
        self.tokens
            .insert(&NewRefreshToken {
                token_id: material.token_id.clone(),
                token_hash: material.token_hash.clone(),
                expires_at: now_utc() + Duration::minutes(config.refresh_token_expire_minutes),
                client_ip: client_ip.map(str::to_owned),
                user_agent: user_agent.map(str::to_owned),
            })
            .await?;

        Ok(Self::build(
            config,
            user.id,
            &user.username,
            &material.plain_token,
            now,
        ))
    }

    /// 刷新令牌 → 新令牌对。**旧令牌立即失效。**
    pub async fn refresh(
        &self,
        config: &AuthConfig,
        refresh_token: &str,
        client_ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<TokenPair, ServiceError> {
        let presented_hash = hash_token(refresh_token);
        let record = match self.tokens.find_active_by_hash(&presented_hash).await? {
            Some(record) => record,
            None => return Err(Self::rejected(RefreshRejection::NotFound)),
        };

        let now_naive = now_utc();
        if record.expires_at <= now_naive {
            return Err(Self::rejected(RefreshRejection::Expired));
        }

        // 上游：User.select().order_by(User.id).first()
        let user = match self.users.find_primary().await? {
            Some(user) => user,
            None => {
                return Err(ServiceError::unauthorized(
                    "unauthorized",
                    "Invalid access token",
                ))
            }
        };

        let now = Utc::now();
        let material = RefreshTokenMaterial::generate();
        // 轮换三步（校验 → 吊销旧 → 插入新）在仓储内部的**同一个事务**里。
        // 分开提交会留下「旧已吊销、新未插入」的窗口，用户被彻底登出且无法自愈。
        self.tokens
            .rotate(
                &record.token_id,
                &presented_hash,
                &NewRefreshToken {
                    token_id: material.token_id.clone(),
                    token_hash: material.token_hash.clone(),
                    expires_at: now_naive + Duration::minutes(config.refresh_token_expire_minutes),
                    client_ip: client_ip.map(str::to_owned),
                    user_agent: user_agent.map(str::to_owned),
                },
                now_naive,
            )
            .await?;

        Ok(Self::build(
            config,
            user.id,
            &user.username,
            &material.plain_token,
            now,
        ))
    }

    fn build(
        config: &AuthConfig,
        user_id: i32,
        username: &str,
        plain_refresh_token: &str,
        now: DateTime<Utc>,
    ) -> TokenPair {
        let expires_at = now + Duration::minutes(config.access_token_expire_minutes);
        let refresh_expires_at = now + Duration::minutes(config.refresh_token_expire_minutes);
        TokenPair {
            access_token: encode_access_token(i64::from(user_id), expires_at, &config.secret),
            refresh_token: plain_refresh_token.to_owned(),
            token_type: "Bearer".to_owned(),
            expires_in: config.access_token_expire_minutes * 60,
            expires_at,
            refresh_expires_at,
            username: username.to_owned(),
        }
    }

    fn invalid_credentials() -> ServiceError {
        ServiceError::unauthorized("invalid_credentials", "Username or password is incorrect")
    }

    fn rejected(rejection: RefreshRejection) -> ServiceError {
        // 对外统一一个错误码，但日志要能区分三种成因。
        tracing::info!(reason = rejection.log_tag(), "刷新令牌被拒");
        ServiceError::unauthorized(RefreshRejection::ERROR_CODE, RefreshRejection::MESSAGE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_upstream() {
        let config = AuthConfig::new("s");
        assert_eq!(config.access_token_expire_minutes, 60 * 24 * 30);
        assert_eq!(config.refresh_token_expire_minutes, 60 * 24 * 7);
    }

    #[test]
    fn expires_in_is_the_configured_window_not_the_remaining_time() {
        let config = AuthConfig::new("s");
        let pair = AuthService::build(&config, 1, "u", "rt", Utc::now());
        assert_eq!(pair.expires_in, 60 * 24 * 30 * 60);
        assert_eq!(pair.token_type, "Bearer");
    }

    #[test]
    fn refresh_window_is_shorter_than_access() {
        // 反直觉但确是上游配置：access 30 天、refresh 只有 7 天
        // （`config.py` 的 access_token_expire_minutes=60*24*30、
        // refresh_token_expire_minutes=60*24*7）。
        //
        // 这意味着**刷新令牌会先过期**，7 天后必须重新登录 —— 与"刷新令牌
        // 活得更久"的常规约定相反。照搬它，不要"修正"：改长 refresh 会
        // 让既有部署的令牌寿命悄悄变化。
        let config = AuthConfig::new("s");
        let pair = AuthService::build(&config, 1, "u", "rt", Utc::now());
        assert!(pair.refresh_expires_at < pair.expires_at);
    }

    #[test]
    fn access_token_carries_the_user_id() {
        // `sub` 必须是用户 id —— 鉴权提取器靠它查 User。
        // 若这里退化成常量，所有 token 会指向同一个（或不存在）的用户。
        let config = AuthConfig::new("secret");
        let pair = AuthService::build(&config, 42, "u", "rt", Utc::now());
        let decoded =
            sm_core::jwt::decode_access_token(&pair.access_token, "secret", Utc::now()).unwrap();
        assert_eq!(decoded.user_id, 42);
    }
}
