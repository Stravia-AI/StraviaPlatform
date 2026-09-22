use std::collections::{BTreeMap, BTreeSet};

use crate::codec::devin_connect::{DevinModelConfig, devin_upstream_provider_name};
use serde_json::{Value, json};
use stravia_vendor_sdk::DiscoveredModel;

use super::selector::{self, SelectorTable, SelectorTraits};

struct Member<'a> {
    selector: &'a str,
    entry: Option<&'a DevinModelConfig>,
    traits: SelectorTraits,
}

pub(crate) struct DevinFamily {
    pub id: String,
    pub name: Option<String>,
    pub default: String,
    pub selectors: Vec<String>,
    pub routers: Option<Vec<String>>,
    pub levels: Vec<String>,
    pub thinking_toggle: bool,
    pub entry: Option<DevinModelConfig>,
}

fn retain_selector(selector: &str, entry: Option<&DevinModelConfig>) -> bool {
    if selector.starts_with("MODEL_") || selector == "fusion" || selector.starts_with("fusion-") {
        return false;
    }
    entry.is_none_or(|entry| {
        entry
            .alias
            .as_deref()
            .is_some_and(|alias| !alias.trim().is_empty())
    })
}

fn effective_alias(entry: &DevinModelConfig) -> Option<&str> {
    entry
        .alias
        .as_deref()
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
}

fn pick_default<'a>(members: &[Member<'a>]) -> &'a str {
    let has_1m = members.iter().any(|member| member.traits.context_1m);
    let in_pool = |member: &Member| !has_1m || member.traits.context_1m;
    let ordinary: Vec<&Member> = members
        .iter()
        .filter(|member| in_pool(member) && !member.traits.speed)
        .collect();
    let fallback: Vec<&Member> = members.iter().filter(|member| in_pool(member)).collect();
    let candidates = if ordinary.is_empty() {
        &fallback
    } else {
        &ordinary
    };
    let non_thinking: Vec<&Member> = candidates
        .iter()
        .copied()
        .filter(|member| !member.traits.thinking)
        .collect();
    let candidates = if non_thinking.is_empty() {
        candidates
    } else {
        &non_thinking
    };
    candidates
        .iter()
        .find(|member| {
            member
                .entry
                .and_then(|entry| entry.short_alias.as_deref())
                .is_some_and(|alias| !alias.trim().is_empty())
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|member| member.traits.level.as_deref() == Some("medium"))
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|member| member.traits.level.is_none())
        })
        .or_else(|| candidates.first())
        .map(|member| member.selector)
        .unwrap_or(members[0].selector)
}

pub(crate) fn group_families(
    selectors: &[String],
    entries: &[DevinModelConfig],
) -> Vec<DevinFamily> {
    let by_selector: BTreeMap<&str, &DevinModelConfig> = entries
        .iter()
        .map(|entry| (entry.selector.as_str(), entry))
        .collect();
    let mut retained = BTreeSet::new();
    let mut groups: BTreeMap<String, Vec<Member<'_>>> = BTreeMap::new();
    for selector in selectors {
        let entry = by_selector.get(selector.as_str()).copied();
        if !retain_selector(selector, entry) {
            continue;
        }
        let key = entry
            .and_then(effective_alias)
            .map(selector::selector_from_alias)
            .unwrap_or_else(|| selector::decompose(selector).family.to_string());
        retained.insert(selector.clone());
        groups.entry(key).or_default().push(Member {
            selector,
            entry,
            traits: selector::traits(selector),
        });
    }
    groups.retain(|_, members| {
        !(members.iter().all(|member| member.traits.speed)
            && members.iter().any(|member| {
                let ordinary = selector::without_speed(member.selector);
                ordinary != member.selector && retained.contains(&ordinary)
            }))
    });
    groups.into_values().map(build_family).collect()
}

fn build_family(members: Vec<Member<'_>>) -> DevinFamily {
    let default = pick_default(&members).to_string();
    let default_member = members
        .iter()
        .find(|member| member.selector == default)
        .expect("default selector is a family member");
    let id = default_member
        .entry
        .and_then(effective_alias)
        .map(str::to_string)
        .or_else(|| most_common_alias(&members))
        .unwrap_or_else(|| selector::decompose(&default).family.to_string());
    let name = members
        .iter()
        .filter_map(|member| member.entry.and_then(|entry| entry.label.as_deref()))
        .find_map(selector::family_name_from_label);
    let default_entry = default_member.entry.cloned();

    let mut levels = BTreeSet::new();
    for member in &members {
        if let Some(level) = &member.traits.level {
            levels.insert(level.clone());
        } else if !member.traits.speed
            && let Some(level) = member
                .entry
                .and_then(|entry| entry.label.as_deref())
                .and_then(selector::level_from_label)
        {
            levels.insert(level);
        }
    }
    let levels = selector::order_levels(levels.iter().map(String::as_str));
    let has_1m = members.iter().any(|member| member.traits.context_1m);
    let pool: Vec<&Member> = members
        .iter()
        .filter(|member| !has_1m || member.traits.context_1m)
        .collect();
    let thinking_count = pool.iter().filter(|member| member.traits.thinking).count();
    let thinking_toggle = thinking_count > 0 && thinking_count < pool.len();
    let selectors = members
        .iter()
        .map(|member| member.selector.to_string())
        .collect();
    let routers = members
        .iter()
        .any(|member| member.entry.is_some())
        .then(|| {
            members
                .iter()
                .filter(|member| member.entry.is_some_and(|entry| entry.is_router))
                .map(|member| member.selector.to_string())
                .collect()
        });

    DevinFamily {
        id,
        name,
        default,
        selectors,
        routers,
        levels,
        thinking_toggle,
        entry: default_entry,
    }
}

fn most_common_alias(members: &[Member<'_>]) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for member in members.iter().filter(|member| !member.traits.speed) {
        if let Some(alias) = member.entry.and_then(effective_alias) {
            *counts.entry(alias).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(alias, _)| alias.to_string())
}

pub(crate) fn discovered_model(family: &DevinFamily) -> DiscoveredModel {
    let table = SelectorTable {
        default: family.default.clone(),
        selectors: family.selectors.clone(),
        routers: family.routers.clone(),
    };
    let mut metadata = BTreeMap::new();
    metadata.insert(
        selector::SELECTOR_EXTENSION_KEY.to_string(),
        selector::table_extension_value(&table),
    );
    metadata.insert("reasoning_levels".into(), json!(family.levels));
    metadata.insert(
        "thinking_toggle".into(),
        Value::Bool(family.thinking_toggle),
    );

    let mut capabilities = vec![
        "infer".to_string(),
        "tools".to_string(),
        "reasoning".to_string(),
    ];
    if let Some(entry) = &family.entry {
        if let Some(context_window) = entry.context_window.filter(|value| *value > 0) {
            metadata.insert("context_window".into(), json!(context_window));
        }
        if let Some(provider) = entry.provider.and_then(devin_upstream_provider_name) {
            metadata.insert(
                "upstream_provider".into(),
                Value::String(provider.to_string()),
            );
        }
        if entry.supports_images == Some(true) {
            capabilities.push("image_input".into());
        }
        if let Some(cost) = entry.cost {
            metadata.insert(
                "cost_per_million_tokens_usd".into(),
                json!({
                    "input": cost.input,
                    "cache_read": cost.cache_read,
                    "output": cost.output,
                }),
            );
        }
    }

    DiscoveredModel {
        id: family.id.clone(),
        display_name: family.name.clone().unwrap_or_else(|| family.id.clone()),
        family: Some(family.id.clone()),
        selector: Some(family.default.clone()),
        capabilities,
        metadata,
    }
}
