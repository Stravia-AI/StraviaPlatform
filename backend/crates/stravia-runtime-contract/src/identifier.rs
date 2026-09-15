//! 平台不透明标识的统一编码；标识本身不授予资源访问权限。

use rand::distr::{Distribution, Uniform};

pub const ID_LEN: usize = 28;
pub const DIGEST_ID_LEN: usize = 55;

/// 生成约 131.6 bit 随机空间的标识。只使用小写字母，避免数字逐位分词。
pub fn new_id() -> String {
    let mut rng = rand::rng();
    // Uniform::sample 使用无偏拒绝采样，不依赖 sample_single 的可选 unbiased 特性。
    let letters = Uniform::new_inclusive(b'a', b'z').expect("nonempty ASCII alphabet");
    let mut id = String::with_capacity(ID_LEN);
    for _ in 0..ID_LEN {
        id.push(char::from(letters.sample(&mut rng)));
    }
    id
}

pub fn valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// 将完整 256-bit 大端摘要无损编码为定长 base26，不截断内容身份。
pub fn encode_digest(digest: &[u8; 32]) -> String {
    let mut value = *digest;
    let mut encoded = [b'a'; DIGEST_ID_LEN];
    let mut first = 0;
    for digit in encoded.iter_mut().rev() {
        let mut remainder = 0_u16;
        for byte in &mut value[first..] {
            let dividend = (remainder << 8) | u16::from(*byte);
            *byte = (dividend / 26) as u8;
            remainder = dividend % 26;
        }
        *digit += remainder as u8;
        while first < value.len() && value[first] == 0 {
            first += 1;
        }
        if first == value.len() {
            break;
        }
    }
    String::from_utf8(encoded.to_vec()).expect("base26 alphabet is ASCII")
}

pub fn valid_digest_id(id: &str) -> bool {
    id.len() == DIGEST_ID_LEN && id.bytes().all(|byte| byte.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_encoding_preserves_leading_zeroes_and_full_width() {
        assert_eq!(encode_digest(&[0; 32]), "a".repeat(DIGEST_ID_LEN));
        assert_eq!(
            encode_digest(&[u8::MAX; 32]),
            "ennjuuzflkeenzhszxamvlrnusvcpknavbgzllukzllrkvatszirbkp"
        );
        assert_eq!(
            encode_digest(&std::array::from_fn(|index| index as u8)),
            "aaabftwgwbyxfikpnfciasblwwqpvgdidgzcaualksxmxnspqssxiah"
        );
    }
}
