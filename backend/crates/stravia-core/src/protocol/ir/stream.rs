//! Stream deltas for `AiResponse`.

use crate::protocol::ir::error::AiError;
use crate::protocol::ir::request::ToolCall;
use crate::protocol::ir::usage::Usage;

use super::AiItem;

/// A single parsed delta from a streaming response.
///
/// The stream parser emits a sequence of `StreamDelta` values.  The accumulator
/// (PR-4) coalesces them into a complete `AiResponse`.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    /// First chunk — identifies the response and model.
    MessageStart { id: String, model: String },
    /// Effective response controls and echoed request state.
    ResponseMetadata { metadata: serde_json::Value },
    /// Incremental text output.
    TextDelta(String),
    /// Incremental text output with dated token metadata retained.
    TextDeltaWithMetadata {
        text: String,
        logprobs: Vec<serde_json::Value>,
        obfuscation: Option<String>,
        /// Original dated output-item index, when the source protocol exposes it.
        output_index: Option<usize>,
        /// Original message content-part index, when available.
        content_index: Option<usize>,
    },
    /// Incremental refusal content. Kept distinct from normal assistant text.
    RefusalDelta(String),
    /// Incremental refusal content with dated output/content indices.
    RefusalDeltaWithIndex {
        text: String,
        output_index: usize,
        content_index: usize,
    },
    /// Incremental thinking / reasoning output (Anthropic `ThinkingBlockParam`,
    /// Google `Part{thought=true}`, OpenAI reasoning items).
    ThinkingDelta(String),
    /// Incremental full reasoning content with dated padding metadata retained.
    ThinkingDeltaWithMetadata {
        text: String,
        obfuscation: Option<String>,
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
    /// Incremental summary of reasoning, distinct from full reasoning content.
    ReasoningSummaryDelta {
        text: String,
        obfuscation: Option<String>,
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
    /// Thinking signature for multi-turn passback (Anthropic).
    ThinkingSignature(String),
    /// A tool call is starting.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// Incremental tool call argument JSON fragment.
    ToolCallDelta { index: usize, arguments: String },
    /// Tool call arguments are complete.
    ToolCallComplete { index: usize, tool_call: ToolCall },
    /// Completed canonical output item with dated wire metadata retained.
    ItemDone { index: usize, item: AiItem },
    /// Final token usage statistics.
    Usage(Usage),
    /// Dated Responses terminal resource semantics.
    ResponseTerminal {
        status: String,
        incomplete_details: Option<serde_json::Value>,
    },
    /// Stream ended normally.
    Done { stop_reason: String },
    /// A mid-stream error detected by the parser (e.g. OAI `data: {"error":{...}}`,
    /// Anthropic `event: error`, Google `promptFeedback.blockReason` in first chunk).
    StreamError { error: AiError },
    /// Stream was truncated without a `[DONE]` sentinel.
    UnexpectedEof,
    /// A verbatim SSE event not classified into any other variant.
    Unknown { raw: String },
}
