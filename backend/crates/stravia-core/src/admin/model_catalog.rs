use super::*;

pub(super) fn resolve_models_endpoint(provider: &Provider) -> Option<String> {
    if let Some(endpoint) = provider.effective_models_source() {
        let trimmed = endpoint.trim();
        if !trimmed.is_empty() && trimmed != "catalog" {
            return Some(trimmed.to_string());
        }
    }

    let base = provider.base_url.trim_end_matches('/');
    match provider.protocol.as_str() {
        "openai" | "openai-compatible" | "openai-compat" | "open-responses" | "anthropic"
        | "anthropic-messages" | "anthropic-msgs" => {
            let has_base_path = reqwest::Url::parse(base)
                .ok()
                .map(|url| {
                    let pathname = url.path().trim_end_matches('/');
                    !pathname.is_empty() && pathname != "/"
                })
                .unwrap_or(false);
            if has_base_path {
                Some(format!("{base}/models"))
            } else {
                Some(format!("{base}/v1/models"))
            }
        }
        "gemini" | "google-gemini" | "google-genai" => Some(format!("{base}/v1beta/models")),
        _ => None,
    }
}

pub(super) fn runtime_binding_headers(binding: &RuntimeBinding) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (key, value) in &binding.extra_headers {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    Ok(headers)
}

pub(super) fn build_model_headers(
    protocol: &str,
    vendor: Option<&str>,
    api_key: &str,
) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let is_google_vendor = vendor
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("google"));
    match protocol {
        "anthropic" => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        "gemini" => {
            if is_google_vendor {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {api_key}"))?,
                );
            }
        }
        _ => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))?,
            );
        }
    }
    Ok(headers)
}

pub(super) fn extract_models_from_response(
    _protocol: &str,
    vendor: Option<&str>,
    json: &Value,
) -> Vec<String> {
    let is_google_vendor = vendor
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("google"));
    let mut models = json
        .get("data")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("id").and_then(|value| value.as_str()))
        .map(|id| {
            if is_google_vendor {
                id.strip_prefix("models/").unwrap_or(id).to_string()
            } else {
                id.to_string()
            }
        })
        .collect::<Vec<_>>();

    if models.is_empty() {
        models = json
            .get("models")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter(|item| {
                item.get("visibility")
                    .and_then(Value::as_str)
                    .map(|visibility| visibility.eq_ignore_ascii_case("list"))
                    .unwrap_or(true)
            })
            .filter_map(|item| {
                item.get("name")
                    .and_then(|value| value.as_str())
                    .or_else(|| item.get("slug").and_then(|value| value.as_str()))
                    .or_else(|| item.get("id").and_then(|value| value.as_str()))
            })
            .map(|name| {
                let normalized = name.rsplit('/').next().unwrap_or(name);
                if is_google_vendor {
                    normalized
                        .strip_prefix("models/")
                        .unwrap_or(normalized)
                        .to_string()
                } else {
                    normalized.to_string()
                }
            })
            .collect::<Vec<_>>();
    }

    models.sort();
    models.dedup();
    models
}

pub(super) fn parse_static_models(raw: Option<&str>) -> Vec<String> {
    let mut models = raw
        .unwrap_or("")
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models
}

pub(super) fn uses_catalog_inventory(provider: &Provider) -> bool {
    if provider.models_source.as_deref() != Some("catalog")
        || provider
            .static_models
            .as_deref()
            .is_some_and(|models| !parse_static_models(Some(models)).is_empty())
    {
        return false;
    }

    let preset_key = provider.preset_key.as_deref().map(str::trim);
    let vendor_id = provider.vendor.as_deref().map(str::trim);
    if preset_key.is_some() && preset_key != vendor_id {
        return true;
    }

    preset_channel(provider)
        .is_none_or(|channel| channel.models_source.is_none() && channel.static_models.is_empty())
}

pub(super) fn preset_static_models(provider: &Provider) -> Vec<String> {
    preset_channel(provider)
        .map(|channel| {
            channel
                .static_models
                .iter()
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn preset_capabilities_source(provider: &Provider) -> CapabilitiesSource {
    preset_channel(provider)
        .map(|channel| channel.capabilities_source)
        .unwrap_or(CapabilitiesSource::Auto)
}

fn preset_channel(provider: &Provider) -> Option<&'static crate::provider::metadata::ChannelDef> {
    let preset_key = provider.preset_key.as_deref()?;
    let registry = VendorRegistry::global();
    let meta = provider
        .vendor
        .as_deref()
        .and_then(|vendor_id| registry.metadata(vendor_id))
        .or_else(|| registry.metadata(preset_key))?;
    let channel_id = provider.channel.as_deref().unwrap_or("default");
    meta.channels
        .iter()
        .find(|channel| channel.id == channel_id)
}

pub(super) fn is_ollama_show_endpoint(url: &str) -> bool {
    url.trim_end_matches('/').ends_with("/api/show")
}

pub(super) fn parse_ollama_capability(json: &Value, model: &str) -> ModelCapabilities {
    let model_info = json.get("model_info").and_then(Value::as_object);
    let capabilities = json
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let has_vision = capabilities
        .iter()
        .any(|capability| capability.eq_ignore_ascii_case("vision"));

    ModelCapabilities {
        provider: "ollama".to_string(),
        model_id: model.to_string(),
        context_window: model_info
            .and_then(extract_ollama_context_window)
            .unwrap_or(8 * 1024),
        embedding_length: model_info.and_then(extract_ollama_embedding_length),
        output_max_tokens: None,
        tool_call: capabilities.iter().any(|capability| capability == "tools"),
        reasoning: capabilities
            .iter()
            .any(|capability| capability == "thinking"),
        input_modalities: if has_vision {
            vec!["text".to_string(), "image".to_string()]
        } else {
            vec!["text".to_string()]
        },
        output_modalities: vec!["text".to_string()],
        input_cost: Some(0.0),
        output_cost: Some(0.0),
    }
}

pub(super) fn extract_ollama_context_window(
    model_info: &serde_json::Map<String, Value>,
) -> Option<u64> {
    let architecture = model_info.get("general.architecture")?.as_str()?;
    model_info
        .get(&format!("{architecture}.context_length"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

pub(super) fn extract_ollama_embedding_length(
    model_info: &serde_json::Map<String, Value>,
) -> Option<u64> {
    if let Some(architecture) = model_info
        .get("general.architecture")
        .and_then(Value::as_str)
        && let Some(value) = model_info
            .get(&format!("{architecture}.embedding_length"))
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
    {
        return Some(value);
    }
    model_info
        .get("embedding_length")
        .and_then(Value::as_u64)
        .or_else(|| {
            model_info
                .get("general.embedding_length")
                .and_then(Value::as_u64)
        })
        .filter(|value| *value > 0)
}

pub(super) fn parse_http_capability(json: &Value, model: &str) -> Option<ModelCapabilities> {
    let item = json
        .get("data")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.eq_ignore_ascii_case(model))
        })?;
    let model_id = item.get("id").and_then(Value::as_str).unwrap_or(model);
    let context_window = item
        .get("context_length")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(128 * 1024);
    let output_max_tokens = item
        .get("top_provider")
        .and_then(Value::as_object)
        .and_then(|object| object.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0);
    let supported_parameters = item
        .get("supported_parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let input_modalities = item
        .get("architecture")
        .and_then(Value::as_object)
        .and_then(|object| object.get("input_modalities"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["text".to_string()]);
    let output_modalities = item
        .get("architecture")
        .and_then(Value::as_object)
        .and_then(|object| object.get("output_modalities"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["text".to_string()]);
    let input_cost = item
        .get("pricing")
        .and_then(Value::as_object)
        .and_then(|object| object.get("prompt"))
        .and_then(parse_maybe_price_per_token);
    let output_cost = item
        .get("pricing")
        .and_then(Value::as_object)
        .and_then(|object| object.get("completion"))
        .and_then(parse_maybe_price_per_token);
    let lower_model_id = model_id.to_lowercase();

    Some(ModelCapabilities {
        provider: "openrouter".to_string(),
        model_id: model_id.to_string(),
        context_window,
        embedding_length: None,
        output_max_tokens,
        tool_call: supported_parameters
            .iter()
            .any(|value| value.as_str() == Some("tools")),
        reasoning: lower_model_id.contains("reason")
            || lower_model_id.contains("thinking")
            || lower_model_id.contains("o1")
            || lower_model_id.contains("o3")
            || lower_model_id.contains("o4"),
        input_modalities,
        output_modalities,
        input_cost,
        output_cost,
    })
}

pub(super) fn parse_maybe_price_per_token(value: &Value) -> Option<f64> {
    let value = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))?;
    (value > 0.0).then_some(value * 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_model_discovery_excludes_hidden_picker_models() {
        let response = serde_json::json!({
            "models": [
                {"slug": "visible-model", "visibility": "list"},
                {"slug": "gpt-5.6-sol-wm", "visibility": "hide"},
                {"slug": "internal-model", "visibility": "none"},
                {"name": "legacy-model"}
            ]
        });

        assert_eq!(
            extract_models_from_response("open-responses", Some("openai"), &response),
            vec!["legacy-model".to_string(), "visible-model".to_string()]
        );
    }
}
