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

use crate::protocol::ir::MediaSource;

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
