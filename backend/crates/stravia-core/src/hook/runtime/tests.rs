use tokio::sync::Mutex;

use super::*;
use crate::protocol::ir::AiItem;

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
        request_id: "req-1".into(),
        run_id: "run-1".into(),
        request_kind: kind,
        ingress: crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        transport: TransportKind::Http,
        principal: Principal::new("test-key"),
        cancellation: crate::proxy::context::CancellationToken::new(),
        inherited_media_turns: Vec::new(),
        response_id: None,
        previous_response_id: None,
    }
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
        egress: crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
    }
}

#[tokio::test]
async fn response_and_tool_result_stages_are_distinct_and_ordered() {
    let runtime = HookRuntime::new(vec![Arc::new(TestHook {
        descriptor: HookDescriptor::all("stages"),
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
        delta: &crate::protocol::ir::AiStreamDelta,
    ) -> Result<crate::hook::StreamDirective, String> {
        let crate::protocol::ir::AiStreamDelta::TextDelta(text) = delta else {
            return Ok(crate::hook::StreamDirective::Pass);
        };
        self.buffer.push_str(text);
        if self.buffer.ends_with('>') {
            self.buffer.clear();
            Ok(crate::hook::StreamDirective::Replace(vec![
                crate::protocol::ir::AiStreamDelta::TextDelta("redacted".into()),
            ]))
        } else {
            Ok(crate::hook::StreamDirective::Hold)
        }
    }

    fn flush(&mut self) -> Result<Vec<crate::protocol::ir::AiStreamDelta>, String> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![crate::protocol::ir::AiStreamDelta::TextDelta(
            std::mem::take(&mut self.buffer),
        )])
    }

    fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

struct DropEverythingTransformer;

impl StreamTransformer for DropEverythingTransformer {
    fn transform(
        &mut self,
        _delta: &crate::protocol::ir::AiStreamDelta,
    ) -> Result<crate::hook::StreamDirective, String> {
        Ok(crate::hook::StreamDirective::Drop)
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
        _delta: &crate::protocol::ir::AiStreamDelta,
    ) -> Result<crate::hook::StreamDirective, String> {
        assert!(self.0 != PanicStage::Transform, "transform panic");
        Ok(crate::hook::StreamDirective::Pass)
    }

    fn close(&mut self) -> Result<Vec<crate::protocol::ir::AiStreamDelta>, String> {
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
        let first =
            run.transform_stream(crate::protocol::ir::AiStreamDelta::TextDelta("text".into()));
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
        descriptor: HookDescriptor::all("delimiter"),
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
        .transform_stream(crate::protocol::ir::AiStreamDelta::TextDelta(
            "<secret".into(),
        ))
        .unwrap();
    let second = run
        .transform_stream(crate::protocol::ir::AiStreamDelta::TextDelta(">".into()))
        .unwrap();
    let flushed = run.flush_stream().unwrap();

    assert!(first.is_empty());
    assert!(matches!(
        second.as_slice(),
        [crate::protocol::ir::AiStreamDelta::TextDelta(text)] if text == "redacted"
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
        .transform_stream(crate::protocol::ir::AiStreamDelta::Done {
            stop_reason: "stop".into(),
        })
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
        .transform_stream(crate::protocol::ir::AiStreamDelta::TextDelta(
            "12345".into(),
        ))
        .unwrap_err();

    assert!(matches!(error, HookError::InvalidAction { .. }));
}

#[tokio::test]
async fn gateway_builder_registers_hooks_in_declared_order() {
    let storage: crate::storage::DynStorage =
        Arc::new(crate::storage::MemoryStorage::new(vec![], vec![], vec![]));
    let (gateway, _logs) = crate::Gateway::builder(crate::config::GatewayConfig::default())
        .storage(storage)
        .hook(Arc::new(TestHook {
            descriptor: HookDescriptor::all("first"),
            make: Arc::new(|| Box::new(ResponseStagesSession)),
        }))
        .hook(Arc::new(TestHook {
            descriptor: HookDescriptor::all("second"),
            make: Arc::new(|| Box::new(ResponseStagesSession)),
        }))
        .build()
        .await
        .unwrap();

    assert_eq!(
        gateway
            .hook_runtime()
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.id.as_str().to_string())
            .collect::<Vec<_>>(),
        [
            "first",
            "second",
            "web-search",
            "media-understanding-planner"
        ]
    );
}

struct ExposeToolSession;

#[async_trait]
impl HookSession for ExposeToolSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        if matches!(event, HookEvent::Request { .. }) {
            Ok(ActionBatch::one(HookAction::ExposeTool(
                crate::hook::ToolId::new("image-understanding"),
            )))
        } else {
            Ok(ActionBatch::default())
        }
    }
}

struct RuntimeEchoTool;

#[async_trait]
impl crate::hook::PlatformTool for RuntimeEchoTool {
    fn id(&self) -> crate::hook::ToolId {
        crate::hook::ToolId::new("image-understanding")
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
        _context: crate::hook::ToolExecutionContext,
    ) -> Result<serde_json::Value, crate::hook::PlatformToolError> {
        if arguments.get("fail").is_some() {
            Err(crate::hook::PlatformToolError::new("tool failed"))
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
    request.tools = Some(vec![crate::protocol::ir::ToolSpec {
        name: "stravia__understand_image".into(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        cache_control: None,
        meta: None,
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
        crate::protocol::ir::ToolCall {
            id: "platform-call".into(),
            name: platform_name,
            arguments: "{}".into(),
        },
        crate::protocol::ir::ToolCall {
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
    if let crate::protocol::ir::MessageContent::Blocks(blocks) = &mut candidate.items[0].content
        && let [
            crate::protocol::ir::ContentBlock::Reasoning {
                encrypted_content, ..
            },
        ] = blocks.as_mut_slice()
    {
        *encrypted_content = Some("changed".into());
    }
    assert!(validate_response_protected_fields(&original, &candidate).is_err());
}
