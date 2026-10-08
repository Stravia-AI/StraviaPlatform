//! Exact brand identities backed by the WebUI's original SVG assets.

macro_rules! icon {
    ($file:literal) => {
        include_str!(concat!(
            "../../../../frontend/stravia-webui/src/assets/icons/",
            $file,
            ".svg"
        ))
    };
}

/// Returns an embedded brand mark, never a protocol implementation's brand.
///
/// Keep registry keys aligned with `assets/icons/index.ts`; aliases below are
/// explicit provider/catalog identities, not substring or package matches.
pub fn provider_svg(id: &str) -> Option<&'static str> {
    Some(match id {
        "aicodemirror" => icon!("aicodemirror"),
        "aigocode" => icon!("algocode"),
        "aihubmix" => icon!("aihubmix-color"),
        "alibaba"
        | "alibaba-cn"
        | "alibaba-coding-plan"
        | "alibaba-coding-plan-cn"
        | "alibaba-token-plan"
        | "alibaba-token-plan-cn" => icon!("alibaba"),
        "anthropic" | "claude-code" => icon!("anthropic"),
        "aws" | "amazon-bedrock" => icon!("aws"),
        "azure" => icon!("azure"),
        "baidu" => icon!("baidu"),
        "bailian" => icon!("bailian"),
        "bytedance" => icon!("bytedance"),
        "catcoder" => icon!("catcoder"),
        "chatglm" => icon!("chatglm"),
        "claude" => icon!("claude"),
        "cloudflare" | "cloudflare-ai-gateway" | "cloudflare-workers-ai" => icon!("cloudflare"),
        "cohere" => icon!("cohere"),
        "copilot" => icon!("copilot"),
        "cubence" => icon!("cubence"),
        "deepseek" => icon!("deepseek"),
        "doubao" => icon!("doubao"),
        "gemini" => icon!("gemini"),
        "gemma" => icon!("gemma"),
        "github" | "github-models" => icon!("github"),
        "githubcopilot" | "github-copilot" => icon!("githubcopilot"),
        "google" => icon!("google"),
        "googlecloud" | "google-vertex" | "google-vertex-anthropic" => icon!("googlecloud"),
        "grok" => icon!("grok"),
        "huawei" => icon!("huawei"),
        "huggingface" => icon!("huggingface"),
        "hunyuan" => icon!("hunyuan"),
        "kimi" | "kimi-for-coding" | "moonshotai" | "moonshotai-cn" => icon!("kimi"),
        "longcat" => icon!("longcat-color"),
        "mcp" => icon!("mcp"),
        "meta" => icon!("meta"),
        "midjourney" => icon!("midjourney"),
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => {
            icon!("minimax")
        }
        "mistral" => icon!("mistral"),
        "modelscope" => icon!("modelscope-color"),
        "newapi" => icon!("newapi"),
        "notion" => icon!("notion"),
        "nvidia" => icon!("nvidia"),
        "ollama" => icon!("ollama"),
        "openai" | "openai-codex" => icon!("openai"),
        "opencode" | "opencode-go" | "opencode-free" => icon!("opencode-logo-light"),
        "openrouter" => icon!("openrouter"),
        "packycode" => icon!("packycode"),
        "palm" => icon!("palm"),
        "perplexity" | "perplexity-agent" => icon!("perplexity"),
        "qwen" => icon!("qwen"),
        "rc" => icon!("rc"),
        "siliconflow" | "siliconflow-cn" => icon!("siliconflow"),
        "stability" => icon!("stability"),
        "tencent" | "tencent-coding-plan" | "tencent-token-plan" | "tencent-tokenhub" => {
            icon!("tencent")
        }
        "vercel" | "gateway" | "v0" => icon!("vercel"),
        "wenxin" => icon!("wenxin"),
        "xai" | "xai-grok" => icon!("xai"),
        "xiaomimimo"
        | "xiaomi"
        | "xiaomi-token-plan-ams"
        | "xiaomi-token-plan-cn"
        | "xiaomi-token-plan-sgp" => icon!("xiaomimimo"),
        "yi" => icon!("yi"),
        "zeroone" => icon!("zeroone"),
        "zai" | "zai-coding-plan" => icon!("zai"),
        "zhipu" | "zhipuai" | "zhipuai-coding-plan" => icon!("zhipu"),
        _ => return None,
    })
}
