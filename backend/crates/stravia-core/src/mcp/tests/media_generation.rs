use super::*;
use crate::db::models::{CreateProviderRecord, CreateRoute, CreateTarget, UpsertOAuthCredential};
use crate::media_generation::{ImageGenerationConfig, MediaGenerationConfig};
use axum::response::IntoResponse;
use std::collections::VecDeque;
use std::time::Duration;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn response_resource(status: &str, output: Value) -> Value {
    json!({
        "id":"resp_generation","object":"response","created_at":1789862400,
        "completed_at":null,"status":status,"incomplete_details":null,"model":"gpt-5.4",
        "previous_response_id":null,"instructions":null,"output":output,"error":null,
        "tools":[],"tool_choice":"auto","truncation":"disabled","parallel_tool_calls":true,
        "text":{"format":{"type":"text"}},"top_p":1.0,"presence_penalty":0.0,"frequency_penalty":0.0,
        "top_logprobs":0,"temperature":1.0,"reasoning":null,
        "usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}},
        "max_output_tokens":null,"max_tool_calls":null,"store":false,"background":false,
        "service_tier":"default","metadata":{},"safety_identifier":null,"prompt_cache_key":null
    })
}

struct GenerationApp {
    app: TestApp,
    requests: Arc<parking_lot::Mutex<Vec<Value>>>,
    upstream: tokio::task::JoinHandle<()>,
    route_id: String,
    replies: Arc<parking_lot::Mutex<VecDeque<GenerationReply>>>,
    entered: Arc<tokio::sync::Notify>,
}

enum GenerationReply {
    Images(Vec<Value>),
    Status(axum::http::StatusCode, &'static str),
    Disconnect,
    Hold,
    WithoutUsage,
}

fn generated_item(encoded: &str) -> Value {
    json!({"type":"image_generation_call","id":"ig_1","status":"completed","result":encoded,"output_format":"png"})
}

fn generation_response(images: Vec<Value>, without_usage: bool) -> axum::response::Response {
    let mut created = response_resource("in_progress", json!([]));
    let mut completed = response_resource("completed", json!(images));
    if without_usage {
        created["usage"] = Value::Null;
        completed["usage"] = Value::Null;
    }
    let mut events =
        vec![json!({"type":"response.created","sequence_number":0,"response":created})];
    for (index, image) in images.into_iter().enumerate() {
        events.push(json!({"type":"response.output_item.added","sequence_number":events.len(),"output_index":index,"item":{"type":"image_generation_call","id":image["id"],"status":"in_progress","result":null}}));
        for phase in ["in_progress", "generating", "completed"] {
            events.push(json!({"type":format!("response.image_generation_call.{phase}"),"sequence_number":events.len(),"output_index":index,"item_id":image["id"]}));
        }
        events.push(json!({"type":"response.output_item.done","sequence_number":events.len(),"output_index":index,"item":image}));
    }
    events.push(
        json!({"type":"response.completed","sequence_number":events.len(),"response":completed}),
    );
    let body: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

impl Drop for GenerationApp {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

async fn generation_app() -> GenerationApp {
    let app = test_app().await;
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let observed = requests.clone();
    let replies = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
    let queued = replies.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let notified = entered.clone();
    let upstream = axum::Router::new().route(
        "/responses",
        axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            let observed = observed.clone();
            let queued = queued.clone();
            let notified = notified.clone();
            async move {
                observed.lock().push(request);
                notified.notify_one();
                let reply = queued.lock().pop_front();
                match reply {
                    Some(GenerationReply::Images(images)) => generation_response(images, false),
                    Some(GenerationReply::Status(status, message)) => (
                        status,
                        axum::Json(json!({"error":{"message":message,"type":"server_error"}})),
                    )
                        .into_response(),
                    Some(GenerationReply::Disconnect) => axum::response::Response::builder()
                        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from_stream(futures::stream::once(
                            async {
                                Err::<Bytes, _>(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "accepted request lost before output",
                                ))
                            },
                        )))
                        .unwrap(),
                    Some(GenerationReply::Hold) => futures::future::pending().await,
                    Some(GenerationReply::WithoutUsage) => {
                        generation_response(vec![generated_item(PNG)], true)
                    }
                    None => generation_response(vec![generated_item(PNG)], false),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let provider = app
        .gateway
        .storage
        .providers()
        .create(CreateProviderRecord {
            name: "Local Codex fixture".into(),
            vendor: Some("openai".into()),
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
        .unwrap();
    app.gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "local-test-not-a-production-credential".into(),
                resource_url: Some(base_url),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    app.gateway.admin().create_manual_provider_model(&provider.id, "gpt-5.4", crate::provider_models::CreateManualProviderModel {
        metadata: json!({"id":"gpt-5.4","name":"Local GPT","tool_call":true,"modalities":{"input":["text","image"],"output":["text"]}}),
    }).await.unwrap();
    let route = app
        .gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "image-generation".into(),
            display_name: Some("Image generation".into()),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "gpt-5.4".into(),
            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: "gpt-5.4".into(),
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
        .unwrap();
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            serde_json::from_value(json!({
                "mcp_access_enabled":true,"transparent_injection_enabled":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    app.gateway
        .admin()
        .set_setting(
            "artifact_settings",
            &json!({
                "client_base_url":app.endpoint.trim_end_matches("/mcp")
            })
            .to_string(),
        )
        .await
        .unwrap();
    app.gateway
        .admin()
        .update_media_generation_config(MediaGenerationConfig {
            enabled: true,
            image: ImageGenerationConfig {
                route_id: Some(route.model_id.clone()),
            },
        })
        .await
        .unwrap();
    GenerationApp {
        app,
        requests,
        upstream,
        route_id: route.model_id,
        replies,
        entered,
    }
}

async fn call_generate(client: &SdkClient, input: Value) -> rmcp::model::CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("generate")
                .with_arguments(input.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

async fn upload_image(app: &TestApp, bytes: Vec<u8>, mime: &str) -> String {
    let base = app.endpoint.strip_suffix("/mcp").unwrap();
    let http = reqwest::Client::new();
    let upload: Value = http
        .post(format!("{base}/v1/artifacts/uploads"))
        .bearer_auth(&app.token)
        .json(&json!({"mime_type":mime,"size":bytes.len()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let upload_id = upload["upload_id"].as_str().unwrap();
    let token = upload["upload_token"].as_str().unwrap();
    let part: Value = http
        .put(format!("{base}/v1/artifacts/uploads/{upload_id}/parts/1"))
        .bearer_auth(&app.token)
        .header("x-upload-token", token)
        .body(bytes)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let completed: Value = http
        .post(format!("{base}/v1/artifacts/uploads/{upload_id}/complete"))
        .bearer_auth(&app.token)
        .json(&json!({"upload_token":token,"parts":[part]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(completed.get("id").is_none());
    assert!(completed.get("reference").is_none());
    let path = completed["path"].as_str().expect("Artifact path");
    assert!(path.starts_with("stravia://artifacts/"));
    path.to_owned()
}

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[tokio::test]
async fn uploaded_references_keep_order_and_do_not_forward_source_identities() {
    let fixture = generation_app().await;
    let first = png_bytes(2, 3);
    let second = png_bytes(4, 5);
    let first_ref = upload_image(&fixture.app, first.clone(), "image/png").await;
    let second_ref = upload_image(&fixture.app, second.clone(), "image/png").await;
    let client = connect(&fixture.app).await;
    let result = call_generate(&client, json!({"type":"image","input":{"prompt":"Use image 1 layout and image 2 colors","reference_images":[first_ref,second_ref]}})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let requests = fixture.requests.lock();
    let images: Vec<&str> = requests[0]["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|content| content["image_url"].as_str())
        .collect();
    let expected: Vec<String> = [first, second]
        .iter()
        .map(|bytes| {
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        })
        .collect();
    assert_eq!(images, expected);
    assert!(
        requests[0]["tools"][0].get("size").is_none(),
        "omitted preferences must stay omitted"
    );
}

#[tokio::test]
async fn invalid_arguments_and_unsafe_references_never_start_generation() {
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let reference = upload_image(&fixture.app, png_bytes(2, 2), "image/png").await;
    for arguments in [
        json!({"type":"video","input":{"prompt":"hello"}}),
        json!({"type":"image","input":{"prompt":"   "}}),
        json!({"type":"image","route":"override","input":{"prompt":"hello"}}),
        json!({"type":"image","input":{"prompt":"hello","quality":"high"}}),
        json!({"type":"image","input":{"prompt":"hello","aspect_ratio":"2:1"}}),
        json!({"type":"image","input":{"prompt":"hello","resolution":null}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":vec![reference.clone();6]}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":[format!("{reference}?download=1")]}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":[format!("data:image/png;base64,{PNG}")]}}),
        json!({"type":"image","input":{"prompt":"hello","reference_images":["http://127.0.0.1/private.png"]}}),
    ] {
        let result = call_generate(&client, arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{arguments}: {result:?}");
        assert!(result.structured_content.unwrap().get("path").is_none());
    }
    assert!(fixture.requests.lock().is_empty());
}

#[tokio::test]
async fn foreign_expired_missing_and_corrupt_artifacts_fail_before_generation() {
    use stravia_runtime_contract::artifact::{ArtifactId, ArtifactSource};
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let store = fixture.app.gateway.artifact_store().unwrap();
    let foreign = store
        .ingest(
            &stravia_runtime_contract::Principal::new("another-principal"),
            "image/png",
            None,
            Box::pin(futures::stream::once(async {
                Ok(Bytes::from(png_bytes(2, 4)))
            })),
            Duration::from_secs(3600),
        )
        .await
        .unwrap();
    let invalid = upload_image(
        &fixture.app,
        b"not a decodable picture".to_vec(),
        "image/png",
    )
    .await;
    let expired = upload_image(&fixture.app, png_bytes(3, 4), "image/png").await;
    sqlx::query("UPDATE artifacts SET expires_at=0 WHERE id=?")
        .bind(ArtifactId::from_reference(&expired).unwrap().as_str())
        .execute(fixture.app.gateway._sqlite_pool.as_ref().unwrap())
        .await
        .unwrap();
    let missing = upload_image(&fixture.app, png_bytes(4, 4), "image/png").await;
    let reader = store
        .open(
            &stravia_runtime_contract::Principal::new(fixture.app.key_id.clone()),
            &ArtifactId::from_reference(&missing).unwrap(),
        )
        .await
        .unwrap();
    let ArtifactSource::LocalPath(path) = &reader.source else {
        panic!("local artifact");
    };
    tokio::fs::remove_file(path).await.unwrap();
    drop(reader);
    for reference in [foreign.reference(), invalid, expired, missing] {
        let result = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"Edit this","reference_images":[reference]}}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
    }
    assert!(fixture.requests.lock().is_empty());
}

#[tokio::test]
async fn unusable_completed_outputs_are_errors_not_regeneration_requests() {
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let mut extra = generated_item(PNG);
    extra["id"] = json!("ig_2");
    for images in [
        vec![],
        vec![generated_item("bm90LWFuLWltYWdl")],
        vec![generated_item(PNG), extra],
    ] {
        fixture
            .replies
            .lock()
            .push_back(GenerationReply::Images(images));
        let result = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"A square"}}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert!(result.structured_content.unwrap().get("path").is_none());
    }
    assert_eq!(fixture.requests.lock().len(), 3);
}

#[tokio::test]
async fn artifact_save_failure_does_not_repeat_successful_upstream_generation() {
    let fixture = generation_app().await;
    let root = fixture.app._data_dir.path().join("artifacts");
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    std::fs::write(&root, b"artifact storage is unavailable").unwrap();
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert_eq!(
        result.structured_content.unwrap()["error"]["code"],
        "artifact_storage_failed"
    );
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn route_failover_handles_reported_and_ambiguous_transport_failures() {
    for failure in [
        GenerationReply::Status(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Local transient failure",
        ),
        GenerationReply::Disconnect,
    ] {
        let fixture = generation_app().await;
        let route = fixture
            .app
            .gateway
            .admin()
            .get_model(&fixture.route_id)
            .await
            .unwrap();
        let provider = &route.targets[0].provider_id;
        fixture
            .app
            .gateway
            .admin()
            .create_manual_provider_model(
                provider,
                "gpt-5.2",
                crate::provider_models::CreateManualProviderModel {
                    metadata: json!({"id":"gpt-5.2","name":"Fallback GPT","tool_call":true}),
                },
            )
            .await
            .unwrap();
        fixture.app.gateway.admin().update_model(&fixture.route_id, serde_json::from_value(json!({
            "targets":[
                {"provider_id":provider,"model":"gpt-5.4","enabled":true,"priority":100,"target_retry_budget":0},
                {"provider_id":provider,"model":"gpt-5.2","enabled":true,"priority":0,"target_retry_budget":0}
            ]
        })).unwrap()).await.unwrap();
        fixture.replies.lock().push_back(failure);
        let client = connect(&fixture.app).await;
        let result = call_generate(
            &client,
            json!({"type":"image","input":{"prompt":"A square"}}),
        )
        .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let requests = fixture.requests.lock();
        assert_eq!(
            requests
                .iter()
                .map(|request| request["model"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["gpt-5.4", "gpt-5.2"]
        );
    }
}

#[tokio::test]
async fn cancellation_stops_an_accepted_generation_without_retry() {
    let fixture = generation_app().await;
    fixture.replies.lock().push_back(GenerationReply::Hold);
    let tool = crate::media_generation::platform::GenerateTool::new(&fixture.app.gateway);
    let token = stravia_runtime_contract::CancellationToken::new();
    let context = McpContext::new(fixture.app.key_id.clone()).for_call(
        token.clone(),
        std::time::Instant::now() + Duration::from_secs(60),
    );
    let call = tokio::spawn(async move {
        McpTool::call(
            &tool,
            json!({"type":"image","input":{"prompt":"A square"}}),
            &context,
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), fixture.entered.notified())
        .await
        .unwrap();
    token.cancel();
    let output = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .unwrap()
        .unwrap();
    assert!(output.is_error);
    assert_eq!(output.structured_content["error"]["code"], "cancelled");
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn upstream_credential_errors_do_not_echo_credentials_or_retry() {
    let fixture = generation_app().await;
    fixture.replies.lock().push_back(GenerationReply::Status(
        axum::http::StatusCode::UNAUTHORIZED,
        "Rejected local-test-not-a-production-credential",
    ));
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let output = result.structured_content.unwrap();
    assert_eq!(output["error"]["code"], "upstream_generation_failed");
    assert!(
        !output
            .to_string()
            .contains("local-test-not-a-production-credential")
    );
    assert_eq!(fixture.requests.lock().len(), 1);
}

#[tokio::test]
async fn missing_upstream_usage_remains_unknown() {
    let fixture = generation_app().await;
    fixture
        .replies
        .lock()
        .push_back(GenerationReply::WithoutUsage);
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    fixture
        .app
        .gateway
        .admin()
        .observation_flush()
        .await
        .unwrap();
    let stats = fixture
        .app
        .gateway
        .admin()
        .get_stats_overview(None)
        .await
        .unwrap();
    assert_eq!(stats.total_requests, 1);
    assert_eq!(stats.total_input_tokens, None);
    assert_eq!(stats.total_output_tokens, None);
}

#[tokio::test]
async fn generated_image_downloads_real_bytes_and_can_be_edited() {
    let fixture = generation_app().await;
    let client = connect(&fixture.app).await;
    let result = call_generate(&client, json!({"type":"image","input":{"prompt":"A blue square","aspect_ratio":"16:9","resolution":"4K"}})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let image = result.structured_content.unwrap();
    assert_eq!(image["mime_type"], "image/png");
    assert_eq!(image["media"], json!({"width":1,"height":1}));
    let reference = image["path"].as_str().unwrap();
    assert!(reference.starts_with("stravia://artifacts/"));
    assert!(image.get("artifact_reference").is_none());
    assert_eq!(
        image.as_object().unwrap().len(),
        4,
        "one stable identity, no transport secrets"
    );
    let grant = client
        .call_tool(
            CallToolRequestParams::new("StraviaRead").with_arguments(
                json!({"path":format!("{reference}?download=1")})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(grant.is_error, Some(true), "{grant:?}");
    let grant = grant.structured_content.unwrap();
    let response = reqwest::get(grant["download_url"].as_str().unwrap())
        .await
        .unwrap();
    assert!(response.status().is_success());
    let bytes = response.bytes().await.unwrap();
    assert_eq!(
        bytes.as_ref(),
        base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap()
    );
    assert_eq!(image["size"], bytes.len());
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1, 1));
    let edited = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"Edit image 1","reference_images":[reference]}}),
    )
    .await;
    assert_ne!(edited.is_error, Some(true), "{edited:?}");
    let requests = fixture.requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["tool_choice"],
        json!({"type":"image_generation"})
    );
    assert_eq!(requests[0]["tools"][0]["action"], "generate");
    assert_eq!(requests[1]["tools"][0]["action"], "edit");
    assert!(
        requests[1]
            .to_string()
            .contains(&format!("data:image/png;base64,{PNG}"))
    );
    assert!(!requests[1].to_string().contains("sa:"));
    assert!(!requests[1].to_string().contains("stravia://artifacts/"));
}

#[tokio::test]
async fn generation_gate_controls_discovery_without_injection_permission() {
    let app = test_app().await;
    app.gateway
        .admin()
        .update_api_key(
            &app.key_id,
            serde_json::from_value(json!({
                "mcp_access_enabled": true,
                "transparent_injection_enabled": false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let client = connect(&app).await;
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .all(|tool| tool.name != "generate")
    );
    app.gateway
        .storage
        .settings()
        .set(
            "media_generation_config",
            r#"{"enabled":true,"image":{"route_id":null}}"#,
        )
        .await
        .unwrap();
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .any(|tool| tool.name == "generate")
    );
    let result = client
        .call_tool(
            CallToolRequestParams::new("generate").with_arguments(
                json!({"type":"image","input":{"prompt":"A blue square"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(true),
        "missing binding cannot yield an image"
    );
    app.gateway
        .storage
        .settings()
        .set(
            "media_generation_config",
            r#"{"enabled":false,"image":{"route_id":null}}"#,
        )
        .await
        .unwrap();
    assert!(
        client
            .list_tools(None)
            .await
            .unwrap()
            .tools
            .iter()
            .all(|tool| tool.name != "generate")
    );
}

#[tokio::test]
async fn generation_reports_usage_and_preserves_raw_media_in_debug_exports() {
    use futures::StreamExt;
    use std::io::Read;
    let fixture = generation_app().await;
    let admin = fixture.app.gateway.admin();
    admin.set_observation_debug(true);
    let client = connect(&fixture.app).await;
    let result = call_generate(
        &client,
        json!({"type":"image","input":{"prompt":"A square"}}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    admin.observation_flush().await.unwrap();
    let stats = admin.get_stats_overview(None).await.unwrap();
    assert_eq!(
        (
            stats.total_requests,
            stats.total_input_tokens,
            stats.total_output_tokens
        ),
        (1, Some(10), Some(20))
    );
    let forest = admin.observation_forest(Default::default()).await.unwrap();
    let id = &forest.roots[0].interactions[0].id;
    let ticket = admin
        .issue_observation_bundle_ticket(crate::admin::BundleRequest {
            kind: crate::admin::BundleResourceKind::Interaction,
            resource_id: id.clone(),
            through_sequence: None,
        })
        .await
        .unwrap();
    let mut stream = admin
        .consume_observation_bundle_ticket(ticket.download_url.rsplit('/').next().unwrap())
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut exported = String::new();
    for index in 0..archive.len() {
        archive
            .by_index(index)
            .unwrap()
            .read_to_string(&mut exported)
            .unwrap();
    }
    assert!(
        exported.contains(PNG),
        "current wire capture keeps the generated media payload in the bundle"
    );
    assert!(!exported.contains("local-test-not-a-production-credential"));
    assert!(!exported.contains(&fixture.app.token));
    assert!(!exported.contains(&fixture.app._data_dir.path().to_string_lossy().to_string()));
}
