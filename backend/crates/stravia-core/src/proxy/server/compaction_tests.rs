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
    crate::protocol::codec::open_responses::formatter::response_resource_snapshot(
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

async fn publication_compact(
    State(requests): State<mpsc::Sender<ProviderRequest>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let (respond, response) = oneshot::channel();
    requests
        .send(ProviderRequest { body, respond })
        .await
        .unwrap();
    Json(response.await.expect("test supplies real compact response"))
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
    tokio::time::timeout(Duration::from_secs(20), async {
        let (requests, mut received) = mpsc::channel::<PublicationRequest>(1);
        let (compact_requests, mut compact_received) = mpsc::channel::<ProviderRequest>(1);
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider_listener.local_addr().unwrap();
        let provider_router = Router::new()
            .route("/v1/responses", post(publication_responses).with_state(requests))
            .route("/v1/responses/compact", post(publication_compact).with_state(compact_requests));
        let provider_server = tokio::spawn(async move {
            axum::serve(provider_listener, provider_router).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        }).await.unwrap();
        let admin = gateway.admin();
        let provider = admin.create_provider(CreateProvider {
            name: Some("local publication fault Provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: Some("custom".into()), protocol: "open-responses".into(),
                base_url: format!("http://{provider_address}/v1"),
                models_source: None, static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey { value: "local-provider-key".into() },
            use_proxy: false,
        }).await.unwrap();
        admin.create_manual_provider_model(&provider.id, "upstream-model", CreateManualProviderModel {
            metadata: json!({"id":"upstream-model","name":"upstream-model"}),
        }).await.unwrap();
        let route = admin.create_model(CreateRoute {
            model_id: "native-publication".into(), display_name: None, balance: None,
            target_provider: provider.id, target_model: "upstream-model".into(), targets: vec![],
        }).await.unwrap();
        let key = admin.create_api_key(CreateApiKey {
            key: None, name: "local publication client".into(), concurrency_limit: None,
            expires_at: None, mcp_access_enabled: false, transparent_injection_enabled: false,
            inject_web_search: false, inject_media_understanding: false, model_ids: vec![route.id],
        }).await.unwrap();
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
        let sending = tokio::spawn(async move { sending.send().await });
        let provider_request = received.recv().await.unwrap();
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
        let response = sending.await.unwrap().unwrap();
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

        let sending = client.post(format!("http://{address}/v1/responses/compact"))
            .bearer_auth(&key.token)
            .json(&json!({"model":"native-publication","input":"standalone source window"}))
            .send();
        let supply = async {
            let request = compact_received.recv().await.unwrap();
            assert!(request.body["input"].as_array().unwrap().iter().any(|item| {
                item["content"].as_array().is_some_and(|parts| parts.iter().any(|part| {
                    part["text"] == "standalone source window"
                }))
            }), "Provider must receive the standalone source text");
            request.respond.send(json!({"id":"standalone-failure","object":"response.compaction","created_at":1730000001,"output":[native],"usage":{"input_tokens":32,"output_tokens":4,"total_tokens":36,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}})).unwrap();
        };
        let (response, ()) = tokio::join!(sending, supply);
        let response = response.unwrap();
        let status = response.status();
        let failure: Value = response.json().await.unwrap();
        assert!(status.is_server_error(), "{status}: {failure}");
        assert!(failure["error"]["message"].as_str().is_some(), "standalone must return a visible error: {failure}");
        assert!(!failure.to_string().contains("never-public-ciphertext"));

        // Detail lookup is an Observation writer barrier, not timing-based polling.
        assert!(admin.observation_interaction("publication-barrier", ForestQuery::default()).await.unwrap().is_none());
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
        let sending = client.post(format!("http://{address}/v1/responses/compact"))
            .bearer_auth(&key.token)
            .json(&json!({"model":"native-publication","input":"standalone source window"}))
            .send();
        let supply = async {
            let request = compact_received.recv().await.unwrap();
            assert!(request.body["input"].as_array().unwrap().iter().any(|item| {
                item["content"].as_array().is_some_and(|parts| parts.iter().any(|part| {
                    part["text"] == "standalone source window"
                }))
            }), "Provider must receive the standalone source text");
            request.respond.send(json!({"id":"standalone-failure","object":"response.compaction","created_at":1730000001,"output":[native],"usage":{"input_tokens":32,"output_tokens":4,"total_tokens":36,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}})).unwrap();
        };
        let (response, ()) = tokio::join!(sending, supply);
        let response = response.unwrap();
        assert!(response.status().is_success());
        let delivered: Value = response.json().await.unwrap();
        assert_eq!(delivered["output"], json!([native]));
        let pair = crate::protocol::transform::ProtocolTransform::global()
            .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24).unwrap();
        let decoded = pair.decode_request(json!({"model":"native-publication","input":delivered["output"]})).unwrap();
        assert!(gateway.compaction.resolve(&Principal::new(key.id), &decoded.items).await.unwrap().is_some());
        server.abort();
        provider_server.abort();
        gateway.observation.shutdown().await;
    }).await.expect("registry publication failure scenario timed out");
}

#[tokio::test]
async fn inbound_responses_websocket_preserves_native_compaction_and_replays_current_window() {
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
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
        let admin = gateway.admin();
        let provider = admin.create_provider(CreateProvider {
            name: Some("local HTTP Responses provider".into()),
            source: ProviderSourceInput::Custom {
                vendor: Some("custom".into()),
                protocol: "open-responses".into(),
                base_url: format!("http://{provider_address}/v1"),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey { value: "local-provider-key".into() },
            use_proxy: false,
        }).await.unwrap();
        admin.create_manual_provider_model(&provider.id, "upstream-model", CreateManualProviderModel {
            metadata: json!({"id":"upstream-model","name":"upstream-model"}),
        }).await.unwrap();
        let route = admin.create_model(CreateRoute {
            model_id: "native-ws".into(),
            display_name: None,
            balance: None,
            target_provider: provider.id,
            target_model: "upstream-model".into(),
            targets: vec![],
        }).await.unwrap();
        let key = admin.create_api_key(CreateApiKey {
            key: None,
            name: "local ingress WebSocket".into(),
            concurrency_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_understanding: false,
            model_ids: vec![route.id],
        }).await.unwrap();
        let principal = Principal::new(key.id.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = super::create_router(gateway.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut socket = reqwest::Client::new()
            .get(format!("http://{address}/v1/responses"))
            .bearer_auth(&key.token)
            .upgrade().send().await.unwrap()
            .into_websocket().await.unwrap();
        let removed = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"history deliberately removed after compaction"}]});
        let retained = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"retained current question"}]});
        socket.send(reqwest_websocket::Message::Text(json!({
            "type":"response.create","model":"native-ws","input":[removed.clone(),retained.clone()],
            "context_management":[{"type":"compaction","compact_threshold":128}]
        }).to_string())).await.unwrap();
        let first = received.recv().await.unwrap();
        assert_eq!(first.body["input"], json!([removed, retained.clone()]));
        assert_eq!(first.body["context_management"], json!([{"type":"compaction","compact_threshold":128}]));
        assert_eq!(first.body["stream"], true);
        let native = json!({
            "type":"compaction","id":"native-state-exact",
            "encrypted_content":"opaque+/cipher==",
            "rolling_identity":{"version":7,"cursor":"preserve-me","nested":[1,{"flag":true}]}
        });
        first.respond.send(completed("first-native", json!([native.clone()]))).unwrap();
        let pair = crate::protocol::transform::ProtocolTransform::global()
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
        assert_eq!(second.body["context_management"], json!([]));
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
        // Detail queries include a writer barrier; no timing-based polling is needed.
        assert!(admin.observation_interaction("not-an-interaction", ForestQuery::default()).await.unwrap().is_none());
        let forest = admin.observation_forest(ForestQuery::default()).await.unwrap();
        let replay = forest.roots.iter().flat_map(|root| &root.interactions)
            .find(|interaction| interaction.context_events.iter().any(|event| event.kind == "native_compaction_associated"))
            .expect("admin forest exposes the native replay association");
        let detail = admin.observation_interaction(&replay.id, ForestQuery::default()).await.unwrap().unwrap();
        assert!(detail.runs.iter().flat_map(|run| &run.events)
            .any(|event| event.kind == "native_compaction_associated"));
        socket.send(reqwest_websocket::Message::Text(json!({
            "type":"response.create","model":"native-ws","input":"return the upstream error",
            "context_management":[{"type":"compaction","compact_threshold":128}]
        }).to_string())).await.unwrap();
        let rejected = received.recv().await.expect("client-requested compaction");
        let upstream_error = json!({
            "type":"provider_capacity_error","code":"compaction_unavailable",
            "message":"Compaction capacity is exhausted.","param":"context_management",
            "details":{"retryable":true}
        });
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
        socket.close(reqwest_websocket::CloseCode::Normal, None).await.unwrap();
        server.abort();
        provider_server.abort();
        gateway.observation.shutdown().await;
    }).await.expect("native ingress WebSocket scenario timed out");
}
