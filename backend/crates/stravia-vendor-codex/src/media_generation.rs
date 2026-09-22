use std::collections::BTreeMap;

use base64::Engine;
use serde_json::{Value, json};
use stravia_vendor_sdk::{
    GuestHost, MediaArtifact, MediaImageAspectRatio, MediaImageRequest, MediaImageResolution,
    MediaImageResponse, ProviderSnapshot,
};

use super::{codex, endpoint, ensure_success, invalid, require_codex_protocol, required_model};

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENCODED_IMAGE_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
// A streamed Responses payload can contain the image in both item completion
// and the terminal response snapshot. Keep both bounded while allowing the
// host's full decoded-image budget.
const MAX_RESPONSE_BYTES: usize = MAX_ENCODED_IMAGE_BYTES * 2 + 2 * 1024 * 1024;

pub(super) fn generate(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: MediaImageRequest,
) -> Result<MediaImageResponse, stravia_vendor_sdk::PluginError> {
    if request.prompt.trim().is_empty() {
        return Err(invalid("image prompt must not be empty"));
    }
    require_codex_protocol(provider)?;
    let action = if request.references.is_empty() {
        "generate"
    } else {
        "edit"
    };
    let mut content = vec![json!({"type":"input_text", "text":request.prompt})];
    for reference in &request.references {
        let media_type = reference.media_type.trim().to_ascii_lowercase();
        if !media_type.starts_with("image/") || reference.bytes.is_empty() {
            return Err(invalid(
                "Codex reference images must contain non-empty image bytes",
            ));
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&reference.bytes);
        content.push(json!({
            "type":"input_image",
            "image_url":format!("data:{media_type};base64,{encoded}")
        }));
    }
    let mut tool = json!({
        "type":"image_generation",
        "action":action,
        "output_format":"png"
    });
    if let Some(size) = mapped_size(request.aspect_ratio, request.resolution) {
        tool["size"] = Value::String(size.into());
    }
    if let Some(quality) = mapped_quality(request.resolution) {
        tool["quality"] = Value::String(quality.into());
    }
    let model = required_model(provider)?;
    if !supported_model(model) {
        return Err(invalid(
            "Codex image generation requires a GPT-5 or newer model",
        ));
    }
    let mut instructions = if action == "edit" {
        "Use the available image generation tool to edit or create exactly one PNG image for the user request. Treat the provided input images as ordered edit/reference images. Do not use any other tool.".to_owned()
    } else {
        "Use the available image generation tool to generate exactly one PNG image for the user request. Do not use any other tool.".to_owned()
    };
    if let Some(aspect_ratio) = request.aspect_ratio {
        instructions.push_str(" Preserve the requested ");
        instructions.push_str(aspect_ratio_label(aspect_ratio));
        instructions.push_str(" aspect ratio as closely as the image tool permits.");
    }
    if let Some(resolution) = request.resolution {
        instructions.push_str(" Treat ");
        instructions.push_str(resolution_label(resolution));
        instructions.push_str(" as the requested output resolution tier and preserve as much detail as the image tool permits.");
    }
    let mut body = json!({
        "model":model,
        "instructions":instructions,
        "input":[{"role":"user", "content":content}],
        "tools":[tool],
        "tool_choice":{"type":"image_generation"},
        "parallel_tool_calls":false,
        "store":false,
        "stream":true,
        "include":[]
    });
    codex::prepare_body(&mut body)?;
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
    ];
    codex::append_runtime_headers(provider, &body, &mut headers)?;
    host.emit_started()?;
    let response = host.http_start(stravia_vendor_sdk::wit::types::HttpRequest {
        method: "POST".into(),
        url: endpoint(&provider.base_url, "/responses"),
        headers,
        body: serde_json::to_vec(&body)
            .map_err(|_| invalid("Codex image request could not be encoded"))?,
    })?;
    ensure_success(&response, "Codex image generation")?;
    let bytes = stravia_vendor_sdk::read_http_body(&response, MAX_RESPONSE_BYTES)?;
    let response = codex::parse_responses_body(&bytes)?;
    image_result(&response)
}

fn mapped_size(
    aspect_ratio: Option<MediaImageAspectRatio>,
    resolution: Option<MediaImageResolution>,
) -> Option<&'static str> {
    match aspect_ratio {
        Some(MediaImageAspectRatio::Portrait | MediaImageAspectRatio::Tall) => Some("1024x1536"),
        Some(MediaImageAspectRatio::Landscape | MediaImageAspectRatio::Wide) => Some("1536x1024"),
        Some(MediaImageAspectRatio::Square) => Some("1024x1024"),
        None if resolution.is_some() => Some("1024x1024"),
        None => None,
    }
}

fn mapped_quality(resolution: Option<MediaImageResolution>) -> Option<&'static str> {
    match resolution {
        Some(MediaImageResolution::OneK) => Some("low"),
        Some(MediaImageResolution::TwoK) => Some("medium"),
        Some(MediaImageResolution::FourK) => Some("high"),
        None => None,
    }
}

fn aspect_ratio_label(aspect_ratio: MediaImageAspectRatio) -> &'static str {
    match aspect_ratio {
        MediaImageAspectRatio::Square => "1:1",
        MediaImageAspectRatio::Portrait => "3:4",
        MediaImageAspectRatio::Landscape => "4:3",
        MediaImageAspectRatio::Tall => "9:16",
        MediaImageAspectRatio::Wide => "16:9",
    }
}

fn resolution_label(resolution: MediaImageResolution) -> &'static str {
    match resolution {
        MediaImageResolution::OneK => "1K",
        MediaImageResolution::TwoK => "2K",
        MediaImageResolution::FourK => "4K",
    }
}

fn image_result(response: &Value) -> Result<MediaImageResponse, stravia_vendor_sdk::PluginError> {
    let mut image = None;
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("image_generation_call"))
    {
        if image.is_some() || item.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(invalid(
                "image generation must return exactly one completed image",
            ));
        }
        let encoded = item
            .get("result")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("image generation returned no image bytes"))?;
        let encoded = encoded
            .split_once(",")
            .filter(|(prefix, _)| prefix.starts_with("data:image/"))
            .map_or(encoded, |(_, data)| data);
        if encoded.len() > MAX_ENCODED_IMAGE_BYTES {
            return Err(invalid("image generation exceeded the 32 MiB image limit"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| invalid("image generation returned invalid base64"))?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(invalid("image generation returned an invalid image size"));
        }
        let (width, height) = png_dimensions(&bytes)
            .ok_or_else(|| invalid("image generation did not return a valid PNG"))?;
        let mut metadata = BTreeMap::new();
        metadata.insert("format".into(), Value::String("png".into()));
        metadata.insert("width".into(), Value::from(width));
        metadata.insert("height".into(), Value::from(height));
        if let Some(model) = response.get("model").and_then(Value::as_str) {
            metadata.insert("model".into(), Value::String(model.into()));
        }
        image = Some(MediaArtifact {
            media_type: "image/png".into(),
            bytes,
            upstream_ref: item.get("id").and_then(Value::as_str).map(str::to_owned),
            metadata,
        });
    }
    let artifact = image.ok_or_else(|| invalid("image generation returned no image"))?;
    let revised_prompt = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|item| item.get("revised_prompt").and_then(Value::as_str))
        .map(str::to_owned);
    Ok(MediaImageResponse {
        artifacts: vec![artifact],
        revised_prompt,
        usage: codex::usage_from_response(response),
    })
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    ))
}

pub(super) fn supported_model(model: &str) -> bool {
    let Some(version) = model.trim().strip_prefix("gpt-") else {
        return false;
    };
    version
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 5)
}
