//! Wire-level codec implementations. Each endpoint module owns the private
//! implementation used by its `ProtocolAdapter` registration.

pub mod anthropic;
pub mod bedrock;
pub mod cohere;
pub mod gateway;
pub mod google;
pub mod open_responses;
pub mod openai;
pub mod reasoning;
pub mod tool_correlation;
pub mod watsonx;

use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_runtime_contract::protocol::ir::DocumentSource;
use stravia_runtime_contract::protocol::ir::MediaSource;

/// Preserve legacy fallback wire shapes without exposing persisted IR metadata.
fn content_block_wire_value(block: &ContentBlock) -> serde_json::Value {
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
pub(crate) fn parse_data_url_source(url: String) -> MediaSource {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some(semi) = rest.find(';')
    {
        let media_type = rest[..semi].to_string();
        let after = &rest[semi + 1..];
        if let Some(data) = after.strip_prefix("base64,") {
            return MediaSource::Base64 {
                media_type,
                data: data.to_string(),
            };
        }
    }
    MediaSource::Url(url)
}
