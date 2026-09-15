//! 可逆引用的保留语法。完整标记是协议原子，不代表持有者获得了还原权限。

pub const PREFIX: &str = "<!-- stravia-redaction-marker:rm_";
pub const SUFFIX: &str = " -->";
pub const IDENTIFIER_LEN: usize = 32;
pub const REFERENCE_LEN: usize = PREFIX.len() + IDENTIFIER_LEN + SUFFIX.len();

/// 检查标记指定字节位置的合法性；超出完整标记长度时返回 false。
pub fn matches_byte(index: usize, byte: u8) -> bool {
    if index < PREFIX.len() {
        byte == PREFIX.as_bytes()[index]
    } else if index < PREFIX.len() + IDENTIFIER_LEN {
        byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
    } else {
        SUFFIX
            .as_bytes()
            .get(index - PREFIX.len() - IDENTIFIER_LEN)
            .is_some_and(|expected| byte == *expected)
    }
}

/// 只识别完整的新格式标记，不解析旧占位符或接受周围空白。
pub fn valid_reference(value: &str) -> bool {
    value.len() == REFERENCE_LEN
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches_byte(index, byte))
}

/// 借用输入开头的完整标记；后续路径、文本和空白均不属于引用。
pub fn reference_prefix(value: &str) -> Option<&str> {
    let reference = value.get(..REFERENCE_LEN)?;
    valid_reference(reference).then_some(reference)
}

/// 返回下一完整标记的字节偏移和原文，跳过不合法的同名前缀。
pub fn find_reference(value: &str) -> Option<(usize, &str)> {
    value.match_indices(PREFIX).find_map(|(start, _)| {
        reference_prefix(&value[start..]).map(|reference| (start, reference))
    })
}

pub(crate) fn new_reference() -> String {
    format!("{PREFIX}{}{SUFFIX}", uuid::Uuid::new_v4().simple())
}
