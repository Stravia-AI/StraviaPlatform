//! 额度页展示的订阅等级。上游给出的人类可读名称（如 `Google AI Pro`）原样保留；
//! 只有明显是机器标识的值（如 `pro`、`team_plus`）才整理成可读形式。
//! 只接受上游确实返回的等级字段，不从额度大小推断。

/// 返回去空白后的订阅等级展示名；空值返回 `None`。
///
/// 仅由小写字母、数字、`_`、`-` 组成的值视为机器标识：分隔符转空格并首字母大写。
/// 含大写字母或空格的值认为上游已经给出展示名，不改写。
pub fn display_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let machine_token = raw.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    });
    if !machine_token {
        return Some(raw.to_owned());
    }
    let words = raw
        .split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            let first = chars.next().map(|c| c.to_ascii_uppercase());
            first.into_iter().chain(chars).collect::<String>()
        })
        .collect::<Vec<_>>();
    (!words.is_empty()).then(|| words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn machine_tokens_become_readable_names() {
        assert_eq!(display_name(" pro ").as_deref(), Some("Pro"));
        assert_eq!(display_name("team_plus").as_deref(), Some("Team Plus"));
        assert_eq!(display_name("max-5x").as_deref(), Some("Max 5x"));
    }

    #[test]
    fn upstream_display_names_are_kept_verbatim() {
        assert_eq!(
            display_name("Google AI Pro").as_deref(),
            Some("Google AI Pro")
        );
        assert_eq!(display_name("Max 5x").as_deref(), Some("Max 5x"));
        assert_eq!(display_name("ChatGPT_Pro").as_deref(), Some("ChatGPT_Pro"));
    }

    #[test]
    fn blank_values_have_no_plan() {
        assert_eq!(display_name(""), None);
        assert_eq!(display_name(" \t "), None);
        assert_eq!(display_name("_-"), None);
    }
}
