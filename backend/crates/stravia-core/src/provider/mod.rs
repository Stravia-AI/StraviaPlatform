//! Provider layer — unified `Vendor` trait, metadata, and orchestration.
//!
//! # Architecture
//!
//! ```text
//! provider/
//! ├── vendor.rs           — Vendor trait + ProviderCtx (primary abstraction)
//! ├── vendor_ext.rs       — VendorExtension trait + VendorCtx (channel/family hooks)
//! ├── registry.rs         — VendorRegistry (unified, replaces dual registry)
//! ├── metadata.rs         — VendorMetadata types
//! ├── outbound.rs         — OutboundRequest (wire-format outbound)
//! ├── inbound.rs          — InboundResponse (wire-format inbound)
//! ├── common/
//! │   ├── openai_compat.rs — Bearer auth, URL helpers, openai_compat_vendor! macro
//! │   └── pipeline.rs      — standard 7-step request/response pipeline
//! └── <vendor>/mod.rs     — per-vendor Vendor impls
//! ```

pub mod common;
pub mod inbound;
pub mod metadata;
pub mod outbound;
pub mod registry;
pub mod vendor;
pub mod vendor_ext;

// ── Known vendors (each registers itself via inventory::submit!) ──────────────
pub mod aihubmix;
pub mod amazon_bedrock;
pub mod anthropic;
pub mod azure;
pub mod cerebras;
pub mod cloudflare_ai_gateway;
pub mod cohere;
pub mod custom;
pub mod deepinfra;
pub mod gateway;
pub mod gitlab;
pub mod google;
pub mod google_vertex;
pub mod groq;
pub mod merge_gateway;
pub mod mistral;
pub mod ollama;
pub mod openai;
pub mod openai_compatible;
pub mod openrouter;
pub mod perplexity;
pub mod qvac;
pub mod salad_cloud;
pub mod sap_ai_core;
pub mod togetherai;
pub mod venice;
pub mod vercel;
pub mod watsonx;
pub mod xai;

// ── Flat re-exports ───────────────────────────────────────────────────────────

pub use inbound::InboundResponse;
pub use metadata::{
    API_KEY_CREDENTIAL_FIELDS, AuthMode, ChannelDef, CredentialFieldDef, CredentialInputKind,
    Label, OAuthConfig, ProtocolBaseUrl, RuntimeConfig, VendorMetadata,
};
pub use outbound::OutboundRequest;
pub use registry::{
    ExtensionRegistration, VendorMetadataRegistration, VendorRegistration, VendorRegistry,
    VendorScope,
};
pub use vendor::ProviderCtx;
pub use vendor::Vendor;
pub use vendor_ext::{VendorCtx, VendorExtension};
