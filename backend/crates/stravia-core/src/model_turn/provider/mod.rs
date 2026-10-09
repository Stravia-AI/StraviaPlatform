use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use parking_lot::Mutex;

use crate::interaction_observation::{
    ConfirmedUsage, FailureDiagnostic, RunEvent, RunObserver, canonical_item_block_id,
};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiResponse, AiStreamDelta, ContentBlock, MessageContent,
};

#[derive(Default)]
struct ObservedThinkingItems {
    accumulator: StreamResponseAccumulator,
    emitted: std::collections::HashSet<usize>,
    emitted_ids: std::collections::HashSet<String>,
}

impl ObservedThinkingItems {
    fn record(
        &mut self,
        observation: &AttemptObservation,
        index: usize,
        item: &AiItem,
        complete: bool,
    ) {
        let MessageContent::Blocks(blocks) = &item.content else {
            return;
        };
        if self.emitted.contains(&index)
            || item
                .id_ref()
                .is_some_and(|id| self.emitted_ids.contains(id))
            || !blocks.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::Reasoning { .. }
                )
            })
        {
            return;
        }
        let mut diagnostic_item = item.clone();
        if let MessageContent::Blocks(blocks) = &mut diagnostic_item.content {
            blocks.retain(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::Reasoning { .. }
                )
            });
            for block in blocks {
                match block {
                    ContentBlock::Thinking { signature, .. } => *signature = None,
                    ContentBlock::Reasoning {
                        encrypted_content, ..
                    } => *encrypted_content = None,
                    _ => {}
                }
            }
        }
        let MessageContent::Blocks(blocks) = &diagnostic_item.content else {
            return;
        };
        self.emitted.insert(index);
        if let Some(id) = item.id_ref() {
            self.emitted_ids.insert(id.to_owned());
        }
        let text = blocks
            .iter()
            .flat_map(|block| match block {
                ContentBlock::Thinking { thinking, .. } => vec![thinking.as_str()],
                ContentBlock::Reasoning {
                    summary, content, ..
                } => summary
                    .iter()
                    .chain(content.iter())
                    .map(String::as_str)
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let parts = blocks
            .iter()
            .map(|block| {
                serde_json::to_value(block).expect("canonical thinking part serialization")
            })
            .collect();
        let mut diagnostic_item =
            serde_json::to_value(&diagnostic_item).expect("canonical thinking item serialization");
        retain_diagnostic_metadata(&mut diagnostic_item);
        if let Some(observer) = &observation.observer {
            observer.record(RunEvent::ModelThinking {
                model_turn_id: observation.model_turn_id.clone(),
                attempt_id: observation.id.clone(),
                text,
                parts,
                block_id: canonical_item_block_id(&observation.id, index),
                item: diagnostic_item,
                complete,
            });
        }
    }
}

fn retain_diagnostic_metadata(item: &mut serde_json::Value) {
    if let Some(meta) = item.get_mut("meta") {
        if let Some(fields) = meta.as_object_mut() {
            // Only typed canonical graph metadata belongs in diagnostic rows;
            // extension bags can contain lossless protected wire snapshots.
            fields.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "id" | "__open_responses_item_reference" | "status" | "provenance" | "audience"
                )
            });
        } else {
            *meta = serde_json::Value::Null;
        }
    }
}

/// Observation state for one Wasm Vendor operation.
///
/// Wire capture belongs to the controlled host transport. Model Turn observes
/// only canonical content, timing, terminal status, and confirmed usage.
pub(crate) struct AttemptObservation {
    observer: Option<RunObserver>,
    pub(crate) id: String,
    model_turn_id: String,
    started_at: Instant,
    duration_span: Mutex<Option<tracing::Span>>,
    first_token_span: Mutex<Option<tracing::Span>>,
    finished: AtomicBool,
    usage_confirmed: AtomicBool,
    confirmed_usage: Mutex<Option<ConfirmedUsage>>,
    thinking_active: AtomicBool,
    thinking_items: Mutex<ObservedThinkingItems>,
    first_token_timed_out: Option<Arc<AtomicBool>>,
}

impl AttemptObservation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        observer: Option<RunObserver>,
        model_turn_id: String,
        target_id: String,
        provider_id: String,
        provider_name: String,
        actual_model: String,
        protocol: String,
        upstream_base_url: String,
        first_token_timed_out: Option<Arc<AtomicBool>>,
    ) -> Self {
        let id = observer
            .as_ref()
            .map(|_| stravia_runtime_contract::identifier::new_id())
            .unwrap_or_default();
        if let Some(observer) = &observer {
            observer.record(RunEvent::TargetAttemptStarted {
                model_turn_id: model_turn_id.clone(),
                attempt_id: id.clone(),
                target_id,
                provider_id,
                provider_name,
                upstream_model: actual_model,
                protocol,
                upstream_url: upstream_base_url,
            });
        }
        let observed = observer.is_some();
        let duration_span = tracing::info_span!(target: "stravia::perf", "model_turn.attempt.duration", status = tracing::field::Empty);
        let first_token_span = tracing::info_span!(target: "stravia::perf", parent: &duration_span, "model_turn.attempt.first_token", status = tracing::field::Empty);
        Self {
            observer,
            id,
            model_turn_id: if observed {
                model_turn_id
            } else {
                String::new()
            },
            started_at: Instant::now(),
            duration_span: Mutex::new(Some(duration_span)),
            first_token_span: Mutex::new(Some(first_token_span)),
            finished: AtomicBool::new(false),
            usage_confirmed: AtomicBool::new(false),
            confirmed_usage: Mutex::new(None),
            thinking_active: AtomicBool::new(false),
            thinking_items: Mutex::new(ObservedThinkingItems::default()),
            first_token_timed_out,
        }
    }

    pub(crate) fn observe_delta(&self, delta: &AiStreamDelta) {
        let Some(observer) = &self.observer else {
            return;
        };
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let identity = self
            .thinking_items
            .lock()
            .accumulator
            .apply_with_identity(delta);
        match delta {
            AiStreamDelta::ThinkingDelta(text)
            | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
            | AiStreamDelta::ReasoningSummaryDelta { text, .. }
                if !text.is_empty() =>
            {
                self.thinking_active.store(true, Ordering::Release);
                observer.record(RunEvent::ModelThinkingDelta {
                    model_turn_id: self.model_turn_id.clone(),
                    attempt_id: self.id.clone(),
                    item_ordinal: identity.expect("canonical thinking identity").0,
                    part_index: identity.expect("canonical thinking identity").1,
                    text: text.clone(),
                });
            }
            AiStreamDelta::TextDelta(text)
            | AiStreamDelta::TextDeltaWithMetadata { text, .. }
            | AiStreamDelta::RefusalDelta(text)
            | AiStreamDelta::RefusalDeltaWithIndex { text, .. }
                if !text.is_empty() =>
            {
                self.finish_thinking();
            }
            AiStreamDelta::ToolCallStart { .. }
            | AiStreamDelta::ToolCallDelta { .. }
            | AiStreamDelta::ToolCallComplete { .. }
            | AiStreamDelta::Done { .. }
            | AiStreamDelta::StreamError { .. }
            | AiStreamDelta::UnexpectedEof => self.finish_thinking(),
            _ => {}
        }
        if let AiStreamDelta::ItemDone { item, .. } = delta
            && let Some((ordinal, _)) = identity
        {
            self.thinking_items.lock().record(self, ordinal, item, true);
        }
    }

    pub(crate) fn observe_response(&self, response: &AiResponse) {
        if self.observer.is_none() || self.finished.load(Ordering::Acquire) {
            return;
        }
        let mut items = self.thinking_items.lock();
        let (_, ordinals) = std::mem::take(&mut items.accumulator).into_ai_response_with_ordinals();
        for (index, item) in response.items.iter().enumerate() {
            let ordinal = ordinals.get(index).copied().unwrap_or(index);
            items.record(self, ordinal, item, true);
        }
    }

    fn finish_thinking(&self) {
        if !self.thinking_active.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::ModelThinkingFinished {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
            });
        }
    }

    pub(crate) fn elapsed_ms(&self) -> i64 {
        self.started_at.elapsed().as_millis() as i64
    }

    pub(crate) fn span(&self) -> tracing::Span {
        self.duration_span
            .lock()
            .as_ref()
            .expect("active Vendor attempt span")
            .clone()
    }

    pub(crate) fn record_first_token(&self) {
        if let Some(span) = self.first_token_span.lock().take() {
            span.record("status", "completed");
        }
    }

    pub(crate) fn confirm_usage(&self, usage: &stravia_runtime_contract::protocol::ir::Usage) {
        if self.usage_confirmed.swap(true, Ordering::AcqRel) {
            return;
        }
        let usage = confirmed_usage(usage);
        *self.confirmed_usage.lock() = Some(usage.clone());
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::UsageConfirmed {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
                usage,
            });
        }
    }

    pub(crate) fn finish(
        &self,
        status: &str,
        status_code: Option<u16>,
        error_code: Option<String>,
        first_token_ms: Option<i64>,
        diagnostic: Option<&FailureDiagnostic>,
    ) {
        let error_code = if status != "completed"
            && self
                .first_token_timed_out
                .as_ref()
                .is_some_and(|signal| signal.load(Ordering::Acquire))
        {
            Some("first_token_timeout".into())
        } else {
            error_code
        };
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(span) = self.duration_span.lock().take() {
            span.record(
                "status",
                if status == "completed" {
                    "completed"
                } else {
                    "error"
                },
            );
        }
        if let Some(span) = self.first_token_span.lock().take() {
            span.record("status", "error");
        }
        self.finish_thinking();
        if self.observer.is_some() {
            let mut items = self.thinking_items.lock();
            let usage = &items.accumulator.usage;
            if !self.usage_confirmed.load(Ordering::Acquire)
                && (usage.required_components_known
                    || usage.cache_read_tokens.is_some()
                    || usage.cache_creation_tokens.is_some()
                    || usage.reasoning_tokens.is_some())
            {
                self.confirm_usage(usage);
            }
            let (response, ordinals) =
                std::mem::take(&mut items.accumulator).into_ai_response_with_ordinals();
            for (item, ordinal) in response.items.iter().zip(ordinals) {
                items.record(self, ordinal, item, status == "completed");
            }
        }
        if let Some(observer) = &self.observer {
            let error = diagnostic.map(|diagnostic| FailureDiagnostic {
                source: diagnostic.source.clone(),
                code: error_code.clone(),
                message: diagnostic.message.clone(),
                status_code,
                upstream_code: diagnostic.upstream_code.clone(),
            });
            observer.record(RunEvent::TargetAttemptFinished {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
                status: status.to_owned(),
                status_code,
                error_code,
                error,
                duration_ms: self.started_at.elapsed().as_millis() as i64,
                first_token_ms,
                usage: self.confirmed_usage.lock().clone(),
            });
        }
    }
}

impl Drop for AttemptObservation {
    fn drop(&mut self) {
        // 尚未终结的尝试不得计作成功；缺失首 token 同样不得计作成功 TTFT。
        if let Some(span) = self.duration_span.get_mut().take() {
            span.record("status", "abandoned");
        }
        if let Some(span) = self.first_token_span.get_mut().take() {
            span.record("status", "abandoned");
        }
        let reason = if self
            .first_token_timed_out
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::Acquire))
        {
            "first_token_timeout"
        } else {
            "attempt_aborted"
        };
        self.finish("failed", None, Some(reason.into()), None, None);
    }
}

fn confirmed_usage(usage: &stravia_runtime_contract::protocol::ir::Usage) -> ConfirmedUsage {
    ConfirmedUsage {
        input_tokens: usage
            .required_components_known
            .then_some(i64::from(usage.prompt_tokens)),
        output_tokens: usage
            .required_components_known
            .then_some(i64::from(usage.completion_tokens)),
        cache_read_tokens: usage.cache_read_tokens.map(i64::from),
        cache_write_tokens: usage.cache_creation_tokens.map(i64::from),
        reasoning_tokens: usage.reasoning_tokens.map(i64::from),
        coverage: None,
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::{AttemptObservation, retain_diagnostic_metadata};
    use crate::interaction_observation::{
        AdmissionFacts, IngressStart, InteractionObservation, RunEvent, RunStart,
    };
    use std::sync::Arc;
    use stravia_runtime_contract::protocol::ir::{AiItemMetadata, AiRequest, AiStreamDelta, Usage};

    #[tokio::test]
    async fn terminal_attempt_retains_reported_usage_without_inventing_unknown_counts()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            1,
            false,
            crate::generation_chain::test_chain().await,
            Some(Arc::new(tokio::sync::Mutex::new(()))),
        )
        .await;
        let reported = Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            required_components_known: true,
            ..Default::default()
        };
        let revised = Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            required_components_known: true,
            ..Default::default()
        };
        let cases = [
            (vec![Usage::default()], None, (None, None, None)),
            (
                vec![reported.clone(), Usage::default()],
                None,
                (Some(7), Some(3), None),
            ),
            (
                vec![
                    reported.clone(),
                    Usage {
                        required_components_known: true,
                        ..Default::default()
                    },
                ],
                None,
                (Some(0), Some(0), None),
            ),
            (
                vec![reported.clone(), revised.clone()],
                None,
                (Some(10), Some(4), None),
            ),
            (
                vec![Usage {
                    cache_read_tokens: Some(0),
                    ..Default::default()
                }],
                None,
                (None, None, Some(0)),
            ),
            (vec![reported], Some(revised), (Some(10), Some(4), None)),
        ];
        for (index, (deltas, terminal_usage, expected)) in cases.into_iter().enumerate() {
            let id = format!("terminal-usage-{index}");
            let run = observation
                .observe_ingress(IngressStart {
                    id: id.clone(),
                    method: "POST".into(),
                    path: "/v1/responses".into(),
                    protocol: "responses".into(),
                })
                .admit(
                    RunStart {
                        id: id.clone(),
                        principal: "owner".into(),
                        api_key_id: None,
                        api_key_name: None,
                        route_id: "route".into(),
                        model_display_name: None,
                        ingress_protocol: "responses".into(),
                    },
                    AdmissionFacts {
                        client_request: AiRequest::new("model", Vec::new()).into(),
                        has_new_user: true,
                        has_matching_pending_tool_result: false,
                        generation_root_id: None,
                        generation_parent_id: None,
                    },
                );
            run.record(RunEvent::ModelTurnStarted {
                model_turn_id: id.clone(),
                route_id: "route".into(),
                model_display_name: None,
                estimated_input_tokens: None,
            });
            let attempt = AttemptObservation::new(
                Some(run),
                id,
                "target".into(),
                "provider".into(),
                "Provider".into(),
                "model".into(),
                "responses".into(),
                "http://localhost".into(),
                None,
            );
            for usage in deltas {
                attempt.observe_delta(&AiStreamDelta::Usage(usage));
            }
            let status = if let Some(usage) = terminal_usage {
                attempt.confirm_usage(&usage);
                "completed"
            } else {
                "failed"
            };
            attempt.finish(status, None, None, None, None);
            attempt.finish("completed", None, None, None, None);
            observation.flush().await?;
            let row: (String, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
                "SELECT status,input_tokens,output_tokens,cache_read_tokens FROM target_attempt_observations WHERE id=?",
            )
            .bind(&attempt.id)
            .fetch_one(&pool)
            .await?;
            assert_eq!(row.0, status, "case {index}");
            assert_eq!((row.1, row.2, row.3), expected, "case {index}");
        }
        observation.shutdown().await;
        Ok(())
    }

    #[test]
    fn diagnostic_metadata_retains_reference_without_wire_state() -> anyhow::Result<()> {
        let metadata = AiItemMetadata::from(serde_json::json!({
            "id": "reasoning-id",
            "__open_responses_item_reference": "reasoning-reference",
            "status": "completed",
            "__open_responses_item": {"encrypted_content": "PROTECTED_WIRE_STATE"},
            "vendor_private": "PRIVATE_EXTENSION",
            "reference": {"encrypted_content": "UNKNOWN_REFERENCE_EXTENSION"},
        }));
        let mut item = serde_json::json!({"meta": serde_json::to_value(metadata)?});
        retain_diagnostic_metadata(&mut item);
        assert_eq!(
            item["meta"]["__open_responses_item_reference"],
            "reasoning-reference"
        );
        assert_eq!(item["meta"]["status"], "completed");
        let diagnostic = serde_json::to_string(&item)?;
        for private in [
            "PROTECTED_WIRE_STATE",
            "PRIVATE_EXTENSION",
            "UNKNOWN_REFERENCE_EXTENSION",
        ] {
            assert!(!diagnostic.contains(private));
        }
        Ok(())
    }
}
