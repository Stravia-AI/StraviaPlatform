//! Wire-level codec implementations. Each endpoint module owns the private
//! implementation used by its `ProtocolAdapter` registration.

pub mod anthropic;
pub mod compaction;
mod control_projection;
pub mod google;
pub mod open_responses;
pub mod openai;
pub mod reasoning;
pub mod thinking;
pub mod tool_correlation;

use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::DocumentSource;
use stravia_runtime_contract::protocol::ir::MediaSource;

pub(crate) fn document_media_source(source: DocumentSource) -> Option<MediaSource> {
    match source {
        DocumentSource::Base64Pdf { data } => Some(MediaSource::Base64 {
            media_type: "application/pdf".into(),
            data,
        }),
        DocumentSource::PlainText { data } => {
            use base64::Engine as _;
            Some(MediaSource::Base64 {
                media_type: "text/plain".into(),
                data: base64::engine::general_purpose::STANDARD.encode(data.as_bytes()),
            })
        }
        DocumentSource::Url(url) => Some(MediaSource::Url(url)),
        DocumentSource::Blocks { .. } => None,
    }
}

/// Preserve legacy fallback wire shapes without exposing persisted IR metadata.
pub(crate) fn content_block_wire_value(block: &ContentBlock) -> serde_json::Value {
    fn strip_internal_fields(block: &ContentBlock, value: &mut serde_json::Value) {
        match block {
            ContentBlock::ToolResult { .. } | ContentBlock::ServerToolResult { .. } => {
                if let Some(object) = value.as_object_mut() {
                    object.remove("content_kind");
                }
            }
            ContentBlock::SearchResult { content, .. } => {
                if let Some(values) = value
                    .get_mut("content")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for (block, value) in content.iter().zip(values) {
                        strip_internal_fields(block, value);
                    }
                }
            }
            ContentBlock::Document {
                source: DocumentSource::Blocks { content },
                ..
            } => {
                if let Some(values) = value
                    .pointer_mut("/source/content")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for (block, value) in content.iter().zip(values) {
                        strip_internal_fields(block, value);
                    }
                }
            }
            _ => {}
        }
    }

    let mut value = serde_json::to_value(block).unwrap_or(serde_json::Value::Null);
    strip_internal_fields(block, &mut value);
    value
}

/// Parse a `data:<media_type>;base64,<data>` URL into canonical media.
///
/// Unrecognized data URLs remain ordinary URLs so protocol validation can
/// report the unsupported source instead of silently discarding it.
pub(crate) fn parse_data_url_source(mut url: String) -> MediaSource {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some(semi) = rest.find(';')
    {
        let after = &rest[semi + 1..];
        if after.starts_with("base64,") {
            let media_type = rest[..semi].to_string();
            let prefix_len = "data:".len() + semi + 1 + "base64,".len();
            url.drain(..prefix_len);
            return MediaSource::Base64 {
                media_type,
                data: url,
            };
        }
    }
    MediaSource::Url(url)
}

/// Reuse the owned payload buffer when forming a vendor data URL.
pub(crate) fn media_data_url(media_type: &str, mut data: String) -> String {
    let prefix = format!("data:{media_type};base64,");
    data.reserve_exact(prefix.len());
    data.insert_str(0, &prefix);
    data
}

#[cfg(test)]
mod media_tests {
    use super::*;

    #[test]
    fn data_urls_preserve_payload_and_unrecognized_sources() {
        for (mime, data) in [
            ("image/png", "aGVsbG8=+/"),
            ("application/pdf", ""),
            ("text/plain;charset=utf-8", "YWJj"),
        ] {
            let url = media_data_url(mime, data.into());
            // Only the first semicolon introduces the Base64 marker.
            if mime.contains(';') {
                let MediaSource::Url(actual) = parse_data_url_source(url.clone()) else {
                    panic!("unsupported data URL must retain its source");
                };
                assert_eq!(actual, url);
            } else {
                let MediaSource::Base64 {
                    media_type,
                    data: actual,
                } = parse_data_url_source(url)
                else {
                    panic!("base64 data URL must decode");
                };
                assert_eq!(media_type, mime);
                assert_eq!(actual, data);
            }
        }
        for url in [
            "https://example.test/image",
            "data:image/png;other,abc",
            "data:é;base64,YQ==",
        ] {
            let source = parse_data_url_source(url.into());
            if url == "data:é;base64,YQ==" {
                let MediaSource::Base64 { media_type, data } = source else {
                    panic!("UTF-8 media type must retain its payload");
                };
                assert_eq!(media_type, "é");
                assert_eq!(data, "YQ==");
            } else {
                let MediaSource::Url(actual) = source else {
                    panic!("unrecognized source must remain a URL");
                };
                assert_eq!(actual, url);
            }
        }
    }
}
