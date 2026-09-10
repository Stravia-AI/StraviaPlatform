use super::*;

const SECRET: &str = "ghp_8Dq7mP2vL9sX4aR6tK3nF5wH1jB0cYzUeIoG";
const TOOL_SECRET: &str = "ghp_3Jt8cR2pX6mQ9wK1vH5nL7aD4sF0bYzUeIoG";
const UNKNOWN: &str = "~stravia-secret:00000000000000000000000000000000~";

struct CredentialFileTool {
    path: std::path::PathBuf,
    array_output: bool,
}

#[async_trait]
impl crate::hook::PlatformTool for CredentialFileTool {
    fn id(&self) -> crate::hook::ToolId {
        crate::hook::ToolId::new("ordered-tool")
    }

    fn external_name(&self) -> &str {
        "ordered_tool"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _context: crate::hook::ToolExecutionContext,
    ) -> Result<serde_json::Value, crate::hook::PlatformToolError> {
        let value = arguments["value"]
            .as_str()
            .ok_or_else(|| crate::hook::PlatformToolError::new("missing credential"))?;
        tokio::fs::write(&self.path, value)
            .await
            .map_err(|_| crate::hook::PlatformToolError::new("credential file unavailable"))?;
        Ok(if self.array_output {
            serde_json::json!([{"type": "text", "value": format!("{value} {TOOL_SECRET}")}])
        } else {
            serde_json::json!({"api_key": TOOL_SECRET, "configured": true})
        })
    }
}

#[tokio::test]
async fn platform_tool_http_roundtrip_restores_execution_and_protects_hidden_turn() {
    platform_tool_roundtrip(false, false, false).await;
}

#[tokio::test]
async fn platform_tool_responses_websocket_restores_execution_and_hidden_turn() {
    platform_tool_roundtrip(true, false, true).await;
}

#[tokio::test]
async fn platform_tool_json_array_type_fields_remain_business_data() {
    platform_tool_roundtrip(false, true, true).await;
}

async fn platform_tool_roundtrip(websocket: bool, array_output: bool, debug: bool) {
    use futures::StreamExt;

    let requests = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = Arc::clone(&requests);
    let provider = Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(body.clone());
                let mut response = if !body["messages"].as_array().unwrap().iter()
                    .any(|message| message["role"] == "tool") {
                    let wire = serde_json::to_string(&body["messages"]).unwrap();
                    let reference = regex::Regex::new(r"~stravia-secret:[0-9a-f]{32}~")
                        .unwrap().find(&wire).map(|value| value.as_str()).unwrap_or("unprotected");
                    serde_json::json!({
                        "id": "chatcmpl-tool", "object": "chat.completion", "model": "provider-model",
                        "choices": [{"index": 0, "message": {
                            "role": "assistant", "content": null,
                            "tool_calls": [{"id": "configure-secret", "type": "function",
                                "function": {"name": "stravia__ordered_tool",
                                    "arguments": serde_json::json!({"value": reference}).to_string()}}]
                        }, "finish_reason": "tool_calls"}]
                    })
                } else {
                    let result = body["messages"].as_array().unwrap().iter()
                        .find(|message| message["role"] == "tool").unwrap();
                    openai_response(result["content"].as_str().unwrap())
                };
                response["choices"][0]["message"]["reasoning_content"] =
                    serde_json::json!("Inspect the request and tool result.");
                use axum::response::IntoResponse;
                if body["stream"] != true {
                    return axum::Json(response).into_response();
                }
                let choice = &response["choices"][0];
                let message = &choice["message"];
                let mut deltas = vec![serde_json::json!({"role": "assistant"})];
                for text in ["Inspect the request ", "and tool result."] {
                    deltas.push(serde_json::json!({"reasoning_content": text}));
                }
                if let Some(calls) = message["tool_calls"].as_array() {
                    for (index, call) in calls.iter().enumerate() {
                        deltas.push(serde_json::json!({"tool_calls": [{
                            "index": index, "id": call["id"], "type": "function",
                            "function": {"name": call["function"]["name"], "arguments": ""}
                        }]}));
                        for character in call["function"]["arguments"].as_str().unwrap().chars() {
                            deltas.push(serde_json::json!({"tool_calls": [{
                                "index": index, "function": {"arguments": character.to_string()}
                            }]}));
                        }
                    }
                } else {
                    for character in message["content"].as_str().unwrap().chars() {
                        deltas.push(serde_json::json!({"content": character.to_string()}));
                    }
                }
                let mut events = String::new();
                for delta in deltas {
                    let event = serde_json::json!({
                        "id": response["id"], "object": "chat.completion.chunk",
                        "model": "provider-model",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
                    });
                    events.push_str(&format!("data: {event}\n\n"));
                }
                let terminal = serde_json::json!({
                    "id": response["id"], "object": "chat.completion.chunk",
                    "model": "provider-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": choice["finish_reason"]}]
                });
                events.push_str(&format!("data: {terminal}\n\ndata: [DONE]\n\n"));
                ([(header::CONTENT_TYPE, "text/event-stream")], events).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_url = format!("http://{}", listener.local_addr().unwrap());
    let provider_task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let credential_path = directory.path().join("configured-token");
    let (hook, _) = ExposeOrderedToolHook::counting();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(hook))
    .platform_tool(Arc::new(CredentialFileTool {
        path: credential_path.clone(),
        array_output,
    }))
    .build()
    .await
    .unwrap();
    gateway.observation.set_debug_enabled(debug);
    let mut observations = gateway.observation.subscribe(0);
    configure_route(&gateway, "redaction-platform", &[provider_url]).await;
    gateway
        .admin()
        .set_setting(crate::reversible_redaction::SETTING_KEY, "true")
        .await
        .unwrap();
    let headers = authorized_headers(&gateway).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::proxy::server::create_router(gateway.clone());
    let gateway_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut response_id = None;
    let content = if websocket {
        use futures::SinkExt;
        use reqwest_websocket::Upgrade as _;
        let response = reqwest::Client::new()
            .get(format!("{gateway_url}/v1/responses"))
            .headers(headers.clone())
            .upgrade()
            .send()
            .await
            .unwrap();
        let mut socket = response.into_websocket().await.unwrap();
        socket
            .send(reqwest_websocket::Message::Text(
                serde_json::json!({
                    "type": "response.create", "model": "redaction-platform",
                    "input": format!("Configure github_token={SECRET}")
                })
                .to_string(),
            ))
            .await
            .unwrap();
        let mut content = String::new();
        loop {
            let message = tokio::time::timeout(std::time::Duration::from_secs(15), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let reqwest_websocket::Message::Text(text) = message else {
                continue;
            };
            let event: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_ne!(event["type"], "error", "{event}");
            assert_ne!(event["type"], "response.failed", "{event}");
            if event["type"] == "response.output_text.delta" {
                content.push_str(event["delta"].as_str().unwrap());
            }
            if event["type"] == "response.completed" {
                break;
            }
        }
        SinkExt::close(&mut socket).await.unwrap();
        content
    } else {
        let (endpoint, payload) = if array_output {
            (
                "/v1/responses",
                serde_json::json!({
                    "model": "redaction-platform", "store": true,
                    "input": format!("Configure github_token={SECRET}"),
                }),
            )
        } else {
            (
                "/v1/chat/completions",
                serde_json::json!({
                    "model": "redaction-platform",
                    "messages": [{"role": "user", "content": format!("Configure github_token={SECRET}")}]
                }),
            )
        };
        let response = reqwest::Client::new()
            .post(format!("{gateway_url}{endpoint}"))
            .headers(headers.clone())
            .json(&payload)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let response: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{response}");
        if array_output {
            response_id = Some(response["id"].as_str().unwrap().to_owned());
            response["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["content"].as_array())
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect()
        } else {
            response["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    assert_eq!(
        tokio::fs::read_to_string(&credential_path).await.unwrap(),
        SECRET
    );
    let configured: serde_json::Value = serde_json::from_str(&content).unwrap();
    if array_output {
        assert_eq!(
            configured,
            serde_json::json!([{"type": "text", "value": format!("{SECRET} {TOOL_SECRET}")}])
        );
    } else {
        assert_eq!(configured["api_key"], TOOL_SECRET);
        assert_eq!(configured["configured"], true);
    }
    let request_snapshot = requests.lock().unwrap().clone();
    assert_eq!(request_snapshot.len(), 2);
    for request in &request_snapshot {
        let wire = request.to_string();
        assert!(!wire.contains(SECRET));
        assert!(!wire.contains(TOOL_SECRET));
    }
    let second = request_snapshot[1].to_string();
    let references = regex::Regex::new(r"~stravia-secret:[0-9a-f]{32}~").unwrap();
    assert_eq!(
        references
            .find_iter(&second)
            .map(|found| found.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
    let interaction_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(update) = observations.next().await {
            if let crate::interaction_observation::ObservationUpdate::Event(event) = update {
                if event.kind == "run_finished" {
                    return event.interaction_id.expect("finished interaction");
                }
            }
        }
        panic!("observation stream ended before the run finished");
    })
    .await
    .unwrap();
    let detail = gateway
        .observation
        .get_interaction(&interaction_id, Default::default())
        .await
        .unwrap()
        .unwrap();
    let events: Vec<_> = detail.runs.iter().flat_map(|run| &run.events).collect();
    let started = events
        .iter()
        .find(|event| event.kind == "platform_tool_started")
        .expect("ordinary platform input");
    assert_eq!(
        started.payload["input"],
        serde_json::json!({"value": "***"})
    );
    let finished = events
        .iter()
        .find(|event| event.kind == "platform_tool_finished")
        .expect("ordinary platform result");
    assert_eq!(finished.payload["tool_id"], started.payload["tool_id"]);
    assert_eq!(finished.payload["status"], "completed");
    assert!(finished.payload.get("content").is_some());
    if !array_output {
        assert_eq!(
            finished.payload["content"],
            serde_json::json!({"api_key": "***", "configured": true})
        );
    }
    let mut thoughts = std::collections::BTreeMap::<String, String>::new();
    for event in &events {
        if event.kind == "model_thinking_delta" {
            thoughts
                .entry(event.payload["attempt_id"].as_str().unwrap().to_owned())
                .or_default()
                .push_str(event.payload["text"].as_str().unwrap());
        }
    }
    assert_eq!(thoughts.len(), 2);
    assert!(
        thoughts
            .values()
            .all(|text| text == "Inspect the request and tool result.")
    );
    if debug {
        let checkpoint = detail
            .runs
            .iter()
            .flat_map(|run| &run.debug_events)
            .find(|event| event["stage"] == "platform_tool_call")
            .expect("platform execution argument checkpoint");
        let recorded_arguments: serde_json::Value =
            serde_json::from_str(checkpoint["payload"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(recorded_arguments, serde_json::json!({"value": "***"}));
    } else {
        assert!(
            detail
                .runs
                .iter()
                .all(|run| !run.debug_enabled && run.debug_events.is_empty())
        );
    }
    if array_output {
        let continuation = serde_json::json!({
            "model": "redaction-platform",
            "previous_response_id": response_id.unwrap(),
            "input": "Continue with the previous tool result",
        });
        let client = reqwest::Client::new();
        let resumed = client
            .post(format!("{gateway_url}/v1/responses"))
            .headers(headers.clone())
            .json(&continuation)
            .send()
            .await
            .unwrap();
        let status = resumed.status();
        let resumed: serde_json::Value = resumed.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{resumed}");
        let wire = requests.lock().unwrap().last().unwrap().to_string();
        assert!(!wire.contains(SECRET) && !wire.contains(TOOL_SECRET));

        fn remove_payload_kinds(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Array(values) => {
                    for value in values {
                        remove_payload_kinds(value);
                    }
                }
                serde_json::Value::Object(values) => {
                    if values.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                    {
                        values.remove("content_kind");
                    }
                    for value in values.values_mut() {
                        remove_payload_kinds(value);
                    }
                }
                _ => {}
            }
        }
        // Simulate history written before the semantic tag existed, rather than
        // allowing an ingress request to forge internal provenance.
        let pool = gateway._sqlite_pool.as_ref().unwrap();
        for (select, update) in [
            (
                "SELECT id, payload FROM turn_chain_nodes WHERE payload IS NOT NULL",
                "UPDATE turn_chain_nodes SET payload=? WHERE id=?",
            ),
            (
                "SELECT reference, segment_payload FROM history_markers WHERE segment_payload IS NOT NULL",
                "UPDATE history_markers SET segment_payload=? WHERE reference=?",
            ),
        ] {
            let rows: Vec<(String, String)> = sqlx::query_as(select).fetch_all(pool).await.unwrap();
            for (id, payload) in rows {
                let mut payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
                remove_payload_kinds(&mut payload);
                sqlx::query(update)
                    .bind(payload.to_string())
                    .bind(id)
                    .execute(pool)
                    .await
                    .unwrap();
            }
        }
        let requests_before = requests.lock().unwrap().len();
        let rejected = client
            .post(format!("{gateway_url}/v1/responses"))
            .headers(headers.clone())
            .json(&continuation)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(requests.lock().unwrap().len(), requests_before);
        gateway
            .admin()
            .set_setting(crate::reversible_redaction::SETTING_KEY, "false")
            .await
            .unwrap();
        let unprotected = client
            .post(format!("{gateway_url}/v1/responses"))
            .headers(headers.clone())
            .json(&continuation)
            .send()
            .await
            .unwrap();
        let status = unprotected.status();
        let unprotected: serde_json::Value = unprotected.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{unprotected}");
        assert_eq!(requests.lock().unwrap().len(), requests_before + 1);
    }
    if !websocket && !array_output {
        let first_wire = request_snapshot[0].to_string();
        let reference = references.find(&first_wire).unwrap().as_str();
        gateway
            .admin()
            .set_setting(crate::reversible_redaction::SETTING_KEY, "false")
            .await
            .unwrap();
        for (input, expected) in [(reference, SECRET), (UNKNOWN, UNKNOWN)] {
            let response = reqwest::Client::new()
                .post(format!("{gateway_url}/v1/chat/completions"))
                .headers(headers.clone())
                .json(&serde_json::json!({
                    "model": "redaction-platform",
                    "messages": [{"role": "user", "content": input}]
                }))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let response: serde_json::Value = response.json().await.unwrap();
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(
                tokio::fs::read_to_string(&credential_path).await.unwrap(),
                expected
            );
            let configured: serde_json::Value = serde_json::from_str(
                response["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(configured["api_key"], TOOL_SECRET);
            assert!(
                requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .to_string()
                    .contains(TOOL_SECRET)
            );
        }
    }
    gateway.shutdown().await;
    gateway_task.abort();
    provider_task.abort();
}

#[tokio::test]
async fn redaction_continuation_reuses_only_equal_provider_visible_history() {
    for (first_enabled, second_enabled, reusable) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
    ] {
        let (upstream, _, captured) =
            serve_responses_websocket_sequence(vec!["first answer", "second answer"]).await;
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap();
        configure_route_with_protocol(
            &gateway,
            "redaction-continuation",
            &[upstream],
            "openai",
            "openai-compatible",
        )
        .await;
        let headers = authorized_headers(&gateway).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let router = crate::proxy::server::create_router(gateway.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        gateway
            .admin()
            .set_setting(
                crate::reversible_redaction::SETTING_KEY,
                if first_enabled { "true" } else { "false" },
            )
            .await
            .unwrap();
        let mut events = gateway.observation.subscribe(0);
        let response = reqwest::Client::new()
            .post(&url)
            .headers(headers.clone())
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "redaction-continuation", "max_tokens": 128,
                "messages": [{"role": "user", "content": SECRET}]
            }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["content"][0]["text"], "first answer");
        wait_for_observed_run_finish(&mut events).await;
        gateway
            .admin()
            .set_setting(
                crate::reversible_redaction::SETTING_KEY,
                if second_enabled { "true" } else { "false" },
            )
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .post(&url)
            .headers(headers)
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "redaction-continuation", "max_tokens": 128,
                "messages": [
                    {"role": "user", "content": SECRET},
                    {"role": "assistant", "content": "first answer"},
                    {"role": "user", "content": "second"}
                ]
            }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["content"][0]["text"], "second answer");
        let requests = captured.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].to_string().contains(SECRET), !first_enabled);
        if reusable {
            assert_eq!(requests[1]["previous_response_id"], "resp-provider");
            assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
        } else {
            assert!(requests[1].get("previous_response_id").is_none());
            assert_eq!(requests[1].to_string().contains(SECRET), !second_enabled);
            assert_eq!(requests[1]["input"].as_array().unwrap().len(), 3);
        }
        gateway.shutdown().await;
        server.abort();
    }
}
