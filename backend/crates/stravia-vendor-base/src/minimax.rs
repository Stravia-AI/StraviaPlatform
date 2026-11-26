use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{AiErrorKind, Usage};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    Capability, DiscoverResponse, DiscoveredModel, ErrorKind, GuestHost, MediaArtifact,
    MediaImageRequest, MediaImageResponse, OriginDeclaration, PluginError, ProviderDescriptor,
    ProviderSnapshot, read_http_body,
};

const SCHEMA_LIMIT: usize = 2 * 1024 * 1024;
const REQUEST_LIMIT: usize = 64 * 1024 * 1024;
const IMAGE_LIMIT: usize = 32 * 1024 * 1024;
const RESPONSE_LIMIT: usize = IMAGE_LIMIT.div_ceil(3) * 4 + 2 * 1024 * 1024;

pub(crate) fn supports(id: &str) -> bool {
    matches!(
        id,
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan"
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
    let hosts = if descriptor.provider_id == "minimax-cn"
        || descriptor.provider_id == "minimax-cn-coding-plan"
    {
        ["platform.minimax.cn", "api.minimax.cn"]
    } else {
        ["platform.minimax.io", "api.minimax.io"]
    };
    for host in hosts {
        let origin = OriginDeclaration {
            scheme: "https".into(),
            host: host.into(),
            port: None,
        };
        if !descriptor.network.extra_origins.contains(&origin) {
            descriptor.network.extra_origins.push(origin);
        }
    }
}

fn docs_origin(id: &str) -> &'static str {
    if id == "minimax-cn" || id == "minimax-cn-coding-plan" {
        "https://platform.minimax.cn"
    } else {
        "https://platform.minimax.io"
    }
}

pub(crate) fn discover(
    host: &GuestHost,
    provider_id: &str,
    mut response: DiscoverResponse,
    restrict_to_existing: bool,
) -> Result<DiscoverResponse, PluginError> {
    let generation = fetch_schema(host, provider_id, "generations")?;
    let edits = fetch_schema(host, provider_id, "edits")?;
    let models = schema_models(&generation, "/v1/images/generations")?;
    let editable: BTreeSet<_> = schema_models(&edits, "/v1/images/edits")?
        .into_iter()
        .collect();
    merge_models(&mut response, &models, &editable, restrict_to_existing);
    Ok(response)
}

fn fetch_schema(host: &GuestHost, id: &str, operation: &str) -> Result<Value, PluginError> {
    let response = host.http_start(
        stravia_vendor_sdk::net::Request {
            method: "GET".into(),
            url: format!(
                "{}/docs/api-reference/image/generation/api/h3-image-{operation}.json",
                docs_origin(id)
            ),
            headers: crate::fields(vec![("accept".into(), "application/json".into())]),
            body: vec![],
        },
        &crate::net_options(),
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(&response, SCHEMA_LIMIT)?;
    if !(200..300).contains(&status) {
        return Err(common::upstream_error(status, &headers, &body));
    }
    serde_json::from_slice(&body).map_err(|_| upstream("MiniMax OpenAPI document is invalid JSON"))
}

fn schema_models(document: &Value, path: &str) -> Result<Vec<String>, PluginError> {
    let fail = || upstream("MiniMax OpenAPI model schema is invalid");
    let mut schema = document
        .get("paths")
        .and_then(|v| v.get(path))
        .and_then(|v| v.pointer("/post/requestBody/content/application~1json/schema"))
        .ok_or_else(fail)?;
    let mut seen = BTreeSet::new();
    while let Some(reference) = schema.get("$ref") {
        let reference = reference.as_str().ok_or_else(fail)?;
        let pointer = reference
            .strip_prefix('#')
            .filter(|p| p.starts_with('/'))
            .ok_or_else(fail)?;
        if !seen.insert(pointer) || seen.len() > 32 {
            return Err(fail());
        }
        schema = document.pointer(pointer).ok_or_else(fail)?;
    }
    let values = schema
        .pointer("/properties/model/enum")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(fail)?;
    let mut result = Vec::new();
    let mut unique = BTreeSet::new();
    for value in values {
        let id = value
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(fail)?;
        if unique.insert(id) {
            result.push(id.to_owned());
        }
    }
    Ok(result)
}

fn merge_models(
    response: &mut DiscoverResponse,
    models: &[String],
    editable: &BTreeSet<String>,
    restricted: bool,
) {
    for id in models {
        let edit = editable.contains(id);
        if let Some(model) = response.models.iter_mut().find(|m| m.id == *id) {
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
            response.models.push(DiscoveredModel { id: id.clone(), display_name: id.clone(), family: None, selector: Some(id.clone()), capabilities: vec!["media_image".into()], metadata: [("modalities".into(), json!({"input": if edit { vec!["text", "image"] } else { vec!["text"] }, "output":["image"]}))].into_iter().collect() });
        }
    }
}

fn endpoint(base: &str, edit: bool) -> Result<String, PluginError> {
    let mut url = url::Url::parse(base).map_err(|_| invalid("MiniMax base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("MiniMax base URL is invalid"));
    }
    if url.scheme() == "https" && url.host_str() == Some("api.minimaxi.com") && url.port().is_none()
    {
        url.set_host(Some("api.minimax.cn"))
            .map_err(|_| invalid("MiniMax base URL is invalid"))?;
    }
    let path = url.path().trim_end_matches('/');
    let prefix = path
        .strip_suffix("/anthropic/v1")
        .or_else(|| path.strip_suffix("/anthropic"))
        .or_else(|| path.strip_suffix("/v1"))
        .unwrap_or(path);
    let suffix = if edit { "edits" } else { "generations" };
    let path = format!("{prefix}/v1/images/{suffix}");
    url.set_path(&path);
    Ok(url.into())
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
        .ok_or_else(|| invalid("MiniMax image model is missing"))?;
    let key = provider
        .credentials
        .get("apiKey")
        .or_else(|| provider.options.get("apiKey"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| common::plugin_error(ErrorKind::Auth, "MiniMax apiKey is missing"))?;
    let body = build_request(model, &request)?;
    let url = endpoint(&provider.base_url, !request.references.is_empty())?;
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
        .map_err(|_| upstream("MiniMax image response is invalid JSON"))?;
    decode_response(&value, &headers)
}

#[derive(serde::Serialize)]
struct ImageInput {
    image_url: String,
}

#[derive(serde::Serialize)]
struct ImageRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    n: u8,
    stream: bool,
    response_format: &'static str,
    output_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<stravia_vendor_sdk::MediaImageAspectRatio>,
    #[serde(skip_serializing_if = "Option::is_none")]
    area: Option<stravia_vendor_sdk::MediaImageResolution>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageInput>,
}

fn build_request(model: &str, request: &MediaImageRequest) -> Result<Vec<u8>, PluginError> {
    if request.prompt.trim().is_empty() {
        return Err(invalid("MiniMax image prompt is empty"));
    }
    if request.references.len() > 16 {
        return Err(invalid("MiniMax allows at most 16 reference images"));
    }
    let mut images = Vec::with_capacity(request.references.len());
    let mut encoded_size = 0usize;
    for reference in &request.references {
        if reference.bytes.is_empty()
            || reference.bytes.len() > IMAGE_LIMIT
            || ![
                "image/jpeg",
                "image/png",
                "image/webp",
                "image/heic",
                "image/heif",
            ]
            .contains(&reference.media_type.as_str())
        {
            return Err(invalid(
                "MiniMax reference image has an unsupported type or size",
            ));
        }
        let size = reference.bytes.len().div_ceil(3).saturating_mul(4);
        encoded_size = encoded_size.saturating_add(size);
        if encoded_size > REQUEST_LIMIT {
            return Err(invalid("MiniMax image request exceeds 64 MiB"));
        }
        let mut image_url = String::with_capacity(5 + reference.media_type.len() + 8 + size);
        image_url.push_str("data:");
        image_url.push_str(&reference.media_type);
        image_url.push_str(";base64,");
        STANDARD.encode_string(&reference.bytes, &mut image_url);
        images.push(ImageInput { image_url });
    }
    let body = ImageRequest {
        model,
        prompt: &request.prompt,
        n: 1,
        stream: false,
        response_format: "b64_json",
        output_format: "jpeg",
        aspect_ratio: request.aspect_ratio,
        area: request.resolution,
        images,
    };
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| invalid("MiniMax image request could not be encoded"))?;
    if bytes.len() > REQUEST_LIMIT {
        return Err(invalid("MiniMax image request exceeds 64 MiB"));
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
        .unwrap_or("MiniMax image request failed");
    let code = error
        .get("code")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .or_else(|| {
            message
                .rsplit_once('(')
                .and_then(|(_, tail)| tail.strip_suffix(')'))
                .map(str::to_owned)
        });
    let kind = match code.as_deref() {
        Some("1026" | "1027") => Some(AiErrorKind::ContentFiltered),
        Some("2068") => Some(AiErrorKind::InvalidRequest),
        Some("1008" | "2053" | "2067" | "2056") => Some(AiErrorKind::QuotaExceeded),
        Some("1002") => Some(AiErrorKind::RateLimitError),
        _ => None,
    };
    match kind {
        Some(kind) => common::model_error(kind, message),
        None => common::upstream_error(status, headers, body),
    }
}

fn decode_response(
    value: &Value,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<MediaImageResponse, PluginError> {
    if value.get("error").is_some() {
        return Err(classify_error(200, headers, value.to_string().as_bytes()));
    }
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream("MiniMax returned no image"))?;
    for item in data {
        if let Some(error) = item.get("error").filter(|v| !v.is_null()) {
            return Err(classify_error(
                200,
                headers,
                json!({"error":error}).to_string().as_bytes(),
            ));
        }
    }
    if data.len() != 1 {
        return Err(upstream("MiniMax must return exactly one image"));
    }
    let encoded = data[0]
        .get("b64_json")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| upstream("MiniMax image bytes are missing"))?;
    if encoded.len() % 4 != 0 || encoded.len() > IMAGE_LIMIT.div_ceil(3) * 4 {
        return Err(upstream("MiniMax image has invalid encoded size"));
    }
    let padding = encoded.bytes().rev().take_while(|b| *b == b'=').count();
    if padding > 2 || encoded.len() / 4 * 3 - padding > IMAGE_LIMIT {
        return Err(upstream("MiniMax image exceeds 32 MiB"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| upstream("MiniMax image contains invalid base64"))?;
    if bytes.is_empty() {
        return Err(upstream("MiniMax image is empty"));
    }
    let media_type = match value.get("output_format").and_then(Value::as_str) {
        Some("jpeg" | "jpg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => {
            return Err(upstream(
                "MiniMax image output format is missing or invalid",
            ));
        }
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
    use stravia_vendor_sdk::{MediaImageAspectRatio, MediaImageResolution, MediaReference};

    fn schema(path: &str, names: Value) -> Value {
        json!({"servers":[{"url":"https://attacker.invalid"}],"paths":{(path):{"post":{"requestBody":{"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Renamed"}}}}}}},"components":{"schemas":{"Renamed":{"properties":{"model":{"enum":names}}}}}})
    }

    #[test]
    fn dynamic_models_merge_and_static_restriction() {
        let generation = schema_models(
            &schema("/v1/images/generations", json!(["new", "new", "other"])),
            "/v1/images/generations",
        )
        .unwrap();
        let edits = schema_models(
            &schema("/v1/images/edits", json!(["new"])),
            "/v1/images/edits",
        )
        .unwrap()
        .into_iter()
        .collect();
        let mut response = DiscoverResponse {
            models: vec![DiscoveredModel {
                id: "new".into(),
                display_name: "existing".into(),
                family: None,
                selector: None,
                capabilities: vec!["infer".into()],
                metadata: [
                    (
                        "modalities".into(),
                        json!({"input":["text"],"output":["text"]}),
                    ),
                    ("limit".into(), json!({"context":42})),
                ]
                .into_iter()
                .collect(),
            }],
            next_cursor: None,
        };
        merge_models(&mut response, &generation, &edits, true);
        assert_eq!(response.models.len(), 1);
        assert_eq!(
            response.models[0].metadata["modalities"],
            json!({"input":["text","image"],"output":["text","image"]})
        );
        assert_eq!(response.models[0].metadata["limit"]["context"], 42);
        merge_models(&mut response, &generation, &edits, false);
        assert_eq!(response.models.len(), 2);
        assert_eq!(
            response.models[0]
                .capabilities
                .iter()
                .filter(|c| *c == "media_image")
                .count(),
            1
        );
        assert_eq!(
            response.models[1].metadata["modalities"],
            json!({"input":["text"],"output":["image"]})
        );
        merge_models(&mut response, &generation, &BTreeSet::new(), true);
        assert_eq!(
            response.models[0].metadata["modalities"],
            json!({"input":["text"],"output":["text","image"]})
        );
    }

    #[test]
    fn invalid_schema_and_pagination() {
        let path = "/v1/images/generations";
        assert!(schema_models(&schema(path, json!([])), path).is_err());
        assert!(schema_models(&schema(path, json!([42])), path).is_err());
        assert!(schema_models(&schema(path, json!("model")), path).is_err());
        let mut inline = schema(path, json!(["model"]));
        inline["paths"][path]["post"]["requestBody"]["content"]["application/json"]["schema"] =
            json!({"properties":{"model":{"enum":["inline"]}}});
        assert_eq!(schema_models(&inline, path).unwrap(), vec!["inline"]);
        let mut cycle = schema(path, json!(["model"]));
        cycle["components"]["schemas"]["Renamed"] = json!({"$ref":"#/components/schemas/Renamed"});
        assert!(schema_models(&cycle, path).is_err());
        let mut document = schema(path, json!(["model"]));
        document["paths"][path]["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"] =
            json!("https://attacker.invalid/schema");
        assert!(schema_models(&document, path).is_err());
        let mut response = DiscoverResponse {
            models: vec![],
            next_cursor: Some("next".into()),
        };
        merge_models(&mut response, &["model".into()], &BTreeSet::new(), false);
        assert!(response.models.is_empty());
    }

    #[test]
    fn endpoint_and_request_preserve_inputs() {
        assert_eq!(
            endpoint("https://api.minimaxi.com/anthropic/v1", false).unwrap(),
            "https://api.minimax.cn/v1/images/generations"
        );
        assert_eq!(
            endpoint("https://example.com/deployment/anthropic/v1", true).unwrap(),
            "https://example.com/deployment/v1/images/edits"
        );
        assert_eq!(
            endpoint("https://example.com/v1", false).unwrap(),
            "https://example.com/v1/images/generations"
        );
        assert_eq!(
            endpoint("https://example.com/deployment/v1", true).unwrap(),
            "https://example.com/deployment/v1/images/edits"
        );
        let request = MediaImageRequest {
            prompt: " prompt ".into(),
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
            aspect_ratio: Some(MediaImageAspectRatio::Wide),
            resolution: Some(MediaImageResolution::FourK),
        };
        let body: Value =
            serde_json::from_slice(&build_request("manual-model", &request).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"model":"manual-model","prompt":" prompt ","n":1,"stream":false,"response_format":"b64_json","output_format":"jpeg","aspect_ratio":"16:9","area":"4K","images":[{"image_url":"data:image/png;base64,AQ=="},{"image_url":"data:image/webp;base64,Ag=="}]})
        );
    }

    #[test]
    fn response_usage_and_failures() {
        let headers = stravia_vendor_sdk::net::Fields::new();
        let value = json!({"data":[{"b64_json":"AQ==","revised_prompt":"revised"}],"output_format":"jpeg","request_id":"id","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}});
        let result = decode_response(&value, &headers).unwrap();
        assert_eq!(result.artifacts[0].bytes, vec![1]);
        assert_eq!(result.artifacts[0].upstream_ref.as_deref(), Some("id"));
        let usage = result.usage.unwrap();
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                usage.required_components_known
            ),
            (2, 3, 5, true)
        );
        for data in [
            json!([]),
            json!([{"b64_json":"@@=="}]),
            json!([{"b64_json":"AQ=="},{"b64_json":"AQ=="}]),
        ] {
            assert!(
                decode_response(&json!({"data":data,"output_format":"jpeg"}), &headers).is_err()
            );
        }
        for code in ["1026", "1027"] {
            let err = decode_response(
                &json!({"data":[{"error":{"code":code,"message":"rejected"}}]}),
                &headers,
            )
            .unwrap_err();
            assert_eq!(
                err.kind.model_error_kind(),
                Some(AiErrorKind::ContentFiltered)
            );
        }
        let err = decode_response(
            &json!({"data":[{"error":{"code":"2068","message":"rejected"}}]}),
            &headers,
        )
        .unwrap_err();
        assert_eq!(
            err.kind.model_error_kind(),
            Some(AiErrorKind::InvalidRequest)
        );
        assert!(!err.kind.model_error_kind().unwrap().is_retryable());
        for (status, expected) in [
            (401, AiErrorKind::AuthenticationError),
            (403, AiErrorKind::AuthorizationError),
        ] {
            let err = classify_error(
                status,
                &headers,
                br#"{"error":{"code":"1027","message":"rejected"}}"#,
            );
            assert_eq!(err.kind.model_error_kind(), Some(expected));
        }
    }
}
