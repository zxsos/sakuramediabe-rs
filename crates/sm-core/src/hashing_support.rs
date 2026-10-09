//! 哈希与 HMAC 支持。
//!
//! 复用独立的 `hashing` crate（零外部依赖），本模块只做两件事：
//! 把常用项 re-export 出来，以及补上 HMAC-SHA256 —— JWT HS256 需要它。

pub use hashing::{hex, sha1, sha256, Sha1, Sha256};

/// SHA-256 的块长度（字节）。
const BLOCK_LEN: usize = 64;

/// HMAC-SHA256（RFC 2104）。
///
/// `key` 超过块长度时先做一次 SHA-256 压缩，与标准要求一致。
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        key_block[..32].copy_from_slice(&sha256(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_LEN];
    let mut outer_pad = [0x5cu8; BLOCK_LEN];
    for index in 0..BLOCK_LEN {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }

    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(&inner_digest);
    outer.finalize()
}

/// 常数时间比较。
///
/// 比较 HMAC 时用 `==` 会按字节短路：第一次不相等就返回，泄露了「前几个
/// 字节是对的」这个信息，攻击者可以逐字节爆破签名。
///
/// 长度不等时直接 false —— 长度本身不是秘密。
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_is_false_on_any_difference() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"), "长度不等直接 false");
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn matches_rfc4231_case_1() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn matches_rfc4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn matches_rfc4231_case_3() {
        let key = [0xaau8; 20];
        let data = [0xddu8; 50];
        assert_eq!(
            hex(&hmac_sha256(&key, &data)),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    #[test]
    fn handles_key_longer_than_block() {
        // RFC 4231 用例 6：key 长度 131 > 64，必须先哈希。
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac_sha256(
                &key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn handles_empty_key_and_message() {
        assert_eq!(
            hex(&hmac_sha256(b"", b"")),
            "b613679a0814d9ec772f95d778c35fc5ff1697c493715653c6c712144292c5ad"
        );
    }
}
