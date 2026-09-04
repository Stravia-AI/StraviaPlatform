use super::*;

#[tokio::test]
async fn generation_parent_prefers_its_target_without_native_continuation() {
    let chain = generation_chain().await;
    let owner = principal("owner");
    let mut root = chain
        .begin(
            owner.clone(),
            responses_request(vec![user_message("question")]),
        )
        .await
        .expect("begin root");
    let mut response = AiResponse::new("response", "model");
    response.items = vec![AiItem::output_text("answer")];
    mark_generation_target(
        &mut response,
        "provider:model",
        OPEN_RESPONSES_2026_04_24,
        "model",
        "provider:model",
    );
    root.stage(&mut response, None);
    root.persist().await.expect("persist root");

    let mut continuation = responses_request(vec![user_message("follow-up")]);
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        panic!("Open Responses extension");
    };
    extension.previous_response_id = Some(root.id().to_owned());

    assert_eq!(
        chain
            .continuation_lookup()
            .preferred_target(&owner, &continuation)
            .await
            .as_deref(),
        Some("provider:model")
    );
}

#[tokio::test]
async fn chat_reasoning_prefix_restores_encrypted_effective_history() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    );
    let owner = principal("owner");
    let mut root = chain
        .begin(
            owner.clone(),
            chat_request(serde_json::json!([
                {"role": "user", "content": "question"}
            ])),
        )
        .await
        .expect("begin root");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::reasoning(
            vec!["visible ".into()],
            vec!["reasoning".into()],
            Some("encrypted".into()),
        ),
        AiItem::output_text("answer"),
    ];
    root.stage(&mut response, Some("upstream-response".into()));
    root.persist().await.expect("persist root");

    let resumed = chain
        .begin(
            owner.clone(),
            chat_request(serde_json::json!([
                {"role": "user", "content": "question"},
                {
                    "role": "assistant",
                    "reasoning_content": "visible reasoning",
                    "content": "answer"
                },
                {"role": "user", "content": "follow-up"}
            ])),
        )
        .await
        .expect("discover matching Chat prefix");
    assert_eq!(resumed.parent.parent_id.as_deref(), Some(root.id()));
    assert_eq!(resumed.request_delta.items.len(), 1);

    let lookup = chain.continuation_lookup();
    let mut continued = resumed.request().clone();
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
                &mut continued,
            )
            .await
            .as_deref(),
        Some("upstream-response")
    );
    assert_eq!(continued.items.len(), 1);
    assert_eq!(continued.items[0].content.to_text(), "follow-up");

    let mut materialized = resumed.request().clone();
    assert_eq!(
        lookup
            .prepare(
                &owner,
                crate::model_turn::ContinuationTarget {
                    namespace: "different-account",
                    protocol: OPEN_RESPONSES_2026_04_24,
                    actual_model: "model",
                    logical_model: "model",
                    allow_ephemeral_response: true,
                },
                &mut materialized,
            )
            .await,
        None
    );
    assert!(materialized.items.iter().any(|item| {
        item.reasoning_ref()
            .is_some_and(|(_, _, encrypted)| encrypted == Some("encrypted"))
    }));
    let encoded = crate::protocol::transform::ProtocolTransform::global()
        .bind(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            OPEN_RESPONSES_2026_04_24,
        )
        .expect("registered protocol pair")
        .encode_request(&materialized)
        .expect("materialized encrypted reasoning remains representable");
    assert_eq!(encoded.body["input"][1]["encrypted_content"], "encrypted");

    let mismatched = chain
        .begin(
            owner,
            chat_request(serde_json::json!([
                {"role": "user", "content": "question"},
                {
                    "role": "assistant",
                    "reasoning_content": "different reasoning",
                    "content": "answer"
                },
                {"role": "user", "content": "follow-up"}
            ])),
        )
        .await
        .expect("begin unmatched Chat request");
    assert!(mismatched.parent.parent_id.is_none());
}

#[tokio::test]
async fn native_responses_replay_uses_whitelisted_provider_context_for_continuation() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut initial = responses_request(vec![question.clone()]);
    initial.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    initial.meta.vendor.ingress.insert(
        GENERATION_SESSION_ID_META.into(),
        serde_json::Value::String("native-session".into()),
    );
    let Some(ProtocolExt::OpenResponses(extension)) = initial.ext.as_mut() else {
        unreachable!();
    };
    extension.store = Some(false);
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin root");
    let mut provider_effective = root.request().clone();
    provider_effective.generation.top_p = Some(0.98);
    root.observe_effective(provider_effective);
    let response_items = vec![
        AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into()))
            .with_graph_metadata(
                Some("rs_provider".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            ),
        AiItem::output_text("answer").with_graph_metadata(
            Some("msg_provider".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        ),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        })
        .with_graph_metadata(
            Some("fc_provider".into()),
            Some(AiItemStatus::Completed),
            AiItemProvenance::Provider,
            AiItemAudience::Client,
        ),
    ];
    let mut response = AiResponse::new("upstream", "model");
    response.items = response_items;
    root.stage(&mut response, Some("upstream-response".into()));
    root.persist().await.expect("persist root");

    let mut distractor_request = responses_request(vec![user_message("title question")]);
    distractor_request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    distractor_request.meta.vendor.ingress.insert(
        GENERATION_SESSION_ID_META.into(),
        serde_json::Value::String("native-session".into()),
    );
    let mut distractor = chain
        .begin(owner.clone(), distractor_request)
        .await
        .expect("begin same-session distractor");
    let mut distractor_response = AiResponse::new("title", "model");
    distractor_response.items = vec![
        AiItem::reasoning(
            vec!["title summary".into()],
            Vec::new(),
            Some("title encrypted".into()),
        ),
        AiItem::output_text("title answer"),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "title_call".into(),
            name: "lookup".into(),
            arguments: "{\"title\":true}".into(),
        }),
    ];
    distractor.stage(
        &mut distractor_response,
        Some("title-upstream-response".into()),
    );
    distractor
        .persist()
        .await
        .expect("persist same-session distractor");

    let replay_items = |encrypted: &str, text: &str, call_id: &str, arguments: &str| {
        vec![
            question.clone(),
            AiItem::reasoning(vec!["summary".into()], Vec::new(), Some(encrypted.into())),
            AiItem::output_text(text),
            AiItem::function_call(crate::protocol::ir::ToolCall {
                id: call_id.into(),
                name: "lookup".into(),
                arguments: arguments.into(),
            }),
            user_message("follow-up"),
        ]
    };
    let request_for = |items| {
        let mut request = responses_request(items);
        request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        request.meta.vendor.ingress.insert(
            GENERATION_SESSION_ID_META.into(),
            serde_json::Value::String("native-session".into()),
        );
        let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_mut() else {
            unreachable!();
        };
        extension.store = Some(false);
        request
    };

    let resumed = chain
        .begin(
            owner.clone(),
            request_for(replay_items(
                "encrypted",
                "answer",
                "call_1",
                "{\"value\":1}",
            )),
        )
        .await
        .expect("discover semantically identical replay");
    assert_eq!(resumed.request_delta.items.len(), 1);
    let mut provider_request = resumed.request().clone();
    assert_eq!(
        chain
            .continuation_lookup()
            .prepare(
                &owner,
                crate::model_turn::ContinuationTarget {
                    namespace: "",
                    protocol: OPEN_RESPONSES_2026_04_24,
                    actual_model: "model",
                    logical_model: "model",
                    allow_ephemeral_response: true,
                },
                &mut provider_request,
            )
            .await
            .as_deref(),
        Some("upstream-response")
    );
    assert_eq!(provider_request.items.len(), 1);
    assert_eq!(provider_request.items[0].content.to_text(), "follow-up");

    for changed in [
        replay_items("different", "answer", "call_1", "{\"value\":1}"),
        replay_items("encrypted", "different", "call_1", "{\"value\":1}"),
        replay_items("encrypted", "answer", "call_2", "{\"value\":1}"),
        replay_items("encrypted", "answer", "call_1", "{\"value\":2}"),
    ] {
        let rejected = chain
            .begin(owner.clone(), request_for(changed))
            .await
            .expect("begin changed replay");
        assert!(rejected.parent.parent_id.is_none());
    }
}

#[tokio::test]
async fn encrypted_reasoning_replay_omits_gateway_projected_item_id() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    );
    let owner = principal("owner");
    let mut initial = responses_request(vec![user_message("question")]);
    initial.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin root");
    let mut response = AiResponse::new("resp_gateway", "model");
    response.items = vec![
        AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("encrypted".into()))
            .with_graph_metadata(
                Some("rs_gateway_0".into()),
                Some(AiItemStatus::Completed),
                AiItemProvenance::Provider,
                AiItemAudience::Client,
            ),
        AiItem::output_text("answer"),
    ];
    root.stage(&mut response, Some("upstream-response".into()));
    root.persist().await.expect("persist root");

    let mut continuation = responses_request(vec![user_message("follow-up")]);
    continuation.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    let Some(ProtocolExt::OpenResponses(extension)) = continuation.ext.as_mut() else {
        unreachable!();
    };
    extension.previous_response_id = Some(root.id().to_owned());
    let resumed = chain
        .begin(owner.clone(), continuation)
        .await
        .expect("materialize response history");
    let mut replay = resumed.request().clone();
    assert_eq!(
        chain
            .continuation_lookup()
            .prepare(
                &owner,
                crate::model_turn::ContinuationTarget {
                    namespace: "different-account",
                    protocol: OPEN_RESPONSES_2026_04_24,
                    actual_model: "model",
                    logical_model: "model",
                    allow_ephemeral_response: true,
                },
                &mut replay,
            )
            .await,
        None
    );

    let encoded = crate::protocol::transform::ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .expect("registered protocol pair")
        .encode_request(&replay)
        .expect("encrypted reasoning replay remains representable");
    assert_eq!(encoded.body["input"][1]["encrypted_content"], "encrypted");
    assert!(
        encoded.body["input"][1].get("id").is_none(),
        "gateway-projected item IDs are not valid identities for provider-encrypted reasoning"
    );
}

#[tokio::test]
async fn automatic_parent_matches_anthropic_opaque_reasoning_replay() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut initial = responses_request(vec![question.clone()]);
    initial.instructions = Some("shared instructions".into());
    initial.meta.source_protocol = Some(crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01);
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin root");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::reasoning(Vec::new(), Vec::new(), Some("opaque".into())),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
    ];
    root.stage(&mut response, Some("upstream-response".into()));
    root.persist().await.expect("persist root");
    let root_id = root.id().to_owned();

    let mut resumed_request = responses_request(vec![
        AiItem {
            role: Role::Developer,
            content: MessageContent::Text("shared instructions".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        },
        question,
        AiItem {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                crate::protocol::ir::ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: Some("opaque".into()),
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
        },
        AiItem::function_call_output("call_1", serde_json::Value::String("result".into())),
    ]);
    resumed_request.meta.source_protocol =
        Some(crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01);
    let resumed = chain
        .begin(owner, resumed_request)
        .await
        .expect("begin resumed request");

    assert_eq!(resumed.parent.parent_id.as_deref(), Some(root_id.as_str()));
    assert_eq!(resumed.request_delta.items.len(), 1);
}

#[tokio::test]
async fn automatic_parent_matches_gemini_reasoning_and_tool_id_replay() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut initial = responses_request(vec![question.clone()]);
    initial.meta.source_protocol =
        Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin root");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::reasoning(vec!["summary".into()], Vec::new(), Some("opaque".into())),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
    ];
    root.stage(&mut response, Some("upstream-response".into()));
    root.persist().await.expect("persist root");
    let root_id = root.id().to_owned();

    let mut resumed_request = responses_request(vec![
        question,
        AiItem {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                crate::protocol::ir::ContentBlock::Thinking {
                    thinking: "summary".into(),
                    signature: Some("opaque".into()),
                },
                crate::protocol::ir::ContentBlock::ToolUse {
                    id: "call_client_random".into(),
                    name: "lookup".into(),
                    input: serde_json::json!({"value": 1}),
                    cache_control: None,
                },
            ]),
            tool_calls: Some(vec![crate::protocol::ir::ToolCall {
                id: "call_client_random".into(),
                name: "lookup".into(),
                arguments: "{\"value\":1}".into(),
            }]),
            tool_call_id: None,
            meta: None,
        },
        AiItem {
            role: Role::Tool,
            content: MessageContent::Blocks(vec![crate::protocol::ir::ContentBlock::ToolResult {
                tool_use_id: "lookup".into(),
                content: serde_json::Value::String("result".into()),
                is_error: None,
                cache_control: None,
            }]),
            tool_calls: None,
            tool_call_id: Some("lookup".into()),
            meta: None,
        },
    ]);
    resumed_request.meta.source_protocol =
        Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    let mut resumed = chain
        .begin(owner, resumed_request)
        .await
        .expect("begin resumed request");

    assert_eq!(resumed.parent.parent_id.as_deref(), Some(root_id.as_str()));
    assert_eq!(resumed.request_delta.items.len(), 1);
    assert_eq!(
        resumed
            .request
            .items
            .last()
            .and_then(|item| item.tool_call_id.as_deref()),
        Some("call_1")
    );

    let mut second_response = AiResponse::new("upstream-2", "model");
    second_response.items = vec![
        AiItem::reasoning(
            vec!["second summary".into()],
            Vec::new(),
            Some("opaque-2".into()),
        ),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_2".into(),
            name: "lookup".into(),
            arguments: "{\"value\":2}".into(),
        }),
    ];
    resumed.stage(&mut second_response, Some("upstream-response-2".into()));
    resumed.persist().await.expect("persist second turn");
    let second_id = resumed.id().to_owned();

    let assistant_turn = |thinking: &str, signature: &str, id: &str, value: i64| AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![
            crate::protocol::ir::ContentBlock::Thinking {
                thinking: thinking.into(),
                signature: Some(signature.into()),
            },
            crate::protocol::ir::ContentBlock::ToolUse {
                id: id.into(),
                name: "lookup".into(),
                input: serde_json::json!({"value": value}),
                cache_control: None,
            },
        ]),
        tool_calls: Some(vec![crate::protocol::ir::ToolCall {
            id: id.into(),
            name: "lookup".into(),
            arguments: format!(r#"{{"value":{value}}}"#),
        }]),
        tool_call_id: None,
        meta: None,
    };
    let tool_result = |value: &str| AiItem {
        role: Role::Tool,
        content: MessageContent::Blocks(vec![crate::protocol::ir::ContentBlock::ToolResult {
            tool_use_id: "lookup".into(),
            content: serde_json::Value::String(value.into()),
            is_error: None,
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: Some("lookup".into()),
        meta: None,
    };
    let mut third_request = responses_request(vec![
        user_message("question"),
        assistant_turn("summary", "opaque", "random-1", 1),
        tool_result("result"),
        assistant_turn("second summary", "opaque-2", "random-2", 2),
        tool_result("result-2"),
    ]);
    third_request.meta.source_protocol =
        Some(crate::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
    let third = chain
        .begin(principal("owner"), third_request)
        .await
        .expect("begin third request");

    assert_eq!(third.parent.parent_id.as_deref(), Some(second_id.as_str()));
    assert_eq!(third.request_delta.items.len(), 1);
    assert_eq!(
        third
            .request
            .items
            .last()
            .and_then(|item| item.tool_call_id.as_deref()),
        Some("call_2")
    );
}

#[tokio::test]
async fn automatic_parent_matches_anthropic_output_replayed_as_responses_items() {
    let chain = GenerationChain::from_turn_chain(
        Arc::new(crate::turn_chain::test_store().await),
        Duration::from_secs(60),
        None,
    );
    let owner = principal("owner");
    let question = user_message("question");
    let mut initial = responses_request(vec![question.clone()]);
    initial.meta.source_protocol = Some(crate::protocol::ids::ANTHROPIC_MESSAGES_2023_06_01);
    let mut root = chain
        .begin(owner.clone(), initial)
        .await
        .expect("begin Anthropic root");
    let mut response = AiResponse::new("upstream", "model");
    response.items = vec![
        AiItem::reasoning(
            vec!["summary".into()],
            vec!["reasoning".into()],
            Some("opaque".into()),
        ),
        AiItem::output_text("answer"),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
    ];
    root.stage(&mut response, Some("upstream-response".into()));
    root.persist().await.expect("persist Anthropic root");
    let root_id = root.id().to_owned();

    let mut resumed_request = responses_request(vec![
        question,
        AiItem::reasoning(
            vec!["summary".into()],
            vec!["reasoning".into()],
            Some("opaque".into()),
        ),
        AiItem::output_text("answer"),
        AiItem::function_call(crate::protocol::ir::ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
        user_message("follow-up"),
    ]);
    resumed_request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
    resumed_request.meta.vendor.ingress.insert(
        GENERATION_SESSION_ID_META.into(),
        serde_json::Value::String("responses-session".into()),
    );
    let resumed = chain
        .begin(owner, resumed_request)
        .await
        .expect("begin Responses replay");

    assert_eq!(resumed.parent.parent_id.as_deref(), Some(root_id.as_str()));
    assert_eq!(resumed.request_delta.items.len(), 1);
}

#[test]
fn open_responses_projects_stamped_graph_ids_and_resolves_them() {
    let mut response = AiResponse::new("resp_saved", "model");
    response.items = vec![AiItem::output_text("saved answer")];
    let output =
        project_client_history(OPEN_RESPONSES_2026_04_24, &response, &mut []).expect("project");
    let id = output[0].id_ref().expect("stamped id").to_owned();
    assert!(id.starts_with("msg_saved") || id.starts_with("msg_"));

    let mut request_items = vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(serde_json::json!({"__open_responses_item_reference": id})),
    }];
    resolve_protocol_item_references(OPEN_RESPONSES_2026_04_24, &mut request_items, &output)
        .expect("resolve");
    assert_eq!(request_items[0].content.to_text(), "saved answer");
    assert_eq!(
        request_items[0].id_ref().map(str::to_owned),
        output[0].id_ref().map(str::to_owned)
    );
}

#[test]
fn chat_projects_flattened_assistant_history() {
    let mut response = AiResponse::new("chat", "model");
    response.items = vec![
        AiItem::thinking("scratch", None),
        AiItem::output_text("answer"),
    ];
    let output = project_client_history(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &response, &mut [])
        .expect("project");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].role, Role::Assistant);
    assert!(matches!(
        &output[0].content,
        MessageContent::Blocks(blocks)
            if blocks.iter().any(|block| matches!(block, ContentBlock::Thinking { .. }))
            && blocks.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "answer"))
    ));
}

#[test]
fn anthropic_projects_reasoning_as_thinking() {
    let mut response = AiResponse::new("ant", "model");
    response.items = vec![
        AiItem::reasoning(Vec::new(), Vec::new(), Some("opaque".into())),
        AiItem::function_call(ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }),
    ];
    let output =
        project_client_history(ANTHROPIC_MESSAGES_2023_06_01, &response, &mut []).expect("project");
    assert_eq!(output.len(), 1);
    let MessageContent::Blocks(blocks) = &output[0].content else {
        panic!("expected blocks");
    };
    assert!(
            blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Thinking { signature: Some(sig), .. } if sig == "opaque"))
        );
}

#[test]
fn gemini_rewrites_tool_ids_across_the_client_prefix() {
    let mut prefix = vec![AiItem {
        role: Role::Assistant,
        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "lookup".into(),
            input: serde_json::json!({"value": 1}),
            cache_control: None,
        }]),
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments: "{\"value\":1}".into(),
        }]),
        tool_call_id: None,
        meta: None,
    }];
    let mut response = AiResponse::new("gemini", "model");
    response.items = vec![AiItem::output_text("done")];
    let output = project_client_history(
        GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        &response,
        &mut prefix,
    )
    .expect("project");
    let MessageContent::Blocks(blocks) = &prefix[0].content else {
        panic!("expected blocks");
    };
    let ContentBlock::ToolUse { id, .. } = &blocks[0] else {
        panic!("expected tool use");
    };
    assert!(id.starts_with("gemini_call_"));
    assert_eq!(prefix[0].tool_calls.as_ref().unwrap()[0].id, *id);
    assert_eq!(output.len(), 1);
}

#[test]
fn chat_rejects_item_references() {
    let mut items = vec![AiItem {
        role: Role::User,
        content: MessageContent::Text(String::new()),
        tool_calls: None,
        tool_call_id: None,
        meta: Some(serde_json::json!({"__open_responses_item_reference": "msg_saved"})),
    }];
    let error =
        resolve_protocol_item_references(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &mut items, &[])
            .expect_err("item references are Open Responses only");
    assert_eq!(error, "item_reference_not_found");
}
