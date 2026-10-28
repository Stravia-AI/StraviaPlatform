use super::*;

#[tokio::test]
async fn master_only_injection_can_download_owned_artifacts_without_media_or_networking() {
    for enabled in [true, false] {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: data_dir.path().to_owned(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let admin = gateway.admin();
        admin
            .set_setting(
                "artifact_settings",
                r#"{"client_base_url":"http://localhost/client"}"#,
            )
            .await
            .expect("download settings");
        let key = admin
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "Basic reader".into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: enabled,
                inject_web_search: false,
                inject_media_generation: false,
                inject_media_understanding: false,
                model_ids: vec![],
                spend_limit: None,
            })
            .await
            .expect("API key");
        let artifact = gateway
            .artifact_store()
            .expect("Artifact store")
            .ingest(
                &stravia_runtime_contract::Principal::new(key.id.clone()),
                "text/plain",
                Some(5),
                stravia_runtime_contract::artifact::bytes_stream(bytes::Bytes::from_static(
                    b"hello",
                )),
                std::time::Duration::from_secs(3600),
            )
            .await
            .expect("owned Artifact");
        let mut first = openai_response("");
        first["choices"][0]["message"]["tool_calls"] = serde_json::json!([{
            "id":"owned-download",
            "type":"function",
            "function":{
                "name":"StraviaRead",
                "arguments":serde_json::json!({"path":format!("{}?download=1", artifact.reference())}).to_string()
            }
        }]);
        first["choices"][0]["finish_reason"] = serde_json::json!("tool_calls");
        let responses = if enabled {
            vec![first, openai_response("Download is available.")]
        } else {
            vec![openai_response("No platform reader was offered.")]
        };
        let (url, _, requests) = serve_openai_sequence_with_requests(responses).await;
        let route_id = configure_route_with_id(&gateway, "basic-reader", &[url]).await;
        admin
            .update_api_key(
                &key.id,
                serde_json::from_value(serde_json::json!({"model_ids":[route_id]})).unwrap(),
            )
            .await
            .expect("bind model");
        let mut user =
            stravia_runtime_contract::protocol::ir::AiItem::output_text("Download my file.");
        user.role = stravia_runtime_contract::protocol::ir::Role::User;
        let response = execute_non_stream_request_with_headers(
            gateway.clone(),
            bearer_headers(&key.token),
            AiRequest::new("basic-reader", vec![user]),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        {
            let requests = requests.lock();
            let first: serde_json::Value =
                serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
            let tools = first["tools"].as_array();
            assert_eq!(
                tools.is_some_and(|tools| tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "StraviaRead")),
                enabled,
                "master switch controls the basic reader"
            );
            if enabled {
                let followup: serde_json::Value =
                    serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
                let result = followup["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["role"] == "tool")
                    .expect("download result");
                let result: serde_json::Value =
                    serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
                assert_eq!(result["path"], artifact.reference());
                assert!(
                    result["download_url"]
                        .as_str()
                        .expect("download URL")
                        .starts_with("http://localhost/client/v1/artifacts/downloads/")
                );
            }
        }
        close_test_gateway(gateway, data_dir).await;
    }
}

#[tokio::test]
async fn media_only_injection_rejects_guessed_search_before_research_execution() {
    let mut tool_round = openai_response("");
    tool_round["choices"][0]["message"]["tool_calls"] = serde_json::json!([{
        "id": "guessed-search",
        "type": "function",
        "function": {"name": "StraviaRead", "arguments": "{\"path\":\"search://unexposed%20networking\"}"}
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
        serde_json::json!({"id":"vision", "reasoning_efforts":["medium"], "modalities":{"input":["text","image"],"output":["text"]}}),
    ).await;
    let admin = gateway.admin();
    let parent = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "read-scope-parent".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: parent_provider.id,
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
        .expect("parent Model");
    let search_model = configure_route_with_id(&gateway, "read-scope-search", &[search_url]).await;
    admin
        .update_media_understanding_config(
            stravia_media::MediaKind::Image,
            stravia_media::admin::MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: Some(parent.id.clone().into()),
                thinking_level: Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium),
            },
        )
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
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: true,
            model_ids: vec![parent.id.into()],
            spend_limit: None,
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
    let requests = requests.lock();
    let followup: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").expect("HTTP body").1)
            .expect("provider request JSON");
    assert!(followup["messages"].as_array().expect("messages").iter()
        .any(|message| message["role"] == "tool" && message["tool_call_id"] == "guessed-search"),
        "guessed platform call must return a tool result without executing research or being delegated to the client");
}

#[tokio::test]
async fn non_vision_parent_uses_capability_owned_media_model() {
    let source_id = Arc::new(parking_lot::Mutex::new(None));
    let (parent_url, parent_calls, parent_requests) = serve_media_parent(source_id.clone()).await;
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
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: parent_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "parent-key".into(),
            },
            vendor_options: Default::default(),
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
                template_id: None,
            },
        )
        .await
        .expect("parent Provider Model");
    let parent_model = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "text-parent".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: parent_provider.id,
                model: "parent".into(),
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
        .expect("parent Model");
    let media_provider = admin
        .create_provider(CreateProvider {
            name: Some("Visual Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("openai-compatible".into()),
                base_url: media_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "media-key".into(),
            },
            vendor_options: Default::default(),
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
                    "reasoning_efforts": ["medium"],
                    "modalities": {"input": ["text", "image"], "output": ["text"]}
                }),
                template_id: None,
            },
        )
        .await
        .expect("Media Provider Model");
    let media_model = admin
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "media-vision".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: media_provider.id,
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
        .expect("Media Model");
    admin
        .update_media_understanding_config(
            stravia_media::MediaKind::Image,
            stravia_media::admin::MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: Some(media_model.id.into()),
                thinking_level: Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium),
            },
        )
        .await
        .expect("enable Media Understanding");
    let api_key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Media caller".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: true,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![parent_model.id.into()],
            inject_media_understanding: true,
            spend_limit: None,
        })
        .await
        .expect("API key");
    let repeated_image = || {
        stravia_runtime_contract::protocol::ir::ContentBlock::Image {
        source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 {
            media_type: "image/png".into(),
            data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".into(),
        },
        detail: None,
        cache_control: None,
    }
    };
    let mut request = AiRequest::new(
        "text-parent",
        vec![
            stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                    stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                        text: "Image prose should remain ordinary text.".to_owned().into(),
                        cache_control: None,
                    },
                    repeated_image(),
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
            stravia_runtime_contract::protocol::ir::AiItem {
                role: stravia_runtime_contract::protocol::ir::Role::User,
                content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                    stravia_runtime_contract::protocol::ir::ContentBlock::Text {
                        text: "What is in this image again?".to_owned().into(),
                        cache_control: None,
                    },
                    repeated_image(),
                ]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            },
        ],
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
    let first_parent_request = parent_requests
        .lock()
        .first()
        .cloned()
        .expect("first parent provider request");
    assert!(
        first_parent_request.contains("Image 1: [stravia://artifacts/"),
        "bridge must preserve first image position and full Artifact URI: {first_parent_request}"
    );
    assert!(
        first_parent_request.contains("Image 2: [stravia://artifacts/"),
        "bridge must preserve repeated image position and full Artifact URI: {first_parent_request}"
    );
    assert!(
        first_parent_request.contains("Image prose should remain ordinary text."),
        "ordinary text beginning with Image must not become a marker: {first_parent_request}"
    );
    assert!(
        !first_parent_request.contains("[sm:"),
        "legacy media markers must be gone"
    );
    let prose = first_parent_request
        .find("Image prose should remain ordinary text.")
        .expect("ordinary Image prose");
    let first_marker = first_parent_request
        .find("Image 1: [stravia://artifacts/")
        .expect("first Image marker");
    let second_text = first_parent_request
        .find("What is in this image again?")
        .expect("second user message");
    let second_marker = first_parent_request
        .find("Image 2: [stravia://artifacts/")
        .expect("second Image marker");
    assert!(
        prose < first_marker && first_marker < second_text && second_text < second_marker,
        "provider content must preserve message/block order: {first_parent_request}"
    );
    let source_marker_id =
        marker_artifact_id(&first_parent_request).expect("bridge Artifact marker");
    assert!(
        first_parent_request
            .matches(&format!("[stravia://artifacts/{source_marker_id}]"))
            .count()
            >= 2,
        "repeated source must preserve one stable Artifact identity: {first_parent_request}"
    );
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
    let mut prior_user = request.items[0].clone();
    prior_user.content = stravia_runtime_contract::protocol::ir::MessageContent::Text(
        std::sync::Arc::new("Image prose should remain ordinary text.".into()),
    );
    let mut second_user =
        stravia_runtime_contract::protocol::ir::AiItem::output_text("What is its subject?");
    second_user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let second_request = AiRequest::new(
        "text-parent",
        vec![
            prior_user,
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
async fn mixed_media_route_prefers_eligible_native_then_bridges_after_output_drift() {
    let (native_url, native_calls, native_requests) =
        serve_openai_sequence_with_requests(vec![openai_response("native vision")]).await;
    let (bridge_url, bridge_calls, bridge_requests) =
        serve_openai_sequence_with_requests(vec![openai_response("bridge after output drift")])
            .await;
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
            "attachment": true,
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
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "mixed-media".into(),
            display_name: None,
            balance: Some("traffic_equalization".into()),
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
                    provider_id: native.id.clone(),
                    model: "native".into(),
                    enabled: true,
                    priority: Some(2),
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                },
            ],
            default_thinking_level: None,
            billing: None,
        })
        .await
        .expect("mixed Media Model");
    let unsupported_model = gateway
        .admin()
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "unsupported-media".into(),
            display_name: None,
            balance: None,
            targets: vec![crate::db::models::CreateTarget {
                provider_id: no_tools.id,
                model: "unsupported".into(),
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
        .expect("unsupported Media Model");
    gateway
        .admin()
        .create_manual_provider_model(
            &native.id,
            "media-reader",
            crate::provider_models::CreateManualProviderModel {
                metadata: serde_json::json!({
                    "id": "media-reader",
                    "reasoning_efforts": ["medium"],
                    "modalities": {"input": ["text", "image"], "output": ["text"]}
                }),
                template_id: None,
            },
        )
        .await
        .expect("independent Media Understanding Provider Model");
    let media_model = gateway
        .admin()
        .create_model(CreateRoute {
            output_modality: crate::db::models::RouteOutputModality::Text,
            model_id: "drift-media-reader".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: native.id.clone(),
                model: "media-reader".into(),
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
        .expect("independent Media Understanding Route");
    gateway
        .admin()
        .update_media_understanding_config(
            stravia_media::MediaKind::Image,
            stravia_media::admin::MediaUnderstandingConfigUpdate {
                enabled: true,
                model_id: Some(media_model.id.into()),
                thinking_level: Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium),
            },
        )
        .await
        .expect("enable Media Understanding for bridge");
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "Native Media caller".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: true,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![mixed_model.id.into(), unsupported_model.id.into()],
            inject_media_understanding: true,
            spend_limit: None,
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
        .first()
        .cloned()
        .expect("native Provider request");
    assert!(native_request.contains(image_data), "{native_request}");
    assert!(
        !native_request.contains("stravia_media"),
        "{native_request}"
    );

    let detail = gateway
        .admin()
        .get_provider_model(&native.id, "native")
        .await
        .expect("native Provider Model before drift");
    gateway
        .admin()
        .update_provider_model(
            &native.id,
            "native",
            crate::provider_models::UpdateProviderModel {
                revision: detail.revision,
                changes: std::collections::BTreeMap::from([
                    (
                        "modalities.input".to_string(),
                        serde_json::json!(["text", "image"]),
                    ),
                    (
                        "modalities.output".to_string(),
                        serde_json::json!(["image"]),
                    ),
                ]),
            },
        )
        .await
        .expect("native Target loses text output");
    let bridge_response = execute_non_stream_request_with_headers(
        gateway.clone(),
        headers.clone(),
        image_request("mixed-media"),
    )
    .await;
    let status = bridge_response.status();
    let body = to_bytes(bridge_response.into_body(), usize::MAX)
        .await
        .expect("bridge response body");
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        native_calls.load(Ordering::SeqCst),
        1,
        "output-ineligible native Target must not receive another request"
    );
    assert_eq!(bridge_calls.load(Ordering::SeqCst), 1);
    let bridge_request = bridge_requests
        .lock()
        .first()
        .cloned()
        .expect("eligible bridge Provider request");
    assert!(
        bridge_request.contains("Image 1: [stravia://artifacts/"),
        "bridge must preserve the image through an Artifact marker: {bridge_request}"
    );

    gateway
        .admin()
        .update_media_understanding_config(
            stravia_media::MediaKind::Image,
            stravia_media::admin::MediaUnderstandingConfigUpdate {
                enabled: false,
                model_id: None,
                thinking_level: None,
            },
        )
        .await
        .expect("restore unavailable bridge for unsupported Target assertion");
    let unsupported_response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        image_request("unsupported-media"),
    )
    .await;
    assert_eq!(unsupported_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(no_tools_calls.load(Ordering::SeqCst), 0);
}

fn attachment_request(model: &str, data: &str) -> AiRequest {
    AiRequest::new(
        model,
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Blocks(vec![
                stravia_runtime_contract::protocol::ir::ContentBlock::Image {
                    source: stravia_runtime_contract::protocol::ir::MediaSource::Base64 {
                        media_type: "image/png".into(),
                        data: data.into(),
                    },
                    detail: None,
                    cache_control: None,
                },
            ]),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    )
}

#[tokio::test]
async fn historical_route_with_image_input_requires_reconfiguration_before_media_planning() {
    let (base_url, calls) =
        serve_openai_sequence(vec![openai_response("must not receive historical request")]).await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let route_id = configure_route_with_id(&gateway, "historical-media", &[base_url]).await;
    let route = gateway
        .storage
        .routes()
        .list()
        .await
        .expect("Routes")
        .into_iter()
        .find(|route| route.id == route_id)
        .expect("configured text-only Route");
    gateway
        .storage
        .routes()
        .put(crate::db::models::PutRoute {
            output_modality: None,
            id: Some(route.id),
            model_id: route.model_id,
            display_name: route.display_name,
            selection_strategy: route.balance,
            is_enabled: route.is_enabled,
            targets: Some(
                route
                    .targets
                    .iter()
                    .map(|target| CreateTarget {
                        provider_id: target.provider_id().clone().into(),
                        model: target.model().clone().into(),
                        enabled: target.enabled,
                        priority: Some(target.priority),
                        first_token_timeout_ms: Some(target.first_token_timeout_ms),
                        target_retry_budget: Some(target.target_retry_budget),
                        target_cooldown_ms: Some(target.target_cooldown_ms),
                        thinking_level_map: target.thinking_level_map.clone(),
                    })
                    .collect(),
            ),
            default_thinking_level: None,
            billing: None,
        })
        .await
        .expect("seed historical unconfigured Route");
    gateway
        .model_cache
        .write()
        .await
        .reload(gateway.storage.routes())
        .await
        .expect("reload historical Route");
    let headers = authorized_headers(&gateway).await;
    let response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        attachment_request(
            "historical-media",
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
        ),
    )
    .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("historical Route rejection body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON rejection");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["error"]["code"], "route_output_modality_unconfigured",
        "{body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("historical-media"),
        "{body}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_attachment_remains_a_client_error() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_id(
        &gateway,
        "invalid-attachment",
        &["http://127.0.0.1:9/v1".into()],
    )
    .await;
    let headers = authorized_headers(&gateway).await;

    let response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        attachment_request("invalid-attachment", "not base64"),
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("attachment rejection body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON rejection");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "attachment_ingest_failed");
}

#[tokio::test]
async fn attachment_storage_failure_is_a_sanitized_server_error() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    configure_route_with_id(
        &gateway,
        "unavailable-attachment-storage",
        &["http://127.0.0.1:9/v1".into()],
    )
    .await;
    let headers = authorized_headers(&gateway).await;
    sqlx::query("ALTER TABLE artifacts RENAME TO unavailable_artifacts")
        .execute(gateway._sqlite_pool.as_ref().expect("Gateway SQLite pool"))
        .await
        .expect("make isolated Artifact storage unavailable");

    let response = execute_non_stream_request_with_headers(
        gateway,
        headers,
        attachment_request("unavailable-attachment-storage", "aGVsbG8="),
    )
    .await;
    let status = response.status();
    let diagnostic = response
        .extensions()
        .get::<crate::interaction_observation::FailureDiagnostic>()
        .cloned()
        .expect("attachment diagnostic");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("attachment rejection body");
    let body_text = String::from_utf8_lossy(&body);
    let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON rejection");

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "attachment_ingest_failed");
    assert!(!body_text.to_ascii_lowercase().contains("database"));
    assert!(!body_text.to_ascii_lowercase().contains("sqlite"));
    assert!(!body_text.to_ascii_lowercase().contains("no such table"));
    assert_eq!(diagnostic.status_code, Some(500));
    assert_eq!(diagnostic.code.as_deref(), Some("attachment_ingest_failed"));
    assert!(
        diagnostic
            .message
            .as_deref()
            .is_some_and(|message| message.starts_with("Artifact storage failed: "))
    );
}
