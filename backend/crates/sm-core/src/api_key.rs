//! API 密钥的生成、展示前缀与存储哈希。
//!
//! 对应上游 `src/service/system/api_key_service.py`（55 行）。
//!
//! # 三条不变量
//!
//! 1. **明文只在响应里出现一次**。库里只存 `sha256(明文)` 的小写 hex，
//!    明文丢失后无法从数据库恢复 —— 这是「生成后请立即保存」那句话的根据。
//! 2. **明文带 `sk-` 前缀**。`sm_api::auth` 的 Bearer 头靠这个前缀把 API key
//!    与 JWT（`eyJ...` 三段式）分开。改前缀 = 改鉴权分派，不是改字符串。
//! 3. **`key_hint` 是明文的头 11 个字符**（`sk-` + 8 位）。它进列表响应，
//!    所以**不是**秘密；但它必须短到无法还原明文 —— 上游取 11 位正是这个量级。
//!
//! # 为什么哈希无盐
//!
//! 鉴权要拿 Bearer 头的原文**直接**按哈希查库（`find_by_hash`）。加盐就得
//! 先知道是谁的 key 才能选盐，而那正是要查的东西。无盐的代价是「哈希可被
//! 离线穷举」，所以明文必须是高熵随机串 —— [`SECRET_BYTES`] 的 32 字节
//! （256 位）正是为此。

use rand::RngCore;

use crate::hashing_support::{hex, sha256};
use crate::jwt::base64url;

/// Bearer 头里以该前缀开头的 token 按 API key 校验。
pub const API_KEY_PREFIX: &str = "sk-";

/// 随机部分的字节数。上游 `secrets.token_urlsafe(32)`。
pub const SECRET_BYTES: usize = 32;

/// `key_hint` 的长度：`"sk-"` + 8 位 base64url。上游 `_KEY_HINT_LENGTH = 11`。
pub const KEY_HINT_LEN: usize = 11;

/// 备注名长度上限。列是 `varchar(64)`，上游 `Field(max_length=64)`。
pub const NAME_MAX_LEN: usize = 64;

/// 一次密钥生成的产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyMaterial {
    /// 下发给客户端的明文，`"sk-" + base64url(32 字节)`（46 字符）。
    /// **只在这里出现一次**，丢失后无法从数据库恢复。
    pub plain_key: String,
    /// 展示用前缀，明文头 [`KEY_HINT_LEN`] 个字符。进列表响应，非秘密。
    pub key_hint: String,
    /// `sha256(plain_key)` 的小写 hex（64 字符），落库。
    pub key_hash: String,
}

impl ApiKeyMaterial {
    /// 生成新的 API 密钥。
    pub fn generate() -> Self {
        let mut secret = [0u8; SECRET_BYTES];
        rand::thread_rng().fill_bytes(&mut secret);
        Self::from_plain(&format!("{API_KEY_PREFIX}{}", base64url(&secret)))
    }

    /// 由已知明文构造，用于测试与对拍。
    pub fn from_plain(plain_key: &str) -> Self {
        Self {
            plain_key: plain_key.to_owned(),
            // `chars().take()` 而不是字节切片：前缀与 base64url 都是 ASCII，
            // 两者等价，但取字符不会在意外输入上切出半个 UTF-8 码点。
            key_hint: plain_key.chars().take(KEY_HINT_LEN).collect(),
            key_hash: hash_key(plain_key),
        }
    }
}

/// API 密钥的存储哈希。
///
/// 对应上游 `_hash_key`：`hashlib.sha256(raw_key.encode("utf-8")).hexdigest()`。
/// 与刷新令牌的哈希（[`crate::refresh_token::hash_token`]）算法相同 ——
/// 两者都是 sha256 hex，别把它和媒体指纹的 SHA-1 搞混。
pub fn hash_key(raw_key: &str) -> String {
    hex(&sha256(raw_key.as_bytes()))
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
            hash_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash_key("abc").len(), 64);
    }

    #[test]
    fn plain_key_is_prefixed_and_46_chars() {
        let material = ApiKeyMaterial::generate();
        assert!(material.plain_key.starts_with(API_KEY_PREFIX));
        // "sk-" 3 + base64url(32 字节) 无 padding 43
        assert_eq!(material.plain_key.len(), 46);
        let allowed = B64URL.as_bytes();
        assert!(
            material.plain_key[API_KEY_PREFIX.len()..]
                .bytes()
                .all(|byte| allowed.contains(&byte)),
            "随机部分只能出现 base64url 字母表字符"
        );
    }

    #[test]
    fn hint_is_the_first_eleven_chars_of_the_plain_key() {
        // 与上游断言 `body["key_hint"] == body["key"][:11]` 一致。
        let material = ApiKeyMaterial::generate();
        assert_eq!(material.key_hint, material.plain_key[..KEY_HINT_LEN]);
        assert_eq!(material.key_hint.len(), KEY_HINT_LEN);
        assert!(material.key_hint.starts_with(API_KEY_PREFIX));
    }

    #[test]
    fn hint_never_carries_the_whole_secret() {
        // 前缀只是展示用：它必须真的短于明文。
        let material = ApiKeyMaterial::generate();
        assert!(material.key_hint.len() < material.plain_key.len());
    }

    #[test]
    fn plain_key_never_equals_its_hash() {
        let material = ApiKeyMaterial::generate();
        assert_ne!(material.plain_key, material.key_hash);
    }

    #[test]
    fn generated_material_is_unique_across_calls() {
        let first = ApiKeyMaterial::generate();
        let second = ApiKeyMaterial::generate();
        assert_ne!(first.plain_key, second.plain_key);
        assert_ne!(first.key_hash, second.key_hash);
    }

    #[test]
    fn from_plain_truncates_short_input_without_padding() {
        // 明文短于 11 个字符时，hint 就是它本身（不补齐、不 panic）。
        let material = ApiKeyMaterial::from_plain("sk-abc");
        assert_eq!(material.key_hint, "sk-abc");
        assert_eq!(material.key_hash, hash_key("sk-abc"));
    }
}
