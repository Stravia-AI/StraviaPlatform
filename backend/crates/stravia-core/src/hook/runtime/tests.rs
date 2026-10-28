use async_trait::async_trait;
use serde_json::Value;
use stravia_runtime_contract::Principal;
use tokio::sync::Mutex;

use super::*;
use stravia_runtime_contract::protocol::ir::{AiItem, ContentBlock};

struct TestHook {
    descriptor: HookDescriptor,
    make: Arc<dyn Fn() -> Box<dyn HookSession> + Send + Sync>,
}

impl Hook for TestHook {
    fn descriptor(&self) -> HookDescriptor {
        self.descriptor.clone()
    }

    fn create_session(&self, _context: &SessionContext) -> Box<dyn HookSession> {
        (self.make)()
    }
}

struct AppendModelSession {
    suffix: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl HookSession for AppendModelSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        let HookEvent::Request { current, .. } = event else {
            return Ok(ActionBatch::default());
        };
        self.seen.lock().await.push(current.model.clone());
        Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
            RequestPatch::SetModel(format!("{}{}", current.model, self.suffix)),
        ))))
    }
}

struct InvalidBatchSession;

#[async_trait]
impl HookSession for InvalidBatchSession {
    async fn handle(&mut self, _event: HookEvent<'_>) -> Result<ActionBatch, String> {
        Ok(ActionBatch {
            actions: vec![
                HookAction::PatchRequest(Box::new(RequestPatch::SetModel("changed".into()))),
                HookAction::PatchResponse(ResponsePatch::SetContent("invalid".into())),
            ],
        })
    }
}

#[test]
#[should_panic(expected = "authenticated API key identity")]
fn anonymous_principal_cannot_be_constructed() {
    Principal::new("anonymous");
}

fn session_context(kind: RequestKind) -> SessionContext {
    SessionContext {
        tools_fixed: false,
        request_id: "req-1".into(),
        run_id: "run-1".into(),
        request_kind: kind,
        ingress: stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        transport: TransportKind::Http,
        principal: Principal::new("test-key"),
        cancellation: stravia_runtime_contract::CancellationToken::new(),
        inherited_media_turns: Vec::new(),
        response_id: None,
        previous_response_id: None,
    }
}

struct ControlledDeliveryTool {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl PlatformTool for ControlledDeliveryTool {
    fn id(&self) -> ToolId {
        ToolId::new("controlled-delivery")
    }
    fn external_name(&self) -> &str {
        "controlled_delivery"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn execution_policy(&self, _arguments: &Value) -> PlatformExecutionPolicy {
        PlatformExecutionPolicy::Bounded
    }
    async fn execute(
        &self,
        _arguments: Value,
        _context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(serde_json::json!({"unprocessed_success":true}))
    }
}

#[tokio::test]
async fn abandoned_inference_delivery_never_restores_unprocessed_success() {
    assert_abandoned_inference_delivery(false).await;
}

#[tokio::test]
async fn published_marker_cancellation_restores_concrete_cancelled_output_without_hook() {
    assert_abandoned_inference_delivery(true).await;
}

async fn assert_abandoned_inference_delivery(cancel: bool) {
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = HookRuntime::with_tools(
        Vec::new(),
        PlatformToolRegistry::new(vec![Arc::new(ControlledDeliveryTool {
            entered: entered.clone(),
            release: release.clone(),
        })])
        .unwrap(),
    );
    let context = session_context(RequestKind::Generation);
    let principal = context.principal.clone();
    let cancellation = context.cancellation.clone();
    let run = runtime
        .begin(
            context,
            &AiRequest::new("model", Vec::new()),
            ContextCompleteness::Full,
        )
        .unwrap();
    let execution = run.detached_platform_execution(
        PlatformToolCall {
            tool_id: ToolId::new("controlled-delivery"),
            call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "controlled-call".into(),
                name: "controlled_delivery".into(),
                arguments: "{}".into(),
            },
        },
        stravia_runtime_contract::CancellationToken::new(),
    );
    let (marker, execution) = gateway
        .prepare_marker_execution(&principal, execution, None, "turn".into())
        .await
        .unwrap();
    gateway
        .history_markers
        .publish(
            &principal,
            std::slice::from_ref(&marker.reference),
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
    let delivery = gateway.start_history_marker_executions(principal.clone(), vec![execution]);
    entered.notified().await;
    if cancel {
        cancellation.cancel();
    }
    drop(delivery);
    drop(run);
    if !cancel {
        release.notify_one();
    }
    let restored = gateway
        .history_markers
        .wait_terminal(&principal, &marker.reference)
        .await
        .unwrap()
        .unwrap();
    let Some(crate::history_marker::HiddenHistorySegment::Platform {
        call,
        result: ContentBlock::ToolResult {
            content, is_error, ..
        },
    }) = restored.segment
    else {
        panic!("published Marker must restore its call and delivery result");
    };
    assert_eq!(call.id.to_string(), "controlled-call");
    assert_eq!(is_error, Some(true));
    assert_eq!(
        content["error"]["code"],
        if cancel {
            "cancelled"
        } else {
            "delivery_processing_failed"
        }
    );
    assert!(content.get("unprocessed_success").is_none());
}

struct DeliveryResultSession(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl HookSession for DeliveryResultSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        let HookEvent::ToolResult { result, .. } = event else {
            return Ok(ActionBatch::default());
        };
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut content = result.content.clone();
        content["delivery_hook"] = serde_json::json!(true);
        Ok(ActionBatch::one(HookAction::PatchToolResult(
            ToolResultPatch::SetContent(content),
        )))
    }
}

#[tokio::test]
async fn marker_delivery_persistence_failure_returns_to_owning_inference_run() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    gateway.history_markers = Arc::new(crate::history_marker::SqlHistoryMarkerStore::sqlite(
        pool.clone(),
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = HookRuntime::with_tools(
        Vec::new(),
        PlatformToolRegistry::new(vec![Arc::new(ControlledDeliveryTool {
            entered: entered.clone(),
            release: release.clone(),
        })])
        .unwrap(),
    );
    let context = session_context(RequestKind::Generation);
    let principal = context.principal.clone();
    let mut run = runtime
        .begin(
            context,
            &AiRequest::new("model", Vec::new()),
            ContextCompleteness::Full,
        )
        .unwrap();
    run.set_route(route_context());
    let execution = run.detached_platform_execution(
        PlatformToolCall {
            tool_id: ToolId::new("controlled-delivery"),
            call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "failed-delivery-write".into(),
                name: "controlled_delivery".into(),
                arguments: "{}".into(),
            },
        },
        stravia_runtime_contract::CancellationToken::new(),
    );
    let (marker, execution) = gateway
        .prepare_marker_execution(&principal, execution, None, "turn".into())
        .await
        .unwrap();
    gateway
        .history_markers
        .publish(
            &principal,
            std::slice::from_ref(&marker.reference),
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_delivery_result BEFORE UPDATE OF segment_payload ON history_markers \
         WHEN NOT EXISTS (SELECT 1 FROM public_tool_executions WHERE marker_reference = OLD.reference) \
         BEGIN SELECT RAISE(ABORT, 'controlled delivery persistence failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let delivery = gateway.start_history_marker_executions(principal, vec![execution]);
    entered.notified().await;
    release.notify_one();
    let error = run.finish_marker_deliveries(delivery).await.unwrap_err();
    assert!(matches!(
        error,
        crate::history_marker::HistoryMarkerError::Storage(_)
    ));
}

#[tokio::test]
async fn marker_reference_delivery_is_fixed_after_background_completion_and_hook_runs_once() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_count = hooks.clone();
    let runtime = HookRuntime::with_tools(
        vec![Arc::new(TestHook {
            descriptor: HookDescriptor {
                event_kinds: vec![EventKind::ToolResult],
                ..HookDescriptor::all("delivery-result")
            },
            make: Arc::new(move || Box::new(DeliveryResultSession(hook_count.clone()))),
        })],
        PlatformToolRegistry::new(vec![
            Arc::new(ControlledDeliveryTool {
                entered: entered.clone(),
                release: release.clone(),
            }),
            Arc::new(crate::mcp::read::ReadTool::new(&gateway, &mut Vec::new()).unwrap()),
        ])
        .unwrap(),
    );
    let key = gateway
        .admin()
        .create_api_key(crate::db::models::CreateApiKey {
            key: None,
            name: "marker query owner".into(),
            rpm_limit: None,
            expires_at: None,
            mcp_access_enabled: true,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_generation: false,
            model_ids: vec![],
            inject_media_understanding: false,
            spend_limit: None,
        })
        .await
        .unwrap();
    let mut context = session_context(RequestKind::Generation);
    context.principal = Principal::new(key.id);
    let principal = context.principal.clone();
    let mut run = runtime
        .begin(
            context,
            &AiRequest::new("model", Vec::new()),
            ContextCompleteness::Full,
        )
        .unwrap();
    run.set_route(route_context());
    let execution = run.detached_platform_execution(
        PlatformToolCall {
            tool_id: ToolId::new("controlled-delivery"),
            call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "fixed-reference-call".into(),
                name: "controlled_delivery".into(),
                arguments: "{}".into(),
            },
        },
        stravia_runtime_contract::CancellationToken::new(),
    );
    let (marker, execution) = gateway
        .prepare_marker_execution(&principal, execution, None, "turn".into())
        .await
        .unwrap();
    gateway
        .history_markers
        .publish(
            &principal,
            std::slice::from_ref(&marker.reference),
            std::time::Duration::from_secs(3600),
        )
        .await
        .unwrap();
    let delivery = gateway.start_history_marker_executions(principal.clone(), vec![execution]);
    entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::time::resume();
    run.finish_marker_deliveries(delivery).await.unwrap();
    let delivered = gateway
        .history_markers
        .wait_terminal(&principal, &marker.reference)
        .await
        .unwrap()
        .unwrap()
        .segment
        .unwrap();
    let crate::history_marker::HiddenHistorySegment::Platform {
        result: ContentBlock::ToolResult { ref content, .. },
        ..
    } = delivered
    else {
        panic!("Marker must contain the current delivery");
    };
    let path = content["path"]
        .as_str()
        .expect("bounded delivery contains its execution reference");
    assert_eq!(content["delivery_hook"], true);
    let id = path.strip_prefix("stravia://tool-executions/").unwrap();
    let mut query_context = session_context(RequestKind::Generation);
    query_context.principal = principal.clone();
    let query_cancellation = query_context.cancellation.clone();
    let query_run = runtime
        .begin(
            query_context,
            &AiRequest::new("model", Vec::new()),
            ContextCompleteness::Full,
        )
        .unwrap();
    let query = query_run.detached_platform_execution(
        PlatformToolCall {
            tool_id: ToolId::new("stravia-read"),
            call: stravia_runtime_contract::protocol::ir::ToolCall {
                id: "independent-query-call".into(),
                name: "StraviaRead".into(),
                arguments: serde_json::json!({"path":format!("{path}?wait=120")}).to_string(),
            },
        },
        stravia_runtime_contract::CancellationToken::new(),
    );
    let waiting = Arc::new(tokio::sync::Notify::new());
    let registered = waiting.clone();
    let query_gateway = gateway.clone();
    let mut query = tokio::spawn(async move {
        crate::tool_execution::QUERY_WAITING
            .scope(registered, query.execute(&query_gateway))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            _ = waiting.notified() => {},
            result = &mut query => panic!("query returned before waiting: {}", result.unwrap().result.content),
        }
    })
    .await
    .expect("query must register its waiter");
    query_cancellation.cancel();
    let cancelled_query = tokio::time::timeout(std::time::Duration::from_secs(10), query)
        .await
        .expect("disconnecting the query must remove its waiter")
        .unwrap();
    assert!(cancelled_query.result.is_error);
    assert_eq!(
        gateway
            .query_tool_execution(
                &principal,
                id,
                std::time::Duration::ZERO,
                &stravia_runtime_contract::CancellationToken::new(),
            )
            .await
            .unwrap()["status"],
        "running",
    );
    release.notify_one();
    let completed = gateway
        .query_tool_execution(
            &principal,
            id,
            std::time::Duration::from_secs(60),
            &stravia_runtime_contract::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(
        completed["result"],
        serde_json::json!({"unprocessed_success":true})
    );
    let restored = gateway
        .history_markers
        .wait_terminal(&principal, &marker.reference)
        .await
        .unwrap()
        .unwrap()
        .segment
        .unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(delivered).unwrap()
    );
    assert_eq!(hooks.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_hooks_run_in_order_and_observe_prior_changes() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hooks = ["-a", "-b"]
        .into_iter()
        .map(|suffix| {
            let seen = seen.clone();
            Arc::new(TestHook {
                descriptor: HookDescriptor::all(suffix),
                make: Arc::new(move || {
                    Box::new(AppendModelSession {
                        suffix,
                        seen: seen.clone(),
                    })
                }),
            }) as Arc<dyn Hook>
        })
        .collect();
    let runtime = HookRuntime::new(hooks);
    let mut request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();

    let control = run.on_request(&mut request).await.unwrap();

    assert!(matches!(control, HookControl::Continue));
    assert_eq!(request.model, "model-a-b");
    assert_eq!(seen.lock().await.as_slice(), ["model", "model-a"]);
}

#[tokio::test]
async fn invalid_action_batch_leaves_request_unchanged() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor::all("invalid"),
        make: Arc::new(|| Box::new(InvalidBatchSession)),
    })]);
    let mut request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();

    let error = run.on_request(&mut request).await.unwrap_err();

    assert!(matches!(error, HookError::InvalidAction { .. }));
    assert_eq!(request.model, "model");
}

#[tokio::test]
async fn hook_requiring_full_context_is_skipped_for_partial_request() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(TestHook {
        descriptor: HookDescriptor {
            requires_full_context: true,
            ..HookDescriptor::all("full-only")
        },
        make: {
            let calls = calls.clone();
            Arc::new(move || {
                Box::new(AppendModelSession {
                    suffix: "-changed",
                    seen: calls.clone(),
                })
            })
        },
    });
    let runtime = HookRuntime::new(vec![hook]);
    let mut request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Partial {
                opaque_refs: vec![],
            },
        )
        .unwrap();

    run.on_request(&mut request).await.unwrap();

    assert_eq!(request.model, "model");
    assert!(calls.lock().await.is_empty());
}

struct ResponseStagesSession;

struct CompleteContextSession;

#[async_trait]
impl HookSession for CompleteContextSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        if matches!(event, HookEvent::Request { .. }) {
            Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                RequestPatch::SetParameters(Default::default()),
            ))))
        } else {
            Ok(ActionBatch::default())
        }
    }
}

struct ObserveCompletedContextSession;

#[async_trait]
impl HookSession for ObserveCompletedContextSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        let HookEvent::Request {
            original,
            current,
            context,
            ..
        } = event
        else {
            return Ok(ActionBatch::default());
        };
        assert!(matches!(
            original.completeness,
            ContextCompleteness::Partial { .. }
        ));
        assert_eq!(context.completeness, ContextCompleteness::Full);
        let mut original_request = AiRequest::new("model", Vec::new());
        original.write_to_request(&mut original_request);
        let mut current_request = AiRequest::new("model", Vec::new());
        context.write_to_request(&mut current_request);
        assert_eq!(
            serde_json::to_value(&original_request.items).unwrap(),
            serde_json::to_value(&current.items).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&current_request.items).unwrap(),
            serde_json::to_value(&current.items).unwrap()
        );
        assert_eq!(original_request.instructions, current.instructions);
        assert_eq!(current_request.instructions, current.instructions);
        Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
            RequestPatch::SetModel("observed-complete-context".into()),
        ))))
    }
}

#[tokio::test]
async fn completing_partial_context_enables_later_full_context_consumers() {
    let runtime = HookRuntime::new(vec![
        Arc::new(TestHook {
            descriptor: HookDescriptor::all("complete"),
            make: Arc::new(|| Box::new(CompleteContextSession)),
        }),
        Arc::new(TestHook {
            descriptor: HookDescriptor {
                requires_full_context: true,
                ..HookDescriptor::all("observe-complete")
            },
            make: Arc::new(|| Box::new(ObserveCompletedContextSession)),
        }),
    ]);
    let mut request = AiRequest::new("model", vec![AiItem::output_text("full visible context")]);
    request.instructions = Some("preserved system".into());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Partial {
                opaque_refs: vec![],
            },
        )
        .unwrap();

    run.on_request(&mut request).await.unwrap();

    assert_eq!(request.model, "observed-complete-context");
}

struct InvalidCompletionSession;

#[async_trait]
impl HookSession for InvalidCompletionSession {
    async fn handle(&mut self, _event: HookEvent<'_>) -> Result<ActionBatch, String> {
        Ok(ActionBatch {
            actions: vec![
                HookAction::PatchRequest(Box::new(RequestPatch::SetParameters(Default::default()))),
                HookAction::PatchResponse(ResponsePatch::SetContent("invalid".into())),
            ],
        })
    }
}

#[tokio::test]
async fn failed_context_completion_does_not_enable_full_context_response_hooks() {
    let runtime = HookRuntime::new(vec![
        Arc::new(TestHook {
            descriptor: HookDescriptor {
                event_kinds: vec![EventKind::Request],
                ..HookDescriptor::all("invalid-completion")
            },
            make: Arc::new(|| Box::new(InvalidCompletionSession)),
        }),
        Arc::new(TestHook {
            descriptor: HookDescriptor {
                event_kinds: vec![EventKind::UpstreamResponse],
                requires_full_context: true,
                ..HookDescriptor::all("full-response")
            },
            make: Arc::new(|| Box::new(ResponseStagesSession)),
        }),
    ]);
    let mut request = AiRequest::new("model", Vec::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Partial {
                opaque_refs: vec![],
            },
        )
        .unwrap();
    assert!(matches!(
        run.on_request(&mut request).await,
        Err(HookError::InvalidAction { .. })
    ));
    run.set_route(route_context());
    let mut response = AiResponse::new("response", "model");
    response.push_output_text("unchanged");

    run.on_upstream_response(&request, &mut response)
        .await
        .unwrap();

    assert_eq!(response.output_text(), "unchanged");
}

#[tokio::test]
async fn no_hooks_preserve_route_errors_and_client_tool_ownership() {
    let runtime = HookRuntime::new(Vec::new());
    let mut request = AiRequest::new("model", vec![AiItem::output_text("visible context")]);
    request.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "StraviaRead".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        defer_loading: None,
    }]);
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    assert!(matches!(
        run.on_request(&mut request).await.unwrap(),
        HookControl::Continue
    ));
    let mut response = AiResponse::new("response", "model");
    response.extend_tool_calls(vec![tool_call("client-read", "StraviaRead", "{}")]);
    assert!(matches!(
        run.on_upstream_response(&request, &mut response).await,
        Err(HookError::Runtime { .. })
    ));
    assert!(matches!(
        run.on_client_output(&mut response).await,
        Err(HookError::Runtime { .. })
    ));
    run.set_route(route_context());
    run.next_round();
    assert!(matches!(
        run.on_upstream_response(&request, &mut response)
            .await
            .unwrap(),
        HookControl::Continue
    ));
    let classified = run.classify_tool_calls(&response);
    assert!(classified.platform.is_empty());
    assert_eq!(classified.client[0].name, "StraviaRead");
    let transformed = run
        .transform_stream(AiStreamDelta::TextDelta("untouched".into()))
        .unwrap();
    assert!(matches!(
        transformed.as_slice(),
        [AiStreamDelta::TextDelta(text)] if text == "untouched"
    ));
    assert!(run.flush_stream().unwrap().is_empty());
    assert!(run.flush_stream().unwrap().is_empty());
}

#[async_trait]
impl HookSession for ResponseStagesSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        match event {
            HookEvent::UpstreamResponse { response, .. } => {
                Ok(ActionBatch::one(HookAction::PatchResponse(
                    ResponsePatch::SetContent(format!("{}-upstream", response.output_text())),
                )))
            }
            HookEvent::ClientOutput { response, .. } => {
                Ok(ActionBatch::one(HookAction::PatchResponse(
                    ResponsePatch::SetContent(format!("{}-client", response.output_text())),
                )))
            }
            HookEvent::ToolResult { .. } => Ok(ActionBatch::one(HookAction::PatchToolResult(
                ToolResultPatch::SetContent(serde_json::json!("redacted")),
            ))),
            HookEvent::Request { .. } => Ok(ActionBatch::default()),
        }
    }
}

struct ProtectedResponseMutationSession;

#[async_trait]
impl HookSession for ProtectedResponseMutationSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        let HookEvent::UpstreamResponse { response, .. } = event else {
            return Ok(ActionBatch::default());
        };
        let mut replacement = response.clone();
        replacement.replace_output_text("changed");
        replacement.usage.prompt_tokens = 999;
        Ok(ActionBatch::one(HookAction::PatchResponse(
            ResponsePatch::ReplaceCanonical(Box::new(replacement)),
        )))
    }
}

fn route_context() -> RouteContext {
    RouteContext {
        model_id: "model-id".into(),
        provider_id: "provider-id".into(),
        target_id: "target-id".into(),
        egress: Some(
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        ),
    }
}

#[tokio::test]
async fn response_and_tool_result_stages_are_distinct_and_ordered() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor {
            event_kinds: vec![
                EventKind::UpstreamResponse,
                EventKind::ToolResult,
                EventKind::ClientOutput,
            ],
            ..HookDescriptor::all("stages")
        },
        make: Arc::new(|| Box::new(ResponseStagesSession)),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    run.set_route(route_context());
    let mut response = AiResponse::new("response", "model");
    response.push_output_text("text");
    let mut result = PlatformToolResult {
        tool_id: ToolId::new("tool"),
        call_id: "call".into(),
        content: serde_json::json!("raw"),
        content_kind: stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
        is_error: false,
        metadata: serde_json::Map::new(),
    };

    run.on_upstream_response(&request, &mut response)
        .await
        .unwrap();
    run.on_tool_result(&mut result).await.unwrap();
    run.on_client_output(&mut response).await.unwrap();

    assert_eq!(response.output_text(), "text-upstream-client");
    assert_eq!(result.content, serde_json::json!("redacted"));
}

#[tokio::test]
async fn protected_response_fields_make_the_whole_batch_fail() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor::all("protected"),
        make: Arc::new(|| Box::new(ProtectedResponseMutationSession)),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    run.set_route(route_context());
    let mut response = AiResponse::new("response", "model");
    response.push_output_text("original");
    response.usage.prompt_tokens = 1;

    let error = run
        .on_upstream_response(&request, &mut response)
        .await
        .unwrap_err();

    assert!(matches!(error, HookError::InvalidAction { .. }));
    assert_eq!(response.output_text(), "original");
    assert_eq!(response.usage.prompt_tokens, 1);
}

struct TransformSession {
    transformer: Box<dyn StreamTransformer>,
}

#[async_trait]
impl HookSession for TransformSession {
    async fn handle(&mut self, _event: HookEvent<'_>) -> Result<ActionBatch, String> {
        Ok(ActionBatch::default())
    }

    fn stream_transformer(&mut self) -> Option<&mut dyn StreamTransformer> {
        Some(self.transformer.as_mut())
    }
}

struct DelimiterTransformer {
    buffer: String,
}

impl StreamTransformer for DelimiterTransformer {
    fn transform(
        &mut self,
        delta: &stravia_runtime_contract::protocol::ir::AiStreamDelta,
    ) -> Result<stravia_runtime_contract::hook::StreamDirective, String> {
        let stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(text) = delta else {
            return Ok(stravia_runtime_contract::hook::StreamDirective::Pass);
        };
        self.buffer.push_str(text);
        if self.buffer.ends_with('>') {
            self.buffer.clear();
            Ok(stravia_runtime_contract::hook::StreamDirective::Replace(
                vec![
                    stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(
                        "redacted".into(),
                    ),
                ],
            ))
        } else {
            Ok(stravia_runtime_contract::hook::StreamDirective::Hold)
        }
    }

    fn flush(
        &mut self,
    ) -> Result<Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta>, String> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![
            stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(std::mem::take(
                &mut self.buffer,
            )),
        ])
    }

    fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

struct DropEverythingTransformer;

impl StreamTransformer for DropEverythingTransformer {
    fn transform(
        &mut self,
        _delta: &stravia_runtime_contract::protocol::ir::AiStreamDelta,
    ) -> Result<stravia_runtime_contract::hook::StreamDirective, String> {
        Ok(stravia_runtime_contract::hook::StreamDirective::Drop)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanicStage {
    Begin,
    Transform,
    Close,
    BufferedBytes,
}

struct PanickingTransformer(PanicStage);

impl StreamTransformer for PanickingTransformer {
    fn begin(&mut self) -> Result<(), String> {
        assert!(self.0 != PanicStage::Begin, "begin panic");
        Ok(())
    }

    fn transform(
        &mut self,
        _delta: &stravia_runtime_contract::protocol::ir::AiStreamDelta,
    ) -> Result<stravia_runtime_contract::hook::StreamDirective, String> {
        assert!(self.0 != PanicStage::Transform, "transform panic");
        Ok(stravia_runtime_contract::hook::StreamDirective::Pass)
    }

    fn close(
        &mut self,
    ) -> Result<Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta>, String> {
        assert!(self.0 != PanicStage::Close, "close panic");
        Ok(Vec::new())
    }

    fn buffered_bytes(&self) -> usize {
        assert!(self.0 != PanicStage::BufferedBytes, "buffered_bytes panic");
        0
    }
}

#[test]
fn stream_transformer_panics_are_fail_closed() {
    for stage in [
        PanicStage::Begin,
        PanicStage::Transform,
        PanicStage::Close,
        PanicStage::BufferedBytes,
    ] {
        let runtime = HookRuntime::new(vec![Arc::new(TestHook {
            descriptor: HookDescriptor::all("panicking-transformer"),
            make: Arc::new(move || {
                Box::new(TransformSession {
                    transformer: Box::new(PanickingTransformer(stage)),
                })
            }),
        })]);
        let request = AiRequest::new("model", Vec::<AiItem>::new());
        let mut run = runtime
            .begin(
                session_context(RequestKind::Generation),
                &request,
                ContextCompleteness::Full,
            )
            .unwrap();
        let first = run.transform_stream(
            stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta("text".into()),
        );
        let error = match stage {
            PanicStage::Begin | PanicStage::Transform | PanicStage::BufferedBytes => {
                first.unwrap_err()
            }
            PanicStage::Close => {
                first.unwrap();
                run.flush_stream().unwrap_err()
            }
        };

        assert!(matches!(error, HookError::Failed { .. }));
    }
}

#[tokio::test]
async fn stream_transformer_holds_across_deltas_and_flushes_semantic_content() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor {
            event_kinds: vec![EventKind::Stream],
            ..HookDescriptor::all("delimiter")
        },
        make: Arc::new(|| {
            Box::new(TransformSession {
                transformer: Box::new(DelimiterTransformer {
                    buffer: String::new(),
                }),
            })
        }),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();

    let first = run
        .transform_stream(
            stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta("<secret".into()),
        )
        .unwrap();
    let second = run
        .transform_stream(
            stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(">".into()),
        )
        .unwrap();
    let flushed = run.flush_stream().unwrap();

    assert!(first.is_empty());
    assert!(matches!(
        second.as_slice(),
        [stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(text)] if text == "redacted"
    ));
    assert!(flushed.is_empty());
}

#[test]
fn stream_replacement_preserves_dated_output_coordinates() {
    let source = AiStreamDelta::TextDeltaWithMetadata {
        text: "secret".into(),
        logprobs: Vec::new(),
        obfuscation: None,
        output_index: Some(4),
        content_index: Some(2),
    };
    let mut replacement = vec![AiStreamDelta::TextDelta("redacted".into())];

    assert!(preserve_stream_coordinates(&source, &mut replacement));
    assert!(matches!(
        replacement.as_slice(),
        [AiStreamDelta::TextDeltaWithMetadata {
            text,
            output_index: Some(4),
            content_index: Some(2),
            ..
        }] if text == "redacted"
    ));
}

#[test]
fn reasoning_replacement_preserves_dated_output_coordinates_and_kind() {
    let summary = AiStreamDelta::ReasoningSummaryDelta {
        text: "secret".into(),
        obfuscation: None,
        output_index: Some(4),
        content_index: Some(2),
    };
    let mut replacement = vec![AiStreamDelta::ThinkingDelta("redacted".into())];

    assert!(preserve_stream_coordinates(&summary, &mut replacement));
    assert!(matches!(
        replacement.as_slice(),
        [AiStreamDelta::ReasoningSummaryDelta {
            text,
            output_index: Some(4),
            content_index: Some(2),
            ..
        }] if text == "redacted"
    ));
    assert_eq!(
        semantic_variant(&replacement[0]),
        Some(SemanticVariant::ReasoningSummary)
    );
}

#[tokio::test]
async fn stream_transformer_cannot_drop_structural_events() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor::all("drop"),
        make: Arc::new(|| {
            Box::new(TransformSession {
                transformer: Box::new(DropEverythingTransformer),
            })
        }),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();

    let error = run
        .transform_stream(
            stravia_runtime_contract::protocol::ir::AiStreamDelta::Done {
                stop_reason: "stop".into(),
            },
        )
        .unwrap_err();

    assert!(matches!(error, HookError::InvalidAction { .. }));
}

#[tokio::test]
async fn stream_transformer_is_rejected_when_its_buffer_exceeds_descriptor_limit() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor {
            max_buffered_bytes: 4,
            ..HookDescriptor::all("bounded")
        },
        make: Arc::new(|| {
            Box::new(TransformSession {
                transformer: Box::new(DelimiterTransformer {
                    buffer: String::new(),
                }),
            })
        }),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();

    let error = run
        .transform_stream(
            stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta("12345".into()),
        )
        .unwrap_err();

    assert!(matches!(error, HookError::InvalidAction { .. }));
}

struct ExposeToolSession;

#[async_trait]
impl HookSession for ExposeToolSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        if matches!(event, HookEvent::Request { .. }) {
            Ok(ActionBatch::one(HookAction::ExposeTool(
                stravia_runtime_contract::hook::ToolId::new("image-understanding"),
            )))
        } else {
            Ok(ActionBatch::default())
        }
    }
}

struct RuntimeEchoTool;

#[async_trait]
impl stravia_runtime_contract::hook::PlatformTool for RuntimeEchoTool {
    fn id(&self) -> stravia_runtime_contract::hook::ToolId {
        stravia_runtime_contract::hook::ToolId::new("image-understanding")
    }

    fn external_name(&self) -> &str {
        "understand_image"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _context: stravia_runtime_contract::hook::ToolExecutionContext,
    ) -> Result<serde_json::Value, stravia_runtime_contract::hook::PlatformToolError> {
        let Some(object) = arguments.as_object() else {
            return Err(stravia_runtime_contract::hook::PlatformToolError::new(
                "arguments must be an object",
            ));
        };
        if object.contains_key("fail") {
            Err(stravia_runtime_contract::hook::PlatformToolError::new(
                "tool failed",
            ))
        } else {
            Ok(arguments)
        }
    }
}

#[tokio::test]
async fn exposed_platform_tool_is_classified_without_claiming_client_tool() {
    let registry = crate::hook::PlatformToolRegistry::new(vec![Arc::new(RuntimeEchoTool)]).unwrap();
    let runtime = HookRuntime::with_tools(
        vec![Arc::new(TestHook {
            descriptor: HookDescriptor::all("expose"),
            make: Arc::new(|| Box::new(ExposeToolSession)),
        })],
        registry,
    );
    let mut request = AiRequest::new("model", Vec::<AiItem>::new());
    request.tools = Some(vec![stravia_runtime_contract::protocol::ir::ToolSpec {
        name: "stravia__understand_image".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        defer_loading: None,
    }]);
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    run.on_request(&mut request).await.unwrap();
    let platform_name = request
        .tools
        .as_ref()
        .unwrap()
        .iter()
        .find(|tool| tool.name != "stravia__understand_image")
        .unwrap()
        .name
        .clone();
    let mut response = AiResponse::new("response", "model");
    response.extend_tool_calls(vec![
        stravia_runtime_contract::protocol::ir::ToolCall {
            id: "platform-call".into(),
            name: platform_name,
            arguments: "{}".into(),
        },
        stravia_runtime_contract::protocol::ir::ToolCall {
            id: "client-call".into(),
            name: "stravia__understand_image".into(),
            arguments: "{}".into(),
        },
    ]);

    let classified = run.classify_tool_calls(&response);

    assert_eq!(classified.platform.len(), 1);
    assert_eq!(classified.platform[0].call.id, "platform-call");
    assert_eq!(classified.client.len(), 1);
    assert_eq!(classified.client[0].id, "client-call");
    assert!(request.items.is_empty());
    assert_eq!(run.round, 0);
}
#[test]
fn session_creation_panic_is_fail_closed() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor::all("panic"),
        make: Arc::new(|| panic!("boom")),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());

    let error = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .err()
        .expect("session creation should fail");

    assert!(error.to_string().contains("session creation panicked"));
}
#[test]
fn reasoning_patch_updates_typed_item_and_protects_encrypted_content() {
    let mut original = AiResponse::new("response", "model");
    original.items.push(AiItem::reasoning(
        vec!["summary".into()],
        vec!["provider reasoning".into()],
        Some("opaque".into()),
    ));
    let mut candidate = original.clone();

    apply_response_patch(
        &mut candidate,
        ResponsePatch::SetReasoning(Some("hook reasoning".into())),
    )
    .expect("reasoning patch");

    assert_eq!(candidate.items.len(), 1);
    assert_eq!(
        candidate.items[0].reasoning_ref(),
        Some((
            ["summary".to_owned()].as_slice(),
            ["hook reasoning".to_owned()].as_slice(),
            Some("opaque")
        ))
    );
    validate_response_protected_fields(&original, &candidate)
        .expect("encrypted content remains unchanged");
    if let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
        &mut candidate.items[0].content
        && let [
            stravia_runtime_contract::protocol::ir::ContentBlock::Reasoning {
                encrypted_content,
                ..
            },
        ] = blocks.as_mut_slice()
    {
        *encrypted_content = Some("changed".into());
    }
    assert!(validate_response_protected_fields(&original, &candidate).is_err());
}

struct NoopResponseSession;

#[async_trait]
impl HookSession for NoopResponseSession {
    async fn handle(&mut self, _event: HookEvent<'_>) -> Result<ActionBatch, String> {
        Ok(ActionBatch::default())
    }
}

fn tool_call(
    id: &str,
    name: &str,
    arguments: &str,
) -> stravia_runtime_contract::protocol::ir::ToolCall {
    stravia_runtime_contract::protocol::ir::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[tokio::test]
async fn client_tool_arguments_pass_through_verbatim_even_when_not_json() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor::all("observe"),
        make: Arc::new(|| Box::new(NoopResponseSession)),
    })]);
    let request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    run.set_route(route_context());
    let mut response = AiResponse::new("response", "model");
    response.extend_tool_calls(vec![
        tool_call("call-truncated", "local_probe", "{\"path\":\"/tmp/x\""),
        tool_call("call-empty", "local_probe", ""),
    ]);

    run.on_upstream_response(&request, &mut response)
        .await
        .expect("client tool arguments are opaque to the platform");
    run.on_client_output(&mut response)
        .await
        .expect("client tool arguments are opaque to the platform");

    let arguments = response
        .tool_calls()
        .map(|call| call.arguments.as_str())
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["{\"path\":\"/tmp/x\"", ""]);
}

#[tokio::test]
async fn valid_platform_call_does_not_block_malformed_client_arguments() {
    let registry = crate::hook::PlatformToolRegistry::new(vec![Arc::new(RuntimeEchoTool)]).unwrap();
    let runtime = HookRuntime::with_tools(
        vec![Arc::new(TestHook {
            descriptor: HookDescriptor::all("expose"),
            make: Arc::new(|| Box::new(ExposeToolSession)),
        })],
        registry,
    );
    let mut request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    run.on_request(&mut request).await.unwrap();
    run.set_route(route_context());
    let platform_name = request
        .tools
        .as_ref()
        .expect("exposed tool spec")
        .iter()
        .map(|tool| tool.name.clone())
        .next()
        .expect("one exposed tool");
    let mut response = AiResponse::new("response", "model");
    response.extend_tool_calls(vec![
        tool_call("platform-call", &platform_name, "{}"),
        tool_call("client-call", "local_probe", "{\"path\":\"/tmp/x\""),
    ]);

    run.on_upstream_response(&request, &mut response)
        .await
        .expect("valid platform call may not fail client argument passthrough");

    let client_arguments = response
        .tool_calls()
        .find(|call| call.id.as_str() == "client-call")
        .map(|call| call.arguments.as_str());
    assert_eq!(client_arguments, Some("{\"path\":\"/tmp/x\""));
}

#[tokio::test]
async fn platform_tool_malformed_arguments_are_refused_at_execution() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let registry = crate::hook::PlatformToolRegistry::new(vec![Arc::new(RuntimeEchoTool)]).unwrap();
    let runtime = HookRuntime::with_tools(
        vec![Arc::new(TestHook {
            descriptor: HookDescriptor::all("expose"),
            make: Arc::new(|| Box::new(ExposeToolSession)),
        })],
        registry,
    );
    let mut request = AiRequest::new("model", Vec::<AiItem>::new());
    let mut run = runtime
        .begin(
            session_context(RequestKind::Generation),
            &request,
            ContextCompleteness::Full,
        )
        .unwrap();
    run.on_request(&mut request).await.unwrap();
    run.set_route(route_context());
    let platform_name = request
        .tools
        .as_ref()
        .expect("exposed tool spec")
        .iter()
        .map(|tool| tool.name.clone())
        .next()
        .expect("one exposed tool");
    let mut response = AiResponse::new("response", "model");
    response.extend_tool_calls(vec![tool_call(
        "platform-call",
        &platform_name,
        "{\"path\":",
    )]);

    // 非法参数不失败响应阶段：拒绝发生在平台执行边界，以 is_error 工具结果返回。
    run.on_upstream_response(&request, &mut response)
        .await
        .expect("argument errors must not fail the response stages");

    let classified = run.classify_tool_calls(&response);
    assert_eq!(classified.platform.len(), 1);
    let result = run
        .detached_platform_execution(
            classified
                .platform
                .into_iter()
                .next()
                .expect("platform call"),
            stravia_runtime_contract::CancellationToken::new(),
        )
        .execute(&gateway)
        .await
        .result;

    assert!(result.is_error, "malformed arguments must refuse execution");
    assert_eq!(result.call_id, "platform-call");
    assert!(
        result
            .content
            .as_str()
            .expect("error content is a string")
            .contains("invalid tool arguments"),
        "refusal must explain the argument error: {}",
        result.content
    );
}
