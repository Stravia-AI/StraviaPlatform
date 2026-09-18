//! Office document extraction.
//!
//! An Office document source (`docx`/`xlsx`/`pptx` plus the legacy `doc`/`xls`
//! /`ppt` containers) is parsed once by `office_oxide`, rendered to a Markdown
//! snapshot, and its embedded raster images are normalized through the existing
//! JPEG pipeline. Extraction state is persisted as a JSON *document manifest*
//! derivative Artifact so repeat reads reuse the stored Markdown and embedded
//! image Artifacts instead of re-parsing.
//!
//! Trust boundary: document bytes are untrusted. Declared MIME is validated
//! against the magic container family before parsing, OOXML containers are
//! preflighted through the ZIP central directory (entry count + declared
//! decompressed total) before `office_oxide` sees the bytes, and all parsing
//! runs on the blocking pool with cooperative cancellation/deadline.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::time::{Duration, Instant};

use base64::Engine;
use bytes::Bytes;
use office_oxide::ir::{DocumentIR, Element, List, Section};
use office_oxide::ir_render::{ImageEmbed, MarkdownOptions};
use office_oxide::{Document, DocumentFormat};
use serde::{Deserialize, Serialize};
use stravia_runtime_contract::artifact::{ArtifactId, ArtifactRef, MAX_ARTIFACT_BYTES};
use stravia_runtime_contract::identifier::valid_digest_id;
use stravia_runtime_contract::{CancellationToken, Principal};

use crate::preprocessor::{
    MediaPreprocessError, PreparedDocument, PreparedDocumentImage, blocking_media_task,
    check_normalization_budget, normalize_image_until,
};
use crate::store::{MediaDerivativeStore, MediaStoreError};

/// Derivative Artifact MIME for the JSON document manifest.
pub const DOCUMENT_MANIFEST_MIME: &str = "application/vnd.stravia.media-document";
/// Artifact MIME for the stored extracted-Markdown snapshot.
pub const DOCUMENT_MARKDOWN_MIME: &str = "text/markdown";

/// Office source bytes are accepted up to the platform Artifact ceiling.
pub const MAX_DOCUMENT_BYTES: u64 = MAX_ARTIFACT_BYTES;
/// Extracted Markdown snapshot ceiling (generous; prompt-side truncation
/// happens later in the service).
pub const MAX_DOCUMENT_MARKDOWN_BYTES: usize = 16 * 1024 * 1024;
/// Manifest JSON ceiling; also the read bound for cached manifests.
pub const MAX_DOCUMENT_MANIFEST_BYTES: usize = 64 * 1024;
/// Embedded images stored as Artifacts per document.
pub const MAX_EMBEDDED_IMAGES: usize = 64;
/// Per-embedded-image byte ceiling for normalization attempts; larger images
/// are stored raw and declared un-normalizable.
pub const MAX_EMBEDDED_IMAGE_BYTES: usize = crate::preprocessor::MAX_SOURCE_BYTES;

/// OOXML ZIP preflight: maximum central-directory entry count.
const MAX_ZIP_ENTRIES: usize = 10_000;
/// OOXML ZIP preflight: maximum declared decompressed bytes across entries.
const MAX_ZIP_DECOMPRESSED_BYTES: u128 = 512 * 1024 * 1024;
/// Embedded-image alt text is untrusted content; bound it before substitution.
const MAX_ALT_BYTES: usize = 512;

/// Marker emitted by `office_oxide` Markdown rendering for embedded images
/// when `ImageEmbed::Base64` is selected.
const IMAGE_MARKER_PREFIX: &str = "[image-base64:";
const IMAGE_MARKER_SUFFIX: char = ']';

const MANIFEST_VERSION: u8 = 1;

/// Returns whether the declared MIME is an Office document Stravia can read.
pub fn is_office_document(declared_mime: &str) -> bool {
    office_document_format(declared_mime).is_some()
}

/// Maps a declared MIME (parameters tolerated, case-insensitive) to the
/// `office_oxide` parse format, or `None` when unsupported.
pub fn office_document_format(declared_mime: &str) -> Option<DocumentFormat> {
    let mime = declared_mime.split(';').next().unwrap_or_default().trim();
    const FORMATS: [DocumentFormat; 6] = [
        DocumentFormat::Docx,
        DocumentFormat::Xlsx,
        DocumentFormat::Pptx,
        DocumentFormat::Doc,
        DocumentFormat::Xls,
        DocumentFormat::Ppt,
    ];
    FORMATS
        .into_iter()
        .find(|format| mime.eq_ignore_ascii_case(format.mime_type()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerFamily {
    Ooxml,
    Legacy,
}

fn container_family(source: &[u8]) -> Option<ContainerFamily> {
    if source.starts_with(b"PK\x03\x04") {
        Some(ContainerFamily::Ooxml)
    } else if source.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        Some(ContainerFamily::Legacy)
    } else {
        None
    }
}

/// Validates that the declared format matches the magic container family and,
/// for OOXML, that the ZIP central directory stays within preflight bounds.
fn check_container(source: &[u8], format: DocumentFormat) -> Result<(), MediaPreprocessError> {
    let expected = if format.is_legacy() {
        ContainerFamily::Legacy
    } else {
        ContainerFamily::Ooxml
    };
    let actual = container_family(source);
    if actual != Some(expected) {
        // Encrypted OOXML payloads are wrapped in a CFB container; surface
        // them as invalid documents rather than type mismatches.
        return Err(
            if expected == ContainerFamily::Ooxml && actual == Some(ContainerFamily::Legacy) {
                MediaPreprocessError::DocumentInvalid
            } else {
                MediaPreprocessError::MimeMismatch
            },
        );
    }
    if expected == ContainerFamily::Ooxml {
        zip_preflight(source)?;
    }
    Ok(())
}

/// Reads only the ZIP central directory: entry count and summed *declared*
/// decompressed sizes. Rejects before `office_oxide` parsing when limits are
/// exceeded or the archive cannot declare its total.
fn zip_preflight(source: &[u8]) -> Result<(), MediaPreprocessError> {
    let archive = zip::ZipArchive::new(Cursor::new(source))
        .map_err(|_| MediaPreprocessError::DocumentInvalid)?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(MediaPreprocessError::DocumentInvalid);
    }
    let decompressed = archive
        .decompressed_size()
        .ok_or(MediaPreprocessError::DocumentInvalid)?;
    if decompressed > MAX_ZIP_DECOMPRESSED_BYTES {
        return Err(MediaPreprocessError::DocumentInvalid);
    }
    Ok(())
}

/// One embedded image recorded in the document manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestImage {
    /// ArtifactId of the stored image (normalized JPEG or raw unsupported
    /// bytes).
    pub artifact_id: ArtifactId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// 1-based position in document order.
    pub ordinal: u32,
    /// Whether the Artifact bytes are a normalized JPEG derivative the model
    /// can consume.
    pub normalizable: bool,
    /// Stored Artifact size in bytes.
    pub size: u64,
}

/// Persisted extraction manifest stored as the document's derivative Artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentManifest {
    pub version: u8,
    /// `office_oxide` serializes formats lowercase ("docx", "xlsx", ...).
    pub format: DocumentFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `sa:<id>` reference to the extracted-Markdown Artifact.
    pub markdown_artifact: String,
    #[serde(default)]
    pub images: Vec<ManifestImage>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub limitations: Vec<String>,
}

impl DocumentManifest {
    /// Parses a manifest with size/version/member validation.
    pub fn parse(bytes: &[u8]) -> Result<Self, MediaStoreError> {
        if bytes.is_empty() || bytes.len() > MAX_DOCUMENT_MANIFEST_BYTES {
            return Err(MediaStoreError::Corrupt);
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| MediaStoreError::Corrupt)?;
        if manifest.version != MANIFEST_VERSION
            || manifest.markdown_artifact_id().is_err()
            || manifest.images.len() > MAX_EMBEDDED_IMAGES
            || manifest
                .images
                .iter()
                .any(|image| !valid_digest_id(image.artifact_id.as_str()))
        {
            return Err(MediaStoreError::Corrupt);
        }
        Ok(manifest)
    }

    /// The Markdown snapshot ArtifactId (`sa:<id>` form, digest-validated).
    pub fn markdown_artifact_id(&self) -> Result<ArtifactId, MediaStoreError> {
        ArtifactId::from_reference(&self.markdown_artifact).map_err(|_| MediaStoreError::Corrupt)
    }

    /// Every ArtifactId the manifest references (Markdown + embedded images).
    pub fn referenced_artifact_ids(&self) -> Vec<ArtifactId> {
        let mut ids = Vec::with_capacity(self.images.len() + 1);
        if let Ok(markdown) = self.markdown_artifact_id() {
            ids.push(markdown);
        }
        ids.extend(self.images.iter().map(|image| image.artifact_id.clone()));
        ids
    }

    fn encode(&self) -> Result<Bytes, MediaStoreError> {
        let bytes = serde_json::to_vec(self).map_err(|_| MediaStoreError::Corrupt)?;
        if bytes.len() > MAX_DOCUMENT_MANIFEST_BYTES {
            return Err(MediaStoreError::Corrupt);
        }
        Ok(Bytes::from(bytes))
    }
}

/// Result of resolving (or creating) a document's extraction derivative.
pub struct DocumentDerivative {
    pub manifest_artifact: ArtifactRef,
    pub manifest: DocumentManifest,
}

/// Returns the cached manifest derivative for a document source, if a prior
/// extraction stored one.
pub async fn cached_document_derivative(
    store: &MediaDerivativeStore,
    principal: &Principal,
    source_id: &ArtifactId,
) -> Result<Option<DocumentDerivative>, MediaStoreError> {
    let Some(media) = store.find_derivative(principal, source_id).await? else {
        return Ok(None);
    };
    if media.derivative.mime_type != DOCUMENT_MANIFEST_MIME {
        return Ok(None);
    }
    let manifest = read_manifest(store, principal, &media.derivative).await?;
    Ok(Some(DocumentDerivative {
        manifest_artifact: media.derivative,
        manifest,
    }))
}

/// Parses a manifest derivative Artifact.
async fn read_manifest(
    store: &MediaDerivativeStore,
    principal: &Principal,
    artifact: &ArtifactRef,
) -> Result<DocumentManifest, MediaStoreError> {
    if artifact.mime_type != DOCUMENT_MANIFEST_MIME {
        return Err(MediaStoreError::Corrupt);
    }
    let (_, bytes) = store
        .read_artifact_bounded(principal, &artifact.id, MAX_DOCUMENT_MANIFEST_BYTES as u64)
        .await?;
    DocumentManifest::parse(&bytes)
}

/// Reads the extracted Markdown snapshot for a manifest.
pub async fn document_markdown(
    store: &MediaDerivativeStore,
    principal: &Principal,
    manifest: &DocumentManifest,
) -> Result<String, MediaStoreError> {
    let id = manifest.markdown_artifact_id()?;
    let artifact = store.inspect_artifact(principal, &id).await?;
    if artifact.mime_type != DOCUMENT_MARKDOWN_MIME {
        return Err(MediaStoreError::Corrupt);
    }
    if artifact.size == 0 {
        return Ok(String::new());
    }
    if artifact.size > MAX_DOCUMENT_MARKDOWN_BYTES as u64 {
        return Err(MediaStoreError::TooLarge);
    }
    let (_, bytes) = store
        .read_artifact_bounded(principal, &id, MAX_DOCUMENT_MARKDOWN_BYTES as u64)
        .await?;
    String::from_utf8(bytes.to_vec()).map_err(|_| MediaStoreError::Corrupt)
}

/// Full document preprocessing for Media Understanding: resolves/creates the
/// extraction derivative and materializes a `PreparedDocument`.
pub async fn prepare_document(
    store: &MediaDerivativeStore,
    principal: &Principal,
    source: &ArtifactRef,
    source_bytes: Option<Bytes>,
    staging_retention: Duration,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<PreparedDocument, MediaPreprocessError> {
    let derivative = document_derivative(
        store,
        principal,
        source,
        source_bytes,
        staging_retention,
        cancellation,
        deadline,
    )
    .await?;
    let markdown = document_markdown(store, principal, &derivative.manifest)
        .await
        .map_err(|error| match error {
            MediaStoreError::TooLarge => MediaPreprocessError::DerivativeTooLarge,
            other => MediaPreprocessError::from(other),
        })?;
    Ok(PreparedDocument {
        source: source.clone(),
        manifest_artifact: derivative.manifest_artifact,
        format: derivative.manifest.format,
        title: derivative.manifest.title.clone(),
        markdown,
        images: derivative
            .manifest
            .images
            .iter()
            .map(|image| PreparedDocumentImage {
                artifact_id: image.artifact_id.clone(),
                alt: image.alt.clone(),
                ordinal: image.ordinal,
                normalizable: image.normalizable,
                size: image.size,
            })
            .collect(),
        truncated: derivative.manifest.truncated,
        limitations: derivative.manifest.limitations.clone(),
    })
}

/// Cache-then-extract pipeline for a document source Artifact.
///
/// `source_bytes` may be supplied when the caller already holds them (public
/// URL reads); otherwise the source Artifact is read through the store with
/// the document byte ceiling.
pub async fn document_derivative(
    store: &MediaDerivativeStore,
    principal: &Principal,
    source: &ArtifactRef,
    source_bytes: Option<Bytes>,
    staging_retention: Duration,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<DocumentDerivative, MediaPreprocessError> {
    if let Some(existing) = cached_document_derivative(store, principal, &source.id)
        .await
        .map_err(MediaPreprocessError::from)?
    {
        return Ok(existing);
    }
    let format =
        office_document_format(&source.mime_type).ok_or(MediaPreprocessError::UnsupportedType)?;
    let source_bytes = match source_bytes {
        Some(bytes) => bytes,
        None => {
            store
                .read_artifact_bounded(principal, &source.id, MAX_DOCUMENT_BYTES)
                .await
                .map_err(|error| match error {
                    MediaStoreError::TooLarge => MediaPreprocessError::SourceTooLarge,
                    other => MediaPreprocessError::from(other),
                })?
                .1
        }
    };
    if source_bytes.is_empty() || source_bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(MediaPreprocessError::SourceTooLarge);
    }

    let extracted = blocking_media_task(cancellation, deadline, move |token, limit| {
        extract_document(&source_bytes, format, token, limit)
    })
    .await?;
    check_normalization_budget(cancellation, deadline)?;

    // Stored embedded images inherit the source Artifact's retention so links
    // do not dangle behind the source's lifetime.
    let retention = staging_retention.max(
        store
            .remaining_source_retention(principal, &source.id)
            .await
            .map_err(MediaPreprocessError::from)?,
    );

    let mut targets = Vec::with_capacity(extracted.images.len());
    let mut images = Vec::with_capacity(extracted.images.len());
    let mut stored_raw = 0usize;
    let mut omitted = 0usize;
    for (index, image) in extracted.images.iter().enumerate() {
        check_normalization_budget(cancellation, deadline)?;
        let ordinal = (index + 1) as u32;
        if images.len() >= MAX_EMBEDDED_IMAGES {
            omitted += 1;
            targets.push(MarkerTarget::Alt(image.alt.clone()));
            continue;
        }
        let normalized = if image.data.len() <= MAX_EMBEDDED_IMAGE_BYTES {
            let data = image.data.clone();
            let mime = image.mime.clone();
            match blocking_media_task(cancellation, deadline, move |token, limit| {
                normalize_image_until(&data, &mime, token, limit)
            })
            .await
            {
                Ok(image) => Some(image),
                Err(error) => {
                    tracing::debug!(%error, "embedded image normalization failed; storing raw bytes");
                    None
                }
            }
        } else {
            None
        };
        let (bytes, mime, normalizable) = match normalized {
            Some(image) => (image.bytes, "image/jpeg".to_owned(), true),
            None => {
                stored_raw += 1;
                (image.data.clone(), image.mime.clone(), false)
            }
        };
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            omitted += 1;
            targets.push(MarkerTarget::Alt(image.alt.clone()));
            continue;
        }
        let artifact = store
            .create_source(principal, &mime, bytes, retention)
            .await
            .map_err(MediaPreprocessError::from)?;
        targets.push(MarkerTarget::Artifact(
            artifact.id.clone(),
            image.alt.clone(),
        ));
        images.push(ManifestImage {
            artifact_id: artifact.id,
            alt: image.alt.clone(),
            ordinal,
            normalizable,
            size: artifact.size,
        });
    }

    let mut limitations = extracted.limitations;
    if stored_raw > 0 {
        limitations.push(format!(
            "{stored_raw} embedded images could not be normalized to JPEG and were stored in their original format"
        ));
    }
    if omitted > 0 {
        limitations.push(format!(
            "{omitted} embedded images exceeded the per-document image limit and were not stored"
        ));
    }
    // Marker substitution scans a template embedding base64 of every image —
    // keep that CPU work on the blocking pool alongside the extraction.
    let markdown_template = extracted.markdown_template;
    let raw_images = extracted.images;
    let (markdown, truncated) = blocking_media_task(cancellation, deadline, move |token, limit| {
        check_normalization_budget(token, limit)?;
        let mut links = HashMap::with_capacity(raw_images.len());
        for (image, target) in raw_images.iter().zip(targets) {
            links.insert(
                base64::engine::general_purpose::STANDARD.encode(&image.data),
                target,
            );
        }
        Ok(truncate_utf8(
            &substitute_image_markers(&markdown_template, &links),
            MAX_DOCUMENT_MARKDOWN_BYTES,
        ))
    })
    .await?;
    if truncated {
        limitations.push(
            "Extracted Markdown exceeded the snapshot size limit and was truncated".to_owned(),
        );
    }

    let markdown_artifact = store
        .create_source(
            principal,
            DOCUMENT_MARKDOWN_MIME,
            Bytes::from(markdown),
            retention,
        )
        .await
        .map_err(MediaPreprocessError::from)?;
    let manifest = DocumentManifest {
        version: MANIFEST_VERSION,
        format,
        title: extracted.title,
        markdown_artifact: markdown_artifact.reference(),
        images,
        truncated,
        limitations,
    };
    let manifest_bytes = manifest.encode().map_err(MediaPreprocessError::from)?;
    let created = store
        .get_or_create_derivative(
            principal,
            &source.id,
            manifest_bytes,
            staging_retention,
            DOCUMENT_MANIFEST_MIME,
        )
        .await
        .map_err(MediaPreprocessError::from)?;
    // Read back through the verified path so a concurrent winner's manifest is
    // validated rather than trusted blindly.
    let manifest = read_manifest(store, principal, &created.derivative).await?;
    Ok(DocumentDerivative {
        manifest_artifact: created.derivative,
        manifest,
    })
}

#[derive(Debug)]
struct ExtractedImage {
    alt: Option<String>,
    mime: String,
    data: Bytes,
}

#[derive(Debug)]
struct ExtractedDocument {
    title: Option<String>,
    /// Markdown template containing `[image-base64:<data>]` markers.
    markdown_template: String,
    /// Unique embedded images in document order.
    images: Vec<ExtractedImage>,
    limitations: Vec<String>,
}

/// Parse + IR + Markdown render inside the blocking pool.
fn extract_document(
    source: &[u8],
    format: DocumentFormat,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<ExtractedDocument, MediaPreprocessError> {
    check_container(source, format)?;
    check_normalization_budget(cancellation, deadline)?;
    let document = Document::from_reader(Cursor::new(source.to_vec()), format)
        .map_err(|_| MediaPreprocessError::DocumentInvalid)?;
    check_normalization_budget(cancellation, deadline)?;
    let ir = document.to_ir();
    check_normalization_budget(cancellation, deadline)?;
    let markdown_template = ir.to_markdown_with(MarkdownOptions {
        image_embed: ImageEmbed::Base64,
    });

    let mut collected: Vec<&office_oxide::ir::Image> = Vec::new();
    for section in &ir.sections {
        collect_section_images(section, &mut collected);
    }
    let (images, missing_data) = unique_images(collected);
    let mut limitations = Vec::new();
    if missing_data > 0 {
        limitations.push(format!(
            "{missing_data} embedded images had no extractable bytes and were skipped"
        ));
    }
    Ok(ExtractedDocument {
        title: document_title(&ir),
        markdown_template,
        images,
        limitations,
    })
}

/// Deduplicate collected IR images by byte content, preserving document order.
/// Returns the unique images and the count skipped for missing data.
fn unique_images(collected: Vec<&office_oxide::ir::Image>) -> (Vec<ExtractedImage>, usize) {
    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut images = Vec::new();
    let mut missing_data = 0usize;
    for image in collected {
        let Some(data) = image.data.as_deref() else {
            missing_data += 1;
            continue;
        };
        if !seen.insert(data) {
            continue;
        }
        images.push(ExtractedImage {
            alt: image.alt_text.as_deref().map(bound_alt),
            mime: image
                .format
                .as_ref()
                .map(|format| format.content_type().to_owned())
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
            data: Bytes::copy_from_slice(data),
        });
    }
    (images, missing_data)
}

fn collect_section_images<'a>(section: &'a Section, images: &mut Vec<&'a office_oxide::ir::Image>) {
    for header_footer in [
        section.header.as_ref(),
        section.first_page_header.as_ref(),
        section.even_page_header.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        collect_element_images(&header_footer.content, images);
    }
    collect_element_images(&section.elements, images);
    for header_footer in [
        section.footer.as_ref(),
        section.first_page_footer.as_ref(),
        section.even_page_footer.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        collect_element_images(&header_footer.content, images);
    }
}

fn collect_element_images<'a>(
    elements: &'a [Element],
    images: &mut Vec<&'a office_oxide::ir::Image>,
) {
    for element in elements {
        match element {
            Element::Image(image) => images.push(image),
            Element::Table(table) => {
                for row in &table.rows {
                    for cell in &row.cells {
                        collect_element_images(&cell.content, images);
                    }
                }
            }
            Element::List(list) => collect_list_images(list, images),
            Element::TextBox(text_box) => collect_element_images(&text_box.content, images),
            Element::Footnote(note) | Element::Endnote(note) => {
                collect_element_images(&note.content, images)
            }
            _ => {}
        }
    }
}

fn collect_list_images<'a>(list: &'a List, images: &mut Vec<&'a office_oxide::ir::Image>) {
    for item in &list.items {
        collect_element_images(&item.content, images);
        if let Some(nested) = &item.nested {
            collect_list_images(nested, images);
        }
    }
}

fn document_title(ir: &DocumentIR) -> Option<String> {
    ir.metadata
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(|title| title.chars().take(256).collect())
}

fn bound_alt(alt: &str) -> String {
    let trimmed = alt.trim();
    if trimmed.len() <= MAX_ALT_BYTES {
        trimmed.to_owned()
    } else {
        trimmed
            .char_indices()
            .take_while(|(index, _)| *index < MAX_ALT_BYTES)
            .map(|(_, c)| c)
            .collect()
    }
}

enum MarkerTarget {
    /// Stored Artifact link `![alt](sa:<id>)`.
    Artifact(ArtifactId, Option<String>),
    /// Image deliberately not stored (limit hit): degrade to italic alt text,
    /// matching `office_oxide`'s non-embed rendering.
    Alt(Option<String>),
}

/// Replaces `[image-base64:<data>]` markers with `![alt](sa:<id>)` links.
/// Markers not present in `links` are left untouched.
fn substitute_image_markers(template: &str, links: &HashMap<String, MarkerTarget>) -> String {
    let mut output = String::with_capacity(template.len().min(MAX_DOCUMENT_MARKDOWN_BYTES * 2));
    let mut rest = template;
    while let Some(start) = rest.find(IMAGE_MARKER_PREFIX) {
        output.push_str(&rest[..start]);
        let body = &rest[start + IMAGE_MARKER_PREFIX.len()..];
        let Some(end) = body.find(IMAGE_MARKER_SUFFIX) else {
            output.push_str(&rest[start..]);
            return output;
        };
        let key = &body[..end];
        match links.get(key) {
            Some(MarkerTarget::Artifact(id, alt)) => {
                output.push_str("![");
                if let Some(alt) = alt {
                    output.push_str(&escape_alt(alt));
                }
                output.push_str("](sa:");
                output.push_str(id.as_str());
                output.push(')');
            }
            Some(MarkerTarget::Alt(alt)) => {
                if let Some(alt) = alt {
                    output.push('*');
                    output.push_str(&escape_alt(alt));
                    output.push('*');
                }
            }
            None => output.push_str(&rest[start..start + IMAGE_MARKER_PREFIX.len() + end + 1]),
        }
        rest = &body[end + 1..];
    }
    output.push_str(rest);
    output
}

/// Escapes `]`/`[`/`\` and flattens newlines so untrusted alt text cannot
/// break out of the Markdown image link.
fn escape_alt(alt: &str) -> String {
    alt.chars()
        .map(|c| match c {
            '[' => "\\[".to_owned(),
            ']' => "\\]".to_owned(),
            '\\' => "\\\\".to_owned(),
            '\n' | '\r' => " ".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

/// Byte-bounded UTF-8 truncation on char boundaries.
pub(crate) fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use office_oxide::ir::{
        Element, Image as IrImage, ImageFormat, InlineContent, List as IrList, ListItem, Paragraph,
        Section, Table, TableCell, TableRow, TextSpan,
    };

    fn ooxml_bytes(elements: Vec<Element>, format: DocumentFormat) -> Vec<u8> {
        let ir = DocumentIR {
            metadata: office_oxide::ir::Metadata {
                format,
                ..Default::default()
            },
            sections: vec![Section {
                elements,
                ..Default::default()
            }],
        };
        let mut out = Cursor::new(Vec::new());
        office_oxide::create::create_from_ir_to_writer(&ir, format, &mut out)
            .expect("synthesize document");
        out.into_inner()
    }

    fn docx_bytes(elements: Vec<Element>) -> Vec<u8> {
        ooxml_bytes(elements, DocumentFormat::Docx)
    }

    /// Writes real CFB containers carrying the given streams.
    fn cfb_bytes(streams: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("cfb create");
        for (name, data) in streams {
            file.create_stream(name)
                .and_then(|mut stream| stream.write_all(data))
                .expect("cfb stream");
        }
        file.flush().expect("cfb flush");
        file.into_inner().into_inner()
    }

    fn biff_record(record_type: u16, data: &[u8]) -> Vec<u8> {
        let mut record = record_type.to_le_bytes().to_vec();
        record.extend_from_slice(&(data.len() as u16).to_le_bytes());
        record.extend_from_slice(data);
        record
    }

    /// Minimal BIFF8 workbook: a globals BOF and one worksheet holding a
    /// single NUMBER cell — a real CFB file the legacy XLS parser accepts.
    fn xls_bytes() -> Vec<u8> {
        const RT_BOF: u16 = 0x0809;
        const RT_EOF: u16 = 0x000A;
        const RT_NUMBER: u16 = 0x0203;
        let mut workbook = biff_record(RT_BOF, &[0x00, 0x06, 0x05, 0x00]);
        workbook.extend(biff_record(RT_EOF, &[]));
        workbook.extend(biff_record(RT_BOF, &[0x00, 0x06, 0x10, 0x00]));
        let mut cell = Vec::new();
        cell.extend_from_slice(&0u16.to_le_bytes()); // row
        cell.extend_from_slice(&0u16.to_le_bytes()); // col
        cell.extend_from_slice(&0u16.to_le_bytes()); // xf index
        cell.extend_from_slice(&42.5f64.to_le_bytes());
        workbook.extend(biff_record(RT_NUMBER, &cell));
        workbook.extend(biff_record(RT_EOF, &[]));
        cfb_bytes(&[("Workbook", &workbook)])
    }

    /// Minimal Word 97 .doc: a FIB pointing at a one-piece CLX in `0Table`,
    /// with UTF-16LE text at a fixed offset in the WordDocument stream.
    fn doc_bytes(text: &str) -> Vec<u8> {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let text_offset = 0x200usize;
        let mut word = vec![0u8; text_offset + utf16.len()];
        word[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes()); // wIdent: Word 97+
        word[2..4].copy_from_slice(&0x00C1u16.to_le_bytes()); // nFib
        // Flags at 0x0A stay zero: the 0Table stream is selected.
        let chars = text.encode_utf16().count() as u32;
        word[0x4C..0x50].copy_from_slice(&chars.to_le_bytes()); // ccpText
        word[0x01A2..0x01A6].copy_from_slice(&0u32.to_le_bytes()); // fcClx
        word[0x01A6..0x01AA].copy_from_slice(&21u32.to_le_bytes()); // lcbClx
        word[text_offset..].copy_from_slice(&utf16);
        let mut clx = vec![0x02u8]; // Pcdt marker
        clx.extend_from_slice(&16u32.to_le_bytes()); // PlcPcd: 2 CPs + 1 PCD
        clx.extend_from_slice(&0u32.to_le_bytes()); // cp start
        clx.extend_from_slice(&chars.to_le_bytes()); // cp end
        clx.extend_from_slice(&0u16.to_le_bytes()); // PCD unused
        clx.extend_from_slice(&(text_offset as u32).to_le_bytes()); // fc, unicode
        clx.extend_from_slice(&0u16.to_le_bytes()); // prm
        cfb_bytes(&[("WordDocument", &word), ("0Table", &clx)])
    }

    fn ppt_record(version_instance: u16, record_type: u16, data: &[u8]) -> Vec<u8> {
        let mut record = version_instance.to_le_bytes().to_vec();
        record.extend_from_slice(&record_type.to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(data);
        record
    }

    /// Minimal .ppt: one Slide container holding a body TextCharsAtom.
    fn ppt_bytes(text: &str) -> Vec<u8> {
        const RT_TEXT_HEADER: u16 = 0x0F9F;
        const RT_TEXT_CHARS: u16 = 0x0FA0;
        const RT_SLIDE: u16 = 0x03EE;
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut slide = ppt_record(0x0000, RT_TEXT_HEADER, &1u32.to_le_bytes());
        slide.extend(ppt_record(0x0000, RT_TEXT_CHARS, &utf16));
        let stream = ppt_record(0x000F, RT_SLIDE, &slide);
        cfb_bytes(&[("PowerPoint Document", &stream)])
    }

    fn paragraph(text: &str) -> Element {
        Element::Paragraph(Paragraph {
            content: vec![InlineContent::Text(TextSpan {
                text: text.to_owned(),
                ..Default::default()
            })],
            ..Default::default()
        })
    }

    // ---- MIME allowlist -------------------------------------------------

    #[test]
    fn office_mime_allowlist() {
        for (mime, expected) in [
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                DocumentFormat::Docx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                DocumentFormat::Xlsx,
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                DocumentFormat::Pptx,
            ),
            ("application/msword", DocumentFormat::Doc),
            ("application/vnd.ms-excel", DocumentFormat::Xls),
            ("application/vnd.ms-powerpoint", DocumentFormat::Ppt),
        ] {
            assert_eq!(office_document_format(mime), Some(expected), "{mime}");
            assert!(is_office_document(mime));
        }
        // Parameters and case tolerated.
        assert_eq!(
            office_document_format("Application/MSWord; charset=binary"),
            Some(DocumentFormat::Doc)
        );
        for mime in [
            "application/pdf",
            "application/vnd.oasis.opendocument.text",
            "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
            "application/zip",
            "image/png",
            "",
        ] {
            assert_eq!(office_document_format(mime), None, "{mime}");
        }
    }

    // ---- Magic container family ------------------------------------------

    #[test]
    fn container_family_magic() {
        assert_eq!(
            container_family(b"PK\x03\x04rest"),
            Some(ContainerFamily::Ooxml)
        );
        assert_eq!(
            container_family(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0x00]),
            Some(ContainerFamily::Legacy)
        );
        assert_eq!(container_family(b"not an office file"), None);
        assert_eq!(container_family(b"PK"), None);
        assert_eq!(container_family(b""), None);
    }

    #[test]
    fn container_mismatch_rejected() {
        let zip_like = b"PK\x03\x04fake-zip-body";
        // Declared legacy but bytes are ZIP.
        assert_eq!(
            check_container(zip_like, DocumentFormat::Doc),
            Err(MediaPreprocessError::MimeMismatch)
        );
        // Declared OOXML but bytes are CFB: encrypted OOXML ships in a CFB
        // wrapper, so this reads as an invalid document, not a mismatch.
        let cfb = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        assert_eq!(
            check_container(&cfb, DocumentFormat::Docx),
            Err(MediaPreprocessError::DocumentInvalid)
        );
        // Unknown magic.
        assert_eq!(
            check_container(b"random", DocumentFormat::Pptx),
            Err(MediaPreprocessError::MimeMismatch)
        );
    }

    // ---- ZIP preflight ----------------------------------------------------

    #[test]
    fn zip_preflight_accepts_real_docx() {
        let bytes = docx_bytes(vec![paragraph("hello")]);
        zip_preflight(&bytes).expect("real docx preflights cleanly");
        check_container(&bytes, DocumentFormat::Docx).expect("family matches");
    }

    #[test]
    fn zip_preflight_rejects_garbage() {
        // Starts with PK\x03\x04 so family detection passes, but is not a ZIP.
        let mut fake = b"PK\x03\x04".to_vec();
        fake.extend_from_slice(&[0u8; 64]);
        assert_eq!(
            zip_preflight(&fake),
            Err(MediaPreprocessError::DocumentInvalid)
        );
    }

    #[test]
    fn zip_preflight_rejects_entry_count() {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            for index in 0..MAX_ZIP_ENTRIES + 1 {
                writer
                    .start_file(format!("f{index}"), options)
                    .expect("start file");
            }
            writer.finish().expect("finish zip");
        }
        let bytes = cursor.into_inner();
        assert_eq!(
            zip_preflight(&bytes),
            Err(MediaPreprocessError::DocumentInvalid)
        );
    }

    /// Builds a minimal stored-ZIP whose central directory declares
    /// `declared_uncompressed` bytes for a one-byte entry.
    fn zip_with_declared_size(declared_uncompressed: u32) -> Vec<u8> {
        let name = b"a";
        let mut zip = Vec::new();
        // Local file header.
        zip.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        zip.extend_from_slice(&[20, 0]); // version needed
        zip.extend_from_slice(&[0, 0]); // flags
        zip.extend_from_slice(&[0, 0]); // method: stored
        zip.extend_from_slice(&[0, 0, 0, 0]); // time + date
        zip.extend_from_slice(&[0, 0, 0, 0]); // crc
        zip.extend_from_slice(&1u32.to_le_bytes()); // compressed size
        zip.extend_from_slice(&1u32.to_le_bytes()); // uncompressed size (local)
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // extra len
        zip.extend_from_slice(name);
        zip.push(b'x');
        let cd_offset = zip.len() as u32;
        // Central directory entry — lies about the uncompressed size.
        zip.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        zip.extend_from_slice(&[20, 0, 20, 0]); // versions
        zip.extend_from_slice(&[0, 0, 0, 0]); // flags + method
        zip.extend_from_slice(&[0, 0, 0, 0]); // time + date
        zip.extend_from_slice(&[0, 0, 0, 0]); // crc
        zip.extend_from_slice(&1u32.to_le_bytes()); // compressed size
        zip.extend_from_slice(&declared_uncompressed.to_le_bytes());
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // extra/comment/disk/int-attr
        zip.extend_from_slice(&0u32.to_le_bytes()); // ext attr
        zip.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        zip.extend_from_slice(name);
        let cd_size = zip.len() as u32 - cd_offset;
        // End of central directory.
        zip.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        zip.extend_from_slice(&[0, 0, 0, 0]); // disk numbers
        zip.extend_from_slice(&1u16.to_le_bytes()); // entries this disk
        zip.extend_from_slice(&1u16.to_le_bytes()); // total entries
        zip.extend_from_slice(&cd_size.to_le_bytes());
        zip.extend_from_slice(&cd_offset.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // comment len
        zip
    }

    #[test]
    fn zip_preflight_rejects_declared_decompressed_total() {
        // Central directory declares 1 GiB for a one-byte stored entry; the
        // declared total exceeds the preflight cap before any decompression.
        let zip = zip_with_declared_size(0x4000_0000);
        assert_eq!(
            zip_preflight(&zip),
            Err(MediaPreprocessError::DocumentInvalid)
        );
        // A truthful small archive passes.
        let zip = zip_with_declared_size(1);
        assert_eq!(zip_preflight(&zip), Ok(()));
    }

    // ---- Manifest serde ----------------------------------------------------

    /// ArtifactIds are content-addressed digests: 55 lowercase letters.
    fn test_id(letter: char) -> String {
        letter.to_string().repeat(55)
    }

    #[test]
    fn manifest_round_trip() {
        let manifest = DocumentManifest {
            version: MANIFEST_VERSION,
            format: DocumentFormat::Docx,
            title: Some("Quarterly report".to_owned()),
            markdown_artifact: format!("sa:{}", test_id('m')),
            images: vec![ManifestImage {
                artifact_id: ArtifactId::new(test_id('i')),
                alt: Some("chart".to_owned()),
                ordinal: 1,
                normalizable: true,
                size: 1234,
            }],
            truncated: false,
            limitations: vec!["1 embedded image had no extractable bytes".to_owned()],
        };
        let bytes = manifest.encode().expect("encode");
        let parsed = DocumentManifest::parse(&bytes).expect("parse");
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.format, DocumentFormat::Docx);
        assert_eq!(
            parsed.markdown_artifact_id().unwrap().as_str(),
            test_id('m')
        );
        assert_eq!(parsed.referenced_artifact_ids().len(), 2);
        // format serializes lowercase per spec.
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["format"], "docx");
    }

    #[test]
    fn manifest_parse_rejects_bad_inputs() {
        assert_eq!(DocumentManifest::parse(b""), Err(MediaStoreError::Corrupt));
        assert_eq!(
            DocumentManifest::parse(b"not json"),
            Err(MediaStoreError::Corrupt)
        );
        // Wrong version.
        let mut manifest = DocumentManifest {
            version: 99,
            format: DocumentFormat::Xlsx,
            title: None,
            markdown_artifact: "sa:m".to_owned(),
            images: vec![],
            truncated: false,
            limitations: vec![],
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        assert_eq!(
            DocumentManifest::parse(&bytes),
            Err(MediaStoreError::Corrupt)
        );
        // Unknown field rejected.
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest.encode().unwrap()).unwrap();
        manifest.version = MANIFEST_VERSION;
        value["extra"] = serde_json::json!(true);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            DocumentManifest::parse(&bytes),
            Err(MediaStoreError::Corrupt)
        );
        // Malformed sa: reference rejected.
        let bad = DocumentManifest {
            markdown_artifact: "sa:".to_owned(),
            ..manifest.clone()
        };
        let bytes = serde_json::to_vec(&bad).unwrap();
        assert_eq!(
            DocumentManifest::parse(&bytes),
            Err(MediaStoreError::Corrupt)
        );
    }

    // ---- Marker substitution -------------------------------------------------

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn marker_substitution_replaces_with_artifact_link() {
        let img = b"\x89PNG fake";
        let template = format!("intro\n\n[image-base64:{}]\n\noutro", b64(img));
        let id = ArtifactId::new("art-9".to_owned());
        let mut links = HashMap::new();
        links.insert(
            b64(img),
            MarkerTarget::Artifact(id, Some("pie chart".into())),
        );
        let out = substitute_image_markers(&template, &links);
        assert_eq!(out, "intro\n\n![pie chart](sa:art-9)\n\noutro");
    }

    #[test]
    fn marker_substitution_empty_alt_and_escaping() {
        let img = b"x";
        let template = format!("[image-base64:{}]", b64(img));
        let mut links = HashMap::new();
        links.insert(
            b64(img),
            MarkerTarget::Artifact(
                ArtifactId::new("a".to_owned()),
                Some("weird ] [ \\ alt\nline".into()),
            ),
        );
        let out = substitute_image_markers(&template, &links);
        assert_eq!(out, "![weird \\] \\[ \\\\ alt line](sa:a)");

        // Empty alt stays empty.
        links.insert(b64(img), MarkerTarget::Artifact(ArtifactId::new("b"), None));
        assert_eq!(substitute_image_markers(&template, &links), "![](sa:b)");
    }

    #[test]
    fn marker_substitution_unknown_and_unterminated_passthrough() {
        let template = "[image-base64:AAAA] tail [image-base64:unterminated";
        let links = HashMap::new();
        assert_eq!(substitute_image_markers(template, &links), template);
    }

    #[test]
    fn marker_substitution_alt_only_for_omitted() {
        let img = b"pic";
        let template = format!("[image-base64:{}]", b64(img));
        let mut links = HashMap::new();
        links.insert(b64(img), MarkerTarget::Alt(Some("diagram".into())));
        assert_eq!(substitute_image_markers(&template, &links), "*diagram*");
        links.insert(b64(img), MarkerTarget::Alt(None));
        assert_eq!(substitute_image_markers(&template, &links), "");
    }

    // ---- Extraction via synthesized docx -------------------------------------

    #[test]
    fn extract_document_produces_markers_and_images_in_order() {
        let img_a = b"\x89PNG\r\n\x1a\n fake-a".to_vec();
        let img_b = b"\xff\xd8\xff fake-jpeg-b".to_vec();
        // office_oxide's DOCX writer only embeds top-level images — nested
        // placements (table cells, text boxes) and `data: None` entries are
        // dropped on write, so nested/missing-data coverage lives in the
        // pure `unique_images`/`collect_element_images` tests below.
        let bytes = docx_bytes(vec![
            paragraph("before"),
            Element::Image(IrImage {
                data: Some(img_a.clone()),
                format: Some(ImageFormat::Png),
                alt_text: Some("first".to_owned()),
                ..Default::default()
            }),
            Element::Image(IrImage {
                data: Some(img_b.clone()),
                format: Some(ImageFormat::Jpeg),
                alt_text: None,
                ..Default::default()
            }),
            // Duplicate of img_a — must dedupe.
            Element::Image(IrImage {
                data: Some(img_a.clone()),
                format: Some(ImageFormat::Png),
                alt_text: Some("first".to_owned()),
                ..Default::default()
            }),
        ]);
        let extracted = extract_document(
            &bytes,
            DocumentFormat::Docx,
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(60),
        )
        .expect("extract");
        assert_eq!(extracted.images.len(), 2, "dedupe by data bytes");
        assert_eq!(extracted.images[0].data.as_ref(), img_a.as_slice());
        assert_eq!(extracted.images[1].data.as_ref(), img_b.as_slice());
        assert_eq!(extracted.images[0].alt.as_deref(), Some("first"));
        assert_eq!(extracted.images[0].mime, "image/png");
        assert_eq!(extracted.images[1].mime, "image/jpeg");
        assert!(
            extracted
                .markdown_template
                .contains(&format!("[image-base64:{}]", b64(&img_a))),
            "template contains markers: {}",
            extracted.markdown_template
        );
        assert!(
            extracted
                .markdown_template
                .contains(&format!("[image-base64:{}]", b64(&img_b)))
        );
    }

    #[test]
    fn unique_images_dedupes_orders_and_counts_missing() {
        let img_a = IrImage {
            data: Some(vec![1, 2, 3]),
            format: Some(ImageFormat::Png),
            alt_text: Some("a".into()),
            ..Default::default()
        };
        let img_b = IrImage {
            data: Some(vec![4, 5, 6]),
            format: None,
            ..Default::default()
        };
        // An image nested in a table cell must still be collected.
        let elements = vec![Element::Table(Table {
            rows: vec![TableRow {
                cells: vec![TableCell {
                    content: vec![Element::Image(img_a.clone())],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })];
        let ghost = IrImage {
            data: None,
            ..Default::default()
        };
        let mut collected = vec![&img_a];
        collect_element_images(&elements, &mut collected);
        collected.push(&img_b);
        collected.push(&ghost);
        let (images, missing) = unique_images(collected);
        assert_eq!(missing, 1);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].data.as_ref(), &[1, 2, 3]);
        assert_eq!(images[0].alt.as_deref(), Some("a"));
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[1].data.as_ref(), &[4, 5, 6]);
        assert_eq!(images[1].mime, "application/octet-stream");
    }

    /// All six supported formats extract from real container bytes: OOXML
    /// files synthesized by office_oxide's writer, CFB files assembled by the
    /// `cfb` crate with minimal legacy payloads.
    #[test]
    fn extract_document_handles_all_six_real_formats() {
        let token = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        for (bytes, format, needle) in [
            (
                docx_bytes(vec![paragraph("docx body")]),
                DocumentFormat::Docx,
                "docx body",
            ),
            (
                ooxml_bytes(vec![paragraph("xlsx body")], DocumentFormat::Xlsx),
                DocumentFormat::Xlsx,
                "xlsx body",
            ),
            (
                ooxml_bytes(vec![paragraph("pptx body")], DocumentFormat::Pptx),
                DocumentFormat::Pptx,
                "pptx body",
            ),
            (
                doc_bytes("legacy doc body"),
                DocumentFormat::Doc,
                "legacy doc body",
            ),
            (xls_bytes(), DocumentFormat::Xls, "42"),
            (
                ppt_bytes("legacy ppt body"),
                DocumentFormat::Ppt,
                "legacy ppt body",
            ),
        ] {
            let extracted = extract_document(&bytes, format, &token, deadline)
                .unwrap_or_else(|error| panic!("{format:?} extraction failed: {error}"));
            assert!(
                extracted.markdown_template.contains(needle),
                "{format:?} markdown lacks {needle:?}: {:?}",
                extracted.markdown_template
            );
        }
    }

    #[test]
    fn extract_document_rejects_garbage_and_mismatch() {
        let token = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        // Valid zip family but corrupt docx.
        let mut fake = b"PK\x03\x04".to_vec();
        fake.extend_from_slice(&[0u8; 128]);
        assert_eq!(
            extract_document(&fake, DocumentFormat::Docx, &token, deadline).unwrap_err(),
            MediaPreprocessError::DocumentInvalid
        );
        // MIME/container mismatch surfaces before parse.
        assert_eq!(
            extract_document(b"plain text", DocumentFormat::Xlsx, &token, deadline).unwrap_err(),
            MediaPreprocessError::MimeMismatch
        );
    }

    #[test]
    fn collect_covers_nested_lists() {
        let img = IrImage {
            data: Some(vec![1, 2, 3]),
            format: Some(ImageFormat::Png),
            ..Default::default()
        };
        let elements = vec![Element::List(IrList {
            items: vec![ListItem {
                content: vec![],
                nested: Some(IrList {
                    items: vec![ListItem {
                        content: vec![Element::Image(img)],
                        nested: None,
                    }],
                    ..Default::default()
                }),
            }],
            ..Default::default()
        })];
        let mut collected = Vec::new();
        collect_element_images(&elements, &mut collected);
        assert_eq!(collected.len(), 1);
    }
}
