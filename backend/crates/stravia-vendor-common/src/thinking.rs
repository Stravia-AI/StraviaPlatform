//! Shared OpenAI-compatible thinking controls for vendor guests.
//!
//! Effort is represented by the shared codec. Toggle-only and companion-toggle
//! vendors translate `TargetThinkingControl::Effort` (or canonical level) into
//! their upstream wire shapes at egress here.

use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ir::{AiRequest, Role};
use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel, effort_is_off};
use stravia_vendor_sdk::{DiscoveredModel, PluginError, ProviderSnapshot};

use crate::common;

pub fn supports_all_models(vendor_id: &str) -> bool {
    vendor_is(
        vendor_id,
        &[
            "baseten",
            "deepseek",
            "xiaomi",
            "xiaomi-token-plan-sgp",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
            "zai",
            "zai-coding-plan",
            "zhipuai",
            "zhipuai-coding-plan",
            "alibaba",
            "alibaba-cn",
            "alibaba-coding-plan",
            "alibaba-coding-plan-cn",
            "alibaba-token-plan",
            "alibaba-token-plan-cn",
        ],
    )
}

pub fn apply(
    vendor_id: &str,
    provider: &ProviderSnapshot,
    protocol: &str,
    request: &mut AiRequest,
) -> Result<(), PluginError> {
    if common::endpoint(protocol)? != OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        return Ok(());
    }

    let model = provider.model.as_deref().unwrap_or_default();
    apply_compatible_control(vendor_id, model, request);
    prepare_reasoning_history(vendor_id, model, request);
    Ok(())
}

pub fn decorate_discovered_model(vendor_id: &str, model: &mut DiscoveredModel) {
    let cohere_reasoning = vendor_is(vendor_id, &["cohere"])
        && (contains_ascii_case_insensitive(&model.id, "reasoning")
            || model
                .capabilities
                .iter()
                .any(|capability| capability.eq_ignore_ascii_case("reasoning")));
    if (supports_all_models(vendor_id)
        || cohere_reasoning
        || toggle_profile(vendor_id, &model.id).is_some())
        && !model.metadata.contains_key("reasoning_efforts")
    {
        model
            .metadata
            .insert("reasoning_efforts".into(), json!(["none", "high"]));
        model
            .metadata
            .insert("reasoning_efforts_fallback".into(), json!(true));
    }
}

/// Normalize upstream discovery rows before publishing the persistent model contract.
pub fn source_metadata(
    object: &serde_json::Map<String, Value>,
) -> std::collections::BTreeMap<String, Value> {
    let mut metadata = object
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "attachment"
                    | "reasoning"
                    | "tool_call"
                    | "structured_output"
                    | "temperature"
                    | "interleaved"
                    | "reasoning_options"
                    | "reasoning_levels"
                    | "thinking_toggle"
                    | "reasoning_efforts_fallback"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if let Some(limit) = metadata.get_mut("limit").and_then(Value::as_object_mut) {
        limit.retain(|key, _| key == "context");
    }
    let legacy_effort = object
        .get("reasoning_options")
        .and_then(|options| match options {
            Value::Array(options) => options
                .iter()
                .find(|option| option.get("type").and_then(Value::as_str) == Some("effort")),
            Value::Object(_) if options.get("type").and_then(Value::as_str) == Some("effort") => {
                Some(options)
            }
            _ => None,
        })
        .and_then(|option| option.get("values"));
    let efforts = object.get("reasoning_efforts").or(legacy_effort);
    if let Some(values) = efforts.and_then(Value::as_array) {
        let values = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| {
                !value.is_empty()
                    && !value.eq_ignore_ascii_case("default")
                    && !value.eq_ignore_ascii_case("null")
            })
            .map(|value| json!(value))
            .collect();
        metadata.insert("reasoning_efforts".into(), Value::Array(values));
    }
    metadata
}

fn prepare_reasoning_history(vendor_id: &str, model: &str, request: &mut AiRequest) {
    if !(vendor_is(vendor_id, &["deepseek"]) || contains_ascii_case_insensitive(model, "deepseek"))
        || request.tools.as_ref().is_none_or(Vec::is_empty)
    {
        return;
    }

    // DeepSeek accepts empty reasoning, but rejects null from streamed terminal
    // deltas when it is replayed as a complete assistant message.
    for item in &mut request.items {
        if item.role != Role::Assistant {
            continue;
        }
        let meta = item.meta.get_or_insert_with(Default::default);
        if meta.object_extensions().is_some()
            && meta.get("reasoning_content").is_none_or(Value::is_null)
        {
            meta.insert_extension("reasoning_content", Value::String(String::new()))
                .expect("reasoning content is not reserved");
        }
    }
}

fn apply_compatible_control(vendor_id: &str, model: &str, request: &mut AiRequest) -> bool {
    let Some(profile) = toggle_profile(vendor_id, model) else {
        return false;
    };
    let enabled = match request.reasoning.target_control.as_ref() {
        Some(TargetThinkingControl::Hidden) => return false,
        Some(TargetThinkingControl::Effort { value }) => !effort_is_off(value),
        None => match request.reasoning.level {
            None => return false,
            Some(ThinkingLevel::Off) => false,
            Some(_) => true,
        },
    };
    let (key, value) = encode_toggle(profile, enabled);
    // Route-resolved control is authoritative over raw client extensions.
    request.meta.vendor.egress.insert(key.to_owned(), value);
    if !enabled || !profile.needs_effort_companion() {
        request.reasoning.target_control = None;
        request.reasoning.level = None;
    }
    true
}

fn encode_toggle(profile: ToggleProfile, enabled: bool) -> (&'static str, Value) {
    match profile {
        ToggleProfile::Thinking => (
            "thinking",
            json!({"type": if enabled { "enabled" } else { "disabled" }}),
        ),
        ToggleProfile::PreservedThinking => (
            "thinking",
            if enabled {
                json!({"type": "enabled", "clear_thinking": false})
            } else {
                json!({"type": "disabled"})
            },
        ),
        ToggleProfile::EnableThinking => ("enable_thinking", Value::Bool(enabled)),
        ToggleProfile::ChatTemplateArgs => {
            ("chat_template_args", json!({"enable_thinking": enabled}))
        }
        ToggleProfile::ChatTemplateThinkingMode => (
            "chat_template_kwargs",
            json!({"thinking_mode": if enabled { "enabled" } else { "disabled" }}),
        ),
        ToggleProfile::AdaptiveThinking => (
            "thinking",
            json!({"type": if enabled { "adaptive" } else { "disabled" }}),
        ),
    }
}

#[derive(Clone, Copy)]
enum ToggleProfile {
    Thinking,
    PreservedThinking,
    EnableThinking,
    ChatTemplateArgs,
    ChatTemplateThinkingMode,
    AdaptiveThinking,
}

impl ToggleProfile {
    fn needs_effort_companion(self) -> bool {
        matches!(
            self,
            Self::PreservedThinking | Self::EnableThinking | Self::ChatTemplateArgs
        )
    }
}

fn toggle_profile(vendor_id: &str, model: &str) -> Option<ToggleProfile> {
    if contains_ascii_case_insensitive(model, "minimax-m3") {
        return if vendor_is(vendor_id, &["nvidia", "lilac"]) {
            Some(ToggleProfile::ChatTemplateThinkingMode)
        } else {
            Some(ToggleProfile::AdaptiveThinking)
        };
    }

    if vendor_is(vendor_id, &["baseten"])
        || (vendor_is(vendor_id, &["opencode", "opencode-go"])
            && (contains_ascii_case_insensitive(model, "kimi-k2-thinking")
                || contains_ascii_case_insensitive(model, "glm-4.6")))
    {
        return Some(ToggleProfile::ChatTemplateArgs);
    }

    if vendor_is(
        vendor_id,
        &[
            "deepseek",
            "xiaomi",
            "xiaomi-token-plan-sgp",
            "xiaomi-token-plan-cn",
            "xiaomi-token-plan-ams",
        ],
    ) {
        return Some(ToggleProfile::Thinking);
    }

    if vendor_is(
        vendor_id,
        &["zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan"],
    ) {
        return Some(ToggleProfile::PreservedThinking);
    }

    if vendor_is(
        vendor_id,
        &[
            "alibaba",
            "alibaba-cn",
            "alibaba-coding-plan",
            "alibaba-coding-plan-cn",
            "alibaba-token-plan",
            "alibaba-token-plan-cn",
        ],
    ) {
        return Some(ToggleProfile::EnableThinking);
    }

    None
}

fn vendor_is(vendor_id: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| vendor_id.eq_ignore_ascii_case(candidate))
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use stravia_protocol_codec::transform::ProtocolTransform;

    use super::*;

    fn provider(model: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "test-provider".into(),
            channel: "default".into(),
            base_url: "https://upstream.test/v1".into(),
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some(model.into()),
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    fn encoded_control(vendor_id: &str, model: &str, control: TargetThinkingControl) -> Value {
        let provider = provider(model);
        let mut request = AiRequest::new(model, Vec::new());
        request.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        request.reasoning.target_control = Some(control);
        apply(
            vendor_id,
            &provider,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string().as_str(),
            &mut request,
        )
        .unwrap();
        common::encode_inference_request(
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            request,
        )
        .unwrap()
        .body
    }

    #[test]
    fn provider_profiles_encode_their_wire_shapes() {
        let cases = [
            (
                "xiaomi",
                "mimo-v2-pro",
                "high",
                "thinking",
                json!({"type": "enabled"}),
                false,
            ),
            (
                "deepseek",
                "deepseek-v4",
                "none",
                "thinking",
                json!({"type": "disabled"}),
                false,
            ),
            (
                "zai",
                "glm-4.7",
                "high",
                "thinking",
                json!({"type": "enabled", "clear_thinking": false}),
                true,
            ),
            (
                "alibaba-cn",
                "qwen3.5-plus",
                "none",
                "enable_thinking",
                json!(false),
                false,
            ),
            (
                "baseten",
                "moonshotai/Kimi-K2.5",
                "high",
                "chat_template_args",
                json!({"enable_thinking": true}),
                true,
            ),
            (
                "opencode",
                "zai-coding-plan/glm-4.6",
                "off",
                "chat_template_args",
                json!({"enable_thinking": false}),
                false,
            ),
            (
                "nvidia",
                "minimaxai/minimax-m3",
                "high",
                "chat_template_kwargs",
                json!({"thinking_mode": "enabled"}),
                false,
            ),
            (
                "custom",
                "minimax-m3",
                "high",
                "thinking",
                json!({"type": "adaptive"}),
                false,
            ),
        ];

        for (vendor, model, effort, key, expected, keeps_reasoning_effort) in cases {
            let body = encoded_control(
                vendor,
                model,
                TargetThinkingControl::Effort {
                    value: effort.into(),
                },
            );
            assert_eq!(body[key], expected, "{vendor}/{model}");
            assert_eq!(
                body.get("reasoning_effort").is_some(),
                keeps_reasoning_effort,
                "{vendor}/{model} body={body}"
            );
        }
    }

    #[test]
    fn route_control_overrides_raw_ingress_extension() {
        let provider = provider("mimo-v2-pro");
        let mut request = AiRequest::new("mimo-v2-pro", Vec::new());
        request.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        request.reasoning.target_control = Some(TargetThinkingControl::Effort {
            value: "none".into(),
        });
        request
            .meta
            .vendor
            .ingress
            .insert("thinking".into(), json!({"type": "enabled"}));

        apply(
            "xiaomi",
            &provider,
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut request,
        )
        .unwrap();
        let body = common::encode_inference_request(
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            request,
        )
        .unwrap()
        .body;
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn effort_keeps_codec_field_and_adds_required_companion() {
        let body = encoded_control(
            "zai",
            "glm-5.2",
            TargetThinkingControl::Effort {
                value: "high".into(),
            },
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
    }

    #[test]
    fn discovery_ingress_preserves_native_facts_without_legacy_model_flags() {
        let value = json!({"reasoning":true,"reasoning_options":[{"type":"toggle"},{"type":"effort","values":[null,"default","low","custom"]}],"limit":{"context":8192,"output":512},"thinking_toggle":true,"native":{"mode":"real"}});
        let metadata = source_metadata(value.as_object().unwrap());
        assert_eq!(metadata["reasoning_efforts"], json!(["low", "custom"]));
        assert_eq!(metadata["limit"], json!({"context":8192}));
        assert!(!metadata.contains_key("reasoning"));
        assert!(!metadata.contains_key("reasoning_options"));
        assert!(!metadata.contains_key("thinking_toggle"));
        assert_eq!(metadata["native"], json!({"mode":"real"}));
    }

    #[test]
    fn hidden_target_control_is_omitted_without_losing_messages() {
        let mut message = stravia_runtime_contract::protocol::ir::AiItem::output_text("hello");
        message.role = stravia_runtime_contract::protocol::ir::Role::User;
        let mut unknown = AiRequest::new("custom-model", vec![message]);
        unknown.reasoning.target_control = Some(TargetThinkingControl::Hidden);
        apply(
            "custom",
            &provider("custom-model"),
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut unknown,
        )
        .unwrap();
        let encoded = common::encode_inference_request(
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            unknown,
        )
        .unwrap();
        assert_eq!(encoded.body["messages"][0]["role"], "user");
        assert_eq!(encoded.body["messages"][0]["content"], "hello");
        assert!(encoded.body.get("thinking").is_none());
        assert!(encoded.body.get("enable_thinking").is_none());
        assert!(encoded.body.get("reasoning").is_none());
        assert!(encoded.body.get("reasoning_effort").is_none());
    }

    #[test]
    fn deepseek_tool_history_preserves_native_reasoning_and_marks_missing_empty() {
        let pair = ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .unwrap();
        let mut request = pair
            .decode_request(json!({
                "model": "deepseek-v4-flash",
                "messages": [
                    {"role": "assistant", "content": "foreign answer"},
                    {"role": "assistant", "content": "native answer", "reasoning_content": "native reasoning"},
                    {"role": "user", "content": "continue"}
                ],
                "tools": [{
                    "type": "function",
                    "function": {"name": "lookup", "parameters": {"type": "object", "properties": {}}}
                }]
            }))
            .unwrap();
        let provider = provider("deepseek-v4-flash");
        apply(
            "deepseek",
            &provider,
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut request,
        )
        .unwrap();
        let body = pair.encode_request(request).unwrap().body;
        assert_eq!(body["messages"][0]["reasoning_content"], "");
        assert_eq!(body["messages"][1]["reasoning_content"], "native reasoning");
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_parallel_tool_continuation_normalizes_null_reasoning() {
        let pair = ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .unwrap();
        let mut request = pair
            .decode_request(json!({
                "model": "deepseek-flash",
                "messages": [
                    {"role": "user", "content": "Look up both keys and add their values."},
                    {
                        "role": "assistant", "content": "Checking both keys.", "reasoning_content": null,
                        "tool_calls": [
                            {"id": "lookup-a", "type": "function", "function": {"name": "lookup", "arguments": "{\"key\":\"A\"}"}},
                            {"id": "lookup-b", "type": "function", "function": {"name": "lookup", "arguments": "{\"key\":\"B\"}"}}
                        ]
                    },
                    {"role": "tool", "tool_call_id": "lookup-a", "content": "1"},
                    {"role": "tool", "tool_call_id": "lookup-b", "content": "2"}
                ],
                "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]
            }))
            .unwrap();
        apply(
            "deepseek",
            &provider("deepseek-flash"),
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut request,
        )
        .unwrap();
        let body = pair.encode_request(request).unwrap().body;
        let messages = body["messages"].as_array().unwrap();
        for message in messages
            .iter()
            .filter(|message| message["role"] == "assistant")
        {
            assert_eq!(message["reasoning_content"], "", "{message}");
        }
        let results = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| {
                (
                    message["tool_call_id"].as_str().unwrap(),
                    message["content"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(results, [("lookup-a", "1"), ("lookup-b", "2")]);
    }

    #[test]
    fn discovery_populates_default_off_and_high_for_toggle_profiles_without_overwriting() {
        let mut deepseek = DiscoveredModel {
            id: "deepseek-reasoner".into(),
            display_name: "DeepSeek".into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        decorate_discovered_model("deepseek", &mut deepseek);
        assert!(deepseek.capabilities.is_empty());
        assert_eq!(
            deepseek.metadata["reasoning_efforts"],
            json!(["none", "high"])
        );
        assert_eq!(deepseek.metadata["reasoning_efforts_fallback"], json!(true));

        let mut nvidia = DiscoveredModel {
            id: "minimaxai/minimax-m3".into(),
            display_name: "MiniMax M3".into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        decorate_discovered_model("nvidia", &mut nvidia);
        assert!(nvidia.capabilities.is_empty());
        assert_eq!(
            nvidia.metadata["reasoning_efforts"],
            json!(["none", "high"])
        );
        assert_eq!(nvidia.metadata["reasoning_efforts_fallback"], json!(true));

        let mut existing = DiscoveredModel {
            id: "glm-5.2".into(),
            display_name: "GLM".into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::from([("reasoning_efforts".into(), json!(["low", "high"]))]),
        };
        decorate_discovered_model("zai", &mut existing);
        assert_eq!(
            existing.metadata["reasoning_efforts"],
            json!(["low", "high"])
        );
        assert!(!existing.metadata.contains_key("reasoning_efforts_fallback"));

        let mut unrelated = DiscoveredModel {
            id: "other-model".into(),
            display_name: "Other".into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        decorate_discovered_model("nvidia", &mut unrelated);
        assert!(unrelated.capabilities.is_empty());
        assert!(!unrelated.metadata.contains_key("reasoning_efforts"));
    }
}
