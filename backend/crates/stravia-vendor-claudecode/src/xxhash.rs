//! XXH64（xxHash 64 位）。仅用于计算 Claude Code 计费头里的 `cch` 校验值；
//! 算法常量与分块规则见 xxHash 规范 v0.1.1。

const PRIME_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME_5: u64 = 0x27D4_EB2F_1656_67C5;

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

pub(crate) fn xxh64(input: &[u8], seed: u64) -> u64 {
    let mut rest = input;
    let mut hash = if input.len() >= 32 {
        let mut v1 = seed.wrapping_add(PRIME_1).wrapping_add(PRIME_2);
        let mut v2 = seed.wrapping_add(PRIME_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME_1);
        while rest.len() >= 32 {
            v1 = round(v1, read_u64(&rest[0..]));
            v2 = round(v2, read_u64(&rest[8..]));
            v3 = round(v3, read_u64(&rest[16..]));
            v4 = round(v4, read_u64(&rest[24..]));
            rest = &rest[32..];
        }
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
        seed.wrapping_add(PRIME_5)
    };
    hash = hash.wrapping_add(input.len() as u64);
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

#[cfg(test)]
mod tests {
    use super::xxh64;

    /// 参考值由 Bun 1.4 的 `Bun.hash.xxHash64` 生成，覆盖空输入、尾部 1/4/8
    /// 字节分支与 32 字节条带主循环，并包含 cch 实际使用的非零种子。
    #[test]
    fn matches_reference_vectors() {
        const SEED: u64 = 0x4d65_9218_e32a_3268;
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
}
