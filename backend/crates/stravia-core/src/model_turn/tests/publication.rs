use super::*;

// The real MappingStore seam can hold intern or publication acknowledgements
// after the database commits. Cancellation is not evidence that commit failed.
struct HeldPublicationStore {
    inner: Arc<dyn stravia_credential_protection::store::MappingStore>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail: bool,
    expire_intern: bool,
    held_intern: Option<Arc<HeldInternAcknowledgement>>,
    starts: AtomicUsize,
    cancel_on_release: Mutex<Option<CancellationToken>>,
}

#[derive(Default)]
struct HeldInternAcknowledgement {
    committed: tokio::sync::Notify,
    release: tokio::sync::Notify,
    acknowledged: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl stravia_credential_protection::store::MappingStore for HeldPublicationStore {
    async fn active(
        &self,
        principal: &Principal,
    ) -> Result<
        Vec<stravia_credential_protection::store::Mapping>,
        stravia_runtime_contract::redaction::RedactionError,
    > {
        self.inner.active(principal).await
    }

    async fn intern(
        &self,
        principal: &Principal,
        secrets: &[String],
    ) -> Result<
        stravia_credential_protection::store::InternedMappings,
        stravia_runtime_contract::redaction::RedactionError,
    > {
        let mut result = self.inner.intern(principal, secrets).await?;
        if let Some(held) = &self.held_intern {
            held.committed.notify_one();
            held.release.notified().await;
            held.acknowledged.notify_one();
        }
        if self.expire_intern {
            for mapping in &mut result.mappings {
                mapping.expires_at = 0;
            }
        }
        Ok(result)
    }

    async fn publish(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), stravia_runtime_contract::redaction::RedactionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if !self.fail {
            self.inner.publish(principal, references, retention).await?;
        }
        self.entered.notify_one();
        self.release.notified().await;
        if let Some(cancellation) = self.cancel_on_release.lock().take() {
            cancellation.cancel();
        }
        if self.fail {
            Err(stravia_runtime_contract::redaction::RedactionError::Storage)
        } else {
            Ok(())
        }
    }

    async fn extend_retention(
        &self,
        principal: &Principal,
        references: &[String],
        retention: Duration,
    ) -> Result<(), stravia_runtime_contract::redaction::RedactionError> {
        self.inner
            .extend_retention(principal, references, retention)
            .await
    }

    async fn cleanup_expired(
        &self,
    ) -> Result<u64, stravia_runtime_contract::redaction::RedactionError> {
        self.inner.cleanup_expired().await
    }
}

#[tokio::test]
async fn committed_discovery_survives_dropped_protection_before_intern_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
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
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let principal = Principal::new("cancelled-discovery-owner");
    let held = Arc::new(HeldInternAcknowledgement::default());
    let store = Arc::new(HeldPublicationStore {
        inner: gateway.redaction.mappings.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        fail: false,
        expire_intern: false,
        held_intern: Some(held.clone()),
        starts: AtomicUsize::new(0),
        cancel_on_release: Mutex::new(None),
    });
    gateway.redaction = crate::reversible_redaction::ReversibleRedaction::new(
        gateway.storage.clone(),
        store.clone(),
        gateway.redaction.custom_rules.clone(),
    );
    let observer = credential_observer(&gateway, &principal);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    let protection = {
        let redaction = gateway.redaction.clone();
        let principal = principal.clone();
        let observer = observer.clone();
        let mut request = request.clone();
        tokio::spawn(async move {
            redaction
                .protect(&principal, &mut request, Some(&observer))
                .await
        })
    };
    held.committed.notified().await;
    assert_eq!(store.inner.active(&principal).await.unwrap().len(), 1);
    protection.abort();
    assert!(matches!(protection.await, Err(error) if error.is_cancelled()));
    observer.finish(crate::interaction_observation::RunOutcome {
        delivery: None,
        client_output_committed: false,
        delivery_completed_at: None,
        status: "cancelled".into(),
        terminal_reason: Some("cancelled".into()),
        generation_node_id: None,
        generation_root_id: None,
    });
    // Terminal delivery must not wait for the held mapping acknowledgement.
    let before_ack = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert!(before_ack.items.is_empty());
    let forest = gateway
        .observation
        .query_forest(Default::default())
        .await
        .unwrap();
    let interaction = &forest.roots[0].interactions[0];
    let detail = gateway
        .observation
        .get_interaction(&interaction.id, Default::default())
        .await
        .unwrap()
        .unwrap();
    let terminal = detail.runs[0]
        .events
        .iter()
        .find(|event| event.kind == "run_finished")
        .expect("cancellation is observable before the mapping acknowledgement");
    assert_eq!(terminal.payload["status"], "cancelled");
    held.release.notify_one();
    // This current-thread test cannot resume between acknowledgement and the
    // synchronous event emission. On the old implementation the cancelled
    // intern never acknowledges; the bounded wait then exposes the missing row.
    let _ = tokio::time::timeout(Duration::from_secs(1), held.acknowledged.notified()).await;
    let page = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].new_credential_count, 1);
    assert_eq!(page.items[0].status, "interrupted");
    assert_eq!(page.items[0].source_types, ["system_or_history"]);
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("Q8n4Vk7sT2p9X5a3Lc6D0h1R"));
    assert!(!encoded.contains(stravia_credential_protection::marker::PREFIX));
    let reused_observer = credential_observer(&gateway, &principal);
    held.release.notify_one();
    gateway
        .redaction
        .protect(&principal, &mut request, Some(&reused_observer))
        .await
        .unwrap();
    let reused = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(reused.items.len(), 1);
    assert_eq!(reused.items[0].new_credential_count, 1);
    assert_eq!(reused.items[0].status, "interrupted");
    assert_eq!(store.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn committed_discovery_survives_replacement_failure_but_failed_intern_creates_none() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let principal = Principal::new("discovery-owner");
    let store = Arc::new(HeldPublicationStore {
        inner: gateway.redaction.mappings.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        fail: false,
        expire_intern: true,
        held_intern: None,
        starts: AtomicUsize::new(0),
        cancel_on_release: Mutex::new(None),
    });
    gateway.redaction = crate::reversible_redaction::ReversibleRedaction::new(
        gateway.storage.clone(),
        store.clone(),
        gateway.redaction.custom_rules.clone(),
    );
    let observer = credential_observer(&gateway, &principal);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    assert!(matches!(
        gateway
            .redaction
            .protect(&principal, &mut request, Some(&observer))
            .await,
        Err(stravia_runtime_contract::redaction::RedactionError::InvalidText)
    ));
    observer.finish(crate::interaction_observation::RunOutcome {
        delivery: None,
        client_output_committed: false,
        delivery_completed_at: None,
        status: "failed".into(),
        terminal_reason: Some("reversible_redaction_failed".into()),
        generation_node_id: None,
        generation_root_id: None,
    });
    drop(observer);
    let page = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].new_credential_count, 1);
    assert_eq!(page.items[0].status, "interrupted");
    assert_eq!(store.inner.active(&principal).await.unwrap().len(), 1);
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("Q8n4Vk7sT2p9X5a3Lc6D0h1R"));
    assert!(!encoded.contains(stravia_credential_protection::marker::PREFIX));

    let pool = gateway._sqlite_pool.as_ref().unwrap();
    sqlx::query("CREATE TRIGGER reject_discovery_mapping BEFORE INSERT ON reversible_redaction_mappings BEGIN SELECT RAISE(FAIL, 'injected mapping failure'); END").execute(pool).await.unwrap();
    let other = Principal::new("discovery-other");
    let observer = credential_observer(&gateway, &other);
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    assert!(matches!(
        gateway
            .redaction
            .protect(&other, &mut request, Some(&observer))
            .await,
        Err(stravia_runtime_contract::redaction::RedactionError::Storage)
    ));
    drop(observer);
    assert!(store.inner.active(&other).await.unwrap().is_empty());
    assert_eq!(
        gateway
            .observation
            .credential_discoveries(Default::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn discovery_event_write_failure_does_not_change_protection_and_reports_gap() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    gateway
        .storage
        .settings()
        .set("reversible_redaction_enabled", "true")
        .await
        .unwrap();
    let principal = Principal::new("discovery-gap-owner");
    let observer = credential_observer(&gateway, &principal);
    gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_discovery_event BEFORE INSERT ON observation_events WHEN NEW.kind = 'credential_mappings_created' BEGIN SELECT RAISE(FAIL, 'injected observation failure'); END").execute(gateway._sqlite_pool.as_ref().unwrap()).await.unwrap();
    let mut request = AiRequest::new("discovery-model", Vec::new());
    request.instructions = Some("api_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"".into());
    let mappings = gateway
        .redaction
        .protect(&principal, &mut request, Some(&observer))
        .await
        .unwrap();
    assert_eq!(mappings.len(), 1);
    assert!(
        !request
            .instructions
            .as_deref()
            .unwrap()
            .contains("Q8n4Vk7sT2p9X5a3Lc6D0h1R")
    );
    observer.finish(crate::interaction_observation::RunOutcome {
        delivery: None,
        client_output_committed: false,
        delivery_completed_at: None,
        status: "completed".into(),
        terminal_reason: None,
        generation_node_id: None,
        generation_root_id: None,
    });
    drop(observer);
    let page = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert!(page.items.is_empty());
    assert!(page.observation_gap);
}

async fn held_publication_turn(
    fail: bool,
    late_reference: bool,
) -> (
    tempfile::TempDir,
    Gateway,
    ModelTurn,
    Arc<HeldPublicationStore>,
    Principal,
    (CancellationToken, Deadline),
    i64,
) {
    let (directory, mut gateway, _, key) =
        gateway_with_captured_thinking("publication-model", true, "answer <!--sr:", None).await;
    let principal = Principal::new(key.id);
    let store = Arc::new(HeldPublicationStore {
        inner: gateway.redaction.mappings.clone(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        fail,
        expire_intern: false,
        held_intern: None,
        starts: AtomicUsize::new(0),
        cancel_on_release: Mutex::new(None),
    });
    gateway.redaction = crate::reversible_redaction::ReversibleRedaction::new(
        gateway.storage.clone(),
        store.clone(),
        gateway.redaction.custom_rules.clone(),
    );
    let mut request = AiRequest::new("publication-model", Vec::new());
    let mut pending_expiry = 0;
    if !late_reference {
        let mapping = store
            .inner
            .intern(&principal, &["synthetic-secret".into()])
            .await
            .unwrap()
            .mappings
            .remove(0);
        gateway
            .storage
            .settings()
            .set("reversible_redaction_enabled", "true")
            .await
            .unwrap();
        request.instructions = Some(format!(
            "{}\napi_key = \"Q8n4Vk7sT2p9X5a3Lc6D0h1R\"",
            mapping.reference
        ));
        pending_expiry = mapping.expires_at;
    }
    let mut related = request.clone();
    let cancellation = CancellationToken::new();
    let deadline = Deadline::from_now(Duration::from_secs(300));
    let executor = LiveModelTurnExecutor::new(
        gateway.clone(),
        crate::router::continuation::ScriptedContinuation::miss(),
    );
    let run_id = stravia_runtime_contract::identifier::new_id();
    let observer = gateway
        .observation
        .observe_ingress(crate::interaction_observation::IngressStart {
            id: run_id.clone(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            protocol: "openai-compatible".into(),
        })
        .admit(
            crate::interaction_observation::RunStart {
                id: run_id,
                principal: principal.continuation_key(),
                api_key_id: None,
                api_key_name: None,
                route_id: "publication-model".into(),
                model_display_name: None,
                ingress_protocol: "openai-compatible".into(),
            },
            crate::interaction_observation::AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                )
                .into(),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    let turn = executor
        .execute(
            TurnInput::new(principal.clone(), request)
                .with_observer(observer)
                .with_execution(cancellation.clone(), deadline.clone()),
        )
        .await
        .expect("upstream completed before local publication");
    if late_reference {
        // The returned turn captured no local mappings. A related turn now adds a
        // valid reference to its shared trace through normal request protection.
        let mapping = store
            .inner
            .intern(&principal, &["related-secret".into()])
            .await
            .unwrap()
            .mappings
            .remove(0);
        pending_expiry = mapping.expires_at;
        related.instructions = Some(mapping.reference);
        gateway
            .redaction
            .protect(&principal, &mut related, None)
            .await
            .unwrap();
    }
    (
        directory,
        gateway,
        turn,
        store,
        principal,
        (cancellation, deadline),
        pending_expiry,
    )
}

async fn consume_until_publication(turn: &mut ModelTurn, store: &HeldPublicationStore) -> String {
    let mut text = String::new();
    loop {
        tokio::select! {
            biased;
            _ = store.entered.notified() => return text,
            event = turn.output.next() => match event.expect("publication remains pending").expect("delta") {
                CanonicalEvent::Delta(AiStreamDelta::TextDelta(delta))
                | CanonicalEvent::Delta(AiStreamDelta::TextDeltaWithMetadata { text: delta, .. }) => text.push_str(&delta),
                CanonicalEvent::Delta(_) => {},
                CanonicalEvent::Completed(_) | CanonicalEvent::Compacted(_) => panic!("success escaped pending publication"),
            }
        }
    }
}

#[tokio::test]
async fn canonical_completion_publishes_after_trailing_output_and_is_permanently_terminal() {
    let (_directory, gateway, mut turn, store, principal, (cancellation, _), pending_expiry) =
        held_publication_turn(false, false).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer <!--sr:"
    );
    // The select above dropped a pending next() future. Publication must survive
    // that pause and resume rather than issuing a second write.
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    store.release.notify_one();
    match turn.output.next().await.unwrap().unwrap() {
        CanonicalEvent::Completed(response) => {
            assert_eq!(response.output_text(), "answer <!--sr:")
        }
        CanonicalEvent::Delta(_) | CanonicalEvent::Compacted(_) => {
            panic!("expected generation completion")
        }
    }
    assert_eq!(store.starts.load(Ordering::SeqCst), 1);
    cancellation.cancel();
    for _ in 0..3 {
        assert!(turn.output.next().await.is_none());
    }
    drop(turn);
    assert_publication_observation(&gateway, "completed").await;
}

#[tokio::test]
async fn canonical_completion_publishes_current_shared_trace_with_empty_local_mappings() {
    let (_directory, _gateway, mut turn, store, principal, _, pending_expiry) =
        held_publication_turn(false, true).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer <!--sr:"
    );
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    store.release.notify_one();
    assert!(matches!(
        turn.output.next().await,
        Some(Ok(CanonicalEvent::Completed(_)))
    ));
    assert!(turn.output.next().await.is_none());
}

#[tokio::test]
async fn canonical_completion_reports_publication_failure_without_success_or_upstream_replay() {
    let (_directory, gateway, mut turn, store, _, _, _) = held_publication_turn(true, false).await;
    assert_eq!(
        consume_until_publication(&mut turn, &store).await,
        "answer <!--sr:"
    );
    store.release.notify_one();
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "reversible_redaction_failed"
    );
    for _ in 0..3 {
        assert!(turn.output.next().await.is_none());
    }
    drop(turn);
    assert_publication_observation(&gateway, "reversible_redaction_failed").await;
}

#[tokio::test]
async fn canonical_completion_cancellation_interrupts_publication_without_revoking_committed_mappings()
 {
    let (_directory, gateway, mut turn, store, principal, (cancellation, _), pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    cancellation.cancel();
    // Do not release publication: cancellation must independently wake the gate.
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "cancelled"
    );
    assert!(turn.output.next().await.is_none());
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
    drop(turn);
    assert_publication_observation(&gateway, "cancelled").await;
    let discoveries = gateway
        .observation
        .credential_discoveries(Default::default())
        .await
        .unwrap();
    assert_eq!(discoveries.items.len(), 1);
    assert_eq!(discoveries.items[0].new_credential_count, 1);
    assert_eq!(discoveries.items[0].source_types, ["system_or_history"]);
    assert_ne!(discoveries.items[0].status, "completed");
}

#[tokio::test]
async fn canonical_completion_deadline_interrupts_publication() {
    let (_directory, gateway, mut turn, store, _, (_, deadline), _) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    // Deadline 使用 std::time::Instant；虚拟推进 Tokio 会空转至真实五分钟。
    // 发布已经进入阻塞点后，移动共享截止时间，验证独立唤醒和终态收口。
    deadline.reset(Instant::now());
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "deadline_exceeded"
    );
    assert!(turn.output.next().await.is_none());
    drop(turn);
    assert_publication_observation(&gateway, "deadline_exceeded").await;
}

async fn assert_publication_observation(gateway: &Gateway, status: &str) {
    use crate::interaction_observation::ForestQuery;

    // 查询路径不再隐式等待异步观测 writer，需要读己之写的断言必须显式请求屏障。
    gateway.observation.flush().await.unwrap();
    let forest = gateway
        .observation
        .query_forest(ForestQuery::default())
        .await
        .unwrap();
    let interaction = &forest.roots[0].interactions[0];
    let detail = gateway
        .observation
        .get_interaction(&interaction.id, ForestQuery::default())
        .await
        .unwrap()
        .unwrap();
    let run = &detail.runs[0];
    let terminals = run
        .events
        .iter()
        .filter(|event| event.kind == "model_turn_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "one owner records the Model Turn result"
    );
    assert_eq!(terminals[0].payload["status"], status);
    let attempts = run
        .events
        .iter()
        .filter(|event| event.kind == "target_attempt_finished")
        .collect::<Vec<_>>();
    assert_eq!(
        attempts.len(),
        1,
        "local publication never retries an upstream attempt"
    );
    assert_eq!(attempts[0].payload["status"], "completed");
    assert_eq!(run.usage.input_tokens, Some(1));
    assert_eq!(run.usage.output_tokens, Some(1));
}

#[tokio::test]
async fn dropping_pending_canonical_publication_records_cancelled_once() {
    let (_directory, gateway, mut turn, store, principal, _, pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    drop(turn);
    assert_publication_observation(&gateway, "cancelled").await;
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
}

#[tokio::test]
async fn cancellation_in_publications_final_poll_preempts_completed() {
    let (_directory, gateway, mut turn, store, principal, (cancellation, _), pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    *store.cancel_on_release.lock() = Some(cancellation);
    store.release.notify_one();
    assert_eq!(
        turn.output.next().await.unwrap().unwrap_err().code,
        "cancelled"
    );
    assert!(turn.output.next().await.is_none());
    drop(turn);
    assert_publication_observation(&gateway, "cancelled").await;
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
}

#[tokio::test]
async fn observation_writer_failure_does_not_change_canonical_publication_success() {
    let (_directory, gateway, mut turn, store, principal, _, pending_expiry) =
        held_publication_turn(false, false).await;
    consume_until_publication(&mut turn, &store).await;
    gateway.observation.shutdown().await;
    store.release.notify_one();
    assert!(matches!(
        turn.output.next().await,
        Some(Ok(CanonicalEvent::Completed(_)))
    ));
    assert!(turn.output.next().await.is_none());
    assert!(store.inner.active(&principal).await.unwrap()[0].expires_at > pending_expiry);
}
