use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use parking_lot::Mutex;

use crate::interaction_observation::{ConfirmedUsage, RunEvent, RunObserver};
use stravia_runtime_contract::protocol::ir::AiStreamDelta;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ObservedThinkingPart {
    Unindexed,
    Thinking {
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
    Summary {
        output_index: Option<usize>,
        content_index: Option<usize>,
    },
}

#[derive(Default)]
struct ObservedThinkingLayout {
    last_part: Option<ObservedThinkingPart>,
    has_text: bool,
}

impl ObservedThinkingLayout {
    fn text(&mut self, part: ObservedThinkingPart, text: &str) -> String {
        use ObservedThinkingPart::*;
        let boundary = self.has_text
            && match self.last_part {
                None => true,
                Some(Unindexed) if matches!(part, Thinking { .. }) => false,
                Some(Thinking { .. }) if part == Unindexed => false,
                Some(previous) => previous != part,
            };
        self.last_part = Some(part);
        self.has_text = true;
        let mut observed = String::with_capacity(text.len() + if boundary { 2 } else { 0 });
        if boundary {
            observed.push_str("\n\n");
        }
        observed.push_str(text);
        observed
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
    finished: AtomicBool,
    usage_confirmed: AtomicBool,
    thinking_active: AtomicBool,
    thinking_layout: Mutex<ObservedThinkingLayout>,
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
        Self {
            observer,
            id,
            model_turn_id: if observed {
                model_turn_id
            } else {
                String::new()
            },
            started_at: Instant::now(),
            finished: AtomicBool::new(false),
            usage_confirmed: AtomicBool::new(false),
            thinking_active: AtomicBool::new(false),
            thinking_layout: Mutex::new(ObservedThinkingLayout::default()),
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
        match delta {
            AiStreamDelta::ThinkingDelta(text)
            | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
            | AiStreamDelta::ReasoningSummaryDelta { text, .. }
                if !text.is_empty() =>
            {
                let part = match delta {
                    AiStreamDelta::ThinkingDelta(_) => ObservedThinkingPart::Unindexed,
                    AiStreamDelta::ThinkingDeltaWithMetadata {
                        output_index,
                        content_index,
                        ..
                    } => ObservedThinkingPart::Thinking {
                        output_index: *output_index,
                        content_index: *content_index,
                    },
                    AiStreamDelta::ReasoningSummaryDelta {
                        output_index,
                        content_index,
                        ..
                    } => ObservedThinkingPart::Summary {
                        output_index: *output_index,
                        content_index: *content_index,
                    },
                    _ => unreachable!(),
                };
                let mut layout = self.thinking_layout.lock();
                self.thinking_active.store(true, Ordering::Release);
                observer.record(RunEvent::ModelThinkingDelta {
                    model_turn_id: self.model_turn_id.clone(),
                    attempt_id: self.id.clone(),
                    text: layout.text(part, text),
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
    }

    fn finish_thinking(&self) {
        let mut layout = self.thinking_layout.lock();
        if !self.thinking_active.swap(false, Ordering::AcqRel) {
            return;
        }
        layout.last_part = None;
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

    pub(crate) fn confirm_usage(&self, usage: &stravia_runtime_contract::protocol::ir::Usage) {
        if self.usage_confirmed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::UsageConfirmed {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
                usage: confirmed_usage(usage),
            });
        }
    }

    pub(crate) fn finish(
        &self,
        status: &str,
        status_code: Option<u16>,
        error_code: Option<String>,
        first_token_ms: Option<i64>,
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
        self.finish_thinking();
        if let Some(observer) = &self.observer {
            observer.record(RunEvent::TargetAttemptFinished {
                model_turn_id: self.model_turn_id.clone(),
                attempt_id: self.id.clone(),
                status: status.to_owned(),
                status_code,
                error_code,
                duration_ms: self.started_at.elapsed().as_millis() as i64,
                first_token_ms,
            });
        }
    }
}

impl Drop for AttemptObservation {
    fn drop(&mut self) {
        let reason = if self
            .first_token_timed_out
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::Acquire))
        {
            "first_token_timeout"
        } else {
            "attempt_aborted"
        };
        self.finish("failed", None, Some(reason.into()), None);
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
