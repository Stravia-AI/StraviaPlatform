//! The engine-owned per-run state, collected into one explicit ledger.
//!
//! `RunLedger` replaces the ad-hoc `RequestContext::extensions` entries the
//! engine used to stash for later phases: hidden-round accumulation, published
//! Platform execution bookkeeping, and the terminal delivery context are all
//! constructed once at admission and flow through the dispatch pipeline as a
//! typed handle. `RunObserver` deliberately stays out of the ledger — its
//! scope is task-local (WebSocket ingress rebinds it) — and keeps travelling
//! in the request extensions. The ledger itself rides the same extensions
//! once, from admission to `execute_observed`: that boundary's signature is
//! fixed by `execute(RunInput)`, so the typemap carries the ledger across
//! while every engine function takes it as a parameter.

use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::{AiItem, AiResponse, Usage};

use super::super::{RunTerminalContext, StreamDeliveryCompletion};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

/// Hidden Model Round output retained for the final visible response: the
/// client-shaped items that precede the last leg, and the usage the hidden
/// rounds consumed.
#[derive(Clone, Default)]
pub(super) struct HiddenRoundState {
    pub(super) items: Vec<AiItem>,
    pub(super) usage: Usage,
    pub(super) round_count: u32,
}

/// Platform executions whose History Markers the client has already seen.
#[derive(Clone, Default)]
pub(super) struct PublishedPlatformExecutions {
    pub(super) references: Vec<String>,
}

pub(super) fn retain_hidden_round_item(item: &AiItem) -> bool {
    item.is_compaction()
        || item.output_text_ref().is_some()
        || item.thinking_ref().is_some()
        || item.reasoning_ref().is_some()
}

/// Shared run state for one Inference Run.
///
/// Cloning shares the underlying cells — the ledger is a handle, not a value.
/// Constructing it is the single point where a malformed run fails; consumers
/// hold a `RunLedger` instead of re-resolving extension entries.
#[derive(Clone)]
pub(in crate::proxy::dispatcher::inference_run) struct RunLedger {
    pub(in crate::proxy::dispatcher::inference_run) terminal: RunTerminalContext,
    pub(super) compaction_records: crate::model_turn::CompactionPublications,
    hidden_rounds: Arc<Mutex<HiddenRoundState>>,
    published: Arc<Mutex<PublishedPlatformExecutions>>,
    stream_completion: Arc<Mutex<Option<StreamDeliveryCompletion>>>,
}

impl RunLedger {
    pub(super) fn new(
        terminal: RunTerminalContext,
        compaction_records: crate::model_turn::CompactionPublications,
    ) -> Self {
        Self {
            terminal,
            compaction_records,
            hidden_rounds: Arc::new(Mutex::new(HiddenRoundState::default())),
            published: Arc::new(Mutex::new(PublishedPlatformExecutions::default())),
            stream_completion: Arc::new(Mutex::new(None)),
        }
    }

    /// Stage the response the client will see as the run's observed output and
    /// queue its visible text for the first committed body chunk.
    pub(super) fn stage_visible_response(&self, ingress: ProtocolId, response: &AiResponse) {
        self.terminal.stage_client_output(ingress, response);
        self.terminal.extend_visible_text(
            response
                .items
                .iter()
                .filter_map(|item| item.output_text_ref().or_else(|| item.refusal_ref()))
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned),
        );
    }

    /// Retain a hidden round's client-shaped items and usage for the final
    /// visible response.
    pub(super) fn record_hidden_round(&self, response: &AiResponse) {
        let mut state = lock(&self.hidden_rounds);
        state.items.extend(
            response
                .items
                .iter()
                .filter(|item| retain_hidden_round_item(item))
                .cloned(),
        );
        if state.round_count == 0 {
            state.usage = response.usage.clone();
        } else {
            add_usage(&mut state.usage, &response.usage);
        }
        state.round_count = state.round_count.saturating_add(1);
    }

    /// Prepend every retained hidden round and merge their usage into the
    /// visible response about to complete.
    pub(super) fn apply_hidden_rounds(&self, response: &mut AiResponse) {
        let state = lock(&self.hidden_rounds);
        if !state.items.is_empty() {
            response.items.splice(0..0, state.items.iter().cloned());
        }
        add_usage(&mut response.usage, &state.usage);
    }

    pub(super) fn record_published_executions(&self, references: impl IntoIterator<Item = String>) {
        lock(&self.published).references.extend(references);
    }

    pub(super) fn published_executions(&self) -> Vec<String> {
        lock(&self.published).references.clone()
    }

    pub(super) fn set_stream_completion(&self, completion: StreamDeliveryCompletion) {
        *lock(&self.stream_completion) = Some(completion);
    }

    pub(in crate::proxy::dispatcher::inference_run) fn take_stream_completion(
        &self,
    ) -> Option<StreamDeliveryCompletion> {
        lock(&self.stream_completion).take()
    }
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
    total.required_components_known =
        total.required_components_known && usage.required_components_known;
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cache_read_tokens = sum_optional(total.cache_read_tokens, usage.cache_read_tokens);
    total.cache_creation_tokens =
        sum_optional(total.cache_creation_tokens, usage.cache_creation_tokens);
    total.reasoning_tokens = sum_optional(total.reasoning_tokens, usage.reasoning_tokens);
    match (&mut total.server_tool_use, &usage.server_tool_use) {
        (Some(total), Some(usage)) => {
            total.web_search_requests = total
                .web_search_requests
                .saturating_add(usage.web_search_requests);
            total.web_fetch_requests = total
                .web_fetch_requests
                .saturating_add(usage.web_fetch_requests);
        }
        (None, Some(usage)) => total.server_tool_use = Some(usage.clone()),
        _ => {}
    }
}

fn sum_optional(left: Option<u32>, right: Option<u32>) -> Option<u32> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_round_usage_accumulates_reasoning_tokens() {
        let mut total = Usage {
            reasoning_tokens: Some(3),
            required_components_known: true,
            ..Usage::default()
        };
        add_usage(
            &mut total,
            &Usage {
                reasoning_tokens: Some(4),
                required_components_known: true,
                ..Usage::default()
            },
        );

        assert_eq!(total.reasoning_tokens, Some(7));
    }

    #[test]
    fn hidden_rounds_retain_typed_reasoning_items() {
        let item = stravia_runtime_contract::protocol::ir::AiItem::reasoning(
            vec!["summary".into()],
            vec!["content".into()],
            Some("opaque".into()),
        );

        assert!(retain_hidden_round_item(&item));
    }
}
