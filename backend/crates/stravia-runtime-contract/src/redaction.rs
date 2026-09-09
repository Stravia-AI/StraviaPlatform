use crate::model_turn::ModelTurnError;
use crate::protocol::ir::{AiRequest, AiResponse};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum RedactionError {
    #[error("Reversible redaction detection failed")]
    Detection,
    #[error("Reversible redaction storage failed")]
    Storage,
    #[error("Reversible redaction text processing failed")]
    InvalidText,
    #[error("Cannot protect legacy tool-result history with unknown payload semantics")]
    AmbiguousToolResult,
}

impl From<RedactionError> for ModelTurnError {
    fn from(error: RedactionError) -> Self {
        Self::new("reversible_redaction_failed", error.to_string())
    }
}

/// 只保留 Provider 视图的引用和语义证明，不保留映射秘密。
#[derive(Clone, Debug, Default)]
pub struct RedactionTrace(Arc<Mutex<TraceState>>);

#[derive(Debug, Default)]
struct TraceState {
    references: BTreeSet<String>,
    provider: Option<ProviderProof>,
    tracking: bool,
}

#[derive(Clone, Debug)]
pub struct ProviderProof {
    pub context_hash: [u8; 32],
    pub context_messages: usize,
    pub controls_fingerprint: String,
}

impl RedactionTrace {
    pub fn set_tracking(&self, tracking: bool) -> Result<(), RedactionError> {
        self.0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .tracking = tracking;
        Ok(())
    }

    pub fn record(
        &self,
        references: impl IntoIterator<Item = String>,
    ) -> Result<(), RedactionError> {
        self.0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .references
            .extend(references);
        Ok(())
    }

    pub fn references(&self) -> Result<Vec<String>, RedactionError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .references
            .iter()
            .cloned()
            .collect())
    }

    pub fn provider_proof(&self) -> Result<Option<ProviderProof>, RedactionError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| RedactionError::InvalidText)?
            .provider
            .clone())
    }

    pub fn observe_provider_request(&self, request: &AiRequest) -> Result<(), RedactionError> {
        let mut state = self.0.lock().map_err(|_| RedactionError::InvalidText)?;
        if !state.tracking {
            state.provider = None;
            return Ok(());
        }
        let canonical = crate::protocol::ir::canonical::history_context_hash(&request.items);
        let controls = crate::protocol::ir::canonical::history_request_controls_hash(request);
        state.provider = Some(ProviderProof {
            context_hash: canonical,
            context_messages: request.items.len(),
            controls_fingerprint: crate::protocol::ir::canonical::hash_hex(&controls),
        });
        Ok(())
    }

    pub fn observe_provider_response(&self, response: &AiResponse) -> Result<(), RedactionError> {
        let mut state = self.0.lock().map_err(|_| RedactionError::InvalidText)?;
        if let Some(proof) = state.provider.as_mut() {
            for item in &response.items {
                proof.context_hash = crate::protocol::ir::canonical::append_history_context_hash(
                    &proof.context_hash,
                    item,
                );
            }
            proof.context_messages += response.items.len();
        }
        Ok(())
    }
}
