use super::*;

#[tokio::test]
async fn codex_native_compaction_preserves_errors_and_account_identity() {
    use axum::{
        Json, Router,
        extract::{
            State,
            ws::{Message, WebSocketUpgrade},
        },
        response::IntoResponse,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    fn compaction_error(code: &str) -> Value {
        json!({
            "code": code,
            "type": "invalid_request_error",
            "message": "native-compaction-rejected access-token-before-refresh",
            "details": {"retained": true},
        })
    }
    async fn compact(
        State(calls): State<Arc<AtomicUsize>>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        calls.fetch_add(1, Ordering::SeqCst);
        if body["instructions"] == "reject-native-compaction" {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": compaction_error("invalid_encrypted_content")})),
            )
                .into_response();
        }
        if body.get("stream").is_some() || body.get("input").and_then(Value::as_array).is_none() {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        Json(json!({"id":"compact-unary","object":"response.compaction","created_at":17,
            "output":[{"type":"message","role":"user","content":[{"type":"input_text","text":"retained"}]},
                {"type":"compaction","id":"compact-state-unary","encrypted_content":"unary-state","rolling_identity":{"version":2}}],
            "usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}})).into_response()
    }
    async fn responses(
        State(calls): State<Arc<AtomicUsize>>,
        upgrade: WebSocketUpgrade,
    ) -> axum::response::Response {
        upgrade.on_upgrade(|mut socket| async move {
            let state = json!({"type":"compaction","id":"inline-state","encrypted_content":"inline-cipher","rolling_identity":{"version":3}});
            while let Some(Ok(Message::Text(text))) = socket.recv().await {
                calls.fetch_add(1, Ordering::SeqCst);
                let request: Value = serde_json::from_str(&text).expect("native request");
                if request["instructions"] == "reject-native-compaction" {
                    socket.send(Message::Text(json!({
                        "type": "error",
                        "status": 503,
                        "error": compaction_error("previous_response_not_found"),
                    }).to_string().into())).await.unwrap();
                    continue;
                }
                let input = request["input"].as_array().expect("native input");
                let triggered = input.iter().any(|item| item["type"] == "compaction_trigger");
                let replayed = input.iter().any(|item| item == &state);
                if !triggered && !replayed {
                    socket.send(Message::Text(json!({"type":"error","error":{"type":"invalid_request_error","code":"invalid_state","message":"native state was changed"}}).to_string().into())).await.unwrap();
                    continue;
                }
                let output = if triggered { vec![state.clone()] } else { vec![json!({"type":"message","id":"reply","role":"assistant","status":"completed","content":[{"type":"output_text","text":"native replay accepted","annotations":[]}]})] };
                let response_id = if triggered { "trigger-response" } else { "replay-response" };
                let created = stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    response_id, "upstream-model", "in_progress", Vec::new(),
                    Value::Null, Value::Null, Value::Null,
                );
                socket.send(Message::Text(json!({"type":"response.created","response":created}).to_string().into())).await.unwrap();
                for (index, item) in output.iter().enumerate() {
                    socket.send(Message::Text(json!({"type":"response.output_item.added","output_index":index,"item":item}).to_string().into())).await.unwrap();
                    socket.send(Message::Text(json!({"type":"response.output_item.done","output_index":index,"item":item}).to_string().into())).await.unwrap();
                }
                let completed = stravia_protocol_codec::codec::open_responses::formatter::response_resource_snapshot(
                    response_id, "upstream-model", "completed", if triggered { Vec::new() } else { output },
                    Value::Null, Value::Null,
                    json!({"input_tokens":3,"output_tokens":1,"total_tokens":4,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}),
                );
                socket.send(Message::Text(json!({"type":"response.completed","response":completed}).to_string().into())).await.unwrap();
            }
        }).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/responses", get(responses))
        .route("/responses/compact", post(compact))
        .with_state(Arc::clone(&upstream_calls));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .await
    .unwrap();
    crate::plugin::test_support::install_distributed_vendor(&gateway, "openai-codex")
        .await
        .expect("Codex vendor plugin");
    let provider = gateway
        .storage
        .providers()
        .create(crate::db::models::CreateProviderRecord {
            name: "local Codex".into(),
            vendor: Some("openai-codex".into()),
            channel: Some("codex".into()),
            protocol: "open-responses".into(),
            base_url: format!("http://{address}"),
            preset_key: Some("openai".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            adapter_credentials: "{}".into(),
            vendor_options: json!({
                "websocket_url": format!("ws://{address}/responses")
            })
            .to_string(),
            auth_mode: "oauth".into(),
            use_proxy: false,
        })
        .await
        .unwrap();
    gateway
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "access-token-before-refresh".into(),
                refresh_token: Some("refresh-token-before-refresh".into()),
                expires_at: Some("2099-01-01T00:00:00Z".into()),
                resource_url: Some(format!("http://{address}")),
                subject_id: Some("account-one".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    add_test_provider_model(&gateway, &provider.id).await;
    let route = gateway
        .admin()
        .create_model(CreateRoute {
            model_id: "compact-codex".into(),
            display_name: None,
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "upstream-model".into(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await
        .unwrap();
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "native compact".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            inject_media_understanding: false,
            model_ids: vec![route.id.into()],
        })
        .await
        .unwrap();
    let principal = Principal::new(key.id);
    let pair = stravia_protocol_codec::transform::ProtocolTransform::global()
        .bind(OPEN_RESPONSES_2026_04_24, OPEN_RESPONSES_2026_04_24)
        .unwrap();
    let request = pair
        .decode_request(
            json!({"model":"compact-codex","input":[{"role":"user","content":"compact this"}]}),
        )
        .unwrap();
    let mut input = TurnInput::new(principal.clone(), request);
    input.purpose = ModelTurnPurpose::Compact;
    let mut turn = gateway
        .model_turn
        .execute(input)
        .await
        .expect("standalone uses HTTP unary despite Codex stream-only generation");
    let CanonicalEvent::Compacted(result) = turn.output.next().await.unwrap().unwrap() else {
        panic!("native compact terminal required")
    };
    assert_eq!(result.wire["id"], "compact-unary");
    assert_eq!(result.wire["output"][0]["content"][0]["text"], "retained");
    assert_eq!(
        result.wire["output"][1]["rolling_identity"],
        json!({"version":2})
    );
    for (standalone, code) in [
        (true, "invalid_encrypted_content"),
        (false, "previous_response_not_found"),
    ] {
        let calls_before_rejection = upstream_calls.load(Ordering::SeqCst);
        let mut wire = json!({
            "model": "compact-codex",
            "input": [{"role": "user", "content": "do not replay rejected compaction"}],
            "instructions": "reject-native-compaction",
        });
        if !standalone {
            wire["context_management"] = json!([{"type": "compaction", "compact_threshold": 2000}]);
        }
        let mut input = TurnInput::new(principal.clone(), pair.decode_request(wire).unwrap());
        if standalone {
            input.purpose = ModelTurnPurpose::Compact;
        }
        let rejected = gateway
            .model_turn
            .execute(input)
            .await
            .err()
            .expect("native compaction must preserve the upstream rejection");
        assert_eq!(rejected.upstream_status, Some(503));
        let body = rejected
            .upstream_body
            .expect("original upstream error body");
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["details"], json!({"retained": true}));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("native-compaction-rejected")
        );
        assert!(!body.to_string().contains("access-token-before-refresh"));
        assert_eq!(
            upstream_calls.load(Ordering::SeqCst),
            calls_before_rejection + 1,
            "native compaction rejection must not trigger recovery or replay"
        );
    }
    let trigger = pair.decode_request(json!({"model":"compact-codex","input":[{"role":"user","content":"remote v2"},{"type":"compaction_trigger"}]})).unwrap();
    let mut triggered = gateway
        .model_turn
        .execute(TurnInput::new(principal.clone(), trigger))
        .await
        .unwrap();
    let mut native = None;
    while let Some(event) = triggered.output.next().await {
        match event.unwrap() {
            CanonicalEvent::Delta(AiStreamDelta::ItemDone { item, .. }) if item.is_compaction() => {
                assert!(
                    gateway
                        .compaction
                        .resolve(&principal, std::slice::from_ref(&item))
                        .await
                        .unwrap()
                        .is_some(),
                    "complete state is durable before delivery"
                );
                native = stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(
                    &item,
                );
            }
            CanonicalEvent::Completed(_) => break,
            _ => {}
        }
    }
    let native = native.expect("native triggered state");
    let oauth = gateway.storage.oauth_credentials();
    let before_refresh = oauth
        .get(&provider.id)
        .await
        .unwrap()
        .expect("OAuth connection");
    let locked = oauth
        .try_begin_refresh(&provider.id, before_refresh.status_version)
        .await
        .unwrap()
        .expect("OAuth refresh lease");
    let refreshed = oauth
        .complete_refresh(
            &provider.id,
            locked.status_version,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "access-token-after-refresh".into(),
                refresh_token: Some("refresh-token-after-refresh".into()),
                expires_at: Some("2099-06-01T00:00:00Z".into()),
                resource_url: Some(format!("http://{address}")),
                subject_id: Some("account-one".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(refreshed.connection_id, before_refresh.connection_id);

    let replay = pair.decode_request(json!({"model":"compact-codex","input":[native.clone(),{"role":"user","content":"continue after refresh"}]})).unwrap();
    let mut continued = gateway
        .model_turn
        .execute(TurnInput::new(principal.clone(), replay))
        .await
        .unwrap();
    let mut accepted = false;
    while let Some(event) = continued.output.next().await {
        if let CanonicalEvent::Completed(response) = event.unwrap() {
            accepted = response.output_text() == "native replay accepted";
            break;
        }
    }
    assert!(
        accepted,
        "rotating every OAuth token field must preserve native state replay"
    );

    let reconnected = oauth
        .upsert(
            &provider.id,
            crate::db::models::UpsertOAuthCredential {
                driver_key: "openai-codex".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "different-account-access-token".into(),
                refresh_token: Some("different-account-refresh-token".into()),
                expires_at: Some("2099-12-01T00:00:00Z".into()),
                resource_url: Some(format!("http://{address}")),
                subject_id: Some("account-two".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(reconnected.connection_id, refreshed.connection_id);
    let calls_before_rejected_replay = upstream_calls.load(Ordering::SeqCst);
    let stale_replay = pair.decode_request(json!({"model":"compact-codex","input":[native,{"role":"user","content":"must not cross accounts"}]})).unwrap();
    let rejected = gateway
        .model_turn
        .execute(TurnInput::new(principal, stale_replay))
        .await
        .err()
        .expect("old account state must be rejected");
    assert_eq!(rejected.code, "compaction_target_mismatch");
    assert_eq!(
        upstream_calls.load(Ordering::SeqCst),
        calls_before_rejected_replay,
        "old native state must be rejected before reaching the reconnected account"
    );
    server.abort();
}
