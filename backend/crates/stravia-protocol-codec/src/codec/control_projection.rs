//! Positive projections of unified request controls; business schemas stay opaque.
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) enum Family {
    Chat,
    Responses,
    Anthropic,
    Google,
}

pub(super) fn project_request(body: &mut Value, family: Family) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let controls: &[(&str, &str)] = match family {
        Family::Chat => &[
            ("audio", "audio"),
            ("web_search_options", "web_search"),
            ("tool_choice", "chat_choice"),
        ],
        Family::Responses => &[
            ("context_management", "compaction"),
            ("tools", "responses_tool"),
            ("tool_choice", "responses_choice"),
        ],
        Family::Anthropic => &[
            ("context_management", "context"),
            ("tools", "anthropic_tool"),
            ("output_config", "output_config"),
            ("cache_control", "cache"),
            ("thinking", "anthropic_thinking"),
        ],
        Family::Google => &[
            ("generationConfig", "generation"),
            ("toolConfig", "tool_config"),
            ("tools", "google_tool"),
        ],
    };
    for (key, shape) in controls {
        if let Some(value) = object.get_mut(*key) {
            project(value, shape);
        }
    }
}

fn project(value: &mut Value, shape: &str) {
    if let Value::Array(items) = value {
        if shape == "compaction" {
            items.retain(|item| item.get("type").and_then(Value::as_str) == Some("compaction"));
        }
        for item in items {
            project(item, shape);
        }
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let fields: &[(&str, &str)] = match shape {
        "audio" => &[("voice", ""), ("format", "")],
        "web_search" => &[("search_context_size", ""), ("user_location", "location")],
        "location" => &[("type", ""), ("approximate", "approximate")],
        "approximate" => &[
            ("city", ""),
            ("country", ""),
            ("region", ""),
            ("timezone", ""),
        ],
        "search_location" => &[
            ("type", ""),
            ("city", ""),
            ("country", ""),
            ("region", ""),
            ("timezone", ""),
        ],
        "chat_choice" => &[("type", ""), ("function", "named")],
        "named" => &[("name", "")],
        "compaction" => &[("type", ""), ("compact_threshold", "")],
        "responses_choice" => &[
            ("type", ""),
            ("name", ""),
            ("mode", ""),
            ("tools", "responses_choice_tool"),
        ],
        "responses_choice_tool" => &[("type", ""), ("name", "")],
        "responses_tool" => &[
            ("type", ""),
            ("name", ""),
            ("description", ""),
            ("parameters", ""),
            ("strict", ""),
            ("defer_loading", ""),
            ("search_context_size", ""),
            ("user_location", "search_location"),
            ("filters", "search_filters"),
            ("container", ""),
            ("execution", ""),
            ("action", ""),
            ("output_format", ""),
            ("size", ""),
            ("quality", ""),
            ("format", "custom_format"),
            ("tools", "responses_tool"),
        ],
        "search_filters" => &[("allowed_domains", "")],
        "custom_format" => &[("type", ""), ("syntax", ""), ("definition", "")],
        "context" => &[("edits", "edit")],
        "edit" => &[
            ("type", ""),
            ("trigger", "threshold"),
            ("keep", "threshold"),
            ("clear_at_least", "threshold"),
            ("clear_tool_inputs", ""),
            ("exclude_tools", ""),
        ],
        "threshold" => &[("type", ""), ("value", "")],
        "anthropic_tool" => &[
            ("type", ""),
            ("name", ""),
            ("description", ""),
            ("input_schema", ""),
            ("strict", ""),
            ("defer_loading", ""),
            ("max_uses", ""),
            ("allowed_domains", ""),
            ("blocked_domains", ""),
            ("user_location", "search_location"),
        ],
        "output_config" => &[("effort", ""), ("format", "anthropic_format")],
        "anthropic_format" => &[("type", ""), ("schema", "")],
        "cache" => &[("type", ""), ("ttl", "")],
        "anthropic_thinking" => &[("type", ""), ("budget_tokens", ""), ("display", "")],
        "generation" => &[
            ("temperature", ""),
            ("topP", ""),
            ("topK", ""),
            ("candidateCount", ""),
            ("maxOutputTokens", ""),
            ("stopSequences", ""),
            ("responseMimeType", ""),
            ("responseModalities", ""),
            ("responseSchema", ""),
            ("responseJsonSchema", ""),
            ("seed", ""),
            ("presencePenalty", ""),
            ("frequencyPenalty", ""),
            ("responseLogprobs", ""),
            ("logprobs", ""),
            ("thinkingConfig", "thinking"),
            ("speechConfig", "speech"),
        ],
        "thinking" => &[
            ("thinkingBudget", ""),
            ("thinkingLevel", ""),
            ("includeThoughts", ""),
        ],
        "speech" => &[("voiceConfig", "voice")],
        "voice" => &[("prebuiltVoiceConfig", "prebuilt")],
        "prebuilt" => &[("voiceName", "")],
        "tool_config" => &[("functionCallingConfig", "calling")],
        "calling" => &[("mode", ""), ("allowedFunctionNames", "")],
        "google_tool" => &[
            ("functionDeclarations", "declaration"),
            ("googleSearch", "google_search"),
            ("codeExecution", "empty"),
        ],
        "declaration" => &[
            ("name", ""),
            ("description", ""),
            ("parameters", ""),
            ("parametersJsonSchema", ""),
        ],
        "google_search" => &[("excludeDomains", "")],
        _ => &[],
    };
    object
        .retain(|key, value| !value.is_null() && fields.iter().any(|(allowed, _)| key == allowed));
    for (key, child) in fields {
        if !child.is_empty()
            && let Some(value) = object.get_mut(*key)
        {
            project(value, child);
        }
    }
}
