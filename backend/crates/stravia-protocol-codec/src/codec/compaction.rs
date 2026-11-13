//! Native Responses compaction control classification and preparation.

use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, AiResponse, ContentBlock, MessageContent, NativeCompactionResponse, Role,
};

mod responses;
pub use responses::parse_responses_compaction_stream;

/// Default instructions for a standalone compaction task.
pub const DEFAULT_COMPACTION_INSTRUCTIONS: &str = concat!(
    "Summarize the conversation above as a concise, structured handoff for another assistant ",
    "to continue the task. Do not continue the conversation or answer questions in it.\n\n",
    "Preserve the user's goals, constraints, preferences, decisions, completed work, current ",
    "work, blockers, and next steps. Preserve unanswered questions and pending requests exactly. ",
    "Keep essential file paths, identifiers, error messages, command results, and repository ",
    "changes. Distinguish observed facts from assumptions.\n\n",
    "Use the sections Goal, Constraints, Progress, Decisions, Next Steps, and Critical Context ",
    "where applicable. The summary replaces the supplied history: anything omitted will be ",
    "lost. Do not call tools. Return only the summary.",
);

/// Project a native compaction response into its deliverable IR.
pub fn compaction_response_ir(
    response: NativeCompactionResponse,
    model: &str,
) -> anyhow::Result<AiResponse> {
    let id = response
        .wire
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Compaction response has no identity"))?;
    let mut canonical = AiResponse::new(id, model);
    let mut items = response.items;
    items.retain(AiItem::is_compaction);
    anyhow::ensure!(!items.is_empty(), "Compaction response has no native state");
    anyhow::ensure!(
        items.len() == 1,
        "Compaction response has multiple native states"
    );
    canonical.items = items;
    canonical.stop_reason = Some("compaction".into());
    if let Some(usage) = response.usage {
        canonical.usage = usage;
    }
    if let Some(created_at) = response.wire.get("created_at") {
        canonical.vendor.ingress.insert(
            "__open_responses_effective_request".into(),
            json!({"created_at": created_at}),
        );
    }
    Ok(canonical)
}

/// Prepare a standalone compaction request without replacing session instructions.
pub fn prepare_compaction_request(request: &mut AiRequest) {
    if request.instructions.is_none() {
        request.instructions = Some(DEFAULT_COMPACTION_INSTRUCTIONS.into());
    }
    if !standalone_compaction_requested(request) {
        request.items.push(AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::CompactionTrigger {}]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
    }
}

/// Prepare a Responses V2 compaction request.
pub fn prepare_responses_compaction_request(request: &mut AiRequest) {
    request.parameters.compaction.take();
    prepare_compaction_request(request);
    request.stream.enabled = true;
    request.parameters.store = Some(false);
}

pub fn standalone_compaction_requested(request: &AiRequest) -> bool {
    request.items.iter().any(AiItem::is_compaction_trigger)
}

/// Whether a decoded request asks for native compaction.
pub fn native_compaction_requested(request: &AiRequest) -> bool {
    standalone_compaction_requested(request) || request.parameters.compaction.is_some()
}
