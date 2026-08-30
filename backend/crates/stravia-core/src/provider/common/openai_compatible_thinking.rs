//! Provider-specific thinking controls for OpenAI-compatible Chat Completions.
//!
//! `reasoning_effort` is portable enough for the protocol codec. Toggle-style
//! controls are not: compatible providers use several different body shapes.
//! Keep that provider knowledge at the provider boundary and fail closed when
//! an unknown provider has no declared wire shape.

use serde_json::{Value, json};

use crate::db::models::Provider;
use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
use crate::protocol::ir::AiRequest;
use crate::provider::vendor::ProviderCtx;
use crate::thinking::TargetThinkingControl;

pub(crate) fn supports_toggle(provider: &Provider, model: &str) -> bool {
    let provider_id = provider_identity(provider.preset_key.as_deref(), provider.vendor.as_deref());
    toggle_profile(provider_id, model).is_some()
}

pub(crate) fn apply(ctx: &ProviderCtx<'_>, req: &mut AiRequest) {
    if ctx.protocol != OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1 {
        return;
    }

    let provider_id = provider_identity(
        ctx.provider.preset_key.as_deref(),
        ctx.provider.vendor.as_deref(),
    );
    apply_compatible_control(provider_id, ctx.actual_model, req);
}

fn provider_identity<'a>(
    preset_key: Option<&'a str>,
    vendor_id: Option<&'a str>,
) -> Option<&'a str> {
    // Catalog entries share the `openai-compatible` adapter vendor, while
    // `preset_key` retains the upstream identity used by these wire rules.
    preset_key.or(vendor_id)
}

fn apply_compatible_control(vendor_id: Option<&str>, model: &str, req: &mut AiRequest) -> bool {
    if matches!(
        req.reasoning.target_control,
        Some(TargetThinkingControl::Effort { .. })
    ) && let Some(profile) = toggle_profile(vendor_id, model)
        && profile.needs_effort_companion()
    {
        let (key, value) = encode_toggle(profile, true);
        req.meta.vendor.ingress.insert(key.to_string(), value);
        return true;
    }

    apply_compatible_toggle(vendor_id, model, req)
}

fn apply_compatible_toggle(vendor_id: Option<&str>, model: &str, req: &mut AiRequest) -> bool {
    let enabled = match req.reasoning.target_control.as_ref() {
        Some(TargetThinkingControl::Enabled) => true,
        Some(TargetThinkingControl::Disabled) => false,
        _ => return false,
    };

    let Some((key, value)) = translated_toggle(vendor_id, model, enabled) else {
        return false;
    };

    // The route-resolved target control is authoritative over a raw ingress
    // extension supplied by the client.
    req.meta.vendor.ingress.insert(key.to_string(), value);
    req.reasoning.target_control = None;
    true
}

fn translated_toggle(
    vendor_id: Option<&str>,
    model: &str,
    enabled: bool,
) -> Option<(&'static str, Value)> {
    let profile = toggle_profile(vendor_id, model)?;
    Some(encode_toggle(profile, enabled))
}

fn encode_toggle(profile: ToggleProfile, enabled: bool) -> (&'static str, Value) {
    match profile {
        ToggleProfile::Thinking => (
            "thinking",
            json!({ "type": if enabled { "enabled" } else { "disabled" } }),
        ),
        ToggleProfile::PreservedThinking => (
            "thinking",
            if enabled {
                json!({ "type": "enabled", "clear_thinking": false })
            } else {
                json!({ "type": "disabled" })
            },
        ),
        ToggleProfile::EnableThinking => ("enable_thinking", Value::Bool(enabled)),
        ToggleProfile::ChatTemplateArgs => {
            ("chat_template_args", json!({ "enable_thinking": enabled }))
        }
        ToggleProfile::ChatTemplateThinkingMode => (
            "chat_template_kwargs",
            json!({
                "thinking_mode": if enabled { "enabled" } else { "disabled" }
            }),
        ),
        ToggleProfile::AdaptiveThinking => (
            "thinking",
            json!({
                "type": if enabled { "adaptive" } else { "disabled" }
            }),
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

fn toggle_profile(vendor_id: Option<&str>, model: &str) -> Option<ToggleProfile> {
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

fn vendor_is(vendor_id: Option<&str>, candidates: &[&str]) -> bool {
    vendor_id.is_some_and(|vendor_id| {
        candidates
            .iter()
            .any(|candidate| vendor_id.eq_ignore_ascii_case(candidate))
    })
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::transform::ProtocolTransform;

    fn request(control: TargetThinkingControl) -> AiRequest {
        let mut req = AiRequest::new("upstream-model", Vec::new());
        req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        req.reasoning.target_control = Some(control);
        req
    }

    #[test]
    fn catalog_preset_identifies_the_upstream_provider() {
        assert_eq!(
            provider_identity(Some("alibaba-cn"), Some("openai-compatible")),
            Some("alibaba-cn")
        );
        assert_eq!(provider_identity(None, Some("xiaomi")), Some("xiaomi"));
    }

    #[test]
    fn known_provider_toggles_use_their_compatible_wire_shapes() {
        let cases = [
            (
                "xiaomi",
                "mimo-v2-pro",
                true,
                "thinking",
                json!({"type": "enabled"}),
            ),
            (
                "deepseek",
                "deepseek-v4",
                false,
                "thinking",
                json!({"type": "disabled"}),
            ),
            (
                "zai",
                "glm-4.7",
                true,
                "thinking",
                json!({"type": "enabled", "clear_thinking": false}),
            ),
            (
                "alibaba-cn",
                "qwen3.5-plus",
                false,
                "enable_thinking",
                json!(false),
            ),
            (
                "baseten",
                "moonshotai/Kimi-K2.5",
                true,
                "chat_template_args",
                json!({"enable_thinking": true}),
            ),
            (
                "opencode",
                "zai-coding-plan/glm-4.6",
                false,
                "chat_template_args",
                json!({"enable_thinking": false}),
            ),
            (
                "nvidia",
                "minimaxai/minimax-m3",
                true,
                "chat_template_kwargs",
                json!({"thinking_mode": "enabled"}),
            ),
            (
                "custom-compatible",
                "minimax-m3",
                true,
                "thinking",
                json!({"type": "adaptive"}),
            ),
        ];

        for (vendor, model, enabled, key, expected) in cases {
            let control = if enabled {
                TargetThinkingControl::Enabled
            } else {
                TargetThinkingControl::Disabled
            };
            let mut req = request(control);

            assert!(
                apply_compatible_toggle(Some(vendor), model, &mut req),
                "{vendor}/{model} should have a provider-specific mapping"
            );
            assert_eq!(req.meta.vendor.ingress.get(key), Some(&expected));
            assert_eq!(req.reasoning.target_control, None);
        }
    }

    #[test]
    fn unknown_provider_keeps_toggle_untranslated() {
        let mut req = request(TargetThinkingControl::Enabled);

        assert!(!apply_compatible_toggle(
            Some("custom-compatible"),
            "custom-model",
            &mut req
        ));
        assert_eq!(
            req.reasoning.target_control,
            Some(TargetThinkingControl::Enabled)
        );
        assert!(req.meta.vendor.ingress.is_empty());
    }

    #[test]
    fn effort_control_remains_owned_by_the_protocol_codec() {
        let mut req = request(TargetThinkingControl::Effort {
            value: "high".into(),
        });

        assert!(!apply_compatible_toggle(
            Some("xiaomi"),
            "mimo-v2.5",
            &mut req
        ));
        assert_eq!(
            req.reasoning.target_control,
            Some(TargetThinkingControl::Effort {
                value: "high".into()
            })
        );
        assert!(req.meta.vendor.ingress.is_empty());
    }

    #[test]
    fn effort_control_keeps_codec_mapping_and_adds_provider_enablement() {
        let mut req = request(TargetThinkingControl::Effort {
            value: "high".into(),
        });

        assert!(apply_compatible_control(Some("zai"), "glm-5.2", &mut req));
        assert_eq!(
            req.reasoning.target_control,
            Some(TargetThinkingControl::Effort {
                value: "high".into()
            })
        );
        assert_eq!(
            req.meta.vendor.ingress.get("thinking"),
            Some(&json!({"type": "enabled", "clear_thinking": false}))
        );

        let pair = ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .expect("OpenAI-compatible transform exists");
        let encoded = pair.encode_request(&req).expect("request encodes");
        assert_eq!(encoded.body["reasoning_effort"], "high");
        assert_eq!(
            encoded.body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
    }

    #[test]
    fn translated_toggle_is_encoded_once_without_generic_thinking_field() {
        let mut req = request(TargetThinkingControl::Enabled);
        assert!(apply_compatible_toggle(
            Some("alibaba-cn"),
            "qwen3.5-plus",
            &mut req
        ));

        let pair = ProtocolTransform::global()
            .bind(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            )
            .expect("OpenAI-compatible transform exists");
        let encoded = pair.encode_request(&req).expect("request encodes");

        assert_eq!(encoded.body["enable_thinking"], json!(true));
        assert!(encoded.body.get("thinking").is_none());
    }
}
