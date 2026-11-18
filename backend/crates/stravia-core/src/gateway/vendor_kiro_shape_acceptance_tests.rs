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
    let provider = create_vendor_provider(
        &gateway,
        "local Kiro",
        "kiro",
        "default",
        "kiro-generate-assistant-response",
        "https://runtime.us-east-1.kiro.dev",
        ProviderCredentialInput::Fields {
            values: BTreeMap::from([("apiKey".into(), json!("local-kiro-key"))]),
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
                    json!({"model":"kiro-wire", "input":"Reply local Kiro answer"}).to_string(),
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
    let mut lines = captured.head.lines();
    assert_eq!(lines.next(), Some("POST / HTTP/1.1"));
    let fields: Vec<_> = lines
        .filter(|line| !line.is_empty())
        .map(|line| line.split_once(": ").expect("HTTP field"))
        .collect();
    let ua = concat!("Stravia/", env!("CARGO_PKG_VERSION"), " Kiro-compat/3.4.0");
    let length = captured.body.len().to_string();
    let expected = [
        ("content-type", "application/x-amz-json-1.0"),
        (
            "x-amz-target",
            "KiroRuntimeService.GenerateAssistantResponse",
        ),
        ("content-length", length.as_str()),
        ("x-amzn-codewhisperer-optout", "true"),
        ("TokenType", "API_KEY"),
        ("x-amzn-kiro-client-attribution", "stravia"),
        ("x-amzn-kiro-mode-identifier", "vibe"),
        ("x-amzn-kiro-fallback", "0;max=0"),
        ("x-amz-user-agent", ua),
        ("user-agent", ua),
        ("host", "runtime.us-east-1.kiro.dev"),
        ("amz-sdk-invocation-id", "<uuid>"),
        ("amz-sdk-request", "attempt=1; max=1"),
        ("x-amzn-kiro-attempt", "1;max=1"),
        ("Authorization", "Bearer local-kiro-key"),
        ("Connection", "keep-alive"),
    ];
    assert_eq!(fields.len(), expected.len(), "{}", captured.head);
    for ((name, value), (expected_name, expected_value)) in fields.iter().zip(expected) {
        assert_eq!(*name, expected_name, "{}", captured.head);
        if expected_value == "<uuid>" {
            assert!(
                uuid::Uuid::parse_str(value).is_ok(),
                "invalid invocation id: {value}"
            );
        } else {
            assert_eq!(*value, expected_value, "field {name}");
        }
    }
    let request: Value = serde_json::from_slice(&captured.body)?;
    let message = &request["conversationState"]["currentMessage"]["userInputMessage"];
    assert_eq!(message["modelId"], "qwen3-coder-next");
    assert_eq!(message["content"], "Reply local Kiro answer");
    with_real_time_limit(server.task).await???;
    Ok(())
}
