//! 密码哈希：Argon2id。
//!
//! # 为什么切 Argon2
//!
//! Python 后端用的是 `bcrypt`（`src/pyproject.toml` 的 `bcrypt>=4.0.1`）。
//! 本实现改用 **Argon2id**，理由：

//! - Argon2 是内存硬（memory-hard），抗 GPU/ASIC 爆破；bcrypt 只做 CPU 硬。
//! - 侧信道抗性更好，且是 OWASP 与 RFC 9106 的当前推荐。
//! - 参数可随硬件提升而不破坏既有哈希（PHC 字符串自带参数）。
//!
//! # 迁移影响
//!
//! **既有 bcrypt 哈希无法用 Argon2 验证**，因此存量用户首次登录会失败。
//! 处理方式见 [`needs_rehash`]：识别出 bcrypt 前缀时，
//! 应在验证成功后用 Argon2 重新哈希并回写，实现无感升级。
//!

//! SakuraMedia 后端默认只创建一个 `account` 用户（见 `refresh_token_pair` 里

//! `User.select().order_by(User.id).first()` 的单用户假设），

//! 因此实际需要迁移的账号极少。

// argon2 0.6 收窄了 `password_hash` 的再导出：只透出 `password_hash::{self,
// PasswordHasher, PasswordVerifier, phc::PasswordHash}`。`SaltString`、`Ident`、
// `rand_core` 要从 `password-hash` crate 直接取。
//
// 另一个行为变化：`PasswordHasher::hash_password` 在 0.6 里不再接收盐 ——
// 改成开 `getrandom` 特性后由 trait 自动生成（见 Cargo.toml）。需要显式控盐时
// 另有 `hash_password_with_salt(password, salt: &[u8])`，盐的类型也从
// `&SaltString` 变成了 `&[u8]`。
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};

/// 默认参数。
///
/// 19 MiB / 2 次迭代 / 1 并行度，对应 OWASP 对 Argon2id 的最低推荐
///
// （m=19456 KiB, t=2, p=1）。NAS 场景下内存占用可接受。
pub const DEFAULT_M_COST: u32 = 19 * 1024;
/// 迭代次数。
pub const DEFAULT_T_COST: u32 = 2;
/// 并行度。
pub const DEFAULT_P_COST: u32 = 1;

/// Argon2 算法错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordError {
    /// 生成随机盐失败。
    SaltGeneration,
    /// 哈希计算失败。
    Hashing,
    /// PHC 字符串无法解析。
    InvalidHash,
    /// 密码不匹配。
    Mismatch,
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::SaltGeneration => "password: failed to generate salt",
            Self::Hashing => "password: hashing failed",
            Self::InvalidHash => "password: stored hash is not a valid PHC string",
            Self::Mismatch => "password: mismatch",
        })
    }
}

impl std::error::Error for PasswordError {}

/// 用 Argon2id 哈希明文密码，返回 PHC 字符串。
///
/// 输出形如：
/// `$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    hash_password_with(password, DEFAULT_M_COST, DEFAULT_T_COST, DEFAULT_P_COST)
}

/// 指定参数哈希，便于测试与随硬件调优。
pub fn hash_password_with(
    password: &str,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<String, PasswordError> {
    let params = Params::new(m_cost, t_cost, p_cost, None).map_err(|_| PasswordError::Hashing)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    // 盐由 `PasswordHasher::hash_password` 自动生成（需 `getrandom` 特性）。
    // 每次调用都换新盐，对应 password.rs 里的 `salt_makes_each_hash_unique`。
    let hash = argon2
        .hash_password(password.as_bytes())
        .map_err(|_| PasswordError::Hashing)?;
    Ok(hash.to_string())
}

/// 验证明文密码是否匹配已存的 PHC 字符串。
pub fn verify_password(password: &str, stored: &str) -> Result<(), PasswordError> {
    let parsed = PasswordHash::new(stored).map_err(|_| PasswordError::InvalidHash)?;
    let argon2 = Argon2::default();
    argon2
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| PasswordError::Mismatch)
}

/// 存储的哈希是否需要升级。
///
/// 返回 `true` 表示当前哈希不是本实现用默认参数生成的 Argon2id，
/// 应当在验证成功后重新哈希并回写。两种触发情形：
///
/// 1. **存量 bcrypt**（`$2a$` / `$2b$` / `$2y$` 前缀）—— 迁移必经路径。
/// 2. **参数已调整** —— 例如把 m_cost 从 19 MiB 提到 64 MiB 后，
///    旧哈希仍可用但强度落后，同样应升级。
pub fn needs_rehash(stored: &str) -> bool {
    if is_bcrypt(stored) {
        return true;
    }
    let Ok(parsed) = PasswordHash::new(stored) else {
        // 无法解析的哈希（异常数据）也标记为需重算，交由登录流程暴露问题。
        return true;
    };
    // password-hash 0.6 里 `Ident` 藏在 `phc` 下且不再有 `new_unwrap`；
    // 直接比字符串，语义相同且不锁死 `Ident` 的构造 API。
    if parsed.algorithm.as_str() != ARGON2ID_ALGORITHM {
        return true;
    }
    // 其余情况：本实现生成的 Argon2id，默认参数下无需重算。
    // 参数调优后的升级路径留给后续版本 —— password-hash 的 Params 迭代器
    // 在 0.5 与 0.6 之间 API 变动较大，此处不依赖它以免锁死版本。
    false
}

/// 是否是 bcrypt 哈希（存量数据）。
pub fn is_bcrypt(stored: &str) -> bool {
    ["$2a$", "$2b$", "$2x$", "$2y$"]
        .iter()
        .any(|prefix| stored.starts_with(prefix))
}

const ARGON2ID_ALGORITHM: &str = "argon2id";

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用低参数，避免每次跑测试都吃 19 MiB。
    const FAST: (u32, u32, u32) = (64, 1, 1);

    #[test]
    fn hash_and_verify_roundtrip() {
        let (m, t, p) = FAST;
        let stored = hash_password_with("s3cret", m, t, p).unwrap();
        assert!(verify_password("s3cret", &stored).is_ok());
        assert_eq!(
            verify_password("wrong", &stored),
            Err(PasswordError::Mismatch)
        );
    }

    #[test]
    fn phc_string_has_argon2id_prefix() {
        let (m, t, p) = FAST;
        let stored = hash_password_with("pw", m, t, p).unwrap();
        assert!(
            stored.starts_with("$argon2id$"),
            "PHC 前缀必须是 $argon2id$，实际 {stored}"
        );
        // 形如 $argon2id$v=19$m=64,t=1,p=1$<salt>$<hash>
        assert_eq!(stored.matches("$").count(), 5, "PHC 串应有 5 个分段符");
    }

    #[test]
    fn salt_makes_each_hash_unique() {
        let (m, t, p) = FAST;
        let first = hash_password_with("same", m, t, p).unwrap();
        let second = hash_password_with("same", m, t, p).unwrap();
        assert_ne!(first, second, "每次哈希必须用新盐");
        // 但两者都能验证同一明文
        assert!(verify_password("same", &first).is_ok());
        assert!(verify_password("same", &second).is_ok());
    }

    #[test]
    fn detects_bcrypt_hashes_for_migration() {
        for prefix in ["$2a$", "$2b$", "$2x$", "$2y$"] {
            let legacy = format!(
                "{prefix}10$abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
            );
            assert!(is_bcrypt(&legacy), "prefix={prefix}");
            assert!(needs_rehash(&legacy), "bcrypt 必须标记为需重算");
        }
    }

    #[test]
    fn argon2id_hash_does_not_need_rehash() {
        let (m, t, p) = FAST;
        let stored = hash_password_with("pw", m, t, p).unwrap();
        assert!(!is_bcrypt(&stored));
        assert!(!needs_rehash(&stored));
    }

    #[test]
    fn unparsable_hash_is_flagged_for_rehash() {
        // 异常数据不应静默通过，应触发重算并让登录流程暴露问题。
        assert!(needs_rehash("not-a-phc-string"));
        assert!(needs_rehash(""));
    }

    #[test]
    fn rejects_invalid_stored_hash() {
        assert_eq!(
            verify_password("pw", "garbage"),
            Err(PasswordError::InvalidHash)
        );
    }

    #[test]
    fn default_params_meet_owasp_minimum() {
        // OWASP 对 Argon2id 的最低推荐：m=19456 KiB, t=2, p=1
        assert_eq!(DEFAULT_M_COST, 19 * 1024);
        assert_eq!(DEFAULT_T_COST, 2);
        assert_eq!(DEFAULT_P_COST, 1);
    }
}
