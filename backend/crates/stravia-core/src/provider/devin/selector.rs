//! Devin model selector decomposition.
//!
//! Reasoning effort on Devin is not a request parameter — it is a suffix of
//! the model selector itself (`gpt-5-5-medium`, `claude-opus-4-8-xhigh`).
//! `GetChatMessageRequest` has no effort field; switching levels means
//! switching the selector inside the same family.
//!
//! Selector grammar observed on the live `GetCliModelConfigs` catalog:
//! `{family}[-{level}][-thinking][-1m][-{speed}]` for dash-form selectors, and
//! `MODEL_{FAMILY}_{LEVEL}` for the legacy enum form. `-fast` and `-priority`
//! are speed lanes, not effort levels: they are preserved verbatim as trailing
//! flags and never participate in level computation.

use std::collections::{BTreeMap, BTreeSet};
use stravia_runtime_contract::thinking::ThinkingLevel;

/// Effort levels as they appear in selectors (lowercase dash form).
const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Trailing flags that sit after the level slot and must survive a rewrite.
const TRAILING_FLAGS: &[&str] = &["thinking", "1m", "fast", "priority"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectorParts<'a> {
    /// Family prefix without level or flags (`claude-opus-4-8`, `MODEL_GPT_5_2`).
    pub family: &'a str,
    /// Level segment as written on the wire (`high`, `HIGH`), if present.
    pub level: Option<&'a str>,
    /// Everything after the level slot, separators included (`-thinking-1m`).
    pub flags: &'a str,
    /// `MODEL_*` selectors use `_` separators and UPPERCASE level tokens.
    pub enum_form: bool,
}

/// Split a selector into family / level / trailing flags.
///
/// Unknown shapes (bare family names, `MODEL_PRIVATE_*`, `swe-1-6-slow`)
/// decompose to `level: None` and rewrite by appending.
pub(crate) fn decompose(selector: &str) -> SelectorParts<'_> {
    let enum_form = selector.starts_with("MODEL_");
    let sep = if enum_form { '_' } else { '-' };
    let is_level = |segment: &str| LEVELS.iter().any(|l| segment.eq_ignore_ascii_case(l));
    let is_flag = |segment: &str| {
        TRAILING_FLAGS
            .iter()
            .any(|f| segment.eq_ignore_ascii_case(f))
    };

    // Strip trailing flag segments, remembering where they started.
    let mut head = selector;
    while let Some((rest, last)) = head.rsplit_once(sep) {
        if !is_flag(last) {
            break;
        }
        head = rest;
    }
    let flags = &selector[head.len()..];

    // The level slot is the last remaining segment, if it names a level.
    if let Some((rest, last)) = head.rsplit_once(sep)
        && is_level(last)
    {
        return SelectorParts {
            family: rest,
            level: Some(last),
            flags,
            enum_form,
        };
    }
    SelectorParts {
        family: head,
        level: None,
        flags,
        enum_form,
    }
}

/// Rewrite a selector to a different effort level, preserving trailing flags.
/// `claude-opus-4-8-low-fast` + `high` → `claude-opus-4-8-high-fast`;
/// `MODEL_GPT_5_2_LOW` + `high` → `MODEL_GPT_5_2_HIGH`.
pub(crate) fn selector_with_level(selector: &str, level: &str) -> String {
    let parts = decompose(selector);
    let (sep, level) = if parts.enum_form {
        ("_", level.to_ascii_uppercase())
    } else {
        ("-", level.to_ascii_lowercase())
    };
    format!("{}{sep}{level}{}", parts.family, parts.flags)
}

/// Map each family to the effort levels its selectors carry. Every discovered
/// selector contributes its family (even with no level) so bare entries see
/// the family's level set too.
pub(crate) fn family_level_map(selectors: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for selector in selectors {
        let parts = decompose(selector);
        let levels = map.entry(parts.family.to_string()).or_default();
        if let Some(level) = parts.level {
            levels.insert(level.to_ascii_lowercase());
        }
    }
    map
}

/// The family's levels ordered by ThinkingLevel rung, for `reasoning_options`.
pub(crate) fn family_levels_ordered(
    map: &BTreeMap<String, BTreeSet<String>>,
    family: &str,
) -> Vec<String> {
    let Some(levels) = map.get(family) else {
        return Vec::new();
    };
    let mut ordered: Vec<(&String, ThinkingLevel)> = levels
        .iter()
        .filter_map(|level| {
            ThinkingLevel::from_wire(level)
                .ok()
                .map(|rung| (level, rung))
        })
        .collect();
    ordered.sort_by_key(|(_, rung)| *rung);
    ordered
        .into_iter()
        .map(|(level, _)| level.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_splits_level_and_flags() {
        let p = decompose("claude-opus-4-8-low-fast");
        assert_eq!(p.family, "claude-opus-4-8");
        assert_eq!(p.level, Some("low"));
        assert_eq!(p.flags, "-fast");
        assert!(!p.enum_form);

        let p = decompose("claude-opus-4-6-thinking-1m");
        assert_eq!(p.family, "claude-opus-4-6");
        assert_eq!(p.level, None);
        assert_eq!(p.flags, "-thinking-1m");

        let p = decompose("gpt-5-5-medium-priority");
        assert_eq!(p.family, "gpt-5-5");
        assert_eq!(p.level, Some("medium"));
        assert_eq!(p.flags, "-priority");

        let p = decompose("MODEL_GPT_5_2_XHIGH");
        assert_eq!(p.family, "MODEL_GPT_5_2");
        assert_eq!(p.level, Some("XHIGH"));
        assert!(p.enum_form);

        // `slow` is a speed tier, not a thinking level.
        let p = decompose("swe-1-6-slow");
        assert_eq!(p.family, "swe-1-6-slow");
        assert_eq!(p.level, None);

        let p = decompose("swe-1-7");
        assert_eq!(p.family, "swe-1-7");
        assert_eq!(p.level, None);
        assert_eq!(p.flags, "");
    }

    #[test]
    fn rewrites_level_and_preserves_flags() {
        assert_eq!(
            selector_with_level("claude-opus-4-8-low-fast", "high"),
            "claude-opus-4-8-high-fast"
        );
        assert_eq!(
            selector_with_level("gpt-5-5-medium-priority", "none"),
            "gpt-5-5-none-priority"
        );
        assert_eq!(
            selector_with_level("MODEL_GPT_5_2_LOW", "high"),
            "MODEL_GPT_5_2_HIGH"
        );
        // Bare family appends the level.
        assert_eq!(selector_with_level("swe-1-7", "medium"), "swe-1-7-medium");
    }

    #[test]
    fn family_map_collects_levels_across_variants() {
        let selectors = vec![
            "claude-sonnet-5-medium".to_string(),
            "claude-sonnet-5-low".to_string(),
            "claude-sonnet-5-xhigh-fast".to_string(),
            "swe-1-7".to_string(),
        ];
        let map = family_level_map(&selectors);
        assert_eq!(
            map["claude-sonnet-5"]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["low", "medium", "xhigh"]
        );
        // The bare selector sees its family's level set (empty here).
        assert!(map["swe-1-7"].is_empty());

        let ordered = family_levels_ordered(&map, "claude-sonnet-5");
        assert_eq!(ordered, ["low", "medium", "xhigh"]);
    }
}
