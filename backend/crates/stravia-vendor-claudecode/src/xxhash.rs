//! XXH64（xxHash 64 位）。仅用于计算 Claude Code 计费头里的 `cch` 校验值；
//! 算法常量与分块规则见 xxHash 规范 v0.1.1。`cch` 的输入是「请求体去掉若干字段后的
//! 若干段」，所以提供流式接口，调用方可直接对已序列化的缓冲区分段喂入而不拼接副本。

const PRIME_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME_5: u64 = 0x27D4_EB2F_1656_67C5;
const STRIPE: usize = 32;

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("slice holds eight bytes"))
}

fn read_u32(bytes: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes(
        bytes[..4].try_into().expect("slice holds four bytes"),
    ))
}

fn round(accumulator: u64, lane: u64) -> u64 {
    accumulator
        .wrapping_add(lane.wrapping_mul(PRIME_2))
        .rotate_left(31)
        .wrapping_mul(PRIME_1)
}

fn merge_round(accumulator: u64, value: u64) -> u64 {
    (accumulator ^ round(0, value))
        .wrapping_mul(PRIME_1)
        .wrapping_add(PRIME_4)
}

/// 流式 XXH64：任意切分的 `update` 序列与一次性哈希它们的拼接结果相同。
pub(crate) struct Xxh64 {
    seed: u64,
    lanes: [u64; 4],
    buffer: [u8; STRIPE],
    buffered: usize,
    total: u64,
}

impl Xxh64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seed,
            lanes: [
                seed.wrapping_add(PRIME_1).wrapping_add(PRIME_2),
                seed.wrapping_add(PRIME_2),
                seed,
                seed.wrapping_sub(PRIME_1),
            ],
            buffer: [0; STRIPE],
            buffered: 0,
            total: 0,
        }
    }

    pub(crate) fn update(&mut self, mut input: &[u8]) {
        self.total = self.total.wrapping_add(input.len() as u64);
        if self.buffered + input.len() < STRIPE {
            self.buffer[self.buffered..self.buffered + input.len()].copy_from_slice(input);
            self.buffered += input.len();
            return;
        }
        if self.buffered > 0 {
            let fill = STRIPE - self.buffered;
            self.buffer[self.buffered..].copy_from_slice(&input[..fill]);
            let stripe = self.buffer;
            self.consume(&stripe);
            self.buffered = 0;
            input = &input[fill..];
        }
        while input.len() >= STRIPE {
            self.consume(&input[..STRIPE]);
            input = &input[STRIPE..];
        }
        self.buffer[..input.len()].copy_from_slice(input);
        self.buffered = input.len();
    }

    fn consume(&mut self, stripe: &[u8]) {
        for (index, lane) in self.lanes.iter_mut().enumerate() {
            *lane = round(*lane, read_u64(&stripe[index * 8..]));
        }
    }

    pub(crate) fn finish(&self) -> u64 {
        let mut hash = if self.total >= STRIPE as u64 {
            let [v1, v2, v3, v4] = self.lanes;
            let mut hash = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            hash = merge_round(hash, v1);
            hash = merge_round(hash, v2);
            hash = merge_round(hash, v3);
            merge_round(hash, v4)
        } else {
            self.seed.wrapping_add(PRIME_5)
        };
        hash = hash.wrapping_add(self.total);
        let mut rest = &self.buffer[..self.buffered];
        while rest.len() >= 8 {
            hash ^= round(0, read_u64(rest));
            hash = hash
                .rotate_left(27)
                .wrapping_mul(PRIME_1)
                .wrapping_add(PRIME_4);
            rest = &rest[8..];
        }
        if rest.len() >= 4 {
            hash ^= read_u32(rest).wrapping_mul(PRIME_1);
            hash = hash
                .rotate_left(23)
                .wrapping_mul(PRIME_2)
                .wrapping_add(PRIME_3);
            rest = &rest[4..];
        }
        for byte in rest {
            hash ^= u64::from(*byte).wrapping_mul(PRIME_5);
            hash = hash.rotate_left(11).wrapping_mul(PRIME_1);
        }
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(PRIME_2);
        hash ^= hash >> 29;
        hash = hash.wrapping_mul(PRIME_3);
        hash ^ (hash >> 32)
    }
}

#[cfg(test)]
pub(crate) fn xxh64(input: &[u8], seed: u64) -> u64 {
    let mut hasher = Xxh64::new(seed);
    hasher.update(input);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{Xxh64, xxh64};

    const SEED: u64 = 0x4d65_9218_e32a_3268;

    /// 参考值由 Bun 1.4 的 `Bun.hash.xxHash64` 生成，覆盖空输入、尾部 1/4/8
    /// 字节分支与 32 字节条带主循环，并包含 cch 实际使用的非零种子。
    #[test]
    fn matches_reference_vectors() {
        let cases: [(&str, u64, u64); 5] = [
            ("", 0xef46_db37_51d8_e999, 0xb8b3_0e7d_e65b_46c5),
            ("a", 0xd24e_c4f1_a98c_6e5b, 0xde28_ae28_b0e0_7bb2),
            ("abc", 0x44bc_2cf5_ad77_0999, 0xdfc4_f4d6_9136_99b6),
            (
                "0123456789abcdef0123456789abcdef",
                0x642a_9495_8e71_e6c5,
                0x7f82_b050_ec99_4b30,
            ),
            (
                "The quick brown fox jumps over the lazy dog, repeatedly and at length!!",
                0x3026_dcd3_906a_c510,
                0xc46f_6301_d535_991e,
            ),
        ];
        for (input, unseeded, seeded) in cases {
            assert_eq!(xxh64(input.as_bytes(), 0), unseeded, "{input:?}");
            assert_eq!(xxh64(input.as_bytes(), SEED), seeded, "{input:?}");
        }
    }

    #[test]
    fn streaming_is_independent_of_how_input_is_split() {
        let input: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let whole = xxh64(&input, SEED);
        for chunk in [1, 3, 7, 31, 32, 33, 64, 500, 1000] {
            let mut hasher = Xxh64::new(SEED);
            for part in input.chunks(chunk) {
                hasher.update(part);
            }
            assert_eq!(hasher.finish(), whole, "chunk size {chunk}");
        }
    }
}
