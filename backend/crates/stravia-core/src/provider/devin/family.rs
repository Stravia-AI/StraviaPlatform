//! Devin catalog entries collapse into one Provider Model per upstream
//! family. Upstream already groups selectors via the `alias` carried on each
//! `ClientModelConfig` (`gpt-5-6-sol-*` → `gpt-5.6-sol`); this module keeps the
//! callable selector table in the record's `extensions["devin"]` and derives
//! the visible metadata (levels, toggle, context, cost) from the family
//! default member.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde_json::Value;

use crate::protocol::codec::devin_connect::{DevinModelConfig, devin_upstream_provider_name};
use crate::provider_models::{ModelCost, PriceComponents, ProviderModelMetadata, ReasoningOption};

use super::selector::{self, SelectorTable, SelectorTraits};

/// One family member: the selector plus its decoded catalog entry when the
/// selector came from `GetCliModelConfigs`. Static fallback selectors carry
/// no entry.
pub(crate) struct Member<'a> {
    pub selector: &'a str,
    pub entry: Option<&'a DevinModelConfig>,
    pub traits: SelectorTraits,
}

/// A collapsed family: one visible record plus the selector table the vendor
/// resolves back to concrete upstream selectors at request time.
pub(crate) struct DevinFamily {
    /// Record/model id — the family's dotted alias when the catalog provides
    /// one (`gpt-5.6-sol`), else the dash-form family key (`glm-5-2`).
    pub id: String,
    /// Display-name fallback recovered from a member label (`GLM-5.2`).
    pub name: Option<String>,
    /// Resting selector — preferred 1M, ordinary lane, canonical form.
    pub default: String,
    /// Every upstream selector id belonging to the family.
    pub selectors: Vec<String>,
    /// Members the catalog flags `is_model_router` — they need `AssignModel`
    /// resolution before `GetChatMessage`. `None` when no member carries a
    /// catalog entry (static/manual selectors): router status unknown.
    pub routers: Option<Vec<String>>,
    /// Ordered effort levels the family exposes (selector suffixes plus the
    /// implicit level of bare members recovered from their labels).
    pub levels: Vec<String>,
    /// The family exposes both `-thinking` and non-thinking members.
    pub thinking_toggle: bool,
    /// Catalog entry of the default member — the metadata source for context,
    /// pricing, provider and image flags.
    pub entry: Option<DevinModelConfig>,
}

/// Filter applied before grouping. Drops enum-form selectors, the fusion
/// pairing product and catalog entries upstream marks as family-less.
fn retain_selector(selector: &str, entry: Option<&DevinModelConfig>) -> bool {
    if selector.starts_with("MODEL_") || selector == "fusion" || selector.starts_with("fusion-") {
        return false;
    }
    if let Some(entry) = entry {
        let alias = entry.alias.as_deref().unwrap_or_default().trim();
        if alias.is_empty() {
            return false;
        }
    }
    true
}

fn effective_alias(entry: &DevinModelConfig) -> Option<&str> {
    entry
        .alias
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
}

/// The selector that should carry the family when no thinking level is
/// requested: a `-1m` ordinary non-thinking member first, then the canonical
/// medium lane, then the first ordinary member, then any member.
fn pick_default<'a>(members: &[Member<'a>]) -> &'a str {
    let has_1m = members.iter().any(|m| m.traits.context_1m);
    let in_pool = |m: &Member| !has_1m || m.traits.context_1m;
    let ordinary: Vec<&Member> = members
        .iter()
        .filter(|m| in_pool(m) && !m.traits.speed)
        .collect();
    let candidates: &[&Member] = if ordinary.is_empty() {
        &members.iter().filter(|m| in_pool(m)).collect::<Vec<_>>()
    } else {
        &ordinary
    };
    let off: Vec<&Member> = candidates
        .iter()
        .copied()
        .filter(|m| !m.traits.thinking)
        .collect();
    let candidates: &[&Member] = if off.is_empty() { candidates } else { &off };
    candidates
        .iter()
        .find(|m| {
            m.entry
                .and_then(|e| e.short_alias.as_deref())
                .is_some_and(|a| !a.trim().is_empty())
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|m| m.traits.level.as_deref() == Some("medium"))
        })
        .or_else(|| candidates.iter().find(|m| m.traits.level.is_none()))
        .or_else(|| candidates.first())
        .map(|m| m.selector)
        .unwrap_or_else(|| members[0].selector)
}

/// Group retained selectors into families, then fold each group into a
/// `DevinFamily`. The group key is upstream's own family identity — the
/// catalog alias normalized to dash form (`swe-1-7-lightning` carries
/// `swe-1.7` and groups with `swe-1-7`, not alone) — falling back to the
/// decomposed selector family for catalog-less static selectors.
///
/// A family whose members are ALL speed lanes (`swe-1-6-fast` upstream lists
/// its own alias) is shadowed when its de-speeded selector exists in the
/// retained set — a fast variant of a family that has an ordinary lane is
/// not a model, it is a price tier of one.
pub(crate) fn group_families(
    selectors: &[String],
    entries: &[DevinModelConfig],
) -> Vec<DevinFamily> {
    let by_selector: BTreeMap<&str, &DevinModelConfig> =
        entries.iter().map(|e| (e.selector.as_str(), e)).collect();
    let mut groups: BTreeMap<String, Vec<Member>> = BTreeMap::new();
    let mut retained = std::collections::BTreeSet::new();
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
        !(members.iter().all(|m| m.traits.speed)
            && members.iter().any(|m| {
                let ordinary = selector::without_speed(m.selector);
                ordinary != m.selector && retained.contains(&ordinary)
            }))
    });
    groups.into_values().map(build_family).collect()
}

fn build_family<'a>(members: Vec<Member<'a>>) -> DevinFamily {
    let default = pick_default(&members).to_string();
    let default_member = members
        .iter()
        .find(|m| m.selector == default)
        .expect("default selector is a family member");

    let id = effective_alias_opt(default_member.entry)
        .or_else(|| most_common_alias(&members))
        .unwrap_or_else(|| selector::decompose(&default).family.to_string());

    let name = members
        .iter()
        .filter_map(|m| m.entry.and_then(|e| e.label.as_deref()))
        .find_map(selector::family_name_from_label);

    // Level axis: explicit selector suffixes plus the implicit level of bare
    // ordinary members (labels carry it, e.g. `GLM-5.2` → high).
    let mut level_set = std::collections::BTreeSet::new();
    for member in &members {
        if let Some(level) = &member.traits.level {
            level_set.insert(level.clone());
        } else if !member.traits.speed
            && let Some(level) = member
                .entry
                .and_then(|e| e.label.as_deref())
                .and_then(selector::level_from_label)
        {
            level_set.insert(level);
        }
    }
    let levels = selector::order_levels(level_set.iter().map(String::as_str));

    let has_1m = members.iter().any(|m| m.traits.context_1m);
    let thinking_toggle = {
        let pool = members.iter().filter(|m| !has_1m || m.traits.context_1m);
        let thinking = pool.clone().filter(|m| m.traits.thinking).count();
        thinking > 0 && thinking < pool.clone().count()
    };

    DevinFamily {
        id,
        name,
        default,
        selectors: members.iter().map(|m| m.selector.to_string()).collect(),
        routers: members.iter().any(|m| m.entry.is_some()).then(|| {
            members
                .iter()
                .filter(|m| m.entry.is_some_and(|e| e.is_router))
                .map(|m| m.selector.to_string())
                .collect()
        }),
        levels,
        thinking_toggle,
        entry: default_member.entry.cloned(),
    }
}

fn effective_alias_opt(entry: Option<&DevinModelConfig>) -> Option<String> {
    entry.and_then(effective_alias).map(str::to_string)
}

/// Most common alias among ordinary members — the family's own name, not a
/// level lane's.
fn most_common_alias(members: &[Member]) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for member in members.iter().filter(|m| !m.traits.speed) {
        if let Some(alias) = member.entry.and_then(effective_alias) {
            *counts.entry(alias).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(alias, _)| alias.to_string())
}

/// Apply the canonical template's identity fields (name, description). Devin
/// catalog fields remain authoritative for capabilities and pricing.
fn apply_canonical_identity(metadata: &mut ProviderModelMetadata, template: &Value) {
    if let Some(name) = template.get("name").and_then(Value::as_str) {
        let name = name.trim();
        if !name.is_empty() {
            metadata.name = Some(name.to_string());
        }
    }
    if let Some(description) = template
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        metadata.description = Some(description.to_string());
    }
}

/// Build the Provider Model metadata for one folded family.
pub(crate) fn family_metadata(
    family: &DevinFamily,
    canonical: Option<&Value>,
) -> ProviderModelMetadata {
    let mut metadata = ProviderModelMetadata::bare(&family.id);
    if let Some(template) = canonical {
        apply_canonical_identity(&mut metadata, template);
    }
    if metadata.name.as_deref() == Some(family.id.as_str())
        && let Some(name) = &family.name
    {
        metadata.name = Some(name.clone());
    }
    metadata.family = Some(family.id.clone());

    let mut options = Vec::new();
    if family.levels.len() >= 2 {
        options.push(ReasoningOption::Effort {
            values: family.levels.iter().cloned().map(Some).collect(),
        });
    }
    if family.thinking_toggle {
        options.push(ReasoningOption::Toggle);
    }
    if options.is_empty() {
        // No axes — emit an empty Effort so the generated map hides the picker.
        options.push(ReasoningOption::Effort { values: Vec::new() });
    }
    metadata.reasoning_options = Some(options);

    if let Some(entry) = &family.entry {
        if entry.supports_images == Some(true)
            && let Some(modalities) = metadata.modalities.as_mut()
            && !modalities.input.iter().any(|kind| kind == "image")
        {
            modalities.input.push("image".to_string());
        }
        if let Some(context) = entry.context_window.filter(|c| *c > 0)
            && let Some(limit) = metadata.limit.as_mut()
        {
            limit.context = Some(context);
        }
        if let Some(upstream) = entry.provider.and_then(devin_upstream_provider_name) {
            metadata.extensions.insert(
                "devin_provider".to_string(),
                Value::String(upstream.to_string()),
            );
        }
        if let Some(cost) = &entry.cost {
            let price = |v: Option<f64>| v.and_then(Decimal::from_f64);
            metadata.cost = Some(ModelCost {
                prices: PriceComponents {
                    input: price(cost.input),
                    output: price(cost.output),
                    cache_read: price(cost.cache_read),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
    }

    metadata.extensions.insert(
        selector::SELECTOR_EXTENSION_KEY.to_string(),
        selector::table_extension_value(&SelectorTable {
            default: family.default.clone(),
            selectors: family.selectors.clone(),
            routers: family.routers.clone(),
        }),
    );
    metadata
}

/// Metadata for a manually added Devin model id. No catalog entry exists, so
/// the request path falls back to the legacy suffix rewrite; the empty Effort
/// keeps the picker hidden since siblings cannot be derived.
pub(crate) fn manual_model_metadata(
    model_id: &str,
    canonical: Option<&Value>,
) -> ProviderModelMetadata {
    let mut metadata = ProviderModelMetadata::bare(model_id);
    if let Some(template) = canonical {
        apply_canonical_identity(&mut metadata, template);
    }
    metadata.family = Some(selector::decompose(model_id).family.to_string());
    metadata.reasoning_options = Some(vec![ReasoningOption::Effort { values: Vec::new() }]);
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::codec::devin_connect::DevinModelConfig;

    fn entry(selector: &str, alias: Option<&str>, label: Option<&str>) -> DevinModelConfig {
        DevinModelConfig {
            label: label.map(str::to_string),
            alias: alias.map(str::to_string),
            short_alias: None,
            selector: selector.to_string(),
            provider: None,
            context_window: None,
            supports_images: None,
            is_router: false,
            cost: None,
        }
    }

    #[test]
    fn groups_selectors_into_one_family_per_alias() {
        let entries = vec![
            entry(
                "gpt-5-6-sol-medium",
                Some("gpt-5.6-sol"),
                Some("GPT-5.6 Sol Medium Thinking"),
            ),
            entry(
                "gpt-5-6-sol-high",
                Some("gpt-5.6-sol"),
                Some("GPT-5.6 Sol High Thinking"),
            ),
            entry(
                "gpt-5-6-sol-high-priority",
                Some("gpt-5.6-sol"),
                Some("GPT-5.6 Sol High Thinking"),
            ),
            entry("glm-5-2", Some("glm-5.2"), Some("GLM-5.2 High")),
            entry("glm-5-2-1m", Some("glm-5.2"), Some("GLM-5.2 High 1M")),
            entry("adaptive", None, Some("Adaptive")),
            entry("MODEL_GPT_5_2", None, Some("GPT-5.2")),
        ];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let families = group_families(&selectors, &entries);
        assert_eq!(families.len(), 2);

        let sol = families.iter().find(|f| f.id == "gpt-5.6-sol").unwrap();
        assert_eq!(sol.default, "gpt-5-6-sol-medium");
        assert_eq!(sol.levels, vec!["medium", "high"]);
        assert!(!sol.thinking_toggle);
        assert_eq!(
            sol.selectors,
            vec![
                "gpt-5-6-sol-medium",
                "gpt-5-6-sol-high",
                "gpt-5-6-sol-high-priority"
            ]
        );

        let glm = families.iter().find(|f| f.id == "glm-5.2").unwrap();
        assert_eq!(glm.default, "glm-5-2-1m");
        assert_eq!(glm.levels, vec!["high"]);
        assert_eq!(glm.name.as_deref(), Some("GLM-5.2"));
    }

    #[test]
    fn picks_canonical_medium_as_default() {
        let mut medium = entry("gpt-5-6-sol-medium", Some("gpt-5.6-sol"), None);
        medium.short_alias = Some("gpt-5p6".to_string());
        let entries = vec![
            entry("gpt-5-6-sol-low", Some("gpt-5.6-sol"), None),
            medium,
            entry("gpt-5-6-sol-high", Some("gpt-5.6-sol"), None),
        ];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let families = group_families(&selectors, &entries);
        assert_eq!(families[0].default, "gpt-5-6-sol-medium");
    }

    #[test]
    fn alias_groups_lanes_the_selector_family_would_split() {
        let entries = vec![
            entry("swe-1-7", Some("swe-1.7"), Some("SWE-1.7 Max")),
            entry(
                "swe-1-7-lightning",
                Some("swe-1.7"),
                Some("SWE-1.7 Lightning Max"),
            ),
        ];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let families = group_families(&selectors, &entries);
        // `lightning` is not a suffix flag — only the shared alias tells the
        // two selectors are one family upstream.
        assert_eq!(families.len(), 1);
        let family = &families[0];
        assert_eq!(family.id, "swe-1.7");
        assert_eq!(family.default, "swe-1-7");
        assert_eq!(family.selectors, ["swe-1-7", "swe-1-7-lightning"]);
    }

    #[test]
    fn speed_only_family_shadowed_by_ordinary_lane() {
        // Upstream lists `swe-1-6-fast` under its own alias, but with the
        // ordinary `swe-1-6` present it is only the fast lane of that family.
        let entries = vec![
            entry("swe-1-6", Some("swe-1.6"), Some("SWE-1.6")),
            entry("swe-1-6-fast", Some("swe-1.6-fast"), Some("SWE-1.6 Fast")),
        ];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let families = group_families(&selectors, &entries);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].id, "swe-1.6");
        assert_eq!(families[0].default, "swe-1-6");

        // Without the ordinary lane the fast selector is a real family.
        let entries = vec![entry(
            "swe-1-6-fast",
            Some("swe-1.6-fast"),
            Some("SWE-1.6 Fast"),
        )];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let families = group_families(&selectors, &entries);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].id, "swe-1.6-fast");
    }

    #[test]
    fn static_only_selectors_still_group() {
        let selectors = vec![
            "swe-1-6".to_string(),
            "swe-1-6-slow".to_string(),
            "glm-5-2".to_string(),
            "glm-5-2-max".to_string(),
        ];
        let families = group_families(&selectors, &[]);
        assert_eq!(families.len(), 2);
        let swe = families.iter().find(|f| f.id == "swe-1-6").unwrap();
        // slow lane exists but ordinary `swe-1-6` wins the default.
        assert_eq!(swe.default, "swe-1-6");
    }

    #[test]
    fn speed_only_family_keeps_speed_default() {
        let selectors = vec!["swe-1-6-slow".to_string()];
        let families = group_families(&selectors, &[]);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].default, "swe-1-6-slow");
    }

    #[test]
    fn metadata_carries_selector_table_and_catalog_fields() {
        let mut medium = entry("glm-5-2-1m", Some("glm-5.2"), Some("GLM-5.2 High 1M"));
        medium.context_window = Some(1_000_000);
        medium.provider = Some(9);
        medium.cost = Some(
            crate::protocol::codec::devin_connect::request::DevinModelCost {
                input: Some(1.4),
                cache_read: Some(0.26),
                output: Some(4.4),
            },
        );
        let entries = vec![
            entry("glm-5-2", Some("glm-5.2"), Some("GLM-5.2 High")),
            medium,
            entry("glm-5-2-max-1m", Some("glm-5.2"), Some("GLM-5.2 Max 1M")),
        ];
        let selectors: Vec<String> = entries.iter().map(|e| e.selector.clone()).collect();
        let family = &group_families(&selectors, &entries)[0];
        let metadata = family_metadata(family, None);

        assert_eq!(metadata.family.as_deref(), Some("glm-5.2"));
        assert_eq!(metadata.limit.and_then(|l| l.context), Some(1_000_000));
        let table = selector::table_from_extensions(&metadata.extensions).unwrap();
        assert_eq!(table.default, "glm-5-2-1m");
        let prices = metadata.cost.unwrap().prices;
        assert_eq!(prices.input, Some(Decimal::from_f64(1.4).unwrap()));
        // Levels {high(implicit), max} → Effort axis exposed.
        let has_effort = metadata
            .reasoning_options
            .unwrap()
            .iter()
            .any(|o| matches!(o, ReasoningOption::Effort { values } if values.len() == 2));
        assert!(has_effort);
    }
}
