//! 已记住秘密的强弱分层与词边界。
//!
//! 强秘密（长且复杂）在任何位置出现都要替换，宁可多换不漏；弱秘密（短或像普通单词）
//! 只在词边界处替换，否则 `pass` 会把 `password` 拆成占位符加 `word`。
//! 判定只取决于秘密本身，不入库，调整阈值对已有映射立即生效。

/// 少于这个字符数一律视为弱：短串太容易是别的词的一部分。
const WEAK_BELOW: usize = 8;
/// 达到这个字符数一律视为强：这么长的串很少会碰巧出现在别的词里。
const STRONG_FROM: usize = 16;
/// 介于两者之间时，小写、大写、数字、其他符号四类中至少出现这么多类才算强。
/// 普通单词通常只有一到两类，随机生成的 token 通常有三类以上。
const STRONG_CLASSES: usize = 3;

/// 熵不适合做依据：n 个字符的熵最高 log2(n)，12 个字符的串最多约 3.58，
/// 短串会被一律判低。字符类别数既便于解释，也不依赖长度。
pub fn is_weak(secret: &str) -> bool {
    if secret.chars().nth(STRONG_FROM - 1).is_some() {
        return false;
    }
    if secret.chars().nth(WEAK_BELOW - 1).is_none() {
        return true;
    }
    let (mut lower, mut upper, mut digit, mut other) = (false, false, false, false);
    for character in secret.chars() {
        if character.is_ascii_lowercase() {
            lower = true;
        } else if character.is_ascii_uppercase() {
            upper = true;
        } else if character.is_ascii_digit() {
            digit = true;
        } else {
            other = true;
        }
    }
    [lower, upper, digit, other]
        .into_iter()
        .filter(|present| *present)
        .count()
        < STRONG_CLASSES
}

/// 只把 ASCII 字母、数字与下划线算作单词字符。若按 Unicode 判断，汉字也是单词字符，
/// `请使用pass登录` 里的 `pass` 会因为前面是汉字而不被替换。
pub fn is_word_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// `text[start..end]` 是否落在词边界上：秘密的首字符是单词字符时，前一个字符不能是单词字符；
/// 尾字符同理。首尾不是单词字符的一侧不设限。`start` 与 `end` 必须在字符边界上。
pub fn at_word_boundary(text: &str, start: usize, end: usize) -> bool {
    let matched = &text[start..end];
    let touches_before = matched.chars().next().is_some_and(is_word_char)
        && text[..start].chars().next_back().is_some_and(is_word_char);
    let touches_after = matched.chars().next_back().is_some_and(is_word_char)
        && text[end..].chars().next().is_some_and(is_word_char);
    !touches_before && !touches_after
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_follows_length_and_character_classes() {
        for weak in [
            "pass",
            "1234567",
            "password",
            "Password",
            "a1b2c3d4e5f6",
            "密钥密钥密钥密钥密钥",
        ] {
            assert!(is_weak(weak), "{weak} 应为弱");
        }
        for strong in [
            "Xk9mQ2vLp7",
            "Passw0rd!",
            "sk-proj-abcdefghij",
            "0123456789abcdef",
            "correcthorsebatterystaple",
        ] {
            assert!(!is_weak(strong), "{strong} 应为强");
        }
    }

    #[test]
    fn length_counts_characters_not_bytes() {
        // 七个汉字是 21 字节，仍按 7 个字符判弱。
        assert!(is_weak("密钥密钥密钥密"));
        assert!(!is_weak(&"密".repeat(16)));
    }

    #[test]
    fn boundary_only_binds_the_edges_that_are_word_characters() {
        let at = |text: &str, needle: &str| {
            let start = text.find(needle).unwrap();
            at_word_boundary(text, start, start + needle.len())
        };
        assert!(!at("password", "pass"));
        assert!(!at("bypass", "pass"));
        assert!(!at("my_pass", "pass"));
        assert!(!at("pass1", "pass"));
        assert!(at("pass,", "pass"));
        assert!(at("= pass", "pass"));
        assert!(at("\"pass\"", "pass"));
        assert!(at("pass", "pass"));
        // 非 ASCII 邻居不是单词字符。
        assert!(at("请使用pass登录", "pass"));
        // 首尾是符号的秘密在该侧不设限。
        assert!(at("xx-token-yy", "-token-"));
    }
}
