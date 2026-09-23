mod allowance;
mod anthropic;
mod catalog;
mod cloud;
mod codec;
mod generic;
mod metadata;
mod openai;

use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{EncodedRequest, ProtocolTransform};
use stravia_runtime_contract::protocol::ir::{AiErrorKind, AiRequest};
use stravia_vendor_common::common;
#[cfg(test)]
use stravia_vendor_sdk::DefaultModelsSource;
#[cfg(target_arch = "wasm32")]
use stravia_vendor_sdk::VendorGuest;
use stravia_vendor_sdk::{
    Capability, CatalogSyncOutcome, CatalogSyncRequest, ErrorKind, GuestHost, Operation,
    OperationInput, OperationOutput, PluginError, ProviderDescriptor, ProviderSnapshot,
    VendorDescriptor, VendorKind,
};

/// Profiles statically owned by this vendor — not catalog entries. `custom`
/// is the merged profile covering every selectable egress protocol.
pub(crate) const INTERNAL_PROVIDER_IDS: &[&str] =
    &["custom", "gateway", "ollama", "openai-compatible"];

fn provider_descriptor(provider_id: &str) -> Option<ProviderDescriptor> {
    let descriptor = match provider_id {
        "openai" => openai::descriptor(provider_id),
        "anthropic" => anthropic::descriptor(provider_id),
        "azure"
        | "amazon-bedrock"
        | "google-vertex"
        | "google-vertex-anthropic"
        | "sap-ai-core"
        | "gitlab"
        | "watsonx" => cloud::descriptor(provider_id),
        _ => metadata::descriptor(provider_id),
    }?;
    Some(add_allowance_capabilities(descriptor))
}

/// The npm implementation a connection's profile claims. Runtime-registered
/// profiles carry it in the host-injected `vendor_profile`; bundled profiles
/// fall back to the embedded catalog list.
fn catalog_implementation(provider: &ProviderSnapshot) -> Option<String> {
    runtime_profile(provider)
        .and_then(|profile| profile.implementation)
        .or_else(|| {
            metadata::bundled_catalog_profile(&provider.provider_id)
                .map(|profile| profile.npm.to_owned())
        })
}

/// Host-injected effective Provider Profile for runtime-registered catalog
/// entries that have no static descriptor slot.
fn runtime_profile(provider: &ProviderSnapshot) -> Option<ProviderDescriptor> {
    provider
        .operation_metadata
        .get("vendor_profile")
        .and_then(|value| serde_json::from_value::<ProviderDescriptor>(value.clone()).ok())
        .filter(|profile| profile.provider_id == provider.provider_id)
}

fn is_static_profile(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "openai"
            | "anthropic"
            | "azure"
            | "amazon-bedrock"
            | "google-vertex"
            | "google-vertex-anthropic"
            | "sap-ai-core"
            | "gitlab"
            | "watsonx"
    ) || INTERNAL_PROVIDER_IDS.contains(&provider_id)
        || metadata::CATALOG_VENDOR_IDS.contains(&provider_id)
}

fn add_allowance_capabilities(mut descriptor: ProviderDescriptor) -> ProviderDescriptor {
    for channel in &mut descriptor.channels {
        if allowance::supports(&descriptor.provider_id, &channel.id) {
            channel.capabilities.insert(Capability::Allowance);
        }
    }
    descriptor.capabilities = descriptor
        .channels
        .iter()
        .flat_map(|channel| channel.capabilities.iter().copied())
        .collect();
    descriptor
}

/// Returns the single fallback manifest exported by this component.
pub fn descriptor() -> VendorDescriptor {
    let providers = catalog::provider_set(
        metadata::bundled_catalog_profiles()
            .iter()
            .map(|profile| {
                metadata::catalog_profile_descriptor(
                    profile.id,
                    profile.name,
                    profile.npm,
                    profile.api,
                )
                .unwrap_or_else(|| {
                    panic!(
                        "bundled Catalog profile `{}` uses unsupported implementation `{}`",
                        profile.id, profile.npm
                    )
                })
            })
            .collect(),
    );

    VendorDescriptor {
        vendor_id: "base".into(),
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("package version must be valid semver"),
        display_name: "Stravia Base Providers".into(),
        description: Some(
            "Built-in fallback implementation for standard protocols and provider integrations."
                .into(),
        ),
        authors: vec!["Stravia".into()],
        canonical_format_version: stravia_vendor_sdk::CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Fallback,
        providers,
    }
}

pub fn select_protocol(
    operation: Operation,
    channel: &str,
    provider: &ProviderSnapshot,
    request: &AiRequest,
) -> Result<String, PluginError> {
    validate_target(provider, channel)?;
    match provider.provider_id.as_str() {
        "openai" => openai::select_protocol(operation, channel, provider, request),
        "azure" | "azure-cognitive-services"
            if operation == Operation::Infer && channel == "default" =>
        {
            Ok(azure_endpoint(request))
        }
        "custom" | "openai-compatible" => Ok(generic::select_protocol(provider, request)),
        provider_id if is_static_profile(provider_id) => Ok(provider.protocol.clone()),
        _ => match catalog_implementation(provider).as_deref() {
            Some("@ai-sdk/openai") => {
                openai::select_protocol(operation, channel, provider, request)
            }
            Some("@ai-sdk/azure") if operation == Operation::Infer && channel == "default" => {
                Ok(azure_endpoint(request))
            }
            _ => Ok(provider.protocol.clone()),
        },
    }
}

fn azure_endpoint(request: &AiRequest) -> String {
    let endpoint = if request.embedding.is_some() {
        stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_EMBEDDINGS_V1
    } else {
        stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1
    };
    endpoint.to_string()
}

/// Dispatches an operation by the supplier profile embedded in the snapshot.
pub fn execute(
    host: &GuestHost,
    operation: Operation,
    channel: &str,
    input: OperationInput,
) -> Result<OperationOutput, PluginError> {
    if input.operation() != operation {
        return Err(error(
            ErrorKind::Invalid,
            "operation kind does not match typed input",
        ));
    }
    let provider_id = input.provider().provider_id.clone();
    validate_target(input.provider(), channel)?;

    if operation == Operation::Discover {
        let provider = input.provider();
        if let Some(response) = generic::explicit_discovery(&provider_id, provider)? {
            return Ok(OperationOutput::Discover(response));
        }
    }
    if operation == Operation::Allowance {
        let OperationInput::Allowance { provider, request } = input else {
            return Err(error(ErrorKind::Invalid, "allowance input mismatch"));
        };
        if provider.channel != channel {
            return Err(error(
                ErrorKind::Invalid,
                "snapshot channel does not match execute channel",
            ));
        }
        if !allowance::supports(&provider_id, channel) {
            return Err(error(
                ErrorKind::Unsupported,
                "provider channel does not support allowance",
            ));
        }
        return allowance::execute(&provider_id, host, provider, request)
            .map(OperationOutput::Allowance);
    }

    match provider_id.as_str() {
        "openai" => openai::execute("openai", host, operation, channel, input),
        "anthropic" => anthropic::execute("anthropic", host, operation, channel, input),
        "azure" | "azure-cognitive-services" => {
            cloud::execute("azure", host, operation, channel, input)
        }
        "amazon-bedrock"
        | "google-vertex"
        | "google-vertex-anthropic"
        | "sap-ai-core"
        | "gitlab"
        | "watsonx" => cloud::execute(&provider_id, host, operation, channel, input),
        profile if is_static_profile(profile) => {
            generic::execute(&provider_id, host, operation, channel, input)
        }
        _ => match catalog_implementation(input.provider()).as_deref() {
            Some("@ai-sdk/openai") => openai::execute("openai", host, operation, channel, input),
            Some("@ai-sdk/anthropic") => {
                anthropic::execute("anthropic", host, operation, channel, input)
            }
            Some("@ai-sdk/azure") => cloud::execute("azure", host, operation, channel, input),
            Some("@ai-sdk/gateway") => generic::execute("gateway", host, operation, channel, input),
            _ => generic::execute(&provider_id, host, operation, channel, input),
        },
    }
}

/// `sync-catalog` export: fetch and derive the complete Provider Profile set
/// owned by this vendor for the given snapshot.
pub fn sync_catalog(
    host: &GuestHost,
    request: &CatalogSyncRequest,
) -> Result<CatalogSyncOutcome, PluginError> {
    let mut outcome = catalog::sync_catalog(host, request)?;
    outcome.providers = catalog::provider_set(std::mem::take(&mut outcome.providers));
    Ok(outcome)
}

pub(crate) fn encode_inference_request(
    protocol: &str,
    request: &AiRequest,
) -> Result<EncodedRequest, PluginError> {
    let Some(adapter) = codec::adapter(protocol) else {
        return common::encode_inference_request(protocol, request);
    };
    common::ensure_no_native_compaction(request)?;
    ProtocolTransform::encode_request_with(adapter, request)
        .map_err(common::map_request_transform_error)
}

pub(crate) fn decode_inference(
    host: &GuestHost,
    protocol: &str,
    response: stravia_vendor_sdk::HttpResponse,
) -> Result<OperationOutput, PluginError> {
    let Some(adapter) = codec::adapter(protocol) else {
        return common::decode_inference(host, protocol, response);
    };
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let body = stravia_vendor_sdk::read_http_body(&response, 256 * 1024)?;
        return Err(common::upstream_error(status, &headers, &body));
    }
    let streaming = headers.iter().any(|(name, value)| {
        if !name.eq_ignore_ascii_case("content-type") {
            return false;
        }
        let value = value.to_ascii_lowercase();
        value.contains("text/event-stream")
            || value.contains("application/x-ndjson")
            || value.contains("application/connect+proto")
            || value.contains("application/vnd.amazon.eventstream")
    });
    let complete = if streaming {
        let mut decoder = ProtocolTransform::decode_stream_with(adapter)
            .map_err(common::map_response_transform_error)?;
        let mut accumulator = StreamResponseAccumulator::default();
        while let Some(chunk) = response.read_body()? {
            let deltas = decoder
                .decode_chunk(&chunk)
                .map_err(common::map_response_transform_error)?;
            common::emit_deltas(host, &mut accumulator, &deltas)?;
        }
        let deltas = decoder
            .finish()
            .map_err(common::map_response_transform_error)?;
        common::emit_deltas(host, &mut accumulator, &deltas)?;
        let complete = accumulator.into_ai_response();
        host.emit_completed(&complete)?;
        complete
    } else {
        let body = stravia_vendor_sdk::read_http_body(&response, 32 * 1024 * 1024)?;
        let value = serde_json::from_slice(&body).map_err(|error| {
            common::model_error(
                AiErrorKind::ServerError,
                format!("upstream returned invalid JSON: {error}"),
            )
        })?;
        ProtocolTransform::decode_response_with(adapter, value)
            .map_err(common::map_response_transform_error)?
    };
    Ok(OperationOutput::Infer(Box::new(complete)))
}

fn validate_target(provider: &ProviderSnapshot, channel: &str) -> Result<(), PluginError> {
    let provider_id = provider.provider_id.as_str();
    let declared = match (provider_id, channel) {
        ("anthropic", "default" | "claude-code") => true,
        ("google-vertex", "native" | "openai") => true,
        ("google-vertex", _) => false,
        (_, "default") => {
            is_static_profile(provider_id)
                || metadata::bundled_catalog_profile(provider_id).is_some()
                || runtime_profile(provider).is_some()
        }
        _ => false,
    };
    if !declared {
        return Err(error(
            ErrorKind::Unsupported,
            format!("provider channel `{provider_id}/{channel}` is not owned by base"),
        ));
    }
    Ok(())
}

fn error(kind: ErrorKind, message: impl Into<String>) -> PluginError {
    PluginError {
        kind,
        message: message.into(),
        upstream_status: None,
    }
}

#[cfg(target_arch = "wasm32")]
struct BaseVendor;

#[cfg(target_arch = "wasm32")]
impl VendorGuest for BaseVendor {
    fn descriptor() -> VendorDescriptor {
        descriptor()
    }

    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        request: &AiRequest,
    ) -> Result<String, PluginError> {
        select_protocol(operation, channel, provider, request)
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        execute(host, operation, channel, input)
    }

    fn sync_catalog(
        host: &GuestHost,
        request: CatalogSyncRequest,
    ) -> Result<CatalogSyncOutcome, PluginError> {
        sync_catalog(host, &request)
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(BaseVendor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_manifest_preserves_every_bundled_catalog_profile() {
        let descriptor = descriptor();
        descriptor.validate().expect("valid base descriptor");
        assert_eq!(
            descriptor.providers.len(),
            metadata::bundled_catalog_profiles().len() + INTERNAL_PROVIDER_IDS.len()
        );
        for provider_id in [
            "minimax",
            "minimax-cn",
            "azure-cognitive-services",
            "meta",
            "v0",
            "vercel",
        ] {
            let profile = descriptor
                .provider(provider_id)
                .unwrap_or_else(|| panic!("missing bundled Catalog profile {provider_id}"));
            assert_eq!(profile.catalog_id.as_deref(), Some(provider_id));
        }
        for provider_id in ["amazon-bedrock", "watsonx"] {
            assert_eq!(
                descriptor
                    .provider(provider_id)
                    .expect("cloud profile")
                    .channels[0]
                    .default_models_source,
                Some(DefaultModelsSource::Catalog)
            );
        }
        for provider_id in ["openai", "google", "minimax", "azure-cognitive-services"] {
            assert_eq!(
                descriptor
                    .provider(provider_id)
                    .expect("live profile")
                    .channels[0]
                    .default_models_source,
                None
            );
        }
        for dedicated in ["openai-codex", "xai-grok", "command-code", "devin"] {
            assert!(descriptor.provider(dedicated).is_none(), "{dedicated}");
        }

        let minimax = descriptor.provider("minimax").expect("MiniMax profile");
        assert_eq!(minimax.channels.len(), 1);
        assert!(minimax.channels[0].auth.is_none());
        assert!(
            minimax
                .network
                .extra_origins
                .iter()
                .all(|origin| origin.host != "claude.com" && origin.host != "platform.claude.com")
        );
        assert_eq!(
            minimax.channels[0].protocol.as_deref(),
            Some("anthropic-messages")
        );
        assert!(minimax.capabilities.contains(&Capability::ConfigValidation));

        let meta = descriptor.provider("meta").expect("Meta profile");
        assert_eq!(meta.channels.len(), 1);
        assert!(meta.channels[0].auth.is_none());
        assert!(meta.capabilities.contains(&Capability::Compact));
        assert!(meta.capabilities.contains(&Capability::ConfigValidation));
        assert!(
            meta.config_fields
                .iter()
                .any(|field| field.key == "websocket_url")
        );
        assert_eq!(
            descriptor
                .provider("vercel")
                .expect("Vercel profile")
                .channels[0]
                .protocol
                .as_deref(),
            Some("gateway-language-model")
        );
        assert_eq!(
            descriptor.provider("v0").expect("v0 profile").channels[0]
                .protocol
                .as_deref(),
            Some("openai-compatible")
        );

        let json = serde_json::to_vec(&descriptor).expect("descriptor JSON");
        assert!(
            json.len() < 2 * 1024 * 1024,
            "descriptor size: {}",
            json.len()
        );
    }
}
