use super::*;

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
        .create_ready_bytes(
            &owner,
            "image/png",
            bytes::Bytes::from_static(b"same image"),
            Duration::from_secs(60),
        )
        .await
        .expect("first Artifact");
    let second = artifacts
        .create_ready_bytes(
            &owner,
            "image/png",
            bytes::Bytes::from_static(b"same image"),
            Duration::from_secs(60),
        )
        .await
        .expect("second Artifact");

    let request_for = |artifact_id: &crate::agent::ArtifactId| {
        responses_request(vec![AiItem {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                source: MediaSource::FileId {
                    file_id: format!("stravia-artifact:{}", artifact_id.as_str()),
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
    let artifact_store: Arc<dyn crate::agent::ArtifactStore> = artifacts;
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

    let mut first_provider_item = first_request.items[0].clone();
    let mut second_provider_item = second_request.items[0].clone();
    first_provider_item.meta = None;
    second_provider_item.meta = None;
    assert_eq!(
        crate::protocol::ir::canonical::item_value(&first_provider_item),
        crate::protocol::ir::canonical::item_value(&second_provider_item),
        "same bytes must produce the same provider-visible media"
    );
    assert_ne!(
        crate::protocol::ir::canonical::item_value(&first_request.items[0]),
        crate::protocol::ir::canonical::item_value(&second_request.items[0]),
        "distinct Artifact identities must not share a reusable prefix"
    );
    let missing = crate::agent::ArtifactId::new("missing");
    assert_eq!(
        chain
            .begin(owner, request_for(&missing))
            .await
            .err()
            .expect("missing Artifact must reject begin"),
        BeginError::ItemReferenceNotFound
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
        AiItem::function_call(crate::protocol::ir::ToolCall {
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
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
    ];
    let assistant = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            crate::protocol::ir::ContentBlock::Text {
                text: "planning".into(),
                cache_control: None,
            },
            crate::protocol::ir::ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "lookup".into(),
                input: serde_json::json!({"value": 1}),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    };
    root.stage(&mut response, Some("upstream-response".into()));
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
    root.stage(&mut response, Some("upstream-response".into()));
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
                crate::model_turn::ContinuationTarget {
                    namespace: "",
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
                crate::model_turn::ContinuationTarget {
                    namespace: "",
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
            crate::protocol::ir::ContentBlock::Text {
                text: "first".into(),
                cache_control: None,
            },
            crate::protocol::ir::ContentBlock::Text {
                text: "transient reminder".into(),
                cache_control: Some(crate::protocol::ir::CacheControl::ephemeral()),
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
    root.stage(&mut response, Some("upstream-response".into()));
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
    let (gateway, _logs) = crate::Gateway::new(config.clone())
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
    write.stage(&mut response, None);
    write.persist().await.expect("persist response");
    drop(gateway);

    let (gateway, _logs) = crate::Gateway::new(config)
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
    tools.tools = Some(vec![crate::protocol::ir::ToolSpec {
        name: "lookup".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        cache_control: None,
        meta: None,
    }]);
    variants.push(tools);
    let mut reasoning = original.clone();
    reasoning.reasoning.effort = Some(crate::protocol::ir::ReasoningEffort::High);
    variants.push(reasoning);
    let mut response_format = original;
    response_format.response_format = Some(crate::protocol::ir::ResponseFormat::JsonObject);
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
    let image_value = crate::protocol::ir::canonical::item_value(&image);
    let changed_image_value = crate::protocol::ir::canonical::item_value(&changed_image);
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
