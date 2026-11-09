use super::{
    AttemptDeadlineGuard, UPSTREAM_FINISHED, UPSTREAM_NOT_STARTED, UPSTREAM_STARTED,
    UpstreamLocalWork, VendorDriverHandle, strip_rejected_protected_reasoning, thinking_authority,
};
use crate::history_marker::{
    ReasoningRejections, RejectedReasoning, ThinkingProvenance, ThinkingSource,
};
use crate::router::{RoutePolicyState, TargetRuntimeState};
use std::sync::{Arc, atomic::AtomicU8};
use std::time::{Duration, Instant};
use stravia_runtime_contract::Deadline;
use stravia_runtime_contract::protocol::ids::{
    ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, OPEN_RESPONSES_2026_04_24,
};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest};

#[test]
fn generation_session_affinity_isolates_principal_target_and_root() {
    let owner = stravia_runtime_contract::Principal::new("owner");
    let other = stravia_runtime_contract::Principal::new("other");
    let affinity = super::generation_session_affinity(&owner, "provider:model", "root-one");
    for (principal, target, root) in [
        (&other, "provider:model", "root-one"),
        (&owner, "provider:other-model", "root-one"),
        (&owner, "other-provider:model", "root-one"),
        (&owner, "provider:model", "root-two"),
    ] {
        assert_ne!(
            affinity,
            super::generation_session_affinity(principal, target, root)
        );
    }
}

#[test]
fn parent_session_transport_affinity_matches_real_parent_and_is_scoped() {
    let principal = stravia_runtime_contract::Principal::new("owner");
    let other = stravia_runtime_contract::Principal::new("other");
    let target = "provider:model";
    let mut parent = AiRequest::new("model", Vec::new());
    parent.parameters.session.id = Some("real-parent-session".into());
    let fingerprint = crate::generation_chain::generation_session_fingerprint(&parent).unwrap();
    let actual_parent_affinity =
        super::namespace_fingerprint(&(principal.continuation_key(), target, fingerprint.as_str()));
    let mut child = AiRequest::new("model", Vec::new());
    child.parameters.session.id = Some("child-session".into());
    child.parameters.session.parent_id = parent.parameters.session.id.clone();
    let label = child.parameters.session.parent_id.as_deref().unwrap();
    let child_parent_affinity = super::scoped_session_transport_affinity(&principal, target, label);
    assert_eq!(child_parent_affinity, actual_parent_affinity);
    assert_ne!(child_parent_affinity, label);
    assert_ne!(
        child_parent_affinity,
        super::scoped_session_transport_affinity(&other, target, label),
    );
    assert_ne!(
        child_parent_affinity,
        super::scoped_session_transport_affinity(&principal, "other:model", label),
    );
    assert_ne!(
        child_parent_affinity,
        super::scoped_session_transport_affinity(&principal, target, "different-parent"),
    );
}

#[test]
fn thinking_provenance_trusts_signing_scope_not_target_wiring() {
    let target = ThinkingSource {
        namespace: "target-namespace".into(),
        protocol: Some(ANTHROPIC_MESSAGES_2023_06_01.into()),
        actual_model: "target-model".into(),
        target_id: "target-id".into(),
        authority: Some("deployment-a".into()),
        issued: Vec::new(),
    };
    let native = AiItem::thinking("reasoning", Some("signature".into()));
    let stamped = |source: ThinkingSource| {
        let mut item = native.clone();
        source.stamp_item(&mut item);
        item
    };
    let other_scope = ThinkingSource {
        authority: Some("deployment-b".into()),
        ..target.clone()
    };
    let gemini = ThinkingSource {
        protocol: Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.into()),
        ..target.clone()
    };
    let plugin = ThinkingSource {
        protocol: Some("acme/custom-wire".into()),
        ..target.clone()
    };
    let mut malformed = native.clone();
    malformed.meta = Some(
        stravia_runtime_contract::protocol::ir::AiItemMetadata::boxed(
            serde_json::json!({"__stravia_thinking_source": "invalid"}),
        ),
    );
    let cases = [
        (
            "no provenance record",
            &target,
            native.clone(),
            ThinkingProvenance::Unknown,
        ),
        (
            "malformed record",
            &target,
            malformed,
            ThinkingProvenance::Unknown,
        ),
        (
            // 路由 Target、代理或选项不同，只要签发作用域相同仍可证明。
            "same scope, different Target wiring",
            &target,
            stamped(ThinkingSource {
                namespace: "other-namespace".into(),
                actual_model: "other-model".into(),
                target_id: "other-target".into(),
                ..target.clone()
            }),
            ThinkingProvenance::Own,
        ),
        (
            "same protocol, other deployment or credential",
            &target,
            stamped(other_scope.clone()),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "same protocol family under an alias",
            &target,
            stamped(ThinkingSource {
                protocol: Some("anthropic-messages".into()),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "other protocol",
            &target,
            stamped(ThinkingSource {
                protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "plugin protocol against a known protocol",
            &target,
            stamped(ThinkingSource {
                protocol: Some("acme/custom-wire".into()),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "same plugin protocol identity",
            &plugin,
            stamped(ThinkingSource {
                authority: Some("deployment-b".into()),
                ..plugin.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "record without protocol",
            &target,
            stamped(ThinkingSource {
                protocol: None,
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "model change on a protocol without model binding",
            &target,
            stamped(ThinkingSource {
                actual_model: "other-model".into(),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "Gemini model change",
            &gemini,
            stamped(ThinkingSource {
                actual_model: "other-model".into(),
                authority: Some("deployment-b".into()),
                ..gemini.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "Gemini same model, other deployment",
            &gemini,
            stamped(ThinkingSource {
                authority: Some("deployment-b".into()),
                ..gemini.clone()
            }),
            ThinkingProvenance::Foreign("deployment-b".into()),
        ),
        (
            "legacy record with the same namespace",
            &target,
            stamped(ThinkingSource {
                authority: None,
                ..target.clone()
            }),
            ThinkingProvenance::Own,
        ),
        (
            "legacy record with another namespace",
            &target,
            stamped(ThinkingSource {
                namespace: "other-namespace".into(),
                authority: None,
                ..target.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
        (
            "legacy record with another namespace and protocol",
            &target,
            stamped(ThinkingSource {
                namespace: "other-namespace".into(),
                protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
                authority: None,
                ..target.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
    ];
    for (case, current, item, expected) in &cases {
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) = &item.content
        else {
            panic!("blocks");
        };
        assert_eq!(current.provenance(item, &blocks[0]), *expected, "{case}");
    }

    let rejections = ReasoningRejections::default();
    rejections.record(
        "deployment-a",
        [RejectedReasoning::Origin("deployment-b".into())],
    );
    let mut unrelated = AiItem::thinking("reasoning", Some("new-signature".into()));
    other_scope.stamp_item(&mut unrelated);
    let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) = &unrelated.content
    else {
        panic!("blocks");
    };
    assert!(rejections.rejected(
        "deployment-a",
        target.provenance(&unrelated, &blocks[0]),
        &blocks[0]
    ));
    assert!(!rejections.rejected(
        "deployment-c",
        target.provenance(&unrelated, &blocks[0]),
        &blocks[0]
    ));
    assert!(!rejections.rejected("deployment-a", ThinkingProvenance::Own, &blocks[0]));
}

#[test]
fn rejected_protected_reasoning_strips_only_foreign_and_unknown_once() {
    let source = ThinkingSource {
        namespace: "target-namespace".into(),
        protocol: Some(ANTHROPIC_MESSAGES_2023_06_01.into()),
        actual_model: "target-model".into(),
        target_id: "target-id".into(),
        authority: Some("deployment-a".into()),
        issued: Vec::new(),
    };
    let mut verified = AiItem::thinking("own reasoning", Some("own-signature".into()));
    verified.role = stravia_runtime_contract::protocol::ir::Role::Assistant;
    source.stamp_item(&mut verified);
    let mut unknown = AiItem::thinking("client reasoning", Some("client-signature".into()));
    unknown.role = stravia_runtime_contract::protocol::ir::Role::Assistant;
    let signatures = |request: &AiRequest| {
        request
            .items
            .iter()
            .filter_map(|item| item.thinking_ref().and_then(|(_, signature)| signature))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut request = AiRequest::new("model", vec![verified.clone(), unknown]);
    let mut stage = 0;

    assert!(strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source
    ));
    assert_eq!(stage, 1);
    assert_eq!(signatures(&request), vec!["own-signature"]);

    assert!(!strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source
    ));
    assert_eq!(stage, 1);
    assert_eq!(signatures(&request), vec!["own-signature"]);

    // 只有本上游载荷时不剥离，也不启动恢复。
    let mut request = AiRequest::new("model", vec![verified]);
    let mut stage = 0;
    assert!(!strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source
    ));
    assert_eq!(stage, 0);
    assert_eq!(signatures(&request), vec!["own-signature"]);
}

#[test]
fn reasoning_removal_projects_duplicate_boundary_gaps_without_collapsing_ids() {
    let source = ThinkingSource {
        namespace: "target".into(),
        protocol: Some(ANTHROPIC_MESSAGES_2023_06_01.into()),
        actual_model: "model".into(),
        target_id: "target".into(),
        authority: Some("authority".into()),
        issued: Vec::new(),
    };
    let mut removed = AiItem::thinking("", Some("foreign".into()));
    removed.role = stravia_runtime_contract::protocol::ir::Role::Assistant;
    let duplicate = AiItem::output_text("same");
    let mut request = AiRequest::new("model", vec![duplicate.clone(), removed, duplicate]);
    let mut map = crate::model_turn::HistoryBoundaryMap {
        version: 1,
        window_id: "window".into(),
        coverage: "complete".into(),
        boundaries: [("before", 1), ("after", 2), ("end", 3)]
            .into_iter()
            .map(|(id, item_index)| crate::model_turn::HistoryBoundary {
                id: id.into(),
                item_index,
            })
            .collect(),
        current_boundary_id: "end".into(),
    };
    assert!(super::strip_rejected_protected_reasoning_with_boundaries(
        &mut request,
        &mut 0,
        &source,
        Some(&mut map),
    ));
    assert_eq!(request.items.len(), 2);
    assert_eq!(
        map.boundaries
            .iter()
            .map(|entry| (entry.id.as_str(), entry.item_index))
            .collect::<Vec<_>>(),
        vec![("before", 1), ("after", 1), ("end", 2)]
    );
}

#[test]
fn continuation_suffix_projection_preserves_full_fallback_boundaries() {
    let full = crate::model_turn::HistoryBoundaryMap {
        version: 1,
        window_id: "window".into(),
        coverage: "complete".into(),
        boundaries: [("first", 1), ("at_split", 2), ("current", 4)]
            .into_iter()
            .map(|(id, item_index)| crate::model_turn::HistoryBoundary {
                id: id.into(),
                item_index,
            })
            .collect(),
        current_boundary_id: "current".into(),
    };
    let mut suffix = full.clone();
    suffix.retain_suffix(2);
    assert_eq!(
        suffix
            .boundaries
            .iter()
            .map(|entry| (entry.id.as_str(), entry.item_index))
            .collect::<Vec<_>>(),
        vec![("current", 2)]
    );
    assert_eq!(
        full.boundaries
            .iter()
            .map(|entry| (entry.id.as_str(), entry.item_index))
            .collect::<Vec<_>>(),
        vec![("first", 1), ("at_split", 2), ("current", 4)]
    );
}

#[test]
fn protected_reasoning_payload_inventory_and_dispatch_metadata_follow_actual_request() {
    use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent, Role};
    let source = ThinkingSource {
        namespace: "current".into(),
        protocol: Some(ANTHROPIC_MESSAGES_2023_06_01.into()),
        actual_model: "model".into(),
        target_id: "target".into(),
        authority: Some("current-authority".into()),
        issued: Vec::new(),
    };
    let foreign = ThinkingSource {
        protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
        authority: Some("foreign-authority".into()),
        ..source.clone()
    };
    let mut item = AiItem::thinking("own", Some("own-signature".into()));
    item.role = Role::Assistant;
    source.stamp_item(&mut item);
    let MessageContent::Blocks(blocks) = &mut item.content else {
        panic!("blocks");
    };
    blocks.push(ContentBlock::Thinking {
        thinking: "inserted".into(),
        signature: Some("inserted-signature".into()),
    });
    let mut external = AiItem::thinking("external", Some("external-signature".into()));
    external.role = Role::Assistant;
    foreign.stamp_item(&mut external);
    let MessageContent::Blocks(blocks) = &item.content else {
        panic!("blocks");
    };
    assert_eq!(
        source.provenance(&item, &blocks[0]),
        ThinkingProvenance::Own
    );
    assert_eq!(
        source.provenance(&item, &blocks[1]),
        ThinkingProvenance::Unknown
    );
    let mut legacy = item.clone();
    let mut legacy_source = ThinkingSource::from_item(&legacy).unwrap();
    legacy_source.issued.clear();
    legacy
        .meta
        .as_mut()
        .unwrap()
        .insert_graph_extension(
            "__stravia_thinking_source",
            serde_json::to_value(legacy_source).unwrap(),
        )
        .unwrap();
    assert_eq!(
        source.provenance(&legacy, &blocks[1]),
        ThinkingProvenance::Own
    );
    let mut request = AiRequest::new("model", vec![item, external]);
    let rejections = ReasoningRejections::default();
    assert!(!stravia_protocol_codec::transform::prepare_thinking_replay(
        &mut request,
        |item, block| {
            !rejections.rejected("current-authority", source.provenance(item, block), block)
        }
    ));
    let mut metadata = Default::default();
    super::update_protected_reasoning_metadata(&mut metadata, &request, &source);
    assert_eq!(
        metadata.get("protected_reasoning_untrusted"),
        Some(&serde_json::Value::Bool(true))
    );
    let rejected = super::rejected_protected_reasoning(&request, &source);
    let mut stage = 0;
    assert!(strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source
    ));
    super::update_protected_reasoning_metadata(&mut metadata, &request, &source);
    assert!(!metadata.contains_key("protected_reasoning_untrusted"));
    let MessageContent::Blocks(blocks) = &request.items[0].content else {
        panic!("blocks");
    };
    assert!(
        matches!(&blocks[0], ContentBlock::Thinking { signature: Some(signature), .. } if signature == "own-signature")
    );
    rejections.record("current-authority", rejected);
    // 同一来源新载荷命中上游对；未知来源仍只按原载荷记忆。
    let mut new_external = AiItem::thinking("new", Some("new-external-signature".into()));
    new_external.role = Role::Assistant;
    foreign.stamp_item(&mut new_external);
    let mut next = AiRequest::new("model", vec![new_external]);
    assert!(stravia_protocol_codec::transform::prepare_thinking_replay(
        &mut next,
        |item, block| {
            !rejections.rejected("current-authority", source.provenance(item, block), block)
        }
    ));
    let inserted = ContentBlock::RedactedThinking {
        data: "inserted-signature".into(),
    };
    assert!(rejections.rejected("current-authority", ThinkingProvenance::Unknown, &inserted));
    let unrelated = ContentBlock::RedactedThinking {
        data: "other-signature".into(),
    };
    assert!(!rejections.rejected("current-authority", ThinkingProvenance::Unknown, &unrelated));
}

#[test]
fn thinking_authority_changes_only_with_signing_scope() {
    let provider = stravia_vendor_sdk::ProviderSnapshot {
        provider_id: "provider".into(),
        channel: "default".into(),
        base_url: "https://api.example.test".into(),
        protocol: ANTHROPIC_MESSAGES_2023_06_01.to_string(),
        options: Default::default(),
        credentials: [("api_key".to_string(), serde_json::json!("key-a"))].into(),
        model: None,
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: Default::default(),
    };
    let anthropic = ANTHROPIC_MESSAGES_2023_06_01.to_string();
    let base = thinking_authority(&anthropic, &provider, None, "model-a");

    let mut rewired = provider.clone();
    rewired.provider_id = "other-provider".into();
    rewired
        .options
        .insert("zdr".into(), serde_json::json!(true));
    assert_eq!(
        thinking_authority(&anthropic, &rewired, None, "model-b"),
        base
    );

    let mut moved = provider.clone();
    moved.base_url = "https://other.example.test".into();
    assert_ne!(
        thinking_authority(&anthropic, &moved, None, "model-a"),
        base
    );
    let mut rekeyed = provider.clone();
    rekeyed
        .credentials
        .insert("api_key".into(), serde_json::json!("key-b"));
    assert_ne!(
        thinking_authority(&anthropic, &rekeyed, None, "model-a"),
        base
    );
    assert_ne!(
        thinking_authority(&anthropic, &provider, Some("connection"), "model-a"),
        base
    );

    let gemini = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA.to_string();
    assert_ne!(
        thinking_authority(&gemini, &provider, None, "model-a"),
        thinking_authority(&gemini, &provider, None, "model-b")
    );
}

fn armed_deadline_guard(
    state: &RoutePolicyState,
    deadline: Deadline,
    upstream_state: bool,
) -> AttemptDeadlineGuard {
    AttemptDeadlineGuard {
        state: state.clone(),
        target_key: "provider:model".into(),
        epoch: 0,
        retry_budget: 0,
        cooldown_ms: 120_000,
        deadline,
        upstream_state: Arc::new(AtomicU8::new(if upstream_state {
            UPSTREAM_STARTED
        } else {
            UPSTREAM_NOT_STARTED
        })),
        armed: true,
    }
}

#[tokio::test]
async fn expired_armed_guard_records_an_upstream_failure() {
    let state = RoutePolicyState::default();
    let deadline = Deadline::from_now(Duration::from_millis(20));
    let pending = {
        let state = state.clone();
        let deadline = deadline.clone();
        async move {
            let _guard = armed_deadline_guard(&state, deadline, true);
            std::future::pending::<()>().await
        }
    };
    while !deadline.is_exceeded() {
        tokio::task::yield_now().await;
    }
    let _ = tokio::time::timeout(Duration::from_millis(50), pending).await;
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::CoolingDown
    );
}

#[tokio::test]
async fn expired_live_driver_drop_cools_target_for_next_selection() {
    let state = RoutePolicyState::default();
    let cancellation = stravia_runtime_contract::CancellationToken::new();
    let handle = VendorDriverHandle {
        join: Some(tokio::spawn(std::future::pending::<()>())),
        cancellation: cancellation.clone(),
        publication_completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        deadline_guard: Some(armed_deadline_guard(
            &state,
            Deadline::fixed(Instant::now() - Duration::from_secs(1)),
            true,
        )),
    };
    drop(handle);
    assert!(cancellation.is_cancelled());
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::CoolingDown
    );
}

#[tokio::test]
async fn dropped_armed_guard_does_not_record_user_cancel() {
    let state = RoutePolicyState::default();
    let pending = {
        let state = state.clone();
        async move {
            let _guard =
                armed_deadline_guard(&state, Deadline::from_now(Duration::from_secs(3600)), true);
            std::future::pending::<()>().await
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(10), pending).await;
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::Available
    );
}

#[test]
fn expired_guard_before_provider_send_does_not_record_failure() {
    let state = RoutePolicyState::default();
    drop(armed_deadline_guard(
        &state,
        Deadline::fixed(Instant::now() - Duration::from_secs(1)),
        false,
    ));
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::Available
    );
}

#[test]
fn expired_local_delivery_keeps_target_available_for_next_selection() {
    let state = RoutePolicyState::default();
    let guard = armed_deadline_guard(
        &state,
        Deadline::fixed(Instant::now() - Duration::from_secs(1)),
        true,
    );
    {
        let _local_work = UpstreamLocalWork::begin(
            &guard.upstream_state,
            Deadline::fixed(Instant::now() - Duration::from_secs(1)),
        );
    }
    drop(guard);
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::Available
    );
}

#[test]
fn expired_guard_after_upstream_completion_keeps_target_available_for_next_selection() {
    let state = RoutePolicyState::default();
    let guard = armed_deadline_guard(
        &state,
        Deadline::fixed(Instant::now() - Duration::from_secs(1)),
        true,
    );
    guard.upstream_state.store(
        UPSTREAM_STARTED | UPSTREAM_FINISHED,
        std::sync::atomic::Ordering::Release,
    );
    drop(guard);
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::Available
    );
}

#[test]
fn disarmed_deadline_guard_does_not_record_failure() {
    let state = RoutePolicyState::default();
    let mut guard = armed_deadline_guard(
        &state,
        Deadline::fixed(Instant::now() - Duration::from_secs(1)),
        true,
    );
    guard.disarm();
    drop(guard);
    assert_eq!(
        state.target_status("provider:model").state,
        TargetRuntimeState::Available
    );
}
