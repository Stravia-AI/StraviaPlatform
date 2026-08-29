use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::{ArtifactId, ArtifactRef};
use crate::hook::Principal;
use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, RgbImage,
};

use super::store::{MediaDerivativeStore, MediaStoreError};

pub(crate) const MAX_MEDIA_ARTIFACTS: usize = 8;
pub(crate) const MAX_SOURCE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const MAX_TURN_SOURCE_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const MAX_DERIVATIVE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const MAX_TURN_DERIVATIVE_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const MAX_IMAGE_EDGE: u32 = 8192;
pub(crate) const MAX_IMAGE_PIXELS: u64 = 25_000_000;
pub(crate) const MAX_DERIVATIVE_EDGE: u32 = 3072;
pub(crate) const JPEG_QUALITY: u8 = 85;

#[derive(Debug, Clone)]
pub(crate) struct NormalizedImage {
    pub bytes: Bytes,
    #[cfg(test)]
    pub width: u32,
    #[cfg(test)]
    pub height: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedMedia {
    pub source: ArtifactRef,
    pub derivative: ArtifactRef,
    #[cfg(test)]
    pub derivative_bytes: Bytes,
}
#[derive(Clone)]
pub(crate) struct MediaInputPreprocessor {
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

    #[cfg(test)]
    pub async fn preprocess(
        &self,
        principal: &Principal,
        source_ids: &[ArtifactId],
    ) -> Result<Vec<PreparedMedia>, MediaPreprocessError> {
        self.preprocess_until(
            principal,
            source_ids,
            &crate::proxy::context::CancellationToken::new(),
            Instant::now() + Duration::from_secs(24 * 60 * 60),
        )
        .await
    }

    pub async fn preprocess_until(
        &self,
        principal: &Principal,
        source_ids: &[ArtifactId],
        cancellation: &crate::proxy::context::CancellationToken,
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
                #[cfg(test)]
                derivative_bytes,
            });
        }
        check_normalization_budget(cancellation, deadline)?;
        Ok(prepared)
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub(crate) enum MediaPreprocessError {
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

#[cfg(test)]
pub(crate) fn normalize_image(
    source: &[u8],
    declared_mime: &str,
) -> Result<NormalizedImage, MediaPreprocessError> {
    normalize_image_until(
        source,
        declared_mime,
        &crate::proxy::context::CancellationToken::new(),
        Instant::now() + Duration::from_secs(24 * 60 * 60),
    )
}

fn normalize_image_until(
    source: &[u8],
    declared_mime: &str,
    cancellation: &crate::proxy::context::CancellationToken,
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
        #[cfg(test)]
        width,
        #[cfg(test)]
        height,
    })
}

fn check_normalization_budget(
    cancellation: &crate::proxy::context::CancellationToken,
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
#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::agent::LocalArtifactStore;
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, GenericImageView, ImageEncoder, ImageReader};

    use super::*;
    const TRANSPARENT_PNG: &[u8] = include_bytes!("../../tests/fixtures/media/transparent.png");
    const STATIC_WEBP: &[u8] = include_bytes!("../../tests/fixtures/media/static.webp");
    const ANIMATED_WEBP: &[u8] = include_bytes!("../../tests/fixtures/media/animated.webp");
    const ORIENTED_JPEG: &[u8] = include_bytes!("../../tests/fixtures/media/orientation-6.jpg");

    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(rgba, width, height, ExtendedColorType::Rgba8)
            .expect("PNG fixture");
        bytes
    }

    fn encode_luma_png(width: u32, height: u32, luma: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(luma, width, height, ExtendedColorType::L8)
            .expect("grayscale PNG fixture");
        bytes
    }

    fn encode_jpeg(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 95)
            .write_image(rgb, width, height, ExtendedColorType::Rgb8)
            .expect("JPEG fixture");
        bytes
    }

    fn decode_jpeg(bytes: &[u8]) -> image::DynamicImage {
        ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Jpeg)
            .decode()
            .expect("normalized JPEG")
    }

    fn jpeg_sampling(bytes: &[u8]) -> Vec<(u8, u8)> {
        let mut offset = 2;
        while offset + 4 <= bytes.len() {
            if bytes[offset] != 0xff {
                offset += 1;
                continue;
            }
            let marker = bytes[offset + 1];
            offset += 2;
            if marker == 0xd8 || marker == 0xd9 {
                continue;
            }
            let length = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
            if marker == 0xc0 {
                let components = bytes[offset + 7] as usize;
                return (0..components)
                    .map(|index| {
                        let sampling = bytes[offset + 9 + index * 3];
                        (sampling >> 4, sampling & 0x0f)
                    })
                    .collect();
            }
            offset += length;
        }
        panic!("baseline JPEG frame header");
    }

    #[test]
    fn transparent_png_is_composited_on_white_without_upscaling() {
        let normalized = normalize_image(TRANSPARENT_PNG, "image/png").expect("normalized PNG");
        assert_eq!((normalized.width, normalized.height), (2, 1));
        assert_eq!(
            image::guess_format(&normalized.bytes).unwrap(),
            image::ImageFormat::Jpeg
        );
        let image = decode_jpeg(&normalized.bytes).to_rgb8();
        let transparent = image.get_pixel(1, 0).0;
        assert!(transparent.iter().all(|channel| *channel > 220));
    }

    #[test]
    fn static_webp_is_accepted_but_animation_is_rejected() {
        let normalized = normalize_image(STATIC_WEBP, "image/webp").expect("static WebP");
        assert_eq!((normalized.width, normalized.height), (2, 1));
        assert_eq!(
            normalize_image(ANIMATED_WEBP, "image/webp").unwrap_err(),
            MediaPreprocessError::AnimatedWebp
        );
    }

    #[test]
    fn exif_orientation_is_applied_and_derivative_has_no_metadata() {
        let normalized = normalize_image(ORIENTED_JPEG, "image/jpeg").expect("oriented JPEG");
        assert_eq!((normalized.width, normalized.height), (1, 2));
        assert!(
            !normalized
                .bytes
                .windows(b"Exif".len())
                .any(|window| window == b"Exif")
        );
        assert!(
            !normalized
                .bytes
                .windows(b"ICC_PROFILE".len())
                .any(|window| window == b"ICC_PROFILE")
        );
        assert!(
            jpeg_sampling(&normalized.bytes)
                .into_iter()
                .all(|sampling| sampling == (1, 1))
        );
    }

    #[test]
    fn large_image_is_resized_once_to_the_bounded_longest_edge() {
        let source = encode_jpeg(4000, 1000, &vec![127; 4000 * 1000 * 3]);
        let normalized = normalize_image(&source, "image/jpeg").expect("normalized JPEG");
        assert_eq!((normalized.width, normalized.height), (3072, 768));
        assert_ne!(normalized.bytes.as_ref(), source.as_slice());
        assert_eq!(decode_jpeg(&normalized.bytes).dimensions(), (3072, 768));
    }

    #[test]
    fn byte_dimension_and_pixel_limits_fail_before_decode() {
        assert_eq!(
            normalize_image(&vec![0; MAX_SOURCE_BYTES + 1], "image/png").unwrap_err(),
            MediaPreprocessError::SourceTooLarge
        );

        let too_wide = encode_jpeg(8193, 1, &vec![127; 8193 * 3]);
        assert_eq!(
            normalize_image(&too_wide, "image/jpeg").unwrap_err(),
            MediaPreprocessError::DimensionsTooLarge
        );

        let too_many_pixels = encode_luma_png(8192, 3052, &vec![0; 8192 * 3052]);
        assert_eq!(
            normalize_image(&too_many_pixels, "image/png").unwrap_err(),
            MediaPreprocessError::TooManyPixels
        );
    }

    #[test]
    fn mime_spoof_and_unsupported_containers_are_rejected() {
        let png = encode_png(1, 1, &[0, 0, 0, 255]);
        assert_eq!(
            normalize_image(&png, "image/jpeg").unwrap_err(),
            MediaPreprocessError::MimeMismatch
        );
        assert_eq!(
            normalize_image(b"GIF89a", "image/gif").unwrap_err(),
            MediaPreprocessError::UnsupportedType
        );
    }

    #[tokio::test]
    async fn preprocessing_preserves_source_order_and_reuses_derivatives() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let artifacts = Arc::new(LocalArtifactStore::sqlite(
            pool.clone(),
            data_dir.path().join("artifacts"),
        ));
        let store = Arc::new(MediaDerivativeStore::sqlite(pool, artifacts));
        let principal = Principal::new("owner");
        let first = store
            .create_source(
                &principal,
                "image/png",
                Bytes::from_static(TRANSPARENT_PNG),
                Duration::from_secs(60),
            )
            .await
            .expect("first source");
        let second = store
            .create_source(
                &principal,
                "image/webp",
                Bytes::from_static(STATIC_WEBP),
                Duration::from_secs(60),
            )
            .await
            .expect("second source");
        let preprocessor = MediaInputPreprocessor::new(Arc::clone(&store), Duration::from_secs(60));
        let cancelled = crate::proxy::context::CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            preprocessor
                .preprocess_until(
                    &principal,
                    std::slice::from_ref(&first.id),
                    &cancelled,
                    Instant::now() + Duration::from_secs(60),
                )
                .await
                .unwrap_err(),
            MediaPreprocessError::Cancelled
        );
        assert_eq!(
            preprocessor
                .preprocess_until(
                    &principal,
                    std::slice::from_ref(&first.id),
                    &crate::proxy::context::CancellationToken::new(),
                    Instant::now() - Duration::from_millis(1),
                )
                .await
                .unwrap_err(),
            MediaPreprocessError::DeadlineExceeded
        );

        let prepared = preprocessor
            .preprocess(&principal, &[second.id.clone(), first.id.clone()])
            .await
            .expect("prepared media");
        assert_eq!(
            prepared
                .iter()
                .map(|media| media.source.id.clone())
                .collect::<Vec<_>>(),
            [second.id.clone(), first.id.clone()]
        );
        assert!(prepared.iter().all(|media| {
            media.derivative.mime_type == "image/jpeg"
                && matches!(
                    image::guess_format(&media.derivative_bytes),
                    Ok(image::ImageFormat::Jpeg)
                )
        }));

        let reused = preprocessor
            .preprocess(&principal, &[second.id.clone(), first.id.clone()])
            .await
            .expect("reused media");
        assert_eq!(
            reused
                .iter()
                .map(|media| media.derivative.id.clone())
                .collect::<Vec<_>>(),
            prepared
                .iter()
                .map(|media| media.derivative.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            preprocessor
                .preprocess(&principal, std::slice::from_ref(&prepared[0].derivative.id),)
                .await
                .unwrap_err(),
            MediaPreprocessError::Unavailable
        );
        assert_eq!(
            preprocessor
                .preprocess(&principal, &[first.id.clone(), first.id])
                .await
                .unwrap_err(),
            MediaPreprocessError::DuplicateArtifact
        );
    }

    #[tokio::test]
    async fn preprocessing_enforces_per_turn_source_and_derivative_bytes() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let pool = crate::db::init_pool(data_dir.path())
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let artifacts = Arc::new(LocalArtifactStore::sqlite(
            pool.clone(),
            data_dir.path().join("artifacts"),
        ));
        let store = Arc::new(MediaDerivativeStore::sqlite(pool, artifacts));
        let principal = Principal::new("owner");
        let preprocessor = MediaInputPreprocessor::new(Arc::clone(&store), Duration::from_secs(60));

        let mut oversized_sources = Vec::new();
        for _ in 0..5 {
            let mut bytes = TRANSPARENT_PNG.to_vec();
            bytes.resize(MAX_SOURCE_BYTES, 0);
            oversized_sources.push(
                store
                    .create_source(
                        &principal,
                        "image/png",
                        Bytes::from(bytes),
                        Duration::from_secs(60),
                    )
                    .await
                    .expect("bounded source")
                    .id,
            );
        }
        assert_eq!(
            preprocessor
                .preprocess(&principal, &oversized_sources)
                .await
                .unwrap_err(),
            MediaPreprocessError::SourceAggregateTooLarge
        );

        let mut derivative_sources = Vec::new();
        for _ in 0..5 {
            let source = store
                .create_source(
                    &principal,
                    "image/png",
                    Bytes::from_static(TRANSPARENT_PNG),
                    Duration::from_secs(60),
                )
                .await
                .expect("source");
            let mut derivative = encode_jpeg(1, 1, &[127, 127, 127]);
            derivative.resize(MAX_DERIVATIVE_BYTES, 0);
            store
                .get_or_create_derivative(
                    &principal,
                    &source.id,
                    Bytes::from(derivative),
                    Duration::from_secs(60),
                )
                .await
                .expect("bounded derivative");
            derivative_sources.push(source.id);
        }
        assert_eq!(
            preprocessor
                .preprocess(&principal, &derivative_sources)
                .await
                .unwrap_err(),
            MediaPreprocessError::DerivativeAggregateTooLarge
        );
    }
}
