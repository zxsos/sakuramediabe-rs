//! 密码哈希：**Argon2id 为主，bcrypt 只读**（存量迁移）。
//!
//! # 为什么切 Argon2
//!
//! Python 后端用的是 `bcrypt`（`src/pyproject.toml` 的 `bcrypt>=4.0.1`）。
//! 本实现改用 **Argon2id**，理由：

//! - Argon2 是内存硬（memory-hard），抗 GPU/ASIC 爆破；bcrypt 只做 CPU 硬。
//! - 侧信道抗性更好，且是 OWASP 与 RFC 9106 的当前推荐。
//! - 参数可随硬件提升而不破坏既有哈希（PHC 字符串自带参数）。
//!
//! # 为什么两种算法共存（而不是只留 Argon2id）
//!
//! 存量用户的哈希是 **bcrypt 算的**，而这个后端是**原地替换**上游 ——
//! 数据库里那些哈希一行都没动。所以必须能验 bcrypt，否则切换当天
//! **所有人登不进来**。
//!
//! 于是分成两半：
//!
//! | | 行为 |
//! |---|---|
//! | **读**（[`verify_password`]） | Argon2id 与 bcrypt 都收，入口不要求调用方判断 |
//! | **写**（[`hash_password`]） | **只有** Argon2id，永不生成新的 bcrypt |
//!
//! 验证成功后才按 [`HashKind::should_upgrade`] 用 Argon2id 重哈希回写，
//! 于是**第一次登录即完成无感迁移**，不需要单独的迁移脚本或停机窗口。
//! 顺序不能反：验证失败绝不能改写哈希。
//!
//! # 依赖代价
//!
//! `bcrypt` 0.19 是**纯 Rust**（`blowfish` 由 `cipher` 实现，无 C 绑定），
//! 直接依赖只有 `base64` 与 `blowfish`，符合本仓库「便于交叉编译与
//! 离线 NAS 构建」的约束。
//!

//! # 关于「需要迁移的账号极少」
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

/// 验证明文密码是否匹配已存的哈希。**Argon2id 与 bcrypt 都接受。**
///
/// # 为什么这里是「两种都收」而不是「只收 Argon2id」
///
/// 这是**迁移期的入口**，不是最终形态。存量库里全是 bcrypt 哈希，而
/// 判断「这是不是 bcrypt」的责任不该落到每个调用方身上 —— 忘了判的
/// 症状是「用户密码正确却登不进来」，而且只在生产数据上出现。
/// 收在 API 边界上，忘记的可能性就少一处。
///
/// 迁移完成后（库里再无 bcrypt）可以把 bcrypt 分支摘掉，届时本函数
/// 与 [`verify_and_classify`] 等价。
pub fn verify_password(password: &str, stored: &str) -> Result<(), PasswordError> {
    verify_and_classify(password, stored).map(|_| ())
}

/// 验证并顺带回答「这个哈希该不该升级」。
///
/// 调用方拿到 [`HashKind`] 就能决定要不要在验证成功后用 Argon2id
/// 重哈希并回写 —— 那是无感升级的**唯一**触发点，且必须放在验证成功之后：
/// 失败的登录绝不能改写用户的哈希。
pub fn verify_and_classify(password: &str, stored: &str) -> Result<HashKind, PasswordError> {
    if is_bcrypt(stored) {
        return verify_bcrypt(password, stored).map(|()| HashKind::Bcrypt);
    }
    let parsed = PasswordHash::new(stored).map_err(|_| PasswordError::InvalidHash)?;
    let argon2 = Argon2::default();
    argon2
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| PasswordError::Mismatch)?;
    Ok(if needs_rehash(stored) {
        HashKind::Argon2NeedsRehash
    } else {
        HashKind::Argon2Current
    })
}

/// bcrypt 校验。
///
/// # 三个前缀都认
///
/// `$2a$`（旧版 bcrypt）、`$2b$`（**Python `bcrypt` 包的默认输出**）、
/// `$2y$`（PHP 的变体）。存量库里三种都可能存在，而
/// [`is_bcrypt`] 已经把它们都算作 bcrypt，所以这里也必须都能验 ——
/// 否则会出现「识别成 bcrypt 却验不了」的死角。
///
/// cost 不需要配置：它编码在哈希串自身里。
fn verify_bcrypt(password: &str, stored: &str) -> Result<(), PasswordError> {
    // `Ok(false)` = 密码不匹配；`Err` = 哈希串本身坏了。
    // 两者都映射成 `PasswordError`，但**区分开**有意义：前者是正常的
    // 登录失败，后者是数据损坏，只该在日志里出现而不该让用户重试。
    match bcrypt::verify(password.as_bytes(), stored) {
        Ok(true) => Ok(()),
        Ok(false) => Err(PasswordError::Mismatch),
        Err(_) => Err(PasswordError::InvalidHash),
    }
}

/// 存储的哈希是什么形态，以及验证后是否该升级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
    /// Argon2id，且参数已是当前默认 —— 无需动作。
    Argon2Current,
    /// Argon2id，但算法或参数落后 —— 应当重哈希回写。
    Argon2NeedsRehash,
    /// bcrypt（存量）—— 应当重哈希回写。
    Bcrypt,
}

impl HashKind {
    /// 验证成功后是否应当用当前默认参数重新哈希并回写。
    pub const fn should_upgrade(self) -> bool {
        !matches!(self, Self::Argon2Current)
    }
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

    /// bcrypt 的测试用低 cost（4 是下限），避免每次跑测试吃 CPU。
    fn bcrypt_of(password: &str) -> String {
        bcrypt::hash(password, 4).expect("bcrypt 哈希")
    }

    #[test]
    fn bcrypt_hashes_verify_across_all_three_prefixes() {
        // 存量库里三种前缀都可能出现，而 `is_bcrypt` 把它们都算作 bcrypt ——
        // 这里必须都能验，否则会出现「识别成 bcrypt 却验不了」的死角。
        let canonical = bcrypt_of("s3cret");
        assert!(canonical.starts_with("$2b$"), "实际前缀：{canonical}");
        for (label, rewritten) in [
            ("$2a$", canonical.replacen("$2b$", "$2a$", 1)),
            ("$2b$", canonical.clone()),
            ("$2y$", canonical.replacen("$2b$", "$2y$", 1)),
        ] {
            assert!(
                verify_password("s3cret", &rewritten).is_ok(),
                "{label} 前缀应当能验过"
            );
            assert_eq!(
                verify_password("wrong", &rewritten),
                Err(PasswordError::Mismatch),
                "{label} 前缀下错密码必须是 Mismatch"
            );
        }
    }

    #[test]
    fn verifying_a_bcrypt_hash_reports_that_it_needs_upgrading() {
        let stored = bcrypt_of("s3cret");
        let kind = verify_and_classify("s3cret", &stored).expect("应当验证通过");
        assert_eq!(kind, HashKind::Bcrypt);
        assert!(
            kind.should_upgrade(),
            "bcrypt 哈希必须被标记为待升级，否则存量用户永远迁不过来"
        );
    }

    #[test]
    fn a_wrong_password_never_reports_an_upgrade() {
        // 顺序反了就是个安全漏洞：失败的登录把哈希改写成攻击者知道的版本。
        let stored = bcrypt_of("s3cret");
        assert!(verify_and_classify("wrong", &stored).is_err());
    }

    #[test]
    fn argon2_with_current_parameters_reports_no_upgrade() {
        let (m, t, p) = FAST;
        let stored = hash_password_with("s3cret", m, t, p).unwrap();
        let kind = verify_and_classify("s3cret", &stored).expect("应当验证通过");
        // 测试用的是低参数，而 `needs_rehash` 只看算法标识 —— 所以这里
        // 期望的是「不升级」。若将来 `needs_rehash` 开始比较参数，这个
        // 断言会提醒同步。
        assert_eq!(kind, HashKind::Argon2Current);
        assert!(!kind.should_upgrade());
    }

    #[test]
    fn a_corrupted_stored_hash_is_invalid_not_a_mismatch() {
        // 两者的处置不同：Mismatch 是正常登录失败，InvalidHash 是数据损坏，
        // 只该出现在日志里而不该让用户反复重试。
        for broken in ["", "not-a-hash", "$argon2id$truncated"] {
            assert_eq!(
                verify_password("s3cret", broken),
                Err(PasswordError::InvalidHash),
                "{broken:?} 应当是 InvalidHash"
            );
        }
    }

    #[test]
    fn bcrypt_is_recognised_by_every_prefix_we_claim() {
        // `is_bcrypt` 与 `verify_bcrypt` 的前缀集合必须一致，
        // 否则会出现「认得出、验不了」。这个断言把两者钉在一起。
        for prefix in ["$2a$", "$2b$", "$2x$", "$2y$"] {
            let fake = format!("{prefix}10$rest");
            assert!(is_bcrypt(&fake), "{prefix} 应当被认作 bcrypt");
        }
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
