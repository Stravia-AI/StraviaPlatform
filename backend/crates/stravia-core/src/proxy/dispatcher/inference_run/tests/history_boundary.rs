use super::*;

type BoundaryObservation = (
    crate::generation_chain::HistoryBoundaryCandidate,
    Arc<AiRequest>,
    Vec<usize>,
);

struct BoundaryRecordingExecutor {
    inner: Arc<dyn crate::model_turn::ModelTurnExecutor>,
    observations: Arc<parking_lot::Mutex<Vec<BoundaryObservation>>>,
}

#[async_trait]
impl crate::model_turn::ModelTurnExecutor for BoundaryRecordingExecutor {
    async fn execute(
        &self,
        input: crate::model_turn::TurnInput,
    ) -> Result<crate::model_turn::ModelTurn, stravia_runtime_contract::model_turn::ModelTurnError>
    {
        self.observations.lock().push((
            input
                .history_boundary_candidate
                .clone()
                .expect("trusted candidate"),
            input
                .history_boundary_source
                .clone()
                .expect("boundary source"),
            input.history_boundary_gaps.clone().expect("boundary gaps"),
        ));
        self.inner.execute(input).await
    }
}

#[tokio::test]
async fn platform_followup_uses_distinct_boundary_after_failover() {
    let (failed_url, failed_calls) = serve_openai_response(
        500,
        serde_json::json!({"error": {"message": "unavailable"}}),
    )
    .await;
    let platform_round = serde_json::json!({
        "id": "boundary-tool", "object": "chat.completion", "created": 1,
        "model": "provider-model", "choices": [{"index": 0, "message": {
            "role": "assistant", "content": null, "tool_calls": [{
                "id": "platform-call", "type": "function", "function": {
                    "name": "stravia__ordered_tool", "arguments": "{\"index\":1}"
                }
            }]}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let (fallback_url, fallback_calls) =
        serve_openai_sequence(vec![platform_round, openai_response("final")]).await;
    let directory = tempfile::tempdir().expect("temporary data directory");
    let (hook, _) = ExposeOrderedToolHook::counting();
    let gateway = Gateway::builder(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .hook(Arc::new(hook))
    .platform_tool(Arc::new(OrderedTool {
        calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
    }))
    .build()
    .await
    .expect("gateway");
    let model = "boundary-failover";
    configure_route(&gateway, model, &[failed_url, fallback_url]).await;
    set_target_retry_budget(&gateway, model, 0).await;
    let headers = authorized_headers(&gateway).await;
    let observations = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let executor = BoundaryRecordingExecutor {
        inner: Arc::clone(&gateway.model_turn),
        observations: Arc::clone(&observations),
    };
    let mut user = stravia_runtime_contract::protocol::ir::AiItem::output_text("hello");
    user.role = stravia_runtime_contract::protocol::ir::Role::User;
    let response = execute(RunInput {
        gateway: gateway.clone(),
        executor: Arc::new(executor),
        headers,
        envelope: RawEnvelope::new(
            Some(serde_json::json!({"model": model})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        ),
        request: AiRequest::new(model, vec![user]),
        ingress: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        context: RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        ),
    })
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    assert!(
        failed_calls.load(Ordering::SeqCst) >= 1,
        "first target must fail"
    );
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);
    {
        let turns = observations.lock();
        assert_eq!(turns.len(), 2, "failover stays inside one Model Turn");
        let (first, source, gaps) = &turns[0];
        assert_eq!(first.source_item_gap, 0);
        assert_eq!(source.items.len(), 1);
        assert_eq!(gaps, &[0, 1]);
        let (next, source, gaps) = &turns[1];
        assert_ne!(first.id, next.id);
        assert_eq!(first.source_admission_id, next.source_admission_id);
        assert_eq!(first.window_id, next.window_id);
        let gap = usize::try_from(next.source_item_gap).unwrap();
        assert_eq!(gap, 2, "boundary precedes the platform tool result");
        assert_eq!(
            source.items[gap].role,
            stravia_runtime_contract::protocol::ir::Role::Tool
        );
        assert_eq!(gaps, &(0..=source.items.len()).collect::<Vec<_>>());
    }
    close_test_gateway(gateway, directory).await;
}
