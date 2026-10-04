use super::{AiStreamDelta, AttemptFailure, VendorPublicationFence};

const PRECOMMIT_BUFFER_BUDGET: usize = 16 * 1024 * 1024;
// 包含事件槽及 Vec 初始/倍增预留空间；预算是保守占用估算，不是进程 RSS。
const PRECOMMIT_EVENT_OVERHEAD: usize =
    4 * std::mem::size_of::<(AiStreamDelta, VendorPublicationFence)>();

// 每个 JSON 对象条目的固定占用：键、值槽以及稀疏占用的 map 节点/索引，按 (String, Value)
// 的 4 倍保守估算。过大的值会让 schema 密集的 `tools` 回显（大量小键）在真实体积远低于
// 预算时被拒绝。
const JSON_OBJECT_ENTRY_OVERHEAD: usize = 4 * std::mem::size_of::<(String, serde_json::Value)>();

#[derive(Default)]
pub(super) struct PrecommitBuffer {
    events: Vec<(AiStreamDelta, VendorPublicationFence)>,
    bytes: usize,
}

impl PrecommitBuffer {
    /// A committing delta is accepted regardless of its size: it releases the
    /// buffered metadata instead of extending the precommit window.
    pub(super) fn push(
        &mut self,
        delta: AiStreamDelta,
        publication: VendorPublicationFence,
    ) -> Result<bool, AttemptFailure> {
        let commits = is_first_output(&delta) || is_terminal_delta(&delta);
        if !commits {
            let bytes = PRECOMMIT_EVENT_OVERHEAD.saturating_add(precommit_payload_bytes(&delta));
            if bytes > PRECOMMIT_BUFFER_BUDGET.saturating_sub(self.bytes) {
                return Err(AttemptFailure::terminal(
                    "vendor_event_limit_exceeded",
                    "Vendor pre-output event buffer exceeded its 16 MiB budget",
                ));
            }
            self.bytes += bytes;
        }
        self.events.push((delta, publication));
        Ok(commits)
    }

    pub(super) fn take(&mut self) -> Vec<(AiStreamDelta, VendorPublicationFence)> {
        self.bytes = 0;
        std::mem::take(&mut self.events)
    }
}

/// Count owned payloads without producing a second serialized copy. Fixed
/// event/fence storage is charged by `push`; capacity accounts for reserved but
/// unused String/Vec bytes. JSON objects reserve a conservative per-entry
/// allowance for their map nodes/index in addition to keys and child values.
fn precommit_payload_bytes(delta: &AiStreamDelta) -> usize {
    use AiStreamDelta as D;
    match delta {
        D::MessageStart { id, model } => id.capacity().saturating_add(model.capacity()),
        D::ResponseMetadata { metadata } => json_payload_bytes(metadata),
        D::TextDelta(text)
        | D::RefusalDelta(text)
        | D::ThinkingDelta(text)
        | D::ThinkingSignature(text) => text.capacity(),
        D::TextDeltaWithMetadata {
            text,
            logprobs,
            obfuscation,
            ..
        } => text
            .capacity()
            .saturating_add(
                logprobs
                    .capacity()
                    .saturating_mul(std::mem::size_of::<serde_json::Value>()),
            )
            .saturating_add(
                logprobs
                    .iter()
                    .map(json_payload_bytes)
                    .fold(0usize, usize::saturating_add),
            )
            .saturating_add(obfuscation.as_ref().map_or(0, String::capacity)),
        D::RefusalDeltaWithIndex { text, .. } => text.capacity(),
        D::ThinkingDeltaWithMetadata {
            text, obfuscation, ..
        }
        | D::ReasoningSummaryDelta {
            text, obfuscation, ..
        } => text
            .capacity()
            .saturating_add(obfuscation.as_ref().map_or(0, String::capacity)),
        D::Unknown { raw } => raw.capacity(),
        D::ProtectedThinkingStart { .. } | D::Usage(_) => 0,
        // These variants always commit, and are never subject to this budget.
        D::ToolCallStart { .. }
        | D::ToolCallDelta { .. }
        | D::ToolCallComplete { .. }
        | D::ItemDone { .. }
        | D::ResponseTerminal { .. }
        | D::Done { .. }
        | D::StreamError { .. }
        | D::UnexpectedEof => 0,
    }
}

fn json_payload_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.capacity(),
        // The workspace enables serde_json's arbitrary_precision feature: a
        // Number can own a large decimal String, not merely an inline float.
        serde_json::Value::Number(number) => {
            struct CountBytes(usize);
            impl std::io::Write for CountBytes {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.0 = self.0.saturating_add(bytes.len());
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut count = CountBytes(0);
            serde_json::to_writer(&mut count, number).expect("counting writer cannot fail");
            count.0.saturating_mul(2) // allow for String spare capacity
        }
        serde_json::Value::Array(values) => values
            .capacity()
            .saturating_mul(std::mem::size_of::<serde_json::Value>())
            .saturating_add(
                values
                    .iter()
                    .map(json_payload_bytes)
                    .fold(0usize, usize::saturating_add),
            ),
        serde_json::Value::Object(values) => values.iter().fold(0usize, |bytes, (key, value)| {
            bytes
                .saturating_add(JSON_OBJECT_ENTRY_OVERHEAD)
                .saturating_add(key.capacity())
                .saturating_add(json_payload_bytes(value))
        }),
        _ => 0,
    }
}

fn is_first_output(delta: &AiStreamDelta) -> bool {
    match delta {
        AiStreamDelta::TextDelta(text)
        | AiStreamDelta::RefusalDelta(text)
        | AiStreamDelta::ThinkingDelta(text) => !text.is_empty(),
        AiStreamDelta::TextDeltaWithMetadata { text, .. }
        | AiStreamDelta::RefusalDeltaWithIndex { text, .. }
        | AiStreamDelta::ThinkingDeltaWithMetadata { text, .. }
        | AiStreamDelta::ReasoningSummaryDelta { text, .. } => !text.is_empty(),
        AiStreamDelta::ToolCallStart { .. }
        | AiStreamDelta::ToolCallDelta { .. }
        | AiStreamDelta::ToolCallComplete { .. }
        | AiStreamDelta::ItemDone { .. } => true,
        _ => false,
    }
}

fn is_terminal_delta(delta: &AiStreamDelta) -> bool {
    matches!(
        delta,
        AiStreamDelta::StreamError { .. }
            | AiStreamDelta::UnexpectedEof
            | AiStreamDelta::Done { .. }
            | AiStreamDelta::ResponseTerminal { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::VendorOperationTracker;
    use std::time::Duration;
    use stravia_runtime_contract::Deadline;
    fn publication() -> VendorPublicationFence {
        let operation = VendorOperationTracker::default()
            .begin("buffer-test")
            .unwrap();
        operation.publication_fence(
            stravia_runtime_contract::CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(60)),
        )
    }

    #[test]
    fn metadata_beyond_32_events_preserves_order_and_publication_fences() {
        let first = publication();
        let second = publication();
        let mut buffer = PrecommitBuffer::default();
        for index in 0..70 {
            let fence = if index % 2 == 0 { &first } else { &second };
            assert!(
                !buffer
                    .push(
                        AiStreamDelta::ResponseMetadata {
                            metadata: serde_json::json!({"model": format!("model-{index}")}),
                        },
                        fence.clone(),
                    )
                    .unwrap_or_else(|failure| panic!("{}", failure.error.code))
            );
        }
        assert!(
            buffer
                .push(AiStreamDelta::TextDelta("answer".into()), first.clone())
                .unwrap_or_else(|failure| panic!("{}", failure.error.code))
        );
        let events = buffer.take();
        assert_eq!(events.len(), 71);
        for (index, (delta, fence)) in events.iter().take(70).enumerate() {
            let AiStreamDelta::ResponseMetadata { metadata } = delta else {
                panic!("metadata event out of order");
            };
            assert_eq!(metadata["model"], format!("model-{index}"));
            assert!(fence.same_activity(if index % 2 == 0 { &first } else { &second }));
        }
        assert!(matches!(&events[70].0, AiStreamDelta::TextDelta(text) if text == "answer"));
        assert!(events[70].1.same_activity(&first));
    }

    #[test]
    fn precommit_budget_accepts_exact_boundary_and_rejects_one_byte_more() {
        let fixed = super::PRECOMMIT_EVENT_OVERHEAD;
        let payload = PRECOMMIT_BUFFER_BUDGET - fixed;
        let mut buffer = PrecommitBuffer::default();
        let mut raw = String::with_capacity(payload);
        raw.push('x');
        assert_eq!(raw.capacity(), payload);
        assert!(
            !buffer
                .push(AiStreamDelta::Unknown { raw }, publication())
                .unwrap_or_else(|failure| panic!("{}", failure.error.code))
        );
        let failure = buffer
            .push(
                AiStreamDelta::ProtectedThinkingStart { index: 0 },
                publication(),
            )
            .unwrap_err();
        assert_eq!(failure.error.code, "vendor_event_limit_exceeded");
        assert_eq!(buffer.take().len(), 1);

        let mut oversized = PrecommitBuffer::default();
        let mut raw = String::with_capacity(payload + 1);
        raw.push('x');
        assert_eq!(raw.capacity(), payload + 1);
        assert_eq!(
            oversized
                .push(AiStreamDelta::Unknown { raw }, publication())
                .unwrap_err()
                .error
                .code,
            "vendor_event_limit_exceeded"
        );
        assert!(oversized.take().is_empty());
    }

    #[test]
    fn nested_json_and_logprobs_consume_the_precommit_budget() {
        let values = vec![serde_json::Value::Null; PRECOMMIT_BUFFER_BUDGET / 16];
        let mut buffer = PrecommitBuffer::default();
        assert_eq!(
            buffer
                .push(
                    AiStreamDelta::ResponseMetadata {
                        metadata: serde_json::Value::Array(values),
                    },
                    publication(),
                )
                .unwrap_err()
                .error
                .code,
            "vendor_event_limit_exceeded"
        );
        assert!(buffer.take().is_empty());

        assert_eq!(
            buffer
                .push(
                    AiStreamDelta::ResponseMetadata {
                        metadata: serde_json::json!({
                            "payload": "x".repeat(PRECOMMIT_BUFFER_BUDGET)
                        }),
                    },
                    publication(),
                )
                .unwrap_err()
                .error
                .code,
            "vendor_event_limit_exceeded"
        );

        let mut buffer = PrecommitBuffer::default();
        assert_eq!(
            buffer
                .push(
                    AiStreamDelta::TextDeltaWithMetadata {
                        text: String::new(),
                        logprobs: vec![serde_json::Value::Null; PRECOMMIT_BUFFER_BUDGET / 16],
                        obfuscation: None,
                        output_index: None,
                        content_index: None,
                    },
                    publication(),
                )
                .unwrap_err()
                .error
                .code,
            "vendor_event_limit_exceeded"
        );
    }

    #[test]
    fn schema_dense_response_metadata_stays_within_the_precommit_budget() {
        // Codex echoes the full `tools` schema in response.created. A schema is
        // key-dense but small; realistic tool collections must fit even when
        // their conservative storage estimate exceeds the former 1 MiB budget.
        let properties: serde_json::Map<String, serde_json::Value> = (0..2000)
            .map(|index| {
                (
                    format!("property_{index}"),
                    serde_json::json!({"type": "string", "description": "x".repeat(40)}),
                )
            })
            .collect();
        let metadata = serde_json::json!({
            "tools": [{"type": "function", "name": "tool", "parameters": {
                "type": "object",
                "properties": properties,
            }}]
        });
        let mut buffer = PrecommitBuffer::default();
        assert!(
            !buffer
                .push(
                    AiStreamDelta::ResponseMetadata {
                        metadata: metadata.clone(),
                    },
                    publication(),
                )
                .unwrap_or_else(|failure| panic!("{}", failure.error.code))
        );
        assert!(
            buffer
                .push(AiStreamDelta::TextDelta("answer".into()), publication())
                .unwrap_or_else(|failure| panic!("{}", failure.error.code))
        );
        let events = buffer.take();
        assert!(
            matches!(&events[0].0, AiStreamDelta::ResponseMetadata { metadata: actual } if actual == &metadata)
        );
        assert!(matches!(&events[1].0, AiStreamDelta::TextDelta(text) if text == "answer"));
    }

    #[test]
    fn first_output_and_normal_terminal_commit_even_with_a_full_buffer() {
        for delta in [
            AiStreamDelta::TextDelta("answer".repeat(PRECOMMIT_BUFFER_BUDGET)),
            AiStreamDelta::Done {
                stop_reason: "done".repeat(PRECOMMIT_BUFFER_BUDGET),
            },
        ] {
            let mut buffer = PrecommitBuffer::default();
            let fixed = super::PRECOMMIT_EVENT_OVERHEAD;
            let mut raw = String::with_capacity(PRECOMMIT_BUFFER_BUDGET - fixed);
            raw.push('x');
            assert!(
                !buffer
                    .push(AiStreamDelta::Unknown { raw }, publication())
                    .unwrap_or_else(|failure| panic!("{}", failure.error.code))
            );
            assert!(
                buffer
                    .push(delta, publication())
                    .unwrap_or_else(|failure| panic!("{}", failure.error.code))
            );
            let events = buffer.take();
            assert_eq!(events.len(), 2);
            assert!(matches!(&events[0].0, AiStreamDelta::Unknown { .. }));
            assert!(
                matches!(&events[1].0, AiStreamDelta::TextDelta(text) if text.starts_with("answer"))
                    || matches!(&events[1].0, AiStreamDelta::Done { stop_reason } if stop_reason.starts_with("done"))
            );
        }
    }
}
