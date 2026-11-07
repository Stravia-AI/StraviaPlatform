use std::future::Future;
use std::time::Duration;

use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use futures::{SinkExt, StreamExt};
use reqwest_websocket::Upgrade as _;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::Gateway;
use crate::config::GatewayConfig;
use crate::db::models::{
    CreateApiKey, CreateProvider, CreateRoute, ProviderCredentialInput, ProviderSourceInput,
};
use crate::interaction_observation::ForestQuery;
use crate::provider_models::CreateManualProviderModel;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;

struct ProviderRequest {
    body: Value,
    respond: oneshot::Sender<Value>,
}

async fn responses(
    State(requests): State<mpsc::Sender<ProviderRequest>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let (respond, response) = oneshot::channel();
    requests
        .send(ProviderRequest { body, respond })
        .await
        .unwrap();
    let response = response.await.expect("test supplies Provider response");
    if response.get("error").is_some_and(Value::is_object) {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(response)).into_response();
    }
    let mut created = response.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["completed_at"] = Value::Null;
    created["usage"] = Value::Null;
    let mut frames = format!(
        "event: response.created\ndata: {}\n\n",
        json!({"type":"response.created","response":created})
    );
    for (index, item) in response["output"].as_array().unwrap().iter().enumerate() {
        frames.push_str(&format!(
            "event: response.output_item.added\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":index,"item":item})
        ));
        frames.push_str(&format!(
            "event: response.output_item.done\ndata: {}\n\n",
            json!({"type":"response.output_item.done","output_index":index,"item":item})
        ));
    }
    frames.push_str(&format!(
        "event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"type":"response.completed","response":response})
    ));
    ([("content-type", "text/event-stream")], frames).into_response()
}

fn completed(id: &str, output: Value) -> Value {
    stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
        id,
        "upstream-model",
        "completed",
        output.as_array().unwrap().clone(),
        Value::Null,
        Value::Null,
        json!({"input_tokens":32,"output_tokens":4,"total_tokens":36,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}),
    )
}

// The test owns both ends of this channel: the Provider cannot send native state
// until the HTTP client has actually observed the preceding public text.
struct PublicationRequest {
    body: Value,
    frames: mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
}

async fn publication_responses(
    State(requests): State<mpsc::Sender<PublicationRequest>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let (frames, receiver) = mpsc::channel(8);
    requests
        .send(PublicationRequest { body, frames })
        .await
        .unwrap();
    let stream = futures::stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|frame| (frame, receiver))
    });
    (
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
}

fn v2_compaction_events(id: &str, state: &Value) -> String {
    let mut resource = completed(id, json!([]));
    resource["status"] = "in_progress".into();
    resource["completed_at"] = Value::Null;
    resource["usage"] = Value::Null;
    let created = json!({
        "type": "response.created",
        "response": resource
    });
    let mut stream = format!("event: response.created\ndata: {created}\n\n");
    for event in [
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"compaction","id":state["id"]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":state}),
        json!({"type":"response.completed","response":completed(id, json!([state]))}),
    ] {
        stream.push_str(&format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        ));
    }
    stream
}

async fn v2_compaction_responses(
    State(requests): State<mpsc::Sender<ProviderRequest>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let is_standalone = body["input"]
        .as_array()
        .and_then(|items| items.last())
        .is_some_and(|item| item["type"] == "compaction_trigger");
    if !is_standalone {
        return responses(State(requests), Json(body)).await;
    }
    let (respond, response) = oneshot::channel();
    requests
        .send(ProviderRequest { body, respond })
        .await
        .unwrap();
    let response = response
        .await
        .expect("test supplies V2 compaction response");
    (
        [("content-type", "text/event-stream")],
        response.as_str().unwrap_or_default().to_owned(),
    )
        .into_response()
}

async fn publication_v2_responses(
    State((inline, compact)): State<(
        mpsc::Sender<PublicationRequest>,
        mpsc::Sender<ProviderRequest>,
    )>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let is_standalone = body["input"]
        .as_array()
        .and_then(|items| items.last())
        .is_some_and(|item| item["type"] == "compaction_trigger");
    if is_standalone {
        v2_compaction_responses(State(compact), Json(body)).await
    } else {
        publication_responses(State(inline), Json(body))
            .await
            .into_response()
    }
}

fn publication_frame(event: Value) -> Result<axum::body::Bytes, std::io::Error> {
    Ok(format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().unwrap()
    )
    .into())
}

#[tokio::test]
async fn registry_failure_gates_http_native_publication_and_standalone_compaction() {
    // Gateway initialization performs independent local fixture preparation; keep the
    // scenario watchdog focused on configuration and the actual HTTP exchanges.
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let stage = std::cell::Cell::new("provider setup");
    tokio::time::timeout(Duration::from_secs(20), async {
        let (requests, mut received) = mpsc::channel::<PublicationRequest>(1);
        let (compact_requests, mut compact_received) = mpsc::channel::<ProviderRequest>(1);
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        let provider_router = Router::new()
            .route("/v1/responses", post(publication_v2_responses).with_state((requests, compact_requests)));
        let provider_server = tokio::spawn(async move {
            axum::serve(provider_listener, provider_router).await.unwrap();
        });
        stage.set("provider and route configuration");
        let admin = gateway.admin();
        let provider = admin.create_provider(CreateProvider {
            name: Some("local publication fault Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(), channel: "default".into(),
                protocol: Some("open-responses".into()),
                base_url: format!("http://{provider_address}/v1"),
                models_source: None, static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey { value: "local-provider-key".into() },
            vendor_options: Default::default(),
            use_proxy: false,
        }).await.unwrap();
        admin.create_manual_provider_model(&provider.id, "upstream-model", CreateManualProviderModel {
            metadata: json!({"id":"upstream-model","name":"upstream-model","modalities":{"input":["text"],"output":["text"]}}),
            template_id: None,
        }).await.unwrap();
        let route = admin.create_model(CreateRoute { output_modality: crate::db::models::RouteOutputModality::Text, model_id: "native-publication".into(), display_name: None, balance: None,
        targets: vec![crate::db::models::CreateTarget {
                        provider_id: provider.id,
                        model: "upstream-model".into(),
                        enabled: true,
                        priority: None,
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    }],
            billing: None,
        default_thinking_level: None, }).await.unwrap();
        let key = admin.create_api_key(CreateApiKey { key: None, name: "local publication client".into(), rpm_limit: None, expires_at: None, mcp_access_enabled: false, transparent_injection_enabled: false,
            spend_limit: None,
        inject_web_search: false, inject_media_generation: false, inject_media_understanding: false, model_ids: vec![route.id.into()], }).await.unwrap();
        sqlx::query("CREATE TRIGGER deny_native_publication BEFORE INSERT ON native_compactions BEGIN SELECT RAISE(ABORT, 'injected registration failure'); END")
            .execute(gateway._sqlite_pool.as_ref().unwrap()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = super::create_router(gateway.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let sending = client.post(format!("http://{address}/v1/responses"))
            .bearer_auth(&key.token)
            .json(&json!({"model":"native-publication","stream":true,"input":"publish public text before native state","context_management":[{"type":"compaction","compact_threshold":128}]}));
        stage.set("inline provider request and headers");
        let mut sending = tokio::spawn(async move { sending.send().await });
        let (provider_request, early_response) = tokio::select! {
            request = received.recv() => (request.expect("inline Provider request"), None),
            response = &mut sending => {
                let response = response.unwrap().unwrap();
                assert!(response.status().is_success(), "Gateway rejected the inline request before reaching the Provider: {}", response.status());
                (received.recv().await.expect("inline Provider request"), Some(response))
            }
        };
        assert_eq!(provider_request.body["stream"], true);
        let text = "public text delivered before registration";
        let message = json!({"type":"message","id":"public-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
        let native = json!({"type":"compaction","id":"undeliverable-native-state","encrypted_content":"never-public-ciphertext"});
        let mut created = completed("publication-failure", json!([]));
        created["status"] = json!("in_progress");
        created["completed_at"] = Value::Null;
        created["usage"] = Value::Null;
        provider_request.frames.send(publication_frame(json!({"type":"response.created","response":created}))).await.unwrap();
        provider_request.frames.send(publication_frame(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"public-message","role":"assistant","status":"in_progress","content":[]}}))).await.unwrap();
        provider_request.frames.send(publication_frame(json!({"type":"response.content_part.added","output_index":0,"item_id":"public-message","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}))).await.unwrap();
        provider_request.frames.send(publication_frame(json!({"type":"response.output_text.delta","output_index":0,"item_id":"public-message","content_index":0,"delta":text}))).await.unwrap();
        stage.set("public text delivery");
        let response = match early_response {
            Some(response) => response,
            None => sending.await.unwrap().unwrap(),
        };
        assert!(response.status().is_success());
        let mut bytes = response.bytes_stream();
        let mut wire = String::new();
        while !wire.contains(text) {
            let chunk = bytes.next().await.expect("public text must arrive before Provider releases native state").unwrap();
            wire.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        let native_frames = [
            json!({"type":"response.output_text.done","output_index":0,"item_id":"public-message","content_index":0,"text":text}),
            json!({"type":"response.content_part.done","output_index":0,"item_id":"public-message","content_index":0,"part":message["content"][0]}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            json!({"type":"response.output_item.added","output_index":1,"item":native}),
            json!({"type":"response.output_item.done","output_index":1,"item":native}),
            json!({"type":"response.completed","response":completed("publication-failure", json!([message,native]))}),
        ].into_iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect::<String>();
        stage.set("inline registration failure delivery");
        provider_request.frames.send(Ok(native_frames.into())).await.unwrap();
        drop(provider_request.frames);
        while let Some(chunk) = bytes.next().await {
            wire.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        assert!(!wire.contains("never-public-ciphertext"), "unregistered resumable state leaked: {wire}");
        let events: Vec<Value> = wire.lines().filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap()).collect();
        assert!(!events.iter().any(|event| event["type"] == "response.completed"), "registration failure cannot complete successfully: {wire}");
        assert!(events.iter().any(|event| event["type"] == "error" || event["type"] == "response.failed"), "client must observe terminal failure: {wire}");

        stage.set("standalone registration failure exchange");
        let sending = client.post(format!("http://{address}/v1/responses"))
            .bearer_auth(&key.token)
            .json(&json!({"model":"native-publication","stream":true,"input":[
                {"role":"user","content":[{"type":"input_text","text":"standalone source window"}]},
                {"type":"compaction_trigger"}
            ]}))
            .send();
        let supply = async {
            let request = compact_received.recv().await.unwrap();
            assert_eq!(request.body["stream"], true);
            assert_eq!(
                request.body["input"].as_array().unwrap().last().unwrap()["type"],
                "compaction_trigger"
            );
            request.respond.send(json!(v2_compaction_events("standalone-failure", &native))).unwrap();
        };
        let response = exchange(sending, supply).await;
        let status = response.status();
        let failure = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{failure}");

        // 查询路径不再隐式等待观测 writer；显式屏障代替基于时间的轮询。
        stage.set("failed request observation flush and query");
        admin.observation_flush().await.unwrap();
        let forest = admin.observation_forest(ForestQuery::default()).await.unwrap();
        let interactions: Vec<_> = forest.roots.iter().flat_map(|root| &root.interactions).collect();
        assert_eq!(interactions.len(), 2, "both failed requests remain visible in admin");
        let mut committed_runs = 0;
        let mut failed_runs = 0;
        for interaction in interactions {
            let detail = admin.observation_interaction(&interaction.id, ForestQuery::default()).await.unwrap().unwrap();
            assert!(!detail.runs.is_empty());
            for run in detail.runs {
                assert_eq!(run.status, "failed");
                failed_runs += 1;
                committed_runs += usize::from(run.client_output_committed);
                assert!(run.generation_node_id.is_none(), "failed publication must not commit a Generation");
            }
        }
        assert_eq!(failed_runs, 2);
        assert_eq!(committed_runs, 1, "SSE public text is committed; failed unary output is not");
        // The identical real compact payload succeeds when only the database fault
        // is removed, ruling out Provider schema/routing errors as the failure cause.
        sqlx::query("DROP TRIGGER deny_native_publication")
            .execute(gateway._sqlite_pool.as_ref().unwrap()).await.unwrap();
        stage.set("standalone registration success exchange");
        let sending = client.post(format!("http://{address}/v1/responses"))
            .bearer_auth(&key.token)
            .json(&json!({"model":"native-publication","stream":true,"input":[
                {"role":"user","content":[{"type":"input_text","text":"standalone source window"}]},
                {"type":"compaction_trigger"}
            ]}))
            .send();
        let supply = async {
            let request = compact_received.recv().await.unwrap();
            assert_eq!(request.body["stream"], true);
            assert_eq!(
                request.body["input"].as_array().unwrap().last().unwrap()["type"],
                "compaction_trigger"
            );
            request.respond.send(json!(v2_compaction_events("standalone-success", &native))).unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(response.status().is_success());
        let wire = response.text().await.unwrap();
        assert!(wire.contains("response.output_item.done"));
        assert!(wire.contains("response.completed"));
        assert!(wire.contains("never-public-ciphertext"));
        let pair = stravia_protocol_codec::transform::ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24).unwrap();
        let decoded = pair.decode_request(json!({"model":"native-publication","input":[native]})).unwrap();
        assert!(gateway.compaction.resolve(&Principal::new(key.id), &decoded.items).await.unwrap().is_some());
        server.abort();
        provider_server.abort();
        stage.set("observation shutdown");
        gateway.observation.shutdown().await;
    }).await.unwrap_or_else(|_| panic!("registry publication failure scenario timed out at {}", stage.get()));
}

#[tokio::test]
async fn inbound_responses_websocket_preserves_inline_native_compaction_and_replays_current_window()
{
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let stage = std::cell::Cell::new("provider setup");
    tokio::time::timeout(Duration::from_secs(20), async {
        let (requests, mut received) = mpsc::channel::<ProviderRequest>(2);
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        let provider_server = tokio::spawn(async move {
            axum::serve(
                provider_listener,
                Router::new().route("/v1/responses", post(responses)).with_state(requests),
            )
            .await
            .unwrap();
        });
        stage.set("provider and route configuration");
        let admin = gateway.admin();
        let provider = admin.create_provider(CreateProvider {
            name: Some("local HTTP Responses provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: "custom".into(),
                channel: "default".into(),
                protocol: Some("open-responses".into()),
                base_url: format!("http://{provider_address}/v1"),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey { value: "local-provider-key".into() },
            vendor_options: Default::default(),
            use_proxy: false,
        }).await.unwrap();
        admin.create_manual_provider_model(&provider.id, "upstream-model", CreateManualProviderModel {
            metadata: json!({"id":"upstream-model","name":"upstream-model","modalities":{"input":["text"],"output":["text"]}}),
            template_id: None,
        }).await.unwrap();
        let route = admin.create_model(CreateRoute { output_modality: crate::db::models::RouteOutputModality::Text, model_id: "native-ws".into(),
        display_name: None,
        balance: None,
        targets: vec![crate::db::models::CreateTarget {
                        provider_id: provider.id,
                        model: "upstream-model".into(),
                        enabled: true,
                        priority: None,
                        first_token_timeout_ms: None,
                        target_retry_budget: None,
                        target_cooldown_ms: None,
                        thinking_level_map: Vec::new(),
                    }],
            billing: None,
        default_thinking_level: None, }).await.unwrap();
        let key = admin.create_api_key(CreateApiKey { key: None,
        name: "local ingress WebSocket".into(), rpm_limit: None, expires_at: None,
        mcp_access_enabled: false,
        transparent_injection_enabled: false,
        inject_web_search: false,
        inject_media_generation: false,
        inject_media_understanding: false,
            spend_limit: None,
        model_ids: vec![route.id.into()], }).await.unwrap();
        let principal = Principal::new(key.id.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = super::create_router(gateway.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        stage.set("WebSocket upgrade");
        let mut socket = reqwest::Client::new()
            .get(format!("http://{address}/v1/responses"))
            .bearer_auth(&key.token)
            .upgrade().send().await.unwrap()
            .into_websocket().await.unwrap();
        let removed = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"history deliberately removed after compaction"}]});
        let retained = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"retained current question"}]});
        stage.set("initial inline dispatch");
        socket.send(reqwest_websocket::Message::Text(json!({
            "type":"response.create","model":"native-ws","input":[removed.clone(),retained.clone()],
            "context_management":[{"type":"compaction","compact_threshold":128}]
        }).to_string())).await.unwrap();
        let first = tokio::select! {
            request = received.recv() => request.expect("inline Provider request"),
            event = socket.next() => panic!("Gateway answered before reaching the inline Provider: {event:?}"),
        };
        assert_eq!(first.body["input"], json!([removed, retained.clone()]));
        assert_eq!(first.body["context_management"], json!([{"type":"compaction","compact_threshold":128}]));
        assert_eq!(first.body["stream"], true);
        let native = json!({
            "type":"compaction","id":"native-state-exact",
            "encrypted_content":"opaque+/cipher==",
            "rolling_identity":{"version":7,"cursor":"preserve-me","nested":[1,{"flag":true}]}
        });
        stage.set("initial native state delivery and registration");
        first.respond.send(completed("first-native", json!([native.clone()]))).unwrap();
        let pair = stravia_protocol_codec::transform::ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24).unwrap();
        let mut exposed = false;
        let terminal = loop {
            let message = socket.next().await.expect("Gateway event").unwrap();
            let reqwest_websocket::Message::Text(text) = message else { continue };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_ne!(event["type"], "error", "{event}");
            if event["type"] == "response.output_item.done" && event["item"]["type"] == "compaction" {
                assert_eq!(event["item"], native);
                let decoded = pair.decode_request(json!({"model":"native-ws","input":[event["item"].clone()]})).unwrap();
                assert!(gateway.compaction.resolve(&principal, &decoded.items).await.unwrap().is_some(),
                    "native state must be replayable as soon as the client can observe it");
                exposed = true;
            }
            if event["type"] == "response.completed" { break event["response"].clone(); }
        };
        assert!(exposed, "native output item must reach the inbound WebSocket");
        assert_eq!(terminal["output"], json!([native.clone()]));
        let followup = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"continue with this complete current window"}]});
        stage.set("native replay dispatch");
        let current_window = json!([retained, terminal["output"][0].clone(), followup]);
        socket.send(reqwest_websocket::Message::Text(json!({
            "type":"response.create","model":"native-ws","input":current_window,
            "context_management":[]
        }).to_string())).await.unwrap();
        let second = loop {
            tokio::select! {
                request = received.recv() => break request.expect("replay provider request"),
                message = socket.next() => {
                    let message = message.expect("replay connection remains open").unwrap();
                    if let reqwest_websocket::Message::Text(text) = message {
                        let event: Value = serde_json::from_str(&text).unwrap();
                        assert_ne!(event["type"], "error", "{event}");
                        assert_ne!(event["type"], "response.failed", "{event}");
                    }
                }
            }
        };
        assert_eq!(second.body["input"], current_window,
            "replay must retain the whole supplied window without restoring removed history");
        assert!(second.body.get("context_management").is_none());
        stage.set("native replay terminal delivery");
        second.respond.send(completed("native-replayed", json!([{
            "type":"message","id":"accepted","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"native replay accepted","annotations":[]}]
        }]))).unwrap();
        loop {
            let message = socket.next().await.expect("replay event").unwrap();
            let reqwest_websocket::Message::Text(text) = message else { continue };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_ne!(event["type"], "error", "{event}");
            if event["type"] == "response.completed" {
                assert_eq!(event["response"]["output"][0]["content"][0]["text"], "native replay accepted");
                break;
            }
        }
        // 查询路径不再隐式等待观测 writer；显式屏障代替基于时间的轮询。
        stage.set("replay observation flush and query");
        admin.observation_flush().await.unwrap();
        let forest = admin.observation_forest(ForestQuery::default()).await.unwrap();
        let replay = forest.roots.iter().flat_map(|root| &root.interactions)
            .find(|interaction| interaction.context_events.iter().any(|event| event.kind == "native_compaction_associated"))
            .expect("admin forest exposes the native replay association");
        let detail = admin.observation_interaction(&replay.id, ForestQuery::default()).await.unwrap().unwrap();
        assert!(detail.runs.iter().flat_map(|run| &run.events)
            .any(|event| event.kind == "native_compaction_associated"));
        stage.set("upstream rejection dispatch");
        socket.send(reqwest_websocket::Message::Text(json!({
            "type":"response.create","model":"native-ws","input":"return the upstream error",
            "context_management":[{"type":"compaction","compact_threshold":128}]
        }).to_string())).await.unwrap();
        let rejected = tokio::select! {
            request = received.recv() => request.expect("client-requested compaction"),
            event = socket.next() => panic!("Gateway answered before reaching the compaction Provider: {event:?}"),
        };
        let upstream_error = json!({
            "type":"provider_capacity_error","code":"compaction_unavailable",
            "message":"Compaction capacity is exhausted.","param":"context_management",
            "details":{"retryable":true}
        });
        stage.set("upstream error delivery");
        rejected.respond.send(json!({"error":upstream_error})).unwrap();
        loop {
            let message = socket.next().await.expect("upstream error event").unwrap();
            let reqwest_websocket::Message::Text(text) = message else { continue };
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["type"] == "error" {
                assert_eq!(event["status"], 503);
                assert_eq!(event["error"], upstream_error);
                break;
            }
            assert_ne!(event["type"], "response.completed", "{event}");
        }
        stage.set("WebSocket close");
        socket.close(reqwest_websocket::CloseCode::Normal, None).await.unwrap();
        server.abort();
        provider_server.abort();
        stage.set("observation shutdown");
        gateway.observation.shutdown().await;
    }).await.unwrap_or_else(|_| panic!("native ingress WebSocket scenario timed out at {}", stage.get()));
}

#[tokio::test]
async fn inbound_responses_websocket_rejects_standalone_compaction_trigger_without_generation() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let stage = std::cell::Cell::new("provider and route configuration");
    tokio::time::timeout(Duration::from_secs(20), async {
        let (requests, mut received) = mpsc::channel::<ProviderRequest>(2);
        let harness = Harness::start_with_gateway(
            "open-responses",
            Router::new()
                .route("/v1/responses", post(responses).with_state(requests)),
            gateway,
            directory,
        )
        .await;
        stage.set("WebSocket upgrade");
        let mut socket = reqwest::Client::new()
            .get(harness.url("/v1/responses"))
            .bearer_auth(&harness.token)
            .upgrade()
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        let history = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"standalone source window"}]});
        let trigger = json!({"type":"compaction_trigger"});
        stage.set("standalone trigger rejections");
        for input in [
            json!([history.clone(), trigger.clone()]),
            json!([trigger.clone(), history.clone()]),
            json!([trigger.clone(), trigger.clone()]),
        ] {
            socket
                .send(reqwest_websocket::Message::Text(
                    json!({"type":"response.create","model":"cross","input":input}).to_string(),
                ))
                .await
                .unwrap();
            let message = socket.next().await.expect("rejection frame").unwrap();
            let reqwest_websocket::Message::Text(text) = message else {
                panic!("expected a JSON text error frame");
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["type"], "error", "{event}");
            assert_eq!(event["status"], 400, "{event}");
            assert_eq!(event["error"]["code"], "unsupported_feature", "{event}");
            assert_eq!(event["error"]["type"], "invalid_request", "{event}");
            assert!(
                event["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("POST /v1/responses")),
                "{event}"
            );
        }
        assert!(
            received.try_recv().is_err(),
            "standalone compaction must not be dispatched to the upstream"
        );

        // Rejections leave the connection usable for an ordinary inference.
        stage.set("ordinary inference dispatch");
        socket
            .send(reqwest_websocket::Message::Text(
                json!({"type":"response.create","model":"cross","input":"ordinary request"})
                    .to_string(),
            ))
            .await
            .unwrap();
        let ordinary = tokio::select! {
            request = received.recv() => request.expect("ordinary inference request"),
            event = socket.next() => panic!("Gateway answered before reaching the upstream: {event:?}"),
        };
        assert!(
            ordinary.body.to_string().contains("ordinary request"),
            "{}",
            ordinary.body
        );
        stage.set("ordinary inference terminal delivery");
        ordinary
            .respond
            .send(completed(
                "ordinary-completed",
                json!([{
                    "type":"message","id":"ordinary-output","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"ordinary response","annotations":[]}]
                }]),
            ))
            .unwrap();
        let terminal = loop {
            let message = socket.next().await.expect("ordinary event").unwrap();
            let reqwest_websocket::Message::Text(text) = message else {
                continue;
            };
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_ne!(event["type"], "error", "{event}");
            if event["type"] == "response.completed" {
                break event;
            }
        };
        assert_eq!(terminal["response"]["status"], "completed", "{terminal}");
        stage.set("WebSocket close");
        socket
            .close(reqwest_websocket::CloseCode::Normal, None)
            .await
            .unwrap();
        stage.set("observation shutdown");
        harness.finish().await;
    })
    .await
    .unwrap_or_else(|_| panic!("WebSocket standalone compaction rejection timed out at {}", stage.get()));
}

// ── Cross-protocol Remote Compaction (ADR-0089) ─────────────────────────────────

/// Drives a client request against the scripted upstream. Streaming success headers
/// can precede upstream dispatch, so keep supplying the upstream in that case.
async fn exchange(
    sending: impl Future<Output = reqwest::Result<reqwest::Response>>,
    supply: impl Future<Output = ()>,
) -> reqwest::Response {
    tokio::pin!(sending);
    tokio::pin!(supply);
    tokio::select! {
        response = &mut sending => {
            let response = response.unwrap();
            assert!(response.status().is_success(), "Gateway rejected the request before reaching the upstream: {}", response.status());
            supply.await;
            response
        }
        () = &mut supply => sending.await.unwrap(),
    }
}

struct SeenRequest {
    path: String,
    headers: axum::http::HeaderMap,
    body: Value,
    respond: oneshot::Sender<Value>,
}

async fn upstream_json(
    State(requests): State<mpsc::Sender<SeenRequest>>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let streaming_compaction = body["stream"] == true && body["compaction"].is_object();
    let (respond, response) = oneshot::channel();
    requests
        .send(SeenRequest {
            path: uri.path().to_owned(),
            headers,
            body,
            respond,
        })
        .await
        .unwrap();
    let message = response.await.expect("test supplies upstream response");
    if !streaming_compaction {
        return Json(message).into_response();
    }
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"] = json!({"input_tokens": 0, "output_tokens": 0});
    let mut events = vec![json!({"type":"message_start", "message":start})];
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        events.push(json!({"type":"content_block_start", "index":index, "content_block":block}));
        events.push(json!({"type":"content_block_stop", "index":index}));
    }
    events.push(json!({"type":"message_delta", "delta":{"stop_reason":message["stop_reason"], "stop_sequence":null}, "usage":message["usage"]}));
    events.push(json!({"type":"message_stop"}));
    let frames: String = events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    ([("content-type", "text/event-stream")], frames).into_response()
}

struct Harness {
    gateway: Gateway,
    address: std::net::SocketAddr,
    token: String,
    principal: Principal,
    servers: Vec<tokio::task::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Harness {
    async fn start_with_gateway(
        protocol: &str,
        upstream: Router,
        gateway: Gateway,
        directory: tempfile::TempDir,
    ) -> Self {
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        let provider_server = tokio::spawn(async move {
            axum::serve(provider_listener, upstream).await.unwrap();
        });
        let admin = gateway.admin();
        let provider = admin
            .create_provider(CreateProvider {
                name: Some("local cross-protocol compaction Provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some(protocol.into()),
                    base_url: format!("http://{provider_address}/v1"),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::ApiKey {
                    value: "local-provider-key".into(),
                },
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .unwrap();
        admin
            .create_manual_provider_model(
                &provider.id,
                "upstream-model",
                CreateManualProviderModel {
                    metadata: json!({"id":"upstream-model","name":"upstream-model","modalities":{"input":["text"],"output":["text"]}}),
                    template_id: None,
                },
            )
            .await
            .unwrap();
        let route = admin
            .create_model(CreateRoute {
                output_modality: crate::db::models::RouteOutputModality::Text,
                model_id: "cross".into(),
                display_name: None,
                balance: None,
                targets: vec![crate::db::models::CreateTarget {
                    provider_id: provider.id,
                    model: "upstream-model".into(),
                    enabled: true,
                    priority: None,
                    first_token_timeout_ms: None,
                    target_retry_budget: None,
                    target_cooldown_ms: None,
                    thinking_level_map: Vec::new(),
                }],
                billing: None,
                default_thinking_level: None,
            })
            .await
            .unwrap();
        let key = admin
            .create_api_key(CreateApiKey {
                key: None,
                name: "local cross-protocol compaction client".into(),
                rpm_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                spend_limit: None,
                inject_web_search: false,
                inject_media_generation: false,
                inject_media_understanding: false,
                model_ids: vec![route.id.into()],
            })
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = super::create_router(gateway.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            gateway,
            address,
            token: key.token,
            principal: Principal::new(key.id),
            servers: vec![provider_server, server],
            _directory: directory,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn finish(self) {
        for server in &self.servers {
            server.abort();
        }
        self.gateway.observation.shutdown().await;
    }
}

fn anthropic_compaction_message(content: &str, signature: &str) -> Value {
    json!({
        "id": "msg_upstream",
        "type": "message",
        "role": "assistant",
        "model": "upstream-model",
        "content": [{"type": "compaction", "content": content, "signature": signature}],
        "stop_reason": "compaction",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "iterations": [{"type": "compaction", "input_tokens": 1200, "output_tokens": 300}]
        }
    })
}

fn anthropic_text_message(text: &str) -> Value {
    json!({
        "id": "msg_followup",
        "type": "message",
        "role": "assistant",
        "model": "upstream-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}
    })
}

#[tokio::test]
async fn responses_client_compacts_through_an_anthropic_upstream_and_replays_the_block() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let stage = std::cell::Cell::new("provider and route configuration");
    tokio::time::timeout(Duration::from_secs(60), async {
        let (requests, mut received) = mpsc::channel::<SeenRequest>(2);
        let harness = Harness::start_with_gateway(
            "anthropic-messages",
            Router::new()
                .route("/v1/messages", post(upstream_json))
                .with_state(requests),
            gateway,
            directory,
        )
        .await;
        stage.set("initial compaction exchange");
        let client = reqwest::Client::new();
        let upstream = anthropic_compaction_message("Summary of the work", "sig-abc");
        let block = upstream["content"][0].clone();

        let sending = client
            .post(harness.url("/v1/responses"))
            .bearer_auth(&harness.token)
            .json(&json!({
                "model": "cross",
                "stream": true,
                "input": [{"role":"user","content":"summarize this long history"}, {"type": "compaction_trigger"}]
            }))
            .send();
        let supply = async {
            let seen = received.recv().await.unwrap();
            assert_eq!(seen.path, "/v1/messages");
            assert_eq!(
                seen.body["compaction"]["type"],
                "summarize"
            );
            assert!(
                seen.body["compaction"]["instructions"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "V2 compaction carries the platform summary instructions"
            );
            assert_eq!(
                seen.body["stream"], true,
                "native compaction must request an SSE envelope: {}",
                seen.body
            );
            assert!(
                seen.body["messages"]
                    .to_string()
                    .contains("summarize this long history")
            );
            assert!(
                seen.headers["anthropic-beta"]
                    .to_str()
                    .unwrap()
                    .contains("compact-2026-09-04")
            );
            assert_eq!(seen.headers["x-api-key"], "local-provider-key");
            seen.respond.send(upstream.clone()).unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let wire = response.text().await.unwrap();
        assert!(wire.contains("response.output_item.done"), "{wire}");
        assert!(wire.contains("response.completed"), "{wire}");
        let item = wire
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|event| {
                event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "compaction"
            })
            .map(|event| event["item"].clone())
            .expect("exactly one V2 compaction item");
        assert_eq!(item["type"], "compaction");
        assert_eq!(
            stravia_protocol_codec::codec::compaction::open(
                item["encrypted_content"].as_str().unwrap()
            ),
            Some(
                stravia_protocol_codec::codec::compaction::Envelope::Anthropic {
                    block: block.clone()
                }
            ),
            "the Anthropic block must travel intact inside the Responses state"
        );
        // Registered at compaction time under the same identity the client replays.
        let pair = stravia_protocol_codec::transform::ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
            .unwrap();
        let decoded = pair
            .decode_request(json!({"model": "cross", "input": [item.clone()]}))
            .unwrap();
        assert!(
            harness
                .gateway
                .compaction
                .resolve(&harness.principal, &decoded.items)
                .await
                .unwrap()
                .is_some()
        );

        // Replay on the same Target sends the original block, byte for byte.
        stage.set("replay exchange");
        let sending = client
            .post(harness.url("/v1/responses"))
            .bearer_auth(&harness.token)
            .json(&json!({
                "model": "cross",
                "input": [item, {"role": "user", "content": "next step"}]
            }))
            .send();
        let supply = async {
            let seen = received.recv().await.unwrap();
            assert_eq!(seen.body["messages"][0]["role"], "assistant");
            assert_eq!(seen.body["messages"][0]["content"], json!([block]));
            assert_eq!(
                seen.body["messages"].as_array().unwrap().last().unwrap()["content"][0]["text"],
                "next step"
            );
            assert!(
                seen.headers["anthropic-beta"]
                    .to_str()
                    .unwrap()
                    .contains("compact-2026-09-04")
            );
            seen.respond
                .send(anthropic_text_message("continuing"))
                .unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        stage.set("observation shutdown");
        harness.finish().await;
    })
    .await
    .unwrap_or_else(|_| panic!("Responses client over Anthropic upstream scenario timed out at {}", stage.get()));
}

#[tokio::test]
async fn anthropic_client_compacts_through_a_responses_upstream_and_replays_the_window() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let stage = std::cell::Cell::new("provider and route configuration");
    tokio::time::timeout(Duration::from_secs(60), async {
        let (requests, mut received) = mpsc::channel::<ProviderRequest>(2);
        let harness = Harness::start_with_gateway(
            "open-responses",
            Router::new().route("/v1/responses", post(v2_compaction_responses).with_state(requests)),
            gateway,
            directory,
        )
        .await;
        stage.set("initial compaction exchange");
        let client = reqwest::Client::new();
        let state = |id: &str, cipher: &str| {
            json!({"type": "compaction", "id": id, "encrypted_content": cipher})
        };
        let history = "summarize this long history";
        let sending = client
            .post(harness.url("/v1/messages"))
            .header("x-api-key", &harness.token)
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": "cross",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": history}],
                "compaction": {"type": "summarize"}
            }))
            .send();
        let supply = async {
            let request = received.recv().await.unwrap();
            assert_eq!(
                request.body["input"].as_array().unwrap().last().unwrap()["type"],
                "compaction_trigger"
            );
            assert!(request.body.to_string().contains(history));
            request
                .respond
                .send(json!(v2_compaction_events(
                    "cmp_up_1",
                    &state("cmp_state_1", "opaque-cipher")
                )))
                .unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(response.status().is_success(), "{}", response.text().await.unwrap());
        let message: Value = response.json().await.unwrap();
        assert_eq!(message["type"], "message");
        assert_eq!(message["stop_reason"], "compaction");
        let block = message["content"][0].clone();
        assert_eq!(block["type"], "compaction");
        let Some(stravia_protocol_codec::codec::compaction::Envelope::Responses { output }) =
            stravia_protocol_codec::codec::compaction::open(block["signature"].as_str().unwrap())
        else {
            panic!("the V2 state must travel in the block signature: {block}");
        };
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["encrypted_content"], "opaque-cipher");

        // `stream: true` still gets one whole block, framed as Anthropic SSE.
        stage.set("streaming compaction exchange");
        let sending = client
            .post(harness.url("/v1/messages"))
            .header("x-api-key", &harness.token)
            .json(&json!({
                "model": "cross",
                "max_tokens": 1024,
                "stream": true,
                "messages": [{"role": "user", "content": "summarize this other history"}],
                "compaction": {"type": "summarize"}
            }))
            .send();
        let supply = async {
            let request = received.recv().await.unwrap();
            request
                .respond
                .send(json!(v2_compaction_events(
                    "cmp_up_2",
                    &state("cmp_state_2", "opaque-cipher-2")
                )))
                .unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(response.status().is_success());
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let wire = response.text().await.unwrap();
        let order = ["message_start", "content_block_start", "content_block_stop", "message_delta", "message_stop"]
            .map(|event| wire.find(&format!("event: {event}")).unwrap_or_else(|| panic!("{event} missing: {wire}")));
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{wire}");
        assert!(wire.contains(r#""stop_reason":"compaction""#), "{wire}");
        assert!(wire.contains(r#""type":"compaction""#), "{wire}");

        // Replay: the one V2 compaction state expands back to a native Responses item.
        stage.set("replay exchange");
        let sending = client
            .post(harness.url("/v1/messages"))
            .header("x-api-key", &harness.token)
            .json(&json!({
                "model": "cross",
                "max_tokens": 256,
                "messages": [
                    {"role": "assistant", "content": [block]},
                    {"role": "user", "content": "next step"}
                ]
            }))
            .send();
        let supply = async {
            let request = received.recv().await.unwrap();
            let input = request.body["input"].as_array().unwrap();
            assert_eq!(input[0]["type"], "compaction", "{input:?}");
            assert_eq!(input[0]["encrypted_content"], "opaque-cipher");
            assert!(input.last().unwrap().to_string().contains("next step"));
            request.respond.send(completed("followup", json!([{
                "type":"message","id":"accepted","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"replay accepted","annotations":[]}]
            }]))).unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(response.status().is_success(), "{}", response.text().await.unwrap());
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["content"][0]["text"], "replay accepted");
        stage.set("observation shutdown");
        harness.finish().await;
    })
    .await
    .unwrap_or_else(|_| panic!("Anthropic client over Responses upstream scenario timed out at {}", stage.get()));
}

#[tokio::test]
async fn anthropic_client_gets_the_upstream_compaction_state_through_ir() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    let stage = std::cell::Cell::new("provider and route configuration");
    tokio::time::timeout(Duration::from_secs(60), async {
        let (requests, mut received) = mpsc::channel::<SeenRequest>(1);
        let harness = Harness::start_with_gateway(
            "anthropic-messages",
            Router::new()
                .route("/v1/messages", post(upstream_json))
                .with_state(requests),
            gateway,
            directory,
        )
        .await;
        stage.set("compaction exchange");
        let upstream = anthropic_compaction_message("Summary", "sig-same");
        let sending = reqwest::Client::new()
            .post(harness.url("/v1/messages"))
            .header("x-api-key", &harness.token)
            .header("anthropic-beta", "prompt-caching-2024-07-31")
            .json(&json!({
                "model": "cross",
                "max_tokens": 1024,
                "stop_sequences": ["END"],
                "messages": [{"role": "user", "content": "history"}],
                "compaction": {"type": "summarize", "instructions": "Only file names"}
            }))
            .send();
        let supply = async {
            let seen = received.recv().await.unwrap();
            assert_eq!(
                seen.body["compaction"],
                json!({"type": "summarize", "instructions": "Only file names"})
            );
            assert!(
                seen.body.get("stop_sequences").is_none(),
                "incompatible with compaction"
            );
            let betas = seen.headers["anthropic-beta"].to_str().unwrap().to_owned();
            assert!(betas.contains("compact-2026-09-04"), "{betas}");
            seen.respond.send(upstream.clone()).unwrap();
        };
        let response = exchange(sending, supply).await;
        assert!(response.status().is_success());
        let message: Value = response.json().await.unwrap();
        let upstream_block = &upstream["content"][0];
        assert_eq!(message["type"], "message");
        assert_eq!(message["stop_reason"], "compaction");
        assert_eq!(message["content"][0]["type"], "compaction");
        assert_eq!(
            message["content"][0]["signature"],
            upstream_block["signature"]
        );
        assert_eq!(message["content"][0]["content"], upstream_block["content"]);
        stage.set("observation shutdown");
        harness.finish().await;
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "Anthropic passthrough compaction scenario timed out at {}",
            stage.get()
        )
    });
}
