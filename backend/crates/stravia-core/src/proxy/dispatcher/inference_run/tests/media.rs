use super::*;

#[tokio::test]
async fn non_vision_parent_uses_capability_owned_media_model() {
    let source_id = Arc::new(std::sync::Mutex::new(None));
    let (parent_url, parent_calls) = serve_media_parent(source_id.clone()).await;
    let (media_url, media_calls) = serve_media_model(source_id).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, mut logs) = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let admin = gateway.admin();
    let parent_provider = admin
        .create_provider(CreateProvider {
            name: Some("Text Parent".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: parent_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "parent-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("parent Provider");
    admin
        .create_manual_provider_model(
            &parent_provider.id,
            "parent",
            crate::provider_models::CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "parent",
                    "tool_call": true,
                    "modalities": {"input": ["text"], "output": ["text"]}
                }),
            },
        )
        .await
        .expect("parent Provider Model");
    let parent_model = admin
        .create_model(CreateModel {
            name: "text-parent".into(),
            balance: None,
            target_provider: parent_provider.id,
            target_model: "parent".into(),
            targets: vec![],
        })
        .await
        .expect("parent Model");
    let media_provider = admin
        .create_provider(CreateProvider {
            name: Some("Visual Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: None,
                protocol: "openai-compatible".into(),
                base_url: media_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "media-key".into(),
            },
            use_proxy: false,
        })
        .await
        .expect("Media Provider");
    admin
        .create_manual_provider_model(
            &media_provider.id,
            "vision",
            crate::provider_models::CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "vision",
                    "modalities": {"input": ["text", "image"], "output": ["text"]}
                }),
            },
        )
        .await
        .expect("Media Provider Model");
    let media_model = admin
        .create_model(CreateModel {
            name: "media-vision".into(),
            balance: None,
            target_provider: media_provider.id,
            target_model: "vision".into(),
            targets: vec![],
        })
        .await
        .expect("Media Model");
    admin
        .update_media_understanding_config(crate::admin::MediaUnderstandingConfigUpdate {
            enabled: true,
            model_id: Some(media_model.id),
        })
        .await
        .expect("enable Media Understanding");
    let api_key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Media caller".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: true,
            inject_web_search: false,
            model_ids: vec![parent_model.id],
            inject_media_understanding: true,
        })
        .await
        .expect("API key");
    let mut request = AiRequest::new(
            "text-parent",
            vec![crate::protocol::ir::AiItem {
                role: crate::protocol::ir::Role::User,
                content: crate::protocol::ir::MessageContent::Blocks(vec![
                    crate::protocol::ir::ContentBlock::Text {
                        text: "What is in this image?".into(),
                        cache_control: None,
                    },
                    crate::protocol::ir::ContentBlock::Image {
                        source: crate::protocol::ir::MediaSource::Base64 {
                            media_type: "image/png".into(),
                            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".into(),
                        },
                        detail: None,
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
    request.stream.enabled = false;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", api_key.token)).expect("Bearer header"),
    );

    let response =
        execute_non_stream_request_with_headers(gateway.clone(), headers.clone(), request.clone())
            .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bridge response body");
    assert!(
        String::from_utf8_lossy(&body).contains("parent used Media Report"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let first_response: serde_json::Value =
        serde_json::from_slice(&body).expect("bridge response JSON");
    let first_assistant = first_response["choices"][0]["message"]["content"]
        .as_str()
        .expect("first assistant content")
        .to_owned();
    let trusted_media_turns = sqlx::query_scalar::<_, i64>(
        "SELECT json_array_length(json_extract(payload, '$.trusted_media_turn_ids')) \
         FROM turn_chain_nodes WHERE kind = 'response' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
    .await
    .expect("persisted trusted Media Turns");
    assert_eq!(trusted_media_turns, 1);

    let mut second_user = crate::protocol::ir::AiItem::output_text("What is its subject?");
    second_user.role = crate::protocol::ir::Role::User;
    let second_response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        AiRequest::new(
            "text-parent",
            vec![
                request.items[0].clone(),
                crate::protocol::ir::AiItem::output_text(first_assistant),
                second_user,
            ],
        ),
    )
    .await;
    let second_status = second_response.status();
    let second_body = to_bytes(second_response.into_body(), usize::MAX)
        .await
        .expect("continued bridge response body");
    assert_eq!(
        second_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&second_body)
    );
    assert!(
        String::from_utf8_lossy(&second_body).contains("parent used continued Media Report"),
        "{}",
        String::from_utf8_lossy(&second_body)
    );

    assert_eq!(parent_calls.load(Ordering::SeqCst), 4);
    assert_eq!(media_calls.load(Ordering::SeqCst), 2);
    let mut entries = Vec::new();
    for _ in 0..6 {
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), logs.recv())
            .await
            .expect("Media request log should be emitted")
            .expect("Media request log channel should remain open");
        assert!(
            entry.client_request_body.is_none()
                && entry.client_response_body.is_none()
                && entry.upstream_request_body.is_none()
                && entry.upstream_response_body.is_none(),
            "Media request payloads must remain redacted: {entry:?}"
        );
        entries.push(entry);
    }
    let parent_entries = entries
        .iter()
        .filter(|entry| entry.client_model == "text-parent")
        .collect::<Vec<_>>();
    let media_entries = entries
        .iter()
        .filter(|entry| entry.client_model == "media-vision")
        .collect::<Vec<_>>();
    assert_eq!(parent_entries.len(), 4, "{entries:#?}");
    assert_eq!(media_entries.len(), 2, "{entries:#?}");
    assert!(entries.iter().all(|entry| {
        entry.usage.prompt_tokens == 1
            && entry.usage.completion_tokens == 1
            && entry.usage.total_tokens == 2
    }));
}

#[tokio::test]
async fn mixed_media_route_prefers_native_targets_and_rejects_targets_without_tools() {
    let (native_url, native_calls, native_requests) =
        serve_openai_sequence_with_requests(vec![openai_response("native vision")]).await;
    let (bridge_url, bridge_calls) =
        serve_openai_sequence(vec![openai_response("bridge must not run")]).await;
    let (no_tools_url, no_tools_calls) =
        serve_openai_sequence(vec![openai_response("unsupported must not run")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let (gateway, _logs) = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let native = create_test_provider_with_model(
        &gateway,
        "Native Vision",
        native_url,
        "native",
        serde_json::json!({
            "id": "native",
            "tool_call": false,
            "modalities": {"input": ["text", "image"], "output": ["text"]}
        }),
    )
    .await;
    let bridge = create_test_provider_with_model(
        &gateway,
        "Tool Parent",
        bridge_url,
        "bridge",
        serde_json::json!({
            "id": "bridge",
            "tool_call": true,
            "modalities": {"input": ["text"], "output": ["text"]}
        }),
    )
    .await;
    let no_tools = create_test_provider_with_model(
        &gateway,
        "Unsupported Parent",
        no_tools_url,
        "unsupported",
        serde_json::json!({
            "id": "unsupported",
            "tool_call": false,
            "modalities": {"input": ["text"], "output": ["text"]}
        }),
    )
    .await;
    let mixed_model = gateway
        .admin()
        .create_model(CreateModel {
            name: "mixed-media".into(),
            balance: Some("priority".into()),
            target_provider: String::new(),
            target_model: String::new(),
            targets: vec![
                CreateModelBackend {
                    provider_id: bridge.id,
                    model: "bridge".into(),
                    weight: Some(100),
                    priority: Some(1),
                    thinking_level_map: Vec::new(),
                },
                CreateModelBackend {
                    provider_id: native.id,
                    model: "native".into(),
                    weight: Some(100),
                    priority: Some(2),
                    thinking_level_map: Vec::new(),
                },
            ],
        })
        .await
        .expect("mixed Media Model");
    let unsupported_model = gateway
        .admin()
        .create_model(CreateModel {
            name: "unsupported-media".into(),
            balance: None,
            target_provider: no_tools.id,
            target_model: "unsupported".into(),
            targets: vec![],
        })
        .await
        .expect("unsupported Media Model");
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Native Media caller".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![mixed_model.id, unsupported_model.id],
            inject_media_understanding: false,
        })
        .await
        .expect("Media API key");
    let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let image_request = |model: &str| {
        let mut request = AiRequest::new(
            model,
            vec![crate::protocol::ir::AiItem {
                role: crate::protocol::ir::Role::User,
                content: crate::protocol::ir::MessageContent::Blocks(vec![
                    crate::protocol::ir::ContentBlock::Image {
                        source: crate::protocol::ir::MediaSource::Base64 {
                            media_type: "image/png".into(),
                            data: image_data.into(),
                        },
                        detail: None,
                        cache_control: None,
                    },
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        request.stream.enabled = false;
        request
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.token)).expect("Bearer header"),
    );

    let native_response = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        image_request("mixed-media"),
    )
    .await;
    assert_eq!(native_response.status(), StatusCode::OK);
    assert_eq!(native_calls.load(Ordering::SeqCst), 1);
    assert_eq!(bridge_calls.load(Ordering::SeqCst), 0);
    let native_request = native_requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .first()
        .cloned()
        .expect("native Provider request");
    assert!(native_request.contains(image_data), "{native_request}");
    assert!(
        !native_request.contains("stravia_media"),
        "{native_request}"
    );
    assert!(
        !native_request.contains("stravia__understand_media"),
        "{native_request}"
    );

    let unsupported_response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        image_request("unsupported-media"),
    )
    .await;
    assert_eq!(unsupported_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(no_tools_calls.load(Ordering::SeqCst), 0);
}
