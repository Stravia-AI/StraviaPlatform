use std::{io::Cursor, time::Instant};

use bytes::Bytes;
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use serde_json::{Value, json};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    artifact::{ArtifactError, ArtifactId, bytes_stream},
    protocol::ir::{AiItem, AiRequest, ContentBlock, MediaSource, MessageContent, Role},
};
use stravia_vendor_sdk::{
    MediaArtifact, MediaImageAspectRatio, MediaImageRequest, MediaImageResolution, MediaReference,
    OperationOutput,
};

use super::{GenerationError, config};
use crate::{
    Gateway,
    plugin::{VendorCallContext, VendorRequest},
};

// 平台对一次调用的素材预算；供应商更小的限制必须由插件明确拒绝。
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
    aspect_ratio: Option<MediaImageAspectRatio>,
    #[serde(default, deserialize_with = "optional_non_null")]
    resolution: Option<MediaImageResolution>,
    #[serde(default, deserialize_with = "optional_non_null")]
    reference_images: Option<Vec<String>>,
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

    let mut observation_request = reference_request(
        route.model_id.clone().into(),
        input.input.prompt.clone(),
        references,
    );
    normalize_references(
        gateway,
        &principal,
        &mut observation_request,
        &cancellation,
        deadline,
    )
    .await?;
    let references = materialize_references(
        gateway,
        &principal,
        &observation_request,
        &cancellation,
        deadline,
    )
    .await?;
    let request = MediaImageRequest {
        prompt: input.input.prompt,
        references,
        aspect_ratio: input.input.aspect_ratio,
        resolution: input.input.resolution,
    };

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
                    route_id: route.model_id.clone().into(),
                    model_display_name: route.display_name.clone(),
                    ingress_protocol: "mcp".into(),
                },
                AdmissionFacts {
                    client_request: observation_request.clone(),
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            )
    });
    let result = generate_image(
        gateway,
        &route,
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
    result.map(|(value, _publication)| value)
}

async fn generate_image(
    gateway: &Gateway,
    route: &crate::db::models::RouteConfig,
    request: MediaImageRequest,
    principal: Principal,
    cancellation: CancellationToken,
    deadline: Instant,
    observer: crate::interaction_observation::RunObserver,
) -> Result<(Value, tokio::sync::OwnedRwLockReadGuard<()>), GenerationError> {
    let mut context = VendorCallContext::new(
        cancellation.clone(),
        stravia_runtime_contract::Deadline::fixed(deadline),
    );
    context.observer = Some(observer);
    let execution = gateway
        .execute_vendor_route(
            &principal,
            route,
            VendorRequest::MediaImage(request),
            context,
        )
        .await
        .map_err(|error| vendor_route_error(error, &cancellation, deadline))?;
    let response = match execution.output {
        OperationOutput::MediaImage(response) => response,
        _ => {
            return Err(GenerationError::new(
                "invalid_generation_output",
                "Image generation returned an unexpected operation result",
            ));
        }
    };
    let [
        MediaArtifact {
            media_type,
            bytes,
            upstream_ref: _,
            metadata: _,
        },
    ] = response.artifacts.try_into().map_err(|_| {
        GenerationError::new(
            "invalid_generation_output",
            "Image generation must return exactly one image",
        )
    })?;
    let bytes = Bytes::from(bytes);
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(GenerationError::new(
            "invalid_generation_output",
            "Generated image has an invalid size",
        ));
    }
    let (mime_type, width, height) = inspect_image_async(
        bytes.clone(),
        Some(media_type),
        "generated image",
        &cancellation,
        deadline,
    )
    .await?;

    check_execution(&cancellation, deadline)?;
    let publication = execution
        .publication
        .write_fence()
        .await
        .map_err(|error| publication_error(error, &cancellation, deadline))?;
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

    Ok((
        json!({
            "path": artifact.reference(),
            "mime_type": artifact.mime_type,
            "size": artifact.size,
            "media": {
                "width": width,
                "height": height,
            },
        }),
        publication,
    ))
}

fn reference_request(model: String, prompt: String, references: Vec<String>) -> AiRequest {
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
    AiRequest::new(
        model,
        vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(content),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
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

async fn materialize_references(
    gateway: &Gateway,
    principal: &Principal,
    request: &AiRequest,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<MediaReference>, GenerationError> {
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
    let mut references = Vec::with_capacity(blocks.len().saturating_sub(1));
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
        let (media_type, _, _) = inspect_image_async(
            bytes.clone(),
            Some(artifact.mime_type),
            "reference image",
            cancellation,
            deadline,
        )
        .await?;
        references.push(MediaReference {
            media_type: media_type.to_owned(),
            bytes: bytes.to_vec(),
        });
    }
    Ok(references)
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

fn vendor_route_error(
    error: anyhow::Error,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> GenerationError {
    match error.downcast_ref::<stravia_vendor_runtime::RuntimeError>() {
        Some(stravia_vendor_runtime::RuntimeError::Cancelled)
        | Some(stravia_vendor_runtime::RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::Cancelled,
            ..
        }) => GenerationError::new("cancelled", "Media generation was cancelled"),
        Some(stravia_vendor_runtime::RuntimeError::DeadlineExceeded)
        | Some(stravia_vendor_runtime::RuntimeError::Plugin {
            kind: stravia_vendor_sdk::ErrorKind::DeadlineExceeded,
            ..
        }) => deadline_error(),
        _ if cancellation.is_cancelled() || Instant::now() >= deadline => interruption(deadline),
        _ => GenerationError::new(
            "upstream_generation_failed",
            "Upstream image generation failed",
        ),
    }
}

fn publication_error(
    _error: anyhow::Error,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> GenerationError {
    if cancellation.is_cancelled() || Instant::now() >= deadline {
        interruption(deadline)
    } else {
        GenerationError::new(
            "generation_result_expired",
            "The generated image can no longer be published",
        )
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
