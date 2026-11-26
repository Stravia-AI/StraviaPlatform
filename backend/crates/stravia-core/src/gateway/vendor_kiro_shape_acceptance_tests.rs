use super::*;

// Construct an AWS EventStream response independently of the guest decoder.
fn event_frame(event: &str, payload: &str) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", event)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let mut frame = Vec::new();
    frame.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(payload.as_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

#[tokio::test]
async fn kiro_real_wasm_inference_matches_http_wire() -> anyhow::Result<()> {
    let mut body = event_frame(
        "assistantResponseEvent",
        r#"{"content":"local Kiro answer"}"#,
    );
    body.extend(event_frame("metadataEvent", r#"{"stopReason":"END_TURN"}"#));
    let (server, capture) = h1_capture_server(
        "runtime.us-east-1.kiro.dev",
        LocalResponse {
            status: "200 OK",
            content_type: "application/vnd.amazon.eventstream",
            body,
            release: None,
        },
    )
    .await?;
    let (_directory, gateway) = inference_gateway(&server, "kiro").await?;
    let provider = gateway
        .storage
        .providers()
        .create(crate::db::models::CreateProviderRecord {
            name: "local Kiro".into(),
            vendor: Some("kiro".into()),
            protocol: "kiro-generate-assistant-response-oauth".into(),
            base_url: "https://runtime.us-east-1.kiro.dev".into(),
            preset_key: None,
            channel: Some("oauth".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: "{}".into(),
            auth_mode: "oauth".into(),
            proxy_mode: "none".into(),
        })
        .await?;
    let profile_arn = "arn:aws:codewhisperer:us-east-1:123456789012:profile/synthetic";
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "kiro".into(),
                scheme: "Bearer".into(),
                access_token: "synthetic-kiro-access".into(),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                meta: Some(json!({"profileArn": profile_arn}).to_string()),
                ..Default::default()
            },
        )
        .await?;
    let key = route_key(&gateway, &provider.id, "qwen3-coder-next", "kiro-wire").await?;
    let response = with_real_time_limit(
        crate::proxy::server::create_router(gateway).oneshot(
            Request::post("/v1/responses")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "kiro-wire", "input": "Reply local Kiro answer",
                        "reasoning": {"summary": "auto"}, "store": false,
                        "prompt_cache_key": "synthetic-cache",
                        "tools": [{
                            "type": "function", "name": "lookup", "strict": true,
                            "parameters": {
                                "type": "object", "properties": {"store": {"type": "boolean"}},
                                "required": ["store"], "additionalProperties": false
                            }
                        }]
                    })
                    .to_string(),
                ))?,
        ),
    )
    .await??;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    anyhow::ensure!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&body)
    );
    let result: Value = serde_json::from_slice(&body)?;
    assert!(
        result["output"].as_array().unwrap().iter().any(|item| {
            item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part["text"] == "local Kiro answer")
            })
        }),
        "{result}"
    );

    let captured = with_real_time_limit(capture).await??;
    crate::plugin::network::shape::tests::assert_wire_server_name(
        &captured.hello,
        "runtime.us-east-1.kiro.dev",
    );
    let tui: Value = serde_json::from_str(include_str!(
        "../../../stravia-vendor-kiro/kiro-cli-3.4.0-tui-http.json"
    ))?;
    let mut lines = captured.head.lines();
    assert_eq!(lines.next(), tui["requestLine"].as_str());
    let fields: Vec<_> = lines
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once(": ").expect("HTTP field"))
        .collect();
    let length = captured.body.len().to_string();
    let tui_headers = tui["headers"].as_array().context("TUI headers")?;
    assert_eq!(fields.len(), tui_headers.len());
    for (&(name, value), expected) in fields.iter().zip(tui_headers) {
        assert_eq!(Some(name), expected[0].as_str());
        let expected_value = match name {
            "content-length" => &length,
            "amz-sdk-invocation-id" => {
                uuid::Uuid::parse_str(value)?;
                continue;
            }
            "Authorization" => "Bearer synthetic-kiro-access",
            _ => expected[1].as_str().context("fixed TUI field value")?,
        };
        assert_eq!(value, expected_value, "field {name}");
    }
    let request: Value = serde_json::from_slice(&captured.body)?;
    let state = &request["conversationState"];
    for (actual, expected) in [(&request, &tui["bodyKeys"]), (state, &tui["stateKeys"])] {
        let keys = actual.as_object().context("wire JSON object")?;
        let expected = expected.as_array().context("TUI JSON keys")?;
        assert_eq!(keys.len(), expected.len());
        for key in expected {
            let key = key.as_str().context("TUI JSON field name")?;
            assert!(keys.contains_key(key), "field {key}");
        }
    }
    assert_eq!(request["profileArn"], profile_arn);
    assert_eq!(request["agentMode"], tui["agentMode"]);
    assert_eq!(state["agentTaskType"], tui["agentTaskType"]);
    assert_eq!(state["chatTriggerType"], tui["chatTriggerType"]);
    assert_eq!(state["rootConversationId"], state["conversationId"]);
    let session = state["conversationId"]
        .as_str()
        .context("conversation ID")?;
    uuid::Uuid::parse_str(
        session
            .strip_prefix("sess_")
            .context("TUI session prefix")?,
    )?;
    uuid::Uuid::parse_str(
        state["agentContinuationId"]
            .as_str()
            .context("continuation ID")?,
    )?;
    let message = &state["currentMessage"]["userInputMessage"];
    assert_eq!(message["origin"], tui["origin"]);
    assert_eq!(message["modelId"], "qwen3-coder-next");
    assert_eq!(message["content"], "Reply local Kiro answer");
    assert_eq!(
        message["userInputMessageContext"]["tools"],
        json!([{
            "toolSpecification": {
                "name": "lookup", "description": "lookup",
                "inputSchema": {"json": {
                    "type": "object", "properties": {"store": {"type": "boolean"}},
                    "required": ["store"], "additionalProperties": false
                }}
            }
        }])
    );
    with_real_time_limit(server.task).await???;
    Ok(())
}
