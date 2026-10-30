use super::*;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::post,
};
use base64::Engine;
use tower::ServiceExt;

#[tokio::test]
async fn opaque_media_uses_inline_bytes_and_standard_media_keeps_signed_downloads() {
    use stravia_runtime_contract::protocol::ids::ProtocolIdentity;
    use stravia_runtime_contract::protocol::ir::{
        AiItem, AiRequest, ContentBlock, DocumentSource, MediaSource, MessageContent,
        ToolResultContentKind,
    };

    fn assert_image_bytes(block: &ContentBlock, expected: &[u8]) {
        let ContentBlock::Image {
            source: MediaSource::Base64 { media_type, data },
            ..
        } = block
        else {
            panic!("opaque guest must receive canonical inline image bytes");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            expected
        );
    }

    fn assert_reference(blocks: &[ContentBlock], reference: &str) {
        let occurrences: usize = blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.matches(reference).count(),
                _ => 0,
            })
            .sum();
        assert_eq!(
            occurrences, 1,
            "attachment reference must remain model-readable"
        );
    }

    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    gateway
        .storage
        .settings()
        .set(
            "artifact_settings",
            r#"{"external_signed_downloads":true,"client_base_url":"https://downloads.example","file_public_base_url":"https://downloads.example"}"#,
        )
        .await
        .unwrap();
    let owner = gateway
        .admin()
        .create_api_key(
            serde_json::from_value(serde_json::json!({
                "name": "opaque-media-owner",
                "transparent_injection_enabled": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let owner = Principal::new(owner.id);
    let bytes = include_bytes!("../../tests/fixtures/media/transparent.png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let image = ContentBlock::Image {
        source: MediaSource::Base64 {
            media_type: "image/png".into(),
            data: encoded.clone(),
        },
        detail: None,
        cache_control: None,
    };
    let pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7\nopaque document\n");
    let mut item = AiItem::output_text("");
    item.role = stravia_runtime_contract::protocol::ir::Role::User;
    item.content = MessageContent::Blocks(vec![
        image.clone(),
        ContentBlock::Document {
            source: DocumentSource::Blocks {
                content: vec![image.clone()],
            },
            title: None,
            context: None,
            cache_control: None,
        },
        ContentBlock::ToolResult {
            tool_use_id: "nested-media".into(),
            content: serde_json::to_value(vec![image.clone()]).unwrap(),
            content_kind: Some(ToolResultContentKind::ContentBlocks),
            is_error: None,
            cache_control: None,
        },
        ContentBlock::Document {
            source: DocumentSource::Base64Pdf { data: pdf },
            title: None,
            context: None,
            cache_control: None,
        },
    ]);
    let original = Arc::new(AiRequest::new("model", vec![item]));
    let mut normalized = original.clone();
    ingest::normalize_shared_request(
        &gateway,
        &owner,
        &mut normalized,
        &stravia_runtime_contract::CancellationToken::new(),
    )
    .await
    .unwrap();
    let MessageContent::Blocks(normalized_blocks) = &normalized.items[0].content else {
        panic!("expected normalized media blocks");
    };
    let ContentBlock::Image {
        source: MediaSource::Url(image_reference),
        ..
    } = &normalized_blocks[0]
    else {
        panic!("expected stored image reference");
    };
    let ContentBlock::Document {
        source: DocumentSource::Url(pdf_reference),
        ..
    } = &normalized_blocks[3]
    else {
        panic!("expected stored PDF reference");
    };
    // 与 Target preparation 一样从 guest identity 查找共享 codec；空 hint 也没有 endpoint。
    let identity = ProtocolIdentity::new("opaque-test-plugin/private-api/v7");
    assert!(identity.protocol().is_none());
    for hint in [identity.as_str(), ""] {
        let protocol =
            stravia_protocol_codec::registry::ProtocolRegistry::global().resolve_alias(hint);
        assert!(protocol.is_none());
        let mut request = normalized.clone();
        let transfers = ingest::materialize_request(&gateway, &owner, &mut request, protocol)
            .await
            .unwrap();
        assert!(
            transfers.is_empty(),
            "opaque delivery must not issue URL grants"
        );
        let MessageContent::Blocks(blocks) = &request.items[0].content else {
            panic!("expected media blocks");
        };
        assert_image_bytes(&blocks[0], bytes);
        assert_reference(blocks, image_reference);
        assert_reference(blocks, pdf_reference);
        let ContentBlock::Document {
            source: DocumentSource::Blocks { content },
            ..
        } = &blocks[1]
        else {
            panic!("expected nested document");
        };
        assert_image_bytes(&content[0], bytes);
        assert_reference(content, image_reference);
        let ContentBlock::ToolResult { content, .. } = &blocks[2] else {
            panic!("expected nested tool result");
        };
        let content: Vec<ContentBlock> = serde_json::from_value(content.clone()).unwrap();
        assert_image_bytes(&content[0], bytes);
        assert_reference(&content, image_reference);
        let ContentBlock::Document {
            source: DocumentSource::Base64Pdf { data },
            ..
        } = &blocks[3]
        else {
            panic!("opaque guest must receive inline PDF bytes");
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            b"%PDF-1.7\nopaque document\n"
        );
    }
    let other = gateway
        .admin()
        .create_api_key(
            serde_json::from_value(serde_json::json!({
                "name": "other-owner",
                "transparent_injection_enabled": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let other = Principal::new(other.id);
    let mut unauthorized = normalized.clone();
    assert!(
        ingest::materialize_request(&gateway, &other, &mut unauthorized, None)
            .await
            .is_err(),
        "inline delivery must retain Artifact ownership enforcement"
    );

    let MessageContent::Blocks(blocks) = &normalized.items[0].content else {
        panic!("expected normalized media blocks");
    };
    let mut standard_item = AiItem::output_text("");
    standard_item.role = stravia_runtime_contract::protocol::ir::Role::User;
    standard_item.content = MessageContent::Blocks(vec![blocks[0].clone()]);
    let mut standard = Arc::new(AiRequest::new("model", vec![standard_item]));
    let protocol = stravia_protocol_codec::registry::ProtocolRegistry::global()
        .resolve_alias("openai-compatible/chat-completions/v1")
        .expect("standard shared endpoint");
    let transfers = ingest::materialize_request(&gateway, &owner, &mut standard, Some(protocol))
        .await
        .unwrap();
    let MessageContent::Blocks(blocks) = &standard.items[0].content else {
        panic!("expected standard media blocks");
    };
    let ContentBlock::Image {
        source: MediaSource::Url(url),
        ..
    } = &blocks[0]
    else {
        panic!("standard image target must still receive a signed URL");
    };
    assert!(url.starts_with("https://downloads.example/v1/artifacts/downloads/"));
    assert_reference(blocks, image_reference);
    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].0, *url);
    let download = crate::proxy::server::create_router(gateway.clone())
        .oneshot(
            Request::get(url.strip_prefix("https://downloads.example").unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(download.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .as_ref(),
        bytes
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn shared_text_normalization_scrubs_parameter_keys_and_keeps_sql_errors() {
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest};
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let owner = Principal::new("shared-input-owner");
    let cancellation = stravia_runtime_contract::CancellationToken::new();
    let grant = "stravia_upload_header.payload.signature";
    let mut item = AiItem::output_text("ordinary text");
    item.role = stravia_runtime_contract::protocol::ir::Role::User;
    let mut input = AiRequest::new("model", vec![item]);
    input.instructions = Some("existing instructions".into());
    input.parameters.session.id = Some(grant.into());
    input.parameters.metadata = Some(std::collections::BTreeMap::from([(
        grant.to_owned(),
        grant.to_owned(),
    )]));
    let mut input = Arc::new(input);
    ingest::normalize_shared_request(&gateway, &owner, &mut input, &cancellation)
        .await
        .unwrap();
    let parameters = serde_json::to_value(&input.parameters).unwrap();
    assert_eq!(parameters["session"]["id"], "<stravia-upload-key>");
    assert_eq!(
        parameters["metadata"]["<stravia-upload-key>"],
        "<stravia-upload-key>"
    );
    assert_eq!(input.instructions.as_deref(), Some("existing instructions"));
    assert!(matches!(
        &input.items[0].content,
        stravia_runtime_contract::protocol::ir::MessageContent::Text(text) if text.as_str() == "ordinary text"
    ));

    gateway
        .storage
        .settings()
        .set(
            "artifact_settings",
            r#"{"upload_prompt_injection":true,"client_base_url":"https://upload.example"}"#,
        )
        .await
        .unwrap();
    ingest::normalize_shared_request(&gateway, &owner, &mut input, &cancellation)
        .await
        .unwrap();
    let instructions = input.instructions.clone().unwrap();
    assert!(instructions.starts_with("existing instructions\n\n"));
    assert!(instructions.contains("https://upload.example"));
    ingest::normalize_shared_request(&gateway, &owner, &mut input, &cancellation)
        .await
        .unwrap();
    assert_eq!(input.instructions.as_deref(), Some(instructions.as_str()));

    gateway
        .storage
        .settings()
        .set("log_retention_days", "invalid")
        .await
        .unwrap();
    let error = ingest::normalize_shared_request(&gateway, &owner, &mut input, &cancellation)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Invalid retention"));
    gateway
        .storage
        .settings()
        .set("log_retention_days", "7")
        .await
        .unwrap();
    gateway
        .storage
        .settings()
        .set("artifact_settings", "{")
        .await
        .unwrap();
    assert!(
        ingest::normalize_shared_request(&gateway, &owner, &mut input, &cancellation)
            .await
            .is_err()
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn shared_normalization_validates_nested_tool_result_content() {
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, ContentBlock, MessageContent};
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let item = AiItem {
        role: stravia_runtime_contract::protocol::ir::Role::Tool,
        content: MessageContent::Blocks(vec![ContentBlock::SearchResult {
            source: "source".into(),
            title: "title".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: serde_json::json!({"not": "content blocks"}),
                content_kind: Some(
                    stravia_runtime_contract::protocol::ir::ToolResultContentKind::ContentBlocks,
                ),
                is_error: None,
                cache_control: None,
            }],
            cache_control: None,
        }]),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    };
    let mut request = Arc::new(AiRequest::new("model", vec![item]));
    assert!(
        ingest::normalize_shared_request(
            &gateway,
            &Principal::new("owner"),
            &mut request,
            &stravia_runtime_contract::CancellationToken::new(),
        )
        .await
        .is_err()
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn public_model_input_snapshots_media_without_scanning_text() {
    let received = Arc::new(tokio::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let captures = received.clone();
    let provider = Router::new().route("/v1/chat/completions", post(move |Json(body): Json<serde_json::Value>| {
        let captures = captures.clone();
        async move {
            captures.lock().await.push(body);
            Json(serde_json::json!({"id":"media-response","object":"chat.completion","created":1,"model":"vision","choices":[{"index":0,"message":{"role":"assistant","content":"received"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider_task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let admin = gateway.admin();
    let provider = admin
        .create_provider(crate::db::models::CreateProvider {
            name: Some("attachment-wire".into()),
            source: crate::db::models::ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: format!("http://{address}/v1"),
                models_source: None,
                static_models: None,
            },
            credential: crate::db::models::ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .unwrap();
    admin.create_manual_provider_model(&provider.id, "vision", crate::provider_models::CreateManualProviderModel {
        metadata: serde_json::json!({"id":"vision","name":"Vision","modalities":{"input":["text","image"],"output":["text"]}}),
        template_id: None,
    }).await.unwrap();
    let route = admin
        .create_model(crate::db::models::CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "vision".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: provider.id,
                model: "vision".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await
        .unwrap();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "attachment-owner".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![route.id.into()],
            inject_media_understanding: false,
            spend_limit: None,
        })
        .await
        .unwrap();
    let router = crate::proxy::server::create_router(gateway.clone());
    let bytes = include_bytes!("../../tests/fixtures/media/transparent.png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let ordinary =
        "Do not fetch http://127.0.0.1/private or sa:not-a-file; YWJjZA== is ordinary text.";
    let principal = Principal::new(key.id.clone());
    let artifact = gateway
        .artifact_store
        .as_ref()
        .unwrap()
        .ingest(
            &principal,
            "image/png",
            Some(bytes.len() as u64),
            stravia_runtime_contract::artifact::bytes_stream(Bytes::from_static(bytes)),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    let upload_grant = gateway
        .upload_grants
        .issue(&Principal::new(key.id.clone()))
        .unwrap();
    let body = serde_json::json!({"model":"vision","safety_identifier":upload_grant.key,"tools":[{"type":"function","function":{"name":"client_action","description":"ordinary description","parameters":{"type":"object","properties":{(upload_grant.key.clone()):{"type":"string","description":"ordinary-value"}}}}}],"messages":[{"role":"user","content":[{"type":"text","text":ordinary},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{encoded}")}}]}]});
    for (index, enabled) in [false, true, false].into_iter().enumerate() {
        admin
            .update_api_key(
                &key.id,
                serde_json::from_value(serde_json::json!({
                    "transparent_injection_enabled": enabled
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", key.token))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response_body)
        );
        let wire = received.lock().await;
        assert_eq!(wire[index]["safety_identifier"], "<stravia-upload-key>");
        let client_tool = wire[index]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == "client_action")
            .unwrap();
        assert_eq!(
            client_tool["function"]["parameters"]["properties"]["<stravia-upload-key>"]["description"],
            "ordinary-value"
        );
        assert!(!wire[index].to_string().contains(&upload_grant.key));
        let contents = wire[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .collect::<Vec<_>>();
        assert!(contents.iter().any(|part| part["text"] == ordinary));
        let sent = contents
            .iter()
            .find_map(|part| {
                part.pointer("/image_url/url")
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(sent.split_once(',').unwrap().1)
                .unwrap(),
            bytes
        );
        assert_eq!(
            contents.iter().any(|part| part["text"]
                .as_str()
                .is_some_and(|text| text.contains(&artifact.reference()))),
            enabled,
            "attachment references must follow the transparent tool injection switch"
        );
    }
    let bad = serde_json::json!({"model":"vision","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://127.0.0.1/private"}}]}]});
    let rejected = router
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", key.token))
                .body(Body::from(bad.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(rejected.status().is_client_error());
    assert_eq!(
        received.lock().await.len(),
        3,
        "required attachment failure must precede provider execution"
    );
    use futures::StreamExt;
    use stravia_runtime_contract::agent::{
        AgentBudgets, AgentDefinitionConfig, AgentDefinitionExposure, AgentDefinitionId,
        AgentDefinitionSpec, AgentEvent, AgentInput, AgentSlug, ArtifactPolicy,
    };
    let definitions = crate::agent::AgentDefinitionRegistry::default();
    let definition_id = AgentDefinitionId::new("artifact_wire_agent");
    definitions
        .synchronize(vec![AgentDefinitionSpec {
            id: definition_id.clone(),
            slug: AgentSlug::new("artifact_wire_agent"),
            revision: 1,
            description: "Read an image".into(),
            instructions: "Describe the image".into(),
            output_schema: None,
            tools: vec![],
            budgets: AgentBudgets {
                total_wall_time: Duration::from_secs(60),
                working_wall_time: Duration::from_secs(50),
                model_turns: 4,
                tool_calls: Some(4),
                tool_parallelism: Some(2),
                concurrent_runs: Some(2),
                total_tokens: Some(1_000),
                finalization_tokens: Some(100),
            },
            artifact_policy: ArtifactPolicy {
                max_artifacts: 1,
                max_bytes: 100 * 1024 * 1024,
                allowed_mime_types: vec!["image/png".into()],
            },
            repair_attempts: 0,
            exposure: AgentDefinitionExposure::Public,
        }])
        .await
        .unwrap();
    definitions
        .patch_config(
            &definition_id,
            AgentDefinitionConfig {
                enabled: true,
                model_id: Some("vision".into()),
                thinking_level: None,
            },
        )
        .await
        .unwrap();
    let runner = crate::agent::AgentRunner::new(
        definitions,
        gateway.model_turn.clone(),
        vec![],
        gateway.turn_chains.clone(),
    )
    .unwrap()
    .with_artifact_store(gateway.artifact_store.clone());
    let events = runner
        .run(AgentInput {
            principal,
            definition_id,
            parent_turn_id: None,
            prompt: "Describe".into(),
            artifacts: vec![artifact.id],
            cancellation: stravia_runtime_contract::CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Completed(_))),
        "{events:?}"
    );
    let wire = received.lock().await;
    let agent_media = wire.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find_map(|part| {
            part.pointer("/image_url/url")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(agent_media.split_once(',').unwrap().1)
            .unwrap(),
        bytes
    );
    drop(wire);
    provider_task.abort();
}

#[tokio::test]
async fn public_gemini_generated_media_is_reusable_without_inline_history() {
    use axum::response::IntoResponse;
    let bytes = include_bytes!("../../tests/fixtures/media/transparent.png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let served = encoded.clone();
    let provider = Router::new().fallback(move |uri: axum::http::Uri| {
        let encoded = served.clone();
        async move {
            let output = serde_json::json!({"responseId":"generated-media","modelVersion":"painter","candidates":[{"content":{"role":"model","parts":[{"text":"painted"},{"inlineData":{"mimeType":"image/png","data":encoded},"thoughtSignature":"media-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}});
            if uri.path().contains("streamGenerateContent") {
                ([("content-type", "text/event-stream")], format!("data: {}\n\n", output)).into_response()
            } else { Json(output).into_response() }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let admin = gateway.admin();
    let provider = admin
        .create_provider(crate::db::models::CreateProvider {
            name: Some("generated-media".into()),
            source: crate::db::models::ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("google-gemini".into()),
                base_url: format!("http://{address}"),
                models_source: None,
                static_models: None,
            },
            credential: crate::db::models::ProviderCredentialInput::None,
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await
        .unwrap();
    admin.create_manual_provider_model(&provider.id, "painter", crate::provider_models::CreateManualProviderModel { metadata: serde_json::json!({"id":"painter","name":"Painter","modalities":{"input":["text","image"],"output":["text","image"]}}), template_id: None }).await.unwrap();
    let route = admin
        .create_model(crate::db::models::CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "painter".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: provider.id,
                model: "painter".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
            billing: None,
        })
        .await
        .unwrap();
    let router = crate::proxy::server::create_router(gateway.clone());
    for method in ["generateContent", "streamGenerateContent"] {
        let key = admin
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: format!("painter-owner-{method}"),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: true,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_generation: false,
                model_ids: vec![route.id.clone().into()],
                inject_media_understanding: false,
                spend_limit: None,
            })
            .await
            .unwrap();
        let response = router.clone().oneshot(Request::post(format!("/v1beta/models/painter:{method}")).header("content-type","application/json").header("x-goog-api-key", &key.token).body(Body::from(serde_json::json!({"contents":[{"role":"user","parts":[{"text":"Paint"}]}]}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("painted"));
        // Each mode has its own Principal, so a missing stream commit cannot
        // pass by finding the preceding unary request's saved Artifact.
        let rows = gateway._sqlite_pool.as_ref().unwrap();
        let nodes: Vec<(String, String)> = sqlx::query_as(
            "SELECT n.id, n.kind FROM turn_nodes n JOIN turn_scopes s ON s.id = n.scope_id WHERE s.principal = ?",
        )
        .bind(Principal::new(key.id.clone()).continuation_key())
        .fetch_all(rows)
        .await
        .unwrap();
        let owner = Principal::new(key.id.clone());
        let mut payloads = Vec::new();
        for (id, kind) in nodes {
            let kind: stravia_runtime_contract::turn_chain::TurnNodeKind =
                serde_json::from_value(serde_json::Value::String(kind)).unwrap();
            for node in gateway
                .turn_chains
                .materialize(
                    &owner,
                    kind,
                    &stravia_runtime_contract::turn_chain::TurnNodeId::new(id),
                )
                .await
                .unwrap()
            {
                payloads.push(node.payload.to_string());
            }
        }
        assert!(payloads.iter().all(|payload| !payload.contains(&encoded)));
        let reference = payloads
            .iter()
            .find_map(|payload| {
                let start = payload.find("stravia://artifacts/")?;
                let suffix = &payload[start..];
                Some(suffix.split('"').next().unwrap().to_owned())
            })
            .expect("generated media history must retain stable Artifact Reference");
        let id = ArtifactId::from_reference(&reference).unwrap();
        let (_, restored) = gateway
            .artifact_store
            .as_ref()
            .unwrap()
            .read_bytes(
                &Principal::new(key.id.clone()),
                &id,
                Duration::from_secs(3600),
            )
            .await
            .unwrap();
        assert_eq!(restored.as_ref(), bytes);
    }
    gateway.shutdown().await;
    task.abort();
}
