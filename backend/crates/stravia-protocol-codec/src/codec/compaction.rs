//! Native compaction control classification on a decoded canonical request.
//!
//! Moved out of `stravia-core::compaction`: the persisted store stays host-side
//! while request classification is pure wire semantics shared with plugins.

use stravia_runtime_contract::protocol::ir::ProtocolExt;
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest};

/// Whether a decoded request asks for native compaction.
///
/// Null and empty controls are inactive; an explicit trigger remains active.
pub fn native_compaction_requested(request: &AiRequest) -> bool {
    if request.items.iter().any(AiItem::is_compaction_trigger) {
        return true;
    }
    let control = match &request.ext {
        Some(ProtocolExt::OpenResponses(ext)) => ext.passthrough_body.get("context_management"),
        _ => None,
    };
    control.is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty))
}
