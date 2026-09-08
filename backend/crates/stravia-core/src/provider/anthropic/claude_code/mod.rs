//! Anthropic Claude Code OAuth channel.
//!
//! Auth-specific headers are injected by `ClaudeOAuthDriver` through
//! `RuntimeBinding.extra_headers`; this channel extension just gives the
//! resolver a concrete `(vendor=anthropic, channel=claude-code)` target
//! and intentionally returns no fallback auth headers so that flipping
//! `disable_default_auth` cannot leak an empty `x-api-key`.

use reqwest::header::HeaderMap;

use crate::provider::registry::{ExtensionRegistration, VendorScope};
use crate::provider::vendor_ext::VendorExtension;

pub struct AnthropicClaudeCodeChannel;

impl VendorExtension for AnthropicClaudeCodeChannel {
    fn scope(&self) -> VendorScope {
        VendorScope::Channel {
            vendor_id: "anthropic",
            channel_id: "claude-code",
        }
    }

    fn construct_request(
        &self,
        _ctx: &crate::provider::vendor_ext::RequestContext<'_>,
        purpose: crate::provider::vendor_ext::RequestPurpose<'_>,
    ) -> anyhow::Result<crate::provider::vendor_ext::ConstructedRequest> {
        // The runtime binding is the sole credential owner for this channel.
        let url = purpose.endpoint();
        reqwest::Url::parse(&url)?;
        Ok(crate::provider::vendor_ext::ConstructedRequest {
            url,
            headers: HeaderMap::new(),
        })
    }
}

inventory::submit! {
    ExtensionRegistration { make: || Box::new(AnthropicClaudeCodeChannel) }
}
