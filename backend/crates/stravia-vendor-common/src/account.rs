//! 额度页展示的账号标识：邮箱优先，其次用户名、昵称。
//! 不使用不透明 ID、令牌或组织名；上游给不出可读身份时返回 `None`。

/// 按调用方给出的优先顺序，返回第一个去空白后非空的候选值。
pub fn first_account_label<'a>(
    candidates: impl IntoIterator<Item = Option<&'a str>>,
) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::first_account_label;

    #[test]
    fn prefers_the_earliest_readable_candidate() {
        assert_eq!(
            first_account_label([None, Some("  "), Some(" a@example.test "), Some("alice")]),
            Some("a@example.test".to_owned())
        );
    }

    #[test]
    fn returns_none_without_a_readable_candidate() {
        assert_eq!(first_account_label([None, Some(""), Some(" \t ")]), None);
    }
}
