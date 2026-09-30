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
    let canonical_summaries = canonical_summaries(&canonical_models);
    Ok(CatalogSnapshot {
        version: version.clone(),
        providers_version: version,
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
        let Some(adapter_id) = adapter_id_for_npm(&package) else {
            hidden_package += 1;
            continue;
        };
        let protocol = protocol_for_package(&package, &id)
            .expect("supported npm package must resolve a protocol");
        // The catalog's npm adapter is presentation/egress metadata, not a
        // selectable provider identity. Installed profiles explicitly opt into
        // this entry through ProviderDescriptor::catalog_id.
        let base_url = object
            .get("api")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| adapter_default_base_url(adapter_id).map(str::to_owned))
            .unwrap_or_default();
        let documentation_url = object.get("doc").and_then(Value::as_str).map(str::to_owned);
        let channels = catalog_channels(&id, &name, &protocol, &base_url);
        providers.push(CatalogProvider {
            catalog_id: Some(id.clone()),
            id,
            name,
            documentation_url,
            npm: package,
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
        models.insert(
            id.clone(),
            ProviderModelMetadata::from_source_value(&id, value.clone())?.to_value()?,
        );
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
        let metadata = ProviderModelMetadata::from_source_value(&id, metadata.clone())
            .with_context(|| format!("invalid Provider Catalog Entry {provider_id}/{id}"))?;
        models.push(CatalogModelSource {
            provider_id: provider_id.to_string(),
            metadata: metadata.to_value()?,
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
    _protocol: &str,
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
    let limit = object.get("limit").and_then(Value::as_object);
    let cost = object.get("cost").and_then(Value::as_object);
    Ok(CatalogModel {
        id,
        name,
        status: optional_string(object.get("status"))?,
        release_date: optional_string(object.get("release_date"))?,
        capabilities: Some(CatalogCapabilities {
            input_modalities,
            output_modalities,
        }),
        limits: Some(CatalogLimits {
            context: optional_u64(limit.and_then(|item| item.get("context")))?,
        }),
        cost: Some(CatalogCost {
            input: optional_f64(cost.and_then(|item| item.get("input")))?,
            output: optional_f64(cost.and_then(|item| item.get("output")))?,
            cache_read: optional_f64(cost.and_then(|item| item.get("cache_read")))?,
            cache_write: optional_f64(cost.and_then(|item| item.get("cache_write")))?,
        }),
        reasoning_efforts: source_reasoning_efforts(object)?,
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

/// Upstream membership uses the raw index keys: a provider scope can exist
/// for entries this instance cannot brand (e.g. an implementation only the
/// guest understands).
pub(super) fn ensure_catalog_provider(
    snapshot: &CatalogSnapshot,
    provider_id: &str,
) -> anyhow::Result<()> {
    if snapshot.providers_raw.get(provider_id).is_some() {
        Ok(())
    } else {
        Err(CatalogError::ProviderNotFound {
            provider_id: provider_id.to_string(),
        }
        .into())
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

pub(crate) fn source_reasoning_efforts(
    object: &Map<String, Value>,
) -> anyhow::Result<Option<Vec<String>>> {
    let explicit = object
        .get("reasoning_efforts")
        .filter(|value| !value.is_null());
    let legacy_options = object.get("reasoning_options");
    let legacy_effort = legacy_options.and_then(|options| match options {
        Value::Array(options) => options
            .iter()
            .find(|option| option.get("type").and_then(Value::as_str) == Some("effort")),
        Value::Object(_) if options.get("type").and_then(Value::as_str) == Some("effort") => {
            Some(options)
        }
        _ => None,
    });
    let legacy = legacy_effort.and_then(|option| option.get("values"));
    let Some(value) = explicit.or(legacy) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("reasoning efforts must be an array"))?;
    let mut efforts = Vec::new();
    for value in values {
        if value.is_null() && explicit.is_none() {
            continue;
        }
        let effort = value
            .as_str()
            .ok_or_else(|| anyhow!("reasoning effort must be a string"))?
            .trim();
        if effort.is_empty()
            || effort.eq_ignore_ascii_case("default")
            || effort.eq_ignore_ascii_case("null")
        {
            if explicit.is_some() {
                bail!("invalid reasoning effort");
            }
            continue;
        }
        if effort.len() > 256 || effort.chars().any(char::is_control) {
            bail!("invalid reasoning effort");
        }
        if !efforts.iter().any(|value| value == effort) {
            efforts.push(effort.to_owned());
        }
    }
    if efforts.len() > 128 {
        bail!("too many reasoning efforts");
    }
    Ok(Some(efforts))
}

// The `npm` → adapter/protocol/base-url vocabulary is contract surface shared
// with the base guest (which registers profiles only for packages this table
// maps); it lives in the SDK so both sides cannot drift.
pub(super) use stravia_vendor_sdk::catalog::{
    adapter_default_base_url, adapter_id_for_package as adapter_id_for_npm, protocol_for_package,
};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xai_catalog_scope_recognizes_the_dedicated_grok_channel() {
        let channels = catalog_channels("xai", "xAI", "openai-compatible", "https://api.x.ai/v1");

        // Raw catalog channels validate model-scope access. Public choices are
        // intersected with one exact ProviderDescriptor in ProviderCatalog::providers.
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
