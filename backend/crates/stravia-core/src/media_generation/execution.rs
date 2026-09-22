use std::{io::Cursor, time::Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures::StreamExt;
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use serde_json::{Value, json};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    artifact::{ArtifactError, ArtifactId, bytes_stream},
    model_turn::CanonicalEvent,
    protocol::ir::{AiRequest, ContentBlock, MediaSource, MessageContent},
};

use super::{GenerationError, config};
use crate::{
    Gateway,
    model_turn::{ModelTurnAuthorization, TurnInput},
};

// 平台对一次调用的素材预算；不是独立 Codex Images API 的数量保证。
pub(crate) const MAX_REFERENCE_IMAGES: usize = 5;
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 25_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateRequest {
    #[serde(rename = "type")]
    media_type: String,
    input: ImageInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageInput {
    prompt: String,
    #[serde(default, deserialize_with = "optional_non_null")]
    aspect_ratio: Option<AspectRatio>,
    #[serde(default, deserialize_with = "optional_non_null")]
    resolution: Option<Resolution>,
    #[serde(default, deserialize_with = "optional_non_null")]
    reference_images: Option<Vec<String>>,
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) enum AspectRatio {
    #[serde(rename = "1:1")]
    Square,
    #[serde(rename = "3:4")]
    Portrait,
    #[serde(rename = "4:3")]
    Landscape,
    #[serde(rename = "9:16")]
    Tall,
    #[serde(rename = "16:9")]
    Wide,
}

#[derive(Clone, Copy, Deserialize)]
pub(crate) enum Resolution {
    #[serde(rename = "1K")]
    OneK,
    #[serde(rename = "2K")]
    TwoK,
    #[serde(rename = "4K")]
    FourK,
}

fn optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub(crate) async fn generate(
    gateway: &Gateway,
    input: Value,
    principal: Principal,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<Value, GenerationError> {
    let input: GenerateRequest = serde_json::from_value(input).map_err(|error| {
        GenerationError::new(
            "invalid_generation_input",
            format!("Invalid media generation input: {error}"),
        )
    })?;
    if input.media_type != "image" {
        return Err(GenerationError::new(
            "unsupported_generation_type",
            "Only image generation is supported",
        ));
    }
    if input.input.prompt.trim().is_empty() {
        return Err(GenerationError::new(
            "invalid_generation_input",
            "Image prompt must not be empty",
        ));
    }
    let references = input.input.reference_images.unwrap_or_default();
    if references.len() > MAX_REFERENCE_IMAGES {
        return Err(GenerationError::new(
            "reference_image_limit_exceeded",
            format!("Image generation accepts at most {MAX_REFERENCE_IMAGES} reference images"),
        ));
    }
    if references.iter().any(|reference| {
        if reference.starts_with("stravia://artifacts/") {
            reference.contains(['?', '#']) || ArtifactId::from_reference(reference).is_err()
        } else if reference.starts_with("stravia://")
            || reference.starts_with("sa:")
            || reference.starts_with("https://stravia/artifact/")
        {
            true
        } else {
            !url::Url::parse(reference).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        }
    }) {
        return Err(GenerationError::new(
            "invalid_reference_image",
            "Reference images must be plain Artifact References without read options, or public HTTP(S) URLs",
        ));
    }

    let route = config::validated_route(gateway).await?;
    check_execution(&cancellation, deadline)?;

    let mut request = crate::provider::openai::codex::media_generation::image_request(
        route,
        input.input.prompt,
        references,
        input.input.aspect_ratio,
        input.input.resolution,
    );
    normalize_references(gateway, &principal, &mut request, &cancellation, deadline).await?;
    validate_references(gateway, &principal, &request, &cancellation, deadline).await?;
    request.instructions = Some(if contains_reference_images(&request) {
        "Use the available image generation tool to edit or create exactly one PNG image for the user request. Treat the provided input images as ordered edit/reference images. Do not use any other tool."
    } else {
        "Use the available image generation tool to generate exactly one PNG image for the user request. Do not use any other tool."
    }.to_owned());

    let inherited = crate::interaction_observation::scope::current();
    let standalone = inherited.is_none();
    let observer = inherited.unwrap_or_else(|| {
        use crate::interaction_observation::{AdmissionFacts, IngressStart, RunStart};
        let id = stravia_runtime_contract::identifier::new_id();
        gateway
            .observation
            .observe_ingress(IngressStart {
                id: id.clone(),
                method: "MCP".into(),
                path: "tools/call/generate".into(),
                protocol: "mcp".into(),
            })
            .admit(
                RunStart {
                    id,
                    principal: principal.api_key_id().into(),
                    api_key_id: Some(principal.api_key_id().into()),
                    api_key_name: None,
                    route_id: request.model.clone(),
                    model_display_name: None,
                    ingress_protocol: "mcp".into(),
                },
                AdmissionFacts {
                    client_request: request.clone(),
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            )
    });
    let result = generate_image(
        gateway,
        request,
        principal,
        cancellation,
        deadline,
        observer.clone(),
    )
    .await;
    if standalone {
        observer.finish(crate::interaction_observation::RunOutcome {
            delivery_completed_at: None,
            status: match &result {
                Ok(_) => "completed",
                Err(error) if error.code == "cancelled" => "cancelled",
                Err(_) => "failed",
            }
            .into(),
            terminal_reason: result.as_ref().err().map(|error| error.code.to_owned()),
            generation_node_id: None,
            generation_root_id: None,
        });
    }
    result
}

async fn generate_image(
    gateway: &Gateway,
    request: AiRequest,
    principal: Principal,
    cancellation: CancellationToken,
    deadline: Instant,
    observer: crate::interaction_observation::RunObserver,
) -> Result<Value, GenerationError> {
    let mut turn_input = TurnInput::new(principal.clone(), request)
        .with_authorization(ModelTurnAuthorization::CapabilityGrant)
        .with_execution(cancellation.clone(), deadline)
        .with_normalized_attachments()
        .without_responses_websocket();
    turn_input = turn_input.with_observer(observer);
    let turn = gateway
        .model_turn
        .execute(turn_input)
        .await
        .map_err(model_turn_error)?;

    let mut output = turn.output;
    let mut image_result = None;
    while let Some(event) = output.next().await {
        match event.map_err(model_turn_error)? {
            CanonicalEvent::Completed(response) => {
                image_result = Some(
                    crate::provider::openai::codex::media_generation::image_result(*response)?,
                );
            }
            CanonicalEvent::Delta(_) => {}
            CanonicalEvent::Compacted(_) => {
                return Err(GenerationError::new(
                    "invalid_generation_output",
                    "Image generation returned an unexpected compacted response",
                ));
            }
        }
    }
    let encoded = image_result.ok_or_else(|| {
        GenerationError::new(
            "invalid_generation_output",
            "Image generation returned no image",
        )
    })?;
    let bytes = decode_image_result(&encoded)?;
    let (mime_type, width, height) = inspect_image_async(
        bytes.clone(),
        None,
        "generated image",
        &cancellation,
        deadline,
    )
    .await?;

    check_execution(&cancellation, deadline)?;
    let retention = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(interruption(deadline)),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(deadline_error()),
        result = crate::media::ingest::retention(gateway) => result.map_err(output_storage_error)?,
    };
    let store = gateway.artifact_store().ok_or_else(|| {
        GenerationError::new("artifact_storage_failed", "Artifact storage is unavailable")
    })?;
    let size = bytes.len() as u64;
    let artifact = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(interruption(deadline)),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(deadline_error()),
        result = store.ingest(&principal, mime_type, Some(size), bytes_stream(bytes), retention) => {
            result.map_err(output_storage_error)?
        },
    };

    Ok(json!({
        "path": artifact.reference(),
        "mime_type": artifact.mime_type,
        "size": artifact.size,
        "media": {
            "width": width,
            "height": height,
        },
    }))
}

async fn normalize_references(
    gateway: &Gateway,
    principal: &Principal,
    request: &mut AiRequest,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), GenerationError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(interruption(deadline)),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Err(deadline_error()),
        result = crate::media::ingest::normalize_request(gateway, principal, request, cancellation) => {
            result.map_err(reference_error)
        },
    }
}

async fn validate_references(
    gateway: &Gateway,
    principal: &Principal,
    request: &AiRequest,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), GenerationError> {
    let retention = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(interruption(deadline)),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(deadline_error()),
        result = crate::media::ingest::retention(gateway) => result.map_err(reference_error)?,
    };
    let store = gateway.artifact_store().ok_or_else(|| {
        GenerationError::new(
            "reference_image_unavailable",
            "Artifact storage is unavailable",
        )
    })?;
    let blocks = request
        .items
        .first()
        .and_then(|item| match &item.content {
            MessageContent::Blocks(blocks) => Some(blocks.as_slice()),
            MessageContent::Text(_) => None,
        })
        .unwrap_or_default();
    for block in blocks {
        let ContentBlock::Image {
            source: MediaSource::Url(reference),
            ..
        } = block
        else {
            continue;
        };
        let id = ArtifactId::from_reference(reference).map_err(reference_error)?;
        let (artifact, bytes) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(interruption(deadline)),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(deadline_error()),
            result = store.read_bytes(principal, &id, retention) => result.map_err(reference_error)?,
        };
        if artifact.size > MAX_IMAGE_BYTES as u64 || bytes.len() > MAX_IMAGE_BYTES {
            return Err(GenerationError::new(
                "unsupported_reference_image",
                "Reference image exceeds the 32 MiB limit",
            ));
        }
        inspect_image_async(
            bytes,
            Some(artifact.mime_type),
            "reference image",
            cancellation,
            deadline,
        )
        .await?;
    }
    Ok(())
}

fn contains_reference_images(request: &AiRequest) -> bool {
    request.items.iter().any(|item| {
        matches!(&item.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ContentBlock::Image { .. })))
    })
}

fn decode_image_result(encoded: &str) -> Result<Bytes, GenerationError> {
    let encoded = encoded.trim();
    if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(GenerationError::new(
            "invalid_generation_output",
            "Generated image exceeds the 32 MiB limit",
        ));
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| {
        GenerationError::new(
            "invalid_generation_output",
            "Generated image is not valid base64",
        )
    })?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(GenerationError::new(
            "invalid_generation_output",
            "Generated image has an invalid size",
        ));
    }
    Ok(Bytes::from(bytes))
}

fn inspect_image(
    bytes: &[u8],
    declared_mime: Option<&str>,
    label: &str,
) -> Result<(&'static str, u32, u32), GenerationError> {
    let format =
        image::guess_format(bytes).map_err(|_| image_error(label, "format is unsupported"))?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => return Err(image_error(label, "format is unsupported")),
    };
    if declared_mime.is_some_and(|declared| {
        declared
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            != mime
    }) {
        return Err(image_error(
            label,
            "declared MIME type does not match its bytes",
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|_| image_error(label, "could not be decoded"))?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_IMAGE_EDGE
        || height > MAX_IMAGE_EDGE
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(image_error(label, "dimensions exceed supported limits"));
    }
    image::DynamicImage::from_decoder(decoder)
        .map_err(|_| image_error(label, "could not be decoded"))?;
    Ok((mime, width, height))
}

async fn inspect_image_async(
    bytes: Bytes,
    declared_mime: Option<String>,
    label: &'static str,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(&'static str, u32, u32), GenerationError> {
    let worker_cancellation = cancellation.clone();
    let mut inspection = tokio::task::spawn_blocking(move || {
        check_execution(&worker_cancellation, deadline)?;
        let result = inspect_image(&bytes, declared_mime.as_deref(), label)?;
        check_execution(&worker_cancellation, deadline)?;
        Ok(result)
    });
    tokio::select! {
        biased;
        result = &mut inspection => result.map_err(|_| image_error(label, "could not be decoded"))?,
        _ = cancellation.cancelled() => {
            if let Err(error) = inspection.await {
                tracing::debug!(%error, "Image inspection failed after cancellation");
            }
            Err(interruption(deadline))
        },
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            if let Err(error) = inspection.await {
                tracing::debug!(%error, "Image inspection failed after deadline");
            }
            Err(deadline_error())
        },
    }
}

fn image_error(label: &str, reason: &str) -> GenerationError {
    GenerationError::new(
        if label == "reference image" {
            "unsupported_reference_image"
        } else {
            "invalid_generation_output"
        },
        format!("The {label} {reason}"),
    )
}

fn reference_error(error: ArtifactError) -> GenerationError {
    match error {
        ArtifactError::NotFound => GenerationError::new(
            "reference_image_unavailable",
            "A reference image is not available",
        ),
        ArtifactError::Forbidden | ArtifactError::Unauthorized => GenerationError::new(
            "reference_image_forbidden",
            "A reference image is not owned by this Principal",
        ),
        ArtifactError::Invalid(message) => GenerationError::new(
            "invalid_reference_image",
            format!("Invalid reference image: {message}"),
        ),
        ArtifactError::Storage(error) => {
            tracing::warn!(%error, "Media generation reference could not be read");
            GenerationError::new(
                "reference_image_unavailable",
                "A reference image could not be read",
            )
        }
    }
}

fn output_storage_error(error: ArtifactError) -> GenerationError {
    tracing::warn!(%error, "Generated image could not be stored");
    GenerationError::new(
        "artifact_storage_failed",
        "The generated image could not be stored",
    )
}

fn model_turn_error(
    error: stravia_runtime_contract::model_turn::ModelTurnError,
) -> GenerationError {
    match error.code.as_str() {
        "cancelled" => GenerationError::new("cancelled", "Media generation was cancelled"),
        "deadline_exceeded" => deadline_error(),
        "authorization_failed" | "api_key_not_found" | "api_key_expired" => {
            GenerationError::new("authorization_failed", "Principal authorization failed")
        }
        // Provider error text can echo credentials, input images, or internal
        // locations. Redacted diagnostics retain details; tool results do not.
        _ => GenerationError::new(
            "upstream_generation_failed",
            format!("Upstream image generation failed ({})", error.code),
        ),
    }
}

fn check_execution(
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), GenerationError> {
    if cancellation.is_cancelled() {
        return Err(interruption(deadline));
    }
    if Instant::now() >= deadline {
        return Err(deadline_error());
    }
    Ok(())
}

fn interruption(deadline: Instant) -> GenerationError {
    if Instant::now() >= deadline {
        deadline_error()
    } else {
        GenerationError::new("cancelled", "Media generation was cancelled")
    }
}

fn deadline_error() -> GenerationError {
    GenerationError::new("deadline_exceeded", "Media generation deadline exceeded")
}
