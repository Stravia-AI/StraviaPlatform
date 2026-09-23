//! Shared Provider Catalog vocabulary: the `npm` implementation table both
//! sides of the contract use to decide which catalog entries are servable.
//! The guest registers a profile only for packages it can map; the host
//! parses the same table so a registered profile always resolves to a
//! catalog adapter — a package neither side maps is dropped, never half
//! registered.

/// Catalog `npm` package → catalog adapter identity. `None` marks an entry
/// no consumer of this contract can serve.
pub fn adapter_id_for_package(package: &str) -> Option<&'static str> {
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

/// Catalog `npm` package → egress protocol the catalog advertises for the
/// entry. Callers check [`adapter_id_for_package`] first; unmapped packages
/// never reach this table.
pub fn protocol_for_package(package: &str, provider_id: &str) -> Option<String> {
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
        _ if adapter_id_for_package(package).is_some() => "openai-compatible",
        _ if provider_id == "openai" => "open-responses",
        _ if provider_id == "anthropic" => "anthropic-messages",
        _ if provider_id == "google" => "google-gemini",
        _ if provider_id == "xai" => "openai-compatible",
        _ => return None,
    };
    Some(protocol.to_string())
}

/// Catalog adapter → default upstream origin used when the index entry does
/// not declare `api`.
pub fn adapter_default_base_url(adapter_id: &str) -> Option<&'static str> {
    match adapter_id {
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
