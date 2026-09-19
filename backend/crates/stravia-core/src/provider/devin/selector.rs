//! Devin model selector decomposition and family resolution.
//!
//! Reasoning effort on Devin is not a request parameter — it is a suffix of
//! the model selector itself (`gpt-5-5-medium`, `claude-opus-4-8-xhigh`).
//! `GetChatMessageRequest` has no effort field; switching levels means
//! switching the selector inside the same family.
//!
//! Selector grammar observed on the live `GetCliModelConfigs` catalog:
//! `{family}[-{level}][-thinking][-1m][-{speed}]` for dash-form selectors, and
//! `MODEL_{FAMILY}_{LEVEL}` for the legacy enum form. `-fast`, `-priority`
//! and `-slow` are speed lanes, `-1m` the 1M-context tier, `-thinking` the
//! on/off thinking variant — all preserved verbatim as trailing flags and
//! never participating in level computation.
//!
//! The catalog collapses to one visible record per family: the record id is
//! the family's dotted alias (`gpt-5.6-sol`), and the record carries the
//! family's whole selector set in metadata extensions so `build_request` can
//! resolve alias + thinking control back to a callable upstream selector by
//! rule — 1M preferred, ordinary lanes before speed lanes, missing levels
//! fall back to the family default.

use std::collections::BTreeMap;

use serde_json::Value;
use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};

/// Effort levels as they appear in selectors (lowercase dash form).
const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Trailing flags that sit after the level slot and must survive a rewrite.
/// `slow` is a speed tier like `fast`/`priority` (`swe-1-6-slow`).
const TRAILING_FLAGS: &[&str] = &["thinking", "1m", "fast", "priority", "slow"];
/// Speed-lane flag segments — `-fast`, `-priority`, `-slow` price a premium
/// lane, not a different model; used only when no ordinary selector exists.
const SPEED_FLAGS: &[&str] = &["fast", "priority", "slow"];

/// Metadata extension key holding the family's selector table so request-time
/// resolution stays inside the Devin vendor.
pub(crate) const SELECTOR_EXTENSION_KEY: &str = "devin";

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

/// The per-selector facts family resolution actually consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectorTraits {
    /// Lowercase effort level written on the selector, if any. Bare selectors
    /// report `None` even though upstream may treat them as a level (`glm-5-2`
    /// is implicitly High) — implicit levels surface via the entry label at
    /// discovery, not here.
    pub level: Option<String>,
    /// `-thinking` thinking-on variant flag.
    pub thinking: bool,
    /// `-1m` 1M-context variant flag.
    pub context_1m: bool,
    /// `-fast` / `-priority` / `-slow` speed-lane flag.
    pub speed: bool,
}

/// The selector table a family record carries in
/// `metadata.extensions["devin"]`: every upstream selector id the family can
/// call plus the default pick. Resolution at request time is pure rule
/// application over this set — no further catalog knowledge is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectorTable {
    pub default: String,
    pub selectors: Vec<String>,
    /// Subset of `selectors` the catalog flags `is_model_router`. These must
    /// be resolved through `AssignModel` before `GetChatMessage` — calling a
    /// router uid directly is answered with the canned `unavailable:
    /// third-party model provider` trailer. `None` means the table predates
    /// router knowledge or was built without catalog entries — those uids are
    /// "unknown" and still attempt assignment with fallback.
    pub routers: Option<Vec<String>>,
}

/// Split a selector into family / level / trailing flags.
///
/// Unknown shapes (bare family names, `MODEL_PRIVATE_*`) decompose to
/// `level: None` and rewrite by appending.
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

/// The resolution-relevant facts of one selector, derived from `decompose`.
pub(crate) fn traits(selector: &str) -> SelectorTraits {
    let parts = decompose(selector);
    let sep = if parts.enum_form { '_' } else { '-' };
    let mut traits = SelectorTraits {
        level: parts.level.map(|level| level.to_ascii_lowercase()),
        thinking: false,
        context_1m: false,
        speed: false,
    };
    for flag in parts.flags.split(sep).filter(|f| !f.is_empty()) {
        if flag.eq_ignore_ascii_case("thinking") {
            traits.thinking = true;
        } else if flag.eq_ignore_ascii_case("1m") {
            traits.context_1m = true;
        } else if SPEED_FLAGS.iter().any(|f| flag.eq_ignore_ascii_case(f)) {
            traits.speed = true;
        }
    }
    traits
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

/// The selector with speed-lane flags removed — the "ordinary form" used to
/// decide whether a speed-only family is just a shadowed lane of another
/// family (`swe-1-6-fast` → `swe-1-6`).
pub(crate) fn without_speed(selector: &str) -> String {
    let parts = decompose(selector);
    let sep = if parts.enum_form { '_' } else { '-' };
    let mut out = parts.family.to_string();
    if let Some(level) = parts.level {
        out.push(sep);
        out.push_str(level);
    }
    for flag in parts.flags.split(sep).filter(|f| !f.is_empty()) {
        if !SPEED_FLAGS.iter().any(|s| flag.eq_ignore_ascii_case(s)) {
            out.push(sep);
            out.push_str(flag);
        }
    }
    out
}

/// A family record id uses the catalog's dotted alias (`gpt-5.6-sol`) while
/// every callable selector is dash-form (`gpt-5-6-sol-medium`). Aliases only
/// dot version boundaries, so dots unconditionally become dashes — dash-form
/// input is returned unchanged, letting this double as the manual-add
/// passthrough normalizer.
pub(crate) fn selector_from_alias(model_id: &str) -> String {
    if model_id.contains('.') {
        model_id.replace('.', "-")
    } else {
        model_id.to_string()
    }
}

/// Serialize a family's selector table into the metadata-extension value.
pub(crate) fn table_extension_value(table: &SelectorTable) -> Value {
    let mut value = serde_json::json!({
        "default": table.default,
        "selectors": table.selectors,
    });
    if let Some(routers) = &table.routers {
        value["routers"] = serde_json::json!(routers);
    }
    value
}

/// Read the selector table back out of a record's metadata extensions.
/// Malformed or absent tables degrade to `None`, which routes the request to
/// the legacy string-rewrite fallback instead of failing.
pub(crate) fn table_from_extensions(extensions: &BTreeMap<String, Value>) -> Option<SelectorTable> {
    let table = extensions.get(SELECTOR_EXTENSION_KEY)?;
    let default = table.get("default")?.as_str()?.to_string();
    let selectors: Vec<String> = table
        .get("selectors")?
        .as_array()?
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect();
    if selectors.is_empty() || !selectors.iter().any(|s| s == &default) {
        return None;
    }
    // `routers` absent = written before the flag existed → `None` (unknown),
    // NOT an empty known set — otherwise stale tables would skip AssignModel
    // and replay into the canned router-uid failure.
    let routers = table.get("routers").and_then(Value::as_array).map(|list| {
        list.iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .filter(|s| selectors.contains(s))
            .collect()
    });
    Some(SelectorTable {
        default,
        selectors,
        routers,
    })
}

/// Resolve a family's selector table + thinking control to the concrete
/// upstream selector. The rules are deterministic and total:
///
/// * candidate pool = the family's `-1m` members, or the whole set when the
///   family has no explicit 1M variant (natively-1M families like `gpt-5-6-sol`
///   never write the suffix);
/// * `Effort{v}` → a pool member carrying level `v`, preferring ordinary
///   lanes over speed lanes and the default's thinking state;
/// * `Enabled`/`Disabled` → a pool member with/without `-thinking`;
/// * anything else, or no match → the stored default.
///
/// Speed-lane members stay in the set as last resort but lose every ordinary
/// alternative, per "speed variants only when no ordinary version exists".
pub(crate) fn resolve_selector(
    table: &SelectorTable,
    control: Option<&TargetThinkingControl>,
) -> String {
    let default_traits = traits(&table.default);
    let members: Vec<(&String, SelectorTraits)> = table
        .selectors
        .iter()
        .map(|selector| (selector, traits(selector)))
        .collect();
    let has_1m = members.iter().any(|(_, t)| t.context_1m);
    let pool = |t: &SelectorTraits| !has_1m || t.context_1m;
    // Rank ordinary lanes first, then the lane matching the default's
    // thinking state, then name order for determinism.
    let rank = |t: &SelectorTraits| (t.speed as u8, (t.thinking != default_traits.thinking) as u8);
    let pick = |predicate: &dyn Fn(&SelectorTraits) -> bool| -> Option<String> {
        members
            .iter()
            .filter(|(_, t)| pool(t))
            .filter(|(_, t)| predicate(t))
            .min_by(|(a, ta), (b, tb)| rank(ta).cmp(&rank(tb)).then_with(|| a.cmp(b)))
            .map(|(s, _)| (*s).clone())
    };

    match control {
        Some(TargetThinkingControl::Effort { value }) => {
            let level = value.to_ascii_lowercase();
            pick(&|t| t.level.as_deref() == Some(level.as_str()))
        }
        Some(TargetThinkingControl::Enabled) => pick(&|t| t.thinking),
        Some(TargetThinkingControl::Disabled) => {
            if !default_traits.thinking {
                Some(table.default.clone())
            } else {
                pick(&|t| !t.thinking)
            }
        }
        _ => None,
    }
    .unwrap_or_else(|| table.default.clone())
}

/// The catalog label encodes what the selector leaves implicit: the level of
/// a bare selector (`GLM-5.2 High` → high) and the trailing qualifier words.
/// Strip `Thinking`/`Fast`/`1M`/`Context` qualifiers, then read the level
/// word at the label tail. `No Thinking` reports `none`.
pub(crate) fn level_from_label(label: &str) -> Option<String> {
    let mut tokens: Vec<&str> = label.split_whitespace().collect();
    while let Some(last) = tokens.last() {
        match last.to_ascii_lowercase().as_str() {
            "1m" | "fast" | "thinking" | "context" => {
                tokens.pop();
            }
            _ => break,
        }
    }
    match tokens.last()?.to_ascii_lowercase().as_str() {
        "no" | "none" => Some("none".into()),
        "minimal" => Some("minimal".into()),
        "low" => Some("low".into()),
        "medium" => Some("medium".into()),
        "high" => Some("high".into()),
        "xhigh" => Some("xhigh".into()),
        "max" => Some("max".into()),
        _ => None,
    }
}

/// The family display name is the label minus its level and qualifier tail
/// (`GLM-5.2 High 1M` → `GLM-5.2`, `GPT-5.6 Sol Medium Thinking` →
/// `GPT-5.6 Sol`). Used only as the fallback name when canonical matching
/// misses; a family genuinely named with a trailing level word loses it —
/// acceptable for a fallback.
pub(crate) fn family_name_from_label(label: &str) -> Option<String> {
    let mut tokens: Vec<&str> = label.split_whitespace().collect();
    while let Some(last) = tokens.last() {
        let word = last.to_ascii_lowercase();
        if matches!(
            word.as_str(),
            "1m" | "fast"
                | "thinking"
                | "context"
                | "no"
                | "none"
                | "minimal"
                | "low"
                | "medium"
                | "high"
                | "xhigh"
                | "max"
        ) {
            tokens.pop();
        } else {
            break;
        }
    }
    let name = tokens.join(" ");
    (!name.is_empty()).then_some(name)
}

/// Order a level set by ThinkingLevel rung (`none` < `minimal` < ... < `max`).
pub(crate) fn order_levels<'a>(levels: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut ordered: Vec<(&str, ThinkingLevel)> = levels
        .filter_map(|level| {
            ThinkingLevel::from_wire(level)
                .ok()
                .map(|rung| (level, rung))
        })
        .collect();
    ordered.sort_by_key(|(_, rung)| *rung);
    ordered
        .into_iter()
        .map(|(level, _)| level.to_string())
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

        // `slow` is a speed lane and strips like `fast`/`priority`.
        let p = decompose("swe-1-6-slow");
        assert_eq!(p.family, "swe-1-6");
        assert_eq!(p.level, None);
        assert_eq!(p.flags, "-slow");

        let p = decompose("swe-1-7");
        assert_eq!(p.family, "swe-1-7");
        assert_eq!(p.level, None);
        assert_eq!(p.flags, "");
    }

    #[test]
    fn traits_classify_thinking_context_and_speed() {
        let t = traits("claude-opus-4-6-thinking-1m");
        assert_eq!(t.level, None);
        assert!(t.thinking && t.context_1m && !t.speed);

        let t = traits("gpt-5-6-sol-high-priority");
        assert_eq!(t.level.as_deref(), Some("high"));
        assert!(t.speed && !t.thinking && !t.context_1m);

        let t = traits("swe-1-6-slow");
        assert_eq!(t.level, None);
        assert!(t.speed && !t.context_1m);
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
    fn alias_ids_normalize_to_selector_form() {
        assert_eq!(selector_from_alias("gpt-5.6-sol"), "gpt-5-6-sol");
        assert_eq!(selector_from_alias("glm-5.2"), "glm-5-2");
        assert_eq!(selector_from_alias("swe-1-7"), "swe-1-7");
    }

    #[test]
    fn resolve_prefers_1m_and_ordinary_lanes() {
        let table = SelectorTable {
            routers: None,
            default: "glm-5-2-1m".into(),
            selectors: [
                "glm-5-2",
                "glm-5-2-1m",
                "glm-5-2-max",
                "glm-5-2-max-1m",
                "glm-5-2-none",
                "glm-5-2-none-1m",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        };
        let effort = |v: &str| {
            Some(TargetThinkingControl::Effort {
                value: v.to_string(),
            })
        };
        assert_eq!(resolve_selector(&table, None), "glm-5-2-1m");
        assert_eq!(
            resolve_selector(&table, effort("max").as_ref()),
            "glm-5-2-max-1m"
        );
        assert_eq!(
            resolve_selector(&table, effort("none").as_ref()),
            "glm-5-2-none-1m"
        );
        // No `-high` member exists: the bare selector is implicitly High, so
        // the family default covers the request.
        assert_eq!(
            resolve_selector(&table, effort("high").as_ref()),
            "glm-5-2-1m"
        );
    }

    #[test]
    fn resolve_matches_levels_in_native_1m_families() {
        // `gpt-5-6-sol` carries 1M context with no `-1m` suffix: the whole set
        // is the pool, and `-priority` lanes lose to ordinary selectors.
        let table = SelectorTable {
            routers: None,
            default: "gpt-5-6-sol-medium".into(),
            selectors: [
                "gpt-5-6-sol-none",
                "gpt-5-6-sol-low",
                "gpt-5-6-sol-medium",
                "gpt-5-6-sol-high",
                "gpt-5-6-sol-xhigh",
                "gpt-5-6-sol-max",
                "gpt-5-6-sol-high-priority",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        };
        let effort = |v: &str| {
            Some(TargetThinkingControl::Effort {
                value: v.to_string(),
            })
        };
        assert_eq!(
            resolve_selector(&table, effort("high").as_ref()),
            "gpt-5-6-sol-high"
        );
        assert_eq!(resolve_selector(&table, None), "gpt-5-6-sol-medium");
    }

    #[test]
    fn resolve_falls_to_speed_lane_only_when_no_ordinary_exists() {
        let table = SelectorTable {
            routers: None,
            default: "swe-1-6-slow".into(),
            selectors: vec!["swe-1-6-slow".to_string()],
        };
        assert_eq!(resolve_selector(&table, None), "swe-1-6-slow");
        // A requested level with no member still lands on the speed default.
        let effort = Some(TargetThinkingControl::Effort {
            value: "high".to_string(),
        });
        assert_eq!(resolve_selector(&table, effort.as_ref()), "swe-1-6-slow");
    }

    #[test]
    fn resolve_thinking_toggle_picks_flagged_members() {
        let table = SelectorTable {
            routers: None,
            default: "claude-opus-4-6-1m".into(),
            selectors: [
                "claude-opus-4-6",
                "claude-opus-4-6-1m",
                "claude-opus-4-6-thinking",
                "claude-opus-4-6-thinking-1m",
                "claude-opus-4-6-thinking-fast",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        };
        assert_eq!(
            resolve_selector(&table, Some(&TargetThinkingControl::Enabled)),
            "claude-opus-4-6-thinking-1m"
        );
        assert_eq!(
            resolve_selector(&table, Some(&TargetThinkingControl::Disabled)),
            "claude-opus-4-6-1m"
        );
    }

    #[test]
    fn extension_table_round_trips_and_rejects_garbage() {
        let table = SelectorTable {
            routers: None,
            default: "glm-5-2-1m".into(),
            selectors: vec!["glm-5-2-1m".into(), "glm-5-2".into()],
        };
        let mut extensions = BTreeMap::new();
        extensions.insert(
            SELECTOR_EXTENSION_KEY.to_string(),
            table_extension_value(&table),
        );
        assert_eq!(table_from_extensions(&extensions), Some(table));

        for value in [
            serde_json::json!({"default": "glm-5-2-1m"}),
            serde_json::json!({"selectors": ["glm-5-2-1m"]}),
            serde_json::json!({"default": "absent", "selectors": ["glm-5-2-1m"]}),
            serde_json::json!({"default": "glm-5-2-1m", "selectors": []}),
        ] {
            let mut extensions = BTreeMap::new();
            extensions.insert(SELECTOR_EXTENSION_KEY.to_string(), value);
            assert_eq!(table_from_extensions(&extensions), None);
        }
        assert_eq!(table_from_extensions(&BTreeMap::new()), None);
    }

    #[test]
    fn stale_table_without_routers_stays_unknown() {
        let decode = |value: Value| {
            let mut extensions = BTreeMap::new();
            extensions.insert(SELECTOR_EXTENSION_KEY.to_string(), value);
            table_from_extensions(&extensions).expect("table").routers
        };
        // Written before router knowledge existed: absence must NOT read as
        // a known non-router set, or stale records skip AssignModel and the
        // bare router uid replays into the canned `unavailable` trailer.
        assert_eq!(
            decode(serde_json::json!({"default": "swe-2", "selectors": ["swe-2"]})),
            None
        );
        assert_eq!(
            decode(serde_json::json!({
                "default": "swe-2", "selectors": ["swe-2"], "routers": []
            })),
            Some(Vec::new())
        );
        assert_eq!(
            decode(serde_json::json!({
                "default": "swe-2", "selectors": ["swe-2"], "routers": ["swe-2"]
            })),
            Some(vec!["swe-2".to_string()])
        );
    }

    #[test]
    fn labels_reveal_implicit_levels_and_family_names() {
        assert_eq!(level_from_label("GLM-5.2 High").as_deref(), Some("high"));
        assert_eq!(level_from_label("GLM-5.2 High 1M").as_deref(), Some("high"));
        assert_eq!(
            level_from_label("GLM-5.2 No Thinking").as_deref(),
            Some("none")
        );
        assert_eq!(
            level_from_label("GPT-5.6 Sol Medium Thinking").as_deref(),
            Some("medium")
        );
        assert_eq!(
            level_from_label("SWE-1.7 Lightning Max").as_deref(),
            Some("max")
        );
        assert_eq!(level_from_label("Adaptive"), None);

        assert_eq!(
            family_name_from_label("GLM-5.2 High 1M").as_deref(),
            Some("GLM-5.2")
        );
        assert_eq!(
            family_name_from_label("GPT-5.6 Sol Medium Thinking").as_deref(),
            Some("GPT-5.6 Sol")
        );
        assert_eq!(
            family_name_from_label("GPT-5.6 Sol No Thinking Fast").as_deref(),
            Some("GPT-5.6 Sol")
        );
        assert_eq!(
            family_name_from_label("SWE-1.7 Lightning Max").as_deref(),
            Some("SWE-1.7 Lightning")
        );
    }

    #[test]
    fn order_levels_sorts_by_thinking_rung() {
        let ordered = order_levels(["xhigh", "low", "medium", "unknown", "none"].into_iter());
        assert_eq!(ordered, ["none", "low", "medium", "xhigh"]);
    }
}
