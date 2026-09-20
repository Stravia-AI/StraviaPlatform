use crate::db::models::Provider;
use crate::media_generation::{AspectRatio, GenerationError, Resolution};
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::{
    ids::OPEN_RESPONSES_2026_04_24,
    ir::{
        AiItem, AiRequest, AiResponse, ContentBlock, MediaSource, MessageContent, OpenResponsesExt,
        ProtocolExt, Role, StreamConfig, ToolChoice,
    },
};

pub(crate) fn image_request(
    route: String,
    prompt: String,
    references: Vec<String>,
    aspect_ratio: Option<AspectRatio>,
    resolution: Option<Resolution>,
) -> AiRequest {
    let action = if references.is_empty() {
        "generate"
    } else {
        "edit"
    };
    let mut content = Vec::with_capacity(references.len() + 1);
    content.push(ContentBlock::Text {
        text: prompt,
        cache_control: None,
    });
    content.extend(references.into_iter().map(|reference| ContentBlock::Image {
        source: MediaSource::Url(reference),
        detail: None,
        cache_control: None,
    }));
    let mut tool = json!({"type":"image_generation","action":action,"output_format":"png"});
    if let Some(size) = mapped_size(aspect_ratio, resolution) {
        tool["size"] = Value::String(size.to_owned());
    }
    let mut request = AiRequest::new(
        route,
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(content),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream = StreamConfig {
        enabled: true,
        include_usage: true,
    };
    request.parallel_tool_calls = Some(false);
    request.tool_choice = Some(ToolChoice::Raw(json!({"type":"image_generation"})));
    request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
        passthrough_tools: vec![tool],
        store: Some(false),
        include: Some(Vec::new()),
        ..Default::default()
    }));
    request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    request
}

fn mapped_size(
    aspect_ratio: Option<AspectRatio>,
    resolution: Option<Resolution>,
) -> Option<&'static str> {
    if aspect_ratio.is_none() && resolution.is_none() {
        return None;
    }
    // OMP hosted Responses 链路只给出这三个尺寸；不能把独立 Images API 的
    // 2K/4K 参数当作 Codex 保证。更高档位按相同构图钳制到已核定尺寸。
    Some(match aspect_ratio.unwrap_or(AspectRatio::Square) {
        AspectRatio::Square => "1024x1024",
        AspectRatio::Portrait | AspectRatio::Tall => "1024x1536",
        AspectRatio::Landscape | AspectRatio::Wide => "1536x1024",
    })
}

pub(crate) fn image_result(mut response: AiResponse) -> Result<String, GenerationError> {
    let mut image = None;
    for item in &mut response.items {
        let Some(raw) = item.unknown_mut() else {
            continue;
        };
        if raw.get("type").and_then(Value::as_str) != Some("image_generation_call") {
            continue;
        }
        if image.is_some() || raw.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(GenerationError::new(
                "invalid_generation_output",
                "Image generation must return exactly one completed image",
            ));
        }
        let Some(Value::String(encoded)) = raw.as_object_mut().and_then(|raw| raw.remove("result"))
        else {
            return Err(GenerationError::new(
                "invalid_generation_output",
                "Image generation returned no image",
            ));
        };
        image = Some(encoded);
    }
    image.ok_or_else(|| {
        GenerationError::new(
            "invalid_generation_output",
            "Image generation returned no image",
        )
    })
}

/// Returns whether a Target can use the ChatGPT-backed Responses image tool.
///
/// Credential liveness is intentionally checked by media-generation config at
/// save and execution time because the Provider snapshot does not contain its
/// OAuth credential row.
pub(crate) fn eligible(provider: &Provider, upstream_model: &str) -> bool {
    provider.is_enabled
        && provider.vendor.as_deref() == Some("openai")
        && provider.preset_key.as_deref() == Some("openai")
        && provider.channel.as_deref() == Some("codex")
        && provider.protocol == "open-responses"
        && provider.effective_auth_mode() == "oauth"
        && supported_model(upstream_model)
}

fn supported_model(model: &str) -> bool {
    let model = model.trim();
    if model.is_empty() || model == "*" {
        return false;
    }
    let Some(version) = model.strip_prefix("gpt-") else {
        return false;
    };
    version
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 5)
}
