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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limits {
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
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
            if let Some(limits) = m.token_limits {
                if let Some(n) = limits.max_input_tokens {
                    metadata.insert("max_input_tokens".into(), n.into());
                }
                if let Some(n) = limits.max_output_tokens {
                    metadata.insert("max_output_tokens".into(), n.into());
                }
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
    fn maps_account_inventory_without_substituting_a_model_or_dropping_the_cursor() {
        let page = decode(br#"{"models":[{"modelId":"plan-model","tokenLimits":{"maxInputTokens":4096}}],"nextToken":"next/page"}"#).unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("next/page"));
        assert_eq!(page.models[0].id, "plan-model");
        assert_eq!(page.models[0].display_name, "plan-model");
        assert_eq!(page.models[0].metadata["max_input_tokens"], 4096);
        assert!(decode(br#"{"error":"denied"}"#).is_err());
    }
}
