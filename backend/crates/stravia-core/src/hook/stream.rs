use crate::protocol::ir::AiStreamDelta;

#[derive(Debug, Clone)]
pub enum StreamDirective {
    Pass,
    Emit(Vec<AiStreamDelta>),
    Hold,
    Replace(Vec<AiStreamDelta>),
    Drop,
}

pub trait StreamTransformer: Send {
    fn begin(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn transform(&mut self, delta: &AiStreamDelta) -> Result<StreamDirective, String>;

    fn flush(&mut self) -> Result<Vec<AiStreamDelta>, String> {
        Ok(Vec::new())
    }

    fn close(&mut self) -> Result<Vec<AiStreamDelta>, String> {
        self.flush()
    }

    fn buffered_bytes(&self) -> usize {
        0
    }
}

pub(crate) fn is_semantic(delta: &AiStreamDelta) -> bool {
    matches!(
        delta,
        AiStreamDelta::TextDelta(_)
            | AiStreamDelta::TextDeltaWithMetadata { .. }
            | AiStreamDelta::RefusalDelta(_)
            | AiStreamDelta::RefusalDeltaWithIndex { .. }
            | AiStreamDelta::ThinkingDelta(_)
            | AiStreamDelta::ThinkingDeltaWithMetadata { .. }
            | AiStreamDelta::ReasoningSummaryDelta { .. }
            | AiStreamDelta::ToolCallDelta { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_delta_is_mutable_semantic_content() {
        assert!(is_semantic(&AiStreamDelta::RefusalDelta(
            "cannot comply".into()
        )));
    }

    #[test]
    fn indexed_reasoning_deltas_are_mutable_semantic_content() {
        assert!(is_semantic(&AiStreamDelta::ThinkingDeltaWithMetadata {
            text: "reasoning".into(),
            obfuscation: None,
            output_index: Some(2),
            content_index: Some(1),
        }));
        assert!(is_semantic(&AiStreamDelta::ReasoningSummaryDelta {
            text: "summary".into(),
            obfuscation: None,
            output_index: Some(2),
            content_index: Some(0),
        }));
    }
}
