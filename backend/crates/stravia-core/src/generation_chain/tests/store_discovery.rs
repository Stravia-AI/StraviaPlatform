use super::*;
use stravia_runtime_contract::artifact::{ArtifactStore, bytes_stream};

#[test]
fn native_generation_controls_inherit_without_inheriting_resets() {
    use stravia_runtime_contract::protocol::ir::{
        AudioOutput, CacheRetention, CompactionControl, Modality, NativeTool, RequestParameters,
        ServiceTier, Truncation, Verbosity, WebSearchTool,
    };
    let mut parent = RequestParameters::default();
    parent.top_k = Some(20);
    parent.min_p = Some(0.1);
    parent.repetition_penalty = Some(1.1);
    parent.candidate_count = Some(2);
    parent.logit_bias = Some(std::collections::HashMap::from([("1".into(), 0.5)]));
    parent.logprobs = Some(0);
    parent.modalities = Some(vec![Modality::Text, Modality::Audio]);
    parent.audio = Some(AudioOutput {
        voice: Some("alloy".into()),
        format: Some("wav".into()),
    });
    parent.verbosity = Some(Verbosity::High);
    parent.max_tool_calls = Some(3);
    parent.background = Some(false);
    parent.truncation = Some(Truncation::Auto);
    parent.compaction = Some(CompactionControl {
        threshold_tokens: Some(2000),
    });
    parent.metadata = Some(std::collections::BTreeMap::from([(
        "key".into(),
        "value".into(),
    )]));
    parent.end_user_id = Some("user".into());
    parent.service_tier = Some(ServiceTier::Default);
    parent.cache.key = Some("cache".into());
    parent.cache.retention = Some(CacheRetention::Extended);
    parent.native_tools = vec![NativeTool::WebSearch(WebSearchTool {
        max_uses: Some(3),
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
        user_location: None,
        context_size: None,
    })];
    parent.reset(ParameterReset::Temperature);
    let mut current = RequestParameters::default();
    inherit_parameters(&mut current, &parent);
    parent.resets.clear();
    assert_eq!(
        serde_json::to_value(current).unwrap(),
        serde_json::to_value(parent).unwrap()
    );
}

#[test]
fn continuation_resets_block_defaults_without_inheriting_per_turn_controls() {
    use stravia_runtime_contract::protocol::ir::{
        CacheRetention, CodeExecutionTool, Continuation, NativeTool, ServiceTier, Truncation,
        Verbosity,
    };
    let mut parent = responses_request(Vec::new());
    parent.instructions = Some("parent instructions".into());
    parent.generation.temperature = Some(0.5);
    parent.generation.top_p = Some(0.8);
    parent.generation.presence_penalty = Some(0.2);
    parent.generation.frequency_penalty = Some(0.3);
    parent.generation.max_tokens = Some(100);
    parent.parallel_tool_calls = Some(true);
    parent.parameters.background = Some(true);
    parent.parameters.max_tool_calls = Some(5);
    parent.parameters.cache.key = Some("parent cache".into());
    parent.parameters.cache.retention = Some(CacheRetention::Extended);
    parent.parameters.logprobs = Some(3);
    parent.parameters.truncation = Some(Truncation::Auto);
    parent.parameters.service_tier = Some(ServiceTier::Priority);
    parent.parameters.verbosity = Some(Verbosity::High);
    parent.parameters.end_user_id = Some("parent user".into());
    parent.parameters.native_tools = vec![NativeTool::CodeExecution(CodeExecutionTool {
        container: Some("container".into()),
    })];
    parent.parameters.store = Some(true);
    parent.parameters.generate = Some(false);
    parent.parameters.continuation = Some(Continuation::PreviousResponse("parent".into()));
    parent.parameters.session.id = Some("parent session".into());
    let config = EffectiveRequestConfig::from_request(&parent);
    let mut current = responses_request(Vec::new());
    for reset in [
        ParameterReset::Instructions,
        ParameterReset::Temperature,
        ParameterReset::TopP,
        ParameterReset::PresencePenalty,
        ParameterReset::FrequencyPenalty,
        ParameterReset::MaxTokens,
        ParameterReset::ParallelToolCalls,
        ParameterReset::Background,
        ParameterReset::MaxToolCalls,
        ParameterReset::CacheKey,
        ParameterReset::CacheRetention,
        ParameterReset::Logprobs,
        ParameterReset::Truncation,
        ParameterReset::ServiceTier,
        ParameterReset::Verbosity,
        ParameterReset::EndUser,
        ParameterReset::NativeTools,
    ] {
        current.parameters.reset(reset);
    }
    let expected = serde_json::to_value(&current).unwrap();
    config.apply_missing_to(&mut current);
    assert_eq!(serde_json::to_value(&current).unwrap(), expected);

    let mut current = responses_request(Vec::new());
    current.generation.temperature = Some(0.1);
    current.parameters.logprobs = Some(0);
    current.parameters.cache.key = Some("current cache".into());
    current.parameters.continuation = Some(Continuation::Conversation("conversation".into()));
    current.parameters.session.id = Some("current session".into());
    config.apply_missing_to(&mut current);
    assert_eq!(current.generation.temperature, Some(0.1));
    assert_eq!(current.parameters.logprobs, Some(0));
    assert_eq!(
        current.parameters.cache.key.as_deref(),
        Some("current cache")
    );
    assert_eq!(
        current
            .parameters
            .continuation
            .as_ref()
            .and_then(|value| value.conversation_id()),
        Some("conversation")
    );
    assert_eq!(
        current.parameters.session.id.as_deref(),
        Some("current session")
    );
    assert!(current.parameters.store.is_none());
    assert!(current.parameters.generate.is_none());
    assert_eq!(current.instructions.as_deref(), Some("parent instructions"));
}

#[test]
fn nullable_nested_controls_block_inherited_values() {
    use stravia_runtime_contract::protocol::ir::{ReasoningSummary, ResponseFormat};
    let mut parent = responses_request(Vec::new());
    parent.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
    parent.reasoning.summary = Some(ReasoningSummary::Auto);
    parent.response_format = Some(ResponseFormat::JsonSchema {
        name: "result".into(),
        description: None,
        schema: serde_json::json!({"type": "object"}),
        strict: Some(true),
    });
    let config = EffectiveRequestConfig::from_request(&parent);
    let mut current = responses_request(Vec::new());
    current.parameters.reset(ParameterReset::ReasoningLevel);
    current.parameters.reset(ParameterReset::ReasoningSummary);
    current
        .parameters
        .reset(ParameterReset::ResponseFormatStrict);
    config.apply_missing_to(&mut current);
    assert!(current.reasoning.level.is_none());
    assert!(current.reasoning.summary.is_none());
    assert!(matches!(
        current.response_format,
        Some(ResponseFormat::JsonSchema { strict: None, .. })
    ));
    let mut current = responses_request(Vec::new());
    current.parameters.reset(ParameterReset::Reasoning);
    current.parameters.reset(ParameterReset::ResponseFormat);
    config.apply_missing_to(&mut current);
    assert!(current.reasoning.level.is_none());
    assert!(current.reasoning.summary.is_none());
    assert!(current.response_format.is_none());
}

struct ControlledDiscoveryTurnChainStore {
    inner: crate::turn_chain::SqlTurnChainStore,
    blocked: parking_lot::Mutex<std::collections::HashSet<String>>,
    deadlines: parking_lot::Mutex<HashMap<String, std::time::Instant>>,
    materialized_deadlines: parking_lot::Mutex<HashMap<String, std::time::Instant>>,
}

impl ControlledDiscoveryTurnChainStore {
    fn new(inner: crate::turn_chain::SqlTurnChainStore) -> Self {
        Self {
            inner,
            blocked: Default::default(),
            deadlines: Default::default(),
            materialized_deadlines: Default::default(),
        }
    }
    fn block(&self, id: &str) {
        self.blocked.lock().insert(id.to_owned());
    }
    fn unblock(&self, id: &str) {
        self.blocked.lock().remove(id);
    }
    fn deadline(&self, id: &str, deadline: std::time::Instant) {
        self.deadlines.lock().insert(id.to_owned(), deadline);
    }
}

#[async_trait::async_trait]
impl TurnChainStore for ControlledDiscoveryTurnChainStore {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<Vec<stravia_runtime_contract::turn_chain::TurnNode>, TurnUnavailable> {
        Ok(self
            .materialize_with_expiry(principal, kind, id)
            .await?
            .nodes)
    }
    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<stravia_runtime_contract::turn_chain::MaterializedTurnChain, TurnUnavailable> {
        if self.blocked.lock().contains(id.as_str()) {
            return Err(TurnUnavailable::Storage(
                "injected temporary history read failure".into(),
            ));
        }
        let expiry = self.deadlines.lock().get(id.as_str()).copied();
        if expiry.is_some_and(|expiry| expiry <= std::time::Instant::now()) {
            return Err(TurnUnavailable::Unavailable);
        }
        let mut chain = self
            .inner
            .materialize_with_expiry(principal, kind, id)
            .await?;
        if let Some(expiry) = expiry {
            chain.expires_at = chain.expires_at.min(expiry);
        }
        self.materialized_deadlines
            .lock()
            .insert(id.as_str().to_owned(), chain.expires_at);
        Ok(chain)
    }
    async fn commit(&self, commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError> {
        self.inner.commit(commit).await
    }
    async fn find_reusable_prefixes(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        query: &ReusablePrefixQuery,
    ) -> Result<Vec<stravia_runtime_contract::turn_chain::ReusablePrefixCandidate>, TurnUnavailable>
    {
        self.inner
            .find_reusable_prefixes(principal, kind, query)
            .await
    }
    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

#[tokio::test]
async fn native_preparation_reuses_owned_workspace_for_diagnostic_source() {
    let owner = principal("workspace-diagnostic-owner");
    let backend = Arc::new(ControlledDiscoveryTurnChainStore::new(
        crate::turn_chain::test_store().await,
    ));
    let chain = GenerationChain::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    let mut root_request = responses_request(vec![user_message("exact question")]);
    set_generation_session_id(&mut root_request, "diagnostic-session");
    let mut ids = Vec::new();
    for (question, answer) in [
        ("exact question", "exact answer"),
        ("other question", "other answer"),
    ] {
        let mut request = root_request.clone();
        request.items = vec![user_message(question)];
        let mut write = chain.begin(owner.clone(), request).await.unwrap();
        let mut response = AiResponse::new("upstream", "model");
        response.push_output_text(answer);
        assert!(write.stage(&mut response, &generation_source(), None));
        write.persist().await.unwrap();
        ids.push(write.id().to_owned());
    }
    // Delivery IDs are not prefix identity; use client-shaped replay without them.
    let mut replay = root_request;
    replay.items.push(AiItem::output_text("exact answer"));
    let mut write = chain
        .begin_native_compaction(owner.clone(), replay.clone())
        .await
        .unwrap();
    assert_eq!(write.parent_id(), Some(ids[0].as_str()));
    let mut workspace = write.take_discovery_workspace();
    for id in &ids {
        backend.block(id);
    }
    assert_eq!(
        chain
            .compaction_source(&owner, &replay, workspace.as_mut())
            .await
            .unwrap()
            .as_deref(),
        Some(ids[0].as_str())
    );
    drop(workspace);
    assert_eq!(
        chain.compaction_source(&owner, &replay, None).await.err(),
        Some(BeginError::CompactionUnavailable)
    );
}

#[tokio::test]
async fn discovery_workspace_recovers_transient_errors_and_does_not_adopt_a_partial_chain() {
    let owner = principal("workspace-failure-owner");
    let backend = Arc::new(ControlledDiscoveryTurnChainStore::new(
        crate::turn_chain::test_store().await,
    ));
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "recoverable-root",
        None,
        observation_payload(vec![user_message("root")], None, None, "answer"),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "partial-root",
        None,
        observation_payload(vec![user_message("partial")], None, None, "answer"),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "invalid-tail",
        Some("partial-root"),
        serde_json::json!({"invalid":true}),
        Duration::from_secs(60),
    )
    .await;
    let mut workspace = DiscoveryWorkspace::new(&owner, true);
    backend.block("recoverable-root");
    assert!(
        store
            .materialize_discovery_generation(
                &owner,
                &TurnNodeId::new("recoverable-root"),
                &mut workspace
            )
            .await
            .is_err()
    );
    backend.unblock("recoverable-root");
    let recovered = store
        .materialize_discovery_generation(
            &owner,
            &TurnNodeId::new("recoverable-root"),
            &mut workspace,
        )
        .await
        .unwrap();
    assert!(items_equal(
        &recovered.client_items,
        &[user_message("root"), AiItem::output_text("answer")]
    ));
    assert!(
        store
            .materialize_discovery_generation(
                &owner,
                &TurnNodeId::new("invalid-tail"),
                &mut workspace
            )
            .await
            .is_err()
    );
    backend.block("partial-root");
    assert!(
        store
            .materialize_discovery_generation(
                &owner,
                &TurnNodeId::new("partial-root"),
                &mut workspace
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn discovery_workspace_budget_overflow_keeps_prior_evidence_without_partial_admission() {
    let owner = principal("workspace-budget-owner");
    let backend = Arc::new(ControlledDiscoveryTurnChainStore::new(
        crate::turn_chain::test_store().await,
    ));
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "small-root",
        None,
        observation_payload(vec![user_message("root")], None, None, "answer"),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "large-child",
        Some("small-root"),
        observation_payload(
            vec![user_message(&"x".repeat(128 * 1024))],
            None,
            None,
            "large answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    let mut workspace = DiscoveryWorkspace::with_budget(&owner, 64 * 1024);
    let small = store
        .materialize_discovery_generation(&owner, &TurnNodeId::new("small-root"), &mut workspace)
        .await
        .unwrap();
    let large = store
        .materialize_discovery_generation(&owner, &TurnNodeId::new("large-child"), &mut workspace)
        .await
        .unwrap();
    assert_eq!(large.client_items.len(), 4);
    backend.block("small-root");
    backend.block("large-child");
    let retained = store
        .materialize_discovery_generation(&owner, &TurnNodeId::new("small-root"), &mut workspace)
        .await
        .unwrap();
    assert!(items_equal(&retained.client_items, &small.client_items));
    assert!(
        store
            .materialize_discovery_generation(
                &owner,
                &TurnNodeId::new("large-child"),
                &mut workspace
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn discovery_workspace_rejects_structurally_large_response_metadata() {
    let owner = principal("workspace-structured-metadata-owner");
    let backend = Arc::new(ControlledDiscoveryTurnChainStore::new(
        crate::turn_chain::test_store().await,
    ));
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    let mut payload = observation_payload(vec![user_message("root")], None, None, "answer");
    payload["effective_output"]["vendor"]["ingress"]["dense"] =
        serde_json::json!(vec![serde_json::json!({}); 4096]);
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "structured-metadata-root",
        None,
        payload,
        Duration::from_secs(60),
    )
    .await;
    let mut workspace = DiscoveryWorkspace::with_budget(&owner, 64 * 1024);
    let id = TurnNodeId::new("structured-metadata-root");
    let first = store
        .materialize_discovery_generation(&owner, &id, &mut workspace)
        .await
        .unwrap();
    assert!(items_equal(
        &first.client_items,
        &[user_message("root"), AiItem::output_text("answer")],
    ));
    backend.block(id.as_str());
    assert!(
        store
            .materialize_discovery_generation(&owner, &id, &mut workspace)
            .await
            .is_err(),
        "decoded metadata above the workspace budget must require another authoritative read"
    );
}

#[tokio::test]
async fn discovery_workspace_preserves_each_overlapping_heads_exact_expiry() {
    let owner = principal("workspace-deadline-owner");
    let backend = Arc::new(ControlledDiscoveryTurnChainStore::new(
        crate::turn_chain::test_store().await,
    ));
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "long-lived-root",
        None,
        observation_payload(vec![user_message("root")], None, None, "answer"),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "short-lived-child",
        Some("long-lived-root"),
        observation_payload(vec![user_message("child")], None, None, "child answer"),
        Duration::from_secs(60),
    )
    .await;
    let parent_expiry = std::time::Instant::now() + Duration::from_secs(40);
    let child_expiry = std::time::Instant::now() + Duration::from_secs(20);
    backend.deadline("long-lived-root", parent_expiry);
    backend.deadline("short-lived-child", child_expiry);
    let mut workspace = DiscoveryWorkspace::new(&owner, true);
    let parent = store
        .materialize_discovery_generation(
            &owner,
            &TurnNodeId::new("long-lived-root"),
            &mut workspace,
        )
        .await
        .unwrap();
    let child = store
        .materialize_discovery_generation(
            &owner,
            &TurnNodeId::new("short-lived-child"),
            &mut workspace,
        )
        .await
        .unwrap();
    let parent_again = store
        .materialize_discovery_generation(
            &owner,
            &TurnNodeId::new("long-lived-root"),
            &mut workspace,
        )
        .await
        .unwrap();
    assert_eq!(parent.expires_at, parent_expiry);
    assert_eq!(child.expires_at, child_expiry);
    assert_eq!(parent_again.expires_at, parent_expiry);
    let after_child_expiry = child_expiry + Duration::from_secs(1);
    assert!(after_child_expiry < parent_expiry);
    assert!(
        workspace
            .chain(
                &owner,
                &TurnNodeId::new("short-lived-child"),
                after_child_expiry
            )
            .is_none()
    );
    let (nodes, expires_at) = workspace
        .chain(
            &owner,
            &TurnNodeId::new("long-lived-root"),
            after_child_expiry,
        )
        .expect("the longer root certificate still permits cached ancestry replay");
    let cached_parent = materialize_discovery_nodes(&nodes, expires_at).unwrap();
    assert_eq!(cached_parent.expires_at, parent_expiry);
    assert_eq!(
        serde_json::to_value(&cached_parent).unwrap(),
        serde_json::to_value(parent.as_ref()).unwrap()
    );
}

#[tokio::test]
async fn discovery_workspace_preserves_candidate_order_and_distinct_complete_windows() {
    for budget in [0, 256, DISCOVERY_WORKSPACE_BYTES] {
        let owner = principal("workspace-owner");
        let store = GenerationChainStore::from_turn_chain(
            Arc::new(crate::turn_chain::test_store().await),
            Duration::from_secs(60),
            crate::runtime_cache::RuntimeCache::tinyufo(1),
        );
        let mut root = responses_request(vec![user_message("exact question")]);
        set_generation_session_id(&mut root, "workspace-session");
        for (id, question, answer) in [
            ("workspace-good", "exact question", "exact answer"),
            ("workspace-bad", "other question", "other answer"),
        ] {
            let mut request = root.clone();
            request.items = vec![user_message(question)];
            let mut response = AiResponse::new(id, "model");
            response.push_output_text(answer);
            store
                .save(GenerationChainCommit {
                    principal: owner.clone(),
                    id: id.into(),
                    parent: &ActiveGenerationChain::default(),
                    request_delta: &request,
                    effective_request: None,
                    response,
                    upstream_response_id: None,
                    effective_state: GenerationChainState::from_request(
                        &request,
                        "target",
                        OPEN_RESPONSES_2026_04_24,
                    ),
                })
                .await
                .unwrap();
        }
        let mut complete = root.clone();
        complete.items.push(AiItem::output_text("exact answer"));
        let mut workspace = DiscoveryWorkspace::with_budget(&owner, budget);
        let mut strict = Arc::new(complete.clone());
        let found = store
            .discover_prefix(&owner, &mut strict, true, &mut workspace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.active.parent_id.as_deref(), Some("workspace-good"));
        assert_eq!(found.matched_items, 2);
        let mut ordinary = Arc::new(complete.clone());
        assert!(
            store
                .discover_parent(&owner, &mut ordinary, &mut workspace)
                .await
                .unwrap()
                .is_none()
        );
        let mut continuation = complete.clone();
        continuation.items.push(user_message("next"));
        let mut continuation = Arc::new(continuation);
        let found = store
            .discover_parent(&owner, &mut continuation, &mut workspace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.active.parent_id.as_deref(), Some("workspace-good"));
        assert!(items_equal(
            &continuation.items,
            &[
                user_message("exact question"),
                AiItem::output_text("exact answer"),
                user_message("next")
            ]
        ));
        let mut mismatch = complete.clone();
        mismatch.items[1] = AiItem::output_text("changed semantic answer");
        mismatch.items.push(user_message("next"));
        assert!(
            store
                .discover_parent(&owner, &mut Arc::new(mismatch), &mut workspace)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .discover_prefix(
                    &principal("different-owner"),
                    &mut Arc::new(complete),
                    true,
                    &mut workspace
                )
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn discovery_workspace_replays_shared_ancestry_across_multiple_candidates() {
    let owner = principal("workspace-shared-owner");
    let store = GenerationChainStore::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    let mut root_request = responses_request(vec![user_message("root question")]);
    set_generation_session_id(&mut root_request, "shared-session");
    let mut root_response = AiResponse::new("shared-root", "model");
    root_response.push_output_text("root answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "shared-root".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root_request,
            effective_request: None,
            response: root_response,
            upstream_response_id: None,
            effective_state: GenerationChainState::from_request(
                &root_request,
                "target",
                OPEN_RESPONSES_2026_04_24,
            ),
        })
        .await
        .unwrap();
    let mut child_request = root_request.clone();
    child_request.items = vec![user_message("old extension")];
    crate::router::stamp_previous_response_id(&mut child_request, "shared-root");
    let parent = store
        .materialize_parent(&owner, &mut child_request)
        .await
        .unwrap();
    let mut child_delta = root_request.clone();
    child_delta.items = vec![user_message("old extension")];
    let mut child_response = AiResponse::new("shared-child", "model");
    child_response.push_output_text("old extension answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "shared-child".into(),
            parent: &parent,
            request_delta: &child_delta,
            effective_request: None,
            response: child_response,
            upstream_response_id: None,
            effective_state: GenerationChainState::from_request(
                &child_request,
                "target",
                OPEN_RESPONSES_2026_04_24,
            ),
        })
        .await
        .unwrap();
    let mut replay = root_request;
    replay.items.extend([
        AiItem::output_text("root answer"),
        user_message("different extension"),
        AiItem::output_text("different extension answer"),
        user_message("next"),
    ]);
    let mut workspace = DiscoveryWorkspace::new(&owner, true);
    for complete_window in [true, false, true] {
        let mut request = Arc::new(replay.clone());
        let found = store
            .discover_prefix(&owner, &mut request, complete_window, &mut workspace)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.active.parent_id.as_deref(), Some("shared-root"));
        assert_eq!(found.matched_items, 2);
        assert!(items_equal(&request.items, &replay.items));
    }
}

#[tokio::test]
async fn discovery_workspace_rechecks_candidate_priority_after_a_previous_match() {
    let owner = principal("workspace-priority-owner");
    let durable = Arc::new(crate::turn_chain::test_store().await);
    let store = GenerationChainStore::from_turn_chain(
        durable.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    let mut root = responses_request(vec![user_message("question")]);
    set_generation_session_id(&mut root, "priority-session");
    let mut response = AiResponse::new("good-priority", "model");
    response.push_output_text("answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "good-priority".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::from_request(
                &root,
                "target",
                OPEN_RESPONSES_2026_04_24,
            ),
        })
        .await
        .unwrap();
    let mut replay = root.clone();
    replay
        .items
        .extend([AiItem::output_text("answer"), user_message("next")]);
    let mut workspace = DiscoveryWorkspace::new(&owner, true);
    assert_eq!(
        store
            .discover_parent(&owner, &mut Arc::new(replay.clone()), &mut workspace)
            .await
            .unwrap()
            .unwrap()
            .active
            .parent_id
            .as_deref(),
        Some("good-priority")
    );
    durable
        .commit(TurnCommit {
            id: TurnNodeId::new("bad-priority"),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: RESPONSE_PAYLOAD_VERSION,
            payload: serde_json::json!({"invalid":true}),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: Some(ReusablePrefixMetadata {
                namespace: format!("{GENERATION_PREFIX_NAMESPACE}session"),
                fingerprint: generation_session_fingerprint(&root).unwrap(),
                item_count: 2,
                completed_at: i64::MAX,
                projection: None,
            }),
        })
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(
            store
                .discover_parent(&owner, &mut Arc::new(replay.clone()), &mut workspace)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn discovery_projection_skips_only_proven_incompatible_corrupt_candidates() {
    use stravia_runtime_contract::turn_chain::REUSABLE_PREFIX_PROJECTION_VERSION;

    for (case, bad_first, expect_error) in [
        ("different-controls", true, false),
        ("different-context", true, false),
        ("matching", true, true),
        ("unknown-version", true, true),
        ("absent", true, true),
        ("matching", false, false),
    ] {
        let owner = principal("projection-corrupt-candidate-owner");
        let durable = Arc::new(crate::turn_chain::test_store().await);
        let store = GenerationChainStore::from_turn_chain(
            durable.clone(),
            Duration::from_secs(60),
            crate::runtime_cache::RuntimeCache::tinyufo(1),
        );
        let mut root = responses_request(vec![user_message("question")]);
        set_generation_session_id(&mut root, "corrupt-candidate-session");
        let mut response = AiResponse::new("good-projection", "model");
        response.push_output_text("answer");
        store
            .save(GenerationChainCommit {
                principal: owner.clone(),
                id: "good-projection".into(),
                parent: &ActiveGenerationChain::default(),
                request_delta: &root,
                effective_request: None,
                response,
                upstream_response_id: None,
                effective_state: GenerationChainState::from_request(
                    &root,
                    "target",
                    OPEN_RESPONSES_2026_04_24,
                ),
            })
            .await
            .unwrap();
        let mut replay = root.clone();
        replay.items.push(AiItem::output_text("answer"));
        let history = ClientHistoryState::from_request_with_count(&root, &replay.items).0;
        let item_count = u32::try_from(
            stravia_runtime_contract::protocol::ir::canonical::history_unit_count(&replay.items),
        )
        .unwrap();
        let projection = match case {
            "absent" => None,
            _ => {
                let mut projection = history.reusable_projection();
                match case {
                    "different-controls" => {
                        let mut different = root.clone();
                        different.instructions = Some("incompatible controls".into());
                        projection.controls_fingerprint =
                            ClientHistoryState::from_request_with_count(&different, &replay.items)
                                .0
                                .controls_fingerprint;
                    }
                    "different-context" => {
                        projection.context_fingerprint = history_context_fingerprint(&[
                            user_message("different question"),
                            AiItem::output_text("different answer"),
                        ]);
                    }
                    "unknown-version" => {
                        projection.version = REUSABLE_PREFIX_PROJECTION_VERSION + 1;
                        projection.controls_fingerprint = "unknown controls projection".into();
                        projection.context_fingerprint = "unknown context projection".into();
                    }
                    "matching" => {}
                    _ => unreachable!(),
                }
                Some(projection)
            }
        };
        // A real index row whose immutable generation payload cannot be decoded.
        // Its earlier/later completion time fixes candidate priority independently
        // of IDs and proves that a successful first hit never reads later payloads.
        durable
            .commit(TurnCommit {
                id: TurnNodeId::new("corrupt-projection"),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: owner.clone(),
                payload_version: RESPONSE_PAYLOAD_VERSION,
                payload: serde_json::json!({"invalid": true}),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: Some(ReusablePrefixMetadata {
                    namespace: history.reusable_namespace(),
                    fingerprint: history.session_fingerprint.clone().unwrap(),
                    item_count,
                    completed_at: if bad_first { i64::MAX } else { 0 },
                    projection,
                }),
            })
            .await
            .unwrap();
        replay.items.push(user_message("next"));
        let mut workspace = DiscoveryWorkspace::new(&owner, true);
        let result = store
            .discover_parent(&owner, &mut Arc::new(replay), &mut workspace)
            .await;
        if expect_error {
            assert_eq!(
                result.err().as_deref(),
                Some("invalid generation payload"),
                "case={case}, bad_first={bad_first}",
            );
        } else {
            assert_eq!(
                result.unwrap().unwrap().active.parent_id.as_deref(),
                Some("good-projection"),
                "case={case}, bad_first={bad_first}",
            );
        }
    }
}

#[tokio::test]
async fn discovery_workspace_does_not_retain_expired_or_invalid_ancestor_evidence() {
    let owner = principal("workspace-expiry-owner");
    let durable = Arc::new(crate::turn_chain::test_store().await);
    let store = GenerationChainStore::from_turn_chain(
        durable.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    commit_observation_node(
        durable.as_ref(),
        &owner,
        "expired-workspace-root",
        None,
        observation_payload(
            vec![user_message("old")],
            None,
            Some(vec![AiItem::output_text("old answer")]),
            "old answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    let mut workspace = DiscoveryWorkspace::new(&owner, true);
    let initial = store
        .materialize_discovery_generation(
            &owner,
            &TurnNodeId::new("expired-workspace-root"),
            &mut workspace,
        )
        .await
        .unwrap();
    assert_eq!(initial.client_items.len(), 2);
    let (_, proof_expires_at) = workspace
        .chain(
            &owner,
            &TurnNodeId::new("expired-workspace-root"),
            std::time::Instant::now(),
        )
        .expect("fresh workspace proof");
    assert!(
        workspace
            .chain(
                &owner,
                &TurnNodeId::new("expired-workspace-root"),
                proof_expires_at,
            )
            .is_none()
    );
    commit_observation_node(
        durable.as_ref(),
        &owner,
        "expired-durable-root",
        None,
        observation_payload(vec![user_message("expired")], None, None, "expired answer"),
        Duration::ZERO,
    )
    .await;
    assert_eq!(
        store
            .materialize_discovery_generation(
                &owner,
                &TurnNodeId::new("expired-durable-root"),
                &mut workspace
            )
            .await
            .err()
            .as_deref(),
        Some("turn unavailable"),
    );
    commit_observation_node(
        durable.as_ref(),
        &owner,
        "valid-workspace-root",
        None,
        observation_payload(vec![user_message("kept")], None, None, "root answer"),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        durable.as_ref(),
        &owner,
        "invalid-workspace-tail",
        Some("valid-workspace-root"),
        serde_json::json!({"invalid":true}),
        Duration::from_secs(60),
    )
    .await;
    for _ in 0..2 {
        assert!(
            store
                .materialize_discovery_generation(
                    &owner,
                    &TurnNodeId::new("invalid-workspace-tail"),
                    &mut workspace
                )
                .await
                .is_err()
        );
    }
    let valid = store
        .materialize_discovery_generation(
            &owner,
            &TurnNodeId::new("valid-workspace-root"),
            &mut workspace,
        )
        .await
        .unwrap();
    assert_eq!(valid.client_items.len(), 2);
    assert!(
        store
            .materialize_discovery_generation(
                &principal("other-owner"),
                &TurnNodeId::new("valid-workspace-root"),
                &mut workspace
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn shared_discovery_replay_preserves_mutations_reasoning_and_media_boundaries() {
    let owner = principal("workspace-replay-owner");
    for (replace, reasoning_field) in [
        (false, serde_json::Value::Null),
        (true, serde_json::Value::Null),
        (false, serde_json::json!("")),
        (true, serde_json::json!("")),
    ] {
        let input = vec![user_message("boundary input")];
        let mutation = if replace {
            EffectiveHistoryMutation::Replace {
                items: freeze_items(input.clone()).into(),
            }
        } else {
            EffectiveHistoryMutation::Append {
                items: freeze_items(input.clone()).into(),
            }
        };
        let mut root = observation_payload(vec![user_message("root")], None, None, "root answer");
        root["compaction_record_ids"] = serde_json::json!(["root-record", "duplicate"]);
        root["trusted_media_turn_ids"] = serde_json::json!(["root-media"]);
        let mut child = observation_payload(
            vec![user_message("unused delta")],
            Some(mutation.clone()),
            Some(vec![AiItem::output_text("client answer")]),
            "effective answer",
        );
        child["effective_history_mutation"] = serde_json::to_value(mutation).unwrap();
        child["compaction_record_ids"] = serde_json::json!(["child-record", "duplicate"]);
        child["trusted_media_turn_ids"] = serde_json::json!(["child-media"]);
        let request = responses_request(input.clone());
        child["client_history"] =
            serde_json::to_value(ClientHistoryState::from_request_with_count(&request, &input).0)
                .unwrap();
        child["effective_state"]["selected_target_key"] = serde_json::json!("selected-target");
        child["upstream_response_id"] = serde_json::json!("upstream-child");
        child["effective_system"] = serde_json::json!("effective child system");
        child["effective_request"] =
            serde_json::to_value(EffectiveRequestConfig::from_request(&request)).unwrap();
        child["effective_output"]["vendor"]["ingress"]
            .as_object_mut()
            .expect("effective output vendor ingress")
            .insert(
                stravia_runtime_contract::protocol::ir::vendor_ext::CHAT_REASONING_FIELD_META
                    .into(),
                reasoning_field,
            );
        let persisted: PersistedResponseNode<'static> = serde_json::from_value(child).unwrap();
        let child = serde_json::to_value(persisted).unwrap();
        let nodes = vec![
            stravia_runtime_contract::turn_chain::TurnNode {
                id: TurnNodeId::new("shared-root"),
                kind: TurnNodeKind::Response,
                parent_id: None,
                payload_version: RESPONSE_PAYLOAD_VERSION,
                payload: root,
            },
            stravia_runtime_contract::turn_chain::TurnNode {
                id: TurnNodeId::new("shared-child"),
                kind: TurnNodeKind::Response,
                parent_id: Some(TurnNodeId::new("shared-root")),
                payload_version: RESPONSE_PAYLOAD_VERSION,
                payload: child,
            },
        ];
        let expires_at = std::time::Instant::now() + Duration::from_secs(60);
        let owned = materialize_generation_nodes(nodes.clone(), expires_at).unwrap();
        let mut workspace = DiscoveryWorkspace::new(&owner, true);
        let decoded = workspace.decode_chain(&owner, nodes).unwrap();
        let borrowed = materialize_discovery_nodes(&decoded, expires_at).unwrap();
        assert_eq!(
            serde_json::to_value(&owned).unwrap(),
            serde_json::to_value(&borrowed).unwrap()
        );
        assert_eq!(
            borrowed.compaction_record_ids,
            ["child-record", "duplicate", "root-record"]
        );
        assert_eq!(borrowed.media_turn_messages.len(), 2);
        assert_eq!(
            borrowed.upstream_response_id.as_deref(),
            Some("upstream-child")
        );
        assert!(
            !borrowed
                .effective_items
                .iter()
                .any(|item| replace && item.content.to_text() == "root")
        );
    }
}

#[test]
fn historical_v6_request_controls_are_imported_before_continuation_inheritance() {
    let root = responses_request(vec![user_message("historical question")]);
    let mut config = serde_json::to_value(EffectiveRequestConfig::from_request(&root)).unwrap();
    config.as_object_mut().unwrap().remove("parameters");
    config["ext"] = serde_json::json!({
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "historical-cache",
        "top_logprobs": 0,
        "background": false,
        "text": {"verbosity": "high", "format": {"type": "json_object"}},
        "service_tier": "default",
    });
    let mut payload = observation_payload(
        vec![user_message("historical question")],
        None,
        None,
        "answer",
    );
    payload["effective_request"] = config;
    let materialized = materialize_generation_nodes(
        vec![stravia_runtime_contract::turn_chain::TurnNode {
            id: TurnNodeId::new("historical-v6"),
            kind: TurnNodeKind::Response,
            parent_id: None,
            payload_version: 6,
            payload,
        }],
        std::time::Instant::now() + Duration::from_secs(60),
    )
    .unwrap();
    let mut continuation = responses_request(vec![user_message("continue")]);
    materialized
        .effective_request
        .as_ref()
        .unwrap()
        .apply_missing_to(&mut continuation);
    assert_eq!(
        continuation.parameters.cache.key.as_deref(),
        Some("historical-cache")
    );
    assert_eq!(continuation.parameters.logprobs, Some(0));
    assert_eq!(continuation.parameters.background, Some(false));
    assert_eq!(
        continuation.parameters.verbosity,
        Some(stravia_runtime_contract::protocol::ir::Verbosity::High)
    );
    assert!(matches!(
        continuation.response_format,
        Some(stravia_runtime_contract::protocol::ir::ResponseFormat::JsonObject)
    ));
    assert_eq!(
        continuation.parameters.service_tier,
        Some(stravia_runtime_contract::protocol::ir::ServiceTier::Default)
    );
}

type PendingPersist = tokio::task::JoinHandle<Result<(), PersistError>>;

struct CountingParentTurnChainStore {
    inner: crate::turn_chain::SqlTurnChainStore,
    materializations: std::sync::atomic::AtomicUsize,
}

impl CountingParentTurnChainStore {
    fn reads(&self) -> usize {
        self.materializations
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl TurnChainStore for CountingParentTurnChainStore {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<
        Vec<stravia_runtime_contract::turn_chain::TurnNode>,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        self.materializations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.materialize(principal, kind, id).await
    }

    async fn materialize_with_expiry(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<
        stravia_runtime_contract::turn_chain::MaterializedTurnChain,
        stravia_runtime_contract::turn_chain::TurnUnavailable,
    > {
        self.materializations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .materialize_with_expiry(principal, kind, id)
            .await
    }

    async fn commit(
        &self,
        commit: TurnCommit,
    ) -> Result<TurnNodeId, stravia_runtime_contract::turn_chain::TurnCommitError> {
        self.inner.commit(commit).await
    }

    async fn sweep_expired(
        &self,
    ) -> Result<u64, stravia_runtime_contract::turn_chain::TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

fn reference_to(id: &str) -> AiItem {
    let mut item = user_message("");
    item.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            serde_json::json!({"__open_responses_item_reference": id}),
        ),
    );
    item
}

fn continuation_to(id: &str, items: Vec<AiItem>) -> AiRequest {
    let mut request = responses_request(items);
    crate::router::stamp_previous_response_id(&mut request, id);
    request
}

#[tokio::test]
async fn optimization_plain_parent_reads_chain_once_cold_and_not_at_all_warm() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let owner = principal("owner");
    let mut root = observation_payload(
        vec![user_message("root question")],
        None,
        Some(vec![AiItem::output_text("root answer")]),
        "effective root answer",
    );
    root["compaction_record_ids"] = serde_json::json!(["compact-root"]);
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "resp_optimization_root",
        None,
        root,
        Duration::from_secs(60),
    )
    .await;
    let mut child = observation_payload(
        vec![user_message("child question")],
        None,
        Some(vec![AiItem::output_text("child answer")]),
        "effective child answer",
    );
    child["compaction_record_ids"] = serde_json::json!(["compact-child", "compact-root"]);
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "resp_optimization_child",
        Some("resp_optimization_root"),
        child,
        Duration::from_secs(60),
    )
    .await;
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    for (iteration, expected_reads) in [(0, 1), (1, 1)] {
        let mut request = continuation_to(
            "resp_optimization_child",
            vec![user_message("next question")],
        );
        let parent = store
            .materialize_parent(&owner, &mut request)
            .await
            .expect("restore parent");
        assert_eq!(backend.reads(), expected_reads, "iteration {iteration}");
        assert_eq!(parent.root_id.as_deref(), Some("resp_optimization_root"));
        assert_eq!(
            parent.compaction_record_ids,
            ["compact-child".to_owned(), "compact-root".to_owned()]
        );
        assert_eq!(
            request
                .items
                .iter()
                .map(|item| item.content.to_text())
                .collect::<Vec<_>>(),
            [
                "root question",
                "effective root answer",
                "child question",
                "effective child answer",
                "next question",
            ]
        );
    }
    let mut denied = continuation_to(
        "resp_optimization_child",
        vec![user_message("unauthorized")],
    );
    assert_eq!(
        store
            .materialize_parent(&principal("other"), &mut denied)
            .await
            .expect_err("cache must not cross principals"),
        "previous_response_not_found"
    );
}

#[tokio::test]
async fn optimization_references_resolve_old_ancestor_after_replace_with_one_cold_read() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let owner = principal("owner");
    let old_output = AiItem::output_text("old answer").with_graph_metadata(
        Some("msg_old".into()),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "resp_optimization_reference_root",
        None,
        observation_payload(
            vec![user_message("old question")],
            None,
            Some(vec![old_output]),
            "private old answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "resp_optimization_reference_child",
        Some("resp_optimization_reference_root"),
        observation_payload(
            vec![user_message("edited question")],
            Some(EffectiveHistoryMutation::Replace {
                items: freeze_items(vec![user_message("edited question")]).into(),
            }),
            Some(vec![AiItem::output_text("new answer")]),
            "private new answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    // 首次冷读后 materialized 与引用目录同键缓存，后续命中不再触发存储读。
    for (iteration, expected_reads) in [(0, 1), (1, 1)] {
        let mut request = continuation_to(
            "resp_optimization_reference_child",
            vec![reference_to("msg_old")],
        );
        store
            .materialize_parent(&owner, &mut request)
            .await
            .expect("ancestor output reference survives Replace");
        assert_eq!(backend.reads(), expected_reads, "iteration {iteration}");
        assert_eq!(
            request.items.last().unwrap().content.to_text(),
            "old answer"
        );
        assert_eq!(request.items.last().unwrap().id_ref(), Some("msg_old"));
        assert!(
            !request.items[..request.items.len() - 1]
                .iter()
                .any(|item| item.content.to_text() == "old answer"),
            "Replace must still remove old output from the effective execution history"
        );
    }
}

#[tokio::test]
async fn optimization_reference_conflicts_across_ancestors_remain_ambiguous() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let owner = principal("owner");
    let item = |text| {
        AiItem::output_text(text).with_graph_metadata(
            Some("msg_conflicting".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        )
    };
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "resp_optimization_conflict_root",
        None,
        observation_payload(
            vec![user_message("question")],
            None,
            Some(vec![item("first answer")]),
            "private first answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "resp_optimization_conflict_child",
        Some("resp_optimization_conflict_root"),
        observation_payload(
            vec![user_message("followup")],
            None,
            Some(vec![item("conflicting answer")]),
            "private conflicting answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let mut request = continuation_to(
        "resp_optimization_conflict_child",
        vec![reference_to("msg_conflicting")],
    );
    assert_eq!(
        store
            .materialize_parent(&owner, &mut request)
            .await
            .expect_err("different answers with the same ID cannot resolve"),
        "item_reference_ambiguous"
    );
    assert_eq!(backend.reads(), 1);
}

async fn blocked_generation_commit(
    fail: bool,
) -> (
    Arc<CommitBarrierTurnChainStore>,
    GenerationChain,
    Principal,
    Vec<AiItem>,
    String,
    String,
    PendingPersist,
) {
    let backend = Arc::new(CommitBarrierTurnChainStore::new(
        crate::turn_chain::test_store().await,
    ));
    let chain = GenerationChain::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("commit-fence-owner");
    let question = user_message("question");
    let first_answer = AiItem::output_text("first answer");
    let follow_up = user_message("follow up");
    let pending_answer = AiItem::output_text("pending answer");

    let mut root = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .expect("begin durable root");
    let mut root_response = AiResponse::new("root-upstream", "model");
    root_response.push_output_text("first answer");
    assert!(root.stage(&mut root_response, &generation_source(), None));
    root.persist().await.expect("persist durable root");
    let root_id = root.id().to_owned();

    let mut pending = chain
        .begin(
            owner.clone(),
            responses_request(vec![
                question.clone(),
                first_answer.clone(),
                follow_up.clone(),
            ]),
        )
        .await
        .expect("begin pending continuation");
    assert_eq!(pending.parent_id(), Some(root_id.as_str()));
    let pending_id = pending.id().to_owned();
    let mut pending_response = AiResponse::new(pending_id.clone(), "model");
    pending_response.push_output_text("pending answer");
    assert!(pending.stage(&mut pending_response, &generation_source(), None,));
    let delivered_history = vec![question, first_answer, follow_up, pending_answer];

    backend.block_next_commit(fail);
    let persist = tokio::spawn(async move { pending.persist().await });
    backend.wait_until_blocked().await;
    (
        backend,
        chain,
        owner,
        delivered_history,
        root_id,
        pending_id,
        persist,
    )
}

#[tokio::test]
async fn delivered_prefix_waits_for_commit_without_serializing_real_branches() {
    let (backend, chain, owner, delivered, root_id, pending_id, persist) =
        blocked_generation_commit(false).await;
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let mut history = delivered.clone();
    history.push(user_message("immediate continuation"));
    let mut matching = Box::pin(chain.begin(owner.clone(), responses_request(history)));
    backend.force_stale_discovery();
    assert!(
        matches!(futures::poll!(&mut matching), std::task::Poll::Pending),
        "matching begin must wait instead of snapshotting the stale durable prefix",
    );
    backend.allow_current_discovery();
    let explicit = {
        let chain = chain.clone();
        let owner = owner.clone();
        let started = Arc::clone(&started);
        let pending_id = pending_id.clone();
        tokio::spawn(async move {
            let mut request = responses_request(vec![user_message("explicit continuation")]);
            crate::router::stamp_previous_response_id(&mut request, &pending_id);
            started.add_permits(1);
            chain.begin(owner, request).await
        })
    };
    let item_reference = {
        let chain = chain.clone();
        let owner = owner.clone();
        let started = Arc::clone(&started);
        let pending_id = pending_id.clone();
        tokio::spawn(async move {
            let mut reference = user_message("");
            reference.meta = Some(
                stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                    "__open_responses_item_reference":
                        stravia_protocol_codec::codec::open_responses::formatter::gateway_item_id(
                            "msg",
                            &pending_id,
                            0,
                        )
                })),
            );
            let mut request = responses_request(vec![
                reference,
                user_message("continuation with item reference"),
            ]);
            crate::router::stamp_previous_response_id(&mut request, &pending_id);
            started.add_permits(1);
            chain.begin(owner, request).await
        })
    };
    started
        .acquire_many(2)
        .await
        .expect("continuations started")
        .forget();

    let mut branch_history = delivered[..2].to_vec();
    branch_history.push(user_message("real branch from durable root"));
    let branch = chain
        .begin(owner.clone(), responses_request(branch_history))
        .await
        .expect("unrelated branch must not wait for pending commit");
    assert_eq!(branch.parent_id(), Some(root_id.as_str()));

    let mut other_history = delivered.clone();
    other_history.push(user_message("other principal"));
    let other = chain
        .begin(
            principal("commit-fence-other"),
            responses_request(other_history),
        )
        .await
        .expect("another Principal must not wait for pending commit");
    assert_eq!(other.parent_id(), None);

    tokio::task::yield_now().await;
    backend.release_commit();
    persist
        .await
        .expect("persist task")
        .expect("pending commit succeeds");
    let matching = matching.await.expect("matching continuation begins");
    assert_eq!(matching.parent_id(), Some(pending_id.as_str()));
    assert_eq!(matching.request_delta().items.len(), 1);
    let explicit = explicit
        .await
        .expect("explicit begin task")
        .expect("explicit continuation begins");
    assert_eq!(explicit.parent_id(), Some(pending_id.as_str()));
    let item_reference = item_reference
        .await
        .expect("item-reference begin task")
        .expect("pending item reference resolves after commit");
    assert_eq!(item_reference.parent_id(), Some(pending_id.as_str()));
    assert!(
        item_reference
            .request()
            .items
            .iter()
            .any(|item| item.content.to_text() == "pending answer")
    );
}

#[tokio::test]
async fn failed_pending_commit_releases_waiter_without_publishing_a_node() {
    let (backend, chain, owner, mut delivered, root_id, pending_id, persist) =
        blocked_generation_commit(true).await;
    delivered.push(user_message("continue after failed commit"));
    let continuation = {
        let chain = chain.clone();
        let owner = owner.clone();
        tokio::spawn(async move { chain.begin(owner, responses_request(delivered)).await })
    };
    tokio::task::yield_now().await;
    backend.release_commit();
    assert!(persist.await.expect("persist task").is_err());
    let continuation = continuation
        .await
        .expect("continuation task")
        .expect("failed commit releases continuation");
    assert_eq!(continuation.parent_id(), Some(root_id.as_str()));
    assert_eq!(continuation.request_delta().items.len(), 3);
    assert!(
        backend
            .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new(pending_id),)
            .await
            .is_err(),
        "failed commit must not publish a resumable node",
    );
}

#[tokio::test]
async fn cancelled_pending_commit_releases_waiter_without_publishing_a_node() {
    let (backend, chain, owner, mut delivered, root_id, pending_id, persist) =
        blocked_generation_commit(false).await;
    delivered.push(user_message("continue after cancelled commit"));
    let continuation = {
        let chain = chain.clone();
        let owner = owner.clone();
        tokio::spawn(async move { chain.begin(owner, responses_request(delivered)).await })
    };
    tokio::task::yield_now().await;
    persist.abort();
    assert!(
        persist
            .await
            .expect_err("persist task is cancelled")
            .is_cancelled()
    );
    let continuation = continuation
        .await
        .expect("continuation task")
        .expect("cancelled commit releases continuation");
    assert_eq!(continuation.parent_id(), Some(root_id.as_str()));
    assert_eq!(continuation.request_delta().items.len(), 3);
    assert!(
        backend
            .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new(pending_id),)
            .await
            .is_err(),
        "cancelled commit must not publish a resumable node",
    );
}

#[tokio::test]
async fn prefix_rebuild_preserves_branch_mutations_and_unindexed_ancestor_context() {
    let owner = principal("rebuild-branches");
    let seed_backend = Arc::new(crate::turn_chain::test_store().await);
    let seed = GenerationChainStore::from_turn_chain(
        seed_backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    let root_input = responses_request(vec![user_message("root question")]);
    let mut root_output = AiResponse::new("upstream-root", "model");
    root_output.push_output_text("root answer");
    seed.save(GenerationChainCommit {
        principal: owner.clone(),
        id: "rebuild-root".into(),
        parent: &ActiveGenerationChain::default(),
        request_delta: &root_input,
        effective_request: None,
        response: root_output,
        upstream_response_id: None,
        effective_state: GenerationChainState::default(),
    })
    .await
    .unwrap();

    // Persist genuine generation commits: one ordinary append and one
    // replacement on the same parent, as produced by a compaction boundary.
    for (id, input, answer, replace) in [
        ("rebuild-append", "append question", "append answer", false),
        (
            "rebuild-replace",
            "replacement question",
            "replacement answer",
            true,
        ),
    ] {
        let mut request = continuation_to("rebuild-root", vec![user_message(input)]);
        let mut parent = seed.materialize_parent(&owner, &mut request).await.unwrap();
        if replace {
            parent.replace_effective_history = true;
            parent.replacement_client_items = Some(freeze_items(vec![user_message(input)]));
        }
        let delta = responses_request(vec![user_message(input)]);
        let mut output = AiResponse::new(id, "model");
        output.push_output_text(answer);
        seed.save(GenerationChainCommit {
            principal: owner.clone(),
            id: id.into(),
            parent: &parent,
            request_delta: &delta,
            effective_request: Some(if replace { &delta } else { &request }),
            response: output,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .unwrap();
    }

    let backend = Arc::new(crate::turn_chain::test_store().await);
    for id in ["rebuild-root", "rebuild-append", "rebuild-replace"] {
        let mut node = seed_backend
            .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new(id))
            .await
            .unwrap()
            .pop()
            .unwrap();
        // Legacy ancestors without client_history are not reusable heads, but
        // their retained client input/output still contributes to descendants.
        let prefix = if id == "rebuild-root" {
            node.payload
                .as_object_mut()
                .unwrap()
                .remove("client_history");
            None
        } else {
            Some(ReusablePrefixMetadata {
                namespace: "previous-generation-projection".into(),
                fingerprint: id.into(),
                item_count: 1,
                completed_at: 123,
                projection: None,
            })
        };
        backend
            .commit(TurnCommit {
                principal: owner.clone(),
                id: node.id,
                kind: node.kind,
                parent_id: node.parent_id,
                payload_version: node.payload_version,
                payload: node.payload,
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: prefix,
            })
            .await
            .unwrap();
    }
    let mut before = Vec::new();
    for id in ["rebuild-append", "rebuild-replace"] {
        before.push(
            backend
                .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new(id))
                .await
                .unwrap(),
        );
    }
    let chain = GenerationChain::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(1),
    );
    chain.rebuild_prefixes().await.unwrap();

    for (index, (id, history)) in [
        (
            "rebuild-append",
            vec![
                user_message("root question"),
                AiItem::output_text("root answer"),
                user_message("append question"),
                AiItem::output_text("append answer"),
            ],
        ),
        (
            "rebuild-replace",
            vec![
                user_message("replacement question"),
                AiItem::output_text("replacement answer"),
            ],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let after = backend
            .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new(id))
            .await
            .unwrap();
        assert_eq!(after.len(), before[index].len());
        for (old, new) in before[index].iter().zip(&after) {
            assert_eq!(new.id, old.id);
            assert_eq!(new.parent_id, old.parent_id);
            assert_eq!(new.payload_version, old.payload_version);
            assert_eq!(new.payload, old.payload);
        }
        let mut continuation = history;
        continuation.push(user_message("continue"));
        let resumed = chain
            .begin(owner.clone(), responses_request(continuation))
            .await
            .unwrap();
        assert_eq!(resumed.parent_id(), Some(id));
        assert!(items_equal(
            &resumed.request_delta().items,
            &[user_message("continue")],
        ));
    }
}

#[tokio::test]
async fn reasoning_tracking_metadata_does_not_fork_generation_history() {
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut a = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .unwrap();
    let mut output_a = AiResponse::new("upstream-a", "model");
    output_a.push_output_text("answer-a");
    a.stage(&mut output_a, &generation_source(), None);
    a.persist().await.unwrap();
    let history_a = vec![
        question,
        AiItem::output_text("answer-a"),
        user_message("next"),
    ];
    let mut b = chain
        .begin(owner.clone(), responses_request(history_a.clone()))
        .await
        .unwrap();
    assert_eq!(b.parent.parent_id.as_deref(), Some(a.id()));
    let mut reasoning = AiItem::reasoning(
        vec!["summary".into()],
        vec!["content".into()],
        Some("ciphertext".into()),
    );
    reasoning.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            serde_json::json!({"__open_responses_item_fields": {
                "internal_chat_message_metadata_passthrough": {"trace": "opaque"},
                "metadata": {"turn_id": "trace-turn"}
            }}),
        ),
    );
    let mut output_b = AiResponse::new("upstream-b", "model");
    output_b.items = vec![reasoning];
    b.stage(&mut output_b, &generation_source(), None);
    b.persist().await.unwrap();

    // Simulate durable indexes written by the previous projection, without
    // changing immutable payloads or parent edges.
    let crate::turn_chain::SqlTurnChainStore::Sqlite(pool) = backend.as_ref() else {
        unreachable!()
    };
    {
        let mut writer = pool.acquire_write().await.unwrap();
        sqlx::query("UPDATE turn_nodes SET prefix_namespace = 'old-controls', prefix_fingerprint = 'old-projection', prefix_item_count = 99, prefix_projection_version = 999, prefix_controls_fingerprint = 'stale-controls', prefix_context_fingerprint = 'stale-context' WHERE id = ?")
            .bind(b.id()).execute(&mut *writer).await.unwrap();
        writer.finish().await.unwrap();
    }
    let mut rebuild = PrefixRebuild::default();
    backend
        .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &mut |node, completed_at| {
            rebuild.decode(node, completed_at)
        })
        .await
        .unwrap();
    let restarted = GenerationChain::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let mut replay = history_a;
    replay.push(AiItem::reasoning(
        vec!["summary".into()],
        vec!["content".into()],
        Some("ciphertext".into()),
    ));
    replay.push(user_message("continue"));
    let projection: (Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT prefix_projection_version, prefix_controls_fingerprint, prefix_context_fingerprint FROM turn_nodes WHERE id = ?",
    )
    .bind(b.id())
    .fetch_one(pool)
    .await
    .unwrap();
    let expected_history = ClientHistoryState::from_request_with_count(
        &responses_request(replay.clone()),
        &replay[..replay.len() - 1],
    )
    .0;
    assert_eq!(
        projection,
        (
            Some(i64::from(
                stravia_runtime_contract::turn_chain::REUSABLE_PREFIX_PROJECTION_VERSION
            )),
            Some(expected_history.controls_fingerprint),
            Some(expected_history.context_fingerprint),
        ),
        "namespace invalidation must rebuild the proof from retained client history",
    );
    let resumed = restarted
        .begin(owner.clone(), responses_request(replay.clone()))
        .await
        .unwrap();
    assert_eq!(resumed.parent.parent_id.as_deref(), Some(b.id()));
    assert_eq!(resumed.request_delta.items.len(), 1);

    for changed in [
        AiItem::reasoning(
            vec!["summarycontent".into()],
            vec![],
            Some("ciphertext".into()),
        ),
        AiItem::reasoning(
            vec!["summary".into()],
            vec!["content".into()],
            Some("changed".into()),
        ),
    ] {
        replay[3] = changed;
        let fork = restarted
            .begin(owner.clone(), responses_request(replay.clone()))
            .await
            .unwrap();
        assert_eq!(fork.parent.parent_id.as_deref(), Some(a.id()));
    }
    let mut unknown_extension = AiItem::reasoning(
        vec!["summary".into()],
        vec!["content".into()],
        Some("ciphertext".into()),
    );
    unknown_extension.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            serde_json::json!({"__open_responses_item_fields": {"future_model_content": "different"}}),
        ),
    );
    replay[3] = unknown_extension;
    let fork = restarted
        .begin(owner.clone(), responses_request(replay))
        .await
        .unwrap();
    assert_eq!(fork.parent.parent_id.as_deref(), Some(a.id()));
    let nodes = restarted
        .store
        .turn_chain
        .materialize(&owner, TurnNodeKind::Response, &TurnNodeId::new(b.id()))
        .await
        .unwrap();
    assert_eq!(
        nodes[1].parent_id.as_ref().map(TurnNodeId::as_str),
        Some(a.id())
    );
    assert!(
        serde_json::to_string(&nodes[1].payload)
            .unwrap()
            .contains("internal_chat_message_metadata_passthrough")
    );
    let mut rebuild = PrefixRebuild::default();
    backend
        .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &mut |node, completed_at| {
            rebuild.decode(node, completed_at)
        })
        .await
        .unwrap();
    sqlx::query("UPDATE turn_node_retention SET expires_at = 0 WHERE node_pk = (SELECT node_pk FROM turn_nodes WHERE id = ?)")
        .bind(a.id())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE turn_nodes SET prefix_namespace = 'old-controls' WHERE id = ?")
        .bind(b.id())
        .execute(pool)
        .await
        .unwrap();
    // Prefix rebuilds must leave the immutable stored payload byte-identical.
    let payload_before: Vec<u8> =
        sqlx::query_scalar("SELECT payload FROM turn_node_payloads WHERE node_pk = (SELECT node_pk FROM turn_nodes WHERE id = ?)")
            .bind(b.id())
            .fetch_one(pool)
            .await
            .unwrap();
    let mut rebuild = PrefixRebuild::default();
    backend
        .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &mut |node, completed_at| {
            rebuild.decode(node, completed_at)
        })
        .await
        .unwrap();
    let unavailable: (Option<String>, Option<String>, Vec<u8>) = sqlx::query_as(
        "SELECT n.prefix_namespace, parent.id, p.payload FROM turn_nodes n LEFT JOIN turn_nodes parent ON parent.node_pk = n.parent_pk JOIN turn_node_payloads p ON p.node_pk = n.node_pk WHERE n.id = ?",
    )
    .bind(b.id())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(unavailable.0, None);
    assert_eq!(unavailable.1.as_deref(), Some(a.id()));
    assert_eq!(unavailable.2, payload_before);
}

#[tokio::test]
async fn observation_tool_result_evidence_requires_a_pending_parent_call() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    let question = user_message("question");
    let call = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
        id: "pending-call".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    });
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .unwrap();
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![call.clone()];
    root.stage(&mut response, &generation_source(), None);
    root.persist().await.unwrap();
    let result = AiItem::function_call_output("pending-call", serde_json::json!("result"));
    let history = vec![question, call, result.clone(), user_message("also this")];
    let mut continuation = chain
        .begin(owner.clone(), responses_request(history.clone()))
        .await
        .unwrap();
    assert!(continuation.has_matching_pending_tool_result());
    let mut unmatched = history.clone();
    unmatched[2] = AiItem::function_call_output("unknown-call", serde_json::json!("result"));
    assert!(
        !chain
            .begin(owner.clone(), responses_request(unmatched))
            .await
            .unwrap()
            .has_matching_pending_tool_result()
    );
    let mut final_response = AiResponse::new("upstream-final", "model");
    final_response.push_output_text("done");
    continuation.stage(&mut final_response, &generation_source(), None);
    continuation.persist().await.unwrap();
    let mut replay = history;
    replay.extend([AiItem::output_text("done"), result]);
    let repeated = chain.begin(owner, responses_request(replay)).await.unwrap();
    assert_eq!(
        repeated.parent.parent_id.as_deref(),
        Some(continuation.id())
    );
    assert!(!repeated.has_matching_pending_tool_result());
}

#[tokio::test]
async fn edited_tool_history_does_not_merge_a_later_user_into_tool_continuation() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    let question = user_message("question");
    let call = AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
        id: "lookup-call".into(),
        name: "lookup".into(),
        arguments: "{}".into(),
    });
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .unwrap();
    let mut response = AiResponse::new("upstream-call", "model");
    response.items = vec![call.clone()];
    root.stage(&mut response, &generation_source(), None);
    root.persist().await.unwrap();
    let mut completed = chain
        .begin(
            owner.clone(),
            responses_request(vec![
                question.clone(),
                call.clone(),
                AiItem::function_call_output("lookup-call", serde_json::json!("original result")),
            ]),
        )
        .await
        .unwrap();
    let mut answer = AiResponse::new("upstream-answer", "model");
    answer.push_output_text("finished");
    completed.stage(&mut answer, &generation_source(), None);
    completed.persist().await.unwrap();

    let edited = AiItem::function_call_output("lookup-call", serde_json::json!("edited result"));
    let followup = user_message("new task");
    let resumed = chain
        .begin(
            owner,
            responses_request(vec![
                question,
                call,
                edited.clone(),
                AiItem::output_text("finished"),
                followup.clone(),
            ]),
        )
        .await
        .unwrap();
    assert_eq!(resumed.parent.parent_id.as_deref(), Some(root.id()));
    assert!(!resumed.has_matching_pending_tool_result());
    assert_eq!(
        serde_json::to_value(&resumed.request().items[2]).unwrap(),
        serde_json::to_value(&edited).unwrap()
    );
    assert_eq!(
        serde_json::to_value(resumed.request().items.last().unwrap()).unwrap(),
        serde_json::to_value(&followup).unwrap()
    );
}

#[tokio::test]
async fn materialization_cache_never_serves_an_expired_durable_chain() {
    let backend = Arc::new(ImmediatelyExpiredTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_immediately_expired".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response: AiResponse::new("upstream", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    for _ in 0..2 {
        let materialized = store
            .materialize_generation(&owner, &TurnNodeId::new("resp_immediately_expired"))
            .await
            .expect("materialize response");
        let encoded = serde_json::to_vec(materialized.as_ref()).unwrap();
        let decoded: MaterializedGeneration = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.expires_at, materialized.expires_at);
        assert!(decoded.expires_at <= std::time::Instant::now());
    }

    assert_eq!(
        backend
            .materializations
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "expired durable chains must be re-read instead of served from cache"
    );
}

#[tokio::test]
async fn shared_materialization_cache_preserves_arcs_deadlines_and_rebuilds_on_miss() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let cache = crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024);
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        cache.clone(),
    );
    let owner = principal("shared-cache-owner");
    let id = TurnNodeId::new("resp_shared_cache");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: id.to_string(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let first = store.materialize_generation(&owner, &id).await.unwrap();
    let warm = store.materialize_generation(&owner, &id).await.unwrap();
    assert!(
        Arc::ptr_eq(&first, &warm),
        "TinyUFO hits must not clone history"
    );
    assert_eq!(backend.reads(), 1);
    let encoded = serde_json::to_vec(first.as_ref()).unwrap();
    let decoded: MaterializedGeneration = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.expires_at, first.expires_at);
    assert_eq!(
        serde_json::to_value(&decoded.effective_items).unwrap(),
        serde_json::to_value(&first.effective_items).unwrap()
    );

    cache
        .remove(
            "generation.materialized",
            &super::super::store::generation_cache_key(&owner.continuation_key(), &id, None),
        )
        .await;
    let rebuilt = store.materialize_generation(&owner, &id).await.unwrap();
    assert_eq!(backend.reads(), 2, "a shared-cache miss must re-read SQL");
    assert!(!Arc::ptr_eq(&first, &rebuilt));
    assert_eq!(
        serde_json::to_value(&rebuilt.effective_items).unwrap(),
        serde_json::to_value(&first.effective_items).unwrap()
    );
}

async fn save_generation_with_effective_request(
    store: &GenerationChainStore,
    owner: &Principal,
    id: &TurnNodeId,
    effective: &AiRequest,
) {
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("ok");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: id.to_string(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("q")]),
            effective_request: Some(effective),
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save generation with effective request controls");
}

#[tokio::test]
async fn materialization_cache_admits_small_effective_request_controls() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let cache = crate::runtime_cache::RuntimeCache::tinyufo(256 * 1024);
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        cache.clone(),
    );
    let owner = principal("effective-weight-small");
    let id = TurnNodeId::new("resp_effective_weight_small");
    let mut effective = responses_request(Vec::new());
    effective.instructions = Some("be brief".into());
    effective.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "lookup".into(),
        description: Some("look something up".into()),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
        }),
        strict: None,
        defer_loading: None,
    }]);
    effective.response_format = Some(
        stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
            name: "answer".into(),
            description: None,
            schema: serde_json::json!({
                "type": "object",
                "properties": {"answer": {"type": "string"}},
            }),
            strict: Some(true),
        },
    );
    effective.parameters.metadata = Some(std::collections::BTreeMap::from([(
        "key".into(),
        "value".into(),
    )]));
    save_generation_with_effective_request(&store, &owner, &id, &effective).await;

    let first = store.materialize_generation(&owner, &id).await.unwrap();
    let warm = store.materialize_generation(&owner, &id).await.unwrap();
    assert_eq!(
        backend.reads(),
        1,
        "small effective request controls must stay within the materialization budget"
    );
    assert!(
        Arc::ptr_eq(&first, &warm),
        "an admitted materialization must be served from the shared cache"
    );
    let encoded = serde_json::to_vec(first.as_ref()).unwrap();
    let decoded: MaterializedGeneration = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        serde_json::to_value(decoded.effective_request.as_ref()).unwrap(),
        serde_json::to_value(first.effective_request.as_ref()).unwrap(),
        "estimating retained bytes must not change effective request serde behavior"
    );
}

#[tokio::test]
async fn materialization_cache_rejects_oversized_effective_request_controls() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let cache = crate::runtime_cache::RuntimeCache::tinyufo(256 * 1024);
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        cache.clone(),
    );
    let owner = principal("effective-weight-oversized");

    // 32 KiB of serialized JSON, but 16K retained empty schema objects.
    let structural_schema = serde_json::json!({
        "type": "array",
        "prefixItems": (0..16 * 1024).map(|_| serde_json::json!({})).collect::<Vec<_>>(),
    });
    assert!(
        serde_json::to_vec(&structural_schema).unwrap().len() < 64 * 1024,
        "the structural schema case must not be detected only by serialized length"
    );

    let mut instructions = responses_request(Vec::new());
    instructions.instructions = Some("i".repeat(300 * 1024));

    let mut tool_parameters = responses_request(Vec::new());
    tool_parameters.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "lookup".into(),
        description: None,
        parameters: structural_schema.clone(),
        strict: None,
        defer_loading: None,
    }]);

    let mut native_tools = responses_request(Vec::new());
    native_tools.parameters.native_tools = vec![
        stravia_runtime_contract::protocol::ir::NativeTool::ToolSearch(
            stravia_runtime_contract::protocol::ir::ToolSearchTool {
                execution: None,
                description: None,
                parameters: Some(structural_schema.clone()),
            },
        ),
    ];

    let mut response_format = responses_request(Vec::new());
    response_format.response_format = Some(
        stravia_runtime_contract::protocol::ir::ResponseFormat::JsonSchema {
            name: "answer".into(),
            description: Some("d".repeat(300 * 1024)),
            schema: serde_json::json!({"type": "object"}),
            strict: None,
        },
    );

    let mut parameters = responses_request(Vec::new());
    parameters.parameters.metadata = Some(std::collections::BTreeMap::from([(
        "key".into(),
        "m".repeat(300 * 1024),
    )]));

    for (case, effective) in [
        ("instructions", instructions),
        ("tool_parameters", tool_parameters),
        ("native_tools", native_tools),
        ("response_format", response_format),
        ("parameters", parameters),
    ] {
        let id = TurnNodeId::new(format!("resp_effective_weight_{case}"));
        save_generation_with_effective_request(&store, &owner, &id, &effective).await;
        let reads = backend.reads();
        let first = store.materialize_generation(&owner, &id).await.unwrap();
        let key = super::super::store::generation_cache_key(&owner.continuation_key(), &id, None);
        assert!(
            cache
                .get::<MaterializedGeneration>("generation.materialized", &key)
                .await
                .is_none(),
            "{case}: oversized effective request controls must not enter TinyUFO"
        );
        let warm = store.materialize_generation(&owner, &id).await.unwrap();
        assert_eq!(
            backend.reads(),
            reads + 2,
            "{case}: an oversized materialization must be rebuilt instead of cached"
        );
        assert!(!Arc::ptr_eq(&first, &warm));
    }
}

#[tokio::test]
async fn materialization_cache_rejects_structurally_large_item_metadata() {
    let backend = Arc::new(CountingParentTurnChainStore {
        inner: crate::turn_chain::test_store().await,
        materializations: std::sync::atomic::AtomicUsize::new(0),
    });
    let cache = crate::runtime_cache::RuntimeCache::tinyufo(256 * 1024);
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(60),
        cache.clone(),
    );
    let owner = principal("item-metadata-weight");
    let id = TurnNodeId::new("resp_item_metadata_weight");
    let metadata = serde_json::Value::Object(
        (0..3072)
            .map(|index| (format!("extension_{index}"), serde_json::Value::Null))
            .collect(),
    );
    let mut item = user_message("metadata");
    item.meta =
        Some(stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(metadata.clone()));
    let effective = responses_request(vec![item]);
    save_generation_with_effective_request(&store, &owner, &id, &effective).await;

    let first = store.materialize_generation(&owner, &id).await.unwrap();
    assert_eq!(
        serde_json::to_value(first.effective_items[0].meta.as_deref()).unwrap(),
        metadata
    );
    let key = super::super::store::generation_cache_key(&owner.continuation_key(), &id, None);
    assert!(
        cache
            .get::<MaterializedGeneration>("generation.materialized", &key)
            .await
            .is_none(),
        "many small metadata fields must include map storage in the cache budget"
    );
    let warm = store.materialize_generation(&owner, &id).await.unwrap();
    assert_eq!(
        backend.reads(),
        2,
        "oversized metadata must require authoritative reads"
    );
    assert!(!Arc::ptr_eq(&first, &warm));
}

#[tokio::test]
async fn materialization_cache_does_not_outlive_the_generation_ttl() {
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite database");
    let cache = crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024);
    let backend = Arc::new(ControlledDiscoveryTurnChainStore::new(
        crate::turn_chain::SqlTurnChainStore::sqlite(pool.clone()),
    ));
    let store = GenerationChainStore::from_turn_chain(
        backend.clone(),
        Duration::from_secs(1),
        cache.clone(),
    );
    let owner = principal("owner");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    let before_save = chrono::Utc::now().timestamp_millis();
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_expiring".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let after_save = chrono::Utc::now().timestamp_millis();
    let id = TurnNodeId::new("resp_expiring");
    let committed_expiry: i64 =
        sqlx::query_scalar("SELECT r.expires_at FROM turn_node_retention r JOIN turn_nodes n ON n.node_pk = r.node_pk WHERE n.id = ?")
            .bind(id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!((before_save + 1000..=after_save + 1000).contains(&committed_expiry));

    // Control the durable fixture deadline before filling the cache; initial
    // materialization must not race the configured one-second retention window.
    let durable_expiry = chrono::Utc::now().timestamp_millis() + 60_000;
    let live_rows = sqlx::query("UPDATE turn_node_retention SET expires_at = ? WHERE node_pk = (SELECT node_pk FROM turn_nodes WHERE id = ?)")
        .bind(durable_expiry)
        .bind(id.as_str())
        .execute(&pool)
        .await
        .expect("set controlled durable deadline");
    assert_eq!(live_rows.rows_affected(), 1);
    let mut cached = responses_request(vec![user_message("follow up")]);
    crate::router::stamp_previous_response_id(&mut cached, "resp_expiring");
    store
        .materialize_parent(&owner, &mut cached)
        .await
        .expect("materialize cached response");

    let key = super::super::store::generation_cache_key(&owner.continuation_key(), &id, None);
    let materialized = cache
        .get::<MaterializedGeneration>("generation.materialized", &key)
        .await
        .expect("initial parent materialization must fill the cache");
    assert_eq!(
        materialized.expires_at,
        *backend
            .materialized_deadlines
            .lock()
            .get(id.as_str())
            .expect("SQL must supply the initial materialization certificate"),
        "the cache must inherit the durable generation deadline exactly"
    );
    assert!(materialized.expires_at > std::time::Instant::now());
    assert!(items_equal(
        &materialized.client_items,
        &[user_message("question"), AiItem::output_text("answer")]
    ));

    // Advance both expiry certificates explicitly rather than racing a one-second
    // TTL against scheduler delays. Keep the stale entry resident so this exercises
    // the consumer's expiry check, not merely the cache backend's eviction.
    let expired_materialization = MaterializedGeneration {
        root_id: materialized.root_id.clone(),
        compaction_record_ids: materialized.compaction_record_ids.clone(),
        effective_items: materialized.effective_items.clone(),
        client_items: materialized.client_items.clone(),
        effective_request: materialized.effective_request.clone(),
        effective_system: materialized.effective_system.clone(),
        upstream_response_id: materialized.upstream_response_id.clone(),
        effective_state: materialized.effective_state.clone(),
        media_turn_messages: materialized.media_turn_messages.clone(),
        client_history: materialized.client_history.clone(),
        client_item_units: materialized.client_item_units,
        payload_version: materialized.payload_version,
        expires_at: std::time::Instant::now(),
    };
    let bytes = stravia_runtime_contract::json::serialized_len(&expired_materialization).unwrap();
    let expired_rows = sqlx::query("UPDATE turn_node_retention SET expires_at = 0 WHERE node_pk = (SELECT node_pk FROM turn_nodes WHERE id = ?)")
        .bind(id.as_str())
        .execute(&pool)
        .await
        .expect("expire durable generation");
    assert_eq!(expired_rows.rows_affected(), 1);
    cache
        .put(
            "generation.materialized",
            &key,
            Arc::new(expired_materialization),
            bytes,
            Duration::from_secs(60),
        )
        .await;
    assert!(
        cache
            .get::<MaterializedGeneration>("generation.materialized", &key)
            .await
            .expect("expired certificate must still be resident before resolution")
            .expires_at
            <= std::time::Instant::now()
    );
    let mut expired = responses_request(vec![user_message("follow up")]);
    crate::router::stamp_previous_response_id(&mut expired, "resp_expiring");
    assert_eq!(
        store
            .materialize_parent(&owner, &mut expired)
            .await
            .expect_err("expired generation must not be served from cache"),
        "previous_response_not_found"
    );
}

#[tokio::test]
async fn artifact_identity_participates_in_reusable_prefix_semantics() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite database");
    let artifacts = Arc::new(crate::agent::LocalArtifactStore::sqlite(
        pool,
        data_dir.path().join("artifacts"),
    ));
    let owner = principal("owner");
    let first = artifacts
        .ingest(
            &owner,
            "image/png",
            Some(10),
            bytes_stream(bytes::Bytes::from_static(b"same image")),
            Duration::from_secs(60),
        )
        .await
        .expect("first Artifact");
    // Stable Artifact identity reuses one final ID for identical content, so
    // the distinct-identity case must upload genuinely different bytes.
    let second = artifacts
        .ingest(
            &owner,
            "image/png",
            Some(14),
            bytes_stream(bytes::Bytes::from_static(b"distinct image")),
            Duration::from_secs(60),
        )
        .await
        .expect("second Artifact");

    let request_for = |artifact_id: &stravia_runtime_contract::artifact::ArtifactId| {
        responses_request(vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::FileId {
                    file_id: format!("stravia://artifacts/{}", artifact_id.as_str()),
                    detail: None,
                },
                detail: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }])
    };
    let artifact_store: Arc<dyn stravia_runtime_contract::artifact::ArtifactStore> = artifacts;
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        Some(artifact_store),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let first_write = chain
        .begin(owner.clone(), request_for(&first.id))
        .await
        .expect("begin first Artifact request");
    let second_write = chain
        .begin(owner.clone(), request_for(&second.id))
        .await
        .expect("begin second Artifact request");
    let first_request = first_write.request();
    let second_request = second_write.request();

    assert_ne!(
        stravia_runtime_contract::protocol::ir::canonical::item_value(&first_request.items[0]),
        stravia_runtime_contract::protocol::ir::canonical::item_value(&second_request.items[0]),
        "distinct Artifact identities must not share a reusable prefix"
    );
    let missing = stravia_runtime_contract::artifact::ArtifactId::new("missing");
    assert_eq!(
        chain
            .begin(owner, request_for(&missing))
            .await
            .err()
            .expect("missing Artifact must reject begin"),
        BeginError::ItemReferenceNotFound
    );
}

// Re-uploading the same picture must keep one durable Artifact identity so the
// next client round continues the previous response instead of forking a new
// root, while different media still starts its own history.
#[tokio::test]
async fn reuploaded_identical_media_continues_the_persisted_generation() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::test_support::migrated_sqlite_pool()
        .await
        .expect("SQLite database");
    let artifacts = Arc::new(crate::agent::LocalArtifactStore::sqlite(
        pool,
        data_dir.path().join("artifacts"),
    ));
    let owner = principal("owner");
    let image = include_bytes!("../../../tests/fixtures/media/transparent.png");
    let first = artifacts
        .ingest(
            &owner,
            "image/png",
            Some(image.len() as u64),
            bytes_stream(bytes::Bytes::copy_from_slice(image)),
            Duration::from_secs(3600),
        )
        .await
        .expect("first Artifact");
    // The second client round re-uploads the same picture with different chunk
    // boundaries; only the final identity may react to the bytes.
    let (head, tail) = image.split_at(image.len() / 2);
    let chunks: Vec<Result<bytes::Bytes, stravia_runtime_contract::artifact::ArtifactError>> = vec![
        Ok(bytes::Bytes::copy_from_slice(head)),
        Ok(bytes::Bytes::copy_from_slice(tail)),
    ];
    let second = artifacts
        .ingest(
            &owner,
            "image/png",
            Some(image.len() as u64),
            Box::pin(futures::stream::iter(chunks)),
            Duration::from_secs(3600),
        )
        .await
        .expect("second Artifact");

    let image_item = |artifact_id: &stravia_runtime_contract::artifact::ArtifactId| AiItem {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::Image {
            source: MediaSource::FileId {
                file_id: format!("stravia://artifacts/{}", artifact_id.as_str()),
                detail: None,
            },
            detail: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let artifact_store: Arc<dyn stravia_runtime_contract::artifact::ArtifactStore> =
        artifacts.clone();
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        Some(artifact_store),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );

    let mut round_one = chain
        .begin(
            owner.clone(),
            responses_request(vec![image_item(&first.id)]),
        )
        .await
        .expect("begin first client round");
    let mut answer = AiResponse::new("upstream-a", "model");
    answer.push_output_text("same answer");
    round_one.stage(&mut answer, &generation_source(), None);
    round_one.persist().await.expect("persist first round");

    let mut round_two = chain
        .begin(
            owner.clone(),
            responses_request(vec![
                image_item(&second.id),
                AiItem::output_text("same answer"),
                user_message("next"),
            ]),
        )
        .await
        .expect("begin second client round");
    assert_eq!(
        round_two.parent_id(),
        Some(round_one.id()),
        "re-uploading identical media must continue the previous response"
    );
    assert_eq!(
        round_two.request_delta().items.len(),
        1,
        "only the new user message may count as new input"
    );
    assert_eq!(round_two.request_delta().items[0].content.to_text(), "next");
    let hydrated_references: Vec<String> = round_two
        .request()
        .items
        .iter()
        .filter_map(|item| match &item.content {
            MessageContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                ContentBlock::Image {
                    source: MediaSource::Url(url),
                    ..
                } => Some(url.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        hydrated_references,
        vec![first.reference()],
        "the continued parent must serve the same stable Artifact reference"
    );
    assert_eq!(
        first.id, second.id,
        "identical bytes under one Principal must keep one final Artifact identity"
    );

    let mut follow_up = AiResponse::new("upstream-b", "model");
    follow_up.push_output_text("follow through");
    round_two.stage(&mut follow_up, &generation_source(), None);
    round_two.persist().await.expect("persist second round");

    let altered = artifacts
        .ingest(
            &owner,
            "image/png",
            Some(13),
            bytes_stream(bytes::Bytes::from_static(b"mutated image")),
            Duration::from_secs(3600),
        )
        .await
        .expect("altered Artifact");
    let fork = chain
        .begin(
            owner.clone(),
            responses_request(vec![
                image_item(&altered.id),
                AiItem::output_text("same answer"),
                user_message("next"),
            ]),
        )
        .await
        .expect("begin altered client round");
    assert_eq!(
        fork.parent_id(),
        None,
        "different media must not continue the previous response"
    );
}

#[tokio::test]
async fn previous_response_materializes_history_and_supports_branching() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    response.items = vec![AiItem::function_call_output(
        "media-call",
        serde_json::json!({
            "path": "stravia://turns/aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "completion": "complete",
            "artifacts": [],
            "report": {
                "answer": "description",
                "artifacts": [],
                "limitations": []
            }
        }),
    )];
    response.trusted_media_turn_ids = vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()];
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_root".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    for follow_up in ["branch-a", "branch-b"] {
        let mut request = responses_request(vec![user_message(follow_up)]);
        crate::router::stamp_previous_response_id(&mut request, "resp_root");
        let active = store
            .materialize_parent(&owner, &mut request)
            .await
            .expect("materialize branch");
        assert_eq!(active.parent_id.as_deref(), Some("resp_root"));
        assert_eq!(
            active.media_turn_messages,
            vec![(1, vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()])]
        );
        assert_eq!(request.items.len(), 3);
        assert_eq!(request.items[2].content.to_text(), follow_up);
    }
}

#[tokio::test]
async fn replacement_mutation_replays_the_effective_history_without_a_hook() {
    let store = generation_store().await;
    let owner = principal("owner");
    let root = responses_request(vec![user_message("client input")]);
    let mut effective_root = root.clone();
    effective_root.items = vec![user_message("rewritten input")];
    let state =
        GenerationChainState::from_request(&effective_root, "provider", OPEN_RESPONSES_2026_04_24);
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_rewritten".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: Some(&effective_root),
            response,
            upstream_response_id: Some("upstream".into()),
            effective_state: state,
        })
        .await
        .expect("save rewritten response");

    let mut next = responses_request(vec![user_message("follow up")]);
    crate::router::stamp_previous_response_id(&mut next, "resp_rewritten");
    store
        .materialize_parent(&owner, &mut next)
        .await
        .expect("materialize rewritten parent");

    assert!(items_equal(
        &next.items,
        &[
            user_message("rewritten input"),
            AiItem::output_text("answer"),
            user_message("follow up"),
        ]
    ));
}

#[tokio::test]
async fn previous_response_materializes_the_ordered_item_graph_without_collapsing() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::thinking("reasoning", Some("opaque".into())).with_graph_metadata(
            Some("rs_1".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        ),
        AiItem::output_text("answer").with_graph_metadata(
            Some("msg_1".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        ),
        AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        })
        .with_graph_metadata(
            Some("fc_1".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        ),
    ];
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_root".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut request = responses_request(vec![user_message("follow-up")]);
    crate::router::stamp_previous_response_id(&mut request, "resp_root");

    store
        .materialize_parent(&owner, &mut request)
        .await
        .expect("materialize parent");

    assert_eq!(request.items.len(), 5);
    assert_eq!(
        request.items[1].thinking_ref(),
        Some(("reasoning", Some("opaque")))
    );
    assert_eq!(request.items[1].id_ref(), Some("rs_1"));
    assert_eq!(request.items[2].output_text_ref(), Some("answer"));
    assert_eq!(request.items[2].id_ref(), Some("msg_1"));
    assert!(request.items[3].function_call_ref().is_some());
    assert_eq!(request.items[3].id_ref(), Some("fc_1"));
    assert_eq!(request.items[4].content.to_text(), "follow-up");
}

#[tokio::test]
async fn automatic_parent_matches_a_combined_assistant_turn() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .expect("begin root");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::output_text("planning"),
        AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
    ];
    let assistant = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                text: "planning".to_owned().into(),
                cache_control: None,
            },
            stravia_runtime_contract::protocol::ir::ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "lookup".into(),
                input: serde_json::json!({"value": 1}),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![stravia_runtime_contract::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    root.stage(
        &mut response,
        &generation_source(),
        Some("upstream-response".into()),
    );
    root.persist().await.expect("persist root");
    let root_id = root.id().to_owned();

    let resumed = chain
        .begin(
            owner,
            responses_request(vec![
                question,
                assistant,
                AiItem::function_call_output("call_1", serde_json::Value::String("result".into())),
            ]),
        )
        .await
        .expect("begin resumed request");

    assert_eq!(resumed.parent.parent_id.as_deref(), Some(root_id.as_str()));
    assert_eq!(resumed.request_delta.items.len(), 1);
}

#[tokio::test]
async fn matching_prefix_prefers_ephemeral_upstream_continuation_when_transport_allows_it() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut initial = responses_request(vec![question.clone()]);
    initial.parameters.store = Some(false);
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin root");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    root.stage(
        &mut response,
        &generation_source(),
        Some("upstream-response".into()),
    );
    root.persist().await.expect("persist root");

    let mut resumed_request = responses_request(vec![
        question,
        AiItem::output_text("answer"),
        user_message("follow-up"),
    ]);
    resumed_request.parameters.store = Some(false);
    let resumed = chain
        .begin(owner.clone(), resumed_request)
        .await
        .expect("discover matching prefix");
    let lookup = chain.continuation_lookup();

    let mut without_affinity = Arc::new(resumed.request().clone());
    let mut miss_fallback = None;
    assert_eq!(
        lookup
            .prepare(
                &owner,
                crate::router::ContinuationTarget {
                    namespace: "provider:model",
                    protocol: Some(OPEN_RESPONSES_2026_04_24),
                    actual_model: "model",
                    allow_ephemeral_response: false,
                },
                &resumed.parent_target(),
                &mut without_affinity,
                &mut miss_fallback,
            )
            .await,
        None
    );
    assert!(miss_fallback.is_none());

    let mut root_request = Arc::new(responses_request(vec![user_message("new root")]));
    assert_eq!(
        lookup
            .prepare(
                &owner,
                crate::router::ContinuationTarget {
                    namespace: "provider:model",
                    protocol: Some(OPEN_RESPONSES_2026_04_24),
                    actual_model: "model",
                    allow_ephemeral_response: true,
                },
                &crate::router::GenerationParentTarget::Unresolved,
                &mut root_request,
                &mut miss_fallback,
            )
            .await,
        None,
    );
    assert!(miss_fallback.is_none());
    assert_eq!(root_request.items[0].content.to_text(), "new root");

    let mut with_affinity = Arc::new(resumed.request().clone());
    assert_eq!(
        lookup
            .prepare(
                &owner,
                crate::router::ContinuationTarget {
                    namespace: "provider:model",
                    protocol: Some(OPEN_RESPONSES_2026_04_24),
                    actual_model: "model",
                    allow_ephemeral_response: true,
                },
                &resumed.parent_target(),
                &mut with_affinity,
                &mut None,
            )
            .await
            .as_deref(),
        Some("upstream-response")
    );
    assert_eq!(with_affinity.items.len(), 1);
    assert_eq!(with_affinity.items[0].content.to_text(), "follow-up");
}

#[tokio::test]
async fn stable_session_does_not_link_semantically_changed_history() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");
    let original_user = AiItem {
        role: Role::User,
        content: MessageContent::Blocks(vec![
            stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                text: "first".to_owned().into(),
                cache_control: None,
            },
            stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                text: "transient reminder".to_owned().into(),
                cache_control: Some(
                    stravia_runtime_contract::protocol::ir::CacheControl::ephemeral(),
                ),
            },
        ]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut initial = responses_request(vec![original_user]);
    initial.instructions = Some("first controls".into());
    initial.parameters.session.id = Some("session-1".into());
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin root");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("first answer");
    root.stage(
        &mut response,
        &generation_source(),
        Some("upstream-response".into()),
    );
    root.persist().await.expect("persist root");
    let mut resumed_request = responses_request(vec![
        user_message("first"),
        AiItem::output_text("first answer"),
        user_message("second"),
    ]);
    resumed_request.instructions = Some("changed controls".into());
    resumed_request.parameters.session.id = Some("session-1".into());
    let resumed = chain
        .begin(owner, resumed_request)
        .await
        .expect("begin resumed request");

    assert!(resumed.parent.parent_id.is_none());
    assert!(!resumed.parent.replace_effective_history);
    assert_eq!(resumed.request_delta.items.len(), 3);
    assert_eq!(resumed.request().items.len(), 3);
}

#[tokio::test]
async fn controls_prefix_wins_when_same_session_candidate_does_not_match() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("owner");

    // Same session id and same controls, but different history content: the
    // session-layer index returns this candidate, then items_equal rejects it.
    let mut session_request = responses_request(vec![user_message("session alpha")]);
    session_request.instructions = Some("shared controls".into());
    session_request.parameters.session.id = Some("session-1".into());
    let mut session_root = chain
        .begin(owner.clone(), session_request)
        .await
        .expect("begin session root");
    let mut session_response = AiResponse::new("upstream", "model");
    session_response.push_output_text("session answer");
    session_root.stage(&mut session_response, &generation_source(), None);
    session_root.persist().await.expect("persist session root");

    // Sessionless generation with the same controls contributes only a
    // controls-layer candidate for the resumed prefix.
    let mut controls_request = responses_request(vec![user_message("controls beta")]);
    controls_request.instructions = Some("shared controls".into());
    let mut controls_root = chain
        .begin(owner.clone(), controls_request)
        .await
        .expect("begin controls root");
    let mut controls_response = AiResponse::new("upstream", "model");
    controls_response.push_output_text("controls answer");
    controls_root.stage(&mut controls_response, &generation_source(), None);
    controls_root
        .persist()
        .await
        .expect("persist controls root");

    // The replayed prefix matches the sessionless history, so discovery must
    // fall through the rejected session candidate into the controls layer.
    let mut resumed_request = responses_request(vec![
        user_message("controls beta"),
        AiItem::output_text("controls answer"),
        user_message("next"),
    ]);
    resumed_request.instructions = Some("shared controls".into());
    resumed_request.parameters.session.id = Some("session-1".into());
    let resumed = chain
        .begin(owner, resumed_request)
        .await
        .expect("begin resumed request");

    assert_eq!(
        resumed.parent.parent_id.as_deref(),
        Some(controls_root.id())
    );
    assert_eq!(resumed.request_delta.items.len(), 1);
    assert!(items_equal(
        &resumed.request().items,
        &[
            user_message("controls beta"),
            AiItem::output_text("controls answer"),
            user_message("next"),
        ]
    ));
}

#[tokio::test]
async fn previous_response_resolves_principal_scoped_item_references() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![AiItem::output_text("saved answer").with_graph_metadata(
        Some("msg_saved".into()),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    )];
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_items".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut request = responses_request(vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                "__open_responses_item_reference": "msg_saved"
            })),
        ),
    }]);
    crate::router::stamp_previous_response_id(&mut request, "resp_items");

    store
        .materialize_parent(&owner, &mut request)
        .await
        .expect("resolve item reference");

    assert_eq!(
        request
            .items
            .last()
            .map(|message| message.content.to_text()),
        Some("saved answer".into())
    );
    assert_eq!(
        request.items.last().and_then(AiItem::id_ref),
        Some("msg_saved")
    );

    let mut unauthorized = request.clone();
    crate::router::stamp_previous_response_id(&mut unauthorized, "resp_items");
    unauthorized.items = vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                "__open_responses_item_reference": "msg_saved"
            })),
        ),
    }];
    let error = store
        .materialize_parent(&principal("other"), &mut unauthorized)
        .await
        .expect_err("cross-principal reference must not resolve");
    assert_eq!(error, "item_reference_not_found");
}

#[tokio::test]
async fn explicit_parent_cannot_reference_another_branch_of_same_principal() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let mut parent = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("first")]),
        )
        .await
        .expect("begin parent");
    let parent_id = parent.id().to_owned();
    let mut parent_output = AiResponse::new(parent_id.clone(), "model");
    parent_output.items = vec![AiItem::output_text("first answer")];
    assert!(parent.stage(&mut parent_output, &generation_source(), None));
    parent.persist().await.expect("persist parent");

    let mut unrelated = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("other")]),
        )
        .await
        .expect("begin independent root");
    let unrelated_id = unrelated.id().to_owned();
    let mut unrelated_output = AiResponse::new(unrelated_id.clone(), "model");
    unrelated_output.items = vec![AiItem::output_text("secret")];
    assert!(unrelated.stage(&mut unrelated_output, &generation_source(), None));
    unrelated.persist().await.expect("persist independent root");

    let item_id = stravia_protocol_codec::codec::open_responses::formatter::gateway_item_id(
        "msg",
        &unrelated_id,
        0,
    );
    let mut reference = user_message("");
    reference.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            serde_json::json!({"__open_responses_item_reference": item_id}),
        ),
    );
    let mut request = responses_request(vec![reference]);
    crate::router::stamp_previous_response_id(&mut request, &parent_id);
    assert_eq!(
        chain
            .begin(owner, request)
            .await
            .err()
            .expect("reference outside the parent lineage"),
        BeginError::ItemReferenceNotFound,
    );
}

#[tokio::test]
async fn previous_response_resolves_references_to_persisted_input_items() {
    let store = generation_store().await;
    let owner = principal("owner");
    let saved_input = user_message("saved question").with_graph_metadata(
        Some("msg_client".into()),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Client,
        AiItemAudience::Provider,
    );
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_input".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![saved_input]),
            effective_request: None,
            response: AiResponse::new("answer", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut request = responses_request(vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new().into()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(
            stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
                "__open_responses_item_reference": "msg_client"
            })),
        ),
    }]);
    crate::router::stamp_previous_response_id(&mut request, "resp_input");

    store
        .materialize_parent(&owner, &mut request)
        .await
        .expect("resolve persisted input reference");

    let resolved = request.items.last().expect("resolved item");
    assert_eq!(resolved.content.to_text(), "saved question");
    assert_eq!(resolved.id_ref(), Some("msg_client"));
}

#[tokio::test]
async fn previous_response_inherits_instructions_unless_replaced() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut root = responses_request(vec![user_message("question")]);
    root.instructions = Some("root instructions".into());
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_instructions".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: None,
            response: AiResponse::new("answer", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    for (replacement, expected) in [
        (None, "root instructions"),
        (Some("replacement instructions"), "replacement instructions"),
    ] {
        let mut request = responses_request(vec![user_message("continue")]);
        request.instructions = replacement.map(str::to_owned);
        crate::router::stamp_previous_response_id(&mut request, "resp_instructions");
        store
            .materialize_parent(&owner, &mut request)
            .await
            .expect("materialize continuation");
        assert_eq!(request.instructions.as_deref(), Some(expected));
    }
    let mut explicit_null = responses_request(vec![user_message("continue")]);
    crate::router::stamp_previous_response_id(&mut explicit_null, "resp_instructions");
    explicit_null.parameters.reset(ParameterReset::Instructions);
    store
        .materialize_parent(&owner, &mut explicit_null)
        .await
        .expect("materialize explicit-null continuation");
    assert_eq!(explicit_null.instructions, None);
}

#[tokio::test]
async fn previous_response_inherits_request_configuration_and_keeps_overrides() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut root = responses_request(vec![user_message("question")]);
    root.model = "root-model".into();
    root.generation.temperature = Some(0.25);
    root.generation.max_tokens = Some(200);
    root.parameters.store = Some(false);
    root.parameters.min_p = Some(0.12);
    root.parameters.logprobs = Some(3);
    root.parameters.metadata = Some(std::collections::BTreeMap::from([(
        "trace".into(),
        "parent".into(),
    )]));
    root.parameters.cache.key = Some("prompt-bucket".into());
    root.parameters.verbosity = Some(stravia_runtime_contract::protocol::ir::Verbosity::High);
    root.response_format = Some(stravia_runtime_contract::protocol::ir::ResponseFormat::Text);
    root.parameters.native_tools.push(
        stravia_runtime_contract::protocol::ir::NativeTool::CodeExecution(
            stravia_runtime_contract::protocol::ir::CodeExecutionTool { container: None },
        ),
    );
    root.parameters.session.id = Some("parent-session".into());
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_config".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: None,
            response: AiResponse::new("answer", "root-model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut continuation = responses_request(vec![user_message("continue")]);
    continuation.model.clear();
    continuation.generation.max_tokens = Some(50);
    continuation.parameters.min_p = Some(0.0);
    continuation.parameters.logprobs = Some(0);
    continuation.parameters.metadata = Some(std::collections::BTreeMap::new());
    continuation.parameters.reset(ParameterReset::NativeTools);
    crate::router::stamp_previous_response_id(&mut continuation, "resp_config");
    store
        .materialize_parent(&owner, &mut continuation)
        .await
        .expect("materialize continuation");

    assert_eq!(continuation.model, "root-model");
    assert_eq!(continuation.generation.temperature, Some(0.25));
    assert_eq!(continuation.generation.max_tokens, Some(50));
    assert_eq!(continuation.parameters.store, None);
    assert_eq!(continuation.parameters.min_p, Some(0.0));
    assert_eq!(continuation.parameters.logprobs, Some(0));
    assert_eq!(
        continuation.parameters.metadata,
        Some(std::collections::BTreeMap::new())
    );
    assert_eq!(
        continuation.parameters.cache.key.as_deref(),
        Some("prompt-bucket")
    );
    assert!(continuation.parameters.continuation.is_none());
    assert!(continuation.parameters.session.id.is_none());
    assert!(continuation.parameters.native_tools.is_empty());
    assert!(continuation.parameters.resets(ParameterReset::NativeTools));

    let mut reset = responses_request(vec![user_message("explicit source nulls")]);
    crate::router::stamp_previous_response_id(&mut reset, "resp_config");
    reset.parameters.resets = vec![
        ParameterReset::Temperature,
        ParameterReset::CacheKey,
        ParameterReset::ResponseFormat,
        ParameterReset::Verbosity,
    ];
    store
        .materialize_parent(&owner, &mut reset)
        .await
        .expect("materialize reset controls");
    assert!(reset.generation.temperature.is_none());
    assert!(reset.parameters.cache.key.is_none());
    assert!(reset.parameters.verbosity.is_none());
    assert!(reset.response_format.is_none());
}

#[tokio::test]
async fn response_ids_are_isolated_by_principal() {
    let store = generation_store().await;
    store
        .save(GenerationChainCommit {
            principal: principal("owner").clone(),
            id: "resp_private".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("secret")]),
            effective_request: None,
            response: AiResponse::new("upstream", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");
    let mut request = responses_request(vec![user_message("steal")]);
    crate::router::stamp_previous_response_id(&mut request, "resp_private");

    assert_eq!(
        store
            .materialize_parent(&principal("other"), &mut request)
            .await
            .expect_err("other principal must not resolve the response"),
        "previous_response_not_found"
    );
}

#[tokio::test]
async fn response_history_survives_adapter_reconstruction() {
    let turn_chain: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let owner = principal("owner");
    let store = GenerationChainStore::from_turn_chain(
        Arc::clone(&turn_chain),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_persisted".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let reconstructed = GenerationChainStore::from_turn_chain(
        turn_chain,
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let mut continuation = responses_request(vec![user_message("follow-up")]);
    crate::router::stamp_previous_response_id(&mut continuation, "resp_persisted");
    reconstructed
        .materialize_parent(&owner, &mut continuation)
        .await
        .expect("materialize response history");

    assert_eq!(continuation.items.len(), 3);
}

#[tokio::test]
async fn persisted_tool_text_semantics_keep_plain_secrets_and_media_distinct() {
    let data_dir = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::from_storage(
        crate::config::GatewayConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
    .unwrap();
    let owner = principal("fresh-tool-owner");
    let secret = "ghp_8Dq7mP2vL9sX4aR6tK3nF5wH1jB0cYzUeIoG";
    let encoded = serde_json::json!([
        {"type": "text", "text": secret},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": secret}}
    ])
    .to_string();
    let plain = AiItem::function_call_output("plain", serde_json::Value::String(encoded.clone()));
    let mut media = AiItem::function_call_output("media", serde_json::Value::String(encoded));
    media.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(serde_json::json!({
            (stravia_runtime_contract::protocol::ir::TOOL_RESULT_CONTENT_KIND_META): "content_blocks"
        })),
    );
    let store = GenerationChainStore::from_turn_chain(
        Arc::clone(&gateway.turn_chains),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_tool_semantics".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &responses_request(vec![plain, media]),
            effective_request: None,
            response: AiResponse::new("upstream", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .unwrap();
    drop(store);
    let reconstructed = GenerationChainStore::from_turn_chain(
        Arc::clone(&gateway.turn_chains),
        Duration::from_secs(60),
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let mut request = responses_request(Vec::new());
    crate::router::stamp_previous_response_id(&mut request, "resp_tool_semantics");
    reconstructed
        .materialize_parent(&owner, &mut request)
        .await
        .unwrap();
    for _ in 0..2 {
        stravia_runtime_contract::hook::ContextSnapshot::from_request(
            &request,
            stravia_runtime_contract::hook::ContextCompleteness::Full,
        )
        .write_to_request(&mut request);
    }
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let mut request = Arc::new(request);
    gateway
        .redaction
        .protect(&owner, &mut request, None)
        .await
        .unwrap();
    let payloads: Vec<serde_json::Value> = request
        .items
        .iter()
        .map(|item| {
            let MessageContent::Blocks(blocks) = &item.content else {
                panic!("rebuilt tool result")
            };
            let ContentBlock::ToolResult {
                content: serde_json::Value::String(text),
                ..
            } = &blocks[0]
            else {
                panic!("encoded tool payload")
            };
            serde_json::from_str(text).unwrap()
        })
        .collect();
    assert!(!payloads[0].to_string().contains(secret));
    assert_ne!(payloads[1][0]["text"], secret);
    assert_eq!(payloads[1][1]["source"]["data"], secret);
}

#[tokio::test]
async fn response_history_survives_gateway_restart_with_sqlite() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let config = crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    };
    let owner = principal("owner");
    let gateway = crate::Gateway::new(config.clone())
        .await
        .expect("first Gateway");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    let mut write = gateway
        .generation_chains
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin response");
    let response_id = write.id().to_owned();
    write.stage(&mut response, &generation_source(), None);
    write.persist().await.expect("persist response");
    drop(gateway);

    let gateway = crate::Gateway::new(config)
        .await
        .expect("restarted Gateway");
    let mut continuation = responses_request(vec![user_message("follow-up")]);
    crate::router::stamp_previous_response_id(&mut continuation, &response_id);
    let write = gateway
        .generation_chains
        .begin(owner, continuation)
        .await
        .expect("begin after restart");

    assert_eq!(write.request().items.len(), 3);
}

#[tokio::test]
async fn compatible_parent_uses_upstream_id_and_only_new_messages() {
    let store = generation_store().await;
    let owner = principal("owner");
    let request = responses_request(vec![user_message("m1")]);
    let state =
        GenerationChainState::from_request(&request, "provider-a", OPEN_RESPONSES_2026_04_24);
    let mut response = AiResponse::new("upstream-1", "model");
    response.push_output_text("r1");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_gateway_1".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &request,
            effective_request: None,
            response,
            upstream_response_id: Some("upstream-1".into()),
            effective_state: state,
        })
        .await
        .expect("save response");

    let mut next = responses_request(vec![user_message("m2")]);
    crate::router::stamp_previous_response_id(&mut next, "resp_gateway_1");
    let active = store
        .materialize_parent(&owner, &mut next)
        .await
        .expect("materialize response");
    let state = GenerationChainState::from_request(&next, "provider-a", OPEN_RESPONSES_2026_04_24);

    let full_items = next.items.clone();
    let mut next = Arc::new(next);
    let mut fallback = None;
    assert!(super::super::continuation::prepare_upstream(
        active.parent_upstream_response_id.as_ref(),
        active.parent_state.as_ref(),
        active.parent_expires_at,
        &mut next,
        &state,
        false,
        &mut fallback
    ));
    let fallback = fallback.expect("successful continuation retains full replay");
    assert!(items_equal(&fallback.items, &full_items));
    assert!(crate::router::parent_id_from_request(&fallback).is_none());
    let mut incompatible = fallback;
    let incompatible_state = GenerationChainState::from_request(
        &incompatible,
        "another-provider",
        OPEN_RESPONSES_2026_04_24,
    );
    let mut miss_fallback = None;
    assert!(!super::super::continuation::prepare_upstream(
        active.parent_upstream_response_id.as_ref(),
        active.parent_state.as_ref(),
        active.parent_expires_at,
        &mut incompatible,
        &incompatible_state,
        false,
        &mut miss_fallback,
    ));
    assert!(miss_fallback.is_none());
    assert!(items_equal(&incompatible.items, &full_items));
    assert_eq!(next.items.len(), 1);
    assert_eq!(
        next.parameters
            .continuation
            .as_ref()
            .and_then(|value| value.previous_response_id()),
        Some("upstream-1")
    );
}

#[tokio::test]
async fn automatic_prefix_selects_exact_completed_context_and_leaves_new_items() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut root = responses_request(vec![user_message("m1")]);
    root.parameters.store = Some(false);
    let state = GenerationChainState::from_request(&root, "target-a", OPEN_RESPONSES_2026_04_24);
    let mut response = AiResponse::new("upstream-1", "model");
    response.push_output_text("r1");
    let completed_items = response.items.clone();
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_gateway_1".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: None,
            response,
            upstream_response_id: Some("upstream-1".into()),
            effective_state: state,
        })
        .await
        .expect("save reusable response");

    let mut next = responses_request(root.items.clone());
    next.items.extend(completed_items);
    next.items.push(user_message("m2"));
    let mut next = Arc::new(next);
    let discovered = store
        .discover_parent(
            &owner,
            &mut next,
            &mut DiscoveryWorkspace::new(&owner, false),
        )
        .await
        .expect("discover prefix")
        .expect("generation parent");

    assert_eq!(discovered.matched_items, 2);
    assert!(items_equal(
        &next.items,
        &[
            user_message("m1"),
            AiItem::output_text("r1"),
            user_message("m2"),
        ]
    ));
    assert_eq!(next.parameters.store, None);
}

#[tokio::test]
async fn automatic_prefix_preserves_parallel_tool_result_ids_after_duplicate_effective_call() {
    let store = generation_store().await;
    let owner = principal("owner");
    let root = chat_request(serde_json::json!([
        {"role": "user", "content": "question"},
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call_existing",
                "type": "function",
                "function": {"name": "todo", "arguments": "{}"}
            }]
        },
        {"role": "tool", "tool_call_id": "call_existing", "content": "done"}
    ]));
    let mut effective_root = root.clone();
    effective_root
        .items
        .insert(2, effective_root.items[1].clone());
    let state =
        GenerationChainState::from_request(&effective_root, "target", OPEN_RESPONSES_2026_04_24);
    let mut response = AiResponse::new("upstream", "model");
    response.items = [
        ("call_a", "glob"),
        ("call_b", "glob"),
        ("call_c", "glob"),
        ("call_d", "bash"),
    ]
    .into_iter()
    .map(|(id, name)| {
        AiItem::function_call(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        })
    })
    .collect();
    let mut client_items = root.items.clone();
    let client_output = project_client_history(
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        &response,
        &mut client_items,
    )
    .expect("project chat response");

    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_parallel_tools".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: Some(&effective_root),
            response,
            upstream_response_id: Some("upstream".into()),
            effective_state: state,
        })
        .await
        .expect("save response");

    let mut next = root;
    next.items.extend(client_output);
    for id in ["call_a", "call_b", "call_c", "call_d"] {
        next.items.push(AiItem::function_call_output(
            id,
            serde_json::Value::String(format!("{id}-result")),
        ));
    }
    let mut next = Arc::new(next);
    store
        .discover_parent(
            &owner,
            &mut next,
            &mut DiscoveryWorkspace::new(&owner, false),
        )
        .await
        .expect("discover prefix")
        .expect("generation parent");
    assert_eq!(
        next.items
            .iter()
            .filter(|item| item.role == Role::Tool)
            .filter_map(|item| item.tool_call_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["call_existing", "call_a", "call_b", "call_c", "call_d"]
    );
    stravia_protocol_codec::codec::tool_correlation::normalize_request_tool_results(Arc::make_mut(
        &mut next,
    ));

    assert_eq!(
        next.items
            .iter()
            .filter(|item| item.role == Role::Tool)
            .filter_map(|item| item.tool_call_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["call_existing", "call_a", "call_b", "call_c", "call_d"]
    );
}

#[test]
fn client_tool_result_remap_preserves_ids_present_in_effective_history() {
    let call = |id: &str, name: &str| {
        AiItem::function_call(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        })
    };
    let client_history = vec![
        call("call_existing", "todo"),
        call("call_a", "glob"),
        call("call_b", "glob"),
        call("call_c", "glob"),
        call("call_d", "bash"),
    ];
    let effective_history = vec![
        call("call_existing", "todo"),
        call("call_existing", "todo"),
        call("call_a", "glob"),
        call("call_b", "glob"),
        call("call_c", "glob"),
        call("call_d", "bash"),
    ];
    let mut delta = ["call_a", "call_b", "call_c", "call_d"]
        .into_iter()
        .map(|id| {
            AiItem::function_call_output(id, serde_json::Value::String(format!("{id}-result")))
        })
        .collect::<Vec<_>>();

    remap_client_tool_result_ids(&mut delta, &client_history, &effective_history);

    assert_eq!(
        delta
            .iter()
            .filter_map(|item| item.tool_call_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["call_a", "call_b", "call_c", "call_d"]
    );
}

#[tokio::test]
async fn automatic_prefix_never_turns_an_identical_full_request_into_an_empty_delta() {
    let store = generation_store().await;
    let owner = principal("owner");
    let root = responses_request(vec![user_message("m1")]);
    let state = GenerationChainState::from_request(&root, "target-a", OPEN_RESPONSES_2026_04_24);
    let mut response = AiResponse::new("upstream-1", "model");
    response.push_output_text("r1");
    let completed_items = response.items.clone();
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_gateway_1".into(),
            parent: &ActiveGenerationChain::default(),
            request_delta: &root,
            effective_request: None,
            response,
            upstream_response_id: Some("upstream-1".into()),
            effective_state: state,
        })
        .await
        .expect("save reusable response");

    let mut identical = root;
    identical.items.extend(completed_items);
    let mut identical = Arc::new(identical);
    assert!(
        store
            .discover_parent(
                &owner,
                &mut identical,
                &mut DiscoveryWorkspace::new(&owner, false)
            )
            .await
            .expect("discover generation parent")
            .is_none()
    );
}

#[test]
fn target_continuation_namespace_includes_every_hard_request_control() {
    let original = responses_request(vec![user_message("m1")]);
    let original_state =
        GenerationChainState::from_request(&original, "target-a", OPEN_RESPONSES_2026_04_24);

    let mut variants = Vec::new();
    let mut instructions = original.clone();
    instructions.instructions = Some("different".into());
    variants.push(instructions);
    let mut tools = original.clone();
    tools.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "lookup".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        defer_loading: None,
    }]);
    let tool_state =
        GenerationChainState::from_request(&tools, "target-a", OPEN_RESPONSES_2026_04_24);
    let mut changed_tools = tools.clone();
    changed_tools.tools.as_mut().unwrap()[0].defer_loading = Some(true);
    let changed_state =
        GenerationChainState::from_request(&changed_tools, "target-a", OPEN_RESPONSES_2026_04_24);
    assert!(!changed_state.compatible_continuation(&tool_state));
    variants.push(tools);
    let mut reasoning = original.clone();
    reasoning.reasoning.level = Some(stravia_runtime_contract::thinking::ThinkingLevel::High);
    variants.push(reasoning);
    let mut response_format = original;
    response_format.response_format =
        Some(stravia_runtime_contract::protocol::ir::ResponseFormat::JsonObject);
    variants.push(response_format);

    for variant in variants {
        let state =
            GenerationChainState::from_request(&variant, "target-a", OPEN_RESPONSES_2026_04_24);
        assert!(!state.compatible_continuation(&original_state));
    }
}

#[test]
fn exact_item_comparison_keeps_reasoning_media_and_unknown_semantics() {
    let reasoning = AiItem::reasoning(
        vec!["summary".into()],
        vec!["opaque reasoning".into()],
        Some("encrypted".into()),
    );
    let changed_reasoning = AiItem::reasoning(
        vec!["summary".into()],
        vec!["different".into()],
        Some("encrypted".into()),
    );
    assert!(!items_equal(&[reasoning], &[changed_reasoning]));

    let image = AiItem {
        role: Role::User,
        content: MessageContent::Blocks(vec![ContentBlock::Image {
            source: MediaSource::Url("https://example.com/a.png".into()),
            detail: Some("high".into()),
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut changed_image = image.clone();
    let MessageContent::Blocks(blocks) = &mut changed_image.content else {
        panic!("image blocks");
    };
    let ContentBlock::Image { detail, .. } = &mut blocks[0] else {
        panic!("image block");
    };
    *detail = Some("low".into());
    let image_value = stravia_runtime_contract::protocol::ir::canonical::item_value(&image);
    let changed_image_value =
        stravia_runtime_contract::protocol::ir::canonical::item_value(&changed_image);
    assert_ne!(
        image_value, changed_image_value,
        "media semantics collapsed: {image_value}"
    );

    assert!(!items_equal(
        &[AiItem::unknown(
            serde_json::json!({"type": "future", "value": 1})
        )],
        &[AiItem::unknown(
            serde_json::json!({"type": "future", "value": 2})
        )]
    ));
}
#[test]
fn native_upstream_reuse_requires_the_same_provider_model() {
    let request = responses_request(vec![user_message("new")]);
    let persisted =
        GenerationChainState::from_request(&request, "provider-a", OPEN_RESPONSES_2026_04_24)
            .with_provider_model("provider-model-a");
    let candidate = persisted.clone().with_provider_model("provider-model-b");

    assert!(!persisted.compatible_continuation(&candidate));
}

#[test]
fn refreshing_request_semantics_tracks_provider_effective_controls() {
    let mut request = responses_request(vec![user_message("new")]);
    let mut state =
        GenerationChainState::from_request(&request, "provider-a", OPEN_RESPONSES_2026_04_24);
    let prior_settings = state.canonical_controls_fingerprint.clone();
    request.generation.temperature = Some(0.7);
    request.parallel_tool_calls = Some(true);

    state.refresh_request_semantics(&request);
    let expected =
        GenerationChainState::from_request(&request, "provider-a", OPEN_RESPONSES_2026_04_24);

    assert_ne!(state.canonical_controls_fingerprint, prior_settings);
    assert!(state.compatible_continuation(&expected));
}

#[test]
fn native_upstream_reuse_requires_the_same_request_settings() {
    let request = responses_request(vec![user_message("new")]);
    let persisted =
        GenerationChainState::from_request(&request, "provider-a", OPEN_RESPONSES_2026_04_24)
            .with_provider_model("provider-model-a");
    let mut hotter = request;
    hotter.generation.temperature = Some(0.7);
    let candidate =
        GenerationChainState::from_request(&hotter, "provider-a", OPEN_RESPONSES_2026_04_24)
            .with_provider_model("provider-model-a");

    assert!(!persisted.compatible_continuation(&candidate));
}

#[test]
fn empty_tools_match_absent_tools_for_native_upstream_reuse() {
    let mut without_tools = responses_request(vec![user_message("new")]);
    without_tools.tools = None;
    let persisted =
        GenerationChainState::from_request(&without_tools, "provider-a", OPEN_RESPONSES_2026_04_24)
            .with_provider_model("provider-model-a");
    let mut empty_tools = without_tools;
    empty_tools.tools = Some(Vec::new());
    let candidate =
        GenerationChainState::from_request(&empty_tools, "provider-a", OPEN_RESPONSES_2026_04_24)
            .with_provider_model("provider-model-a");

    assert!(persisted.compatible_continuation(&candidate));
}

#[test]
fn native_upstream_reuse_requires_persisted_open_responses_target() {
    for (egress, store_value) in [
        (OPEN_RESPONSES_2026_04_24, Some(false)),
        (ANTHROPIC_MESSAGES_2023_06_01, Some(true)),
    ] {
        let mut request = responses_request(vec![user_message("new")]);
        request.parameters.store = store_value;
        let state = GenerationChainState::from_request(&request, "provider", egress);
        let active = ActiveGenerationChain {
            parent_upstream_response_id: Some("upstream".into()),
            parent_state: Some(state.clone()),
            ..ActiveGenerationChain::default()
        };

        let mut request = Arc::new(request);
        let mut fallback = None;
        assert!(!super::super::continuation::prepare_upstream(
            active.parent_upstream_response_id.as_ref(),
            active.parent_state.as_ref(),
            active.parent_expires_at,
            &mut request,
            &state,
            false,
            &mut fallback
        ));
        assert!(fallback.is_none());
        assert_eq!(request.items.len(), 1);
    }
}

#[test]
fn changed_response_semantics_disable_upstream_reuse() {
    let mut original = AiResponse::new("upstream-1", "model");
    original.push_output_text("original");
    let mut changed = original.clone();
    changed.id = "resp_gateway_1".into();
    assert!(GenerationChainStore::preserves_upstream_response(
        &original, &changed
    ));
    changed.replace_output_text("rewritten");
    assert!(!GenerationChainStore::preserves_upstream_response(
        &original, &changed
    ));
}

#[test]
fn changed_url_media_disables_upstream_reuse() {
    let mut original = AiResponse::new("upstream-1", "model");
    original.items.push(AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::Image {
            source: MediaSource::Url("https://example.test/original.png".into()),
            detail: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    });
    let mut changed = original.clone();
    let MessageContent::Blocks(blocks) = &mut changed.items[0].content else {
        panic!("image blocks");
    };
    let ContentBlock::Image { source, .. } = &mut blocks[0] else {
        panic!("image block");
    };
    *source = MediaSource::Url("https://example.test/rewritten.png".into());

    assert!(!GenerationChainStore::preserves_upstream_response(
        &original, &changed
    ));
}

fn observation_payload(
    client_delta: Vec<AiItem>,
    client_history_mutation: Option<EffectiveHistoryMutation<'_>>,
    client_output: Option<Vec<AiItem>>,
    effective_output_text: &str,
) -> serde_json::Value {
    let mut effective_output = AiResponse::new("private-upstream", "model");
    effective_output.push_output_text(effective_output_text);
    serde_json::to_value(PersistedResponseNode {
        client_delta: RequestDelta {
            messages: Cow::Owned(freeze_items(client_delta)),
            system: None,
        },
        client_output: client_output.map(freeze_items),
        client_history_mutation,
        compaction_record_ids: Vec::new(),
        effective_history_mutation: None,
        effective_system: None,
        effective_output: FrozenResponse::new(effective_output),
        client_history: None,
        trusted_media_turn_ids: Vec::new(),
        upstream_response_id: None,
        effective_state: GenerationChainState::default(),
        effective_request: None,
    })
    .expect("observation payload")
}

async fn commit_observation_node(
    store: &dyn TurnChainStore,
    owner: &Principal,
    id: &str,
    parent: Option<&str>,
    payload: serde_json::Value,
    idle_ttl: Duration,
) {
    store
        .commit(TurnCommit {
            id: TurnNodeId::new(id),
            kind: TurnNodeKind::Response,
            parent_id: parent.map(TurnNodeId::new),
            principal: owner.clone(),
            payload_version: RESPONSE_PAYLOAD_VERSION,
            payload,
            idle_ttl,
            reusable_prefix: None,
        })
        .await
        .expect("commit observation node");
}

#[tokio::test]
async fn ancestor_client_item_visitor_yields_each_complete_root_to_head_history() {
    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(
        Arc::clone(&backend),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("observation-owner");

    commit_observation_node(
        backend.as_ref(),
        &owner,
        "observation-root",
        None,
        observation_payload(
            vec![user_message("root question")],
            None,
            Some(vec![AiItem::output_text("public root")]),
            "private root",
        ),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "observation-append",
        Some("observation-root"),
        observation_payload(
            Vec::new(),
            Some(EffectiveHistoryMutation::Append {
                items: freeze_items(vec![user_message("follow up")]).into(),
            }),
            None,
            "fallback answer",
        ),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "observation-replace",
        Some("observation-append"),
        observation_payload(
            Vec::new(),
            Some(EffectiveHistoryMutation::Replace {
                items: freeze_items(vec![user_message("edited question")]).into(),
            }),
            Some(vec![AiItem::output_text("public replacement")]),
            "private replacement",
        ),
        Duration::from_secs(60),
    )
    .await;

    let mut snapshots = Vec::new();
    let available = chain
        .visit_ancestor_client_items(
            &owner,
            "observation-replace",
            &mut None,
            |node, items, input_end| {
                assert_eq!(
                    input_end,
                    match node {
                        "observation-root" | "observation-replace" => 1,
                        "observation-append" => 3,
                        _ => panic!("unexpected node"),
                    }
                );
                snapshots.push((
                node.to_owned(),
                items
                    .iter()
                    .map(|item| {
                        serde_json::json!({"role": item.role, "content": item.content.to_text()})
                    })
                    .collect::<Vec<_>>(),
            ))
            },
        )
        .await
        .expect("visit complete chain");

    assert!(available);
    assert_eq!(
        serde_json::to_value(snapshots).expect("public history snapshots"),
        serde_json::json!([
            ["observation-root", [
                {"role": "user", "content": "root question"},
                {"role": "assistant", "content": "public root"}
            ]],
            ["observation-append", [
                {"role": "user", "content": "root question"},
                {"role": "assistant", "content": "public root"},
                {"role": "user", "content": "follow up"},
                {"role": "assistant", "content": "fallback answer"}
            ]],
            ["observation-replace", [
                {"role": "user", "content": "edited question"},
                {"role": "assistant", "content": "public replacement"}
            ]]
        ])
    );
}

#[tokio::test]
async fn ancestor_client_item_workspace_keeps_branches_and_principals_isolated() {
    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(
        Arc::clone(&backend),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("workspace-observation-owner");
    for (id, parent, question, answer) in [
        ("shared-root", None, "root question", "root answer"),
        ("left", Some("shared-root"), "left question", "left answer"),
        (
            "right",
            Some("shared-root"),
            "right question",
            "right answer",
        ),
    ] {
        commit_observation_node(
            backend.as_ref(),
            &owner,
            id,
            parent,
            observation_payload(vec![user_message(question)], None, None, answer),
            Duration::from_secs(60),
        )
        .await;
    }
    let mut workspace = None;
    assert!(
        chain
            .visit_ancestor_client_items(&owner, "left", &mut workspace, |_, _, _| {})
            .await
            .expect("left branch")
    );
    let mut right = Vec::new();
    assert!(
        chain
            .visit_ancestor_client_items(&owner, "right", &mut workspace, |id, items, input_end| {
                if id == "right" {
                    right = items.iter().map(|item| item.content.to_text()).collect();
                    assert_eq!(input_end, 3);
                }
            })
            .await
            .expect("right branch")
    );
    assert_eq!(
        right,
        [
            "root question",
            "root answer",
            "right question",
            "right answer"
        ]
    );
    let mut foreign = Vec::new();
    assert!(
        !chain
            .visit_ancestor_client_items(
                &principal("foreign-workspace-owner"),
                "shared-root",
                &mut workspace,
                |id, _, _| foreign.push(id.to_owned()),
            )
            .await
            .expect("foreign principal declines")
    );
    assert!(foreign.is_empty());
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "expired-child",
        Some("shared-root"),
        observation_payload(vec![user_message("expired")], None, None, "expired"),
        Duration::ZERO,
    )
    .await;
    assert!(
        !chain
            .visit_ancestor_client_items(&owner, "expired-child", &mut workspace, |_, _, _| {
                panic!("expired suffix must not emit cached partial evidence")
            })
            .await
            .expect("expired suffix declines")
    );
}

#[tokio::test]
async fn ancestor_client_item_visitor_declines_unavailable_chains_without_partial_evidence() {
    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(
        Arc::clone(&backend),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("unavailable-observation-owner");

    let mut visited = Vec::new();
    assert!(
        !chain
            .visit_ancestor_client_items(&owner, "missing-node", &mut None, |node, _, _| {
                visited.push(node.to_owned())
            })
            .await
            .expect("missing chain declines")
    );
    assert!(visited.is_empty());

    commit_observation_node(
        backend.as_ref(),
        &owner,
        "expired-observation",
        None,
        observation_payload(vec![user_message("expired")], None, None, "expired answer"),
        Duration::ZERO,
    )
    .await;

    assert!(
        !chain
            .visit_ancestor_client_items(&owner, "expired-observation", &mut None, |node, _, _| {
                visited.push(node.to_owned())
            })
            .await
            .expect("expired chain declines")
    );
    assert!(visited.is_empty());
}

#[tokio::test]
async fn ancestor_client_item_visitor_rejects_invalid_tail_before_yielding_root() {
    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(
        Arc::clone(&backend),
        Duration::from_secs(60),
        None,
        crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
    );
    let owner = principal("invalid-observation-owner");

    commit_observation_node(
        backend.as_ref(),
        &owner,
        "valid-observation-root",
        None,
        observation_payload(vec![user_message("valid")], None, None, "answer"),
        Duration::from_secs(60),
    )
    .await;
    commit_observation_node(
        backend.as_ref(),
        &owner,
        "invalid-observation-tail",
        Some("valid-observation-root"),
        serde_json::json!({"invalid": true}),
        Duration::from_secs(60),
    )
    .await;

    let mut visited = Vec::new();
    assert!(
        chain
            .visit_ancestor_client_items(
                &owner,
                "invalid-observation-tail",
                &mut None,
                |node, _, _| { visited.push(node.to_owned()) }
            )
            .await
            .is_err()
    );
    assert!(visited.is_empty());
}
