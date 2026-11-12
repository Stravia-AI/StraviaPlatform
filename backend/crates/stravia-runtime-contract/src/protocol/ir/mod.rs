//! Internal Representation (IR) for Stravia Agent infra.
//!
//! # Design principles
//!
//! 1. **Unified controls** — supported request intent has one typed canonical
//!    field; unsupported vendor controls are discarded at the protocol boundary.
//!    `ProtocolExt` is only a legacy snapshot DTO, not runtime request state.
//! 2. **Lossless envelope** — `RawEnvelope` keeps a snapshot of the original
//!    request body + headers for pass-through and audit.
//! 3. **Repair, not validate** — `repair.rs` performs single-direction
//!    mutations (fill missing `tool_call_id`, fix orphaned refs) without
//!    rejecting the request.

pub mod cache;
pub mod canonical;
pub mod content;
pub mod envelope;
pub mod error;
mod ext;
pub mod native_tools;
pub mod parameters;
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
use ext::ProtocolExt;
pub use native_tools::{
    CodeExecutionTool, CustomTool, ImageGenerationTool, NamespaceMember, NativeTool,
    NativeToolKind, SearchContextSize, ToolGrammar, ToolNamespace, ToolSearchTool, UserLocation,
    WebSearchTool, native_tool_from_anthropic, native_tool_from_chat_web_search_options,
    native_tool_from_gemini, native_tool_from_responses, native_tool_to_anthropic,
    native_tool_to_chat_web_search_options, native_tool_to_gemini, native_tool_to_responses,
};
pub use parameters::{
    AudioOutput, CacheParameters, CacheRetention, CompactionControl, Continuation, Modality,
    ParameterReset, RequestParameters, ServiceTier, SessionParameters, Truncation, Verbosity,
};

// ── Request ───────────────────────────────────────────────────────────────────
pub use request::TOOL_RESULT_CONTENT_KIND_META;
pub use request::{
    AiItem, AiItemAudience, AiItemMetadata, AiItemProvenance, AiItemStatus, AiRequest,
    CanonicalItemId, ContentBlock, DocumentSource, GenerationConfig, ItemReference, MediaSource,
    MessageContent, ReasoningConfig, ReasoningSummary, RequestMetadata, ResponseFormat, Role,
    StreamConfig, ToolCall, ToolCallId, ToolChoice, ToolResultContentKind, ToolSpec,
};

// ── Response ──────────────────────────────────────────────────────────────────
pub use response::{AiResponse, NativeCompactionResponse};

// ── Schema ────────────────────────────────────────────────────────────────────
pub use schema::SchemaObject;

// ── Stream ────────────────────────────────────────────────────────────────────
pub use stream::StreamDelta as AiStreamDelta;

// ── Usage ─────────────────────────────────────────────────────────────────────
pub use usage::{ServerToolUsage, Usage};

// ── Vendor ────────────────────────────────────────────────────────────────────
pub use envelope::RawEnvelope;
pub use vendor_ext::VendorExtensions;
