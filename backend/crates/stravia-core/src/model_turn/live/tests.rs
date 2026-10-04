use super::{
    AttemptDeadlineGuard, UPSTREAM_FINISHED, UPSTREAM_NOT_STARTED, UPSTREAM_STARTED,
    UpstreamLocalWork, VendorDriverHandle, strip_rejected_protected_reasoning, thinking_authority,
};
use crate::history_marker::{
    ReasoningRejections, ThinkingProvenance, ThinkingSource, protected_payload_digests,
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
fn thinking_provenance_trusts_signing_scope_not_target_wiring() {
    let target = ThinkingSource {
        namespace: "target-namespace".into(),
        protocol: Some(ANTHROPIC_MESSAGES_2023_06_01.into()),
        actual_model: "target-model".into(),
        target_id: "target-id".into(),
        authority: Some("deployment-a".into()),
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
            ThinkingProvenance::Foreign,
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
            ThinkingProvenance::Verified,
        ),
        (
            "same protocol, other deployment or credential",
            &target,
            stamped(other_scope.clone()),
            ThinkingProvenance::Unknown,
        ),
        (
            "same protocol family under an alias",
            &target,
            stamped(ThinkingSource {
                protocol: Some("anthropic-messages".into()),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
        (
            "other protocol",
            &target,
            stamped(ThinkingSource {
                protocol: Some(OPEN_RESPONSES_2026_04_24.into()),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign,
        ),
        (
            "plugin protocol against a known protocol",
            &target,
            stamped(ThinkingSource {
                protocol: Some("acme/custom-wire".into()),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Foreign,
        ),
        (
            "same plugin protocol identity",
            &plugin,
            stamped(ThinkingSource {
                authority: Some("deployment-b".into()),
                ..plugin.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
        (
            "record without protocol",
            &target,
            stamped(ThinkingSource {
                protocol: None,
                ..other_scope.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
        (
            "model change on a protocol without model binding",
            &target,
            stamped(ThinkingSource {
                actual_model: "other-model".into(),
                ..other_scope.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
        (
            "Gemini model change",
            &gemini,
            stamped(ThinkingSource {
                actual_model: "other-model".into(),
                authority: Some("deployment-b".into()),
                ..gemini.clone()
            }),
            ThinkingProvenance::Foreign,
        ),
        (
            "Gemini same model, other deployment",
            &gemini,
            stamped(ThinkingSource {
                authority: Some("deployment-b".into()),
                ..gemini.clone()
            }),
            ThinkingProvenance::Unknown,
        ),
        (
            "legacy record with the same namespace",
            &target,
            stamped(ThinkingSource {
                authority: None,
                ..target.clone()
            }),
            ThinkingProvenance::Verified,
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
            ThinkingProvenance::Foreign,
        ),
    ];
    let rejections = ReasoningRejections::default();
    for (case, current, item, expected) in &cases {
        assert_eq!(current.provenance(item, &rejections), *expected, "{case}");
    }

    // 被当前签发作用域拒绝过的载荷一律剥离，包括本可证明同源的载荷；
    // 其它签发作用域不受影响。
    rejections.record(
        "deployment-a",
        protected_payload_digests(std::slice::from_ref(&native)),
    );
    let verified = stamped(target.clone());
    assert_eq!(
        target.provenance(&verified, &rejections),
        ThinkingProvenance::Foreign
    );
    assert_eq!(
        target.provenance(&native, &rejections),
        ThinkingProvenance::Foreign
    );
    assert_eq!(
        other_scope.provenance(&native, &rejections),
        ThinkingProvenance::Unknown
    );
    let unrelated = AiItem::thinking("reasoning", Some("other-signature".into()));
    assert_eq!(
        target.provenance(&unrelated, &rejections),
        ThinkingProvenance::Unknown
    );
}

#[test]
fn rejected_protected_reasoning_strips_unverified_before_verified() {
    let source = ThinkingSource {
        namespace: "target-namespace".into(),
        protocol: Some(ANTHROPIC_MESSAGES_2023_06_01.into()),
        actual_model: "target-model".into(),
        target_id: "target-id".into(),
        authority: Some("deployment-a".into()),
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
    let rejections = ReasoningRejections::default();
    let mut request = AiRequest::new("model", vec![verified.clone(), unknown]);
    let mut stage = 0;

    assert!(strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source,
        &rejections
    ));
    assert_eq!(stage, 1);
    assert_eq!(signatures(&request), vec!["own-signature"]);

    assert!(strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source,
        &rejections
    ));
    assert_eq!(stage, 2);
    assert!(signatures(&request).is_empty());

    // 只有已证实载荷时直接进入全剥离，不浪费一次重试。
    let mut request = AiRequest::new("model", vec![verified]);
    let mut stage = 0;
    assert!(strip_rejected_protected_reasoning(
        &mut request,
        &mut stage,
        &source,
        &rejections
    ));
    assert_eq!(stage, 2);
    assert!(signatures(&request).is_empty());
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
