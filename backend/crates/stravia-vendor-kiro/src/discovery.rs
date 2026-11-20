use crate::transport::{self, Action};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use stravia_vendor_common::connection_check::invalid_response;
use stravia_vendor_sdk::{
    DiscoverResponse, DiscoveredModel, GuestHost, PluginError, ProviderSnapshot, read_http_body,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListModels<'a> {
    origin: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_token: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    models: Vec<Model>,
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Model {
    model_id: String,
    model_name: Option<String>,
    token_limits: Option<Limits>,
    supported_input_types: Option<Vec<String>>,
    additional_model_request_fields_schema: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limits {
    max_input_tokens: Option<u64>,
}

pub(super) fn page(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    cursor: Option<&str>,
) -> Result<DiscoverResponse, PluginError> {
    let body = ListModels {
        origin: "KIRO_CLI",
        next_token: cursor,
    };
    let request = transport::rpc(provider, Action::ListModels, &body)?;
    let response = transport::send(host, request)?;
    decode(&read_http_body(&response, 4 * 1024 * 1024)?)
}

fn decode(bytes: &[u8]) -> Result<DiscoverResponse, PluginError> {
    let page: Page = serde_json::from_slice(bytes)
        .map_err(|e| invalid_response(format!("invalid Kiro model list: {e}")))?;
    let models = page
        .models
        .into_iter()
        .map(|m| {
            if m.model_id.trim().is_empty() {
                return Err(invalid_response("Kiro model is missing its ID"));
            }
            let mut metadata = BTreeMap::new();
            if let Some(input) = m.supported_input_types {
                let input: Vec<_> = input
                    .into_iter()
                    .filter_map(|kind| match kind.as_str() {
                        "TEXT" => Some("text"),
                        "IMAGE" => Some("image"),
                        _ => None,
                    })
                    .collect();
                // Kiro's GenerateAssistantResponse stream exposes text output; the
                // catalog itself only declares supported input types.
                metadata.insert(
                    "modalities".into(),
                    serde_json::json!({ "input": input, "output": ["text"] }),
                );
            }
            if let Some(schema) = m.additional_model_request_fields_schema {
                // Match KAS's effort schema precedence, including fallback for empty enums.
                let efforts = [
                    (
                        "output_config",
                        "/properties/output_config/properties/effort/enum",
                    ),
                    ("reasoning", "/properties/reasoning/properties/effort/enum"),
                ]
                .into_iter()
                .filter_map(|(group, path)| {
                    schema
                        .pointer(path)
                        .and_then(serde_json::Value::as_array)
                        .map(|values| (group, values))
                })
                .map(|(group, values)| {
                    let values = values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= 256
                                && !value.chars().any(char::is_control)
                                && !value.eq_ignore_ascii_case("default")
                                && !value.eq_ignore_ascii_case("null")
                        })
                        .take(128)
                        .collect::<Vec<_>>();
                    (group, values)
                })
                .find(|(_, values)| !values.is_empty());
                if let Some((group, efforts)) = efforts {
                    metadata.insert("kiro_effort_group".into(), group.into());
                    metadata.insert("reasoning_efforts".into(), serde_json::json!(efforts));
                }
                // Match KAS 3.4.0 KFu: thinking is toggleable iff enum includes both disabled and adaptive.
                let thinking_toggleable = schema
                    .pointer("/properties/thinking/properties/type/enum")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|values| {
                        values.iter().any(|v| v.as_str() == Some("disabled"))
                            && values.iter().any(|v| v.as_str() == Some("adaptive"))
                    });
                if thinking_toggleable {
                    metadata.insert("kiro_thinking_toggleable".into(), serde_json::json!(true));
                }
            }
            if let Some(context) = m
                .token_limits
                .and_then(|limits| limits.max_input_tokens)
                .filter(|&n| n > 0)
            {
                metadata.insert("limit".into(), serde_json::json!({ "context": context }));
            }
            Ok(DiscoveredModel {
                display_name: m
                    .model_name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| m.model_id.clone()),
                id: m.model_id,
                family: None,
                selector: None,
                capabilities: Vec::new(),
                metadata,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DiscoverResponse {
        models,
        next_cursor: page.next_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_declared_input_types_and_effort_schema_without_inventing_output() {
        let page = decode(br#"{"models":[
            {"modelId":"anthropic-model","supportedInputTypes":["TEXT","IMAGE"],"additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{"effort":{"enum":["low","medium","high","max"]}}}}}},
            {"modelId":"reasoning-model","supportedInputTypes":["TEXT"],"additionalModelRequestFieldsSchema":{"properties":{"reasoning":{"properties":{"effort":{"enum":["low","high"]}}}}}},
            {"modelId":"unknown-model"}
        ]}"#).unwrap();
        let first = &page.models[0].metadata;
        assert_eq!(
            first["modalities"],
            serde_json::json!({"input": ["text", "image"], "output": ["text"]})
        );
        assert_eq!(
            first["reasoning_efforts"],
            serde_json::json!(["low", "medium", "high", "max"])
        );
        assert_eq!(first["kiro_effort_group"], "output_config");
        let second = &page.models[1].metadata;
        assert_eq!(
            second["modalities"],
            serde_json::json!({"input": ["text"], "output": ["text"]})
        );
        assert_eq!(
            second["reasoning_efforts"],
            serde_json::json!(["low", "high"])
        );
        assert_eq!(second["kiro_effort_group"], "reasoning");
        assert!(!page.models[2].metadata.contains_key("modalities"));
        assert!(!page.models[2].metadata.contains_key("kiro_effort_group"));
        assert!(!page.models[2].metadata.contains_key("reasoning_efforts"));
    }

    #[test]
    fn effort_schema_uses_first_nonempty_supported_path() {
        let page = decode(br#"{"models":[{"modelId":"model","supportedInputTypes":[],"additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{"effort":{"enum":[]}}},"reasoning":{"properties":{"effort":{"enum":["high"]}}}}}}]}"#).unwrap();
        assert_eq!(
            page.models[0].metadata["modalities"],
            serde_json::json!({"input": [], "output": ["text"]})
        );
        assert_eq!(
            page.models[0].metadata["reasoning_efforts"],
            serde_json::json!(["high"])
        );
    }

    #[test]
    fn ignores_unknown_types_and_prefers_output_config_effort() {
        let page = decode(br#"{"models":[{"modelId":"model","supportedInputTypes":["TEXT","FUTURE"],"additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{"effort":{"enum":["low","high"]}}},"reasoning":{"properties":{"effort":{"enum":["max"]}}}}}}]}"#).unwrap();
        assert_eq!(
            page.models[0].metadata["modalities"],
            serde_json::json!({"input": ["text"], "output": ["text"]})
        );
        assert_eq!(
            page.models[0].metadata["reasoning_efforts"],
            serde_json::json!(["low", "high"])
        );
        assert_eq!(
            page.models[0].metadata["kiro_effort_group"],
            "output_config"
        );
        assert!(
            !page.models[0]
                .metadata
                .contains_key("kiro_thinking_toggleable")
        );
    }

    #[test]
    fn detects_thinking_toggleable_schema_without_polluting_efforts() {
        let page = decode(br#"{"models":[
            {"modelId":"toggle-model","additionalModelRequestFieldsSchema":{"properties":{"thinking":{"properties":{"type":{"enum":["disabled","adaptive"],"default":"adaptive"}}}}}},
            {"modelId":"partial-model","additionalModelRequestFieldsSchema":{"properties":{"thinking":{"properties":{"type":{"enum":["adaptive"]}}}}}}
        ]}"#).unwrap();
        assert_eq!(page.models[0].metadata["kiro_thinking_toggleable"], true);
        assert!(!page.models[0].metadata.contains_key("reasoning_efforts"));
        assert!(
            !page.models[1]
                .metadata
                .contains_key("kiro_thinking_toggleable")
        );
    }

    #[test]
    fn maps_account_inventory_without_substituting_a_model_or_dropping_the_cursor() {
        let page = decode(br#"{"models":[
            {"modelId":"plan-model","tokenLimits":{"maxInputTokens":4096,"maxOutputTokens":64000}},
            {"modelId":"zero-limit-model","tokenLimits":{"maxInputTokens":0,"maxOutputTokens":64000}}
        ],"nextToken":"next/page"}"#).unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("next/page"));
        assert_eq!(page.models[0].id, "plan-model");
        assert_eq!(page.models[0].display_name, "plan-model");
        assert_eq!(
            page.models[0].metadata["limit"],
            serde_json::json!({"context": 4096})
        );
        assert!(!page.models[0].metadata.contains_key("max_input_tokens"));
        assert!(!page.models[0].metadata.contains_key("max_output_tokens"));
        assert!(!page.models[1].metadata.contains_key("limit"));
        assert!(decode(br#"{"error":"denied"}"#).is_err());
    }
}
