use super::*;

pub(super) fn parse_version(body: &[u8]) -> anyhow::Result<CatalogVersion> {
    let version: CatalogVersion =
        serde_json::from_slice(body).context("decode catalog version JSON")?;
    validate_version(&version)?;
    Ok(version)
}

pub(super) fn validate_version(version: &CatalogVersion) -> anyhow::Result<()> {
    if version.revision.is_empty()
        || version.revision.len() > 256
        || !version
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("catalog revision must be a non-empty safe path segment");
    }
    if version.generated_at.trim().is_empty() || version.generated_at.len() > 256 {
        bail!("catalog generated_at must be a non-empty string");
    }
    Ok(())
}

pub(super) fn bootstrap_snapshot() -> anyhow::Result<CatalogSnapshot> {
    parse_snapshot(
        BUILTIN_PROVIDERS.as_bytes(),
        BUILTIN_CANONICAL_MODELS.as_bytes(),
        CatalogVersion {
            revision: BOOTSTRAP_REVISION.to_string(),
            generated_at: BOOTSTRAP_GENERATED_AT.to_string(),
        },
    )
    .context("parse built-in Provider Catalog bootstrap")
}

pub(super) fn parse_snapshot(
    providers_body: &[u8],
    canonical_models_body: &[u8],
    version: CatalogVersion,
) -> anyhow::Result<CatalogSnapshot> {
    let providers_raw: Value =
        serde_json::from_slice(providers_body).context("decode provider index JSON")?;
    let providers = parse_providers(&providers_raw)?;
    let canonical_models = parse_canonical_models(canonical_models_body)?;
    let mut canonical_summaries: Vec<_> = canonical_models
        .iter()
        .map(|(id, metadata)| CanonicalModelSummary {
            id: id.clone(),
            name: metadata
                .get("name")
                .and_then(Value::as_str)
                .expect("validated Canonical Model name")
                .to_string(),
        })
        .collect();
    canonical_summaries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(CatalogSnapshot {
        version,
        providers,
        providers_raw,
        canonical_models,
        canonical_summaries,
    })
}

pub(super) fn parse_providers(raw: &Value) -> anyhow::Result<Vec<CatalogProvider>> {
    let root = raw
        .as_object()
        .ok_or_else(|| anyhow!("provider index root must be an object"))?;
    let mut providers = Vec::new();
    let mut hidden_package = 0usize;
    for (provider_key, value) in root {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("provider {provider_key} must be an object"))?;
        let id = required_string(object, "id", provider_key)?;
        if id != *provider_key {
            bail!("provider key/id mismatch: {provider_key}/{id}");
        }
        validate_provider_id(&id)
            .with_context(|| format!("catalog provider key {provider_key:?} id {id:?}"))?;
        let name = required_string(object, "name", provider_key)?;
        let package = required_string(object, "npm", provider_key)?;
        let Some(vendor_id) = vendor_id_for_npm(&package) else {
            hidden_package += 1;
            continue;
        };
        let protocol = protocol_for_package(&package, &id)
            .expect("supported npm package must resolve a protocol");
        let base_url = object
            .get("api")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| adapter_default_base_url(vendor_id).map(str::to_owned))
            .unwrap_or_default();
        let documentation_url = object.get("doc").and_then(Value::as_str).map(str::to_owned);
        let channels = catalog_channels(&id, &name, &protocol, &base_url);
        providers.push(CatalogProvider {
            id,
            name,
            documentation_url,
            npm: package,
            vendor_id: vendor_id.to_string(),
            protocol,
            base_url,
            channels,
        });
    }
    if providers.is_empty() {
        bail!("provider index contains no supported providers");
    }
    providers.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    tracing::info!(
        hidden_package,
        available = providers.len(),
        "normalized provider index"
    );
    Ok(providers)
}

pub(super) fn parse_canonical_models(body: &[u8]) -> anyhow::Result<BTreeMap<String, Value>> {
    let raw: Value = serde_json::from_slice(body).context("decode Canonical Model index JSON")?;
    let root = raw
        .as_object()
        .ok_or_else(|| anyhow!("Canonical Model index root must be an object"))?;
    if root.is_empty() {
        bail!("Canonical Model index is empty");
    }
    let mut models = BTreeMap::new();
    for (key, value) in root {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("Canonical Model {key} must be an object"))?;
        let id = required_string(object, "id", key)?;
        if id != *key {
            bail!("Canonical Model key/id mismatch: {key}/{id}");
        }
        validate_canonical_model_id(&id)?;
        required_string(object, "name", &id)?;
        models.insert(id, value.clone());
    }
    Ok(models)
}

pub(super) fn parse_scope(
    body: &[u8],
    revision: &str,
    provider_id: &str,
) -> anyhow::Result<CatalogProviderScope> {
    let raw: Value = serde_json::from_slice(body).context("decode Provider Catalog scope JSON")?;
    let root = raw
        .as_object()
        .ok_or_else(|| anyhow!("Provider Catalog scope root must be an object"))?;
    let mut models = Vec::with_capacity(root.len());
    for (key, metadata) in root {
        let object = metadata.as_object().ok_or_else(|| {
            anyhow!("Provider Catalog Entry {provider_id}/{key} must be an object")
        })?;
        let id = required_string(object, "id", key)?;
        if id != *key {
            bail!("Provider Catalog Entry key/id mismatch: {provider_id}/{key}/{id}");
        }
        if let Some(canonical_id) = object.get("canonical_id").and_then(Value::as_str) {
            validate_canonical_model_id(canonical_id).with_context(|| {
                format!("Provider Catalog Entry {provider_id}/{id} canonical_id")
            })?;
        }
        ProviderModelMetadata::from_source_value(&id, metadata.clone())
            .with_context(|| format!("invalid Provider Catalog Entry {provider_id}/{id}"))?;
        models.push(CatalogModelSource {
            provider_id: provider_id.to_string(),
            metadata: metadata.clone(),
        });
    }
    models.sort_by(|left, right| {
        left.metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase()
            .cmp(
                &right
                    .metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase(),
            )
            .then_with(|| model_source_id(left).cmp(model_source_id(right)))
    });
    Ok(CatalogProviderScope {
        revision: revision.to_string(),
        provider_id: provider_id.to_string(),
        models,
    })
}

pub(super) fn parse_catalog_model(
    provider_id: &str,
    protocol: &str,
    value: &Value,
) -> anyhow::Result<CatalogModel> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("Provider Catalog Entry {provider_id} must be an object"))?;
    let id = required_string(object, "id", provider_id)?;
    let name = required_string(object, "name", &id)?;
    let modalities = object.get("modalities").and_then(Value::as_object);
    let input_modalities = string_array(modalities.and_then(|item| item.get("input")))?;
    let output_modalities = string_array(modalities.and_then(|item| item.get("output")))?;
    let reasoning = optional_bool(object.get("reasoning"))?;
    let limit = object.get("limit").and_then(Value::as_object);
    let cost = object.get("cost").and_then(Value::as_object);
    Ok(CatalogModel {
        id,
        name,
        status: optional_string(object.get("status"))?,
        release_date: optional_string(object.get("release_date"))?,
        capabilities: Some(CatalogCapabilities {
            tool_call: optional_bool(object.get("tool_call"))?,
            reasoning,
            attachment: optional_bool(object.get("attachment"))?,
            temperature: optional_bool(object.get("temperature"))?,
            input_modalities,
            output_modalities,
        }),
        limits: Some(CatalogLimits {
            context: optional_u64(limit.and_then(|item| item.get("context")))?,
            output: optional_u64(limit.and_then(|item| item.get("output")))?,
        }),
        cost: Some(CatalogCost {
            input: optional_f64(cost.and_then(|item| item.get("input")))?,
            output: optional_f64(cost.and_then(|item| item.get("output")))?,
            cache_read: optional_f64(cost.and_then(|item| item.get("cache_read")))?,
            cache_write: optional_f64(cost.and_then(|item| item.get("cache_write")))?,
        }),
        reasoning_options: parse_reasoning_options(object.get("reasoning_options"))?
            .or_else(|| infer_reasoning_options(provider_id, protocol, reasoning)),
    })
}

pub(super) fn model_sort_order(left: &CatalogModel, right: &CatalogModel) -> std::cmp::Ordering {
    left.name
        .to_lowercase()
        .cmp(&right.name.to_lowercase())
        .then_with(|| left.id.cmp(&right.id))
}

pub(super) fn model_source_id(source: &CatalogModelSource) -> &str {
    source
        .metadata
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

pub(super) fn ensure_catalog_provider(
    snapshot: &CatalogSnapshot,
    provider_id: &str,
) -> anyhow::Result<()> {
    if snapshot
        .providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        Ok(())
    } else {
        bail!("catalog provider not found: {provider_id}")
    }
}

pub(super) fn validate_provider_id(provider_id: &str) -> anyhow::Result<()> {
    if provider_id.is_empty()
        || provider_id.len() > 128
        || !provider_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid catalog provider id");
    }
    Ok(())
}

pub(super) fn validate_canonical_model_id(id: &str) -> anyhow::Result<()> {
    let Some((lab_id, model_id)) = id.split_once('/') else {
        bail!("Canonical Model ID must contain a lab and model segment");
    };
    if lab_id.is_empty()
        || model_id.is_empty()
        || id.matches('/').count() != 1
        || lab_id.len() > 128
        || model_id.len() > 256
    {
        bail!("invalid Canonical Model ID");
    }
    Ok(())
}

pub(super) fn validate_svg(body: &[u8]) -> anyhow::Result<()> {
    let text = std::str::from_utf8(body).context("provider logo is not UTF-8")?;
    let trimmed = text.trim_start();
    if !(trimmed.starts_with("<svg") || trimmed.starts_with("<?xml")) {
        bail!("provider logo is not SVG");
    }
    Ok(())
}

pub(super) fn required_string(
    object: &Map<String, Value>,
    field: &str,
    context: &str,
) -> anyhow::Result<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{context}.{field} must be a non-empty string"))
}

pub(super) fn optional_string(value: Option<&Value>) -> anyhow::Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => bail!("expected a string or null"),
    }
}

pub(super) fn optional_bool(value: Option<&Value>) -> anyhow::Result<bool> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => bail!("expected a boolean or null"),
    }
}

pub(super) fn optional_u64(value: Option<&Value>) -> anyhow::Result<Option<u64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("expected a non-negative integer")),
        Some(_) => bail!("expected a number or null"),
    }
}

pub(super) fn optional_budget_bound(value: Option<&Value>) -> anyhow::Result<Option<u64>> {
    if value.and_then(Value::as_i64).is_some_and(|value| value < 0) {
        return Ok(None);
    }
    optional_u64(value)
}

pub(super) fn optional_f64(value: Option<&Value>) -> anyhow::Result<Option<f64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => {
            let value = value
                .as_f64()
                .ok_or_else(|| anyhow!("expected a finite number"))?;
            if !value.is_finite() || value < 0.0 {
                bail!("expected a finite non-negative number");
            }
            Ok(Some(value))
        }
        Some(_) => bail!("expected a number or null"),
    }
}

pub(super) fn string_array(value: Option<&Value>) -> anyhow::Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| anyhow!("expected an array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("expected a string array"))
        })
        .collect()
}

pub(super) fn parse_reasoning_options(
    value: Option<&Value>,
) -> anyhow::Result<Option<CatalogReasoningOptions>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let mut selected = None;
    let entries: &[Value] = match value {
        Value::Null => return Ok(None),
        Value::Array(entries) => entries,
        Value::Object(_) => std::slice::from_ref(value),
        _ => bail!("reasoning_options must be an array"),
    };
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or_else(|| anyhow!("reasoning_options entries must be objects"))?;
        let kind = required_string(object, "type", "reasoning_options")?;
        let candidate = match kind.as_str() {
            "effort" => Some(CatalogReasoningOptions::Effort {
                values: reasoning_effort_values(object.get("values"))?,
            }),
            "toggle" => Some(CatalogReasoningOptions::Toggle),
            "budget" | "budget_tokens" => Some(CatalogReasoningOptions::Budget {
                min: optional_budget_bound(object.get("min"))?,
                max: optional_budget_bound(object.get("max"))?,
            }),
            _ => None,
        };
        let selected_priority = selected
            .as_ref()
            .map(reasoning_option_priority)
            .unwrap_or(0);
        if candidate
            .as_ref()
            .is_some_and(|candidate| reasoning_option_priority(candidate) > selected_priority)
        {
            selected = candidate;
        }
    }
    Ok(selected)
}

pub(super) fn reasoning_effort_values(value: Option<&Value>) -> anyhow::Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| anyhow!("expected an array"))?
        .iter()
        .filter_map(|value| match value {
            Value::Null => None,
            Value::String(value) => Some(Ok(value.clone())),
            _ => Some(Err(anyhow!("expected string or null effort values"))),
        })
        .collect()
}

pub(super) fn reasoning_option_priority(option: &CatalogReasoningOptions) -> u8 {
    match option {
        CatalogReasoningOptions::Effort { .. } => 3,
        CatalogReasoningOptions::Budget { .. } => 2,
        CatalogReasoningOptions::Toggle => 1,
    }
}

pub(super) fn infer_reasoning_options(
    provider_id: &str,
    protocol: &str,
    reasoning: bool,
) -> Option<CatalogReasoningOptions> {
    if !reasoning {
        return None;
    }
    match provider_id {
        "openai" => Some(CatalogReasoningOptions::Effort {
            values: ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }),
        "anthropic" => Some(CatalogReasoningOptions::Budget {
            min: Some(1024),
            max: None,
        }),
        "google" if protocol == "google-gemini" => Some(CatalogReasoningOptions::Toggle),
        _ => None,
    }
}

pub(super) fn vendor_id_for_npm(package: &str) -> Option<&'static str> {
    Some(match package {
        "@ai-sdk/openai" => "openai",
        "@ai-sdk/openai-compatible" => "openai-compatible",
        "@ai-sdk/anthropic" => "anthropic",
        "@ai-sdk/google" => "google",
        "@ai-sdk/xai" => "xai",
        "@ai-sdk/azure" => "azure",
        "@ai-sdk/groq" => "groq",
        "@ai-sdk/cerebras" => "cerebras",
        "@ai-sdk/togetherai" => "togetherai",
        "@ai-sdk/mistral" => "mistral",
        "@ai-sdk/deepinfra" => "deepinfra",
        "@ai-sdk/perplexity" => "perplexity",
        "@ai-sdk/gateway" => "gateway",
        "@ai-sdk/vercel" => "vercel",
        "@ai-sdk/google-vertex" => "google-vertex",
        "@ai-sdk/google-vertex/anthropic" => "google-vertex-anthropic",
        "@ai-sdk/amazon-bedrock" => "amazon-bedrock",
        "@ai-sdk/cohere" => "cohere",
        "@openrouter/ai-sdk-provider" => "openrouter",
        "watsonx-ai-provider" => "watsonx",
        "venice-ai-sdk-provider" => "venice",
        "@aihubmix/ai-sdk-provider" => "aihubmix",
        "@jerome-benoit/sap-ai-provider-v2" => "sap-ai-core",
        "@qvac/ai-sdk-provider" => "qvac",
        "@saladtechnologies-oss/ai-sdk-provider" => "salad-cloud",
        "ai-gateway-provider" => "cloudflare-ai-gateway",
        "gitlab-ai-provider" => "gitlab",
        "merge-gateway-ai-sdk-provider" => "merge-gateway",
        _ => return None,
    })
}

pub(super) fn protocol_for_package(package: &str, provider_id: &str) -> Option<String> {
    let protocol = match package {
        "@ai-sdk/openai" => "open-responses",
        "@ai-sdk/openai-compatible" => "openai-compatible",
        "@ai-sdk/anthropic" => "anthropic-messages",
        "@ai-sdk/google" => "google-gemini",
        "@ai-sdk/xai" => "openai-compatible",
        "@ai-sdk/google-vertex" => "google-gemini",
        "@ai-sdk/google-vertex/anthropic" => "anthropic-messages",
        "@ai-sdk/amazon-bedrock" => "bedrock-converse",
        "@ai-sdk/cohere" => "cohere-chat",
        "watsonx-ai-provider" => "watsonx-text-chat",
        "@ai-sdk/gateway" => "gateway-language-model",
        _ if vendor_id_for_npm(package).is_some() => "openai-compatible",
        _ if provider_id == "openai" => "open-responses",
        _ if provider_id == "anthropic" => "anthropic-messages",
        _ if provider_id == "google" => "google-gemini",
        _ if provider_id == "xai" => "openai-compatible",
        _ => return None,
    };
    Some(protocol.to_string())
}

pub(super) fn adapter_default_base_url(vendor_id: &str) -> Option<&'static str> {
    match vendor_id {
        "openai" => Some("https://api.openai.com/v1"),
        "anthropic" => Some("https://api.anthropic.com"),
        "google" => Some("https://generativelanguage.googleapis.com"),
        "xai" => Some("https://api.x.ai/v1"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "cerebras" => Some("https://api.cerebras.ai/v1"),
        "togetherai" => Some("https://api.together.xyz/v1"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        "deepinfra" => Some("https://api.deepinfra.com/v1/openai"),
        "perplexity" => Some("https://api.perplexity.ai"),
        "gateway" => Some("https://ai-gateway.vercel.sh/v4/ai"),
        "vercel" => Some("https://api.v0.dev/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        "cohere" => Some("https://api.cohere.com/v2"),
        "watsonx" => Some("https://us-south.ml.cloud.ibm.com"),
        "venice" => Some("https://api.venice.ai/api/v1"),
        "aihubmix" => Some("https://aihubmix.com/v1"),
        "qvac" => Some("http://127.0.0.1:11435/v1"),
        "salad-cloud" => Some("https://ai.salad.cloud/v1"),
        "merge-gateway" => Some("https://api-gateway.merge.dev/v1/ai-sdk"),
        "gitlab" => Some("https://cloud.gitlab.com/ai/v1/proxy/openai/v1"),
        "google-vertex" | "google-vertex-anthropic" => {
            Some("https://aiplatform.googleapis.com/v1/projects/{project}/locations/global")
        }
        _ => None,
    }
}

pub(super) fn catalog_channels(
    provider_id: &str,
    provider_name: &str,
    protocol: &str,
    base_url: &str,
) -> Vec<CatalogChannel> {
    let mut channels = vec![channel(
        provider_id,
        "default",
        provider_name,
        protocol,
        base_url,
        CatalogAuthMode::OptionalApiKey,
    )];
    if provider_id == "openai" {
        channels.push(channel(
            provider_id,
            "codex",
            "Codex",
            "open-responses",
            "https://chatgpt.com/backend-api/codex",
            CatalogAuthMode::OAuth,
        ));
    }
    if provider_id == "anthropic" {
        channels.push(channel(
            provider_id,
            "claude-code",
            "Claude Code subscription",
            "anthropic-messages",
            "https://api.anthropic.com",
            CatalogAuthMode::SetupToken,
        ));
    }
    if provider_id == "xai" {
        channels.push(channel(
            provider_id,
            "grok",
            "grok",
            "open-responses",
            "https://cli-chat-proxy.grok.com/v1",
            CatalogAuthMode::OAuth,
        ));
    }
    channels
}

pub(super) fn channel(
    provider_id: &str,
    channel_id: &str,
    label: &str,
    protocol: &str,
    base_url: &str,
    auth_mode: CatalogAuthMode,
) -> CatalogChannel {
    let fingerprint_source = format!(
        "{provider_id}\0{channel_id}\0{protocol}\0{base_url}\0{}",
        match auth_mode {
            CatalogAuthMode::OptionalApiKey => "optional_api_key",
            CatalogAuthMode::OAuth => "oauth",
            CatalogAuthMode::SetupToken => "setup_token",
        }
    );
    let digest = sha2::Sha256::digest(fingerprint_source.as_bytes());
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    use std::fmt::Write;
    for byte in digest {
        write!(&mut fingerprint, "{byte:02x}").expect("writing to a String cannot fail");
    }
    CatalogChannel {
        id: channel_id.to_string(),
        label: label.to_string(),
        protocol: protocol.to_string(),
        base_url: base_url.to_string(),
        auth_mode,
        fingerprint,
    }
}

pub(super) fn codex_subscription_model(model_id: &str) -> bool {
    const EXPLICIT: &[&str] = &["gpt-5.5", "gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.4-mini"];
    const DENIED: &[&str] = &["gpt-5.5-pro"];
    if DENIED.contains(&model_id) {
        return false;
    }
    if EXPLICIT.contains(&model_id) {
        return true;
    }
    let Some(rest) = model_id.strip_prefix("gpt-") else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(major) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let Some(minor) = parts.next().and_then(|value| {
        value
            .split(|character: char| !character.is_ascii_digit())
            .next()
            .and_then(|value| value.parse::<u32>().ok())
    }) else {
        return false;
    };
    (major, minor) > (5, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xai_catalog_publishes_grok_oauth_channel() {
        let channels = catalog_channels("xai", "xAI", "openai-compatible", "https://api.x.ai/v1");

        let grok = channels
            .iter()
            .find(|channel| channel.id == "grok")
            .expect("Grok OAuth channel should be available");
        assert_eq!(grok.label, "grok");
        assert_eq!(grok.protocol, "open-responses");
        assert_eq!(grok.base_url, "https://cli-chat-proxy.grok.com/v1");
        assert_eq!(grok.auth_mode, CatalogAuthMode::OAuth);
    }
}
