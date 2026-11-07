use async_trait::async_trait;
use std::sync::Arc;

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::{AiRequest, Continuation};

pub(crate) use crate::generation_chain::GenerationParentTarget;

pub struct ContinuationTarget<'a> {
    pub namespace: &'a str,
    pub protocol: Option<ProtocolId>,
    pub actual_model: &'a str,
    pub allow_ephemeral_response: bool,
}

#[async_trait]
pub(crate) trait ContinuationLookup: Send + Sync {
    /// Resolve affinity when reusable validated parent evidence is unavailable.
    /// Resolved parents travel separately from client-writable IR.
    async fn preferred_target(
        &self,
        _principal: &Principal,
        _request: &AiRequest,
    ) -> Option<String> {
        None
    }

    /// Retain full replay only after eligibility succeeds, before rewriting the
    /// provider request. A miss leaves `full_fallback` empty. When a fallback
    /// is retained, request items may only be changed by removing a prefix;
    /// callers project trusted canonical item gaps through that suffix cut.
    async fn prepare(
        &self,
        principal: &Principal,
        target: ContinuationTarget<'_>,
        parent: &GenerationParentTarget,
        request: &mut Arc<AiRequest>,
        full_fallback: &mut Option<Arc<AiRequest>>,
    ) -> Option<String>;
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct ScriptedContinuation {
    previous_response_id: Option<String>,
}

#[cfg(test)]
impl ScriptedContinuation {
    pub fn hit(previous_response_id: impl Into<String>) -> Arc<dyn ContinuationLookup> {
        Arc::new(Self {
            previous_response_id: Some(previous_response_id.into()),
        })
    }

    pub fn miss() -> Arc<dyn ContinuationLookup> {
        Arc::new(Self {
            previous_response_id: None,
        })
    }
}

#[cfg(test)]
#[async_trait]
impl ContinuationLookup for ScriptedContinuation {
    async fn prepare(
        &self,
        _principal: &Principal,
        _target: ContinuationTarget<'_>,
        _parent: &GenerationParentTarget,
        request: &mut Arc<AiRequest>,
        full_fallback: &mut Option<Arc<AiRequest>>,
    ) -> Option<String> {
        *full_fallback = None;
        match &self.previous_response_id {
            Some(previous_response_id) => {
                let mut fallback = request.clone();
                if parent_id_from_request(&fallback).is_some() {
                    clear_previous_response_id(Arc::make_mut(&mut fallback));
                }
                *full_fallback = Some(fallback);
                stamp_previous_response_id(Arc::make_mut(request), previous_response_id);
                Some(previous_response_id.clone())
            }
            None => {
                if parent_id_from_request(request).is_some() {
                    clear_previous_response_id(Arc::make_mut(request));
                }
                None
            }
        }
    }
}

pub(crate) fn parent_id_from_request(request: &AiRequest) -> Option<String> {
    request
        .parameters
        .continuation
        .as_ref()
        .and_then(Continuation::previous_response_id)
        .map(str::to_owned)
}

pub(crate) fn stamp_previous_response_id(request: &mut AiRequest, parent_id: &str) {
    request.parameters.continuation = Some(Continuation::PreviousResponse(parent_id.to_owned()));
}

pub(crate) fn clear_previous_response_id(request: &mut AiRequest) {
    if matches!(
        request.parameters.continuation,
        Some(Continuation::PreviousResponse(_))
    ) {
        request.parameters.continuation = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_response_helpers_stamp_and_clear_only_response_continuations() {
        let mut request = AiRequest::new("model", Vec::new());
        stamp_previous_response_id(&mut request, "response");
        assert_eq!(
            parent_id_from_request(&request).as_deref(),
            Some("response")
        );
        clear_previous_response_id(&mut request);
        assert!(request.parameters.continuation.is_none());

        request.parameters.continuation = Some(Continuation::Conversation("conversation".into()));
        assert!(parent_id_from_request(&request).is_none());
        clear_previous_response_id(&mut request);
        assert_eq!(
            request
                .parameters
                .continuation
                .as_ref()
                .and_then(Continuation::conversation_id),
            Some("conversation")
        );
    }
}
