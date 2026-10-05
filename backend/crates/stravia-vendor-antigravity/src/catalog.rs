use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{
    DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
};

use crate::selector::{self, SelectorTable, Variant};

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
) -> Result<DiscoverResponse, PluginError> {
    let project = crate::client::project(host, provider)?;
    let payload = crate::client::post_json(
        host,
        provider,
        "fetchAvailableModels",
        &json!({"project": project}),
    )?;
    parse(&payload).ok_or_else(|| {
        plugin_error(
            ErrorKind::upstream_unknown(),
            "Upstream model catalog has an unsupported shape",
        )
    })
}

fn parse(payload: &Value) -> Option<DiscoverResponse> {
    let entries = payload.get("models")?.as_object()?;
    let sorts = payload.get("agentModelSorts")?.as_array()?;
    let reroutes = match payload.get("deprecatedModelIds") {
        Some(value) => Some(value.as_object()?),
        None => None,
    };
    let default = match payload.get("defaultAgentModelId") {
        Some(value) => Some(resolve_reroute(value.as_str()?, reroutes)?),
        None => None,
    };
    let mut seen = BTreeSet::new();
    let mut families: BTreeMap<String, Vec<Variant>> = BTreeMap::new();
    for sort in sorts {
        for group in sort.get("groups")?.as_array()? {
            for id in group.get("modelIds")?.as_array()? {
                let id = resolve_reroute(id.as_str()?, reroutes)?;
                if id.trim().is_empty() {
                    return None;
                }
                if !seen.insert(id) {
                    continue;
                }
                let Some(entry) = entries.get(id) else {
                    continue;
                };
                let details = entry.as_object()?;
                if details.get("disabled").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let metadata = model_metadata(details)?;
                let slug = selector::user_facing_slug(id);
                let (family, effort) = selector::family_and_effort(slug);
                families.entry(family.into()).or_default().push(Variant {
                    id: id.into(),
                    slug: slug.into(),
                    effort: effort.map(str::to_owned),
                    metadata,
                });
            }
        }
    }
    let mut models = Vec::with_capacity(families.len());
    for (family, variants) in families {
        let selected = default
            .and_then(|id| variants.iter().find(|variant| variant.id == id))
            .unwrap_or(&variants[0]);
        let selected_id = selected.id.clone();
        let mut metadata = selected.metadata.clone();
        let details = entries.get(&selected_id)?.as_object()?;
        let name = match details.get("displayName") {
            Some(value) => value.as_str()?.trim(),
            None => &family,
        };
        let name = if name.is_empty() { &family } else { name };
        let name = if let Some(effort) = selected.effort.as_deref()
            && let Some((base, label)) = name.rsplit_once(" (")
            && label
                .strip_suffix(')')
                .is_some_and(|label| label.eq_ignore_ascii_case(effort))
        {
            base
        } else {
            name
        };
        let efforts: Vec<&str> = selector::EFFORTS
            .iter()
            .copied()
            .filter(|effort| {
                variants
                    .iter()
                    .any(|variant| variant.effort.as_deref() == Some(effort))
            })
            .collect();
        metadata.insert("reasoning_efforts".into(), json!(efforts));
        // 合并规格只承诺每档都支持的输入和上下文，原始每档规格保留在选择表。
        let context = variants.iter().try_fold(i64::MAX, |min, variant| {
            variant
                .metadata
                .get("limit")?
                .get("context")?
                .as_i64()
                .map(|value| min.min(value))
        });
        if let Some(context) = context {
            metadata.insert("limit".into(), json!({"context": context}));
        } else {
            metadata.remove("limit");
        }
        let input: Vec<&str> = ["text", "image", "video", "pdf"]
            .into_iter()
            .filter(|modality| {
                variants.iter().all(|variant| {
                    variant.metadata["modalities"]["input"]
                        .as_array()
                        .is_some_and(|values| {
                            values.iter().any(|value| value.as_str() == Some(modality))
                        })
                })
            })
            .collect();
        metadata.insert(
            "modalities".into(),
            json!({"input": input, "output": ["text"]}),
        );
        metadata.insert(
            selector::EXTENSION_KEY.into(),
            serde_json::to_value(SelectorTable {
                default: selected_id.clone(),
                variants,
            })
            .ok()?,
        );
        models.push(DiscoveredModel {
            id: family.clone(),
            display_name: name.into(),
            family: Some(family),
            selector: Some(selected_id),
            capabilities: vec!["infer".into()],
            metadata,
        });
    }
    Some(DiscoverResponse {
        models,
        next_cursor: None,
    })
}

fn resolve_reroute<'a>(
    mut id: &'a str,
    reroutes: Option<&'a serde_json::Map<String, Value>>,
) -> Option<&'a str> {
    let mut visited = BTreeSet::new();
    while let Some(reroute) = reroutes.and_then(|reroutes| reroutes.get(id)) {
        if !visited.insert(id) {
            return None;
        }
        id = reroute.get("newModelId")?.as_str()?;
    }
    Some(id)
}

fn model_metadata(details: &serde_json::Map<String, Value>) -> Option<BTreeMap<String, Value>> {
    for field in [
        "disabled",
        "isInternal",
        "supportsImages",
        "supportsVideo",
        "supportsPdf",
        "supportsThinking",
    ] {
        if details.get(field).is_some_and(|value| !value.is_boolean()) {
            return None;
        }
    }
    let mut metadata = BTreeMap::new();
    let mut input = vec!["text"];
    for (field, modality) in [
        ("supportsImages", "image"),
        ("supportsVideo", "video"),
        ("supportsPdf", "pdf"),
    ] {
        if details.get(field).and_then(Value::as_bool) == Some(true) {
            input.push(modality);
        }
    }
    metadata.insert(
        "modalities".into(),
        json!({"input": input, "output": ["text"]}),
    );
    if let Some(value) = details.get("maxTokens") {
        let number = value.as_i64()?;
        if number < 0 {
            return None;
        }
        if number > 0 {
            metadata.insert("limit".into(), json!({"context": number}));
        }
    }
    // 标量预算不能证明 low/medium/high 等离散 effort，保持上游声明而不编造范围。
    if let Some(thinking) = details.get("supportsThinking") {
        metadata.insert("supportsThinking".into(), thinking.clone());
        for field in ["thinkingBudget", "minThinkingBudget", "thinkingLevel"] {
            if let Some(value) = details.get(field) {
                value.as_i64()?;
                metadata.insert(field.into(), value.clone());
            }
        }
    }
    Some(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_account_keys_and_filters_unavailable_models() {
        let result = parse(&json!({"agentModelSorts": [{"groups": [{"modelIds": ["live-account-id", "disabled"]}]}], "models": {
            "live-account-id": {"model": "MODEL_ENUM_NAME", "displayName": "Account model", "supportsImages": true, "supportsPdf": true, "supportsVideo": true, "supportsThinking": true, "thinkingBudget": 2048, "maxTokens": 131072},
            "disabled": {"disabled": true}, "internal": {"isInternal": true}
        }})).unwrap();
        assert_eq!(result.models.len(), 1);
        let model = &result.models[0];
        assert_eq!(model.id, "live-account-id");
        assert_eq!(
            model.metadata["modalities"],
            json!({"input": ["text", "image", "video", "pdf"], "output": ["text"]})
        );
        assert_eq!(model.metadata["limit"], json!({"context": 131072}));
        assert_eq!(model.metadata["supportsThinking"], true);
    }

    #[test]
    fn rejects_malformed_catalog_without_guessing_models() {
        for payload in [
            json!({}),
            json!({"models": []}),
            json!({"agentModelSorts": [{"groups": [{"modelIds": ["x"]}]}], "models": {"x": {"disabled": "false"}}}),
            json!({"agentModelSorts": [{"groups": [{"modelIds": ["x"]}]}], "models": {"x": {"maxTokens": -1}}}),
            json!({"agentModelSorts": [{"groups": [{"modelIds": [7]}]}], "models": {}}),
        ] {
            assert!(parse(&payload).is_none());
        }
    }

    #[test]
    fn agent_families_keep_real_selectors_and_exclude_other_uses() {
        let payload = json!({
            "defaultAgentModelId": "gemini-3.8-flash-low",
            "agentModelSorts": [{"groups": [{"modelIds": [
                "gemini-3.8-flash-high", "gemini-3.8-flash-low",
                "gemini-pro-agent", "gemini-3.1-pro-low", "disabled"
            ]}, {"modelIds": ["gemini-3.8-flash-high"]}]}],
            "deprecatedModelIds": {
                "gemini-3.1-pro-high": {"newModelId": "gemini-pro-agent"}
            },
            "imageGenerationModelIds": ["gemini-3.1-flash-image"],
            "models": {
                "gemini-3.8-flash-high": {"displayName": "Gemini 3.8 Flash (High)", "supportsImages": true, "maxTokens": 1048576},
                "gemini-3.8-flash-low": {"displayName": "Gemini 3.8 Flash (Low)", "maxTokens": 131072},
                "gemini-pro-agent": {"displayName": "Gemini 3.1 Pro (High)", "model": "MODEL_M16"},
                "gemini-3.1-pro-low": {"displayName": "Gemini 3.1 Pro (Low)"},
                "gemini-3.1-pro-high": {"displayName": "Gemini 3.1 Pro (High)", "model": "MODEL_M37"},
                "gemini-3.1-flash-image": {"displayName": "Gemini 3.1 Flash Image"},
                "gemini-2.5-pro": {"displayName": "Gemini 2.5 Pro"},
                "disabled": {"disabled": true}
            }
        });
        let result = parse(&payload).unwrap();
        assert_eq!(
            result
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gemini-3.1-pro", "gemini-3.8-flash"]
        );
        let flash = &result.models[1];
        assert_eq!(flash.display_name, "Gemini 3.8 Flash");
        assert_eq!(flash.selector.as_deref(), Some("gemini-3.8-flash-low"));
        assert_eq!(flash.metadata["reasoning_efforts"], json!(["low", "high"]));
        assert_eq!(flash.metadata["limit"]["context"], 131072);
        assert_eq!(flash.metadata["modalities"]["input"], json!(["text"]));
        let table = &flash.metadata[selector::EXTENSION_KEY];
        assert_eq!(
            table["variants"][0]["metadata"]["limit"]["context"],
            1048576
        );
        let pro = &result.models[0];
        assert_eq!(pro.display_name, "Gemini 3.1 Pro");
        assert_eq!(pro.selector.as_deref(), Some("gemini-pro-agent"));
        assert_eq!(
            selector::resolve(&pro.metadata[selector::EXTENSION_KEY], None).unwrap(),
            "gemini-pro-agent"
        );
    }

    #[test]
    fn reroutes_agent_references_and_rejects_cycles() {
        let mut payload = json!({
            "agentModelSorts": [{"groups": [{"modelIds": ["old", "live"]}]}],
            "deprecatedModelIds": {"old": {"newModelId": "live"}},
            "models": {"old": {}, "live": {"displayName": "Live"}}
        });
        let result = parse(&payload).unwrap();
        assert_eq!(
            result
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["live"]
        );
        payload["deprecatedModelIds"]["live"] = json!({"newModelId": "old"});
        assert!(parse(&payload).is_none());
    }

    #[test]
    fn identical_names_do_not_merge_unrelated_identities() {
        let result = parse(&json!({
            "agentModelSorts": [{"groups": [{"modelIds": ["first", "second"]}]}],
            "models": {
                "first": {"displayName": "Same", "model": "SAME_ENUM"},
                "second": {"displayName": "Same", "model": "SAME_ENUM"}
            }
        }))
        .unwrap();
        assert_eq!(
            result
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }
}
