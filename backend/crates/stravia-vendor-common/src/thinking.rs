//! Shared OpenAI-compatible thinking controls for vendor guests.
//!
//! Effort is represented by the shared codec. Toggle controls are translated
//! here because compatible providers use incompatible request shapes.

use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use stravia_runtime_contract::protocol::ir::{AiRequest, Role};
use stravia_runtime_contract::thinking::TargetThinkingControl;
use stravia_vendor_sdk::{
    DiscoveredModel, MODEL_CAPABILITY_THINKING_TOGGLE, PluginError, ProviderSnapshot,
};

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
    channel: &str,
    provider: &ProviderSnapshot,
    protocol: &str,
    request: &mut AiRequest,
) -> Result<(), PluginError> {
    if common::endpoint(protocol)? != OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        return Ok(());
    }

    let model = provider.model.as_deref().unwrap_or_default();
    let authorized = toggle_authorized(vendor_id, channel, provider);
    apply_compatible_control(vendor_id, model, authorized, request);
    prepare_reasoning_history(vendor_id, model, request);
    Ok(())
}

pub fn decorate_discovered_model(vendor_id: &str, model: &mut DiscoveredModel) {
    if explicitly_disables_reasoning(&model.metadata) {
        model
            .capabilities
            .retain(|capability| capability != MODEL_CAPABILITY_THINKING_TOGGLE);
        if let Some(capabilities) = model
            .metadata
            .get_mut("capabilities")
            .and_then(Value::as_array_mut)
        {
            capabilities
                .retain(|capability| capability.as_str() != Some(MODEL_CAPABILITY_THINKING_TOGGLE));
        }
        return;
    }
    if !supports_all_models(vendor_id)
        && toggle_profile(vendor_id, &model.id).is_some()
        && !model
            .capabilities
            .iter()
            .any(|capability| capability == MODEL_CAPABILITY_THINKING_TOGGLE)
    {
        model
            .capabilities
            .push(MODEL_CAPABILITY_THINKING_TOGGLE.to_owned());
    }
}

fn toggle_authorized(vendor_id: &str, channel: &str, provider: &ProviderSnapshot) -> bool {
    if provider
        .model_metadata
        .as_ref()
        .is_some_and(|metadata| explicitly_disables_reasoning(&metadata.extensions))
    {
        return false;
    }

    let model_declares_capability = provider.model_metadata.as_ref().is_some_and(|metadata| {
        metadata
            .capabilities
            .iter()
            .any(|capability| capability == MODEL_CAPABILITY_THINKING_TOGGLE)
            || metadata
                .extensions
                .get("capabilities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|capability| capability == MODEL_CAPABILITY_THINKING_TOGGLE)
    });
    model_declares_capability || (channel == "default" && supports_all_models(vendor_id))
}

fn explicitly_disables_reasoning(metadata: &std::collections::BTreeMap<String, Value>) -> bool {
    metadata.get("reasoning").and_then(Value::as_bool) == Some(false)
}

fn prepare_reasoning_history(vendor_id: &str, model: &str, request: &mut AiRequest) {
    if !(vendor_is(vendor_id, &["deepseek"]) || contains_ascii_case_insensitive(model, "deepseek"))
        || request.tools.as_ref().is_none_or(Vec::is_empty)
    {
        return;
    }

    // DeepSeek requires every assistant history item to carry this field when
    // tools are present. Empty means that foreign reasoning was not captured.
    for item in &mut request.items {
        if item.role != Role::Assistant {
            continue;
        }
        let meta = item.meta.get_or_insert_with(|| json!({}));
        if let Some(meta) = meta.as_object_mut() {
            meta.entry("reasoning_content")
                .or_insert_with(|| Value::String(String::new()));
        }
    }
}

fn apply_compatible_control(
    vendor_id: &str,
    model: &str,
    authorized: bool,
    request: &mut AiRequest,
) -> bool {
    if !authorized {
        return false;
    }
    if matches!(
        request.reasoning.target_control,
        Some(TargetThinkingControl::Effort { .. })
    ) && let Some(profile) = toggle_profile(vendor_id, model)
        && profile.needs_effort_companion()
    {
        let (key, value) = encode_toggle(profile, true);
        request.meta.vendor.ingress.insert(key.to_owned(), value);
        return true;
    }

    apply_compatible_toggle(vendor_id, model, request)
}

fn apply_compatible_toggle(vendor_id: &str, model: &str, request: &mut AiRequest) -> bool {
    let enabled = match request.reasoning.target_control.as_ref() {
        Some(TargetThinkingControl::Enabled) => true,
        Some(TargetThinkingControl::Disabled) => false,
        _ => return false,
    };
    let Some(profile) = toggle_profile(vendor_id, model) else {
        return false;
    };
    let (key, value) = encode_toggle(profile, enabled);

    // Route-resolved control is authoritative over raw client extensions.
    request.meta.vendor.ingress.insert(key.to_owned(), value);
    request.reasoning.target_control = None;
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
    use stravia_vendor_sdk::ModelMetadata;

    use super::*;

    fn provider(model: &str, declared: bool) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: "test-provider".into(),
            channel: "default".into(),
            base_url: "https://upstream.test/v1".into(),
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some(model.into()),
            model_metadata: declared.then(|| ModelMetadata {
                capabilities: vec![MODEL_CAPABILITY_THINKING_TOGGLE.into()],
                ..ModelMetadata::default()
            }),
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::new(),
        }
    }

    fn encoded_control(
        vendor_id: &str,
        model: &str,
        control: TargetThinkingControl,
        declared: bool,
    ) -> Value {
        let provider = provider(model, declared);
        let mut request = AiRequest::new(model, Vec::new());
        request.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        request.reasoning.target_control = Some(control);
        apply(
            vendor_id,
            "default",
            &provider,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string().as_str(),
            &mut request,
        )
        .unwrap();
        common::encode_inference_request(
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &request,
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
                true,
                "thinking",
                json!({"type": "enabled"}),
                false,
            ),
            (
                "deepseek",
                "deepseek-v4",
                false,
                "thinking",
                json!({"type": "disabled"}),
                false,
            ),
            (
                "zai",
                "glm-4.7",
                true,
                "thinking",
                json!({"type": "enabled", "clear_thinking": false}),
                false,
            ),
            (
                "alibaba-cn",
                "qwen3.5-plus",
                false,
                "enable_thinking",
                json!(false),
                false,
            ),
            (
                "baseten",
                "moonshotai/Kimi-K2.5",
                true,
                "chat_template_args",
                json!({"enable_thinking": true}),
                false,
            ),
            (
                "opencode",
                "zai-coding-plan/glm-4.6",
                false,
                "chat_template_args",
                json!({"enable_thinking": false}),
                true,
            ),
            (
                "nvidia",
                "minimaxai/minimax-m3",
                true,
                "chat_template_kwargs",
                json!({"thinking_mode": "enabled"}),
                true,
            ),
            (
                "custom",
                "minimax-m3",
                true,
                "thinking",
                json!({"type": "adaptive"}),
                true,
            ),
        ];

        for (vendor, model, enabled, key, expected, declared) in cases {
            let control = if enabled {
                TargetThinkingControl::Enabled
            } else {
                TargetThinkingControl::Disabled
            };
            let body = encoded_control(vendor, model, control, declared);
            assert_eq!(body[key], expected, "{vendor}/{model}");
        }
    }

    #[test]
    fn route_control_overrides_raw_ingress_extension() {
        let provider = provider("mimo-v2-pro", false);
        let mut request = AiRequest::new("mimo-v2-pro", Vec::new());
        request.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        request.reasoning.target_control = Some(TargetThinkingControl::Disabled);
        request
            .meta
            .vendor
            .ingress
            .insert("thinking".into(), json!({"type": "enabled"}));

        apply(
            "xiaomi",
            "default",
            &provider,
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut request,
        )
        .unwrap();
        let body = common::encode_inference_request(
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &request,
        )
        .unwrap()
        .body;
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn effort_keeps_codec_field_and_adds_required_companion() {
        let body = encoded_control(
            "zai",
            "glm-5.2",
            TargetThinkingControl::Effort {
                value: "high".into(),
            },
            false,
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
    }

    #[test]
    fn undeclared_or_explicitly_disabled_toggle_fails_closed() {
        let mut unknown = AiRequest::new("custom-model", Vec::new());
        unknown.reasoning.target_control = Some(TargetThinkingControl::Enabled);
        apply(
            "custom",
            "default",
            &provider("custom-model", false),
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut unknown,
        )
        .unwrap();
        assert!(
            common::encode_inference_request(
                &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
                &unknown
            )
            .is_err()
        );

        let mut disabled_provider = provider("mimo-v2-pro", false);
        disabled_provider.model_metadata = Some(ModelMetadata {
            extensions: BTreeMap::from([("reasoning".into(), Value::Bool(false))]),
            ..ModelMetadata::default()
        });
        let mut disabled = AiRequest::new("mimo-v2-pro", Vec::new());
        disabled.reasoning.target_control = Some(TargetThinkingControl::Enabled);
        apply(
            "xiaomi",
            "default",
            &disabled_provider,
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut disabled,
        )
        .unwrap();
        assert!(
            common::encode_inference_request(
                &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
                &disabled
            )
            .is_err()
        );
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
        let provider = provider("deepseek-v4-flash", false);
        apply(
            "deepseek",
            "default",
            &provider,
            &OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            &mut request,
        )
        .unwrap();
        let body = pair.encode_request(&request).unwrap().body;
        assert_eq!(body["messages"][0]["reasoning_content"], "");
        assert_eq!(body["messages"][1]["reasoning_content"], "native reasoning");
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn discovery_marks_only_model_specific_profiles() {
        let mut nvidia = DiscoveredModel {
            id: "minimaxai/minimax-m3".into(),
            display_name: "MiniMax M3".into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata: BTreeMap::new(),
        };
        decorate_discovered_model("nvidia", &mut nvidia);
        assert_eq!(
            nvidia.capabilities,
            vec![MODEL_CAPABILITY_THINKING_TOGGLE.to_owned()]
        );

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
    }
}
