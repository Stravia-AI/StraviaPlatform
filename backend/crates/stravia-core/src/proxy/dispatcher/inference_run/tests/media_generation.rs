use super::*;
use crate::db::models::{
    CreateApiKey, CreateProviderRecord, CreateRoute, CreateTarget, UpdateApiKey,
    UpsertOAuthCredential,
};
use crate::media_generation::{ImageGenerationConfig, MediaGenerationConfig};
use axum::response::IntoResponse;
use serde_json::{Value, json};

const PARENT_MODEL: &str = "media-generation-parent";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

struct GenerationHttpFixture {
    gateway: Gateway,
    _directory: tempfile::TempDir,
    headers: HeaderMap,
    key_id: String,
    parent_requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    generation_requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    upstream: tokio::task::JoinHandle<()>,
}

impl Drop for GenerationHttpFixture {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

fn response_resource(id: &str, output: Vec<Value>) -> Value {
    stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
        id,
        "provider-model",
        "completed",
        output,
        Value::Null,
        Value::Null,
        json!({
            "input_tokens": 1,
            "output_tokens": 1,
            "total_tokens": 2,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }),
    )
}

fn image_generation_sse() -> String {
    let image = json!({
        "type": "image_generation_call",
        "id": "ig_local",
        "status": "completed",
        "result": PNG,
        "output_format": "png"
    });
    let in_progress =
        stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
            "resp_image",
            "gpt-5.4",
            "in_progress",
            Vec::new(),
            Value::Null,
            Value::Null,
            Value::Null,
        );
    let completed =
        stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
            "resp_image",
            "gpt-5.4",
            "completed",
            vec![image.clone()],
            Value::Null,
            Value::Null,
            json!({
                "input_tokens": 10,
                "output_tokens": 20,
                "total_tokens": 30,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }),
        );
    let events = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": in_progress
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "image_generation_call",
                "id": "ig_local",
                "status": "in_progress",
                "result": null
            }
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 2,
            "output_index": 0,
            "item": image
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 3,
            "response": completed
        }),
    ];
    let mut stream = events
        .into_iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect::<String>();
    stream.push_str("data: [DONE]\n\n");
    stream
}

async fn generation_http_fixture(
    execute_generate: bool,
    generation_enabled: bool,
    transparent_injection_enabled: bool,
    inject_media_generation: bool,
) -> GenerationHttpFixture {
    let parent_requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let observed_parent = Arc::clone(&parent_requests);
    let parent = move |axum::Json(request): axum::Json<Value>| {
        let observed_parent = Arc::clone(&observed_parent);
        async move {
            let ordinal = {
                let mut requests = observed_parent.lock();
                let ordinal = requests.len();
                requests.push(request.clone());
                ordinal
            };
            if execute_generate && ordinal == 0 {
                let platform_name = request["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find_map(|tool| {
                        tool["name"]
                            .as_str()
                            .filter(|name| name.starts_with("stravia__generate"))
                    })
                    .expect("Media Generation tool in parent request");
                return axum::Json(response_resource(
                    "resp_generate_call",
                    vec![json!({
                        "type": "function_call",
                        "id": "fc_generate",
                        "call_id": "call_generate",
                        "name": platform_name,
                        "arguments": json!({
                            "type": "image",
                            "input": {
                                "prompt": "A blue square",
                                "aspect_ratio": "16:9",
                                "resolution": "4K"
                            }
                        }).to_string(),
                        "status": "completed"
                    })],
                ));
            }
            axum::Json(response_resource(
                "resp_parent_final",
                vec![json!({
                    "type": "message",
                    "id": "msg_parent_final",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": "Image created by the platform.",
                        "annotations": []
                    }]
                })],
            ))
        }
    };

    let generation_requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let observed_generation = Arc::clone(&generation_requests);
    let generation = move |axum::Json(request): axum::Json<Value>| {
        let observed_generation = Arc::clone(&observed_generation);
        async move {
            observed_generation.lock().push(request);
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                image_generation_sse(),
            )
                .into_response()
        }
    };

    let app = axum::Router::new()
        .route("/v1/responses", axum::routing::post(parent))
        .route("/responses", axum::routing::post(generation));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local media generation upstream");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("upstream address")
    );
    let upstream = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve local media generation upstream")
    });

    let directory = tempfile::tempdir().expect("temporary gateway directory");
    let gateway = Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let parent_route_id = configure_route_with_protocol(
        &gateway,
        PARENT_MODEL,
        &[format!("{base_url}/v1")],
        "protocol-open-responses",
        "open-responses",
    )
    .await;

    let generation_provider = gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Local Codex image fixture".into(),
            vendor: Some("openai-codex".into()),
            protocol: "open-responses".into(),
            base_url: base_url.clone(),
            preset_key: Some("openai".into()),
            channel: Some("codex".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            use_proxy: false,
        })
        .await
        .expect("local Codex Provider");
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            &generation_provider.id,
            UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "local-test-not-a-production-credential".into(),
                resource_url: Some(base_url),
                ..Default::default()
            },
        )
        .await
        .expect("local Codex credential");
    gateway
        .admin()
        .create_manual_provider_model(
            &generation_provider.id,
            "gpt-5.4",
            crate::provider_models::CreateManualProviderModel {
                metadata: json!({
                    "id": "gpt-5.4",
                    "name": "Local GPT",
                    "attachment": true,
                    "tool_call": true,
                    "capabilities": ["media_image"],
                    "modalities": {"input": ["text", "image"], "output": ["text"]}
                }),
            },
        )
        .await
        .expect("local Codex model");
    let generation_route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "local-image-generation".into(),
            display_name: Some("Local image generation".into()),
            balance: None,
            target_provider: generation_provider.id.clone(),
            target_model: Some("gpt-5.4".into()),
            targets: vec![CreateTarget {
                provider_id: generation_provider.id,
                model: Some("gpt-5.4".into()),
                enabled: true,
                priority: Some(0),
                first_token_timeout_ms: None,
                target_retry_budget: Some(0),
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .expect("image generation Route");
    gateway
        .admin()
        .update_media_generation_config(MediaGenerationConfig {
            enabled: generation_enabled,
            image: ImageGenerationConfig {
                route_id: Some(generation_route.model_id),
            },
        })
        .await
        .expect("Media Generation configuration");

    let key = gateway
        .admin()
        .create_api_key(CreateApiKey {
            key: None,
            name: "media-generation-http-key".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled,
            inject_web_search: false,
            inject_media_generation,
            model_ids: vec![parent_route_id],
            inject_media_understanding: false,
        })
        .await
        .expect("Media Generation HTTP API key");
    let headers = bearer_headers(&key.token);

    GenerationHttpFixture {
        gateway,
        _directory: directory,
        headers,
        key_id: key.id,
        parent_requests,
        generation_requests,
        upstream,
    }
}

async fn post_responses(fixture: &GenerationHttpFixture, body: Value) -> (StatusCode, Value) {
    let router = crate::proxy::server::create_router(fixture.gateway.clone());
    let mut request = Request::post("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("public Responses request");
    request.headers_mut().extend(fixture.headers.clone());
    let response = router
        .oneshot(request)
        .await
        .expect("public Responses response");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("public Responses body");
    let body = serde_json::from_slice(&body).unwrap_or_else(|error| {
        panic!(
            "Responses JSON ({error}): {}",
            String::from_utf8_lossy(&body)
        )
    });
    (status, body)
}

async fn set_generation_injection(
    fixture: &GenerationHttpFixture,
    transparent_injection_enabled: bool,
    inject_media_generation: bool,
) {
    fixture
        .gateway
        .admin()
        .update_api_key(
            &fixture.key_id,
            UpdateApiKey {
                key: None,
                name: None,
                concurrency_limit: None,
                is_enabled: None,
                mcp_access_enabled: None,
                transparent_injection_enabled: Some(transparent_injection_enabled),
                inject_web_search: None,
                inject_media_generation: Some(inject_media_generation),
                inject_media_understanding: None,
                expires_at: None,
                model_ids: None,
            },
        )
        .await
        .expect("update Media Generation injection flags");
}

fn explicit_generate_tool() -> Value {
    json!({
        "type": "function",
        "name": "generate",
        "description": "Generate media",
        "parameters": {
            "type": "object",
            "properties": {
                "type": {"type": "string"},
                "input": {"type": "object"}
            },
            "required": ["type", "input"]
        },
        "strict": true
    })
}

fn generated_tools(request: &Value) -> Vec<&Value> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("stravia__generate"))
        })
        .collect()
}

#[tokio::test]
async fn explicit_generate_executes_without_injection_permission_and_stays_platform_owned() {
    let fixture = generation_http_fixture(true, true, false, false).await;
    let (status, response) = post_responses(
        &fixture,
        json!({
            "model": PARENT_MODEL,
            "input": "Create a blue square.",
            "tools": [explicit_generate_tool()],
            "tool_choice": {"type":"function","name":"generate"}
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(
        response
            .to_string()
            .contains("Image created by the platform.")
    );
    assert!(
        response["output"]
            .as_array()
            .expect("Responses output")
            .iter()
            .all(|item| item["type"] != "function_call"),
        "the generate call must not be handed back to the client: {response}"
    );

    let parent_requests = fixture.parent_requests.lock();
    assert_eq!(
        parent_requests.len(),
        2,
        "model must receive the generated result and continue"
    );
    let exposed = generated_tools(&parent_requests[0]);
    assert_eq!(exposed.len(), 1, "{}", parent_requests[0]);
    let exposed = exposed[0];
    assert_eq!(parent_requests[0]["tool_choice"]["name"], exposed["name"]);
    assert_eq!(
        parent_requests[1]["tool_choice"], "auto",
        "the forced image call is fulfilled before the answer turn"
    );
    assert_eq!(exposed["strict"], false);
    let schema = jsonschema::validator_for(&exposed["parameters"]).unwrap();
    assert!(schema.is_valid(&json!({"type":"image","input":{"prompt":"A square"}})));
    assert!(schema.is_valid(&json!({"type":"image","input":{
        "prompt":"Edit image 1","aspect_ratio":"16:9","resolution":"4K",
        "reference_images":["https://example.com/image.png"]
    }})));
    assert!(!schema.is_valid(&json!({"type":"image","input":{"prompt":"A square","seed":42}})));
    let generated_result = parent_requests[1]["input"]
        .as_array()
        .and_then(|items| {
            items.iter().find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "call_generate"
            })
        })
        .and_then(|item| item["output"].as_str())
        .and_then(|output| serde_json::from_str::<Value>(output).ok())
        .expect("generated function output");
    assert!(
        generated_result["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("stravia://artifacts/")),
        "the generated Artifact result must return to the model: {}",
        parent_requests[1]
    );
    drop(parent_requests);

    let generation_requests = fixture.generation_requests.lock();
    assert_eq!(generation_requests.len(), 1);
    assert_eq!(
        generation_requests[0]["tool_choice"],
        json!({"type": "image_generation"})
    );
    assert_eq!(generation_requests[0]["tools"][0]["action"], "generate");
    assert_eq!(generation_requests[0]["tools"][0]["size"], "1536x1024");
}

#[tokio::test]
async fn disabled_generation_rejects_explicit_use_without_automatic_exposure() {
    let fixture = generation_http_fixture(false, false, true, true).await;

    let (automatic_status, automatic) = post_responses(
        &fixture,
        json!({"model": PARENT_MODEL, "input": "Answer normally."}),
    )
    .await;
    assert_eq!(automatic_status, StatusCode::OK, "{automatic}");
    {
        let requests = fixture.parent_requests.lock();
        assert_eq!(requests.len(), 1);
        assert!(
            generated_tools(&requests[0]).is_empty(),
            "a disabled platform capability must not be injected: {}",
            requests[0]
        );
    }

    let (explicit_status, explicit) = post_responses(
        &fixture,
        json!({
            "model": PARENT_MODEL,
            "input": "Try explicit generation.",
            "tools": [explicit_generate_tool()]
        }),
    )
    .await;
    assert_eq!(explicit_status, StatusCode::FORBIDDEN, "{explicit}");
    assert_eq!(
        fixture.parent_requests.lock().len(),
        1,
        "rejected explicit generation must not call the parent model"
    );
    assert!(fixture.generation_requests.lock().is_empty());
}

#[tokio::test]
async fn transparent_exposure_requires_both_switches_and_preserves_fixed_client_tools() {
    let fixture = generation_http_fixture(false, true, false, true).await;

    let (status, body) = post_responses(
        &fixture,
        json!({"model": PARENT_MODEL, "input": "Total switch is off."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    set_generation_injection(&fixture, true, false).await;
    let (status, body) = post_responses(
        &fixture,
        json!({"model": PARENT_MODEL, "input": "Capability switch is off."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    set_generation_injection(&fixture, true, true).await;
    let fixed_client_tool = json!({
        "type": "function",
        "name": "stravia__generate",
        "description": "Client-owned fixed tool",
        "parameters": {
            "type": "object",
            "properties": {"client_value": {"type": "string"}},
            "required": ["client_value"],
            "additionalProperties": false
        },
        "strict": true
    });
    let fixed_choice = json!({"type": "function", "name": "stravia__generate"});
    let (status, body) = post_responses(
        &fixture,
        json!({
            "model": PARENT_MODEL,
            "input": "Keep the client tool fixed.",
            "tools": [fixed_client_tool],
            "tool_choice": fixed_choice
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let requests = fixture.parent_requests.lock();
    assert_eq!(requests.len(), 3);
    assert!(generated_tools(&requests[0]).is_empty(), "{}", requests[0]);
    assert!(generated_tools(&requests[1]).is_empty(), "{}", requests[1]);

    let tools = requests[2]["tools"].as_array().expect("outbound tools");
    let client = tools
        .iter()
        .find(|tool| tool["name"] == "stravia__generate")
        .expect("fixed client tool");
    assert_eq!(client["description"], "Client-owned fixed tool");
    assert_eq!(client["strict"], true);
    assert_eq!(client["parameters"]["required"], json!(["client_value"]));
    assert_eq!(requests[2]["tool_choice"], fixed_choice);

    let platform = tools
        .iter()
        .find(|tool| tool["name"] == "stravia__generate_2")
        .expect("collision-free Media Generation tool");
    assert_eq!(platform["strict"], false);
    assert_eq!(platform["parameters"]["required"], json!(["type", "input"]));
}
