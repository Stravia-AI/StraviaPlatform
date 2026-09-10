//! Structured attachment admission and per-attempt transfer representations.
use base64::Engine;
use bytes::Bytes;
use std::time::Duration;
use stravia_runtime_contract::artifact::{
    ArtifactError, ArtifactId, ArtifactSettings, ArtifactStore,
};
use stravia_runtime_contract::protocol::ids::{Protocol, ProtocolId};
use stravia_runtime_contract::protocol::ir::{
    AiRequest, ContentBlock, DocumentSource, MediaSource, MessageContent, ToolResultContentKind,
};
use stravia_runtime_contract::{CancellationToken, Principal};

/// Fetch a complete public HTTP(S) resource (at most 100 MiB). DNS answers are
/// vetted and pinned; proxies are disabled and every redirect is independently vetted.
pub(crate) async fn fetch_public_file(
    url: &str,
    cancellation: &CancellationToken,
) -> Result<(String, Bytes), stravia_media::ingest::MediaBridgeError> {
    stravia_media::ingest::fetch_public_file(url, cancellation).await
}

pub(crate) use stravia_media::ingest::PublicReadResource;

pub(crate) async fn fetch_public_read_resource(
    url: &str,
    cancellation: &CancellationToken,
) -> Result<PublicReadResource, stravia_media::ingest::MediaBridgeError> {
    stravia_media::ingest::fetch_public_read_resource(url, cancellation).await
}

pub(crate) async fn settings(gateway: &crate::Gateway) -> Result<ArtifactSettings, ArtifactError> {
    gateway
        .storage
        .settings()
        .get("artifact_settings")
        .await
        .map_err(|e| ArtifactError::Storage(e.to_string()))?
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|e| ArtifactError::Invalid(format!("Invalid artifact settings: {e}")))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(crate) async fn retention(gateway: &crate::Gateway) -> Result<Duration, ArtifactError> {
    let days = gateway
        .storage
        .settings()
        .get("log_retention_days")
        .await
        .map_err(|e| ArtifactError::Storage(e.to_string()))?
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|e| ArtifactError::Invalid(format!("Invalid retention: {e}")))
        })
        .transpose()?
        .unwrap_or(7);
    Ok(Duration::from_secs(u64::from(days) * 86400))
}

pub(crate) async fn normalize_request(
    gateway: &crate::Gateway,
    principal: &Principal,
    request: &mut AiRequest,
    cancellation: &CancellationToken,
) -> Result<(), ArtifactError> {
    fn scrub(text: &mut String) {
        if let std::borrow::Cow::Owned(clean) =
            crate::agent::upload_grant::scrub_upload_grants(text)
        {
            *text = clean;
        }
    }
    scrub(&mut request.model);
    if let Some(text) = &mut request.instructions {
        scrub(text);
    }
    scrub_serialized(&mut request.generation)?;
    scrub_serialized(&mut request.embedding)?;
    scrub_serialized(&mut request.stream)?;
    scrub_serialized(&mut request.tools)?;
    scrub_serialized(&mut request.tool_choice)?;
    scrub_serialized(&mut request.reasoning)?;
    scrub_serialized(&mut request.response_format)?;
    scrub_serialized(&mut request.safety_settings)?;
    if let Some(extension) = &mut request.ext {
        use stravia_runtime_contract::protocol::ir::ProtocolExt;
        match extension {
            ProtocolExt::OpenAiChat(extension) => scrub_serialized(extension)?,
            ProtocolExt::OpenResponses(extension) => scrub_serialized(extension)?,
            ProtocolExt::Anthropic(extension) => scrub_serialized(extension)?,
            ProtocolExt::Google(extension) => scrub_serialized(extension)?,
        }
    }
    scrub_serialized(&mut request.meta.vendor)?;
    if let Some(raw) = &mut request.meta.raw {
        if let Some(body) = &mut raw.body {
            crate::agent::upload_grant::scrub_upload_grant_value(body);
        }
        scrub_serialized(&mut raw.headers)?;
        scrub(&mut raw.method);
        scrub(&mut raw.path);
    }
    for item in &mut request.items {
        match &mut item.content {
            MessageContent::Text(text) => scrub(text),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if contains_upload_grant(block)? {
                        let mut value = serde_json::to_value(&*block)
                            .map_err(|e| ArtifactError::Invalid(e.to_string()))?;
                        crate::agent::upload_grant::scrub_upload_grant_value(&mut value);
                        *block = serde_json::from_value(value)
                            .map_err(|e| ArtifactError::Invalid(e.to_string()))?;
                    }
                }
            }
        }
        if let Some(calls) = &mut item.tool_calls {
            for call in calls {
                scrub(&mut call.id);
                scrub(&mut call.name);
                scrub(&mut call.arguments);
            }
        }
        if let Some(id) = &mut item.tool_call_id {
            scrub(id);
        }
        if let Some(meta) = &mut item.meta {
            crate::agent::upload_grant::scrub_upload_grant_value(meta);
        }
    }
    if let Some(instructions) = crate::agent::upload_grant::upload_instructions(gateway).await? {
        let current = request.instructions.get_or_insert_default();
        if !current.contains(&instructions) {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&instructions);
        }
    }
    let retention = retention(gateway).await?;
    for item in &mut request.items {
        if let MessageContent::Blocks(blocks) = &mut item.content {
            normalize_blocks(gateway, principal, blocks, cancellation, retention).await?;
        }
    }
    Ok(())
}

pub(crate) async fn normalize_response(
    gateway: &crate::Gateway,
    principal: &Principal,
    response: &mut stravia_runtime_contract::protocol::ir::AiResponse,
    cancellation: &CancellationToken,
) -> Result<(), ArtifactError> {
    let retention = retention(gateway).await?;
    for item in &mut response.items {
        if let MessageContent::Blocks(blocks) = &mut item.content {
            normalize_blocks(gateway, principal, blocks, cancellation, retention).await?;
        }
    }
    Ok(())
}

fn scrub_serialized<T: serde::Serialize + serde::de::DeserializeOwned>(
    field: &mut T,
) -> Result<(), ArtifactError> {
    if contains_upload_grant(field)? {
        let mut value =
            serde_json::to_value(&*field).map_err(|e| ArtifactError::Invalid(e.to_string()))?;
        crate::agent::upload_grant::scrub_upload_grant_value(&mut value);
        *field =
            serde_json::from_value(value).map_err(|e| ArtifactError::Invalid(e.to_string()))?;
    }
    Ok(())
}

fn contains_upload_grant(value: &impl serde::Serialize) -> Result<bool, ArtifactError> {
    struct Scan {
        matched: usize,
        found: bool,
    }
    impl std::io::Write for Scan {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            const PREFIX: &[u8] = b"stravia_upload_";
            for &byte in bytes {
                if self.found {
                    break;
                }
                if byte == PREFIX[self.matched] {
                    self.matched += 1;
                } else {
                    self.matched = usize::from(byte == PREFIX[0]);
                }
                if self.matched == PREFIX.len() {
                    self.found = true;
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut scan = Scan {
        matched: 0,
        found: false,
    };
    serde_json::to_writer(&mut scan, value).map_err(|e| ArtifactError::Invalid(e.to_string()))?;
    Ok(scan.found)
}

fn normalize_blocks<'a>(
    gateway: &'a crate::Gateway,
    principal: &'a Principal,
    blocks: &'a mut [ContentBlock],
    cancellation: &'a CancellationToken,
    retention: Duration,
) -> futures::future::BoxFuture<'a, Result<(), ArtifactError>> {
    Box::pin(async move {
        for block in blocks {
            match block {
                ContentBlock::Image { source, .. } | ContentBlock::Audio { source } => {
                    normalize_source(gateway, principal, source, cancellation, retention).await?
                }
                ContentBlock::File { source, media_type }
                | ContentBlock::Video { source, media_type } => {
                    normalize_source(gateway, principal, source, cancellation, retention).await?;
                    if let MediaSource::Url(reference) = source {
                        let reader = store(gateway)?
                            .open(principal, &ArtifactId::from_reference(reference)?)
                            .await?;
                        *media_type = Some(reader.artifact.mime_type.clone());
                    }
                }
                ContentBlock::Document { source, .. } => match source {
                    DocumentSource::Base64Pdf { data } => {
                        let mut media = MediaSource::Base64 {
                            media_type: "application/pdf".into(),
                            data: std::mem::take(data),
                        };
                        normalize_source(gateway, principal, &mut media, cancellation, retention)
                            .await?;
                        let MediaSource::Url(reference) = media else {
                            unreachable!()
                        };
                        *source = DocumentSource::Url(reference);
                    }
                    DocumentSource::Url(url) => {
                        let mut media = MediaSource::Url(std::mem::take(url));
                        normalize_source(gateway, principal, &mut media, cancellation, retention)
                            .await?;
                        let MediaSource::Url(reference) = media else {
                            unreachable!()
                        };
                        *url = reference;
                    }
                    DocumentSource::Blocks { content } => {
                        normalize_blocks(gateway, principal, content, cancellation, retention)
                            .await?
                    }
                    DocumentSource::PlainText { .. } => {}
                },
                ContentBlock::SearchResult { content, .. } => {
                    normalize_blocks(gateway, principal, content, cancellation, retention).await?
                }
                ContentBlock::ToolResult {
                    content,
                    content_kind: Some(ToolResultContentKind::ContentBlocks),
                    ..
                }
                | ContentBlock::ServerToolResult {
                    content,
                    content_kind: Some(ToolResultContentKind::ContentBlocks),
                    ..
                } => {
                    let mut nested: Vec<ContentBlock> = serde_json::from_value(content.clone())
                        .map_err(|e| ArtifactError::Invalid(e.to_string()))?;
                    normalize_blocks(gateway, principal, &mut nested, cancellation, retention)
                        .await?;
                    *content = serde_json::to_value(nested)
                        .map_err(|e| ArtifactError::Invalid(e.to_string()))?;
                }
                _ => {}
            }
        }
        Ok(())
    })
}

fn has_media(blocks: &[ContentBlock]) -> bool {
    blocks.iter().any(|block| match block {
        ContentBlock::Image { .. }
        | ContentBlock::Audio { .. }
        | ContentBlock::File { .. }
        | ContentBlock::Video { .. } => true,
        ContentBlock::Document { source, .. } => match source {
            DocumentSource::PlainText { .. } => false,
            DocumentSource::Blocks { content } => has_media(content),
            _ => true,
        },
        ContentBlock::SearchResult { content, .. } => has_media(content),
        ContentBlock::ToolResult {
            content_kind: Some(ToolResultContentKind::ContentBlocks),
            ..
        }
        | ContentBlock::ServerToolResult {
            content_kind: Some(ToolResultContentKind::ContentBlocks),
            ..
        } => true,
        _ => false,
    })
}

fn store(gateway: &crate::Gateway) -> Result<&dyn ArtifactStore, ArtifactError> {
    gateway
        .artifact_store
        .as_deref()
        .ok_or_else(|| ArtifactError::Storage("Artifact storage is unavailable".into()))
}

async fn normalize_source(
    gateway: &crate::Gateway,
    principal: &Principal,
    source: &mut MediaSource,
    cancellation: &CancellationToken,
    retention: Duration,
) -> Result<(), ArtifactError> {
    let (mime, bytes) = match source {
        MediaSource::Url(url) => {
            if let Ok(id) = ArtifactId::from_reference(url) {
                store(gateway)?
                    .extend_retention(principal, &id, retention)
                    .await?;
                *url = format!("https://stravia/artifact/{}", id.as_str());
                return Ok(());
            }
            if url.starts_with("https://stravia/") {
                return Err(ArtifactError::Invalid("Invalid Artifact Reference".into()));
            }
            fetch_public_file(url, cancellation)
                .await
                .map_err(|e| ArtifactError::Invalid(e.message))?
        }
        MediaSource::Base64 { media_type, data } => {
            const MAX: usize = 100 * 1024 * 1024;
            if data.len() > MAX.div_ceil(3) * 4 {
                return Err(ArtifactError::Invalid("Attachment exceeds 100 MiB".into()));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| ArtifactError::Invalid("Invalid attachment base64".into()))?;
            if bytes.len() > MAX {
                return Err(ArtifactError::Invalid("Attachment exceeds 100 MiB".into()));
            }
            (media_type.clone(), Bytes::from(bytes))
        }
        MediaSource::FileId { .. } => return Ok(()),
    };
    let size = bytes.len() as u64;
    let artifact = store(gateway)?
        .ingest(
            principal,
            &mime,
            Some(size),
            Box::pin(futures::stream::once(async move { Ok(bytes) })),
            retention,
        )
        .await?;
    *source = MediaSource::Url(artifact.reference());
    Ok(())
}

/// Called anew for each selected Target, never as a response to upstream URL failure.
pub(crate) async fn materialize_request(
    gateway: &crate::Gateway,
    principal: &Principal,
    request: &mut AiRequest,
    protocol: ProtocolId,
) -> Result<Vec<(String, ArtifactId)>, ArtifactError> {
    if !request
        .items
        .iter()
        .any(|item| matches!(&item.content, MessageContent::Blocks(blocks) if has_media(blocks)))
    {
        return Ok(Vec::new());
    }
    let settings = settings(gateway).await?;
    let retention = retention(gateway).await?;
    let mut transfers = Vec::new();
    for item in &mut request.items {
        if let MessageContent::Blocks(blocks) = &mut item.content {
            materialize_blocks(
                store(gateway)?,
                principal,
                blocks,
                protocol,
                &settings,
                retention,
                &mut transfers,
            )
            .await?;
        }
    }
    Ok(transfers)
}

fn materialize_blocks<'a>(
    store: &'a dyn ArtifactStore,
    principal: &'a Principal,
    blocks: &'a mut [ContentBlock],
    protocol: ProtocolId,
    settings: &'a ArtifactSettings,
    retention: Duration,
    transfers: &'a mut Vec<(String, ArtifactId)>,
) -> futures::future::BoxFuture<'a, Result<(), ArtifactError>> {
    Box::pin(async move {
        for block in blocks {
            let kind = match block {
                ContentBlock::Image { .. } => "image",
                ContentBlock::Audio { .. } => "audio",
                ContentBlock::File { .. } => "file",
                ContentBlock::Video { .. } => "video",
                _ => "",
            };
            match block {
                ContentBlock::Image { source, .. }
                | ContentBlock::Audio { source }
                | ContentBlock::File { source, .. }
                | ContentBlock::Video { source, .. } => {
                    materialize_source(
                        store, principal, source, protocol, kind, settings, retention, transfers,
                    )
                    .await?;
                }
                ContentBlock::Document { source, .. } => match source {
                    DocumentSource::Url(url) => {
                        let mut media = MediaSource::Url(url.clone());
                        materialize_source(
                            store, principal, &mut media, protocol, "document", settings,
                            retention, transfers,
                        )
                        .await?;
                        *source = match media {
                            MediaSource::Url(url) => DocumentSource::Url(url),
                            MediaSource::Base64 { data, media_type }
                                if media_type == "application/pdf" =>
                            {
                                DocumentSource::Base64Pdf { data }
                            }
                            _ => {
                                return Err(ArtifactError::Invalid(
                                    "Document inline representation requires PDF".into(),
                                ));
                            }
                        };
                    }
                    DocumentSource::Blocks { content } => {
                        materialize_blocks(
                            store, principal, content, protocol, settings, retention, transfers,
                        )
                        .await?
                    }
                    _ => {}
                },
                ContentBlock::SearchResult { content, .. } => {
                    materialize_blocks(
                        store, principal, content, protocol, settings, retention, transfers,
                    )
                    .await?
                }
                ContentBlock::ToolResult {
                    content,
                    content_kind: Some(ToolResultContentKind::ContentBlocks),
                    ..
                }
                | ContentBlock::ServerToolResult {
                    content,
                    content_kind: Some(ToolResultContentKind::ContentBlocks),
                    ..
                } => {
                    let mut nested: Vec<ContentBlock> = serde_json::from_value(content.clone())
                        .map_err(|e| ArtifactError::Invalid(e.to_string()))?;
                    materialize_blocks(
                        store,
                        principal,
                        &mut nested,
                        protocol,
                        settings,
                        retention,
                        transfers,
                    )
                    .await?;
                    *content = serde_json::to_value(nested)
                        .map_err(|e| ArtifactError::Invalid(e.to_string()))?;
                }
                _ => {}
            }
        }
        Ok(())
    })
}

async fn materialize_source(
    store: &dyn ArtifactStore,
    principal: &Principal,
    source: &mut MediaSource,
    protocol: ProtocolId,
    kind: &str,
    settings: &ArtifactSettings,
    retention: Duration,
    transfers: &mut Vec<(String, ArtifactId)>,
) -> Result<(), ArtifactError> {
    let MediaSource::Url(reference) = source else {
        return Ok(());
    };
    let id = ArtifactId::from_reference(reference)?;
    let (url, inline) = match protocol.protocol {
        Protocol::BedrockConverse => (false, kind == "image"),
        Protocol::AnthropicMessages => (
            matches!(kind, "image" | "document"),
            matches!(kind, "image" | "document"),
        ),
        Protocol::OpenAICompatible | Protocol::WatsonxTextChat => {
            (kind == "image", matches!(kind, "image" | "audio" | "file"))
        }
        Protocol::OpenResponses => (
            matches!(kind, "image" | "file" | "video"),
            matches!(kind, "image" | "file" | "video"),
        ),
        Protocol::GoogleGemini | Protocol::GatewayLanguageModel => {
            (kind != "document", kind != "document")
        }
        Protocol::CohereChat => (kind == "image", kind == "image"),
    };
    if settings.external_signed_downloads && url {
        let download = store.download(principal, &id, retention, settings).await?;
        if download
            .expires_at
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            < 300_000
        {
            return Err(ArtifactError::Invalid(
                "Artifact download cannot retain five minutes of validity".into(),
            ));
        }
        transfers.push((download.url.clone(), id));
        *source = MediaSource::Url(download.url);
    } else if inline {
        let (artifact, bytes) = store.read_bytes(principal, &id, retention).await?;
        if kind == "audio"
            && matches!(
                protocol.protocol,
                Protocol::OpenAICompatible | Protocol::WatsonxTextChat
            )
            && !matches!(
                artifact.mime_type.as_str(),
                "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/mp3" | "audio/mpeg"
            )
        {
            return Err(ArtifactError::Invalid(
                "Target accepts only WAV or MP3 input audio".into(),
            ));
        }
        *source = MediaSource::Base64 {
            media_type: artifact.mime_type,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
    } else {
        return Err(ArtifactError::Invalid(format!(
            "Target protocol cannot represent {kind} attachment"
        )));
    }
    Ok(())
}
