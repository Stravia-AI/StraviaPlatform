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
    let images = match payload.get("imageGenerationModelIds") {
        Some(value) => value.as_array()?.as_slice(),
        None => &[],
    };
    let sorts = match payload.get("agentModelSorts") {
        Some(value) => value.as_array()?.as_slice(),
        None if payload.get("imageGenerationModelIds").is_some() => &[],
        None => return None,
    };
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
        // CLI DefaultEffortForBase：同家族目录默认优先，否则选 medium，再回退首档。
        let selected = default
            .and_then(|id| variants.iter().find(|variant| variant.id == id))
            .or_else(|| {
                variants
                    .iter()
                    .find(|variant| variant.effort.as_deref() == Some("medium"))
            })
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
    let mut seen_images = BTreeSet::new();
    for image in images {
        let id = image.as_str()?;
        if id.trim().is_empty() {
            continue;
        }
        let id = resolve_reroute(id, reroutes)?;
        if id.trim().is_empty() || !seen_images.insert(id) {
            continue;
        }
        let details = match entries.get(id) {
            Some(value) => Some(value.as_object()?),
            None => None,
        };
        if details
            .and_then(|details| details.get("disabled"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let mut metadata = match details {
            Some(details) => model_metadata(details)?,
            None => BTreeMap::from([(
                "modalities".into(),
                json!({"input": ["text"], "output": ["text"]}),
            )]),
        };
        // 仅相同公开身份合并能力；图片 effort ID 独立登记，推理保留原选择表。
        if let Some(model) = models.iter_mut().find(|model| model.id == id) {
            model.selector = Some(id.into());
            model.capabilities.push("media_image".into());
            model.metadata.get_mut("modalities")?["output"] = json!(["text", "image"]);
            continue;
        }
        metadata.get_mut("modalities")?["output"] = json!(["image"]);
        let name = match details.and_then(|details| details.get("displayName")) {
            Some(value) => value.as_str()?.trim(),
            None => id,
        };
        models.push(DiscoveredModel {
            id: id.into(),
            display_name: if name.is_empty() { id } else { name }.into(),
            family: Some(id.into()),
            selector: Some(id.into()),
            capabilities: vec!["media_image".into()],
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
    if let Some(value) = details.get("maxOutputTokens") {
        let number = value.as_i64().and_then(|value| i32::try_from(value).ok())?;
        if number < 0 {
            return None;
        }
        if number > 0 {
            metadata.insert("maxOutputTokens".into(), json!(number));
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
            json!({"imageGenerationModelIds": "image", "models": {}}),
            json!({"imageGenerationModelIds": [7], "models": {}}),
            json!({"imageGenerationModelIds": ["image"], "models": {"image": {"supportsImages": "true"}}}),
        ] {
            assert!(parse(&payload).is_none());
        }
    }

    #[test]
    fn family_default_prefers_medium_without_overriding_the_catalog_default() {
        let mut payload = json!({
            "defaultAgentModelId": "gemini-4-argon-medium",
            "agentModelSorts": [{"groups": [{"modelIds": [
                "gemini-4-hydrogen-high", "gemini-4-hydrogen-xhigh", "gemini-4-hydrogen-medium", "gemini-4-hydrogen-low"
            ]}]}],
            "models": {
                "gemini-4-hydrogen-high": {}, "gemini-4-hydrogen-xhigh": {},
                "gemini-4-hydrogen-medium": {}, "gemini-4-hydrogen-low": {}
            }
        });
        assert_eq!(
            parse(&payload).unwrap().models[0].selector.as_deref(),
            Some("gemini-4-hydrogen-medium")
        );
        payload["defaultAgentModelId"] = json!("gemini-4-hydrogen-low");
        assert_eq!(
            parse(&payload).unwrap().models[0].selector.as_deref(),
            Some("gemini-4-hydrogen-low")
        );
        payload["defaultAgentModelId"] = json!("gemini-4-argon-medium");
        payload["models"]
            .as_object_mut()
            .unwrap()
            .remove("gemini-4-hydrogen-medium");
        assert_eq!(
            parse(&payload).unwrap().models[0].selector.as_deref(),
            Some("gemini-4-hydrogen-high")
        );
    }

    #[test]
    fn agent_families_keep_real_selectors_alongside_image_models() {
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
            [
                "gemini-3.1-pro",
                "gemini-3.8-flash",
                "gemini-3.1-flash-image"
            ]
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
            selector::resolve(&pro.metadata[selector::EXTENSION_KEY], None)
                .unwrap()
                .id,
            "gemini-pro-agent"
        );
        let image = &result.models[2];
        assert_eq!(image.selector.as_deref(), Some("gemini-3.1-flash-image"));
        assert_eq!(image.capabilities, ["media_image"]);
        assert_eq!(
            image.metadata["modalities"],
            json!({"input": ["text"], "output": ["image"]})
        );
    }

    #[test]
    fn image_only_catalog_preserves_advertised_ids_without_details() {
        for sorts in [None, Some(json!([]))] {
            let mut payload = json!({
                "imageGenerationModelIds": ["image-low", "image-high", "missing"],
                "models": {
                    "image-low": {"displayName": "Low", "supportsImages": true, "maxTokens": 8192},
                    "image-high": {"displayName": ""}
                }
            });
            if let Some(sorts) = sorts {
                payload["agentModelSorts"] = sorts;
            }
            let result = parse(&payload).unwrap();
            assert_eq!(
                result
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>(),
                ["image-low", "image-high", "missing"]
            );
            for model in &result.models {
                assert_eq!(model.selector.as_deref(), Some(model.id.as_str()));
                assert_eq!(model.capabilities, ["media_image"]);
                assert_eq!(model.metadata["modalities"]["output"], json!(["image"]));
                assert!(!model.metadata.contains_key(selector::EXTENSION_KEY));
            }
            assert_eq!(result.models[0].display_name, "Low");
            assert_eq!(result.models[0].metadata["limit"]["context"], 8192);
            assert_eq!(
                result.models[0].metadata["modalities"]["input"],
                json!(["text", "image"])
            );
            assert_eq!(result.models[1].display_name, "image-high");
            assert_eq!(result.models[2].display_name, "missing");
            assert_eq!(
                result.models[2].metadata["modalities"]["input"],
                json!(["text"])
            );
        }
    }

    #[test]
    fn image_ids_deduplicate_reroutes_and_filter_disabled_targets() {
        let mut payload = json!({
            "imageGenerationModelIds": ["", " ", "old", "live", "old", "disabled", "retired"],
            "deprecatedModelIds": {
                "old": {"newModelId": "live"},
                "retired": {"newModelId": "disabled"}
            },
            "models": {"live": {}, "disabled": {"disabled": true}}
        });
        let result = parse(&payload).unwrap();
        assert_eq!(result.models.len(), 1);
        assert_eq!(result.models[0].id, "live");
        assert_eq!(result.models[0].selector.as_deref(), Some("live"));
        payload["deprecatedModelIds"]["live"] = json!({"newModelId": "old"});
        assert!(parse(&payload).is_none());
    }

    #[test]
    fn image_capability_merges_only_exact_agent_ids_and_keeps_infer_selectors() {
        let result = parse(&json!({
            "defaultAgentModelId": "dual-high",
            "agentModelSorts": [{"groups": [{"modelIds": ["dual-low", "dual-high", "vision"]}]}],
            "imageGenerationModelIds": ["dual", "dual-high"],
            "models": {
                "dual": {"supportsImages": true},
                "dual-low": {},
                "dual-high": {},
                "vision": {"supportsImages": true}
            }
        }))
        .unwrap();
        let dual = result
            .models
            .iter()
            .find(|model| model.id == "dual")
            .unwrap();
        assert_eq!(dual.capabilities, ["infer", "media_image"]);
        assert_eq!(dual.selector.as_deref(), Some("dual"));
        assert_eq!(
            dual.metadata["modalities"]["output"],
            json!(["text", "image"])
        );
        assert_eq!(
            selector::resolve(&dual.metadata[selector::EXTENSION_KEY], None)
                .unwrap()
                .id,
            "dual-high"
        );
        let image = result
            .models
            .iter()
            .find(|model| model.id == "dual-high")
            .unwrap();
        assert_eq!(image.capabilities, ["media_image"]);
        let vision = result
            .models
            .iter()
            .find(|model| model.id == "vision")
            .unwrap();
        assert_eq!(vision.capabilities, ["infer"]);
        assert_eq!(
            vision.metadata["modalities"],
            json!({"input": ["text", "image"], "output": ["text"]})
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
