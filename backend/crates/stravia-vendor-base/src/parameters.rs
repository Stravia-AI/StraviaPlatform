//! Target-only legalization; protocol encoding remains owned by the shared codecs.
//! Only explicit canonical controls receive wire authority.
//! Cohere native allowlists follow https://docs.cohere.com/v2/reference/chat .
//! Bedrock cache TTL follows
//! https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_CachePointBlock.html .
//! Model `compat` gates follow omp's resolved model contract, not protocol identity:
//! https://github.com/can1357/oh-my-pi/blob/v18.10.4/packages/ai/src/providers/openai-shared.ts .
use serde_json::{Map, Value};
use stravia_protocol_codec::transform::EncodedRequest;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_sdk::{PluginError, ProviderSnapshot};

pub(crate) fn encode(
    provider: &ProviderSnapshot,
    protocol: &str,
    mut request: AiRequest,
) -> Result<EncodedRequest, PluginError> {
    request.normalize_parameters();
    legalize_model_controls(provider, &mut request);
    let sampling = compatible_sampling(provider, protocol, &mut request);
    let official = matches!(
        provider.provider_id.as_str(),
        "openai" | "azure" | "azure-cognitive-services"
    ) || official_openai_origin(&provider.base_url);
    let reasoning_model = official && official_reasoning_model(&request.model);
    let sampling_authorized = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.extensions.get("compat"))
        .and_then(|compat| compat.get("supportsSamplingParams"))
        .and_then(Value::as_bool)
        == Some(true);
    let endpoint =
        stravia_protocol_codec::registry::ProtocolRegistry::global().resolve_alias(protocol);
    if endpoint == Some(stravia_runtime_contract::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01) {
        stravia_vendor_common::common::apply_anthropic_output_limit(provider, &mut request);
    }
    let mut encoded = crate::encode_inference_request(protocol, request)?;
    if let Some(body) = encoded.body.as_object_mut() {
        body.extend(sampling);
        if provider.provider_id == "deepseek"
            && endpoint
                == Some(
                    stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                )
            && body.get("tool_choice").is_some_and(|choice| {
                choice == "required"
                    || choice.get("type").and_then(Value::as_str) == Some("function")
            })
        {
            // DeepSeek defaults to thinking even without an effort. Forced tools
            // take precedence after codec and vendor overrides have been applied.
            body.remove("reasoning_effort");
            body.insert("thinking".into(), serde_json::json!({"type": "disabled"}));
        }
        if official
            && endpoint == Some(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24)
        {
            body.remove("frequency_penalty");
            body.remove("presence_penalty");
        }
        if reasoning_model {
            if !sampling_authorized {
                body.remove("temperature");
                body.remove("top_p");
            }
            if endpoint
                == Some(
                    stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                )
                && let Some(cap) = body.remove("max_tokens")
            {
                body.insert("max_completion_tokens".into(), cap);
            }
        }
    }
    Ok(encoded)
}

fn compatible_sampling(
    provider: &ProviderSnapshot,
    protocol: &str,
    request: &mut AiRequest,
) -> Map<String, Value> {
    let mut projected = Map::new();
    // Compatibility is not authority: only advertised, known wire controls qualify.
    // In particular, the official OpenAI and Azure contracts do not define these.
    if stravia_protocol_codec::registry::ProtocolRegistry::global().resolve_alias(protocol)
        != Some(stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1)
        || matches!(
            provider.provider_id.as_str(),
            "openai" | "azure" | "azure-cognitive-services"
        )
        || official_openai_origin(&provider.base_url)
    {
        return projected;
    }
    let supported = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.extensions.get("supported_parameters"))
        .and_then(Value::as_array);
    let accepts = |name: &str| {
        supported.is_some_and(|values| values.iter().any(|value| value.as_str() == Some(name)))
    };
    if !accepts("logprobs") {
        request.parameters.logprobs = None;
    }
    if accepts("top_k")
        && let Some(value) = request.parameters.top_k
    {
        projected.insert("top_k".into(), value.into());
    }
    if accepts("min_p")
        && let Some(value) = request.parameters.min_p
    {
        projected.insert("min_p".into(), value.into());
    }
    if accepts("repetition_penalty")
        && let Some(value) = request.parameters.repetition_penalty
    {
        projected.insert("repetition_penalty".into(), value.into());
    }
    projected
}

fn official_openai_origin(base: &str) -> bool {
    url::Url::parse(base)
        .ok()
        .is_some_and(|url| url.host_str() == Some("api.openai.com"))
}

fn official_reasoning_model(model: &str) -> bool {
    ["o1", "o3", "o4", "gpt-5"].iter().any(|prefix| {
        model == *prefix
            || model
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('-') || suffix.starts_with('.'))
    })
}

fn legalize_model_controls(provider: &ProviderSnapshot, request: &mut AiRequest) {
    let metadata = provider
        .model_metadata
        .as_ref()
        .map(|metadata| &metadata.extensions);
    let compat = metadata.and_then(|metadata| metadata.get("compat"));
    if compat
        .and_then(|compat| compat.get("supportsSamplingParams"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        request.generation.temperature = None;
        request.generation.top_p = None;
        request.parameters.top_k = None;
        request.parameters.min_p = None;
    }
    if compat
        .and_then(|compat| compat.get("supportsPenaltyAndStopParams"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        request.generation.frequency_penalty = None;
        request.generation.presence_penalty = None;
        request.parameters.repetition_penalty = None;
        request.generation.stop = None;
    }
    if compat
        .and_then(|compat| compat.get("clampOutputToModelMax"))
        .and_then(Value::as_bool)
        == Some(true)
        && let Some(limit) = metadata
            .and_then(|metadata| metadata.get("limit"))
            .and_then(|limit| limit.get("output"))
            .and_then(Value::as_u64)
            .and_then(|limit| u32::try_from(limit).ok())
    {
        request.generation.max_tokens = request.generation.max_tokens.map(|value| value.min(limit));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use stravia_vendor_sdk::ModelMetadata;

    fn provider() -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "custom".into(),
            channel: "default".into(),
            base_url: "https://compatible.example/v1".into(),
            protocol: "openai-chat-completions".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some("model".into()),
            model_metadata: Some(ModelMetadata {
                extensions: BTreeMap::from([(
                    "supported_parameters".into(),
                    json!(["top_k", "min_p", "repetition_penalty", "future_control"]),
                )]),
                ..Default::default()
            }),
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    fn anthropic_provider(max_tokens: Option<Value>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "anthropic".into(),
            channel: "default".into(),
            base_url: "https://api.anthropic.com".into(),
            protocol: "anthropic-messages".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some("claude-sonnet-4-5".into()),
            model_metadata: Some(ModelMetadata {
                extensions: max_tokens
                    .map(|value| BTreeMap::from([("max_tokens".into(), value)]))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    fn thinking_tool_request(choice: Value) -> AiRequest {
        use stravia_protocol_codec::transform::ProtocolTransform;
        use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;

        ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .unwrap()
            .decode_request(json!({
                "model": "deepseek-flash",
                "messages": [{"role": "user", "content": "Return the result using respond."}],
                "tools": [{"type": "function", "function": {"name": "respond", "parameters": {"type": "object"}}}],
                "tool_choice": choice,
                "reasoning_effort": "high",
                "thinking": {"type": "enabled"}
            }))
            .unwrap()
    }

    #[test]
    fn deepseek_forced_tool_choice_disables_thinking_without_relaxing_choice() {
        let mut target = provider();
        target.provider_id = "deepseek".into();
        target.model = Some("deepseek-flash".into());
        for choice in [
            json!({"type": "function", "function": {"name": "respond"}}),
            json!("required"),
        ] {
            let mut request = thinking_tool_request(choice.clone());
            // Target and raw wire overrides must not re-enable thinking at egress.
            request
                .meta
                .vendor
                .egress
                .insert("thinking".into(), json!({"type": "enabled"}));
            request
                .meta
                .vendor
                .egress
                .insert("reasoning_effort".into(), json!("max"));
            let body = encode(&target, "openai-chat-completions", request)
                .unwrap()
                .body;
            assert_eq!(body["tool_choice"], choice);
            assert_eq!(body["tools"][0]["function"]["name"], "respond");
            assert_eq!(body["thinking"], json!({"type": "disabled"}));
            assert!(body.get("reasoning_effort").is_none(), "{body}");
        }
    }

    #[test]
    fn forced_tool_thinking_override_is_limited_to_deepseek_chat() {
        for (provider_id, protocol, choice, effort_pointer) in [
            (
                "deepseek",
                "openai-chat-completions",
                json!("auto"),
                "/reasoning_effort",
            ),
            (
                "deepseek",
                "openai-chat-completions",
                json!("none"),
                "/reasoning_effort",
            ),
            (
                "custom",
                "openai-chat-completions",
                json!("required"),
                "/reasoning_effort",
            ),
            (
                "deepseek",
                "open-responses",
                json!("required"),
                "/reasoning/effort",
            ),
        ] {
            let mut target = provider();
            target.provider_id = provider_id.into();
            let body = encode(&target, protocol, thinking_tool_request(choice.clone()))
                .unwrap()
                .body;
            assert_eq!(body["tool_choice"], choice);
            assert_eq!(body.pointer(effort_pointer), Some(&json!("high")), "{body}");
            assert_ne!(
                body.pointer("/thinking/type"),
                Some(&json!("disabled")),
                "{body}"
            );
        }
    }

    #[test]
    fn anthropic_omitted_limit_uses_native_model_metadata() {
        for max_tokens in [json!(128_000), json!(8192), json!(u32::MAX)] {
            let body = encode(
                &anthropic_provider(Some(max_tokens.clone())),
                "anthropic-messages",
                AiRequest::new("claude-sonnet-4-5", vec![]),
            )
            .unwrap()
            .body;
            assert_eq!(body["max_tokens"], max_tokens);
        }
    }

    #[test]
    fn anthropic_untrusted_model_metadata_falls_back_to_codec_default() {
        for metadata in [
            json!(0),
            json!(-1),
            json!(u32::MAX as u64 + 1),
            json!("128000"),
            json!(1.5),
            json!(null),
        ] {
            let body = encode(
                &anthropic_provider(Some(metadata.clone())),
                "anthropic-messages",
                AiRequest::new("claude-sonnet-4-5", vec![]),
            )
            .unwrap()
            .body;
            assert_eq!(body["max_tokens"], 64_000, "{metadata}");
        }
        let body = encode(
            &anthropic_provider(None),
            "anthropic-messages",
            AiRequest::new("claude-sonnet-4-5", vec![]),
        )
        .unwrap()
        .body;
        assert_eq!(body["max_tokens"], 64_000);

        let mut target = anthropic_provider(None);
        target.model_metadata = None;
        let body = encode(
            &target,
            "anthropic-messages",
            AiRequest::new("claude-sonnet-4-5", vec![]),
        )
        .unwrap()
        .body;
        assert_eq!(body["max_tokens"], 64_000);
    }

    #[test]
    fn anthropic_explicit_limit_wins_over_model_metadata() {
        for explicit in [2048, 200_000] {
            let mut request = AiRequest::new("claude-sonnet-4-5", vec![]);
            request.generation.max_tokens = Some(explicit);
            let body = encode(
                &anthropic_provider(Some(json!(128_000))),
                "anthropic-messages",
                request,
            )
            .unwrap()
            .body;
            assert_eq!(body["max_tokens"], explicit);
        }
    }

    #[test]
    fn native_model_output_limit_is_not_injected_into_other_protocols() {
        let mut target = provider();
        target
            .model_metadata
            .as_mut()
            .unwrap()
            .extensions
            .insert("max_tokens".into(), json!(128_000));
        let request = AiRequest::new(
            "model",
            vec![stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                    std::sync::Arc::new("hello".into()),
                ),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        for (protocol, limit_pointer) in [
            ("openai-chat-completions", "/max_tokens"),
            ("open-responses", "/max_output_tokens"),
            ("google-gemini", "/generationConfig/maxOutputTokens"),
        ] {
            let body = encode(&target, protocol, request.clone()).unwrap().body;
            assert!(body.pointer(limit_pointer).is_none(), "{protocol}");
        }
    }

    #[test]
    fn compatible_sampling_requires_explicit_known_target_contract() {
        let mut target = provider();
        let mut request = AiRequest::new("model", vec![]);
        request.parameters.top_k = Some(0);
        request.parameters.min_p = Some(0.0);
        request.parameters.repetition_penalty = Some(0.0);

        let encoded = encode(&target, "openai-chat-completions", request.clone()).unwrap();
        assert_eq!(encoded.body["top_k"], 0);
        assert_eq!(encoded.body["min_p"], 0.0);
        assert_eq!(encoded.body["repetition_penalty"], 0.0);
        assert!(encoded.body.get("future_control").is_none());
        for provider_id in ["openai", "azure", "azure-cognitive-services"] {
            target.provider_id = provider_id.into();
            let body = encode(&target, "openai-chat-completions", request.clone())
                .unwrap()
                .body;
            assert!(body.get("top_k").is_none());
            assert!(body.get("min_p").is_none());
            assert!(body.get("repetition_penalty").is_none());
        }
        target.provider_id = "custom".into();
        target.base_url = "https://api.openai.com/v1".into();
        assert!(
            encode(&target, "openai-chat-completions", request.clone())
                .unwrap()
                .body
                .get("top_k")
                .is_none()
        );
        target.base_url = "https://compatible.example/v1".into();
        target.model_metadata = None;
        assert!(
            encode(&target, "openai-chat-completions", request)
                .unwrap()
                .body
                .get("top_k")
                .is_none()
        );
    }

    #[test]
    fn explicit_model_gates_do_not_enable_unknown_future_controls() {
        let mut target = provider();
        target.model_metadata.as_mut().unwrap().extensions.extend([
            ("compat".into(), json!({"supportsSamplingParams": false, "supportsPenaltyAndStopParams": false, "clampOutputToModelMax": true, "futureOptional": false})),
            ("limit".into(), json!({"output": 64})),
        ]);
        let mut request = AiRequest::new("model", vec![]);
        request.generation.temperature = Some(0.0);
        request.generation.top_p = Some(0.0);
        request.generation.presence_penalty = Some(0.0);
        request.generation.max_tokens = Some(100);
        request.parameters.top_k = Some(0);
        let body = encode(&target, "openai-chat-completions", request.clone())
            .unwrap()
            .body;
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
        assert!(body.get("presence_penalty").is_none());
        assert_eq!(body["max_tokens"], 64);
        request.generation.max_tokens = Some(0);
        assert_eq!(
            encode(&target, "openai-chat-completions", request)
                .unwrap()
                .body["max_tokens"],
            0
        );
    }

    #[test]
    fn compatible_logprobs_require_target_support_and_keep_zero_candidates() {
        let mut target = provider();
        let mut request = AiRequest::new("model", vec![]);
        request.parameters.logprobs = Some(0);
        let body = encode(&target, "openai-chat-completions", request.clone())
            .unwrap()
            .body;
        assert!(body.get("logprobs").is_none());
        assert!(body.get("top_logprobs").is_none());
        target
            .model_metadata
            .as_mut()
            .unwrap()
            .extensions
            .insert("supported_parameters".into(), json!(["logprobs"]));
        let body = encode(&target, "openai-chat-completions", request.clone())
            .unwrap()
            .body;
        assert_eq!(body["logprobs"], true);
        assert_eq!(body["top_logprobs"], 0);
        target.provider_id = "openai".into();
        target.model_metadata = None;
        let body = encode(&target, "openai-chat-completions", request)
            .unwrap()
            .body;
        assert_eq!(body["logprobs"], true);
        assert_eq!(body["top_logprobs"], 0);
    }

    #[test]
    fn official_reasoning_models_use_completion_cap_without_sampling() {
        let mut target = provider();
        target.provider_id = "openai".into();
        let mut request = AiRequest::new(
            "o3",
            vec![stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                    std::sync::Arc::new("hello".into()),
                ),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.generation.max_tokens = Some(0);
        request.generation.temperature = Some(0.0);
        request.generation.top_p = Some(0.0);
        let body = encode(&target, "openai-chat-completions", request.clone())
            .unwrap()
            .body;
        assert_eq!(body["max_completion_tokens"], 0);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        request.generation.frequency_penalty = Some(0.0);
        request.generation.presence_penalty = Some(0.0);
        let body = encode(
            &target,
            &stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24.to_string(),
            request,
        )
        .unwrap()
        .body;
        assert!(body.get("frequency_penalty").is_none());
        assert!(body.get("presence_penalty").is_none());
    }
}
