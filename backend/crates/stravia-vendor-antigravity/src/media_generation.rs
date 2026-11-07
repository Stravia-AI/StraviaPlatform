use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::protocol::{
    ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, ir::AiErrorKind,
};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, MediaArtifact, MediaImageAspectRatio, MediaImageRequest,
    MediaImageResponse, PluginError, ProviderSnapshot, read_http_body,
};

use crate::{client, wire};

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENCODED_IMAGE_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_RESPONSE_BYTES: usize = MAX_ENCODED_IMAGE_BYTES + 2 * 1024 * 1024;

pub(crate) fn generate(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    request: MediaImageRequest,
) -> Result<MediaImageResponse, PluginError> {
    let model = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.selector.as_deref())
        .filter(|model| !model.trim().is_empty())
        .or(provider
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty()))
        .ok_or_else(|| invalid("Antigravity requires an upstream image model"))?;
    let body = build_request(&request)?;
    let project = client::project(host, provider)?;
    let envelope = json!({
        "project":project, "model":model, "request":body,
        "requestType":"image_gen",
        "requestId":format!("image_gen/{}/{}/1", chrono::Utc::now().timestamp_millis(), uuid::Uuid::new_v4()),
        "userAgent":"antigravity"
    });
    let body = serde_json::to_vec(&envelope)
        .map_err(|_| invalid("Antigravity image request could not be encoded"))?;
    let headers = client::headers(provider)?;
    host.emit_started()?;
    let response = client::http_start(
        host,
        stravia_vendor_sdk::net::Request {
            method: "POST".into(),
            url: client::endpoint(provider, "generateContent"),
            headers,
            body,
        },
    )?;
    let status = response.status()?;
    let headers = response.headers()?;
    if !(200..300).contains(&status) {
        let body = read_http_body(&response, client::MAX_JSON_BODY)?;
        return Err(common::upstream_error(status, &headers, &body));
    }
    let body = read_http_body(&response, MAX_RESPONSE_BYTES)?;
    let response = serde_json::from_slice(&body)
        .map_err(|_| upstream("Antigravity image response contained invalid JSON"))?;
    decode_response(response, &headers)
}

fn build_request(request: &MediaImageRequest) -> Result<Value, PluginError> {
    if request.prompt.trim().is_empty() {
        return Err(invalid("Antigravity image prompt must not be empty"));
    }
    let mut parts = Vec::with_capacity(request.references.len() + 1);
    parts.push(json!({"text":request.prompt}));
    for reference in &request.references {
        if reference.bytes.is_empty()
            || reference.bytes.len() > MAX_IMAGE_BYTES
            || !image_mime(&reference.media_type)
        {
            return Err(invalid(
                "Antigravity references must contain non-empty image bytes of at most 32 MiB",
            ));
        }
        parts.push(json!({"inlineData":{"mimeType":reference.media_type, "data":STANDARD.encode(&reference.bytes)}}));
    }
    let mut image =
        json!({"aspectRatio":request.aspect_ratio.unwrap_or(MediaImageAspectRatio::Square)});
    if let Some(size) = request.resolution {
        image["imageSize"] = json!(size);
    }
    Ok(json!({
        "contents":[{"role":"user", "parts":parts}],
        "generationConfig":{"candidateCount":1, "imageConfig":image}
    }))
}

fn decode_response(
    mut value: Value,
    headers: &stravia_vendor_sdk::net::Fields,
) -> Result<MediaImageResponse, PluginError> {
    if let Some(error) = wire::response_error(&value, headers) {
        return Err(error);
    }
    let mut inner = value.get_mut("response").map(Value::take).unwrap_or(value);
    if let Some(reason) = inner
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .filter(|reason| policy_reason(reason))
    {
        let message = inner
            .pointer("/promptFeedback/blockReasonMessage")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or(reason);
        return Err(common::model_error(AiErrorKind::ContentFiltered, message));
    }
    let candidates = inner
        .get("candidates")
        .and_then(Value::as_array)
        .filter(|candidates| !candidates.is_empty())
        .ok_or_else(|| upstream("Antigravity image generation returned no image"))?;
    if candidates.len() != 1 {
        return Err(upstream("Antigravity returned multiple image candidates"));
    }
    let candidate = &candidates[0];
    if candidate
        .get("index")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(upstream(
            "Antigravity returned an unexpected image candidate",
        ));
    }
    let message = candidate
        .get("finishMessage")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty());
    let reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty());
    // 官方 JSON 拒绝响应可能只有 finishMessage，不能将已结束的失败误报为断流。
    let policy_message = message.is_some_and(|message| {
        message.contains("The image was filtered out because it violated Google's")
            && message.contains("Generative AI Prohibited Use policy")
    });
    if reason.is_some_and(policy_reason) || policy_message {
        return Err(common::model_error(
            AiErrorKind::ContentFiltered,
            message
                .or(reason)
                .unwrap_or("Antigravity image generation was blocked by policy"),
        ));
    }
    if reason != Some("STOP") {
        return Err(match message.or(reason) {
            Some(message) => upstream(message),
            None => common::model_error(
                AiErrorKind::UnexpectedEof,
                "Antigravity image response omitted its finish reason",
            ),
        });
    }
    let mut images = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("inlineData"));
    let inline = images.next().ok_or_else(|| {
        upstream(message.unwrap_or("Antigravity image generation returned no image"))
    })?;
    if images.next().is_some() {
        return Err(upstream("Antigravity returned multiple images"));
    }
    let image = decode_image(inline)?;
    let usage = if inner.get("usageMetadata").is_some() {
        wire::normalize_final_usage(&mut inner);
        let metadata = inner.get_mut("usageMetadata").map(Value::take);
        // 复用 Gemini codec 的已确认用量语义，包括思考与缓存计数。
        let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
        Some(
            ProtocolTransform::global()
                .bind(protocol, protocol)
                .and_then(|pair| pair.decode_response(json!({"usageMetadata":metadata})))
                .map_err(common::map_response_transform_error)?
                .usage,
        )
    } else {
        None
    };
    Ok(MediaImageResponse {
        artifacts: vec![image],
        revised_prompt: None,
        usage,
    })
}

fn policy_reason(reason: &str) -> bool {
    matches!(
        reason,
        "SAFETY"
            | "RECITATION"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "IMAGE_SAFETY"
            | "IMAGE_PROHIBITED_CONTENT"
            | "IMAGE_RECITATION"
    )
}

fn decode_image(inline: &Value) -> Result<MediaArtifact, PluginError> {
    let mime = inline
        .get("mimeType")
        .and_then(Value::as_str)
        .ok_or_else(|| upstream("Antigravity image MIME is missing"))?;
    if !image_mime(mime) {
        return Err(upstream("Antigravity returned invalid image MIME"));
    }
    let encoded = inline
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| upstream("Antigravity image bytes are missing"))?;
    if encoded.is_empty() || encoded.len() % 4 != 0 || encoded.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(upstream("Antigravity image has an invalid encoded size"));
    }
    let padding = encoded
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 || encoded.len() / 4 * 3 - padding > MAX_IMAGE_BYTES {
        return Err(exhausted("Antigravity image exceeds the 32 MiB limit"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| upstream("Antigravity image contains invalid base64"))?;
    if bytes.is_empty() {
        return Err(upstream("Antigravity returned empty image bytes"));
    }
    Ok(MediaArtifact {
        media_type: mime.into(),
        bytes,
        upstream_ref: None,
        metadata: Default::default(),
    })
}

// 像素解码和实际格式校验由 Core 统一执行，插件不维护第二套格式识别表。
fn image_mime(mime: &str) -> bool {
    mime.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        && mime.len() > 6
        && mime
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'+' | b'.'))
}

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}
fn upstream(message: &str) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}
fn exhausted(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::ResourceExhausted, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_vendor_sdk::MediaReference;

    fn png() -> Vec<u8> {
        STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap()
    }

    #[test]
    fn response_requires_one_valid_image_and_successful_finish() {
        let mut response = json!({"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png", "data":STANDARD.encode(png())}}]}}]}});
        assert!(
            decode_response(response.clone(), &stravia_vendor_sdk::net::Fields::new()).is_err()
        );
        response["response"]["candidates"][0]["finishReason"] = json!("STOP");
        assert_eq!(
            decode_response(response.clone(), &stravia_vendor_sdk::net::Fields::new())
                .unwrap()
                .artifacts[0]
                .bytes,
            png()
        );
        let parts = response["response"]["candidates"][0]["content"]["parts"]
            .as_array_mut()
            .unwrap();
        parts.push(parts[0].clone());
        assert!(decode_response(response, &stravia_vendor_sdk::net::Fields::new()).is_err());
    }

    #[test]
    fn invalid_images_and_empty_success_are_rejected() {
        for inline in [
            json!({"mimeType":"text/plain","data":"dGV4dA=="}),
            json!({"mimeType":"image/png","data":"!!!!"}),
            json!({"mimeType":"image/png","data":""}),
        ] {
            assert!(decode_image(&inline).is_err());
        }
        assert!(
            decode_response(
                json!({"response":{"candidates":[{"finishReason":"STOP"}]}}),
                &stravia_vendor_sdk::net::Fields::new()
            )
            .is_err()
        );
    }

    #[test]
    fn empty_prompts_and_invalid_references_are_rejected() {
        let mut request = MediaImageRequest {
            prompt: " ".into(),
            references: vec![],
            aspect_ratio: None,
            resolution: None,
        };
        assert!(matches!(
            build_request(&request).unwrap_err().kind,
            ErrorKind::Invalid
        ));
        request.prompt = "paint".into();
        for reference in [
            MediaReference {
                media_type: "image/png".into(),
                bytes: Vec::new(),
            },
            MediaReference {
                media_type: "text/plain".into(),
                bytes: png(),
            },
            MediaReference {
                media_type: "image/".into(),
                bytes: png(),
            },
            MediaReference {
                media_type: "image/png".into(),
                bytes: vec![0; MAX_IMAGE_BYTES + 1],
            },
        ] {
            request.references = vec![reference];
            assert!(matches!(
                build_request(&request).unwrap_err().kind,
                ErrorKind::Invalid
            ));
        }
    }

    #[test]
    fn response_rejects_candidate_mixing_failed_finish_and_upstream_error() {
        for event in [
            json!({"response":{"candidates":[{}, {}]}}),
            json!({"response":{"candidates":[{"index":1}]}}),
            json!({"response":{"candidates":[{"finishReason":"MAX_TOKENS"}]}}),
            json!({"error":{"code":429,"message":"rate limited"}}),
            json!({"response":{"error":{"code":503,"message":"unavailable"}}}),
        ] {
            assert!(decode_response(event, &stravia_vendor_sdk::net::Fields::new()).is_err());
        }
    }
}
