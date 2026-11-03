use super::*;
use crate::router::{ContinuationLookup, ContinuationTarget};
use std::time::Duration;

/// Trusted request-local evidence; never constructed from client-writable IR.
/// Only continuation metadata is retained, not the materialized history.
#[derive(Clone, Debug, Default)]
pub(crate) enum GenerationParentTarget {
    #[default]
    Unresolved,
    Resolved(Arc<ParentContinuationProof>),
}

#[derive(Clone, Debug)]
pub(crate) struct ParentContinuationProof {
    principal: Principal,
    parent_id: Option<String>,
    target: Option<String>,
    expires_at: Option<std::time::Instant>,
    upstream_response_id: Option<String>,
    state: Option<GenerationChainState>,
}

impl GenerationParentTarget {
    pub(crate) fn absent(principal: &Principal) -> Self {
        Self::Resolved(Arc::new(ParentContinuationProof {
            principal: principal.clone(),
            parent_id: None,
            target: None,
            expires_at: None,
            upstream_response_id: None,
            state: None,
        }))
    }

    pub(crate) fn preferred_target(
        &self,
        principal: &Principal,
        parent_id: &str,
    ) -> Option<String> {
        match self {
            Self::Resolved(proof) if proof.matches(principal, parent_id) => proof.target.clone(),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn route_evidence(
        principal: &Principal,
        parent_id: &str,
        target: &str,
        expires_at: std::time::Instant,
    ) -> Self {
        Self::Resolved(Arc::new(ParentContinuationProof {
            principal: principal.clone(),
            parent_id: Some(parent_id.to_owned()),
            target: Some(target.to_owned()),
            expires_at: Some(expires_at),
            upstream_response_id: None,
            state: None,
        }))
    }
}

impl ParentContinuationProof {
    fn matches(&self, principal: &Principal, parent_id: &str) -> bool {
        &self.principal == principal
            && self.parent_id.as_deref() == Some(parent_id)
            && self
                .expires_at
                .is_some_and(|expiry| expiry > std::time::Instant::now())
    }
}

impl GenerationChain {
    pub(crate) fn continuation_lookup(&self) -> Arc<dyn ContinuationLookup> {
        Arc::new(GenerationChainContinuationLookup {
            chain: self.clone(),
        })
    }
}

impl GenerationChainWrite {
    pub(crate) fn parent_target(&self) -> GenerationParentTarget {
        if self.parent.parent_id.is_some() && self.parent.parent_expires_at.is_none() {
            // Unknown-TTL adapters must freshly validate the parent on every lookup.
            return GenerationParentTarget::Unresolved;
        }
        GenerationParentTarget::Resolved(Arc::new(ParentContinuationProof {
            principal: self.principal.clone(),
            parent_id: self.parent.parent_id.clone(),
            target: self
                .parent
                .parent_state
                .as_ref()
                .map(|state| &state.selected_target_key)
                .or(self.parent.parent_target_key.as_ref())
                .filter(|key| !key.is_empty())
                .cloned(),
            expires_at: self.parent.parent_expires_at,
            upstream_response_id: self.parent.parent_upstream_response_id.clone(),
            // Source-only ancestry deliberately has no execution state.
            state: self.parent.parent_state.clone(),
        }))
    }
}

struct GenerationChainContinuationLookup {
    chain: GenerationChain,
}

#[async_trait]
impl ContinuationLookup for GenerationChainContinuationLookup {
    async fn preferred_target(&self, principal: &Principal, request: &AiRequest) -> Option<String> {
        let parent_id = crate::router::parent_id_from_request(request)?;
        let materialized = self
            .chain
            .store
            .materialize_generation(principal, &TurnNodeId::new(parent_id))
            .await
            .ok()?;
        (!materialized.effective_state.selected_target_key.is_empty())
            .then(|| materialized.effective_state.selected_target_key.clone())
    }

    async fn prepare(
        &self,
        principal: &Principal,
        target: ContinuationTarget<'_>,
        parent: &GenerationParentTarget,
        request: &mut Arc<AiRequest>,
        full_fallback: &mut Option<Arc<AiRequest>>,
    ) -> Option<String> {
        *full_fallback = None;
        let parent_id = crate::router::parent_id_from_request(request)?;
        let materialized = match parent {
            GenerationParentTarget::Unresolved => self
                .chain
                .store
                .materialize_generation(principal, &TurnNodeId::new(&parent_id))
                .await
                .ok(),
            GenerationParentTarget::Resolved(_) => None,
        };
        let continuation = match parent {
            GenerationParentTarget::Unresolved => materialized.as_ref().map(|materialized| {
                (
                    materialized.upstream_response_id.as_ref(),
                    Some(&materialized.effective_state),
                    (materialized.expires_at > std::time::Instant::now())
                        .then_some(materialized.expires_at),
                )
            }),
            GenerationParentTarget::Resolved(proof) if proof.matches(principal, &parent_id) => {
                Some((
                    proof.upstream_response_id.as_ref(),
                    proof.state.as_ref(),
                    proof.expires_at,
                ))
            }
            GenerationParentTarget::Resolved(_) => None,
        };
        let candidate_state =
            GenerationChainState::from_request(request, target.namespace, target.protocol)
                .with_provider_model(target.actual_model);
        if continuation.is_some_and(|(upstream_id, state, expires_at)| {
            prepare_upstream(
                upstream_id,
                state,
                expires_at,
                request,
                &candidate_state,
                target.allow_ephemeral_response,
                full_fallback,
            )
        }) {
            crate::router::parent_id_from_request(request)
        } else {
            crate::router::clear_previous_response_id(Arc::make_mut(request));
            None
        }
    }
}

pub(super) fn prepare_upstream(
    upstream_response_id: Option<&String>,
    parent_state: Option<&GenerationChainState>,
    parent_expires_at: Option<std::time::Instant>,
    request: &mut Arc<AiRequest>,
    candidate_state: &GenerationChainState,
    allow_ephemeral_response: bool,
    full_fallback: &mut Option<Arc<AiRequest>>,
) -> bool {
    *full_fallback = None;
    if !(request_preserves_upstream_response(request) || allow_ephemeral_response)
        || !candidate_state.supports_open_responses_continuation()
    {
        return false;
    }
    let (Some(upstream_id), Some(parent_state)) = (upstream_response_id, parent_state) else {
        return false;
    };
    let verified_history_replay = request
        .meta
        .vendor
        .ingress
        .get(VERIFIED_HISTORY_REPLAY_META)
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let compatible = if verified_history_replay {
        parent_state.same_target(candidate_state)
    } else {
        parent_state.compatible_continuation(candidate_state)
    };
    if !compatible
        || request.items.len() < parent_state.context_messages
        || history_context_fingerprint(&request.items[..parent_state.context_messages])
            != parent_state.context_fingerprint
    {
        return false;
    }
    if parent_expires_at.is_some_and(|expiry| expiry <= std::time::Instant::now()) {
        return false;
    }
    // Snapshot only after every continuation gate succeeds, immediately before
    // discarding history or replacing protocol controls.
    let mut fallback = Arc::clone(request);
    if crate::router::parent_id_from_request(&fallback).is_some() {
        crate::router::clear_previous_response_id(Arc::make_mut(&mut fallback));
    }
    *full_fallback = Some(fallback);
    let request = Arc::make_mut(request);
    request.items = request.items.split_off(parent_state.context_messages);
    crate::router::stamp_previous_response_id(request, upstream_id);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;

    fn evidence(principal: &Principal, expires_at: std::time::Instant) -> GenerationParentTarget {
        let history = AiRequest::new("model", vec![AiItem::output_text("history")]);
        GenerationParentTarget::Resolved(Arc::new(ParentContinuationProof {
            principal: principal.clone(),
            parent_id: Some("parent".into()),
            target: Some("provider:model".into()),
            expires_at: Some(expires_at),
            upstream_response_id: Some("upstream".into()),
            state: Some(GenerationChainState::from_request(
                &history,
                "provider:model",
                Some(OPEN_RESPONSES_2026_04_24),
            )),
        }))
    }

    fn request(parent: &str) -> Arc<AiRequest> {
        let mut request = AiRequest::new(
            "model",
            vec![AiItem::output_text("history"), AiItem::output_text("delta")],
        );
        crate::router::stamp_previous_response_id(&mut request, parent);
        Arc::new(request)
    }

    fn target(namespace: &str) -> ContinuationTarget<'_> {
        ContinuationTarget {
            namespace,
            protocol: Some(OPEN_RESPONSES_2026_04_24),
            actual_model: "model",
            allow_ephemeral_response: true,
        }
    }

    #[tokio::test]
    async fn trusted_parent_prepares_delta_and_preserves_complete_fallback() {
        let lookup = test_chain().await.continuation_lookup();
        let owner = Principal::new("owner");
        let proof = evidence(&owner, std::time::Instant::now() + Duration::from_secs(60));
        let mut request = request("parent");
        let original_items = request.items.clone();
        let mut fallback = None;
        assert_eq!(
            lookup
                .prepare(
                    &owner,
                    target("provider:model"),
                    &proof,
                    &mut request,
                    &mut fallback
                )
                .await
                .as_deref(),
            Some("upstream")
        );
        assert_eq!(request.items.len(), 1);
        assert_eq!(request.items[0].content.to_text(), "delta");
        let fallback = fallback.expect("complete replay");
        assert!(items_equal(&fallback.items, &original_items));
        assert!(crate::router::parent_id_from_request(&fallback).is_none());
    }

    #[tokio::test]
    async fn trusted_proof_does_not_bypass_history_controls_or_protocol_gates() {
        let lookup = test_chain().await.continuation_lookup();
        let owner = Principal::new("owner");
        let proof = evidence(&owner, std::time::Instant::now() + Duration::from_secs(60));
        for mode in ["history", "controls", "protocol"] {
            let mut request = request("parent");
            let mut target = target("provider:model");
            match mode {
                "history" => Arc::make_mut(&mut request).items[0] = AiItem::output_text("changed"),
                "controls" => Arc::make_mut(&mut request).instructions = Some("changed".into()),
                "protocol" => target.protocol = None,
                _ => unreachable!(),
            }
            let original_items = request.items.clone();
            let mut fallback = None;
            assert_eq!(
                lookup
                    .prepare(&owner, target, &proof, &mut request, &mut fallback)
                    .await,
                None,
                "{mode}"
            );
            assert!(items_equal(&request.items, &original_items), "{mode}");
            assert!(
                crate::router::parent_id_from_request(&request).is_none(),
                "{mode}"
            );
            assert!(fallback.is_none(), "{mode}");
        }
    }

    #[tokio::test]
    async fn invalid_or_source_only_proofs_preserve_complete_history() {
        let lookup = test_chain().await.continuation_lookup();
        let owner = Principal::new("owner");
        let valid = evidence(&owner, std::time::Instant::now() + Duration::from_secs(60));
        let expired = evidence(&owner, std::time::Instant::now() - Duration::from_secs(1));
        let mut source_only = valid.clone();
        if let GenerationParentTarget::Resolved(proof) = &mut source_only {
            Arc::make_mut(proof).state = None;
        }
        assert_eq!(
            source_only.preferred_target(&owner, "parent").as_deref(),
            Some("provider:model")
        );
        for (principal, parent, namespace, proof) in [
            (
                Principal::new("foreign"),
                "parent",
                "provider:model",
                valid.clone(),
            ),
            (
                owner.clone(),
                "other-parent",
                "provider:model",
                valid.clone(),
            ),
            (owner.clone(), "parent", "provider:model", expired),
            (owner.clone(), "parent", "other-target", valid),
            (owner.clone(), "parent", "provider:model", source_only),
        ] {
            let mut request = request(parent);
            let original_items = request.items.clone();
            let mut fallback = None;
            assert_eq!(
                lookup
                    .prepare(
                        &principal,
                        target(namespace),
                        &proof,
                        &mut request,
                        &mut fallback
                    )
                    .await,
                None
            );
            assert!(items_equal(&request.items, &original_items));
            assert!(crate::router::parent_id_from_request(&request).is_none());
            assert!(fallback.is_none());
        }
    }
}
