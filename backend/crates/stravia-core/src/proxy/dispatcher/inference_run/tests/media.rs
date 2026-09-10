use super::*;

#[tokio::test]
async fn media_only_injection_rejects_guessed_search_before_research_execution() {
    let mut tool_round = openai_response("");
    tool_round["choices"][0]["message"]["tool_calls"] = serde_json::json!([{
        "id": "guessed-search",
        "type": "function",
        "function": {"name": "StraviaRead", "arguments": "{\"url\":\"query://unexposed%20networking\"}"}
    }]);
    tool_round["choices"][0]["finish_reason"] = serde_json::json!("tool_calls");
    let (parent_url, parent_calls, requests) = serve_openai_sequence_with_requests(vec![
        tool_round,
        openai_response("The requested search capability is unavailable in this run."),
    ])
    .await;
    let (search_url, search_calls) =
        serve_openai_sequence(vec![openai_response("unexpected research")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let parent_provider = create_test_provider_with_model(
        &gateway, "Read scope parent", parent_url, "vision",
        serde_json::json!({"id":"vision", "tool_call":true, "modalities":{"input":["text","image"],"output":["text"]}}),
    ).await;
    let admin = gateway.admin();
    let parent = admin
        .create_model(CreateRoute {
            model_id: "read-scope-parent".into(),
            display_name: None,
            balance: None,
            target_provider: parent_provider.id,
            target_model: "vision".into(),
            targets: vec![],
        })
        .await
        .expect("parent Model");
    let search_model = configure_route_with_id(&gateway, "read-scope-search", &[search_url]).await;
    admin
        .update_media_understanding_config(stravia_media::admin::MediaUnderstandingConfigUpdate {
            enabled: true,
            model_id: Some(parent.id.clone()),
            thinking_level: Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium),
        })
        .await
        .expect("enable Media Understanding");
    let source = admin
        .create_web_provider(crate::db::models::CreateWebProvider {
            name: "Unused search source".into(),
            kind: "exa".into(),
            api_key: Some("unused-test-key".into()),
            use_proxy: false,
            local_engines: None,
        })
        .await
        .expect("configure search source");
    admin
        .update_web_access_settings(crate::db::models::WebAccessSettings {
            search_provider_ids: vec![source.id.clone()],
            fetch_provider_ids: vec![source.id],
        })
        .await
        .expect("configure search sources");
    let search_config = admin
        .get_web_search_config()
        .await
        .expect("search configuration");
    admin
        .update_web_search_config(stravia_web_search::WebSearchConfig {
            enabled: true,
            backend: Some(stravia_web_search::WebSearchBackendDraft::Local {
                model_id: Some(search_model),
            }),
            ..search_config.config
        })
        .await
        .expect("enable Networking globally");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Media-only reader".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            inject_web_search: false,
            inject_media_understanding: true,
            model_ids: vec![parent.id],
        })
        .await
        .expect("media-only API key");
    let mut user = stravia_runtime_contract::protocol::ir::AiItem::output_text(
        "Read the available media if needed.",
    );
    user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let request = AiRequest::new("read-scope-parent", vec![user]);
    let response =
        execute_non_stream_request_with_headers(gateway, bearer_headers(&key.token), request).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(parent_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        search_calls.load(Ordering::SeqCst),
        0,
        "guessed networking must not start research despite global availability"
    );
    let requests = requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let followup: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").expect("HTTP body").1)
            .expect("provider request JSON");
    assert!(followup["messages"].as_array().expect("messages").iter()
        .any(|message| message["role"] == "tool" && message["tool_call_id"] == "guessed-search"),
        "guessed platform call must return a tool result without executing research or being delegated to the client");
}

#[tokio::test]
async fn non_vision_parent_uses_capability_owned_media_model() {
    let source_id = Arc::new(std::sync::Mutex::new(None));
    let (parent_url, parent_calls) = serve_media_parent(source_id.clone()).await;
    let (media_url, media_calls) = serve_media_model(source_id).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(ClearHiddenMediaPlanHook))
    .build()
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
        .create_model(CreateRoute {
            model_id: "text-parent".into(),
            display_name: None,
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
        .create_model(CreateRoute {
            model_id: "media-vision".into(),
            display_name: None,
            balance: None,
            target_provider: media_provider.id,
            target_model: "vision".into(),
            targets: vec![],
        })
        .await
        .expect("Media Model");
    admin
        .update_media_understanding_config(stravia_media::admin::MediaUnderstandingConfigUpdate {
            enabled: true,
            model_id: Some(media_model.id),
            thinking_level: Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium),
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
            vec![stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                    stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                        text: "What is in this image?".into(),
                        cache_control: None,
                    },
                    stravia_runtime_contract::protocol::ir::ContentBlock::Image {
                        source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 {
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

    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bridge response body");
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
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
    let first_assistant_reasoning = first_response["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .expect("first assistant reasoning")
        .to_owned();
    let mut second_user =
        stravia_runtime_contract::protocol::ir::AiItem::output_text("What is its subject?");
    second_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let second_request = AiRequest::new(
        "text-parent",
        vec![
            request.items[0].clone(),
            stravia_runtime_contract::protocol::ir::AiItem::thinking(
                first_assistant_reasoning,
                None,
            ),
            stravia_runtime_contract::protocol::ir::AiItem::output_text(first_assistant),
            second_user,
        ],
    );
    let second_response =
        execute_non_stream_request_with_headers(gateway, headers, second_request).await;
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
}

struct ClearHiddenMediaPlanHook;

struct ClearHiddenMediaPlanSession;

impl stravia_runtime_contract::hook::Hook for ClearHiddenMediaPlanHook {
    fn descriptor(&self) -> stravia_runtime_contract::hook::HookDescriptor {
        stravia_runtime_contract::hook::HookDescriptor {
            event_kinds: vec![stravia_runtime_contract::hook::EventKind::Request],
            ..stravia_runtime_contract::hook::HookDescriptor::all("clear-hidden-media-plan")
        }
    }

    fn create_session(
        &self,
        _context: &stravia_runtime_contract::hook::SessionContext,
    ) -> Box<dyn stravia_runtime_contract::hook::HookSession> {
        Box::new(ClearHiddenMediaPlanSession)
    }
}

#[async_trait]
impl stravia_runtime_contract::hook::HookSession for ClearHiddenMediaPlanSession {
    async fn handle(
        &mut self,
        event: stravia_runtime_contract::hook::HookEvent<'_>,
    ) -> Result<stravia_runtime_contract::hook::ActionBatch, String> {
        let stravia_runtime_contract::hook::HookEvent::Request { current, round, .. } = event
        else {
            return Ok(stravia_runtime_contract::hook::ActionBatch::default());
        };
        if round == 0 {
            return Ok(stravia_runtime_contract::hook::ActionBatch::default());
        }
        let mut replacement = current.clone();
        replacement.meta.media_routing = None;
        Ok(stravia_runtime_contract::hook::ActionBatch::one(
            stravia_runtime_contract::hook::HookAction::PatchRequest(Box::new(
                stravia_runtime_contract::hook::RequestPatch::ReplaceCanonical(Box::new(
                    replacement,
                )),
            )),
        ))
    }
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
    let gateway = Gateway::new(crate::config::GatewayConfig {
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
        .create_model(CreateRoute {
            model_id: "mixed-media".into(),
            display_name: None,
            balance: Some("traffic_equalization".into()),
            target_provider: String::new(),
            target_model: String::new(),
            targets: vec![
                CreateTarget {
                    provider_id: bridge.id,
                    model: "bridge".into(),
                    enabled: true,
                    priority: Some(1),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
                CreateTarget {
                    provider_id: native.id,
                    model: "native".into(),
                    enabled: true,
                    priority: Some(2),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
            ],
        })
        .await
        .expect("mixed Media Model");
    let unsupported_model = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "unsupported-media".into(),
            display_name: None,
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
            vec![stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                    stravia_runtime_contract::protocol::ir::ContentBlock::Image {
                        source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 {
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

    let unsupported_response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        image_request("unsupported-media"),
    )
    .await;
    assert_eq!(unsupported_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(no_tools_calls.load(Ordering::SeqCst), 0);
}
