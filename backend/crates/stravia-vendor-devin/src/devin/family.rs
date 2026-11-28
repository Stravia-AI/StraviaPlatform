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
    pub efforts: Vec<String>,
    pub effort_selectors: BTreeMap<String, String>,
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

fn member_traits(selector: &str, entry: Option<&DevinModelConfig>) -> SelectorTraits {
    let mut traits = selector::traits(selector);
    if let Some(entry) = entry {
        traits.speed |= entry.fast_mode;
        traits.context_1m |= entry.context_1m;
        if let Some(thinking) = entry.thinking_toggle {
            traits.thinking = thinking;
        }
        if let Some(level) = &entry.effort_level {
            traits.level = Some(level.clone());
        } else if traits.level.is_none()
            && !traits.speed
            && let Some(level) = entry.label.as_deref().and_then(selector::level_from_label)
        {
            traits.level = Some(level);
        }
    }
    traits
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
    let family_default_level = members
        .iter()
        .find(|member| member.entry.is_some_and(|entry| entry.is_default_in_family))
        .and_then(|member| member.traits.level.as_deref());
    candidates
        .iter()
        .find(|member| member.entry.is_some_and(|entry| entry.is_default_in_family))
        .or_else(|| {
            family_default_level.and_then(|level| {
                candidates
                    .iter()
                    .find(|member| member.traits.level.as_deref() == Some(level))
            })
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
            traits: member_traits(selector, entry),
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

fn pick_best_member<'a>(
    members: &[Member<'a>],
    default_thinking: bool,
    has_1m: bool,
    predicate: impl Fn(&Member<'a>) -> bool,
) -> Option<&'a str> {
    let rank = |member: &Member<'a>| {
        (
            (has_1m && !member.traits.context_1m) as u8,
            member.traits.speed as u8,
            (member.traits.thinking != default_thinking) as u8,
            !member.entry.is_some_and(|entry| entry.is_default_in_family) as u8,
        )
    };
    members
        .iter()
        .filter(|member| predicate(member))
        .min_by(|left, right| {
            rank(left)
                .cmp(&rank(right))
                .then_with(|| left.selector.cmp(right.selector))
        })
        .map(|member| member.selector)
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
    let name = default_member
        .entry
        .and_then(|entry| entry.family_label.clone())
        .or_else(|| {
            members
                .iter()
                .find_map(|member| member.entry.and_then(|entry| entry.family_label.clone()))
        })
        .or_else(|| {
            members
                .iter()
                .filter_map(|member| member.entry.and_then(|entry| entry.label.as_deref()))
                .find_map(selector::family_name_from_label)
        });
    let default_entry = default_member.entry.cloned();
    let default_thinking = default_member.traits.thinking;
    let has_1m = members.iter().any(|member| member.traits.context_1m);

    let mut levels = BTreeSet::new();
    for member in &members {
        if let Some(level) = &member.traits.level {
            levels.insert(level.clone());
        }
    }
    let ordered_levels = selector::order_levels(levels.iter().map(String::as_str));
    let has_thinking = members.iter().any(|member| member.traits.thinking);
    let has_non_thinking = members.iter().any(|member| !member.traits.thinking);

    let mut effort_selectors = BTreeMap::new();
    let efforts = if ordered_levels.len() >= 2 {
        for level in &ordered_levels {
            if let Some(target) = pick_best_member(&members, default_thinking, has_1m, |member| {
                member.traits.level.as_deref() == Some(level.as_str())
            }) {
                effort_selectors.insert(level.clone(), target.to_string());
            }
        }
        ordered_levels
    } else if has_thinking && has_non_thinking {
        if let Some(off) =
            pick_best_member(&members, false, has_1m, |member| !member.traits.thinking)
        {
            effort_selectors.insert("none".to_string(), off.to_string());
        }
        if let Some(on) = pick_best_member(&members, true, has_1m, |member| member.traits.thinking)
        {
            effort_selectors.insert("high".to_string(), on.to_string());
        }
        vec!["none".to_string(), "high".to_string()]
    } else {
        Vec::new()
    };

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
        efforts,
        effort_selectors,
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
        effort_selectors: family.effort_selectors.clone(),
    };
    let mut metadata = BTreeMap::new();
    metadata.insert(
        selector::SELECTOR_EXTENSION_KEY.to_string(),
        selector::table_extension_value(&table),
    );
    if !family.efforts.is_empty() || family.entry.is_some() {
        metadata.insert("reasoning_efforts".into(), json!(family.efforts));
    }

    let mut capabilities = vec!["infer".to_string()];
    if let Some(entry) = &family.entry {
        if entry.supports_tool_calls != Some(false) {
            capabilities.push("tools".into());
        }
        if entry.supports_thinking != Some(false) || !family.efforts.is_empty() {
            capabilities.push("reasoning".into());
        }
        match entry.supports_images {
            Some(true) => {
                metadata.insert(
                    "modalities".into(),
                    json!({"input": ["text", "image"], "output": ["text"]}),
                );
                capabilities.push("image_input".into());
            }
            Some(false) => {
                metadata.insert(
                    "modalities".into(),
                    json!({"input": ["text"], "output": ["text"]}),
                );
            }
            None => {}
        }
        if let Some(context_window) = entry.context_window.filter(|value| *value > 0) {
            metadata.insert("context_window".into(), json!(context_window));
        }
        if let Some(max_output_tokens) = entry.max_output_tokens.filter(|value| *value > 0) {
            metadata.insert("max_output_tokens".into(), json!(max_output_tokens));
        }
        if let Some(provider) = entry.provider.and_then(devin_upstream_provider_name) {
            metadata.insert(
                "upstream_provider".into(),
                Value::String(provider.to_string()),
            );
        }
        // 渠道协议自己的报价（USD／百万 Token）；宿主以标准 `cost` 采用它，优先于目录价与官方参考价。
        if let Some(cost) = entry.cost {
            let prices: serde_json::Map<String, Value> = [
                ("input", cost.input),
                ("output", cost.output),
                ("cache_read", cost.cache_read),
            ]
            .into_iter()
            .filter_map(|(field, price)| Some((field.to_owned(), json!(price?))))
            .collect();
            metadata.insert("cost".into(), Value::Object(prices));
        }
    } else {
        capabilities.push("tools".into());
        capabilities.push("reasoning".into());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::devin_connect::proto::{
        write_fixed32_field, write_message_field, write_string_field, write_varint_field,
    };
    use crate::codec::devin_connect::{DevinModelCost, decode_cli_model_configs};
    use stravia_runtime_contract::thinking::TargetThinkingControl;

    fn discover(selectors: &[&str], supports_images: Option<bool>) -> DiscoveredModel {
        let entries: Vec<_> = selectors
            .iter()
            .map(|selector| DevinModelConfig {
                selector: (*selector).into(),
                alias: Some("test-family".into()),
                supports_images,
                ..Default::default()
            })
            .collect();
        let selectors = entries
            .iter()
            .map(|entry| entry.selector.clone())
            .collect::<Vec<_>>();
        discovered_model(&group_families(&selectors, &entries)[0])
    }

    type FamilyMetaSpec<'a> = (&'a str, Option<u64>, Option<&'a str>, Option<u64>);

    struct ProtoEntrySpec<'a> {
        selector: &'a str,
        label: &'a str,
        alias: Option<&'a str>,
        provider: u64,
        disabled: bool,
        top_images: Option<bool>,
        feat_images: Option<bool>,
        top_max_tokens: Option<u64>,
        info_max_tokens: Option<u64>,
        max_output_tokens: Option<u64>,
        family_label: Option<&'a str>,
        family_meta: &'a [FamilyMetaSpec<'a>],
        is_default: bool,
        prices: &'a [(&'a str, f32)],
    }

    fn encode_proto_entry(spec: &ProtoEntrySpec<'_>) -> Vec<u8> {
        let mut features = Vec::new();
        if spec.feat_images == Some(true) {
            write_varint_field(&mut features, 11, 1);
        }
        write_varint_field(&mut features, 12, 1);
        write_varint_field(&mut features, 15, 1);

        let mut info = Vec::new();
        if let Some(max_tokens) = spec.info_max_tokens {
            write_varint_field(&mut info, 4, max_tokens);
        }
        write_message_field(&mut info, 6, &features);
        if let Some(max_out) = spec.max_output_tokens {
            write_varint_field(&mut info, 13, max_out);
        }
        if let Some(alias) = spec.alias {
            write_string_field(&mut info, 23, alias);
        }

        let mut config = Vec::new();
        write_string_field(&mut config, 1, spec.label);
        if spec.disabled {
            write_varint_field(&mut config, 4, 1);
        }
        if spec.top_images == Some(true) {
            write_varint_field(&mut config, 5, 1);
        }
        write_varint_field(&mut config, 10, spec.provider);
        if let Some(max_tokens) = spec.top_max_tokens {
            write_varint_field(&mut config, 18, max_tokens);
        }
        write_string_field(&mut config, 22, spec.selector);
        write_message_field(&mut config, 23, &info);

        if spec.family_label.is_some() || !spec.family_meta.is_empty() {
            let mut meta = Vec::new();
            if let Some(label) = spec.family_label {
                write_string_field(&mut meta, 1, label);
            }
            for &(key, order, display, control_type) in spec.family_meta {
                let mut val = Vec::new();
                if let Some(order) = order {
                    write_varint_field(&mut val, 1, order);
                }
                if let Some(display) = display {
                    write_string_field(&mut val, 2, display);
                }
                if let Some(control_type) = control_type {
                    write_varint_field(&mut val, 3, control_type);
                }
                let mut pair = Vec::new();
                write_string_field(&mut pair, 1, key);
                write_message_field(&mut pair, 2, &val);
                write_message_field(&mut meta, 2, &pair);
            }
            write_message_field(&mut config, 30, &meta);
        }
        if spec.is_default {
            write_varint_field(&mut config, 31, 1);
        }
        for &(name, price) in spec.prices {
            let mut row = Vec::new();
            write_string_field(&mut row, 1, name);
            write_fixed32_field(&mut row, 2, price);
            write_string_field(&mut row, 3, "1M tokens");
            write_varint_field(&mut row, 6, 1);
            write_message_field(&mut config, 32, &row);
        }
        let mut wrapped = Vec::new();
        write_message_field(&mut wrapped, 1, &config);
        wrapped
    }

    fn discover_from_proto(wire: &[u8]) -> BTreeMap<String, DiscoveredModel> {
        let entries = decode_cli_model_configs(wire).expect("valid GetCliModelConfigs protobuf");
        let selectors: Vec<String> = entries.iter().map(|entry| entry.selector.clone()).collect();
        group_families(&selectors, &entries)
            .iter()
            .map(discovered_model)
            .map(|model| (model.id.clone(), model))
            .collect()
    }

    #[test]
    fn discovery_declares_text_and_optional_image_modalities() {
        let with_images = discover(&["test-family"], Some(true));
        assert_eq!(
            with_images.metadata.get("modalities"),
            Some(&json!({"input": ["text", "image"], "output": ["text"]}))
        );
        assert!(
            with_images
                .capabilities
                .contains(&"image_input".to_string())
        );

        let text_only = discover(&["test-family"], Some(false));
        assert_eq!(
            text_only.metadata.get("modalities"),
            Some(&json!({"input": ["text"], "output": ["text"]}))
        );
        assert!(!text_only.capabilities.contains(&"image_input".to_string()));

        // Unknown modality omits `modalities` so canonical catalog metadata is
        // never clobbered with a fabricated text-only declaration.
        let unspecified = discover(&["test-family"], None);
        assert_eq!(unspecified.metadata.get("modalities"), None);
    }

    #[test]
    fn discovery_exposes_ordered_efforts() {
        let model = discover(&["swe-2-high", "swe-2-medium", "swe-2-max"], None);
        assert_eq!(
            model.metadata.get("reasoning_efforts"),
            Some(&json!(["medium", "high", "max"]))
        );
    }

    #[test]
    fn discovery_exposes_canonical_off_and_high_for_toggle_family() {
        let model = discover(&["test-family", "test-family-thinking"], None);
        assert_eq!(
            model.metadata.get("reasoning_efforts"),
            Some(&json!(["none", "high"]))
        );
    }

    #[test]
    fn discovery_exposes_only_the_quoted_prices_as_model_cost() {
        let entry = |cost| DevinModelConfig {
            selector: "test-family".into(),
            alias: Some("test-family".into()),
            cost,
            ..Default::default()
        };
        let quote = |cost| {
            let entry = entry(cost);
            let selectors = vec![entry.selector.clone()];
            discovered_model(&group_families(&selectors, &[entry])[0])
        };
        let quoted = quote(Some(DevinModelCost {
            input: Some(0.15),
            output: Some(0.6),
            cache_read: None,
        }));
        assert_eq!(
            quoted.metadata.get("cost"),
            Some(&json!({"input": 0.15, "output": 0.6}))
        );
        assert_eq!(quote(None).metadata.get("cost"), None);
    }

    #[test]
    fn discovery_hides_picker_for_fixed_effort() {
        let model = discover(&["test-family-high"], None);
        assert_eq!(model.metadata.get("reasoning_efforts"), Some(&json!([])));
    }

    #[test]
    fn real_free_account_protobuf_drops_disabled_and_pseudo_models_with_full_metadata() {
        // Mirrors live 3000.15.14 free-tier GetCliModelConfigs: only swe-1-6-slow,
        // swe-2-flash-medium, and internal pseudo-selectors are enabled; flagship
        // models are flagged #4 disabled=1.
        let wire = [
            encode_proto_entry(&ProtoEntrySpec {
                selector: "swe-1-6-slow",
                label: "SWE-1.6 (Promo)",
                alias: Some("swe-1.6-slow"),
                provider: 1,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(200_000),
                info_max_tokens: Some(200_000),
                max_output_tokens: Some(32_768),
                family_label: None,
                family_meta: &[],
                is_default: false,
                prices: &[("Input", 0.5), ("Cached input", 0.2), ("Output", 2.5)],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "swe-2-flash-medium",
                label: "SWE-2 Flash Medium",
                alias: Some("swe-2-flash"),
                provider: 1,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: Some(256_000),
                info_max_tokens: Some(262_144),
                max_output_tokens: Some(65_536),
                family_label: Some("SWE-2 Flash"),
                family_meta: &[("Reasoning Effort", None, Some("Medium"), Some(1))],
                is_default: true,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "subagent-default",
                label: "Subagent Default",
                alias: None,
                provider: 1,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: Some(200_000),
                info_max_tokens: Some(200_000),
                max_output_tokens: None,
                family_label: None,
                family_meta: &[],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-opus-5-medium",
                label: "Claude Opus 5 Medium",
                alias: Some("claude-opus-5"),
                provider: 3,
                disabled: true,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: None,
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("Claude Opus 5"),
                family_meta: &[("Effort", Some(1), Some("Medium"), Some(1))],
                is_default: true,
                prices: &[("Input", 5.0), ("Cached input", 0.5), ("Output", 25.0)],
            }),
        ]
        .concat();

        let models = discover_from_proto(&wire);
        assert_eq!(
            models.keys().cloned().collect::<Vec<_>>(),
            vec!["swe-1.6-slow", "swe-2-flash"]
        );

        let swe16 = &models["swe-1.6-slow"];
        assert_eq!(swe16.selector.as_deref(), Some("swe-1-6-slow"));
        assert_eq!(swe16.metadata.get("context_window"), Some(&json!(200_000)));
        assert_eq!(
            swe16.metadata.get("max_output_tokens"),
            Some(&json!(32_768))
        );
        assert_eq!(
            swe16.metadata.get("upstream_provider"),
            Some(&json!("cognition"))
        );
        assert_eq!(
            swe16.metadata.get("modalities"),
            Some(&json!({"input": ["text", "image"], "output": ["text"]}))
        );
        assert!(swe16.capabilities.contains(&"image_input".to_string()));
        assert_eq!(
            swe16.metadata.get("cost"),
            Some(&json!({"input": 0.5, "cache_read": 0.2, "output": 2.5}))
        );
        assert_eq!(swe16.metadata.get("reasoning_efforts"), Some(&json!([])));

        let flash = &models["swe-2-flash"];
        assert_eq!(flash.display_name, "SWE-2 Flash");
        assert_eq!(flash.selector.as_deref(), Some("swe-2-flash-medium"));
        assert_eq!(flash.metadata.get("context_window"), Some(&json!(256_000)));
        assert_eq!(
            flash.metadata.get("max_output_tokens"),
            Some(&json!(65_536))
        );
        assert_eq!(
            flash.metadata.get("modalities"),
            Some(&json!({"input": ["text"], "output": ["text"]}))
        );
        assert!(!flash.capabilities.contains(&"image_input".to_string()));
        assert_eq!(flash.metadata.get("cost"), None);
        assert_eq!(flash.metadata.get("reasoning_efforts"), Some(&json!([])));
    }

    #[test]
    fn real_multi_family_protobuf_parses_images_efforts_defaults_and_selectors() {
        // Mirrors real 3000.15.14 GetCliModelConfigs payloads across Claude, GLM,
        // SWE, and Nemotron families (including unsuffixed effort selectors,
        // ModelFeatures.#11 image flags, 1M pools, #31 default markers, and
        // lightning speed flags vs model names containing "lightning").
        let wire = [
            // Claude Opus 5: wire-ordered low..max, #31 default=medium, ModelFeatures.#11=1
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-opus-5-low",
                label: "Claude Opus 5 Low",
                alias: Some("claude-opus-5"),
                provider: 3,
                disabled: false,
                top_images: None,
                feat_images: Some(true),
                top_max_tokens: None,
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("Claude Opus 5"),
                family_meta: &[("Effort", None, Some("Low"), Some(1))],
                is_default: false,
                prices: &[("Input", 5.0), ("Cached input", 0.5), ("Output", 25.0)],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-opus-5-medium",
                label: "Claude Opus 5 Medium",
                alias: Some("claude-opus-5"),
                provider: 3,
                disabled: false,
                top_images: None,
                feat_images: Some(true),
                top_max_tokens: None,
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("Claude Opus 5"),
                family_meta: &[("Effort", Some(1), Some("Medium"), Some(1))],
                is_default: true,
                prices: &[("Input", 5.0), ("Cached input", 0.5), ("Output", 25.0)],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-opus-5-max",
                label: "Claude Opus 5 Max",
                alias: Some("claude-opus-5"),
                provider: 3,
                disabled: false,
                top_images: None,
                feat_images: Some(true),
                top_max_tokens: None,
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("Claude Opus 5"),
                family_meta: &[("Effort", Some(4), Some("Max"), Some(1))],
                is_default: false,
                prices: &[("Input", 5.0), ("Cached input", 0.5), ("Output", 25.0)],
            }),
            // Claude Sonnet 4.6: Thinking toggle + 1M Context pool
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-sonnet-4-6",
                label: "Claude Sonnet 4.6",
                alias: Some("claude-sonnet-4.6"),
                provider: 3,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(200_000),
                info_max_tokens: Some(200_000),
                max_output_tokens: Some(64_000),
                family_label: Some("Claude Sonnet 4.6"),
                family_meta: &[
                    ("Effort", Some(1), Some("High"), Some(1)),
                    ("Thinking", None, None, Some(2)),
                    ("1M Context", None, None, Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-sonnet-4-6-thinking",
                label: "Claude Sonnet 4.6 Thinking",
                alias: Some("claude-sonnet-4.6"),
                provider: 3,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(200_000),
                info_max_tokens: Some(200_000),
                max_output_tokens: Some(64_000),
                family_label: Some("Claude Sonnet 4.6"),
                family_meta: &[
                    ("Effort", Some(1), Some("High"), Some(1)),
                    ("Thinking", Some(1), None, Some(2)),
                    ("1M Context", None, None, Some(2)),
                ],
                is_default: true,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-sonnet-4-6-1m",
                label: "Claude Sonnet 4.6 1M",
                alias: Some("claude-sonnet-4.6"),
                provider: 3,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(1_000_000),
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(64_000),
                family_label: Some("Claude Sonnet 4.6"),
                family_meta: &[
                    ("Effort", Some(1), Some("High"), Some(1)),
                    ("Thinking", None, None, Some(2)),
                    ("1M Context", Some(1), None, Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "claude-sonnet-4-6-thinking-1m",
                label: "Claude Sonnet 4.6 Thinking 1M",
                alias: Some("claude-sonnet-4.6"),
                provider: 3,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(1_000_000),
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(64_000),
                family_label: Some("Claude Sonnet 4.6"),
                family_meta: &[
                    ("Effort", Some(1), Some("High"), Some(1)),
                    ("Thinking", Some(1), None, Some(2)),
                    ("1M Context", Some(1), None, Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            // GLM-5.2: unsuffixed High selector (`glm-5-2` / `glm-5-2-1m`), text-only
            encode_proto_entry(&ProtoEntrySpec {
                selector: "glm-5-2",
                label: "GLM-5.2 High",
                alias: Some("glm-5.2"),
                provider: 9,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: Some(200_000),
                info_max_tokens: Some(200_000),
                max_output_tokens: Some(128_000),
                family_label: Some("GLM-5.2"),
                family_meta: &[
                    ("Effort", Some(1), Some("High"), Some(1)),
                    ("1M Context", None, None, Some(2)),
                ],
                is_default: true,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "glm-5-2-none-1m",
                label: "GLM-5.2 No Thinking 1M",
                alias: Some("glm-5.2"),
                provider: 9,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: Some(1_000_000),
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("GLM-5.2"),
                family_meta: &[
                    ("Effort", None, Some("No Thinking"), Some(1)),
                    ("1M Context", Some(1), None, Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "glm-5-2-1m",
                label: "GLM-5.2 High 1M",
                alias: Some("glm-5.2"),
                provider: 9,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: Some(1_000_000),
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("GLM-5.2"),
                family_meta: &[
                    ("Effort", Some(1), Some("High"), Some(1)),
                    ("1M Context", Some(1), None, Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "glm-5-2-max-1m",
                label: "GLM-5.2 Max 1M",
                alias: Some("glm-5.2"),
                provider: 9,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: Some(1_000_000),
                info_max_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
                family_label: Some("GLM-5.2"),
                family_meta: &[
                    ("Effort", Some(2), Some("Max"), Some(1)),
                    ("1M Context", Some(1), None, Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            // SWE-2: #31 default is `high`, plus `-lightning` speed variant
            encode_proto_entry(&ProtoEntrySpec {
                selector: "swe-2-medium",
                label: "SWE-2 Medium",
                alias: Some("swe-2"),
                provider: 1,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(262_144),
                info_max_tokens: Some(262_144),
                max_output_tokens: Some(128_000),
                family_label: Some("SWE-2"),
                family_meta: &[("Reasoning Effort", None, Some("Medium"), Some(1))],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "swe-2-high",
                label: "SWE-2 High",
                alias: Some("swe-2"),
                provider: 1,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(262_144),
                info_max_tokens: Some(262_144),
                max_output_tokens: Some(128_000),
                family_label: Some("SWE-2"),
                family_meta: &[("Reasoning Effort", Some(1), Some("High"), Some(1))],
                is_default: true,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "swe-2-high-lightning",
                label: "SWE-2 High Lightning",
                alias: Some("swe-2"),
                provider: 1,
                disabled: false,
                top_images: Some(true),
                feat_images: Some(true),
                top_max_tokens: Some(262_144),
                info_max_tokens: Some(262_144),
                max_output_tokens: Some(128_000),
                family_label: Some("SWE-2"),
                family_meta: &[
                    ("Reasoning Effort", Some(1), Some("High"), Some(1)),
                    ("Fast Mode", Some(1), Some("Ultrafast"), Some(2)),
                ],
                is_default: false,
                prices: &[],
            }),
            // Nemotron 3.5 Lightning: model name contains "lightning", must not be stripped
            encode_proto_entry(&ProtoEntrySpec {
                selector: "nemotron-3-5-lightning-none",
                label: "Nemotron 3.5 Lightning None",
                alias: Some("nemotron-3-5-lightning"),
                provider: 11,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: None,
                info_max_tokens: Some(1_048_576),
                max_output_tokens: Some(65_536),
                family_label: Some("Nemotron 3.5 Lightning"),
                family_meta: &[("Reasoning Effort", None, Some("None"), Some(1))],
                is_default: false,
                prices: &[],
            }),
            encode_proto_entry(&ProtoEntrySpec {
                selector: "nemotron-3-5-lightning-high",
                label: "Nemotron 3.5 Lightning High",
                alias: Some("nemotron-3-5-lightning"),
                provider: 11,
                disabled: false,
                top_images: None,
                feat_images: None,
                top_max_tokens: None,
                info_max_tokens: Some(1_048_576),
                max_output_tokens: Some(65_536),
                family_label: Some("Nemotron 3.5 Lightning"),
                family_meta: &[("Reasoning Effort", Some(2), Some("High"), Some(1))],
                is_default: true,
                prices: &[],
            }),
        ]
        .concat();

        let models = discover_from_proto(&wire);

        // Claude Opus 5
        let opus5 = &models["claude-opus-5"];
        assert_eq!(opus5.display_name, "Claude Opus 5");
        assert_eq!(opus5.selector.as_deref(), Some("claude-opus-5-medium"));
        assert_eq!(
            opus5.metadata.get("context_window"),
            Some(&json!(1_000_000))
        );
        assert_eq!(
            opus5.metadata.get("max_output_tokens"),
            Some(&json!(128_000))
        );
        assert_eq!(
            opus5.metadata.get("upstream_provider"),
            Some(&json!("anthropic"))
        );
        assert_eq!(
            opus5.metadata.get("modalities"),
            Some(&json!({"input": ["text", "image"], "output": ["text"]}))
        );
        assert_eq!(
            opus5.metadata.get("reasoning_efforts"),
            Some(&json!(["low", "medium", "max"]))
        );

        // Claude Sonnet 4.6 toggle family
        let sonnet46 = &models["claude-sonnet-4.6"];
        assert_eq!(sonnet46.selector.as_deref(), Some("claude-sonnet-4-6-1m"));
        assert_eq!(
            sonnet46.metadata.get("reasoning_efforts"),
            Some(&json!(["none", "high"]))
        );
        let sonnet46_table = selector::table_from_extensions(&sonnet46.metadata).unwrap();
        assert_eq!(
            selector::resolve_selector(
                &sonnet46_table,
                Some(&TargetThinkingControl::Effort {
                    value: "none".into()
                }),
            ),
            "claude-sonnet-4-6-1m"
        );
        assert_eq!(
            selector::resolve_selector(
                &sonnet46_table,
                Some(&TargetThinkingControl::Effort {
                    value: "high".into()
                }),
            ),
            "claude-sonnet-4-6-thinking-1m"
        );

        // GLM-5.2 unsuffixed High selector in 1M pool
        let glm = &models["glm-5.2"];
        assert_eq!(glm.selector.as_deref(), Some("glm-5-2-1m"));
        assert_eq!(glm.metadata.get("upstream_provider"), Some(&json!("zhipu")));
        assert_eq!(
            glm.metadata.get("modalities"),
            Some(&json!({"input": ["text"], "output": ["text"]}))
        );
        assert_eq!(
            glm.metadata.get("reasoning_efforts"),
            Some(&json!(["none", "high", "max"]))
        );
        let glm_table = selector::table_from_extensions(&glm.metadata).unwrap();
        assert_eq!(
            selector::resolve_selector(
                &glm_table,
                Some(&TargetThinkingControl::Effort {
                    value: "high".into()
                }),
            ),
            "glm-5-2-1m"
        );
        assert_eq!(
            selector::resolve_selector(
                &glm_table,
                Some(&TargetThinkingControl::Effort {
                    value: "none".into()
                }),
            ),
            "glm-5-2-none-1m"
        );
        assert_eq!(
            selector::resolve_selector(
                &glm_table,
                Some(&TargetThinkingControl::Effort {
                    value: "max".into()
                }),
            ),
            "glm-5-2-max-1m"
        );

        // SWE-2 honors #31 default = high and avoids lightning speed variant
        let swe2 = &models["swe-2"];
        assert_eq!(swe2.selector.as_deref(), Some("swe-2-high"));
        assert_eq!(
            swe2.metadata.get("reasoning_efforts"),
            Some(&json!(["medium", "high"]))
        );
        let swe2_table = selector::table_from_extensions(&swe2.metadata).unwrap();
        assert_eq!(
            selector::resolve_selector(
                &swe2_table,
                Some(&TargetThinkingControl::Effort {
                    value: "high".into()
                }),
            ),
            "swe-2-high"
        );

        // Nemotron 3.5 Lightning keeps its name and efforts
        let nemotron = &models["nemotron-3-5-lightning"];
        assert_eq!(nemotron.display_name, "Nemotron 3.5 Lightning");
        assert_eq!(
            nemotron.selector.as_deref(),
            Some("nemotron-3-5-lightning-high")
        );
        assert_eq!(
            nemotron.metadata.get("upstream_provider"),
            Some(&json!("nvidia"))
        );
        assert_eq!(
            nemotron.metadata.get("context_window"),
            Some(&json!(1_048_576))
        );
        assert_eq!(
            nemotron.metadata.get("reasoning_efforts"),
            Some(&json!(["none", "high"]))
        );
    }
}
