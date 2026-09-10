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
                vendor: Some("test-http".into()),
                protocol: "openai-compatible".into(),
                base_url: format!("http://{address}/v1"),
                models_source: None,
                static_models: None,
            },
            credential: crate::db::models::ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await
        .unwrap();
    admin.create_manual_provider_model(&provider.id, "vision", crate::provider_models::CreateManualProviderModel {
        metadata: serde_json::json!({"id":"vision","name":"Vision","tool_call":true,"modalities":{"input":["text","image"],"output":["text"]}}),
    }).await.unwrap();
    let route = admin
        .create_model(crate::db::models::CreateRoute {
            model_id: "vision".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "vision".into(),
            targets: vec![],
        })
        .await
        .unwrap();
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "attachment-owner".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            model_ids: vec![route.id],
            inject_media_understanding: false,
        })
        .await
        .unwrap();
    let router = crate::proxy::server::create_router(gateway.clone());
    let bytes = include_bytes!("../../tests/fixtures/media/transparent.png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let ordinary = "Do not fetch http://127.0.0.1/private or https://stravia/artifact/not-a-file; YWJjZA== is ordinary text.";
    let upload_grant = gateway
        .upload_grants
        .issue(&Principal::new(key.id.clone()))
        .unwrap();
    let body = serde_json::json!({"model":"vision","prediction":{"type":"content","content":upload_grant.key},"tools":[{"type":"function","function":{"name":"client_action","description":"ordinary description","parameters":{"type":"object","properties":{(upload_grant.key.clone()):{"type":"string","description":"ordinary-value"}}}}}],"messages":[{"role":"user","content":[{"type":"text","text":ordinary},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{encoded}")}}]}]});
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
    assert_eq!(wire[0]["prediction"]["content"], "<stravia-upload-key>");
    assert_eq!(
        wire[0]["tools"][0]["function"]["parameters"]["properties"]["<stravia-upload-key>"]["description"],
        "ordinary-value"
    );
    assert!(!wire[0].to_string().contains(&upload_grant.key));
    let contents = wire[0]["messages"]
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
    drop(wire);
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
        1,
        "required attachment failure must precede provider execution"
    );
    use futures::StreamExt;
    use stravia_runtime_contract::agent::{
        AgentBudgets, AgentDefinitionConfig, AgentDefinitionExposure, AgentDefinitionId,
        AgentDefinitionSpec, AgentEvent, AgentInput, AgentSlug, ArtifactPolicy,
    };
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
                vendor: Some("test-http".into()),
                protocol: "google-gemini".into(),
                base_url: format!("http://{address}"),
                models_source: None,
                static_models: None,
            },
            credential: crate::db::models::ProviderCredentialInput::None,
            use_proxy: false,
        })
        .await
        .unwrap();
    admin.create_manual_provider_model(&provider.id, "painter", crate::provider_models::CreateManualProviderModel { metadata: serde_json::json!({"id":"painter","name":"Painter","modalities":{"input":["text","image"],"output":["text","image"]}}) }).await.unwrap();
    let route = admin
        .create_model(crate::db::models::CreateRoute {
            model_id: "painter".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "painter".into(),
            targets: vec![],
        })
        .await
        .unwrap();
    let router = crate::proxy::server::create_router(gateway.clone());
    for method in ["generateContent", "streamGenerateContent"] {
        let key = admin
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: format!("painter-owner-{method}"),
                concurrency_limit: None,
                expires_at: None,
                mcp_access_enabled: true,
                transparent_injection_enabled: false,
                inject_web_search: false,
                model_ids: vec![route.id.clone()],
                inject_media_understanding: false,
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
        let payloads: Vec<String> =
            sqlx::query_scalar("SELECT payload FROM turn_chain_nodes WHERE principal = ?")
                .bind(Principal::new(key.id.clone()).continuation_key())
                .fetch_all(rows)
                .await
                .unwrap();
        assert!(payloads.iter().all(|payload| !payload.contains(&encoded)));
        let reference = payloads
            .iter()
            .find_map(|payload| {
                let start = payload.find("https://stravia/artifact/")?;
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
