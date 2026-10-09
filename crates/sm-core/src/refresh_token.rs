//! 刷新令牌的生成、哈希与轮换状态机。
//!
//! 对应后端 `src/service/system/auth_service.py` 的 `_create_refresh_token`、
//! `_hash_token` 与 `refresh_token_pair`。
//!
//! # 三个安全不变量
//!
//! 1. **明文永不落库**。数据库只存 `sha256(plain_token)`。
//! 2. **每次刷新都轮换**。旧行标 `revoked` 并写 `replaced_by_token_id`，旧 token
//!    立即失效；重放会被 `status != active` 拦下。
//! 3. **未知状态一律拒绝**，见 `sm_db::RefreshTokenStatus`。

use rand::RngCore;

use crate::hashing_support::{hex, sha256};
use crate::jwt::base64url;

/// 明文 token 的字节数。后端 `secrets.token_urlsafe(32)`。
pub const PLAIN_TOKEN_BYTES: usize = 32;
/// `token_id` 的字节数。后端 `secrets.token_hex(16)`。
pub const TOKEN_ID_BYTES: usize = 16;

/// 一次刷新产生的三件套。
///
/// `plain_token` 只在响应里出现一次，丢失后无法从数据库恢复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshTokenMaterial {
    /// 下发给客户端的明文。base64url(32 字节)，无 padding，43 字符。
    pub plain_token: String,
    /// 公开标识，存 `token_id` 列。hex(16 字节)，32 字符。
    pub token_id: String,
    /// `sha256(plain_token)` 的小写 hex，落库。
    pub token_hash: String,
}

impl RefreshTokenMaterial {
    /// 生成新的刷新令牌。
    pub fn generate() -> Self {
        let mut plain = [0u8; PLAIN_TOKEN_BYTES];
        rand::thread_rng().fill_bytes(&mut plain);
        let mut id = [0u8; TOKEN_ID_BYTES];
        rand::thread_rng().fill_bytes(&mut id);
        let plain_token = base64url(&plain);
        let token_id = hex(&id);
        let token_hash = hash_token(&plain_token);
        Self {
            plain_token,
            token_id,
            token_hash,
        }
    }

    /// 由已知明文构造，用于测试与对拍。
    pub fn from_plain(plain_token: &str, token_id: &str) -> Self {
        Self {
            plain_token: plain_token.to_owned(),
            token_id: token_id.to_owned(),
            token_hash: hash_token(plain_token),
        }
    }
}

/// 刷新令牌的存储哈希。
///
/// 对应后端 `AuthService._hash_token`。注意是 **sha256** 而不是 sha1 ——
/// 这里容易与媒体指纹算法搞混。
pub fn hash_token(plain_token: &str) -> String {
    hex(&sha256(plain_token.as_bytes()))
}

/// 轮换时对旧记录施加的变更。
///
/// 与后端 `refresh_token_pair` 里对 `token_record` 的三行赋值一一对应：
/// `status = revoked`、`revoked_at = now`、`replaced_by_token_id = 新 id`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotation {
    pub status: String,
    pub replaced_by_token_id: String,
}

impl Rotation {
    /// 构造一次轮换：旧记录转为 `revoked` 并指向新令牌的 `token_id`。
    pub fn to(replacement_token_id: &str) -> Self {
        Self {
            status: "revoked".to_owned(),
            replaced_by_token_id: replacement_token_id.to_owned(),
        }
    }

    /// 该轮换是否应把 `replaced_by_token_id` 视为已设置。
    pub fn replaces(&self) -> bool {
        !self.replaced_by_token_id.is_empty()
    }
}

/// 刷新令牌不可用的原因。
///
/// 三种情况在客户端都映射到同一错误码，但日志里应区分：
/// 不存在、已吊销、已过期是不同的运维信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRejection {
    /// 库中查不到匹配的 `active` 记录（含明文错误）。
    NotFound,
    /// 状态不是 `active`：已吊销、已过期，或出现未知字面量。
    NotActive,
    /// `expires_at` 已过。
    Expired,
}

impl RefreshRejection {
    /// 对外错误码。后端三种情况统一抛 `ApiError(401, "invalid_refresh_token", ...)`。
    pub const ERROR_CODE: &str = "invalid_refresh_token";

    /// HTTP 状态码。
    pub const STATUS: u16 = 401;

    /// 与后端一致的英文提示。
    pub const MESSAGE: &str = "Refresh token is invalid";

    /// 运维日志用的区分标签。
    pub const fn log_tag(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotActive => "not_active",
            Self::Expired => "expired",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// base64url 字母表：大小写字母、数字、连字符、下划线。
    const B64URL: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    #[test]
    fn hash_matches_python_hashlib() {
        // Python: hashlib.sha256(b"abc").hexdigest()
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hash_of_empty_token_is_well_known() {
        assert_eq!(
            hash_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hash_is_stable_and_unsalted() {
        // 无盐：同一明文恒得同一哈希，因此可以按哈希直接查库。
        let material = RefreshTokenMaterial::from_plain("fixed-token", "tid");
        assert_eq!(material.token_hash, hash_token("fixed-token"));
        assert_eq!(material.token_hash, hash_token("fixed-token"));
    }

    #[test]
    fn plain_token_is_43_char_base64url() {
        let material = RefreshTokenMaterial::generate();
        assert_eq!(
            material.plain_token.len(),
            43,
            "base64url(32 字节) 无 padding"
        );
        let allowed = B64URL.as_bytes();
        assert!(
            material
                .plain_token
                .bytes()
                .all(|byte| allowed.contains(&byte)),
            "只能出现 base64url 字母表字符"
        );
    }

    #[test]
    fn token_id_is_32_char_hex() {
        let material = RefreshTokenMaterial::generate();
        assert_eq!(material.token_id.len(), 32);
        assert!(material
            .token_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn generated_material_is_unique_across_calls() {
        let first = RefreshTokenMaterial::generate();
        let second = RefreshTokenMaterial::generate();
        assert_ne!(first.plain_token, second.plain_token);
        assert_ne!(first.token_id, second.token_id);
        assert_ne!(first.token_hash, second.token_hash);
    }

    #[test]
    fn plain_token_never_equals_its_hash() {
        // 落库的必须是哈希而非明文，两者绝不能相同。
        let material = RefreshTokenMaterial::generate();
        assert_ne!(material.plain_token, material.token_hash);
    }

    #[test]
    fn rotation_marks_old_token_revoked_and_points_to_new() {
        let rotation = Rotation::to("new-token-id");
        assert_eq!(rotation.status, "revoked");
        assert_eq!(rotation.replaced_by_token_id, "new-token-id");
        assert!(rotation.replaces());
    }

    #[test]
    fn all_rejections_share_one_error_code() {
        // 客户端按单一 code 处理，不区分具体原因。三种成因共享同一组
        // 对外常量，逐个变体也必须返回相同值。
        for rejection in [
            RefreshRejection::NotFound,
            RefreshRejection::NotActive,
            RefreshRejection::Expired,
        ] {
            assert_eq!(RefreshRejection::ERROR_CODE, "invalid_refresh_token");
            assert_eq!(RefreshRejection::STATUS, 401);
            assert_eq!(RefreshRejection::MESSAGE, "Refresh token is invalid");
            // 唯一按实例变化的应是日志标签 —— 对外统一、运维可区分。
            assert!(!rejection.log_tag().is_empty(), "{rejection:?}");
        }
    }

    #[test]
    fn rejections_have_distinct_log_tags() {
        // 对外同一个 code，但日志必须能区分三种成因。
        assert_eq!(RefreshRejection::NotFound.log_tag(), "not_found");
        assert_eq!(RefreshRejection::NotActive.log_tag(), "not_active");
        assert_eq!(RefreshRejection::Expired.log_tag(), "expired");
    }
}
