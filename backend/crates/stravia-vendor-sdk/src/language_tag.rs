use std::collections::BTreeSet;

/// Check ordinary BCP 47 syntax (language, optional script/region, variants,
/// extensions, private use). Grandfathered tags and IANA registry validity are
/// deliberately not supported; this is syntax validation, not registration.
pub(crate) fn valid_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-').peekable();
    let alpha = |s: &str| s.bytes().all(|b| b.is_ascii_alphabetic());
    let alnum = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric());
    let subtag = |s: &str, min: usize, max: usize| (min..=max).contains(&s.len()) && alnum(s);
    let language = parts.next().unwrap_or_default();
    if language.eq_ignore_ascii_case("x") {
        let mut count = 0;
        for part in parts {
            if !subtag(part, 1, 8) {
                return false;
            }
            count += 1;
        }
        return count > 0;
    }
    if !(2..=8).contains(&language.len()) || !alpha(language) {
        return false;
    }
    if language.len() <= 3 {
        for _ in 0..3 {
            if !parts
                .peek()
                .is_some_and(|part| part.len() == 3 && alpha(part))
            {
                break;
            }
            parts.next();
        }
    }
    if parts
        .peek()
        .is_some_and(|part| part.len() == 4 && alpha(part))
    {
        parts.next();
    }
    if parts.peek().is_some_and(|part| {
        part.len() == 2 && alpha(part)
            || part.len() == 3 && part.bytes().all(|b| b.is_ascii_digit())
    }) {
        parts.next();
    }
    let variants = parts.clone();
    let mut variant_count = 0;
    while parts.peek().is_some_and(|part| {
        subtag(part, 5, 8) || part.len() == 4 && part.as_bytes()[0].is_ascii_digit() && alnum(part)
    }) {
        let variant = parts.next().unwrap();
        if variants
            .clone()
            .take(variant_count)
            .any(|seen| seen.eq_ignore_ascii_case(variant))
        {
            return false;
        }
        variant_count += 1;
    }
    let mut extension_keys = BTreeSet::new();
    while parts
        .peek()
        .is_some_and(|part| part.len() == 1 && !part.eq_ignore_ascii_case("x"))
    {
        let key = parts.next().unwrap().as_bytes()[0];
        if !key.is_ascii_alphanumeric() || !extension_keys.insert(key.to_ascii_lowercase()) {
            return false;
        }
        let mut count = 0;
        while parts.peek().is_some_and(|part| subtag(part, 2, 8)) {
            parts.next();
            count += 1;
        }
        if count == 0 {
            return false;
        }
    }
    if parts
        .peek()
        .is_some_and(|part| part.eq_ignore_ascii_case("x"))
    {
        parts.next();
        let mut count = 0;
        while parts.peek().is_some_and(|part| subtag(part, 1, 8)) {
            parts.next();
            count += 1;
        }
        if count == 0 {
            return false;
        }
    }
    parts.next().is_none()
}
