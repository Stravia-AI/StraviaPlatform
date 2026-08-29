//! Vendor extensions — three-segment model.
//!
//! Every `AiRequest` and `AiResponse` carries a `VendorExtensions` bag that
//! holds fields which don't have a home in the canonical IR schema.
//!
//! ## Three segments
//!
//! - **`ingress`** — extra fields extracted from the *client* body.  These
//!   belong to the ingress protocol family (e.g. OpenAI `service_tier`).
//!   Forwarded to the egress by the codec if the egress vendor understands them.
//!
//! - **`egress`** — fields injected by the egress codec or `Vendor` adapter
//!   just before the upstream call. Examples include provider-specific header
//!   hints and opaque context references. They are not part of the ingress body.
//!
//! - **`passthrough_safe`** — unknown fields explicitly admitted by a codec's
//!   allowlist. Despite the historical field name, these remain inside
//!   canonical IR and are re-encoded by the target codec; they do not bypass
//!   the inference pipeline.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VendorExtensions {
    /// Extra fields from the ingress body (client side).
    pub ingress: HashMap<String, Value>,
    /// Extra fields for the egress body (provider side).
    pub egress: HashMap<String, Value>,
    /// Allowlisted unknown fields retained in canonical IR for target re-encoding.
    pub passthrough_safe: HashMap<String, Value>,
}

impl VendorExtensions {
    pub fn is_empty(&self) -> bool {
        self.ingress.is_empty() && self.egress.is_empty() && self.passthrough_safe.is_empty()
    }
}
