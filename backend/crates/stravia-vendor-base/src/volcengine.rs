use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{AiErrorKind, Usage};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    Capability, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost, MediaArtifact,
    MediaImageAspectRatio, MediaImageRequest, MediaImageResolution, MediaImageResponse,
    OriginDeclaration, PluginError, ProviderDescriptor, ProviderSnapshot, read_http_body,
};

const MODELS_LIMIT: usize = 8 * 1024 * 1024;
const REQUEST_LIMIT: usize = 64 * 1024 * 1024;
const REFERENCE_LIMIT: usize = 30 * 1024 * 1024;
const IMAGE_LIMIT: usize = 32 * 1024 * 1024;
const RESPONSE_LIMIT: usize = IMAGE_LIMIT.div_ceil(3) * 4 + 2 * 1024 * 1024;
const MAX_REFERENCES: usize = 14;

pub(crate) fn supports(id: &str) -> bool {
    matches!(
        id,
        "volcengine"
            | "volcengine-coding-plan"
            | "volcengine-agent-plan"
            | "volcengine-token-plan"
            | "volcengine-plan"
            | "byteplus"
    )
}

pub(crate) fn configure_descriptor(descriptor: &mut ProviderDescriptor) {
    if !supports(&descriptor.provider_id) {
        return;
    }
    if let Some(channel) = descriptor.channels.iter_mut().find(|c| c.id == "default") {
        channel.capabilities.insert(Capability::MediaImage);
        descriptor.capabilities.insert(Capability::MediaImage);
    }
    let host = if descriptor.provider_id == "byteplus" {
        "ark.ap-southeast.bytepluses.com"
    } else {
        "ark.cn-beijing.volces.com"
    };
    let origin = OriginDeclaration {
        scheme: "https".into(),
        host: host.into(),
        port: None,
    };
    if !descriptor.network.extra_origins.contains(&origin) {
        descriptor.network.extra_origins.push(origin);
    }
}

#[derive(Clone)]
pub(crate) struct DiscoveryContext {
    provider_id: String,
    base_url: String,
    api_key: Option<String>,
}

impl DiscoveryContext {
    pub(crate) fn from_provider(provider: &ProviderSnapshot) -> Self {
        Self {
            provider_id: provider.provider_id.clone(),
            base_url: provider.base_url.clone(),
            api_key: api_key(provider).map(str::to_owned),
        }
    }
}

fn api_key(provider: &ProviderSnapshot) -> Option<&str> {
    provider
        .credentials
        .get("apiKey")
        .or_else(|| provider.options.get("apiKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    Standard,
    AgentPlan,
    CodingPlan,
}

fn classify_endpoint(provider_id: &str, base_url: &str) -> EndpointKind {
    if let Ok(url) = url::Url::parse(base_url) {
        let path = url.path().trim_end_matches('/');
        if path.ends_with("/api/plan/v3")
            || path.ends_with("/api/plan")
            || path.ends_with("/api/plan/v1")
        {
            return EndpointKind::AgentPlan;
        }
        if path.ends_with("/api/coding/v3")
            || path.ends_with("/api/coding")
            || path.ends_with("/api/coding/v1")
        {
            return EndpointKind::CodingPlan;
        }
    }
    match provider_id {
        "volcengine-agent-plan" | "volcengine-token-plan" | "volcengine-plan" => {
            EndpointKind::AgentPlan
        }
        "volcengine-coding-plan" => EndpointKind::CodingPlan,
        _ => EndpointKind::Standard,
    }
}

fn normalized_api_url(base: &str) -> Result<url::Url, PluginError> {
    let mut url = url::Url::parse(base).map_err(|_| invalid("Volcengine base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("Volcengine base URL is invalid"));
    }
    let path = url.path().trim_end_matches('/');
    let normalized = if let Some(prefix) = path.strip_suffix("/api/plan/v1") {
        format!("{prefix}/api/plan/v3")
    } else if path.ends_with("/api/plan") {
        format!("{path}/v3")
    } else if let Some(prefix) = path.strip_suffix("/api/coding/v1") {
        format!("{prefix}/api/coding/v3")
    } else if path.ends_with("/api/coding") {
        format!("{path}/v3")
    } else {
        path.to_owned()
    };
    url.set_path(&normalized);
    Ok(url)
}

fn images_endpoint(base: &str) -> Result<String, PluginError> {
    let mut url = normalized_api_url(base)?;
    let prefix = url.path().trim_end_matches('/');
    let path = format!("{prefix}/images/generations");
    url.set_path(&path);
    Ok(url.into())
}

fn models_endpoint(base: &str) -> Result<String, PluginError> {
    let mut url = normalized_api_url(base)?;
    let prefix = url.path().trim_end_matches('/');
    let path = format!("{prefix}/models");
    url.set_path(&path);
    Ok(url.into())
}

pub(crate) fn discover(
    host: &GuestHost,
    ctx: &DiscoveryContext,
    mut response: DiscoverResponse,
    restrict_to_existing: bool,
) -> Result<DiscoverResponse, PluginError> {
    ensure_existing_image_capabilities(&mut response);
    let kind = classify_endpoint(&ctx.provider_id, &ctx.base_url);
    if kind == EndpointKind::CodingPlan {
        // Volcengine Coding Plan (/api/coding/v3) does not expose /images/generations;
        // do not advertise uncallable image models from /api/coding/v3/models.
        return Ok(response);
    }
    let Some(key) = ctx.api_key.as_deref() else {
        if kind == EndpointKind::AgentPlan {
            return Ok(response);
        }
        return Err(common::plugin_error(
            ErrorKind::Auth,
            "Volcengine apiKey is missing",
        ));
    };
    let url = models_endpoint(&ctx.base_url)?;
    let http_response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "GET".into(),
            url,
            headers: crate::fields(vec![
                ("authorization".into(), format!("Bearer {key}")),
                ("accept".into(), "application/json".into()),
            ]),
            body: vec![],
        },
        &crate::net_options(),
    )?;
    let status = http_response.status()?;
    let headers = http_response.headers()?;
    let body = read_http_body(&http_response, MODELS_LIMIT)?;
    if status == 404 && kind == EndpointKind::AgentPlan {
        // Agent Plan (/api/plan/v3) currently returns 404 on /models; preserve
        // catalog or explicit discovery rather than breaking text model sync.
        return Ok(response);
    }
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    let document: Value = serde_json::from_slice(&body)
        .map_err(|_| upstream("Volcengine models response is invalid JSON"))?;
    let discovered = parse_image_models(&document)?;
    merge_models(&mut response, &discovered, restrict_to_existing);
    Ok(response)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageModelEntry {
    id: String,
    supports_image_input: bool,
}

fn parse_image_models(document: &Value) -> Result<Vec<ImageModelEntry>, PluginError> {
    let rows = document
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("Volcengine models response has no data array"))?;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let is_image_model = row
            .pointer("/modalities/output_modalities")
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|v| v.as_str() == Some("image")))
            || row
                .pointer("/api_supported/images~1generations")
                .and_then(Value::as_bool)
                == Some(true)
            || row.get("domain").and_then(Value::as_str) == Some("ImageGeneration");
        if !is_image_model {
            continue;
        }
        let supports_image_input = row
            .pointer("/modalities/input_modalities")
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|v| v.as_str() == Some("image")))
            || row
                .get("task_type")
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|v| v.as_str() == Some("image_to_image")));
        if seen.insert(id.to_owned()) {
            entries.push(ImageModelEntry {
                id: id.to_owned(),
                supports_image_input,
            });
        }
    }
    Ok(entries)
}

fn ensure_existing_image_capabilities(response: &mut DiscoverResponse) {
    for model in &mut response.models {
        let outputs_image = model
            .metadata
            .get("modalities")
            .and_then(|m| m.get("output"))
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|v| v.as_str() == Some("image")));
        if outputs_image && !model.capabilities.iter().any(|c| c == "media_image") {
            model.capabilities.push("media_image".into());
        }
    }
}

fn merge_models(
    response: &mut DiscoverResponse,
    models: &[ImageModelEntry],
    restricted: bool,
) {
    for entry in models {
        let edit = entry.supports_image_input;
        if let Some(model) = response.models.iter_mut().find(|m| m.id == entry.id) {
            if !model.capabilities.iter().any(|c| c == "media_image") {
                model.capabilities.push("media_image".into());
            }
            let modalities = model
                .metadata
                .entry("modalities".into())
                .or_insert_with(|| json!({}));
            if !modalities.is_object() {
                *modalities = json!({});
            }
            for (key, additions) in [
                (
                    "input",
                    if edit {
                        vec!["text", "image"]
                    } else {
                        vec!["text"]
                    },
                ),
                ("output", vec!["image"]),
            ] {
                let field = modalities
                    .as_object_mut()
                    .unwrap()
                    .entry(key)
                    .or_insert_with(|| json!([]));
                if !field.is_array() {
                    *field = json!([]);
                }
                let array = field.as_array_mut().unwrap();
                if key == "input" && !edit {
                    array.retain(|value| value.as_str() != Some("image"));
                }
                for addition in additions {
                    if !array.iter().any(|v| v.as_str() == Some(addition)) {
                        array.push(json!(addition));
                    }
                }
            }
        } else if !restricted && response.next_cursor.is_none() {
            response.models.push(DiscoveredModel {
                id: entry.id.clone(),
                display_name: entry.id.clone(),
                family: None,
                selector: Some(entry.id.clone()),
                capabilities: vec!["media_image".into()],
                metadata: [(
                    "modalities".into(),
                    json!({
                        "input": if edit { vec!["text", "image"] } else { vec!["text"] },
                        "output": ["image"]
                    }),
                )]
                .into_iter()
                .collect(),
            });
        }
    }
}

pub(crate) fn generate(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: MediaImageRequest,
) -> Result<MediaImageResponse, PluginError> {
    let model = provider
        .model_metadata
        .as_ref()
        .and_then(|m| m.selector.as_deref())
        .filter(|s| !s.trim().is_empty())
        .or(provider.model.as_deref().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| invalid("Volcengine image model is missing"))?;
    let key = api_key(provider)
        .ok_or_else(|| common::plugin_error(ErrorKind::Auth, "Volcengine apiKey is missing"))?;
    let body = build_request(model, &request)?;
    let url = images_endpoint(&provider.base_url)?;
    let headers = crate::fields(vec![
        ("authorization".into(), format!("Bearer {key}")),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ]);
    host.emit_started()?;
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url,
            headers,
            body,
        },
        &crate::net_options(),
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, RESPONSE_LIMIT)?;
    if !(200..300).contains(&status) {
        return Err(classify_error(status, &headers, &body));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| upstream("Volcengine image response is invalid JSON"))?;
    decode_response(&value, &headers)
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ImageInput {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(serde::Serialize)]
struct ImageRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    response_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<ImageInput>,
}

fn clamp_resolution(model: &str, resolution: MediaImageResolution) -> MediaImageResolution {
    let normalized = model.to_ascii_lowercase().replace('.', "-");
    // Seedream 5.5, 5.0 pro, and 5.0 flash support 1K/1.5K/2K (max 2K).
    let max_2k = normalized.contains("seedream-5-5")
        || normalized.contains("seedream-5-0-pro")
        || normalized.contains("seedream-5-0-flash");
    // Seedream 4.5 and 5.0 lite (incl. doubao-seedream-5-0-260128) start at 2K.
    let min_2k = !max_2k
        && (normalized.contains("seedream-4-5")
            || normalized.contains("seedream-5-0-lite")
            || normalized.contains("seedream-5-0-260128"));
    match resolution {
        MediaImageResolution::FourK if max_2k => MediaImageResolution::TwoK,
        MediaImageResolution::OneK if min_2k => MediaImageResolution::TwoK,
        other => other,
    }
}

fn map_size(
    model: &str,
    aspect_ratio: Option<MediaImageAspectRatio>,
    resolution: Option<MediaImageResolution>,
) -> Option<&'static str> {
    match (aspect_ratio, resolution) {
        (None, None) => None,
        (None, Some(res)) => Some(match clamp_resolution(model, res) {
            MediaImageResolution::OneK => "1K",
            MediaImageResolution::TwoK => "2K",
            MediaImageResolution::FourK => "4K",
        }),
        (Some(ratio), res) => {
            let tier = clamp_resolution(model, res.unwrap_or(MediaImageResolution::TwoK));
            Some(match (tier, ratio) {
                (MediaImageResolution::OneK, MediaImageAspectRatio::Square) => "1024x1024",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Portrait) => "864x1152",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Landscape) => "1152x864",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Tall) => "800x1424",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Wide) => "1424x800",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Square) => "2048x2048",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Portrait) => "1728x2304",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Landscape) => "2304x1728",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Tall) => "1584x2816",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Wide) => "2816x1584",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Square) => "4096x4096",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Portrait) => "3520x4704",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Landscape) => "4704x3520",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Tall) => "3040x5504",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Wide) => "5504x3040",
            })
        }
    }
}

fn build_request(model: &str, request: &MediaImageRequest) -> Result<Vec<u8>, PluginError> {
    if request.prompt.trim().is_empty() {
        return Err(invalid("Volcengine image prompt is empty"));
    }
    if request.references.len() > MAX_REFERENCES {
        return Err(invalid("Volcengine allows at most 14 reference images"));
    }
    let mut encoded_images = Vec::with_capacity(request.references.len());
    let mut encoded_size = 0usize;
    for reference in &request.references {
        let media_type = reference.media_type.to_ascii_lowercase();
        if reference.bytes.is_empty()
            || reference.bytes.len() > REFERENCE_LIMIT
            || ![
                "image/jpeg",
                "image/png",
                "image/webp",
                "image/bmp",
                "image/tiff",
                "image/gif",
                "image/heic",
                "image/heif",
            ]
            .contains(&media_type.as_str())
        {
            return Err(invalid(
                "Volcengine reference image has an unsupported type or size",
            ));
        }
        let size = reference.bytes.len().div_ceil(3).saturating_mul(4);
        encoded_size = encoded_size.saturating_add(size);
        if encoded_size > REQUEST_LIMIT {
            return Err(invalid("Volcengine image request exceeds 64 MiB"));
        }
        let mut image_url = String::with_capacity(5 + media_type.len() + 8 + size);
        image_url.push_str("data:");
        image_url.push_str(&media_type);
        image_url.push_str(";base64,");
        STANDARD.encode_string(&reference.bytes, &mut image_url);
        encoded_images.push(image_url);
    }
    let image = match encoded_images.len() {
        0 => None,
        1 => Some(ImageInput::Single(encoded_images.pop().unwrap())),
        _ => Some(ImageInput::Multiple(encoded_images)),
    };
    let body = ImageRequest {
        model,
        prompt: &request.prompt,
        response_format: "b64_json",
        size: map_size(model, request.aspect_ratio, request.resolution),
        image,
    };
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| invalid("Volcengine image request could not be encoded"))?;
    if bytes.len() > REQUEST_LIMIT {
        return Err(invalid("Volcengine image request exceeds 64 MiB"));
    }
    Ok(bytes)
}

fn classify_error(
    status: u16,
    headers: &stravia_vendor_sdk::net::Fields,
    body: &[u8],
) -> PluginError {
    if matches!(status, 401 | 403) {
        return common::upstream_error(status, headers, body);
    }
    let value: Value = serde_json::from_slice(body).unwrap_or_default();
    let error = value.get("error").unwrap_or(&value);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Volcengine image request failed");
    let code = error.get("code").and_then(Value::as_str);
    let kind = code.and_then(classify_code);
    match kind {
        Some(kind) => common::model_error(kind, message),
        None => common::upstream_error(status, headers, body),
    }
}

fn classify_code(code: &str) -> Option<AiErrorKind> {
    if code.starts_with("SensitiveContentDetected")
        || code.starts_with("InputTextSensitiveContentDetected")
        || code.starts_with("InputImageSensitiveContentDetected")
        || code.starts_with("OutputTextSensitiveContentDetected")
        || code.starts_with("OutputImageSensitiveContentDetected")
        || matches!(
            code,
            "InputTextRiskDetection"
                | "InputImageRiskDetection"
                | "OutputTextRiskDetection"
                | "OutputImageRiskDetection"
        )
    {
        return Some(AiErrorKind::ContentFiltered);
    }
    if code == "QuotaExceeded"
        || code.starts_with("QuotaExceeded.")
        || matches!(
            code,
            "AccountQuotaExceeded" | "SetLimitExceeded" | "AccountOverdueError"
        )
    {
        return Some(AiErrorKind::QuotaExceeded);
    }
    if code.starts_with("RateLimitExceeded.")
        || matches!(
            code,
            "ModelAccountRpmRateLimitExceeded"
                | "ModelAccountTpmRateLimitExceeded"
                | "ModelAccountFlexTpmRateLimitExceeded"
                | "ModelAccountIpmRateLimitExceeded"
                | "ModelAccountTpdRateLimitExceeded"
                | "ModelAccountConcurrentRequestsLimitExceeded"
                | "APIAccountRpmRateLimitExceeded"
                | "AccountRateLimitExceeded"
                | "RequestBurstTooFast"
                | "ServerOverloaded"
        )
    {
        return Some(AiErrorKind::RateLimitError);
    }
    if code == "MissingParameter"
        || code.starts_with("MissingParameter.")
        || code == "InvalidParameter"
        || code.starts_with("InvalidParameter.")
        || code == "InvalidArgumentError"
        || code.starts_with("InvalidArgumentError.")
        || code.starts_with("InvalidImageURL.")
        || code.starts_with("InvalidEndpointOrModel.")
        || matches!(
            code,
            "OutofContextError" | "InvalidSubscription" | "UnsupportedModel" | "ModelNotOpen"
        )
    {
        return Some(AiErrorKind::InvalidRequest);
    }
    None
}

fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn decode_response(
    value: &Value,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<MediaImageResponse, PluginError> {
    if value.get("error").is_some_and(|v| !v.is_null()) {
        return Err(classify_error(200, headers, value.to_string().as_bytes()));
    }
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("Volcengine returned no image"))?;
    for item in data {
        if let Some(error) = item.get("error").filter(|v| !v.is_null()) {
            return Err(classify_error(
                200,
                headers,
                json!({"error": error}).to_string().as_bytes(),
            ));
        }
    }
    if data.len() != 1 {
        return Err(upstream("Volcengine must return exactly one image"));
    }
    let encoded = data[0]
        .get("b64_json")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| upstream("Volcengine image bytes are missing"))?;
    if encoded.len() % 4 != 0 || encoded.len() > IMAGE_LIMIT.div_ceil(3) * 4 {
        return Err(upstream("Volcengine image has invalid encoded size"));
    }
    let padding = encoded.bytes().rev().take_while(|b| *b == b'=').count();
    if padding > 2 || encoded.len() / 4 * 3 - padding > IMAGE_LIMIT {
        return Err(upstream("Volcengine image exceeds 32 MiB"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| upstream("Volcengine image contains invalid base64"))?;
    if bytes.is_empty() {
        return Err(upstream("Volcengine image is empty"));
    }
    let format_hint = data[0]
        .get("output_format")
        .or_else(|| value.get("output_format"))
        .and_then(Value::as_str);
    let media_type = match format_hint {
        Some("jpeg" | "jpg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some(_) => {
            return Err(upstream(
                "Volcengine image output format is missing or invalid",
            ));
        }
        None => sniff_image_media_type(&bytes)
            .ok_or_else(|| upstream("Volcengine image output format is missing or invalid"))?,
    };
    let usage = value.get("usage").and_then(|v| {
        let count = |key| {
            v.get(key)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        let (input, output, total) = (
            count("input_tokens"),
            count("output_tokens"),
            count("total_tokens"),
        );
        if input.is_none() && output.is_none() && total.is_none() {
            return None;
        }
        Some(Usage {
            prompt_tokens: input.unwrap_or_default(),
            completion_tokens: output.unwrap_or_default(),
            total_tokens: total.unwrap_or_default(),
            required_components_known: input.is_some() && output.is_some() && total.is_some(),
            ..Default::default()
        })
    });
    Ok(MediaImageResponse {
        artifacts: vec![MediaArtifact {
            media_type: media_type.into(),
            bytes,
            upstream_ref: value
                .get("request_id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            metadata: Default::default(),
        }],
        revised_prompt: data[0]
            .get("revised_prompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage,
    })
}

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

fn upstream(message: &str) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_vendor_sdk::MediaReference;

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    const JPEG_BYTES: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

    #[test]
    fn parse_and_merge_discovered_image_models() {
        let document = json!({
            "object": "list",
            "data": [
                {
                    "id": "doubao-seed-2-1-pro-260915",
                    "domain": "VLM",
                    "modalities": {"input_modalities": ["text", "image"], "output_modalities": ["text"]}
                },
                {
                    "id": "doubao-seedream-5-5-261028",
                    "domain": "ImageGeneration",
                    "api_supported": {"images/generations": true},
                    "modalities": {"input_modalities": ["text", "image"], "output_modalities": ["image"]},
                    "task_type": ["text_to_image", "image_to_image"]
                },
                {
                    "id": "doubao-seedream-4-0-20260415",
                    "domain": "ImageGeneration"
                }
            ]
        });
        let entries = parse_image_models(&document).unwrap();
        assert_eq!(
            entries,
            vec![
                ImageModelEntry {
                    id: "doubao-seedream-5-5-261028".into(),
                    supports_image_input: true,
                },
                ImageModelEntry {
                    id: "doubao-seedream-4-0-20260415".into(),
                    supports_image_input: false,
                },
            ]
        );

        let mut response = DiscoverResponse {
            models: vec![DiscoveredModel {
                id: "doubao-seedream-5-5-261028".into(),
                display_name: "Seedream 5.5".into(),
                family: None,
                selector: None,
                capabilities: vec!["infer".into()],
                metadata: [(
                    "modalities".into(),
                    json!({"input": ["text"], "output": ["text"]}),
                )]
                .into_iter()
                .collect(),
            }],
            next_cursor: None,
        };
        merge_models(&mut response, &entries, true);
        assert_eq!(response.models.len(), 1);
        assert_eq!(
            response.models[0].metadata["modalities"],
            json!({"input": ["text", "image"], "output": ["text", "image"]})
        );

        merge_models(&mut response, &entries, false);
        assert_eq!(response.models.len(), 2);
        assert_eq!(
            response.models[1].metadata["modalities"],
            json!({"input": ["text"], "output": ["image"]})
        );
    }

    #[test]
    fn endpoints_normalize_standard_plan_and_custom_paths() {
        assert_eq!(
            images_endpoint("https://ark.cn-beijing.volces.com/api/v3").unwrap(),
            "https://ark.cn-beijing.volces.com/api/v3/images/generations"
        );
        assert_eq!(
            images_endpoint("https://ark.cn-beijing.volces.com/api/plan/v3").unwrap(),
            "https://ark.cn-beijing.volces.com/api/plan/v3/images/generations"
        );
        assert_eq!(
            images_endpoint("https://ark.cn-beijing.volces.com/api/plan").unwrap(),
            "https://ark.cn-beijing.volces.com/api/plan/v3/images/generations"
        );
        assert_eq!(
            images_endpoint("https://ark.ap-southeast.bytepluses.com/api/v3").unwrap(),
            "https://ark.ap-southeast.bytepluses.com/api/v3/images/generations"
        );
        assert_eq!(
            images_endpoint("https://proxy.example.com/ark/api/plan/v1").unwrap(),
            "https://proxy.example.com/ark/api/plan/v3/images/generations"
        );
        assert_eq!(
            models_endpoint("https://ark.cn-beijing.volces.com/api/v3").unwrap(),
            "https://ark.cn-beijing.volces.com/api/v3/models"
        );
    }

    #[test]
    fn size_mapping_and_request_encoding() {
        assert_eq!(
            map_size(
                "doubao-seedream-5-5-261028",
                Some(MediaImageAspectRatio::Wide),
                Some(MediaImageResolution::FourK),
            ),
            Some("2816x1584")
        );
        assert_eq!(
            map_size(
                "doubao-seedream-5-0-lite-260128",
                Some(MediaImageAspectRatio::Square),
                Some(MediaImageResolution::OneK),
            ),
            Some("2048x2048")
        );
        assert_eq!(
            map_size(
                "doubao-seedream-4-5-251128",
                Some(MediaImageAspectRatio::Wide),
                Some(MediaImageResolution::FourK),
            ),
            Some("5504x3040")
        );
        assert_eq!(map_size("doubao-seedream-5.5", None, None), None);
        assert_eq!(
            map_size("doubao-seedream-5.5", None, Some(MediaImageResolution::FourK)),
            Some("2K")
        );

        let single_ref = MediaImageRequest {
            prompt: "edit single".into(),
            references: vec![MediaReference {
                media_type: "image/PNG".into(),
                bytes: vec![1, 2],
            }],
            aspect_ratio: Some(MediaImageAspectRatio::Square),
            resolution: None,
        };
        let single_body: Value =
            serde_json::from_slice(&build_request("doubao-seedream-5.5", &single_ref).unwrap())
                .unwrap();
        assert_eq!(
            single_body,
            json!({
                "model": "doubao-seedream-5.5",
                "prompt": "edit single",
                "response_format": "b64_json",
                "size": "2048x2048",
                "image": "data:image/png;base64,AQI="
            })
        );

        let multi_ref = MediaImageRequest {
            prompt: "edit multi".into(),
            references: vec![
                MediaReference {
                    media_type: "image/png".into(),
                    bytes: vec![1],
                },
                MediaReference {
                    media_type: "image/webp".into(),
                    bytes: vec![2],
                },
            ],
            aspect_ratio: None,
            resolution: Some(MediaImageResolution::TwoK),
        };
        let multi_body: Value = serde_json::from_slice(
            &build_request("doubao-seedream-5-0-pro-260628", &multi_ref).unwrap(),
        )
        .unwrap();
        assert_eq!(
            multi_body,
            json!({
                "model": "doubao-seedream-5-0-pro-260628",
                "prompt": "edit multi",
                "response_format": "b64_json",
                "size": "2K",
                "image": [
                    "data:image/png;base64,AQ==",
                    "data:image/webp;base64,Ag=="
                ]
            })
        );
    }

    #[test]
    fn decode_response_format_sniffing_usage_and_errors() {
        let headers = stravia_vendor_sdk::net::Fields::new();
        let png_b64 = STANDARD.encode(PNG_BYTES);
        let jpeg_b64 = STANDARD.encode(JPEG_BYTES);

        let with_format = json!({
            "data": [{"b64_json": png_b64, "output_format": "png"}],
            "usage": {"generated_images": 1, "output_tokens": 16384, "total_tokens": 16384}
        });
        let decoded = decode_response(&with_format, &headers).unwrap();
        assert_eq!(decoded.artifacts[0].media_type, "image/png");
        assert_eq!(decoded.artifacts[0].bytes, PNG_BYTES);
        let usage = decoded.usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                usage.required_components_known
            ),
            (0, 16384, 16384, false)
        );

        // Older Seedream models omit output_format; sniff JPEG from bytes.
        let without_format = json!({"data": [{"b64_json": jpeg_b64}]});
        let decoded = decode_response(&without_format, &headers).unwrap();
        assert_eq!(decoded.artifacts[0].media_type, "image/jpeg");

        for code in [
            "SensitiveContentDetected",
            "OutputImageSensitiveContentDetected.PolicyViolation",
            "InputImageRiskDetection",
        ] {
            let err = classify_error(
                400,
                &headers,
                json!({"error": {"code": code, "message": "blocked"}})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(
                err.kind.model_error_kind(),
                Some(AiErrorKind::ContentFiltered),
                "{code}"
            );
        }
        for code in [
            "QuotaExceeded",
            "QuotaExceeded.AgentPlanQuotaExceeded",
            "AccountQuotaExceeded",
            "SetLimitExceeded",
        ] {
            let err = classify_error(
                429,
                &headers,
                json!({"error": {"code": code, "message": "quota"}})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(
                err.kind.model_error_kind(),
                Some(AiErrorKind::QuotaExceeded),
                "{code}"
            );
        }
        for code in [
            "ModelAccountIpmRateLimitExceeded",
            "RateLimitExceeded.EndpointRPMExceeded",
            "ServerOverloaded",
        ] {
            let err = classify_error(
                429,
                &headers,
                json!({"error": {"code": code, "message": "rate"}})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(
                err.kind.model_error_kind(),
                Some(AiErrorKind::RateLimitError),
                "{code}"
            );
        }
        for code in ["ModelNotOpen", "InvalidSubscription", "InvalidParameter.Size"] {
            let err = classify_error(
                404,
                &headers,
                json!({"error": {"code": code, "message": "invalid"}})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(
                err.kind.model_error_kind(),
                Some(AiErrorKind::InvalidRequest),
                "{code}"
            );
        }
        for (status, expected) in [
            (401, AiErrorKind::AuthenticationError),
            (403, AiErrorKind::AuthorizationError),
        ] {
            let err = classify_error(
                status,
                &headers,
                br#"{"error":{"code":"QuotaExceeded","message":"denied"}}"#,
            );
            assert_eq!(err.kind.model_error_kind(), Some(expected));
        }
    }
}
