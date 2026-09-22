use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_runtime_contract::thinking::TargetThinkingControl;

pub(crate) const SELECTOR_EXTENSION_KEY: &str = "devin";

const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const TRAILING_FLAGS: &[&str] = &["thinking", "1m", "fast", "priority", "slow"];
const SPEED_FLAGS: &[&str] = &["fast", "priority", "slow"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectorParts<'a> {
    pub family: &'a str,
    pub level: Option<&'a str>,
    pub flags: &'a str,
    pub enum_form: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectorTraits {
    pub level: Option<String>,
    pub thinking: bool,
    pub context_1m: bool,
    pub speed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectorTable {
    pub default: String,
    pub selectors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routers: Option<Vec<String>>,
}

pub(crate) fn decompose(selector: &str) -> SelectorParts<'_> {
    let enum_form = selector.starts_with("MODEL_");
    let sep = if enum_form { '_' } else { '-' };
    let is_level = |segment: &str| {
        LEVELS
            .iter()
            .any(|level| segment.eq_ignore_ascii_case(level))
    };
    let is_flag = |segment: &str| {
        TRAILING_FLAGS
            .iter()
            .any(|flag| segment.eq_ignore_ascii_case(flag))
    };

    let mut head = selector;
    while let Some((rest, last)) = head.rsplit_once(sep) {
        if !is_flag(last) {
            break;
        }
        head = rest;
    }
    let flags = &selector[head.len()..];
    if let Some((family, level)) = head.rsplit_once(sep)
        && is_level(level)
    {
        return SelectorParts {
            family,
            level: Some(level),
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

pub(crate) fn traits(selector: &str) -> SelectorTraits {
    let parts = decompose(selector);
    let sep = if parts.enum_form { '_' } else { '-' };
    let mut traits = SelectorTraits {
        level: parts.level.map(str::to_ascii_lowercase),
        thinking: false,
        context_1m: false,
        speed: false,
    };
    for flag in parts.flags.split(sep).filter(|flag| !flag.is_empty()) {
        if flag.eq_ignore_ascii_case("thinking") {
            traits.thinking = true;
        } else if flag.eq_ignore_ascii_case("1m") {
            traits.context_1m = true;
        } else if SPEED_FLAGS
            .iter()
            .any(|speed| flag.eq_ignore_ascii_case(speed))
        {
            traits.speed = true;
        }
    }
    traits
}

pub(crate) fn selector_with_level(selector: &str, level: &str) -> String {
    let parts = decompose(selector);
    let (sep, level) = if parts.enum_form {
        ("_", level.to_ascii_uppercase())
    } else {
        ("-", level.to_ascii_lowercase())
    };
    format!("{}{sep}{level}{}", parts.family, parts.flags)
}

pub(crate) fn without_speed(selector: &str) -> String {
    let parts = decompose(selector);
    let sep = if parts.enum_form { '_' } else { '-' };
    let mut out = parts.family.to_string();
    if let Some(level) = parts.level {
        out.push(sep);
        out.push_str(level);
    }
    for flag in parts.flags.split(sep).filter(|flag| !flag.is_empty()) {
        if !SPEED_FLAGS
            .iter()
            .any(|speed| flag.eq_ignore_ascii_case(speed))
        {
            out.push(sep);
            out.push_str(flag);
        }
    }
    out
}

pub(crate) fn selector_from_alias(model_id: &str) -> String {
    if model_id.contains('.') {
        model_id.replace('.', "-")
    } else {
        model_id.to_string()
    }
}

pub(crate) fn table_extension_value(table: &SelectorTable) -> Value {
    serde_json::to_value(table).unwrap_or(Value::Null)
}

pub(crate) fn table_from_extensions(extensions: &BTreeMap<String, Value>) -> Option<SelectorTable> {
    let table: SelectorTable =
        serde_json::from_value(extensions.get(SELECTOR_EXTENSION_KEY)?.clone()).ok()?;
    if table.selectors.is_empty()
        || !table
            .selectors
            .iter()
            .any(|selector| selector == &table.default)
    {
        return None;
    }
    let SelectorTable {
        default,
        selectors,
        routers,
    } = table;
    let routers = routers.map(|routers| {
        routers
            .into_iter()
            .filter(|router| selectors.contains(router))
            .collect()
    });
    Some(SelectorTable {
        default,
        selectors,
        routers,
    })
}

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
    let has_1m = members.iter().any(|(_, traits)| traits.context_1m);
    let in_pool = |traits: &SelectorTraits| !has_1m || traits.context_1m;
    let rank = |traits: &SelectorTraits| {
        (
            traits.speed as u8,
            (traits.thinking != default_traits.thinking) as u8,
        )
    };
    let pick = |predicate: &dyn Fn(&SelectorTraits) -> bool| {
        members
            .iter()
            .filter(|(_, traits)| in_pool(traits) && predicate(traits))
            .min_by(|(left, left_traits), (right, right_traits)| {
                rank(left_traits)
                    .cmp(&rank(right_traits))
                    .then_with(|| left.cmp(right))
            })
            .map(|(selector, _)| (*selector).clone())
    };

    match control {
        Some(TargetThinkingControl::Effort { value }) => {
            let level = value.to_ascii_lowercase();
            pick(&|traits| traits.level.as_deref() == Some(level.as_str()))
        }
        Some(TargetThinkingControl::Enabled) => pick(&|traits| traits.thinking),
        Some(TargetThinkingControl::Disabled) => {
            if default_traits.thinking {
                pick(&|traits| !traits.thinking)
            } else {
                Some(table.default.clone())
            }
        }
        _ => None,
    }
    .unwrap_or_else(|| table.default.clone())
}

pub(crate) fn level_from_label(label: &str) -> Option<String> {
    let mut words: Vec<&str> = label.split_whitespace().collect();
    while let Some(last) = words.last() {
        if matches!(
            last.to_ascii_lowercase().as_str(),
            "1m" | "fast" | "thinking" | "context"
        ) {
            words.pop();
        } else {
            break;
        }
    }
    match words.last()?.to_ascii_lowercase().as_str() {
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

pub(crate) fn family_name_from_label(label: &str) -> Option<String> {
    let mut words: Vec<&str> = label.split_whitespace().collect();
    while let Some(last) = words.last() {
        if matches!(
            last.to_ascii_lowercase().as_str(),
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
            words.pop();
        } else {
            break;
        }
    }
    let name = words.join(" ");
    (!name.is_empty()).then_some(name)
}

pub(crate) fn order_levels<'a>(levels: impl Iterator<Item = &'a str>) -> Vec<String> {
    let rank = |level: &str| LEVELS.iter().position(|candidate| *candidate == level);
    let mut ordered: Vec<&str> = levels.filter(|level| rank(level).is_some()).collect();
    ordered.sort_by_key(|level| rank(level));
    ordered.dedup();
    ordered.into_iter().map(str::to_string).collect()
}
