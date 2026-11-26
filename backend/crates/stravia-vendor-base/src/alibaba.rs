use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{AiErrorKind, Usage};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    Capability, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost, MediaArtifact,
    MediaImageAspectRatio, MediaImageRequest, MediaImageResolution, MediaImageResponse,
    OriginDeclaration, OriginHost, PluginError, ProviderDescriptor, ProviderSnapshot,
    read_http_body,
};

const MODELS_LIMIT: usize = 8 * 1024 * 1024;
const MODELS_PAGE_SIZE: usize = 100;
const MAX_MODEL_PAGES: usize = 20;
const REQUEST_LIMIT: usize = 64 * 1024 * 1024;
const REFERENCE_LIMIT: usize = 20 * 1024 * 1024;
const IMAGE_LIMIT: usize = 32 * 1024 * 1024;
const RESPONSE_LIMIT: usize = IMAGE_LIMIT.div_ceil(3) * 4 + 2 * 1024 * 1024;
const MAX_REFERENCES: usize = 10;

/// DashScope generated image URLs are hosted on Alibaba Cloud OSS buckets such as
/// `dashscope-a717.oss-accelerate.aliyuncs.com` or `dashscope-result-bj.oss-cn-beijing.aliyuncs.com`.
const RESULT_OSS_HOST_REGEX: &str = r"dashscope-[a-z0-9-]+\.oss-[a-z0-9-]+\.aliyuncs\.com";

pub(crate) fn supports(id: &str) -> bool {
    matches!(
        id,
        "alibaba" | "alibaba-cn" | "alibaba-token-plan" | "alibaba-token-plan-cn"
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
    let api_host = match descriptor.provider_id.as_str() {
        "alibaba" => "dashscope-intl.aliyuncs.com",
        "alibaba-cn" => "dashscope.aliyuncs.com",
        "alibaba-token-plan" => "token-plan.ap-southeast-1.maas.aliyuncs.com",
        "alibaba-token-plan-cn" => "token-plan.cn-beijing.maas.aliyuncs.com",
        _ => return,
    };
    let origins = [
        OriginDeclaration {
            scheme: "https".into(),
            host: api_host.into(),
            port: None,
        },
        OriginDeclaration {
            scheme: "https".into(),
            host: OriginHost::Regex {
                regex: RESULT_OSS_HOST_REGEX.into(),
            },
            port: None,
        },
    ];
    for origin in origins {
        if !descriptor.network.extra_origins.contains(&origin) {
            descriptor.network.extra_origins.push(origin);
        }
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

fn is_token_plan(provider_id: &str, base_url: &str) -> bool {
    if matches!(provider_id, "alibaba-token-plan" | "alibaba-token-plan-cn") {
        return true;
    }
    url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host.starts_with("token-plan."))
}

fn api_root_url(base: &str) -> Result<url::Url, PluginError> {
    let mut url = url::Url::parse(base).map_err(|_| invalid("Alibaba base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("Alibaba base URL is invalid"));
    }
    let prefix = {
        let path = url.path().trim_end_matches('/');
        path.strip_suffix("/api/v1/services/aigc/multimodal-generation/generation")
            .or_else(|| path.strip_suffix("/compatible-mode/v1"))
            .or_else(|| path.strip_suffix("/compatible-mode"))
            .or_else(|| path.strip_suffix("/apps/anthropic/v1"))
            .or_else(|| path.strip_suffix("/apps/anthropic"))
            .or_else(|| path.strip_suffix("/api/v1"))
            .or_else(|| path.strip_suffix("/v1"))
            .unwrap_or(path)
            .to_owned()
    };
    url.set_path(&prefix);
    Ok(url)
}

fn images_endpoint(base: &str) -> Result<String, PluginError> {
    let mut url = api_root_url(base)?;
    let prefix = url.path().trim_end_matches('/');
    let path = format!("{prefix}/api/v1/services/aigc/multimodal-generation/generation");
    url.set_path(&path);
    Ok(url.into())
}

fn models_endpoint(base: &str, page_no: usize) -> Result<String, PluginError> {
    let mut url = api_root_url(base)?;
    let prefix = url.path().trim_end_matches('/');
    let path = format!("{prefix}/api/v1/models");
    url.set_path(&path);
    url.set_query(Some(&format!(
        "page_no={page_no}&page_size={MODELS_PAGE_SIZE}"
    )));
    Ok(url.into())
}

pub(crate) fn discover(
    host: &GuestHost,
    ctx: &DiscoveryContext,
    mut response: DiscoverResponse,
    restrict_to_existing: bool,
) -> Result<DiscoverResponse, PluginError> {
    ensure_existing_image_capabilities(&mut response);
    let token_plan = is_token_plan(&ctx.provider_id, &ctx.base_url);
    let Some(key) = ctx.api_key.as_deref() else {
        if token_plan {
            return Ok(response);
        }
        return Err(common::plugin_error(
            ErrorKind::Auth,
            "Alibaba apiKey is missing",
        ));
    };

    let mut discovered = Vec::new();
    let mut seen = BTreeSet::new();
    let mut fetched_total = 0usize;

    for page_no in 1..=MAX_MODEL_PAGES {
        let url = models_endpoint(&ctx.base_url, page_no)?;
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
        if status == 404 && token_plan && page_no == 1 {
            // Token Plan endpoints currently return 404 on /api/v1/models; preserve
            // catalog or explicit discovery rather than breaking text model sync.
            return Ok(response);
        }
        if !(200..300).contains(&status) {
            return Err(common::upstream_error(status, &headers, &body));
        }
        let document: Value = serde_json::from_slice(&body)
            .map_err(|_| upstream("Alibaba models response is invalid JSON"))?;
        let page = parse_models_page(&document, &mut seen, &mut discovered)?;
        fetched_total = fetched_total.saturating_add(page.row_count);
        let done = page.row_count == 0
            || page.row_count < page.page_size
            || page.total.is_some_and(|total| fetched_total >= total);
        if done {
            break;
        }
    }

    merge_models(&mut response, &discovered, restrict_to_existing);
    Ok(response)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageModelEntry {
    id: String,
    display_name: String,
    supports_image_input: bool,
}

struct ModelsPage {
    row_count: usize,
    page_size: usize,
    total: Option<usize>,
}

fn supports_multimodal_generation_protocol(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains('/') {
        // Third-party partner models (e.g. vidu/*, kling/*) use separate async endpoints.
        return false;
    }
    if lower.starts_with("wanx")
        || lower.starts_with("wan2.0")
        || lower.starts_with("wan2.1")
        || lower.starts_with("wan2.2")
        || lower.starts_with("wan2.5")
    {
        // Legacy Wanx / Wan <= 2.5 models only support async text2image/image-synthesis.
        return false;
    }
    true
}

fn parse_models_page(
    document: &Value,
    seen: &mut BTreeSet<String>,
    entries: &mut Vec<ImageModelEntry>,
) -> Result<ModelsPage, PluginError> {
    let output = document
        .get("output")
        .and_then(Value::as_object)
        .ok_or_else(|| upstream("Alibaba models response has no output object"))?;
    let rows = output
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("Alibaba models response has no models array"))?;
    let total = output
        .get("total")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let page_size = output
        .get("page_size")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .unwrap_or(MODELS_PAGE_SIZE);

    for row in rows {
        if row
            .get("inference_offline_info")
            .is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let Some(id) = row
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if !supports_multimodal_generation_protocol(id) {
            continue;
        }
        let has_ig_capability = row
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter()
                    .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("IG")))
            });
        let outputs_image = row
            .pointer("/inference_metadata/response_modality")
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter()
                    .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("image")))
            });
        let accepts_text = row
            .pointer("/inference_metadata/request_modality")
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter()
                    .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("text")))
            });
        if !has_ig_capability || !outputs_image || !accepts_text {
            continue;
        }
        let supports_image_input = row
            .pointer("/inference_metadata/request_modality")
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter()
                    .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("image")))
            });
        let display_name = row
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(id)
            .to_owned();
        if seen.insert(id.to_owned()) {
            entries.push(ImageModelEntry {
                id: id.to_owned(),
                display_name,
                supports_image_input,
            });
        }
    }

    Ok(ModelsPage {
        row_count: rows.len(),
        page_size,
        total,
    })
}

fn ensure_existing_image_capabilities(response: &mut DiscoverResponse) {
    for model in &mut response.models {
        let outputs_image = model
            .metadata
            .get("modalities")
            .and_then(|m| m.get("output"))
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|v| v.as_str() == Some("image")));
        if outputs_image {
            crate::add_media_image_capability(model);
        }
    }
}

fn merge_models(response: &mut DiscoverResponse, models: &[ImageModelEntry], restricted: bool) {
    for entry in models {
        let edit = entry.supports_image_input;
        if let Some(model) = response.models.iter_mut().find(|m| m.id == entry.id) {
            crate::add_media_image_capability(model);
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
                display_name: entry.display_name.clone(),
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
        .ok_or_else(|| invalid("Alibaba image model is missing"))?;
    let key = api_key(provider)
        .ok_or_else(|| common::plugin_error(ErrorKind::Auth, "Alibaba apiKey is missing"))?;
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
        .map_err(|_| upstream("Alibaba image response is invalid JSON"))?;
    decode_response(host, &value, &headers)
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ContentPart<'a> {
    Image { image: String },
    Text { text: &'a str },
}

#[derive(serde::Serialize)]
struct Message<'a> {
    role: &'static str,
    content: Vec<ContentPart<'a>>,
}

#[derive(serde::Serialize)]
struct Input<'a> {
    messages: [Message<'a>; 1],
}

#[derive(serde::Serialize)]
struct Parameters {
    size: &'static str,
}

#[derive(serde::Serialize)]
struct MultimodalGenerationRequest<'a> {
    model: &'a str,
    input: Input<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<Parameters>,
}

fn is_wan27_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.starts_with("wan2.7-image") || lower.starts_with("wan2.7")
}

fn is_wan26_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("wan2.6")
}

fn supports_4k(model: &str, has_references: bool) -> bool {
    !has_references && model.to_ascii_lowercase().starts_with("wan2.7-image-pro")
}

fn map_size(
    model: &str,
    aspect_ratio: Option<MediaImageAspectRatio>,
    resolution: Option<MediaImageResolution>,
    has_references: bool,
) -> Option<&'static str> {
    let is_wan27 = is_wan27_model(model);
    let is_wan26 = is_wan26_model(model);
    let allow_4k = supports_4k(model, has_references);
    let clamp = |res: MediaImageResolution| match res {
        MediaImageResolution::FourK if !allow_4k => MediaImageResolution::TwoK,
        other => other,
    };

    match (aspect_ratio, resolution) {
        (None, None) => None,
        (None, Some(res)) => {
            let tier = clamp(res);
            if is_wan27 {
                Some(match tier {
                    MediaImageResolution::OneK => "1K",
                    MediaImageResolution::TwoK => "2K",
                    MediaImageResolution::FourK => "4K",
                })
            } else if is_wan26 {
                Some(match tier {
                    MediaImageResolution::OneK => "1024*1024",
                    MediaImageResolution::TwoK | MediaImageResolution::FourK => "1280*1280",
                })
            } else {
                Some(match tier {
                    MediaImageResolution::OneK => "1024*1024",
                    MediaImageResolution::TwoK | MediaImageResolution::FourK => "2048*2048",
                })
            }
        }
        (Some(ratio), res) => {
            let default_res = if is_wan27 {
                MediaImageResolution::TwoK
            } else {
                MediaImageResolution::OneK
            };
            let tier = clamp(res.unwrap_or(default_res));
            if is_wan26 {
                return Some(match (tier, ratio) {
                    (MediaImageResolution::OneK, MediaImageAspectRatio::Square) => "1024*1024",
                    (MediaImageResolution::OneK, MediaImageAspectRatio::Portrait) => "864*1152",
                    (MediaImageResolution::OneK, MediaImageAspectRatio::Landscape) => "1152*864",
                    (MediaImageResolution::OneK, MediaImageAspectRatio::Tall) => "720*1280",
                    (MediaImageResolution::OneK, MediaImageAspectRatio::Wide) => "1280*720",
                    (_, MediaImageAspectRatio::Square) => "1280*1280",
                    (_, MediaImageAspectRatio::Portrait) => "1104*1472",
                    (_, MediaImageAspectRatio::Landscape) => "1472*1104",
                    (_, MediaImageAspectRatio::Tall) => "960*1696",
                    (_, MediaImageAspectRatio::Wide) => "1696*960",
                });
            }
            Some(match (tier, ratio) {
                (MediaImageResolution::OneK, MediaImageAspectRatio::Square) => "1024*1024",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Portrait) => "864*1152",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Landscape) => "1152*864",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Tall) if is_wan27 => "768*1344",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Wide) if is_wan27 => "1344*768",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Tall) => "720*1280",
                (MediaImageResolution::OneK, MediaImageAspectRatio::Wide) => "1280*720",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Square) => "2048*2048",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Portrait) => "1728*2304",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Landscape) => "2304*1728",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Tall) => "1440*2560",
                (MediaImageResolution::TwoK, MediaImageAspectRatio::Wide) => "2560*1440",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Square) => "4096*4096",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Portrait) => "3456*4608",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Landscape) => "4608*3456",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Tall) => "2880*5120",
                (MediaImageResolution::FourK, MediaImageAspectRatio::Wide) => "5120*2880",
            })
        }
    }
}

fn build_request(model: &str, request: &MediaImageRequest) -> Result<Vec<u8>, PluginError> {
    if request.prompt.trim().is_empty() {
        return Err(invalid("Alibaba image prompt is empty"));
    }
    if request.references.len() > MAX_REFERENCES {
        return Err(invalid("Alibaba allows at most 10 reference images"));
    }
    let mut content = Vec::with_capacity(request.references.len() + 1);
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
            ]
            .contains(&media_type.as_str())
        {
            return Err(invalid(
                "Alibaba reference image has an unsupported type or size",
            ));
        }
        let size = reference.bytes.len().div_ceil(3).saturating_mul(4);
        encoded_size = encoded_size.saturating_add(size);
        if encoded_size > REQUEST_LIMIT {
            return Err(invalid("Alibaba image request exceeds 64 MiB"));
        }
        let mut image_url = String::with_capacity(5 + media_type.len() + 8 + size);
        image_url.push_str("data:");
        image_url.push_str(&media_type);
        image_url.push_str(";base64,");
        STANDARD.encode_string(&reference.bytes, &mut image_url);
        content.push(ContentPart::Image { image: image_url });
    }
    content.push(ContentPart::Text {
        text: &request.prompt,
    });
    let parameters = map_size(
        model,
        request.aspect_ratio,
        request.resolution,
        !request.references.is_empty(),
    )
    .map(|size| Parameters { size });
    let body = MultimodalGenerationRequest {
        model,
        input: Input {
            messages: [Message {
                role: "user",
                content,
            }],
        },
        parameters,
    };
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| invalid("Alibaba image request could not be encoded"))?;
    if bytes.len() > REQUEST_LIMIT {
        return Err(invalid("Alibaba image request exceeds 64 MiB"));
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
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Alibaba image request failed");
    let code = error.get("code").and_then(Value::as_str);
    let kind = code.and_then(classify_code);
    match kind {
        Some(kind) => common::model_error(kind, message),
        None => common::upstream_error(status, headers, body),
    }
}

fn classify_code(code: &str) -> Option<AiErrorKind> {
    if matches!(
        code,
        "DataInspectionFailed"
            | "IPInfringementSuspect"
            | "InputTextRiskDetection"
            | "InputImageRiskDetection"
            | "OutputImageRiskDetection"
    ) {
        return Some(AiErrorKind::ContentFiltered);
    }
    if matches!(
        code,
        "Throttling.AllocationQuota"
            | "AllocationQuota.FreeTierOnly"
            | "insufficient_quota"
            | "Arrearage"
            | "QuotaExceeded"
    ) {
        return Some(AiErrorKind::QuotaExceeded);
    }
    if code == "Throttling"
        || code.starts_with("Throttling.")
        || matches!(code, "LimitRequests" | "rate_limit_exceeded")
    {
        return Some(AiErrorKind::RateLimitError);
    }
    if code == "InvalidParameter"
        || code.starts_with("InvalidParameter.")
        || code == "MissingParameter"
        || code.starts_with("MissingParameter.")
        || code == "InvalidFile"
        || code.starts_with("InvalidFile.")
        || code == "BadRequest"
        || code.starts_with("BadRequest.")
        || matches!(
            code,
            "InvalidURL"
                | "ModelNotFound"
                | "NotFound"
                | "UnsupportedModel"
                | "invalid_request_error"
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

fn parse_content_type_header(headers: &stravia_vendor_sdk::net::Fields) -> Option<&'static str> {
    for field in headers.iter() {
        if !field.name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        let value = String::from_utf8_lossy(&field.bytes);
        let mime = value.split(';').next().unwrap_or("").trim();
        if mime.eq_ignore_ascii_case("image/png") {
            return Some("image/png");
        }
        if mime.eq_ignore_ascii_case("image/jpeg") || mime.eq_ignore_ascii_case("image/jpg") {
            return Some("image/jpeg");
        }
        if mime.eq_ignore_ascii_case("image/webp") {
            return Some("image/webp");
        }
    }
    None
}

fn decode_data_url(data_url: &str) -> Result<(String, Vec<u8>), PluginError> {
    let rest = data_url
        .strip_prefix("data:")
        .ok_or_else(|| upstream("Alibaba image data URL is invalid"))?;
    let (header, encoded) = rest
        .split_once(',')
        .ok_or_else(|| upstream("Alibaba image data URL is missing payload"))?;
    let (mime, encoding) = header
        .split_once(';')
        .ok_or_else(|| upstream("Alibaba image data URL is not base64"))?;
    if !encoding.eq_ignore_ascii_case("base64") {
        return Err(upstream("Alibaba image data URL is not base64"));
    }
    let normalized_mime = if mime.eq_ignore_ascii_case("image/png") {
        "image/png"
    } else if mime.eq_ignore_ascii_case("image/jpeg") || mime.eq_ignore_ascii_case("image/jpg") {
        "image/jpeg"
    } else if mime.eq_ignore_ascii_case("image/webp") {
        "image/webp"
    } else {
        return Err(upstream("Alibaba image output format is unsupported"));
    };
    if encoded.is_empty() || encoded.len() % 4 != 0 || encoded.len() > IMAGE_LIMIT.div_ceil(3) * 4 {
        return Err(upstream("Alibaba image has invalid encoded size"));
    }
    let padding = encoded.bytes().rev().take_while(|b| *b == b'=').count();
    if padding > 2 || encoded.len() / 4 * 3 - padding > IMAGE_LIMIT {
        return Err(upstream("Alibaba image exceeds 32 MiB"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| upstream("Alibaba image contains invalid base64"))?;
    if bytes.is_empty() {
        return Err(upstream("Alibaba image is empty"));
    }
    Ok((normalized_mime.into(), bytes))
}

fn download_image(host: &GuestHost, image_url: &str) -> Result<(String, Vec<u8>), PluginError> {
    let parsed = url::Url::parse(image_url)
        .map_err(|_| upstream("Alibaba returned an invalid image URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(upstream("Alibaba returned an unsupported image URL"));
    }
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "GET".into(),
            url: parsed.into(),
            headers: crate::fields(vec![("accept".into(), "image/*".into())]),
            body: vec![],
        },
        &crate::net_options(),
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    let bytes = read_http_body(&response, IMAGE_LIMIT)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &bytes));
    }
    if bytes.is_empty() {
        return Err(upstream("Alibaba image download returned empty body"));
    }
    let media_type = sniff_image_media_type(&bytes)
        .or_else(|| parse_content_type_header(&headers))
        .ok_or_else(|| upstream("Alibaba downloaded image format is unsupported"))?;
    Ok((media_type.into(), bytes))
}

fn extract_image_and_prompt<'a>(
    value: &'a Value,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<(&'a str, Option<String>), PluginError> {
    if value
        .get("code")
        .and_then(Value::as_str)
        .is_some_and(|code| !code.trim().is_empty())
        || value.get("error").is_some_and(|v| !v.is_null())
    {
        return Err(classify_error(200, headers, value.to_string().as_bytes()));
    }
    let choices = value
        .pointer("/output/choices")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| upstream("Alibaba returned no image choices"))?;
    let mut images = Vec::new();
    let mut revised_prompt = None;
    for choice in choices {
        let Some(content) = choice.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            if let Some(image) = part
                .get("image")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                images.push(image);
            }
            if revised_prompt.is_none()
                && let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                revised_prompt = Some(text.to_owned());
            }
        }
    }
    if images.len() != 1 {
        return Err(upstream("Alibaba must return exactly one image"));
    }
    Ok((images[0], revised_prompt))
}

fn parse_usage(value: &Value) -> Option<Usage> {
    value.get("usage").and_then(|v| {
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
    })
}

fn decode_response(
    host: &GuestHost,
    value: &Value,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<MediaImageResponse, PluginError> {
    let (image_ref, revised_prompt) = extract_image_and_prompt(value, headers)?;
    let (media_type, bytes) = if image_ref.starts_with("data:") {
        decode_data_url(image_ref)?
    } else {
        download_image(host, image_ref)?
    };
    Ok(MediaImageResponse {
        artifacts: vec![MediaArtifact {
            media_type,
            bytes,
            upstream_ref: value
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            metadata: Default::default(),
        }],
        revised_prompt,
        usage: parse_usage(value),
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

    #[test]
    fn profile_support_excludes_coding_plan() {
        for id in [
            "alibaba",
            "alibaba-cn",
            "alibaba-token-plan",
            "alibaba-token-plan-cn",
        ] {
            assert!(supports(id), "{id}");
        }
        for id in ["alibaba-coding-plan", "alibaba-coding-plan-cn", "openai"] {
            assert!(!supports(id), "{id}");
        }
    }

    #[test]
    fn endpoint_normalization_preserves_workspace_and_proxy_prefixes() {
        assert_eq!(
            images_endpoint("https://dashscope.aliyuncs.com/compatible-mode/v1").unwrap(),
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
        );
        assert_eq!(
            images_endpoint("https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1")
                .unwrap(),
            "https://token-plan.cn-beijing.maas.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
        );
        assert_eq!(
            images_endpoint("https://ws-123.cn-beijing.maas.aliyuncs.com/apps/anthropic/v1")
                .unwrap(),
            "https://ws-123.cn-beijing.maas.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
        );
        assert_eq!(
            images_endpoint("https://proxy.example.com/bailian/compatible-mode/v1").unwrap(),
            "https://proxy.example.com/bailian/api/v1/services/aigc/multimodal-generation/generation"
        );
        assert_eq!(
            models_endpoint("https://proxy.example.com/bailian/compatible-mode/v1", 2).unwrap(),
            "https://proxy.example.com/bailian/api/v1/models?page_no=2&page_size=100"
        );
        assert!(images_endpoint("https://dashscope.aliyuncs.com/v1?foo=bar").is_err());
    }

    #[test]
    fn parse_models_filters_protocol_offline_and_modalities() {
        let doc = json!({
            "output": {
                "total": 7,
                "page_size": 100,
                "models": [
                    {
                        "model": "qwen-image-3.0-pro",
                        "name": "Qwen-Image-3.0-Pro",
                        "capabilities": ["IG"],
                        "inference_metadata": {
                            "request_modality": ["Image", "Text"],
                            "response_modality": ["Image"]
                        },
                        "inference_offline_info": null
                    },
                    {
                        "model": "z-image-turbo",
                        "name": "Z-Image-Turbo",
                        "capabilities": ["IG"],
                        "inference_metadata": {
                            "request_modality": ["Text"],
                            "response_modality": ["Text", "Image"]
                        },
                        "inference_offline_info": null
                    },
                    {
                        "model": "qwen-mt-image",
                        "name": "Qwen-MT-Image",
                        "capabilities": ["IG"],
                        "inference_metadata": {
                            "request_modality": ["Image"],
                            "response_modality": ["Image"]
                        },
                        "inference_offline_info": null
                    },
                    {
                        "model": "wanx-v1",
                        "name": "wanx-t2i",
                        "capabilities": ["IG"],
                        "inference_metadata": {
                            "request_modality": ["Text"],
                            "response_modality": ["Image"]
                        },
                        "inference_offline_info": null
                    },
                    {
                        "model": "kling/kling-v3-image-generation",
                        "name": "Kling Image 3.0",
                        "capabilities": ["IG"],
                        "inference_metadata": {
                            "request_modality": ["Text", "Image"],
                            "response_modality": ["Image"]
                        },
                        "inference_offline_info": null
                    },
                    {
                        "model": "qwen-image-offline",
                        "name": "Offline",
                        "capabilities": ["IG"],
                        "inference_metadata": {
                            "request_modality": ["Text"],
                            "response_modality": ["Image"]
                        },
                        "inference_offline_info": {"offline_time": "2026-01-01"}
                    },
                    {
                        "model": "qwen4-max",
                        "name": "Qwen4-Max",
                        "capabilities": ["TG"],
                        "inference_metadata": {
                            "request_modality": ["Text", "Image"],
                            "response_modality": ["Text"]
                        },
                        "inference_offline_info": null
                    }
                ]
            }
        });
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        let page = parse_models_page(&doc, &mut seen, &mut entries).unwrap();
        assert_eq!(page.row_count, 7);
        assert_eq!(
            entries,
            vec![
                ImageModelEntry {
                    id: "qwen-image-3.0-pro".into(),
                    display_name: "Qwen-Image-3.0-Pro".into(),
                    supports_image_input: true,
                },
                ImageModelEntry {
                    id: "z-image-turbo".into(),
                    display_name: "Z-Image-Turbo".into(),
                    supports_image_input: false,
                },
            ]
        );
    }

    #[test]
    fn size_mapping_uses_dashscope_asterisk_and_model_limits() {
        assert_eq!(map_size("qwen-image-3.0-pro", None, None, false), None);
        assert_eq!(
            map_size(
                "qwen-image-3.0-pro",
                Some(MediaImageAspectRatio::Wide),
                Some(MediaImageResolution::FourK),
                false
            ),
            Some("2560*1440")
        );
        assert_eq!(
            map_size(
                "z-image-turbo",
                Some(MediaImageAspectRatio::Tall),
                None,
                false
            ),
            Some("720*1280")
        );
        assert_eq!(
            map_size(
                "wan2.7-image-pro",
                None,
                Some(MediaImageResolution::FourK),
                false
            ),
            Some("4K")
        );
        assert_eq!(
            map_size(
                "wan2.7-image-pro",
                None,
                Some(MediaImageResolution::FourK),
                true
            ),
            Some("2K")
        );
        assert_eq!(
            map_size(
                "wan2.7-image-pro",
                Some(MediaImageAspectRatio::Square),
                Some(MediaImageResolution::FourK),
                false
            ),
            Some("4096*4096")
        );
        assert_eq!(
            map_size(
                "wan2.7-image",
                None,
                Some(MediaImageResolution::FourK),
                false
            ),
            Some("2K")
        );
        assert_eq!(
            map_size(
                "wan2.6-t2i",
                Some(MediaImageAspectRatio::Wide),
                Some(MediaImageResolution::TwoK),
                false
            ),
            Some("1696*960")
        );
    }

    #[test]
    fn request_building_preserves_reference_order_and_omits_default_parameters() {
        let plain = MediaImageRequest {
            prompt: "a red apple".into(),
            references: vec![],
            aspect_ratio: None,
            resolution: None,
        };
        let body: Value =
            serde_json::from_slice(&build_request("qwen-image-3.0-pro", &plain).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "qwen-image-3.0-pro",
                "input": {
                    "messages": [{
                        "role": "user",
                        "content": [{"text": "a red apple"}]
                    }]
                }
            })
        );

        let edit = MediaImageRequest {
            prompt: "combine images".into(),
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
            aspect_ratio: Some(MediaImageAspectRatio::Landscape),
            resolution: Some(MediaImageResolution::TwoK),
        };
        let body: Value =
            serde_json::from_slice(&build_request("wan2.7-image-pro", &edit).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "wan2.7-image-pro",
                "input": {
                    "messages": [{
                        "role": "user",
                        "content": [
                            {"image": "data:image/png;base64,AQ=="},
                            {"image": "data:image/webp;base64,Ag=="},
                            {"text": "combine images"}
                        ]
                    }]
                },
                "parameters": {
                    "size": "2304*1728"
                }
            })
        );
    }

    #[test]
    fn data_url_and_error_classification() {
        let (mime, bytes) = decode_data_url("data:image/png;base64,AQ==").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, vec![1]);
        assert!(decode_data_url("data:text/plain;base64,AQ==").is_err());
        assert!(decode_data_url("data:image/png;base64,@@==").is_err());

        let headers = stravia_vendor_sdk::net::Fields::new();
        let payload = json!({
            "output": {
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"image": "https://dashscope-a717.oss-accelerate.aliyuncs.com/a.png"},
                            {"text": "revised prompt"}
                        ]
                    }
                }]
            },
            "request_id": "req-1"
        });
        let (img, prompt) = extract_image_and_prompt(&payload, &headers).unwrap();
        assert_eq!(
            img,
            "https://dashscope-a717.oss-accelerate.aliyuncs.com/a.png"
        );
        assert_eq!(prompt.as_deref(), Some("revised prompt"));

        for (code, expected) in [
            ("DataInspectionFailed", AiErrorKind::ContentFiltered),
            ("IPInfringementSuspect", AiErrorKind::ContentFiltered),
            ("Throttling.AllocationQuota", AiErrorKind::QuotaExceeded),
            ("Arrearage", AiErrorKind::QuotaExceeded),
            ("Throttling.RateQuota", AiErrorKind::RateLimitError),
            ("InvalidParameter", AiErrorKind::InvalidRequest),
            ("ModelNotFound", AiErrorKind::InvalidRequest),
        ] {
            let err = classify_error(
                400,
                &headers,
                json!({"code": code, "message": "failed"})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(err.kind.model_error_kind(), Some(expected), "{code}");
        }
        for (status, expected) in [
            (401, AiErrorKind::AuthenticationError),
            (403, AiErrorKind::AuthorizationError),
        ] {
            let err = classify_error(
                status,
                &headers,
                br#"{"code":"DataInspectionFailed","message":"rejected"}"#,
            );
            assert_eq!(err.kind.model_error_kind(), Some(expected));
        }
    }
}
