use super::*;

#[tokio::test]
async fn completed_inline_window_is_the_parent_after_cold_restore() {
    let durable = Arc::new(crate::turn_chain::test_store().await);
    let chain =
        GenerationChain::from_turn_chain(durable.clone(), DEFAULT_GENERATION_CHAIN_TTL, None);
    let owner = principal("inline-owner");
    let state =
        crate::protocol::codec::open_responses::decoder::decode_input_item(&serde_json::json!({
            "type": "compaction", "id": "native-inline-state", "encrypted_content": "opaque-inline"
        }))
        .unwrap()
        .unwrap();
    let mut source = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("removed-history")]),
        )
        .await
        .unwrap();
    let mut effective = source.request().clone();
    effective
        .items
        .push(user_message("effective-only-source-history"));
    source.observe_effective(effective);
    let mut source_response = AiResponse::new("source-response", "model");
    source_response.items = vec![AiItem::output_text("source answer")];
    source_response.items[0].set_graph_metadata(
        Some(
            crate::protocol::codec::open_responses::formatter::gateway_item_id(
                "msg",
                source.id(),
                0,
            ),
        ),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    assert!(source.stage(&mut source_response, &generation_source(), None));
    source.persist().await.unwrap();
    let mut full_history = vec![user_message("removed-history")];
    full_history.extend(source_response.items);
    let ordinary = chain
        .begin(owner.clone(), responses_request(full_history.clone()))
        .await
        .unwrap();
    assert_eq!(ordinary.parent_id(), None);
    let mut first_request = responses_request(full_history.clone());
    // Exercise source-only adoption even when a trigger or new input follows the prefix.
    first_request.instructions = Some("Prepare the native compacted window".into());
    if let Some(ProtocolExt::OpenResponses(extension)) = first_request.ext.as_mut() {
        extension.passthrough_body.insert(
            "context_management".into(),
            serde_json::json!([{"type": "compaction", "compact_threshold": 2000}]),
        );
    }
    let mut below_threshold = chain
        .begin_native_compaction(owner.clone(), first_request.clone())
        .await
        .unwrap();
    assert_eq!(below_threshold.parent_id(), Some(source.id()));
    assert!(below_threshold.request_delta().items.is_empty());
    let mut ordinary_response = AiResponse::new("below-threshold", "model");
    ordinary_response.items = vec![AiItem::output_text("ordinary answer")];
    assert!(below_threshold.stage(&mut ordinary_response, &generation_source(), None));
    below_threshold.persist().await.unwrap();
    let cold_chain =
        GenerationChain::from_turn_chain(durable.clone(), DEFAULT_GENERATION_CHAIN_TTL, None);
    let mut continuation = responses_request(vec![user_message("after ordinary answer")]);
    crate::model_turn::stamp_previous_response_id(&mut continuation, below_threshold.id());
    let continued = cold_chain.begin(owner.clone(), continuation).await.unwrap();
    let mut expected = full_history;
    expected.extend(ordinary_response.items);
    expected.push(user_message("after ordinary answer"));
    assert_eq!(
        history_context_fingerprint(&continued.request().items),
        history_context_fingerprint(&expected)
    );
    let trigger = crate::protocol::codec::open_responses::decoder::decode_input_item(
        &serde_json::json!({"type": "compaction_trigger"}),
    )
    .unwrap()
    .unwrap();
    first_request.items.push(trigger.clone());
    let mut write = chain
        .begin_native_compaction(owner.clone(), first_request.clone())
        .await
        .unwrap();
    assert_eq!(write.parent_id(), Some(source.id()));
    assert_eq!(write.root_id(), source.root_id());
    assert_eq!(
        history_context_fingerprint(&write.request_delta().items),
        history_context_fingerprint(std::slice::from_ref(&trigger))
    );
    assert_eq!(
        history_context_fingerprint(&write.request().items),
        history_context_fingerprint(&first_request.items)
    );
    let mut new_input = first_request;
    new_input.items.push(user_message("new after source"));
    let with_new_input = chain
        .begin_native_compaction(owner.clone(), new_input)
        .await
        .unwrap();
    assert_eq!(with_new_input.parent_id(), Some(source.id()));
    assert_eq!(
        history_context_fingerprint(&with_new_input.request_delta().items),
        history_context_fingerprint(&[trigger, user_message("new after source")])
    );
    write.parent.fresh_inline_states.push(state.clone());
    let mut response = AiResponse::new("inline-response", "model");
    response.items = vec![state.clone(), AiItem::output_text("inline answer")];
    response.items[1].set_graph_metadata(
        Some(
            crate::protocol::codec::open_responses::formatter::gateway_item_id(
                "msg",
                write.id(),
                1,
            ),
        ),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    assert!(write.stage(&mut response, &generation_source(), None));
    write.persist().await.unwrap();
    let inline_id = write.id().to_owned();
    for cold in [false, true] {
        let chain = if cold {
            GenerationChain::from_turn_chain(durable.clone(), DEFAULT_GENERATION_CHAIN_TTL, None)
        } else {
            chain.clone()
        };
        let mut items = response.items.clone();
        items.push(user_message("next user"));
        let mut next_request = responses_request(items);
        next_request.instructions = write.request().instructions.clone();
        let mut next = chain.begin(owner.clone(), next_request).await.unwrap();
        assert_eq!(next.parent_id(), Some(inline_id.as_str()));
        assert_eq!(
            history_context_fingerprint(&next.request_delta().items),
            history_context_fingerprint(&[user_message("next user")])
        );
        assert!(
            !next
                .request()
                .items
                .iter()
                .any(
                    |item| history_context_fingerprint(std::slice::from_ref(item))
                        == history_context_fingerprint(&[user_message("removed-history")])
                )
        );
        assert_eq!(
            next.request().items.first().unwrap().id_ref(),
            Some("native-inline-state")
        );
        let mut answer = AiResponse::new("next-response", "model");
        answer.items = vec![AiItem::output_text("next answer")];
        answer.items[0].set_graph_metadata(
            Some(
                crate::protocol::codec::open_responses::formatter::gateway_item_id(
                    "msg",
                    next.id(),
                    0,
                ),
            ),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        );
        assert!(next.stage(&mut answer, &generation_source(), None));
        next.persist().await.unwrap();
        let mut continuation = response.items.clone();
        continuation.push(user_message("next user"));
        continuation.extend(answer.items);
        continuation.push(user_message("third user"));
        let mut third_request = responses_request(continuation);
        third_request.instructions = write.request().instructions.clone();
        let third = chain.begin(owner.clone(), third_request).await.unwrap();
        assert_eq!(third.parent_id(), Some(next.id()));
        assert_eq!(
            history_context_fingerprint(&third.request_delta().items),
            history_context_fingerprint(&[user_message("third user")])
        );
    }
}

#[tokio::test]
async fn native_compaction_source_keeps_reference_and_artifact_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let artifacts = Arc::new(crate::agent::LocalArtifactStore::sqlite(
        pool.clone(),
        directory.path().join("artifacts"),
    ));
    let owner = principal("native-artifact-owner");
    let artifact = artifacts
        .create_ready_bytes(
            &owner,
            "image/png",
            bytes::Bytes::from_static(b"image"),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::SqlTurnChainStore::sqlite(pool)),
        DEFAULT_GENERATION_CHAIN_TTL,
        Some(artifacts),
    );
    let original = user_message("source question");
    let mut source = chain
        .begin(owner.clone(), responses_request(vec![original.clone()]))
        .await
        .unwrap();
    let item_id =
        crate::protocol::codec::open_responses::formatter::gateway_item_id("msg", source.id(), 0);
    let mut answer = AiResponse::new("source-answer", "model");
    answer.items = vec![AiItem::output_text("saved answer")];
    answer.items[0].set_graph_metadata(
        Some(item_id.clone()),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    assert!(source.stage(&mut answer, &generation_source(), None));
    source.persist().await.unwrap();
    let decode = crate::protocol::codec::open_responses::decoder::decode_input_item;
    let reference = decode(&serde_json::json!({"type":"item_reference","id":item_id}))
        .unwrap()
        .unwrap();
    let mut image = user_message("");
    image.content = MessageContent::Blocks(vec![ContentBlock::Image {
        source: MediaSource::FileId {
            file_id: format!("stravia-artifact:{}", artifact.id.as_str()),
            detail: None,
        },
        detail: None,
        cache_control: None,
    }]);
    let mut items = vec![original];
    items.extend(answer.items);
    items.extend([reference, image]);
    let mut request = responses_request(items);
    request.instructions = Some("new compact instructions".into());
    let write = chain.begin_native_compaction(owner, request).await.unwrap();
    assert_eq!(write.parent_id(), Some(source.id()));
    assert_eq!(write.request().items[2].content.to_text(), "saved answer");
    assert!(matches!(
        &write.request().items[3].content,
        MessageContent::Blocks(blocks) if matches!(
            &blocks[0],
            ContentBlock::Image { source: MediaSource::Base64 { media_type, data }, .. }
                if media_type == "image/png" && data == "aW1hZ2U="
        )
    ));
}

#[tokio::test]
async fn recompaction_excludes_source_prefix_and_native_window_from_new_user_delta() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let compaction = crate::compaction::Compaction::sqlite(pool.clone());
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::SqlTurnChainStore::sqlite(pool)),
        DEFAULT_GENERATION_CHAIN_TTL,
        None,
    )
    .with_compaction(compaction.clone());
    let owner = principal("recompaction-owner");
    let original = user_message("already delivered user");
    let mut source = chain
        .begin(owner.clone(), responses_request(vec![original.clone()]))
        .await
        .unwrap();
    let mut answer = AiResponse::new("source-answer", "model");
    answer.items = vec![AiItem::output_text("already delivered answer")];
    answer.items[0].set_graph_metadata(
        Some(
            crate::protocol::codec::open_responses::formatter::gateway_item_id(
                "msg",
                source.id(),
                0,
            ),
        ),
        Some(AiItemStatus::Completed),
        AiItemProvenance::Provider,
        AiItemAudience::Client,
    );
    assert!(source.stage(&mut answer, &generation_source(), None));
    source.persist().await.unwrap();
    let state = crate::protocol::codec::open_responses::decoder::decode_input_item(
        &serde_json::json!({"type":"compaction","id":"same-source-state","encrypted_content":"opaque"}),
    ).unwrap().unwrap();
    compaction
        .register(
            &owner,
            crate::compaction::CompactionRegistration {
                source_generation_id: Some(source.id().to_owned()),
                source_record_ids: Vec::new(),
                operation_id: "first-boundary".into(),
                target: crate::compaction::CompactionTarget {
                    target_key: "local-target".into(),
                    namespace: "local-account".into(),
                    model: "model".into(),
                    protocol: OPEN_RESPONSES_2026_04_24.to_string(),
                },
                window: vec![state.clone()],
                state_items: vec![state.clone()],
            },
        )
        .await
        .unwrap();
    let trigger = crate::protocol::codec::open_responses::decoder::decode_input_item(
        &serde_json::json!({"type":"compaction_trigger"}),
    )
    .unwrap()
    .unwrap();
    let mut items = vec![original];
    items.extend(answer.items);
    items.extend([state, trigger.clone()]);
    let mut request = responses_request(items.clone());
    request.instructions = Some("New compact instructions".into());
    let write = chain
        .begin_native_compaction(owner.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(write.parent_id(), Some(source.id()));
    assert_eq!(
        history_context_fingerprint(&write.request().items),
        history_context_fingerprint(&items)
    );
    assert_eq!(
        history_context_fingerprint(&write.request_delta().items),
        history_context_fingerprint(std::slice::from_ref(&trigger))
    );
    let prepared = chain.prepare_compaction(owner, request).await.unwrap();
    assert_eq!(prepared.parent_id.as_deref(), Some(source.id()));
    assert!(!prepared.has_new_user);
}

#[tokio::test]
async fn native_window_excludes_only_verified_items_from_new_input() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let compaction = crate::compaction::Compaction::sqlite(pool);
    let owner = principal("leading-user");
    let state =
        crate::protocol::codec::open_responses::decoder::decode_input_item(&serde_json::json!({
            "type": "compaction", "encrypted_content": "leading-boundary"
        }))
        .unwrap()
        .unwrap();
    let window = vec![user_message("retained old user"), state];
    compaction
        .register(
            &owner,
            crate::compaction::CompactionRegistration {
                source_generation_id: None,
                source_record_ids: Vec::new(),
                operation_id: "leading-boundary".into(),
                target: crate::compaction::CompactionTarget {
                    target_key: "local-target".into(),
                    namespace: "local-account".into(),
                    model: "model".into(),
                    protocol: OPEN_RESPONSES_2026_04_24.to_string(),
                },
                state_items: vec![window[1].clone()],
                window: window.clone(),
            },
        )
        .await
        .unwrap();
    let chain = generation_chain().await.with_compaction(compaction);
    let mut items = vec![user_message("unverified leading user")];
    items.extend(window);
    items.push(user_message("new trailing user"));
    let write = chain
        .begin(owner, responses_request(items.clone()))
        .await
        .unwrap();
    assert_eq!(
        history_context_fingerprint(&write.request().items),
        history_context_fingerprint(&items)
    );
    assert_eq!(
        history_context_fingerprint(&write.request_delta().items),
        history_context_fingerprint(&[
            user_message("unverified leading user"),
            user_message("new trailing user")
        ])
    );
}

#[tokio::test]
async fn observe_effective_preserves_marker_without_repeating_public_tool_call() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let marker = "<!-- stravia-history-marker:hm_0123456789abcdefabcd -->";
    let public_call = ToolCall {
        id: "call_public".into(),
        name: "glob".into(),
        arguments: "{}".into(),
    };
    let assistant = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Thinking {
                thinking: "summary".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: format!("{marker}public text"),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![public_call.clone()]),
        tool_call_id: None,
        meta: Some(serde_json::json!({"reasoning_content": "summary"})),
    };
    let result =
        AiItem::function_call_output("call_public", serde_json::Value::String("result".into()));
    let mut write = chain
        .begin(
            owner,
            responses_request(vec![user_message("question"), assistant, result.clone()]),
        )
        .await
        .expect("begin generation");

    let mut restored =
        AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into()));
    restored.meta = Some(serde_json::json!({
        "__stravia_history_marker_restored": true
    }));
    let stripped = AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            ContentBlock::Thinking {
                thinking: "summary".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: "public text".into(),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![public_call]),
        tool_call_id: None,
        meta: Some(serde_json::json!({"reasoning_content": "summary"})),
    };
    let mut effective = write.request().clone();
    effective.items = vec![user_message("question"), restored, stripped, result];

    write.observe_effective(effective);

    assert_eq!(
        write
            .request
            .items
            .iter()
            .flat_map(|item| item.tool_calls.iter().flatten())
            .map(|call| (call.id.clone(), call.name.clone()))
            .collect::<Vec<_>>(),
        vec![("call_public".into(), "glob".into())]
    );
    assert_eq!(
        crate::history_marker::history_marker_references(&write.request.items),
        vec!["hm_0123456789abcdefabcd"]
    );
    let marker_item = write
        .request
        .items
        .iter()
        .find(|item| {
            !crate::history_marker::history_marker_references(std::slice::from_ref(item)).is_empty()
        })
        .expect("preserved History Marker item");
    assert!(
        marker_item.thinking_ref().is_some(),
        "Generation Chain must preserve the client-visible Thinking carrier"
    );
    assert!(marker_item.output_text_ref().is_none());
}

#[tokio::test]
async fn observe_effective_persists_marker_at_ordered_projection_atom() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let marker_store: Arc<dyn crate::history_marker::HistoryMarkerStore> =
        Arc::new(crate::history_marker::SqlHistoryMarkerStore::sqlite(pool));
    let owner = principal("owner");
    let marker = marker_store
        .create_thinking(
            &owner,
            crate::history_marker::ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "R1".into(),
                    signature: Some("opaque".into()),
                },
                activity: "Preserving reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .expect("thinking marker");
    marker_store
        .publish(
            &owner,
            std::slice::from_ref(&marker.reference),
            Duration::from_secs(60),
        )
        .await
        .expect("publish thinking marker");

    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain =
        GenerationChain::from_turn_chain(Arc::clone(&backend), Duration::from_secs(60), None)
            .with_history_markers(Arc::clone(&marker_store));
    let unknown_reference = "hm_abcdefghijklmnopqrst";
    let projected = format!(
        "R1{}{}R2",
        crate::history_marker::render_text_projection_span(&marker.reference, 0, "C1"),
        crate::history_marker::render_history_marker(&marker),
    );
    let mut write = chain
        .begin(
            owner.clone(),
            responses_request(vec![
                AiItem::thinking(
                    crate::history_marker::render_history_marker_reference(unknown_reference),
                    None,
                ),
                AiItem::thinking(projected, None),
            ]),
        )
        .await
        .expect("begin generation");
    let mut effective = write.request().clone();
    crate::history_marker::resolve_request_markers(marker_store.as_ref(), &owner, &mut effective)
        .await
        .expect("resolve ordered marker");
    assert_eq!(effective.items.len(), 4);
    assert_eq!(effective.items[0].thinking_ref(), Some(("R1", None)));
    assert_eq!(effective.items[1].output_text_ref(), Some("C1"));
    assert_eq!(
        effective.items[2].thinking_ref(),
        Some(("R1", Some("opaque")))
    );
    assert_eq!(effective.items[3].thinking_ref(), Some(("R2", None)));

    write.observe_effective(effective);
    let marker_text = crate::history_marker::render_history_marker_reference(&marker.reference);
    let unknown_marker_text =
        crate::history_marker::render_history_marker_reference(unknown_reference);
    assert_eq!(write.request.items.len(), 5);
    assert_eq!(
        write.request.items[0].thinking_ref(),
        Some((unknown_marker_text.as_str(), None))
    );
    assert_eq!(write.request.items[1].thinking_ref(), Some(("R1", None)));
    assert_eq!(write.request.items[2].output_text_ref(), Some("C1"));
    assert_eq!(
        write.request.items[3].thinking_ref(),
        Some((marker_text.as_str(), None))
    );
    assert_eq!(write.request.items[4].thinking_ref(), Some(("R2", None)));

    let id = write.id().to_owned();
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    assert!(write.stage(&mut response, &generation_source(), None));
    write.persist().await.expect("persist generation");

    let materialized = chain
        .store
        .materialize_generation(&owner, &TurnNodeId::new(id.clone()))
        .await
        .expect("materialize client history");
    let mut exact_request = responses_request(materialized.client_items.clone());
    exact_request.items.push(user_message("follow-up"));
    let exact_write = chain
        .begin(owner.clone(), exact_request)
        .await
        .expect("discover exact projected parent");
    assert_eq!(exact_write.parent.parent_id.as_deref(), Some(id.as_str()));

    let mut edited_items = materialized.client_items;
    let edited_index = edited_items
        .iter()
        .position(|item| {
            item.thinking_ref()
                .is_some_and(|(thinking, _)| thinking.contains("C1"))
        })
        .expect("projected thinking carrier");
    let edited_thinking = edited_items[edited_index]
        .thinking_ref()
        .expect("projected thinking carrier")
        .0
        .replace("C1", "edited C1");
    *edited_items[edited_index]
        .thinking_mut()
        .expect("mutable projected thinking carrier") = edited_thinking;
    edited_items.push(user_message("follow-up"));
    let edited_write = chain
        .begin(owner.clone(), responses_request(edited_items))
        .await
        .expect("begin semantic edit");
    assert!(
        edited_write.parent.parent_id.is_none(),
        "semantic edits must create a new root"
    );

    let mut replay = responses_request(Vec::new());
    let Some(ProtocolExt::OpenResponses(extension)) = replay.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some(id);
    let replay = chain
        .begin(owner.clone(), replay)
        .await
        .expect("materialize persisted generation");
    assert_eq!(replay.request().items.len(), 6);
    assert_eq!(
        replay.request().items[0].thinking_ref(),
        Some((unknown_marker_text.as_str(), None))
    );
    assert_eq!(replay.request().items[1].thinking_ref(), Some(("R1", None)));
    assert_eq!(replay.request().items[2].output_text_ref(), Some("C1"));
    assert_eq!(
        replay.request().items[3].thinking_ref(),
        Some((marker_text.as_str(), None))
    );
    assert_eq!(replay.request().items[4].thinking_ref(), Some(("R2", None)));
    assert_eq!(replay.request().items[5].output_text_ref(), Some("answer"));
    let mut resolved_replay = replay.request().clone();
    crate::history_marker::resolve_request_markers(
        marker_store.as_ref(),
        &owner,
        &mut resolved_replay,
    )
    .await
    .expect("resolve persisted marker");
    assert_eq!(resolved_replay.items.len(), 5);
    assert_eq!(resolved_replay.items[0].thinking_ref(), Some(("R1", None)));
    assert_eq!(resolved_replay.items[1].output_text_ref(), Some("C1"));
    assert_eq!(
        resolved_replay.items[2].thinking_ref(),
        Some(("R1", Some("opaque")))
    );
    assert_eq!(resolved_replay.items[3].thinking_ref(), Some(("R2", None)));
    assert_eq!(resolved_replay.items[4].output_text_ref(), Some("answer"));
}

#[tokio::test]
async fn persisted_unavailable_marker_text_does_not_poison_a_continuation() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let marker_store: Arc<dyn crate::history_marker::HistoryMarkerStore> =
        Arc::new(crate::history_marker::SqlHistoryMarkerStore::sqlite(pool));
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    )
    .with_history_markers(marker_store);
    let owner = principal("owner");
    let unavailable =
        crate::history_marker::render_history_marker_reference("hm_abcdefghijklmnopqrst");
    let mut root = chain
        .begin(
            owner.clone(),
            responses_request(vec![AiItem::thinking(unavailable, None)]),
        )
        .await
        .expect("begin root");
    let root_id = root.id().to_owned();
    let mut root_response = AiResponse::new("upstream", "model");
    root_response.push_output_text("first answer");
    assert!(root.stage(&mut root_response, &generation_source(), None));
    root.persist()
        .await
        .expect("persist unavailable marker as inert client text");

    let mut request = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some(root_id);
    let mut continuation = chain
        .begin(owner, request)
        .await
        .expect("begin continuation");
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("second answer");
    assert!(continuation.stage(&mut response, &generation_source(), None));

    continuation
        .persist()
        .await
        .expect("unavailable marker-like text must remain inert");
}

#[tokio::test]
async fn begin_reports_a_typed_missing_parent_error() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let mut request = responses_request(vec![user_message("new input")]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some("resp_missing".into());

    let error = chain
        .begin(owner, request)
        .await
        .err()
        .expect("missing parent must reject begin");

    assert_eq!(error, BeginError::PreviousResponseNotFound);
}

#[tokio::test]
async fn begin_reports_a_typed_missing_item_reference_error() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let mut reference = user_message("");
    reference.meta = Some(serde_json::json!({
        "__open_responses_item_reference": "msg_missing"
    }));

    let error = chain
        .begin(owner, responses_request(vec![reference]))
        .await
        .err()
        .expect("missing item reference must reject begin");

    assert_eq!(error, BeginError::ItemReferenceNotFound);
}

#[tokio::test]
async fn write_materializes_an_explicit_parent_before_observation() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let mut root = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin root");
    let root_id = root.id().to_owned();
    assert_eq!(root.root_id(), root_id);
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    root.stage(&mut response, &generation_source(), None);
    root.persist().await.expect("persist root");

    let mut continuation = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some(root_id.clone());
    let write = chain
        .begin(owner, continuation)
        .await
        .expect("begin continuation");

    assert_eq!(write.root_id(), root_id);
    assert_eq!(write.request().items.len(), 3);
    assert_eq!(write.request().items[0].content.to_text(), "question");
    assert_eq!(write.request().items[1].content.to_text(), "answer");
    assert_eq!(write.request().items[2].content.to_text(), "follow-up");
}

#[tokio::test]
async fn persist_requires_a_staged_response() {
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
    let owner = principal("owner");
    let mut write = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin write");
    let id = TurnNodeId::new(write.id().to_owned());

    assert!(matches!(
        write.persist().await,
        Err(PersistError::NotStaged)
    ));
    assert!(
        backend
            .materialize(&owner, TurnNodeKind::Response, &id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropping_a_staged_write_does_not_persist_a_node() {
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
    let owner = principal("owner");
    let mut write = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin write");
    let id = TurnNodeId::new(write.id().to_owned());
    let mut dropped = AiResponse::new("upstream", "model");
    write.stage(&mut dropped, &generation_source(), None);
    drop(write);

    assert!(
        backend
            .materialize(&owner, TurnNodeKind::Response, &id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_later_stage_replaces_the_unpersisted_response() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let mut write = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin write");
    let id = write.id().to_owned();
    let mut intermediate = AiResponse::new("upstream", "model");
    intermediate.push_output_text("intermediate");
    write.stage(&mut intermediate, &generation_source(), None);
    let mut final_response = AiResponse::new("upstream", "model");
    final_response.push_output_text("final");
    write.stage(&mut final_response, &generation_source(), None);
    write.persist().await.expect("persist final response");

    let mut continuation = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some(id);
    let continuation = chain
        .begin(owner, continuation)
        .await
        .expect("begin continuation");

    assert_eq!(continuation.request().items[1].content.to_text(), "final");
}

#[tokio::test]
async fn observe_effective_persists_rewritten_history() {
    let backend: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let chain =
        GenerationChain::from_turn_chain(Arc::clone(&backend), Duration::from_secs(60), None);
    let owner = principal("owner");
    let mut write = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("client input")]),
        )
        .await
        .expect("begin write");
    let id = write.id().to_owned();
    let mut effective = write.request().clone();
    effective.instructions = Some("rewritten instructions".into());
    effective.items = vec![user_message("rewritten input")];
    write.observe_effective(effective);
    let mut response = AiResponse::new("upstream", "model");
    response.push_output_text("answer");
    write.stage(&mut response, &generation_source(), None);
    write.persist().await.expect("persist response");
    drop(chain);
    let chain = GenerationChain::from_turn_chain(backend, Duration::from_secs(60), None);

    let mut continuation = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some(id);
    let continuation = chain
        .begin(owner, continuation)
        .await
        .expect("begin continuation");

    assert_eq!(
        continuation.request().instructions.as_deref(),
        Some("rewritten instructions")
    );
    assert_eq!(
        continuation.request().items[0].content.to_text(),
        "rewritten input"
    );
}

#[tokio::test]
async fn automatic_parent_discovery_writes_only_a_nonempty_branch_delta() {
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
    let owner = principal("owner");
    let question = user_message("question");
    let mut root = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .expect("begin root");
    let mut root_response = AiResponse::new("upstream", "model");
    root_response.push_output_text("answer");
    let answer = root_response.items[0].clone();
    root.stage(&mut root_response, &generation_source(), None);
    root.persist().await.expect("persist root");

    let mut branch = chain
        .begin(
            owner.clone(),
            responses_request(vec![question, answer, user_message("follow-up")]),
        )
        .await
        .expect("discover parent");
    let branch_id = TurnNodeId::new(branch.id().to_owned());
    let mut branch_response = AiResponse::new("upstream", "model");
    branch_response.push_output_text("continued");
    branch.stage(&mut branch_response, &generation_source(), None);
    branch.persist().await.expect("persist branch");

    let nodes = backend
        .materialize(&owner, TurnNodeKind::Response, &branch_id)
        .await
        .expect("materialize branch");
    assert_eq!(nodes.len(), 2, "branch must retain the discovered parent");
}

#[tokio::test]
async fn an_identical_full_request_creates_a_new_root() {
    let backend = Arc::new(crate::turn_chain::test_store().await);
    let chain = GenerationChain::from_turn_chain(backend.clone(), Duration::from_secs(60), None);
    let owner = principal("owner");
    let question = user_message("question");
    let mut first = chain
        .begin(owner.clone(), responses_request(vec![question.clone()]))
        .await
        .expect("begin first write");
    let mut first_response = AiResponse::new("upstream", "model");
    first_response.push_output_text("answer");
    let answer = first_response.items[0].clone();
    first.stage(&mut first_response, &generation_source(), None);
    first.persist().await.expect("persist first write");

    let mut retry = chain
        .begin(owner.clone(), responses_request(vec![question, answer]))
        .await
        .expect("begin identical retry");
    let retry_id = TurnNodeId::new(retry.id().to_owned());
    let mut retry_response = AiResponse::new("upstream", "model");
    retry.stage(&mut retry_response, &generation_source(), None);
    retry.persist().await.expect("persist retry");

    let nodes = backend
        .materialize(&owner, TurnNodeKind::Response, &retry_id)
        .await
        .expect("materialize retry");
    assert_eq!(
        nodes.len(),
        1,
        "identical retry must not create an empty delta"
    );
}

#[tokio::test]
async fn begin_does_not_resolve_a_parent_across_principals() {
    let chain = generation_chain().await;
    let mut owner_write = chain
        .begin(
            principal("owner"),
            responses_request(vec![user_message("private")]),
        )
        .await
        .expect("begin owner write");
    let owner_id = owner_write.id().to_owned();
    let mut owner_response = AiResponse::new("upstream", "model");
    owner_write.stage(&mut owner_response, &generation_source(), None);
    owner_write.persist().await.expect("persist owner write");

    let mut request = responses_request(vec![user_message("guess")]);
    let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
        panic!("Open Responses request");
    };
    extension.previous_response_id = Some(owner_id);

    let error = chain
        .begin(principal("attacker"), request)
        .await
        .err()
        .expect("cross-Principal parent must be hidden");
    assert_eq!(error, BeginError::PreviousResponseNotFound);
}

#[tokio::test]
async fn completed_and_incomplete_writes_can_both_be_parents() {
    for status in ["completed", "incomplete"] {
        let chain = generation_chain().await;
        let owner = principal(status);
        let mut root = chain
            .begin(
                owner.clone(),
                responses_request(vec![user_message("question")]),
            )
            .await
            .expect("begin root");
        let root_id = root.id().to_owned();
        let mut response = AiResponse::new("upstream", "model");
        response.vendor.egress.insert(
            "__open_responses_terminal".into(),
            serde_json::json!({ "status": status }),
        );
        root.stage(&mut response, &generation_source(), None);
        root.persist().await.expect("persist terminal response");

        let mut request = responses_request(vec![user_message("follow-up")]);
        let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
            panic!("Open Responses request");
        };
        extension.previous_response_id = Some(root_id);
        assert!(
            chain.begin(owner, request).await.is_ok(),
            "{status} response must be a valid parent"
        );
    }
}

#[tokio::test]
async fn write_stage_rejects_failed_terminals() {
    let chain = generation_chain().await;
    let mut write = chain
        .begin(
            principal("owner"),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin");
    let mut response = AiResponse::new("upstream", "model");
    response.vendor.egress.insert(
        "__open_responses_terminal".into(),
        serde_json::json!({ "status": "failed" }),
    );
    assert!(!write.stage(&mut response, &generation_source(), None));
    assert!(matches!(
        write.persist().await,
        Err(PersistError::NotStaged)
    ));
}

#[tokio::test]
async fn write_stage_rejects_unprojected_post_text_thinking() {
    let chain = generation_chain().await;
    let mut write = chain
        .begin(
            principal("owner"),
            chat_request(serde_json::json!([
                {"role": "user", "content": "question"}
            ])),
        )
        .await
        .expect("begin");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::output_text("answer"),
        AiItem::thinking("authoritative reasoning", None),
    ];

    assert!(!write.stage(&mut response, &generation_source(), None));
    assert!(matches!(
        write.persist().await,
        Err(PersistError::NotStaged)
    ));
}
