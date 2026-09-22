use super::*;
use stravia_runtime_contract::artifact::{ArtifactStore, bytes_stream};

type PendingPersist = tokio::task::JoinHandle<Result<(), PersistError>>;

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
    let chain = GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
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
            reference.meta = Some(serde_json::json!({
                "__open_responses_item_reference":
                    crate::protocol::codec::open_responses::formatter::gateway_item_id(
                        "msg",
                        &pending_id,
                        0,
                    )
            }));
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
async fn reasoning_tracking_metadata_does_not_fork_generation_history() {
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
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
    reasoning.meta = Some(serde_json::json!({"__open_responses_item_fields": {
        "internal_chat_message_metadata_passthrough": {"trace": "opaque"},
        "metadata": {"turn_id": "trace-turn"}
    }}));
    let mut output_b = AiResponse::new("upstream-b", "model");
    output_b.items = vec![reasoning];
    b.stage(&mut output_b, &generation_source(), None);
    b.persist().await.unwrap();

    // Simulate durable indexes written by the previous projection, without
    // changing immutable payloads or parent edges.
    let crate::turn_chain::SqlTurnChainStore::Sqlite(pool) = backend.as_ref() else {
        unreachable!()
    };
    sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = 'old-controls', prefix_fingerprint = 'old-projection', prefix_item_count = 99 WHERE id = ?")
        .bind(b.id()).execute(pool).await.unwrap();
    backend
        .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &rebuilt_prefix)
        .await
        .unwrap();
    let restarted =
        GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
    let mut replay = history_a;
    replay.push(AiItem::reasoning(
        vec!["summary".into()],
        vec!["content".into()],
        Some("ciphertext".into()),
    ));
    replay.push(user_message("continue"));
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
        serde_json::json!({"__open_responses_item_fields": {"future_model_content": "different"}}),
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
    backend
        .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &rebuilt_prefix)
        .await
        .unwrap();
    sqlx::query("UPDATE turn_chain_nodes SET expires_at = 0 WHERE id = ?")
        .bind(a.id())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE turn_chain_nodes SET prefix_namespace = 'old-controls' WHERE id = ?")
        .bind(b.id())
        .execute(pool)
        .await
        .unwrap();
    backend
        .rebuild_prefixes(GENERATION_PREFIX_NAMESPACE, &rebuilt_prefix)
        .await
        .unwrap();
    let unavailable: (Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT prefix_namespace, parent_id, payload FROM turn_chain_nodes WHERE id = ?",
    )
    .bind(b.id())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(unavailable.0, None);
    assert_eq!(unavailable.1.as_deref(), Some(a.id()));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&unavailable.2).unwrap(),
        nodes[1].payload
    );
}

#[tokio::test]
async fn observation_tool_result_evidence_requires_a_pending_parent_call() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
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
    let store = GenerationChainStore::from_turn_chain(backend.clone(), Duration::from_secs(60));
    let owner = principal("owner");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_immediately_expired".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("question")]),
            effective_request: None,
            response: AiResponse::new("upstream", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    for _ in 0..2 {
        store
            .materialize_generation(&owner, &TurnNodeId::new("resp_immediately_expired"))
            .await
            .expect("materialize response");
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
async fn materialization_cache_does_not_outlive_the_generation_ttl() {
    let store = GenerationChainStore::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(1),
    );
    let owner = principal("owner");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_expiring".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut cached = responses_request(vec![user_message("follow up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = cached.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_expiring".into());
    store
        .materialize_parent(&owner, &mut cached)
        .await
        .expect("materialize cached response");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut expired = responses_request(vec![user_message("follow up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = expired.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_expiring".into());
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
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
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
                    file_id: format!("sa:{}", artifact_id.as_str()),
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
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
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
                file_id: format!("sa:{}", artifact_id.as_str()),
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
    response.items = vec![AiItem::unknown(serde_json::json!({
        "type": "stravia:media_result",
        "turn_id": "aturn_media",
        "completion": "complete"
    }))];
    response.trusted_media_turn_ids = vec!["aturn_media".into()];
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_root".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    for follow_up in ["branch-a", "branch-b"] {
        let mut request = responses_request(vec![user_message(follow_up)]);
        let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
            unreachable!();
        };
        extension.previous_response_id = Some("resp_root".into());
        let active = store
            .materialize_parent(&owner, &mut request)
            .await
            .expect("materialize branch");
        assert_eq!(active.parent_id.as_deref(), Some("resp_root"));
        assert_eq!(
            active.media_turn_messages,
            vec![(1, vec!["aturn_media".into()])]
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
        .save_with_effective(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_rewritten".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: root,
            effective_request: Some(effective_root),
            response,
            upstream_response_id: Some("upstream".into()),
            effective_state: state,
        })
        .await
        .expect("save rewritten response");

    let mut next = responses_request(vec![user_message("follow up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = next.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_rewritten".into());
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
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut request = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_root".into());

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
                text: "planning".into(),
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
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut initial = responses_request(vec![question.clone()]);
    let Some(ProtocolExt::OpenResponses(extension)) = initial.ext.as_mut() else {
        unreachable!();
    };
    extension.store = Some(false);
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
    let Some(ProtocolExt::OpenResponses(extension)) = resumed_request.ext.as_mut() else {
        unreachable!();
    };
    extension.store = Some(false);
    let resumed = chain
        .begin(owner.clone(), resumed_request)
        .await
        .expect("discover matching prefix");
    let lookup = chain.continuation_lookup();

    let mut without_affinity = resumed.request().clone();
    assert_eq!(
        lookup
            .prepare(
                &owner,
                crate::router::ContinuationTarget {
                    namespace: "provider:model",
                    protocol: OPEN_RESPONSES_2026_04_24,
                    actual_model: "model",
                    logical_model: "model",
                    allow_ephemeral_response: false,
                },
                &mut without_affinity,
            )
            .await,
        None
    );

    let mut with_affinity = resumed.request().clone();
    assert_eq!(
        lookup
            .prepare(
                &owner,
                crate::router::ContinuationTarget {
                    namespace: "provider:model",
                    protocol: OPEN_RESPONSES_2026_04_24,
                    actual_model: "model",
                    logical_model: "model",
                    allow_ephemeral_response: true,
                },
                &mut with_affinity,
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
    );
    let owner = principal("owner");
    let original_user = AiItem {
        role: Role::User,
        content: MessageContent::Blocks(vec![
            stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                text: "first".into(),
                cache_control: None,
            },
            stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                text: "transient reminder".into(),
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
    initial.meta.vendor.ingress.insert(
        GENERATION_SESSION_ID_META.into(),
        serde_json::Value::String("session-1".into()),
    );
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
    resumed_request.meta.vendor.ingress.insert(
        GENERATION_SESSION_ID_META.into(),
        serde_json::Value::String("session-1".into()),
    );
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
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut request = responses_request(vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(serde_json::json!({
            "__open_responses_item_reference": "msg_saved"
        })),
    }]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_items".into());

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
    let Some(ProtocolExt::OpenResponses(extension)) = unauthorized.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_items".into());
    unauthorized.items = vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(serde_json::json!({
            "__open_responses_item_reference": "msg_saved"
        })),
    }];
    let error = store
        .materialize_parent(&principal("other"), &mut unauthorized)
        .await
        .expect_err("cross-principal reference must not resolve");
    assert_eq!(error, "item_reference_not_found");
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
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![saved_input]),
            effective_request: None,
            response: AiResponse::new("answer", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let mut request = responses_request(vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(serde_json::json!({
            "__open_responses_item_reference": "msg_client"
        })),
    }]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_input".into());

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
            parent: ActiveGenerationChain::default(),
            request_delta: root,
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
        let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
            unreachable!();
        };
        extension.previous_response_id = Some("resp_instructions".into());
        store
            .materialize_parent(&owner, &mut request)
            .await
            .expect("materialize continuation");
        assert_eq!(request.instructions.as_deref(), Some(expected));
    }
    let mut explicit_null = responses_request(vec![user_message("continue")]);
    let Some(ProtocolExt::OpenResponses(extension)) = explicit_null.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_instructions".into());
    extension.instructions_present = true;
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
    let Some(ProtocolExt::OpenResponses(root_extension)) = root.ext.as_mut() else {
        unreachable!();
    };
    root_extension.include = Some(vec!["reasoning.encrypted_content".into()]);
    root_extension.store = Some(false);
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_config".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: root,
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
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_config".into());
    store
        .materialize_parent(&owner, &mut continuation)
        .await
        .expect("materialize continuation");

    assert_eq!(continuation.model, "root-model");
    assert_eq!(continuation.generation.temperature, Some(0.25));
    assert_eq!(continuation.generation.max_tokens, Some(50));
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext else {
        unreachable!();
    };
    assert_eq!(
        extension.include,
        Some(vec!["reasoning.encrypted_content".into()])
    );
    assert_eq!(extension.store, None);
}

#[tokio::test]
async fn response_ids_are_isolated_by_principal() {
    let store = generation_store().await;
    store
        .save(GenerationChainCommit {
            principal: principal("owner").clone(),
            id: "resp_private".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("secret")]),
            effective_request: None,
            response: AiResponse::new("upstream", "model"),
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");
    let mut request = responses_request(vec![user_message("steal")]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_private".into());

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
    let store =
        GenerationChainStore::from_turn_chain(Arc::clone(&turn_chain), Duration::from_secs(60));
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_persisted".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![user_message("question")]),
            effective_request: None,
            response,
            upstream_response_id: None,
            effective_state: GenerationChainState::default(),
        })
        .await
        .expect("save response");

    let reconstructed = GenerationChainStore::from_turn_chain(turn_chain, Duration::from_secs(60));
    let mut continuation = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_persisted".into());
    reconstructed
        .materialize_parent(&owner, &mut continuation)
        .await
        .expect("materialize response history");

    assert_eq!(continuation.items.len(), 3);
}

#[tokio::test]
async fn legacy_tool_meta_cannot_authorize_restored_encoded_payloads() {
    let data_dir = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let owner = principal("legacy-tool-owner");
    let key = stravia_runtime_contract::protocol::ir::TOOL_RESULT_CONTENT_KIND_META;
    let business = serde_json::json!([
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "opaque"},
         (key): "business-data"}
    ])
    .to_string();
    // These are actual pre-reservation AiItems, including the old decoder's
    // verbatim vendor extra field. Do not manufacture them with a fresh factory.
    let legacy_item = serde_json::json!({
        "role": "tool", "content": business, "tool_calls": null,
        "tool_call_id": "legacy-call",
        "meta": {(key): "content_blocks", "vendor-extra": { (key): "nested-business" }}
    });
    for version in 1..=4 {
        let id = format!("resp_legacy_tool_{version}");
        let mutation = match version {
            3 => serde_json::json!({"type": "append", "items": [legacy_item]}),
            4 => serde_json::json!({"type": "replace", "items": [legacy_item]}),
            _ => serde_json::Value::Null,
        };
        let mut response = AiResponse::new("upstream", "model");
        response.items = vec![serde_json::from_value(legacy_item.clone()).unwrap()];
        gateway
            .turn_chains
            .commit(TurnCommit {
                id: TurnNodeId::new(&id),
                kind: TurnNodeKind::Response,
                parent_id: None,
                principal: owner.clone(),
                payload_version: version,
                payload: serde_json::json!({
                    "client_delta": {"messages": [legacy_item], "system": null},
                    "client_output": [legacy_item], "effective_input": [legacy_item],
                    "effective_history_mutation": mutation, "effective_system": null,
                    "effective_output": response, "upstream_response_id": null,
                    "effective_state": GenerationChainState::default()
                }),
                idle_ttl: Duration::from_secs(60),
                reusable_prefix: None,
            })
            .await
            .unwrap();
        let store = GenerationChainStore::from_turn_chain(
            Arc::clone(&gateway.turn_chains),
            Duration::from_secs(60),
        );
        let mut request = responses_request(Vec::new());
        let Some(ProtocolExt::OpenResponses(ext)) = request.ext.as_mut() else {
            unreachable!()
        };
        ext.previous_response_id = Some(id);
        store
            .materialize_parent(&owner, &mut request)
            .await
            .unwrap();
        assert_eq!(request.items.len(), 2);
        for item in &request.items {
            assert_eq!(
                item.meta.as_ref().unwrap()["vendor-extra"][key],
                "nested-business"
            );
            let MessageContent::Text(text) = &item.content else {
                panic!("legacy tool text")
            };
            assert_eq!(text, &business);
        }
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
            .set("reversible_redaction_enabled", "false")
            .await
            .unwrap();
        let before = serde_json::to_value(&request.items).unwrap();
        gateway
            .redaction
            .protect(&owner, &mut request, None)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(&request.items).unwrap(), before);
        gateway
            .storage
            .settings()
            .set("reversible_redaction_enabled", "true")
            .await
            .unwrap();
        assert!(matches!(
            gateway.redaction.protect(&owner, &mut request, None).await,
            Err(stravia_runtime_contract::redaction::RedactionError::AmbiguousToolResult)
        ));
    }
}

#[tokio::test]
async fn persisted_tool_text_semantics_keep_plain_secrets_and_media_distinct() {
    let data_dir = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
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
    media.meta = Some(serde_json::json!({
        (stravia_runtime_contract::protocol::ir::TOOL_RESULT_CONTENT_KIND_META): "content_blocks"
    }));
    let store = GenerationChainStore::from_turn_chain(
        Arc::clone(&gateway.turn_chains),
        Duration::from_secs(60),
    );
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_tool_semantics".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: responses_request(vec![plain, media]),
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
    );
    let mut request = responses_request(Vec::new());
    let Some(ProtocolExt::OpenResponses(ext)) = request.ext.as_mut() else {
        unreachable!()
    };
    ext.previous_response_id = Some("resp_tool_semantics".into());
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
async fn legacy_response_payload_keeps_target_continuation() {
    let turn_chain: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let owner = principal("owner");
    let store =
        GenerationChainStore::from_turn_chain(Arc::clone(&turn_chain), Duration::from_secs(60));
    let root = responses_request(vec![user_message("question")]);
    let mut response = AiResponse::new("upstream-1", "model");
    response.push_output_text("answer");
    let mut state =
        GenerationChainState::from_request(&root, "provider-a", OPEN_RESPONSES_2026_04_24);
    let mut legacy_items = root.items.clone();
    legacy_items.extend(response.items.clone());
    state.context_fingerprint = legacy_context_fingerprint(&legacy_items);
    state.context_messages = legacy_items.len();
    turn_chain
        .commit(TurnCommit {
            id: TurnNodeId::new("resp_legacy"),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: owner.clone(),
            payload_version: LEGACY_RESPONSE_PAYLOAD_VERSION,
            payload: serde_json::to_value(PersistedResponseNode {
                client_delta: RequestDelta {
                    messages: root.items.clone(),
                    system: root.instructions.clone(),
                },
                client_output: None,
                client_history_mutation: None,
                compaction_record_ids: Vec::new(),
                effective_history_mutation: None,
                effective_system: root.instructions.clone(),
                effective_output: response,
                effective_input: root.items.clone(),
                client_history: None,
                trusted_media_turn_ids: Vec::new(),
                upstream_response_id: Some("upstream-1".into()),
                effective_state: state,
                effective_request: Some(EffectiveRequestConfig::from_request(&root)),
            })
            .expect("legacy payload"),
            idle_ttl: Duration::from_secs(60),
            reusable_prefix: None,
        })
        .await
        .expect("save legacy response");

    let mut next = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = next.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_legacy".into());
    let active = store
        .materialize_parent(&owner, &mut next)
        .await
        .expect("materialize legacy response");
    let candidate =
        GenerationChainState::from_request(&next, "provider-a", OPEN_RESPONSES_2026_04_24);

    assert!(store.prepare_upstream(&active, &mut next, &candidate, false));
    assert!(items_equal(&next.items, &[user_message("follow-up")]));
    let Some(ProtocolExt::OpenResponses(extension)) = next.ext else {
        unreachable!();
    };
    assert_eq!(
        extension.previous_response_id.as_deref(),
        Some("upstream-1")
    );
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
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some(response_id);
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
            parent: ActiveGenerationChain::default(),
            request_delta: request,
            effective_request: None,
            response,
            upstream_response_id: Some("upstream-1".into()),
            effective_state: state,
        })
        .await
        .expect("save response");

    let mut next = responses_request(vec![user_message("m2")]);
    let Some(ProtocolExt::OpenResponses(extension)) = next.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some("resp_gateway_1".into());
    let active = store
        .materialize_parent(&owner, &mut next)
        .await
        .expect("materialize response");
    let state = GenerationChainState::from_request(&next, "provider-a", OPEN_RESPONSES_2026_04_24);

    assert!(store.prepare_upstream(&active, &mut next, &state, false));
    assert_eq!(next.items.len(), 1);
    let Some(ProtocolExt::OpenResponses(extension)) = next.ext else {
        unreachable!();
    };
    assert_eq!(
        extension.previous_response_id.as_deref(),
        Some("upstream-1")
    );
}

#[tokio::test]
async fn automatic_prefix_selects_exact_completed_context_and_leaves_new_items() {
    let store = generation_store().await;
    let owner = principal("owner");
    let mut root = responses_request(vec![user_message("m1")]);
    let Some(ProtocolExt::OpenResponses(extension)) = root.ext.as_mut() else {
        unreachable!();
    };
    extension.store = Some(false);
    let state = GenerationChainState::from_request(&root, "target-a", OPEN_RESPONSES_2026_04_24);
    let mut response = AiResponse::new("upstream-1", "model");
    response.push_output_text("r1");
    let completed_items = response.items.clone();
    store
        .save(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_gateway_1".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: root.clone(),
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
    let discovered = store
        .discover_parent(&owner, &mut next)
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
    let Some(ProtocolExt::OpenResponses(extension)) = next.ext else {
        unreachable!();
    };
    assert_eq!(extension.store, None);
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
    let client_output = project_client_output(
        Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1),
        &response,
        &mut client_items,
    )
    .expect("project chat response");

    store
        .save_with_effective(GenerationChainCommit {
            principal: owner.clone(),
            id: "resp_parallel_tools".into(),
            parent: ActiveGenerationChain::default(),
            request_delta: root.clone(),
            effective_request: Some(effective_root),
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
    store
        .discover_parent(&owner, &mut next)
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
    crate::protocol::codec::tool_correlation::normalize_request_tool_results(&mut next);

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
            parent: ActiveGenerationChain::default(),
            request_delta: root.clone(),
            effective_request: None,
            response,
            upstream_response_id: Some("upstream-1".into()),
            effective_state: state,
        })
        .await
        .expect("save reusable response");

    let mut identical = root;
    identical.items.extend(completed_items);
    assert!(
        store
            .discover_parent(&owner, &mut identical)
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
        cache_control: None,
        meta: None,
    }]);
    variants.push(tools);
    let mut reasoning = original.clone();
    reasoning.reasoning.effort =
        Some(stravia_runtime_contract::protocol::ir::ReasoningEffort::High);
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
    let prior_settings = state.request_settings_fingerprint.clone();
    request.generation.temperature = Some(0.7);
    request.parallel_tool_calls = Some(true);

    state.refresh_request_semantics(&request);
    let expected =
        GenerationChainState::from_request(&request, "provider-a", OPEN_RESPONSES_2026_04_24);

    assert_ne!(state.request_settings_fingerprint, prior_settings);
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

#[tokio::test]
async fn native_upstream_reuse_requires_persisted_open_responses_target() {
    let store = generation_store().await;
    for (egress, store_value) in [
        (OPEN_RESPONSES_2026_04_24, Some(false)),
        (ANTHROPIC_MESSAGES_2023_06_01, Some(true)),
    ] {
        let mut request = responses_request(vec![user_message("new")]);
        let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
            panic!("Open Responses extension");
        };
        extension.store = store_value;
        let state = GenerationChainState::from_request(&request, "provider", egress);
        let active = ActiveGenerationChain {
            parent_upstream_response_id: Some("upstream".into()),
            parent_state: Some(state.clone()),
            ..ActiveGenerationChain::default()
        };

        assert!(!store.prepare_upstream(&active, &mut request, &state, false));
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
    client_history_mutation: Option<EffectiveHistoryMutation>,
    client_output: Option<Vec<AiItem>>,
    effective_output_text: &str,
) -> serde_json::Value {
    let mut effective_output = AiResponse::new("private-upstream", "model");
    effective_output.push_output_text(effective_output_text);
    serde_json::to_value(PersistedResponseNode {
        client_delta: RequestDelta {
            messages: client_delta,
            system: None,
        },
        client_output,
        client_history_mutation,
        compaction_record_ids: Vec::new(),
        effective_history_mutation: None,
        effective_system: None,
        effective_output,
        effective_input: Vec::new(),
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
    let chain =
        GenerationChain::from_turn_chain(Arc::clone(&backend), Duration::from_secs(60), None);
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
                items: vec![user_message("follow up")],
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
                items: vec![user_message("edited question")],
            }),
            Some(vec![AiItem::output_text("public replacement")]),
            "private replacement",
        ),
        Duration::from_secs(60),
    )
    .await;

    let mut snapshots = Vec::new();
    let available = chain
        .visit_ancestor_client_items(&owner, "observation-replace", |node, items| {
            snapshots.push((
                node.to_owned(),
                items
                    .iter()
                    .map(|item| {
                        serde_json::json!({"role": item.role, "content": item.content.to_text()})
                    })
                    .collect::<Vec<_>>(),
            ))
        })
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
async fn ancestor_client_item_visitor_declines_unavailable_chains_without_partial_evidence() {
    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain =
        GenerationChain::from_turn_chain(Arc::clone(&backend), Duration::from_secs(60), None);
    let owner = principal("unavailable-observation-owner");

    let mut visited = Vec::new();
    assert!(
        !chain
            .visit_ancestor_client_items(&owner, "missing-node", |node, _| {
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
            .visit_ancestor_client_items(&owner, "expired-observation", |node, _| {
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
    let chain =
        GenerationChain::from_turn_chain(Arc::clone(&backend), Duration::from_secs(60), None);
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
            .visit_ancestor_client_items(&owner, "invalid-observation-tail", |node, _| {
                visited.push(node.to_owned())
            })
            .await
            .is_err()
    );
    assert!(visited.is_empty());
}
