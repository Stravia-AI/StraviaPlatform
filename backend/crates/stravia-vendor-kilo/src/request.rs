use super::*;
use serde_json::json;

pub(super) fn metadata(provider: &ProviderSnapshot) -> Option<&BTreeMap<String, Value>> {
    provider.model_metadata.as_ref().map(|m| &m.extensions)
}
pub(super) fn responses(provider: &ProviderSnapshot, _model: &str) -> bool {
    // The catalog is authoritative. Names (including GPT/o-series names hosted by
    // another provider) do not confer first-party Responses transport rights.
    metadata(provider)
        .and_then(|m| m.get("opencode"))
        .and_then(|m| m.get("ai_sdk_provider"))
        .and_then(Value::as_str)
        == Some("openai")
}
pub(super) fn prepare(
    provider: &ProviderSnapshot,
    mut request: AiRequest,
) -> Result<(String, Value), PluginError> {
    let protocol = if responses(provider, &request.model) {
        RESPONSES
    } else {
        CHAT
    };
    let params = metadata(provider)
        .and_then(|m| m.get("supported_parameters"))
        .and_then(Value::as_array);
    let supports =
        |name: &str| params.is_some_and(|params| params.iter().any(|v| v.as_str() == Some(name)));
    if !supports("reasoning") {
        request.reasoning = Default::default();
    }
    if !supports("tools") {
        request.tools = None;
        request.tool_choice = None;
    }
    if !supports("tool_choice") {
        request.tool_choice = None;
    }
    if matches!(
        request.tool_choice.as_ref(),
        Some(stravia_runtime_contract::protocol::ir::ToolChoice::Native(
            _
        ))
    ) {
        request.tool_choice = None;
    }
    if let Some(tools) = &mut request.tools {
        tools.retain(|tool| !tool.name.starts_with("__builtin__"));
    }
    // Native state and hosted definitions are not portable to a gateway account.
    // Only canonical function tools and explicit, catalog-supported controls cross.
    request.parameters.native_tools.clear();

    request.parameters.continuation = Default::default();
    request.parameters.session = Default::default();
    request.items.retain(|item| item.item_reference().is_none());
    for item in &mut request.items {
        item.meta = None;
        if let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
            &mut item.content
        {
            blocks.retain(|block| !matches!(block, stravia_runtime_contract::protocol::ir::ContentBlock::Compaction { .. } | stravia_runtime_contract::protocol::ir::ContentBlock::CompactionTrigger { .. } | stravia_runtime_contract::protocol::ir::ContentBlock::Unknown { .. }));
            for block in blocks {
                if let stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning {
                    encrypted_content,
                    ..
                } = block
                {
                    *encrypted_content = None;
                }
                if let stravia_runtime_contract::protocol::ir::ContentBlock::Thinking {
                    signature,
                    ..
                } = block
                {
                    *signature = None;
                }
            }
        }
    }
    let toggle = if protocol == CHAT {
        use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
        match request.reasoning.target_control.as_ref() {
            Some(TargetThinkingControl::Enabled) => Some(true),
            Some(TargetThinkingControl::Disabled) => Some(false),
            _ if request.reasoning.mode.as_deref() == Some("disabled")
                || request.reasoning.level == Some(ThinkingLevel::Off) =>
            {
                Some(false)
            }
            _ if request.reasoning.level.is_none() && request.reasoning.is_enabled() => Some(true),
            _ => None,
        }
    } else {
        None
    };
    let budget = request.reasoning.budget_tokens;
    let store_specified = request.parameters.store.is_some();
    if toggle.is_some() {
        request.reasoning = Default::default();
    }
    // These OpenRouter controls have no ordinary OpenAI Chat spelling in the codec.
    let gateway_sampling = if protocol == CHAT {
        [
            ("top_k", request.parameters.top_k.map(Value::from)),
            ("min_p", request.parameters.min_p.map(Value::from)),
            (
                "repetition_penalty",
                request.parameters.repetition_penalty.map(Value::from),
            ),
        ]
    } else {
        [
            ("top_k", None),
            ("min_p", None),
            ("repetition_penalty", None),
        ]
    };
    let mut body = common::encode_inference_request(protocol, request)?.body;
    let object = body
        .as_object_mut()
        .ok_or_else(|| fail(ErrorKind::Invalid, "Codec returned a non-object request"))?;
    if !store_specified {
        object.remove("store");
    }
    for (name, value) in gateway_sampling {
        if let Some(value) = value {
            object.insert(name.into(), value);
        }
    }
    // Closed target allowlists ensure new canonical defaults and unknown native
    // extensions cannot acquire passthrough rights by accident.
    object.retain(|name, _| {
        if matches!(name.as_str(), "model" | "stream") {
            return true;
        }
        if protocol == CHAT && name == "messages"
            || protocol == RESPONSES && matches!(name.as_str(), "input" | "instructions")
        {
            return true;
        }
        let supported_name = if name == "max_output_tokens" {
            "max_tokens"
        } else {
            name.as_str()
        };
        let legal = if protocol == CHAT {
            matches!(
                name.as_str(),
                "temperature"
                    | "top_p"
                    | "top_k"
                    | "min_p"
                    | "repetition_penalty"
                    | "max_tokens"
                    | "stop"
                    | "frequency_penalty"
                    | "presence_penalty"
                    | "seed"
                    | "n"
                    | "logit_bias"
                    | "logprobs"
                    | "top_logprobs"
                    | "response_format"
                    | "modalities"
                    | "audio"
                    | "prediction"
                    | "verbosity"
                    | "tools"
                    | "tool_choice"
                    | "parallel_tool_calls"
                    | "reasoning_effort"
                    | "metadata"
                    | "user"
                    | "safety_identifier"
                    | "service_tier"
                    | "prompt_cache_key"
                    | "prompt_cache_retention"
                    | "store"
                    | "stream_options"
            )
        } else {
            matches!(
                name.as_str(),
                "temperature"
                    | "top_p"
                    | "max_output_tokens"
                    | "top_logprobs"
                    | "text"
                    | "tools"
                    | "tool_choice"
                    | "parallel_tool_calls"
                    | "reasoning"
                    | "max_tool_calls"
                    | "include"
                    | "background"
                    | "truncation"
                    | "metadata"
                    | "safety_identifier"
                    | "service_tier"
                    | "prompt_cache_key"
                    | "store"
                    | "stream_options"
            )
        };
        legal
            && match name.as_str() {
                "reasoning_effort" => supports("reasoning"),
                "text" => supports("response_format") || supports("verbosity"),
                // Usage collection is part of streaming, not model sampling support.
                "stream_options" if protocol == CHAT => true,
                _ => supports(supported_name),
            }
    });
    if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
        text.retain(|name, _| matches!(name.as_str(), "format" | "verbosity"));
        if !supports("response_format") {
            text.remove("format");
        }
        if !supports("verbosity") {
            text.remove("verbosity");
        }
        if text.is_empty() {
            object.remove("text");
        }
    }
    if let Some(options) = object
        .get_mut("stream_options")
        .and_then(Value::as_object_mut)
    {
        options.retain(|name, _| {
            protocol == CHAT && name == "include_usage"
                || supports("stream_options") && name == "include_obfuscation"
        });
    }
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.retain(|name, _| matches!(name.as_str(), "effort" | "summary"));
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(tool) = tool.as_object_mut() {
                tool.retain(|name, _| {
                    matches!(
                        name.as_str(),
                        "type" | "function" | "name" | "description" | "parameters" | "strict"
                    )
                });
                if let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) {
                    function.retain(|name, _| {
                        matches!(
                            name.as_str(),
                            "name" | "description" | "parameters" | "strict"
                        )
                    });
                }
            }
        }
    }
    let effort = if protocol == RESPONSES {
        object.get("reasoning").and_then(|v| v.get("effort"))
    } else {
        object.get("reasoning_effort")
    }
    .and_then(Value::as_str)
    .map(str::to_owned);
    if let Some(effort) = effort {
        let variants = metadata(provider)
            .and_then(|m| m.get("opencode"))
            .and_then(|m| m.get("variants"))
            .and_then(Value::as_object);
        let mapped = variants.and_then(|v| v.get(&effort)).map(|v| {
            if v.pointer("/reasoning/enabled").and_then(Value::as_bool) == Some(false) {
                "none"
            } else {
                v.pointer("/reasoning/effort")
                    .and_then(Value::as_str)
                    .unwrap_or(&effort)
            }
        });
        if protocol == RESPONSES {
            if let Some(mapped) = mapped {
                object.entry("reasoning").or_insert_with(|| json!({}))["effort"] = json!(mapped);
            } else if let Some(reasoning) =
                object.get_mut("reasoning").and_then(Value::as_object_mut)
            {
                reasoning.remove("effort");
            }
        } else {
            object.remove("reasoning_effort");
            if let Some(mapped) = mapped {
                object.insert("reasoning".into(), json!({"effort":mapped}));
            }
        }
    }
    if protocol == CHAT && supports("reasoning") {
        if let Some(enabled) = toggle {
            object.insert("reasoning".into(), json!({"enabled": enabled}));
        }
        if let Some(budget) = budget {
            object.entry("reasoning").or_insert_with(|| json!({}))["max_tokens"] = json!(budget);
        }
    }
    Ok((protocol.into(), body))
}
pub(super) fn infer(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    request.model = provider
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| fail(ErrorKind::Invalid, "Kilo inference requires a model"))?
        .into();
    token(provider)?;
    let (protocol, body) = prepare(provider, request)?;
    let path = if protocol == RESPONSES {
        "/api/openrouter/responses"
    } else {
        "/api/gateway/chat/completions"
    };
    let body = serde_json::to_vec(&body).map_err(|e| fail(ErrorKind::Invalid, e.to_string()))?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::net::Request {
        method: "POST".into(),
        url: format!("{ORIGIN}{path}"),
        headers: crate::net_fields(headers(provider)?),
        body,
    }, &crate::net_options())?;
    stream::decode(host, &protocol, response)
        .map(Box::new)
        .map(OperationOutput::Infer)
}
