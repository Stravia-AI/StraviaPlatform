use std::collections::BTreeSet;
use stravia_vendor_common::thinking;

use serde_json::Value;
use stravia_vendor_sdk::{
    Capability, ChannelDescriptor, ConfigField, ConfigFieldKind, DataCompatibility,
    MODEL_CAPABILITY_THINKING_TOGGLE, NetworkDeclaration, OriginDeclaration, ProviderDescriptor,
};

pub(crate) const PROTOCOL_GEMINI: &str = "protocol-gemini";
pub(crate) const PROTOCOL_OPENAI_CHAT: &str = "protocol-openai-chat-completions";
pub(crate) const PROTOCOL_OPEN_RESPONSES: &str = "protocol-open-responses";
pub(crate) const PROTOCOL_ANTHROPIC: &str = "protocol-anthropic-messages";

#[derive(Debug, Clone, Copy)]
pub(crate) struct BundledCatalogProfile {
    pub(crate) id: &'static str,
    pub(crate) npm: &'static str,
    pub(crate) name: &'static str,
    pub(crate) api: Option<&'static str>,
}

include!(concat!(env!("OUT_DIR"), "/catalog_profiles.rs"));

pub(crate) fn bundled_catalog_profiles() -> &'static [BundledCatalogProfile] {
    BUNDLED_CATALOG_PROFILES
}

pub(crate) fn bundled_catalog_profile(provider_id: &str) -> Option<&'static BundledCatalogProfile> {
    BUNDLED_CATALOG_PROFILES
        .binary_search_by(|profile| profile.id.cmp(provider_id))
        .ok()
        .map(|index| &BUNDLED_CATALOG_PROFILES[index])
}

pub const CATALOG_VENDOR_IDS: &[&str] = &[
    "aihubmix",
    "alibaba",
    "alibaba-cn",
    "alibaba-coding-plan",
    "alibaba-coding-plan-cn",
    "alibaba-token-plan",
    "alibaba-token-plan-cn",
    "amazon-bedrock",
    "anthropic",
    "azure",
    "baseten",
    "cerebras",
    "cloudflare-ai-gateway",
    "cohere",
    "command-code",
    "custom",
    "deepinfra",
    "devin",
    "gateway",
    "gitlab",
    "google",
    "google-vertex",
    "google-vertex-anthropic",
    "groq",
    "lilac",
    "merge-gateway",
    "mistral",
    "nvidia",
    "ollama",
    "openai",
    "openai-compatible",
    "opencode",
    "openrouter",
    "perplexity",
    "qvac",
    "salad-cloud",
    "sap-ai-core",
    "togetherai",
    "venice",
    "vercel",
    "watsonx",
    "xai",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "zai",
    "zhipuai",
];

pub(crate) fn descriptor(vendor_id: &str) -> Option<ProviderDescriptor> {
    let (display_name, mut channels, fields, network) = match vendor_id {
        PROTOCOL_GEMINI => protocol_vendor("Gemini Protocol", "google-gemini"),
        PROTOCOL_OPENAI_CHAT => protocol_vendor("OpenAI-Compatible Protocol", "openai-compatible"),
        PROTOCOL_OPEN_RESPONSES => protocol_vendor("Open Responses Protocol", "open-responses"),
        PROTOCOL_ANTHROPIC => protocol_vendor("Anthropic Messages Protocol", "anthropic-messages"),
        "alibaba" => standard(
            "Alibaba",
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
        ),
        "alibaba-cn" => standard(
            "Alibaba (China)",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        ),
        "alibaba-coding-plan" => standard(
            "Alibaba Coding Plan",
            Some("https://coding-intl.dashscope.aliyuncs.com/v1"),
        ),
        "alibaba-coding-plan-cn" => standard(
            "Alibaba Coding Plan (China)",
            Some("https://coding.dashscope.aliyuncs.com/v1"),
        ),
        "alibaba-token-plan" => standard(
            "Alibaba Token Plan",
            Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"),
        ),
        "alibaba-token-plan-cn" => standard(
            "Alibaba Token Plan (China)",
            Some("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"),
        ),
        "aihubmix" => standard("AIHubMix", Some("https://aihubmix.com/v1")),
        "baseten" => standard("Baseten", Some("https://inference.baseten.co/v1")),
        "cerebras" => standard("Cerebras", Some("https://api.cerebras.ai/v1")),
        "custom" => custom_standard(),
        "deepinfra" => standard("DeepInfra", Some("https://api.deepinfra.com/v1/openai")),
        "deepseek" => (
            "DeepSeek",
            vec![channel(
                "default",
                "Default",
                Some("openai-compatible"),
                Some("https://api.deepseek.com"),
                inference_capabilities(true, false),
            )],
            vec![optional_api_key()],
            NetworkDeclaration {
                extra_origins: vec![origin("https", "api.deepseek.com")],
                ..NetworkDeclaration::default()
            },
        ),
        "groq" => standard("Groq", Some("https://api.groq.com/openai/v1")),
        "lilac" => standard("Lilac", Some("https://api.getlilac.com/v1")),
        "merge-gateway" => standard(
            "Merge Gateway",
            Some("https://api-gateway.merge.dev/v1/ai-sdk"),
        ),
        "mistral" => standard("Mistral AI", Some("https://api.mistral.ai/v1")),
        "nvidia" => standard("Nvidia", Some("https://integrate.api.nvidia.com/v1")),
        "opencode" => standard("OpenCode Zen", Some("https://opencode.ai/zen/v1")),
        "perplexity" => standard("Perplexity", Some("https://api.perplexity.ai")),
        "qvac" => standard("Qvac", Some("http://127.0.0.1:11435/v1")),
        "salad-cloud" => standard("SaladCloud", Some("https://ai.salad.cloud/v1")),
        "togetherai" => standard("Together AI", Some("https://api.together.xyz/v1")),
        "venice" => standard("Venice AI", Some("https://api.venice.ai/api/v1")),
        "vercel" => standard("v0", Some("https://api.v0.dev/v1")),
        "openai-compatible" => standard("OpenAI Compatible", None),
        "cohere" => codec_vendor(
            "Cohere",
            "cohere-chat",
            Some("https://api.cohere.com/v2"),
            vec![optional_api_key()],
        ),
        "gateway" => codec_vendor(
            "Vercel AI Gateway",
            "gateway-language-model",
            Some("https://ai-gateway.vercel.sh/v4/ai"),
            vec![optional_api_key()],
        ),
        "google" => codec_vendor(
            "Google",
            "google-gemini",
            Some("https://generativelanguage.googleapis.com"),
            vec![optional_api_key()],
        ),
        "ollama" => (
            "Ollama",
            vec![channel(
                "default",
                "Default",
                Some("openai-compatible"),
                Some("http://127.0.0.1:11434/v1"),
                inference_capabilities(false, false),
            )],
            vec![optional_api_key()],
            NetworkDeclaration::default(),
        ),
        "openrouter" => (
            "OpenRouter",
            vec![channel(
                "default",
                "Default",
                Some("openai-compatible"),
                Some("https://openrouter.ai/api/v1"),
                inference_capabilities(false, false),
            )],
            vec![
                optional_api_key(),
                string_field("httpReferer", "App referer URL", false, false),
                string_field("xTitle", "App title", false, false),
            ],
            NetworkDeclaration::default(),
        ),
        "cloudflare-ai-gateway" => (
            "Cloudflare AI Gateway",
            vec![channel(
                "default",
                "Default",
                Some("openai-compatible"),
                None,
                {
                    let mut capabilities = inference_capabilities(false, false);
                    capabilities.insert(Capability::ConfigValidation);
                    capabilities
                },
            )],
            vec![
                string_field("apiToken", "AI Gateway API token", true, true),
                string_field("accountId", "Cloudflare account ID", false, true),
                string_field("gatewayId", "Gateway ID", false, true),
            ],
            NetworkDeclaration {
                extra_origins: vec![origin("https", "gateway.ai.cloudflare.com")],
                ..NetworkDeclaration::default()
            },
        ),
        "github-copilot" => monitored_vendor(
            "GitHub Copilot",
            "openai-compatible",
            "https://api.githubcopilot.com",
            &["api.github.com"],
        ),
        "kimi-for-coding" => monitored_vendor(
            "Kimi for Coding",
            "anthropic-messages",
            "https://api.kimi.com/coding/v1",
            &["api.kimi.com"],
        ),
        "nano-gpt" => monitored_vendor(
            "NanoGPT",
            "openai-compatible",
            "https://nano-gpt.com/api/v1",
            &["nano-gpt.com"],
        ),
        "zai-coding-plan" => monitored_vendor(
            "Z.ai Coding Plan",
            "openai-compatible",
            "https://api.z.ai/api/coding/paas/v4",
            &["api.z.ai"],
        ),
        "zhipuai-coding-plan" => monitored_vendor(
            "Zhipu AI Coding Plan",
            "openai-compatible",
            "https://open.bigmodel.cn/api/coding/paas/v4",
            &["open.bigmodel.cn"],
        ),
        "minimax-coding-plan" => monitored_vendor(
            "MiniMax Coding Plan",
            "anthropic-messages",
            "https://api.minimax.io/anthropic/v1",
            &["api.minimax.io"],
        ),
        "minimax-cn-coding-plan" => monitored_vendor(
            "MiniMax CN Coding Plan",
            "anthropic-messages",
            "https://api.minimaxi.com/anthropic/v1",
            &["api.minimaxi.com", "www.minimaxi.com"],
        ),
        "wafer.ai" => monitored_vendor(
            "Wafer AI",
            "openai-compatible",
            "https://pass.wafer.ai/v1",
            &["pass.wafer.ai"],
        ),
        "opencode-go" => monitored_vendor(
            "OpenCode Go",
            "openai-compatible",
            "https://opencode.ai/zen/go/v1",
            &["opencode.ai"],
        ),
        "crof" => monitored_vendor(
            "Crof",
            "openai-compatible",
            "https://crof.ai/v1",
            &["crof.ai"],
        ),
        "neuralwatt" => monitored_vendor(
            "NeuralWatt",
            "openai-compatible",
            "https://api.neuralwatt.com/v1",
            &["api.neuralwatt.com"],
        ),
        "xiaomi" => standard("Xiaomi", Some("https://api.xiaomimimo.com/v1")),
        "xiaomi-token-plan-sgp" => standard(
            "Xiaomi Token Plan (Singapore)",
            Some("https://token-plan-sgp.xiaomimimo.com/v1"),
        ),
        "xiaomi-token-plan-cn" => standard(
            "Xiaomi Token Plan (China)",
            Some("https://token-plan-cn.xiaomimimo.com/v1"),
        ),
        "xiaomi-token-plan-ams" => standard(
            "Xiaomi Token Plan (Europe)",
            Some("https://token-plan-ams.xiaomimimo.com/v1"),
        ),
        "zai" => standard("Z.AI", Some("https://api.z.ai/api/paas/v4")),
        "zhipuai" => standard("Zhipu AI", Some("https://open.bigmodel.cn/api/paas/v4")),
        "xai" => {
            let mut descriptor = standard("xAI", Some("https://api.x.ai/v1"));
            descriptor.1[0].capabilities.insert(Capability::Compact);
            descriptor
        }
        _ => return None,
    };
    if thinking::supports_all_models(vendor_id) {
        for channel in &mut channels {
            channel
                .model_capabilities
                .insert(MODEL_CAPABILITY_THINKING_TOGGLE.to_owned());
        }
    }
    let capabilities = channels
        .iter()
        .flat_map(|channel| channel.capabilities.iter().copied())
        .collect();
    Some(ProviderDescriptor {
        provider_id: vendor_id.to_owned(),
        catalog_id: CATALOG_VENDOR_IDS
            .contains(&vendor_id)
            .then(|| vendor_id.to_owned()),
        display_name: display_name.to_owned(),
        description: Some(format!("Built-in {display_name} vendor component")),
        channels,
        capabilities,
        config_fields: fields,
        network,
        data_compat: DataCompatibility::default(),
    })
}

pub(crate) fn compatible_catalog_descriptor(
    profile: &BundledCatalogProfile,
    protocol: &str,
    fallback_base_url: Option<&str>,
) -> ProviderDescriptor {
    let mut capabilities = inference_capabilities(false, false);
    if protocol == "open-responses" {
        capabilities.insert(Capability::Compact);
    }
    let mut channels = vec![channel(
        "default",
        "Default",
        Some(protocol),
        profile.api.or(fallback_base_url),
        capabilities.clone(),
    )];
    if thinking::supports_all_models(profile.id) {
        channels[0]
            .model_capabilities
            .insert(MODEL_CAPABILITY_THINKING_TOGGLE.to_owned());
    }
    ProviderDescriptor {
        provider_id: profile.id.to_owned(),
        catalog_id: Some(profile.id.to_owned()),
        display_name: profile.name.to_owned(),
        description: Some(format!("Built-in {} vendor component", profile.name)),
        channels,
        capabilities,
        config_fields: vec![optional_api_key()],
        network: NetworkDeclaration::default(),
        data_compat: DataCompatibility::default(),
    }
}

fn monitored_vendor(
    display_name: &'static str,
    protocol: &'static str,
    default_base_url: &'static str,
    allowance_hosts: &[&str],
) -> (
    &'static str,
    Vec<ChannelDescriptor>,
    Vec<ConfigField>,
    NetworkDeclaration,
) {
    (
        display_name,
        vec![channel(
            "default",
            "Default",
            Some(protocol),
            Some(default_base_url),
            inference_capabilities(true, false),
        )],
        vec![optional_api_key()],
        NetworkDeclaration {
            extra_origins: allowance_hosts
                .iter()
                .map(|host| origin("https", host))
                .collect(),
            ..NetworkDeclaration::default()
        },
    )
}

fn protocol_vendor(
    display_name: &'static str,
    protocol: &'static str,
) -> (
    &'static str,
    Vec<ChannelDescriptor>,
    Vec<ConfigField>,
    NetworkDeclaration,
) {
    let mut descriptor = codec_vendor(display_name, protocol, None, vec![optional_api_key()]);
    if protocol == "open-responses" {
        descriptor.1[0].capabilities.insert(Capability::Compact);
    }
    descriptor
}

fn custom_standard() -> (
    &'static str,
    Vec<ChannelDescriptor>,
    Vec<ConfigField>,
    NetworkDeclaration,
) {
    let mut descriptor = standard("Custom", None);
    descriptor.1[0].capabilities.insert(Capability::Compact);
    descriptor
}

fn standard(
    display_name: &'static str,
    default_base_url: Option<&'static str>,
) -> (
    &'static str,
    Vec<ChannelDescriptor>,
    Vec<ConfigField>,
    NetworkDeclaration,
) {
    codec_vendor(
        display_name,
        "openai-compatible",
        default_base_url,
        vec![optional_api_key()],
    )
}

fn codec_vendor(
    display_name: &'static str,
    protocol: &'static str,
    default_base_url: Option<&'static str>,
    fields: Vec<ConfigField>,
) -> (
    &'static str,
    Vec<ChannelDescriptor>,
    Vec<ConfigField>,
    NetworkDeclaration,
) {
    (
        display_name,
        vec![channel(
            "default",
            "Default",
            Some(protocol),
            default_base_url,
            inference_capabilities(false, false),
        )],
        fields,
        NetworkDeclaration::default(),
    )
}

fn inference_capabilities(allowance: bool, oauth: bool) -> BTreeSet<Capability> {
    let mut values = BTreeSet::from([Capability::Infer, Capability::ModelDiscovery]);
    if allowance {
        values.insert(Capability::Allowance);
    }
    if oauth {
        values.insert(Capability::AuthOauth);
    }
    values
}

fn channel(
    id: &str,
    name: &str,
    protocol: Option<&str>,
    default_base_url: Option<&str>,
    capabilities: BTreeSet<Capability>,
) -> ChannelDescriptor {
    ChannelDescriptor {
        id: id.to_owned(),
        name: name.to_owned(),
        description: None,
        auth: None,
        protocol: protocol.map(str::to_owned),
        default_base_url: default_base_url.map(str::to_owned),
        default_models_source: None,
        capabilities,
        model_capabilities: BTreeSet::new(),
        search_model_required: false,
    }
}

fn optional_api_key() -> ConfigField {
    string_field("apiKey", "API key", true, false)
}

fn string_field(key: &str, label: &str, secret: bool, required: bool) -> ConfigField {
    ConfigField {
        key: key.to_owned(),
        label: label.to_owned(),
        description: None,
        kind: ConfigFieldKind::String { multiline: false },
        required,
        default_json: None::<Value>,
        group: Some(if secret { "Credentials" } else { "Connection" }.to_owned()),
        secret,
        min: None,
        max: None,
        max_length: Some(16_384),
        pattern: None,
        visible_when: None,
    }
}

fn origin(scheme: &str, host: &str) -> OriginDeclaration {
    OriginDeclaration {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        port: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW_VENDORS: [(&str, &str); 16] = [
        ("baseten", "https://inference.baseten.co/v1"),
        ("lilac", "https://api.getlilac.com/v1"),
        ("nvidia", "https://integrate.api.nvidia.com/v1"),
        ("opencode", "https://opencode.ai/zen/v1"),
        ("xiaomi", "https://api.xiaomimimo.com/v1"),
        (
            "xiaomi-token-plan-sgp",
            "https://token-plan-sgp.xiaomimimo.com/v1",
        ),
        (
            "xiaomi-token-plan-cn",
            "https://token-plan-cn.xiaomimimo.com/v1",
        ),
        (
            "xiaomi-token-plan-ams",
            "https://token-plan-ams.xiaomimimo.com/v1",
        ),
        ("zai", "https://api.z.ai/api/paas/v4"),
        ("zhipuai", "https://open.bigmodel.cn/api/paas/v4"),
        (
            "alibaba",
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        ),
        (
            "alibaba-cn",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ),
        (
            "alibaba-coding-plan",
            "https://coding-intl.dashscope.aliyuncs.com/v1",
        ),
        (
            "alibaba-coding-plan-cn",
            "https://coding.dashscope.aliyuncs.com/v1",
        ),
        (
            "alibaba-token-plan",
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        ),
        (
            "alibaba-token-plan-cn",
            "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        ),
    ];

    #[test]
    fn new_catalog_profiles_use_verified_defaults_and_optional_api_keys() {
        for (vendor_id, expected_url) in NEW_VENDORS {
            assert!(CATALOG_VENDOR_IDS.contains(&vendor_id));
            let descriptor = crate::provider_descriptor(vendor_id).unwrap();
            let channel = descriptor.channels.first().unwrap();
            assert_eq!(channel.id, "default", "{vendor_id}");
            assert_eq!(
                channel.protocol.as_deref(),
                Some("openai-compatible"),
                "{vendor_id}"
            );
            assert_eq!(
                channel.default_base_url.as_deref(),
                Some(expected_url),
                "{vendor_id}"
            );
            assert!(
                descriptor
                    .config_fields
                    .iter()
                    .any(|field| { field.key == "apiKey" && !field.required && field.secret }),
                "{vendor_id}"
            );
        }
    }

    #[test]
    fn generic_profiles_declare_native_compaction_only_when_open_responses_is_selectable() {
        for vendor_id in ["custom", PROTOCOL_OPEN_RESPONSES] {
            let descriptor = crate::provider_descriptor(vendor_id).unwrap();
            assert!(descriptor.capabilities.contains(&Capability::Compact));
            assert!(
                descriptor.channels[0]
                    .capabilities
                    .contains(&Capability::Compact)
            );
        }
        for vendor_id in [PROTOCOL_OPENAI_CHAT, PROTOCOL_ANTHROPIC, PROTOCOL_GEMINI] {
            let descriptor = crate::provider_descriptor(vendor_id).unwrap();
            assert!(!descriptor.capabilities.contains(&Capability::Compact));
        }
    }

    #[test]
    fn only_all_model_profiles_declare_channel_toggle_support() {
        for vendor_id in [
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
        ] {
            let descriptor = crate::provider_descriptor(vendor_id).unwrap();
            assert!(
                descriptor.channels[0]
                    .model_capabilities
                    .contains(MODEL_CAPABILITY_THINKING_TOGGLE),
                "{vendor_id}"
            );
        }
        for vendor_id in ["lilac", "nvidia", "opencode", "opencode-go"] {
            let descriptor = crate::provider_descriptor(vendor_id).unwrap();
            assert!(
                !descriptor.channels[0]
                    .model_capabilities
                    .contains(MODEL_CAPABILITY_THINKING_TOGGLE),
                "{vendor_id}"
            );
        }
    }
}
