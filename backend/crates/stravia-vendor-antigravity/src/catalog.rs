use std::collections::BTreeMap;

use serde_json::{Value, json};
use stravia_vendor_common::common::plugin_error;
use stravia_vendor_sdk::{
    DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
};

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
    let mut models = Vec::new();
    for (id, entry) in entries {
        let details = entry.as_object()?;
        if id.trim().is_empty() {
            return None;
        }
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
        if details.get("disabled").and_then(Value::as_bool) == Some(true)
            || details.get("isInternal").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        // ModelDetails.model 是 enum，实际请求 ID 来自 models map key。
        let display_name = match details.get("displayName") {
            Some(value) => value.as_str()?.trim(),
            None => id,
        };
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
        models.push(DiscoveredModel {
            id: id.clone(),
            display_name: if display_name.is_empty() {
                id.clone()
            } else {
                display_name.into()
            },
            family: None,
            selector: None,
            capabilities: vec!["infer".into()],
            metadata,
        });
    }
    Some(DiscoverResponse {
        models,
        next_cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_account_keys_and_filters_unavailable_models() {
        let result = parse(&json!({"models": {
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
        assert!(!model.metadata.contains_key("reasoning_efforts"));
    }

    #[test]
    fn rejects_malformed_catalog_without_guessing_models() {
        for payload in [
            json!({}),
            json!({"models": []}),
            json!({"models": {"x": {"disabled": "false"}}}),
            json!({"models": {"x": {"maxTokens": -1}}}),
        ] {
            assert!(parse(&payload).is_none());
        }
    }
}
