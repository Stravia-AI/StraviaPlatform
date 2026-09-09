use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, RgbImage,
};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::artifact::{ArtifactId, ArtifactRef};

use super::store::{MediaDerivativeStore, MediaStoreError};

pub const MAX_MEDIA_ARTIFACTS: usize = 8;
pub const MAX_SOURCE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_TURN_SOURCE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_DERIVATIVE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_TURN_DERIVATIVE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_IMAGE_EDGE: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 25_000_000;
pub const MAX_DERIVATIVE_EDGE: u32 = 3072;
pub const JPEG_QUALITY: u8 = 85;

#[derive(Debug, Clone)]
pub struct NormalizedImage {
    pub bytes: Bytes,
    #[cfg(any(test, feature = "test-support"))]
    pub width: u32,
    #[cfg(any(test, feature = "test-support"))]
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct PreparedMedia {
    pub source: ArtifactRef,
    pub derivative: ArtifactRef,
    #[cfg(any(test, feature = "test-support"))]
    pub derivative_bytes: Bytes,
}
#[derive(Clone)]
pub struct MediaInputPreprocessor {
    store: Arc<MediaDerivativeStore>,
    derivative_retention: Duration,
}

impl MediaInputPreprocessor {
    pub fn new(store: Arc<MediaDerivativeStore>, derivative_retention: Duration) -> Self {
        Self {
            store,
            derivative_retention,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn preprocess(
        &self,
        principal: &Principal,
        source_ids: &[ArtifactId],
    ) -> Result<Vec<PreparedMedia>, MediaPreprocessError> {
        self.preprocess_until(
            principal,
            source_ids,
            &stravia_runtime_contract::CancellationToken::new(),
            Instant::now() + Duration::from_secs(24 * 60 * 60),
        )
        .await
    }

    pub async fn preprocess_until(
        &self,
        principal: &Principal,
        source_ids: &[ArtifactId],
        cancellation: &stravia_runtime_contract::CancellationToken,
        deadline: Instant,
    ) -> Result<Vec<PreparedMedia>, MediaPreprocessError> {
        check_normalization_budget(cancellation, deadline)?;
        if source_ids.len() > MAX_MEDIA_ARTIFACTS {
            return Err(MediaPreprocessError::TooManyArtifacts);
        }
        let mut seen = HashSet::with_capacity(source_ids.len());
        let mut declared_total = 0_u64;
        for source_id in source_ids {
            check_normalization_budget(cancellation, deadline)?;
            if !seen.insert(source_id.clone()) {
                return Err(MediaPreprocessError::DuplicateArtifact);
            }
            let derivative_source = self
                .store
                .source_for_derivative(principal, source_id)
                .await
                .map_err(MediaPreprocessError::from)?;
            check_normalization_budget(cancellation, deadline)?;
            if derivative_source.is_some() {
                return Err(MediaPreprocessError::Unavailable);
            }
            let source = self
                .store
                .inspect_artifact(principal, source_id)
                .await
                .map_err(MediaPreprocessError::from)?;
            check_normalization_budget(cancellation, deadline)?;
            if source.size == 0 || source.size > MAX_SOURCE_BYTES as u64 {
                return Err(MediaPreprocessError::SourceTooLarge);
            }
            declared_total = declared_total
                .checked_add(source.size)
                .ok_or(MediaPreprocessError::SourceAggregateTooLarge)?;
            if declared_total > MAX_TURN_SOURCE_BYTES as u64 {
                return Err(MediaPreprocessError::SourceAggregateTooLarge);
            }
        }
        let mut source_total = 0_usize;
        let mut sources = Vec::with_capacity(source_ids.len());
        for source_id in source_ids {
            check_normalization_budget(cancellation, deadline)?;
            let (source, bytes) = self
                .store
                .read_artifact_bounded(principal, source_id, MAX_SOURCE_BYTES as u64)
                .await
                .map_err(|error| match error {
                    MediaStoreError::TooLarge => MediaPreprocessError::SourceTooLarge,
                    other => MediaPreprocessError::from(other),
                })?;
            check_normalization_budget(cancellation, deadline)?;
            if bytes.is_empty() || bytes.len() > MAX_SOURCE_BYTES {
                return Err(MediaPreprocessError::SourceTooLarge);
            }
            source_total = source_total
                .checked_add(bytes.len())
                .ok_or(MediaPreprocessError::SourceAggregateTooLarge)?;
            if source_total > MAX_TURN_SOURCE_BYTES {
                return Err(MediaPreprocessError::SourceAggregateTooLarge);
            }
            sources.push((source, bytes));
        }

        let mut derivative_total = 0_usize;
        let mut prepared = Vec::with_capacity(sources.len());
        for (source, source_bytes) in sources {
            check_normalization_budget(cancellation, deadline)?;
            let existing = self
                .store
                .find_derivative(principal, &source.id)
                .await
                .map_err(MediaPreprocessError::from)?;
            check_normalization_budget(cancellation, deadline)?;
            let media = match existing {
                Some(media) => media,
                None => {
                    let bytes = source_bytes.clone();
                    let mime_type = source.mime_type.clone();
                    let worker_cancellation = cancellation.clone();
                    let mut normalization = tokio::task::spawn_blocking(move || {
                        normalize_image_until(&bytes, &mime_type, &worker_cancellation, deadline)
                    });
                    let normalized = tokio::select! {
                        biased;
                        result = &mut normalization => {
                            result.map_err(|_| MediaPreprocessError::Decode)??
                        }
                        _ = cancellation.cancelled() => {
                            let _ = normalization.await;
                            return Err(MediaPreprocessError::Cancelled);
                        }
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                            cancellation.cancel();
                            let _ = normalization.await;
                            return Err(MediaPreprocessError::DeadlineExceeded);
                        }
                    };
                    check_normalization_budget(cancellation, deadline)?;
                    self.store
                        .get_or_create_derivative(
                            principal,
                            &source.id,
                            normalized.bytes,
                            self.derivative_retention,
                        )
                        .await
                        .map_err(MediaPreprocessError::from)?
                }
            };
            check_normalization_budget(cancellation, deadline)?;
            let (derivative, derivative_bytes) = self
                .store
                .read_artifact_bounded(principal, &media.derivative.id, MAX_DERIVATIVE_BYTES as u64)
                .await
                .map_err(|error| match error {
                    MediaStoreError::TooLarge => MediaPreprocessError::DerivativeTooLarge,
                    other => MediaPreprocessError::from(other),
                })?;
            check_normalization_budget(cancellation, deadline)?;
            if derivative.mime_type != "image/jpeg"
                || derivative_bytes.is_empty()
                || derivative_bytes.len() > MAX_DERIVATIVE_BYTES
            {
                return Err(MediaPreprocessError::DerivativeTooLarge);
            }
            derivative_total = derivative_total
                .checked_add(derivative_bytes.len())
                .ok_or(MediaPreprocessError::DerivativeAggregateTooLarge)?;
            if derivative_total > MAX_TURN_DERIVATIVE_BYTES {
                return Err(MediaPreprocessError::DerivativeAggregateTooLarge);
            }
            prepared.push(PreparedMedia {
                source,
                derivative,
                #[cfg(any(test, feature = "test-support"))]
                derivative_bytes,
            });
        }
        check_normalization_budget(cancellation, deadline)?;
        Ok(prepared)
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MediaPreprocessError {
    #[error("Media source exceeds the byte limit")]
    SourceTooLarge,
    #[error("A Media Turn accepts at most eight new Artifacts")]
    TooManyArtifacts,
    #[error("Duplicate Media Artifact")]
    DuplicateArtifact,
    #[error("Media source format is unsupported")]
    UnsupportedType,
    #[error("Declared media type does not match the source bytes")]
    MimeMismatch,
    #[error("Media sources exceed the per-Turn byte limit")]
    SourceAggregateTooLarge,
    #[error("Animated WebP is unsupported")]
    AnimatedWebp,
    #[error("Media dimensions exceed the limit")]
    DimensionsTooLarge,
    #[error("Media pixel count exceeds the limit")]
    TooManyPixels,
    #[error("Media decoding failed")]
    Decode,
    #[error("Media Derivative exceeds the byte limit")]
    DerivativeTooLarge,
    #[error("Media Derivatives exceed the per-Turn byte limit")]
    DerivativeAggregateTooLarge,
    #[error("Media Artifact is unavailable")]
    Unavailable,
    #[error("Media storage failed")]
    Storage,
    #[error("Media preprocessing was cancelled")]
    Cancelled,
    #[error("Media preprocessing deadline exceeded")]
    DeadlineExceeded,
}

impl From<MediaStoreError> for MediaPreprocessError {
    fn from(error: MediaStoreError) -> Self {
        match error {
            MediaStoreError::Unavailable => Self::Unavailable,
            MediaStoreError::TooLarge | MediaStoreError::Corrupt | MediaStoreError::Storage(_) => {
                Self::Storage
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn normalize_image(
    source: &[u8],
    declared_mime: &str,
) -> Result<NormalizedImage, MediaPreprocessError> {
    normalize_image_until(
        source,
        declared_mime,
        &stravia_runtime_contract::CancellationToken::new(),
        Instant::now() + Duration::from_secs(24 * 60 * 60),
    )
}

fn normalize_image_until(
    source: &[u8],
    declared_mime: &str,
    cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: Instant,
) -> Result<NormalizedImage, MediaPreprocessError> {
    check_normalization_budget(cancellation, deadline)?;
    if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
        return Err(MediaPreprocessError::SourceTooLarge);
    }
    let format = image::guess_format(source).map_err(|_| MediaPreprocessError::UnsupportedType)?;
    let expected_mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::WebP => "image/webp",
        _ => return Err(MediaPreprocessError::UnsupportedType),
    };
    if declared_mime.trim().to_ascii_lowercase() != expected_mime {
        return Err(MediaPreprocessError::MimeMismatch);
    }
    if format == ImageFormat::WebP {
        let decoder =
            WebPDecoder::new(Cursor::new(source)).map_err(|_| MediaPreprocessError::Decode)?;
        if decoder.has_animation() {
            return Err(MediaPreprocessError::AnimatedWebp);
        }
    }
    check_normalization_budget(cancellation, deadline)?;

    let mut reader = ImageReader::with_format(Cursor::new(source), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits.clone());
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| MediaPreprocessError::Decode)?;
    let (source_width, source_height) = decoder.dimensions();
    if source_width == 0
        || source_height == 0
        || source_width > MAX_IMAGE_EDGE
        || source_height > MAX_IMAGE_EDGE
    {
        return Err(MediaPreprocessError::DimensionsTooLarge);
    }
    let pixels = u64::from(source_width)
        .checked_mul(u64::from(source_height))
        .ok_or(MediaPreprocessError::TooManyPixels)?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(MediaPreprocessError::TooManyPixels);
    }
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    decoder
        .set_limits(limits)
        .map_err(|_| MediaPreprocessError::Decode)?;
    let orientation = decoder
        .orientation()
        .map_err(|_| MediaPreprocessError::Decode)?;
    check_normalization_budget(cancellation, deadline)?;
    let mut decoded =
        DynamicImage::from_decoder(decoder).map_err(|_| MediaPreprocessError::Decode)?;
    decoded.apply_orientation(orientation);
    check_normalization_budget(cancellation, deadline)?;

    let mut rgb = if decoded.color().has_alpha() {
        composite_alpha_on_white(decoded)
    } else {
        decoded.into_rgb8()
    };
    check_normalization_budget(cancellation, deadline)?;
    let (width, height) = rgb.dimensions();
    let longest_edge = width.max(height);
    if longest_edge > MAX_DERIVATIVE_EDGE {
        let resized_width = ((u64::from(width) * u64::from(MAX_DERIVATIVE_EDGE))
            / u64::from(longest_edge))
        .max(1) as u32;
        let resized_height = ((u64::from(height) * u64::from(MAX_DERIVATIVE_EDGE))
            / u64::from(longest_edge))
        .max(1) as u32;
        rgb = image::imageops::resize(&rgb, resized_width, resized_height, FilterType::Lanczos3);
    }
    check_normalization_budget(cancellation, deadline)?;
    let (width, height) = rgb.dimensions();
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, JPEG_QUALITY)
        .write_image(rgb.as_raw(), width, height, ExtendedColorType::Rgb8)
        .map_err(|_| MediaPreprocessError::Decode)?;
    check_normalization_budget(cancellation, deadline)?;
    if encoded.len() > MAX_DERIVATIVE_BYTES {
        return Err(MediaPreprocessError::DerivativeTooLarge);
    }
    Ok(NormalizedImage {
        bytes: Bytes::from(encoded),
        #[cfg(any(test, feature = "test-support"))]
        width,
        #[cfg(any(test, feature = "test-support"))]
        height,
    })
}

fn check_normalization_budget(
    cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: Instant,
) -> Result<(), MediaPreprocessError> {
    if cancellation.is_cancelled() {
        Err(MediaPreprocessError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(MediaPreprocessError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

fn composite_alpha_on_white(image: DynamicImage) -> RgbImage {
    let rgba = image.into_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u16::from(source[3]);
        for channel in 0..3 {
            let foreground = u16::from(source[channel]) * alpha;
            let background = 255_u16 * (255 - alpha);
            target[channel] = ((foreground + background + 127) / 255) as u8;
        }
    }
    rgb
}
