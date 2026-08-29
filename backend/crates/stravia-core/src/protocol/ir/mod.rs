//! Internal Representation (IR) for the Stravia AI Gateway.
//!
//! # Design principles
//!
//! 1. **No silent drops** — every field from every supported protocol has an
//!    explicit home: core IR field, `ProtocolExt`, `VendorExtensions`, or DROP.
//!    See `docs/design/ir/FIELD_HOMING.md` for the authoritative decision table.
//! 2. **Lossless envelope** — `RawEnvelope` keeps a snapshot of the original
//!    request body + headers for pass-through and audit.
//! 3. **Repair, not validate** — `repair.rs` performs single-direction
//!    mutations (fill missing `tool_call_id`, fix orphaned refs) without
//!    rejecting the request.

pub mod cache;
pub(crate) mod canonical;
pub mod envelope;
pub mod error;
pub mod ext;
pub mod repair;
pub mod request;
pub mod response;
pub mod schema;
pub mod stream;
pub mod usage;
pub mod vendor_ext;

// ── Cache ──────────────────────────────────────────────────────────────────────
pub use cache::{CacheControl, CacheTtl};

// ── Error ──────────────────────────────────────────────────────────────────────
pub use error::{AiError, AiErrorKind};

// ── ProtocolExt ───────────────────────────────────────────────────────────────
pub use ext::{AnthropicExt, GoogleExt, OpenAIChatExt, OpenResponsesExt, ProtocolExt};

// ── Request ───────────────────────────────────────────────────────────────────
pub use request::{
    AiItem, AiItemAudience, AiItemProvenance, AiItemStatus, AiRequest, ContentBlock,
    DocumentSource, EmbeddingInput, EmbeddingRequest, GenerationConfig, MediaSource,
    MessageContent, ReasoningConfig, ReasoningEffort, RequestMetadata, ResponseFormat, Role,
    SafetySettings, StreamConfig, ToolCall, ToolChoice, ToolSpec,
};

// ── Response ──────────────────────────────────────────────────────────────────
pub use response::{AiResponse, EmbeddingData, EmbeddingOutput, EmbeddingVector};

// ── Schema ────────────────────────────────────────────────────────────────────
pub use schema::SchemaObject;

// ── Stream ────────────────────────────────────────────────────────────────────
pub use stream::StreamDelta as AiStreamDelta;

// ── Usage ─────────────────────────────────────────────────────────────────────
pub use usage::{ServerToolUsage, Usage};

// ── Vendor ────────────────────────────────────────────────────────────────────
pub use envelope::RawEnvelope;
pub use vendor_ext::VendorExtensions;
