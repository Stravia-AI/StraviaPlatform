use super::*;

async fn serve_continuation_recovery() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                raw.extend_from_slice(&chunk[..read]);
                let Some(end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&raw[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if raw.len() >= end + 4 + length {
                    break;
                }
            }
            let (_, body) = captured_http(&raw);
            let attempt = {
                let mut requests = observed.lock();
                requests.push(body);
                requests.len()
            };
            let (status, body) = if attempt == 1 {
                (
                    "404 Not Found",
                    serde_json::json!({"error":"continuation expired"}).to_string(),
                )
            } else {
                let mut response = AiResponse::new("resp-replayed", "fixture-model");
                response.push_output_text("recovered");
                ("200 OK", serde_json::to_string(&response).unwrap())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}/v1"), captured)
}

#[tokio::test]
async fn continuation_recovery_discards_failed_operation_metadata() {
    let (base_url, captured) = serve_continuation_recovery().await;
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let gateway = Gateway::new(GatewayConfig {
        data_dir: data_dir.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .expect("Gateway");
    let artifact = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("target/vendor-test-fixtures/lifecycle-v1.wasm");
    let preview = gateway
        .admin()
        .preview_vendor_plugin(std::fs::read(&artifact).unwrap_or_else(|error| {
            panic!(
                "missing fixture {} ({error}); run task build:vendor-fixtures",
                artifact.display()
            )
        }))
        .await
        .unwrap();
    gateway
        .admin()
        .confirm_vendor_plugin(crate::plugin::ConfirmPluginUpdate {
            preview_id: preview.id,
            allow_data_discard: false,
        })
        .await
        .unwrap();
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some("ZDR".into()),
            source: ProviderSourceInput::Custom {
                vendor: "fixture.lifecycle".into(),
                channel: "default".into(),
                protocol: Some("fixture-lifecycle".into()),
                base_url,
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::Fields {
                values: std::collections::BTreeMap::from([(
                    "apiKey".into(),
                    serde_json::json!("fixture-key"),
                )]),
            },
            vendor_options: serde_json::Map::from_iter([(
                "mode".into(),
                serde_json::json!("continuation"),
            )]),
            use_proxy: false,
        })
        .await
        .expect("Provider");
    gateway
        .admin()
        .create_manual_provider_model(
            &provider.id,
            "fixture-model",
            CreateManualProviderModel {
                metadata: serde_json::json!({"id":"fixture-model","name":"Fixture model"}),
                template_id: None,
            },
        )
        .await
        .unwrap();
    let model = admin
        .create_model(CreateRoute {
            model_id: "zdr-model".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id,
                model: "fixture-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: Some(1),
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .expect("Model");
    let key = admin
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "ZDR key".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![model.id.into()],
            inject_media_understanding: false,
        })
        .await
        .expect("API key");
    let executor = LiveModelTurnExecutor::new(
        gateway,
        crate::router::continuation::ScriptedContinuation::hit("resp-zdr"),
    );
    let mut request = AiRequest::new(
        "zdr-model",
        vec![stravia_runtime_contract::protocol::ir::AiItem {
            role: stravia_runtime_contract::protocol::ir::Role::User,
            content: stravia_runtime_contract::protocol::ir::MessageContent::Text(
                "follow-up".into(),
            ),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }],
    );
    request.stream.enabled = true;
    request.meta.source_protocol =
        Some(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24);
    request.ext = Some(
        stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt::default(),
        ),
    );

    let turn = executor
        .execute(TurnInput::new(Principal::new(key.id), request))
        .await
        .expect("ZDR continuation falls back to full replay");
    let mut output = turn.output;
    let events = output.by_ref().collect::<Vec<_>>().await;
    assert!(output.next().await.is_none());
    assert!(events.iter().all(Result::is_ok));
    let deltas = events
        .iter()
        .filter_map(|event| match event {
            Ok(CanonicalEvent::Delta(delta)) => Some(delta),
            _ => None,
        })
        .collect::<Vec<_>>();
    let text: String = deltas
        .iter()
        .filter_map(|delta| match delta {
            AiStreamDelta::TextDelta(text) | AiStreamDelta::TextDeltaWithMetadata { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(text, "recovered");
    for delta in deltas {
        match delta {
            AiStreamDelta::MessageStart { id, model } => {
                assert_eq!(id, "resp-replayed");
                assert_eq!(model, "fixture-model");
            }
            AiStreamDelta::ResponseMetadata { metadata } => {
                assert!(metadata.get("discarded_operation").is_none());
            }
            _ => {}
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(CanonicalEvent::Completed(_))))
            .count(),
        1
    );
    assert!(matches!(
        events.last(),
        Some(Ok(CanonicalEvent::Completed(response)))
            if response.id == "resp-replayed" && response.output_text() == "recovered"
    ));

    let captured = captured.lock();
    assert_eq!(captured.len(), 2);
    let first: AiRequest = serde_json::from_value(captured[0].clone()).unwrap();
    let replay: AiRequest = serde_json::from_value(captured[1].clone()).unwrap();
    assert!(matches!(
        first.ext,
        Some(stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(ext))
            if ext.previous_response_id.as_deref() == Some("resp-zdr")
    ));
    assert!(matches!(
        replay.ext,
        Some(stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(ext))
            if ext.previous_response_id.is_none()
    ));
}
