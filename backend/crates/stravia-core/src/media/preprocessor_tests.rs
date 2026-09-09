use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use stravia_runtime_contract::Principal;

use stravia_media::store::MediaDerivativeStore;

use stravia_media::preprocessor::*;

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
        let store = Arc::new(MediaDerivativeStore::sqlite(
            pool,
            Arc::new(super::super::ArtifactHost(artifacts)),
        ));
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
        let cancelled = stravia_runtime_contract::CancellationToken::new();
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
                    &stravia_runtime_contract::CancellationToken::new(),
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
        let store = Arc::new(MediaDerivativeStore::sqlite(
            pool,
            Arc::new(super::super::ArtifactHost(artifacts)),
        ));
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
