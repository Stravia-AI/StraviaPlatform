//! Explicit connection checks for Base-owned profiles.
//!
//! A check performs one real, non-generating read against the configured
//! provider. It never generates output, synchronizes models, binds an account,
//! or writes vendor private state.

use serde_json::Value;
use stravia_vendor_common::common;
use stravia_vendor_common::connection_check::invalid_response;
use stravia_vendor_sdk::{
    ConnectionCheckResponse, ErrorKind, GuestHost, PluginError, ProviderSnapshot,
};

use crate::metadata;
use crate::{INTERNAL_PROVIDER_IDS, catalog_implementation};

/// Which read verifies a profile. `None` is an explicit "no safe probe"
/// decision, never an implicit fallback.
#[derive(Clone, Copy)]
enum Route {
    OpenAi,
    Anthropic,
    /// A provider-specific credential exchange plus inventory read.
    Cloud(&'static str),
    /// The documented Vercel AI Gateway inventory read.
    Gateway,
    /// A models read at the configured endpoint.
    Generic,
}

/// Catalog profiles borrow a static implementation; static profiles are
/// identified by id. The implementation is checked first so a storefront id
/// never falls back to an unrelated static profile shape.
fn route(provider_id: &str, implementation: Option<&str>) -> Option<Route> {
    if let Some(implementation) = implementation {
        return match implementation {
            "@ai-sdk/openai" => Some(Route::OpenAi),
            "@ai-sdk/anthropic" => Some(Route::Anthropic),
            "@ai-sdk/azure" => Some(Route::Cloud("azure")),
            "@ai-sdk/google-vertex/anthropic" => Some(Route::Cloud("google-vertex-anthropic")),
            "@jerome-benoit/sap-ai-provider-v2" => Some(Route::Cloud("sap-ai-core")),
            "gitlab-ai-provider" => Some(Route::Cloud("gitlab")),
            "@ai-sdk/gateway" => Some(Route::Gateway),
            // These profiles have no existing read-only probe: Bedrock has no
            // inventory endpoint on the runtime origin, native Vertex discovery
            // is a curated static list, and Watsonx exposes no inventory read.
            "@ai-sdk/google-vertex" | "@ai-sdk/amazon-bedrock" | "watsonx-ai-provider" => None,
            _ => Some(Route::Generic),
        };
    }
    Some(match provider_id {
        "openai" => Route::OpenAi,
        "anthropic" => Route::Anthropic,
        "azure" | "azure-cognitive-services" => Route::Cloud("azure"),
        "google-vertex-anthropic" => Route::Cloud("google-vertex-anthropic"),
        "sap-ai-core" => Route::Cloud("sap-ai-core"),
        "gitlab" => Route::Cloud("gitlab"),
        "amazon-bedrock" | "google-vertex" | "watsonx" => return None,
        id if INTERNAL_PROVIDER_IDS.contains(&id) => Route::Generic,
        id if metadata::CATALOG_VENDOR_IDS.contains(&id) => Route::Generic,
        id if metadata::bundled_catalog_profile(id).is_some() => Route::Generic,
        _ => return None,
    })
}

/// Every Base channel is either declared with `ConnectionCheck` or rejected as
/// unsupported; the capability and this predicate are derived from one route.
pub(crate) fn supports(provider_id: &str, channel: &str, implementation: Option<&str>) -> bool {
    channel == "default" && route(provider_id, implementation).is_some()
}

pub(crate) fn execute(
    host: &GuestHost,
    channel: &str,
    provider: &ProviderSnapshot,
) -> Result<ConnectionCheckResponse, PluginError> {
    let provider_id = provider.provider_id.as_str();
    let implementation = catalog_implementation(provider);
    let Some(route) = route(provider_id, implementation.as_deref()) else {
        return Err(common::unsupported(
            "connection_check",
            provider_id,
            channel,
        ));
    };
    if provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "provider snapshot channel does not match execute channel",
        ));
    }
    match route {
        Route::OpenAi => crate::openai::connection_check(host, provider),
        Route::Anthropic => crate::anthropic::connection_check(host, provider),
        Route::Cloud(vendor_id) => {
            crate::cloud::connection_check(host, vendor_id, channel, provider)
        }
        Route::Gateway => crate::generic::connection_check(host, "gateway", channel, provider),
        Route::Generic => crate::generic::connection_check(host, provider_id, channel, provider),
    }
}

/// Validate the structural shape of an inventory response. An empty list is a
/// valid account state; arbitrary JSON, a non-array wrapper, or entries without
/// any model identifier are response failures.
pub(crate) fn validate_models(
    value: &Value,
    keys: &[&str],
    id_fields: &[&str],
) -> Result<(), PluginError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response("model list response must be a JSON object"))?;
    let (key, list) = keys
        .iter()
        .find_map(|key| object.get(*key).map(|value| (*key, value)))
        .ok_or_else(|| {
            invalid_response(format!(
                "model list response has no `{}` field",
                keys.first().copied().unwrap_or("data")
            ))
        })?;
    let list = list
        .as_array()
        .ok_or_else(|| invalid_response(format!("model list response `{key}` must be an array")))?;
    for (index, entry) in list.iter().enumerate() {
        let entry = entry.as_object().ok_or_else(|| {
            invalid_response(format!("model list entry {index} must be an object"))
        })?;
        let identified = id_fields.iter().any(|field| {
            entry
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty())
        });
        if !identified {
            return Err(invalid_response(format!(
                "model list entry {index} has no model identifier"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lists_accept_empty_inventories_and_reject_unknown_shapes() {
        assert!(validate_models(&serde_json::json!({"data": []}), &["data"], &["id"]).is_ok());
        assert!(
            validate_models(
                &serde_json::json!({"data": [{"id": "model"}]}),
                &["data", "models"],
                &["id", "name"],
            )
            .is_ok()
        );
        for (value, keys, ids) in [
            (serde_json::json!([]), &["data"][..], &["id"][..]),
            (serde_json::json!({"data": {}}), &["data"][..], &["id"][..]),
            (
                serde_json::json!({"data": ["model"]}),
                &["data"][..],
                &["id"][..],
            ),
            (
                serde_json::json!({"data": [{}]}),
                &["data"][..],
                &["id"][..],
            ),
            (
                serde_json::json!({"data": [{"id": "  "}]}),
                &["data"][..],
                &["id"][..],
            ),
            (serde_json::json!({}), &["data"][..], &["id"][..]),
        ] {
            let error = validate_models(&value, keys, ids).unwrap_err();
            assert!(matches!(error.kind, ErrorKind::InvalidResponse));
        }
    }
}
