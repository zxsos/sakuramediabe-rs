//! SHA-1（FIPS 180-4）。仅用于内容指纹与 BitTorrent v1 info hash。

/// 增量式 SHA-1 计算器。
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    buffer: [u8; 64],
    buffered: usize,
    length_bits: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    /// 创建空状态（对应 `hashlib.sha1()`）。
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0],
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

    /// 产出 20 字节摘要；`self` 之后不可再 `update`。
    pub fn finalize(mut self) -> [u8; 20] {
        let length_bits = self.length_bits;

        // 追加 0x80，再补零到 56 mod 64，最后 8 字节为原始比特长度。
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
        for slot in self.buffer.iter_mut().skip(self.buffered).take(56 - self.buffered) {
            *slot = 0;
        }
        self.buffer[56..64].copy_from_slice(&length_bits.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut digest = [0u8; 20];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut words = [0u32; 80];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;

        for (index, word) in words.iter().enumerate() {
            let (mixed, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(mixed)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hex, sha1_hex};

    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let one_shot = sha1_hex(&data);

        for chunk_size in [1usize, 7, 63, 64, 65, 128] {
            let mut hasher = Sha1::new();
            for chunk in data.chunks(chunk_size) {
                hasher.update(chunk);
            }
            assert_eq!(hex(&hasher.finalize()), one_shot, "chunk_size={chunk_size}");
        }
    }

    #[test]
    fn handles_exact_block_boundaries() {
        for length in [0usize, 55, 56, 57, 63, 64, 119, 120, 128] {
            let data = vec![b'x'; length];
            let mut hasher = Sha1::new();
            hasher.update(&data);
            let streamed = hex(&hasher.finalize());
            assert_eq!(streamed, sha1_hex(&data), "length={length}");
        }
    }
}
