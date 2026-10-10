//! MD5（RFC 1321）。仅用于 JavDB 客户端的 `jdsignature` 请求头。
//!
//! 与 `sha1` / `sha256` 同样的理由自实现（见 crate 文档）：算法固定、有 RFC
//! 公开测试向量兜底，多一个依赖就多一份离线构建与审计成本。
//!
//! **用途限定**：MD5 早已不适合任何抗碰撞场景，本工作区的完整性保证一律用
//! SHA-1/SHA-256。这里用它只因为上游 `javdb.py::_get_sign` 用 `hashlib.md5`，
//! 而 JavDB 服务端按同一算法校验 —— 换更强的摘要会导致签名**不被接受**。

/// 每步的左移位数（RFC 1321 §3.4，按轮分四组）。
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// 每步的加法常量：`K[i] = floor(2^32 * abs(sin(i + 1)))`（RFC 1321 §3.4）。
const K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee, //
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501, //
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be, //
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821, //
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa, //
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8, //
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed, //
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a, //
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c, //
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70, //
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05, //
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665, //
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039, //
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1, //
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1, //
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// 增量式 MD5 计算器。
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    buffer: [u8; 64],
    buffered: usize,
    length_bits: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

impl Md5 {
    /// 创建空状态（对应 `hashlib.md5()`）。
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buffer: [0u8; 64],
            buffered: 0,
            length_bits: 0,
        }
    }

    /// 追加数据，可多次调用。
    pub fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self.length_bits.wrapping_add((data.len() as u64) * 8);

        if self.buffered > 0 {
            let take = core::cmp::min(64 - self.buffered, data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }

        let mut chunks = data.chunks_exact(64);
        for chunk in &mut chunks {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            self.compress(&block);
        }

        let rest = chunks.remainder();
        if !rest.is_empty() {
            self.buffer[..rest.len()].copy_from_slice(rest);
            self.buffered = rest.len();
        }
    }

    /// 产出 16 字节摘要；`self` 之后不可再 `update`。
    pub fn finalize(mut self) -> [u8; 16] {
        let length_bits = self.length_bits;

        // 追加 0x80，再补零到 56 mod 64，最后 8 字节为原始比特长度。
        // ⚠️ 与 SHA-1 不同：MD5 的长度字段与摘要字都是**小端**。
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            for slot in self.buffer.iter_mut().skip(self.buffered) {
                *slot = 0;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        for slot in self
            .buffer
            .iter_mut()
            .skip(self.buffered)
            .take(56 - self.buffered)
        {
            *slot = 0;
        }
        self.buffer[56..64].copy_from_slice(&length_bits.to_le_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut digest = [0u8; 16];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0u32; 16];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }

        let [mut a, mut b, mut c, mut d] = self.state;

        for index in 0..64 {
            // 每轮的非线性函数与该轮取词的步长（RFC 1321 §3.4）。
            let (mixed, slot) = match index {
                0..=15 => ((b & c) | ((!b) & d), index),
                16..=31 => ((d & b) | ((!d) & c), (5 * index + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * index) % 16),
            };
            // a = b + ((a + F + K + M) <<< s)，四个寄存器的角色每步轮转。
            let rotated = a
                .wrapping_add(mixed)
                .wrapping_add(K[index])
                .wrapping_add(words[slot])
                .rotate_left(SHIFTS[index]);
            let previous_d = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
            a = previous_d;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hex, md5_hex};

    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let one_shot = md5_hex(&data);

        for chunk_size in [1usize, 7, 63, 64, 65, 128] {
            let mut hasher = Md5::new();
            for chunk in data.chunks(chunk_size) {
                hasher.update(chunk);
            }
            assert_eq!(hex(&hasher.finalize()), one_shot, "chunk_size={chunk_size}");
        }
    }

    #[test]
    fn handles_exact_block_boundaries() {
        // 55/56 是补位分界的经典边界（56 字节时 0x80 恰好挤出本块）。
        for length in [0usize, 55, 56, 57, 63, 64, 119, 120, 128] {
            let data = vec![b'x'; length];
            let mut hasher = Md5::new();
            hasher.update(&data);
            let streamed = hex(&hasher.finalize());
            assert_eq!(streamed, md5_hex(&data), "length={length}");
        }
    }

    #[test]
    fn matches_rfc1321_vectors() {
        // RFC 1321 §A.5 的全部七个用例。
        for (plain, expected) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(md5_hex(plain.as_bytes()), expected, "input={plain:?}");
        }
    }
}
