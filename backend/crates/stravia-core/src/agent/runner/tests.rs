use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;

use super::*;
use stravia_runtime_contract::agent::AgentBudgets;
use stravia_runtime_contract::agent::AgentDefinitionConfig;
use stravia_runtime_contract::agent::AgentDefinitionSpec;
use stravia_runtime_contract::agent::AgentSlug;
use stravia_runtime_contract::agent::ArtifactPolicy;

struct EchoTool;

#[async_trait]
impl AgentTool for EchoTool {
    fn id(&self) -> VersionedToolId {
        VersionedToolId {
            id: "echo".into(),
            version: 1,
        }
    }

    fn description(&self) -> &str {
        "Echo an input value"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _context: AgentToolContext,
        input: Value,
    ) -> Result<crate::agent::AgentToolOutput, super::super::AgentToolError> {
        Ok(crate::agent::AgentToolOutput {
            content: input,
            content_kind: stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
        })
    }
}

struct BlockOutputTool(Vec<ContentBlock>);

#[async_trait]
impl stravia_runtime_contract::hook::PlatformTool for BlockOutputTool {
    fn id(&self) -> ToolId {
        ToolId::new("echo")
    }

    fn external_name(&self) -> &str {
        "echo"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _arguments: Value,
        _context: stravia_runtime_contract::hook::ToolExecutionContext,
    ) -> Result<Value, stravia_runtime_contract::hook::PlatformToolError> {
        Err(stravia_runtime_contract::hook::PlatformToolError::new(
            "use typed output",
        ))
    }

    async fn execute_blocks(
        &self,
        _arguments: Value,
        _context: stravia_runtime_contract::hook::ToolExecutionContext,
    ) -> Result<Vec<ContentBlock>, stravia_runtime_contract::hook::PlatformToolError> {
        Ok(self.0.clone())
    }
}

async fn platform_output_roundtrip(blocks: Vec<ContentBlock>) -> (AiRequest, PlatformToolResult) {
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::new(crate::config::GatewayConfig {
        data_dir: directory.path().to_owned(),
        ..Default::default()
    })
    .await
    .unwrap();
    let tool = Arc::new(BlockOutputTool(blocks));
    let registry = crate::hook::PlatformToolRegistry::new(vec![tool.clone()]).unwrap();
    let ordinary = registry
        .execute(
            &gateway,
            &ToolId::new("echo"),
            "call-1".into(),
            serde_json::json!({}),
            stravia_runtime_contract::hook::ToolExecutionContext {
                request_id: "request".into(),
                run_id: "run".into(),
                principal: Principal::new("owner"),
                read_scope: stravia_runtime_contract::hook::ReadExposureScope::NONE,
                cancellation: CancellationToken::new(),
                progress: None,
            },
            crate::tool_execution::ExecutionSource::Mcp,
        )
        .await
        .result;
    let mut tool_response = AiResponse::new("response-1", "model-1");
    tool_response.extend_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: "{}".into(),
    }]);
    tool_response.stop_reason = Some("tool_calls".into());
    let mut final_response = AiResponse::new("response-2", "model-1");
    final_response.push_output_text(r#"{"answer":"done"}"#);
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        tool_response,
        final_response,
    ]));
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(crate::agent::PlatformToolAgentAdapter::new(
            tool, 1,
        ))],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .unwrap();
    let events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    assert!(
        matches!(events.last(), Some(AgentEvent::Completed(_))),
        "{events:?}"
    );
    let mut requests = model.requests();
    assert_eq!(requests.len(), 2);
    (requests.remove(1), ordinary)
}

fn agent_tool_payload(request: &AiRequest) -> (&Value, Option<bool>) {
    let message = request
        .items
        .iter()
        .find(|item| item.role == Role::Tool)
        .unwrap();
    let MessageContent::Blocks(blocks) = &message.content else {
        panic!("expected tool result blocks");
    };
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &blocks[0]
    else {
        panic!("expected tool result");
    };
    (content, *is_error)
}

#[tokio::test]
async fn platform_agent_typed_media_preserves_opaque_secret_and_redacts_readable_text() {
    const SECRET: &str = "ghp_8Dq7mP2vL9sX4aR6tK3nF5wH1jB0cYzUeIoG";
    let (request, _) = platform_output_roundtrip(vec![
        ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/png".into(),
                data: SECRET.into(),
            },
            detail: None,
            cache_control: None,
        },
        ContentBlock::Text {
            text: format!("credential: {SECRET}").into(),
            cache_control: None,
        },
        ContentBlock::Document {
            source: stravia_runtime_contract::protocol::ir::DocumentSource::Blocks {
                content: vec![ContentBlock::Text {
                    text: SECRET.to_owned().into(),
                    cache_control: None,
                }],
            },
            title: None,
            context: None,
            cache_control: None,
        },
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .storage(std::sync::Arc::new(crate::storage::MemoryStorage::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )))
    .build()
    .await
    .unwrap();
    gateway
        .admin()
        .set_setting(stravia_credential_protection::SETTING_KEY, "true")
        .await
        .unwrap();
    gateway
        .redaction
        .mappings
        .intern(&Principal::new("owner"), &[SECRET.into()])
        .await
        .unwrap();
    let mut request = std::sync::Arc::new(request);
    gateway
        .redaction
        .protect(&Principal::new("owner"), &mut request, None)
        .await
        .unwrap();
    let (content, _) = agent_tool_payload(&request);
    assert_eq!(content[0]["source"]["data"], SECRET);
    let text = content[1]["text"].as_str().unwrap();
    assert!(!text.contains(SECRET));
    assert!(stravia_credential_protection::marker::valid_reference(
        text.strip_prefix("credential: ").unwrap()
    ));
    assert_eq!(
        content[2]["source"]["content"][0]["text"],
        text.strip_prefix("credential: ").unwrap()
    );
}

#[tokio::test]
async fn platform_agent_business_json_and_single_text_remain_readable_payloads() {
    const SECRET: &str = "ghp_8Dq7mP2vL9sX4aR6tK3nF5wH1jB0cYzUeIoG";
    let directory = tempfile::tempdir().unwrap();
    let gateway = crate::Gateway::builder(crate::config::GatewayConfig {
        data_dir: directory.path().to_path_buf(),
        ..Default::default()
    })
    .storage(std::sync::Arc::new(crate::storage::MemoryStorage::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )))
    .build()
    .await
    .unwrap();
    gateway
        .admin()
        .set_setting(stravia_credential_protection::SETTING_KEY, "true")
        .await
        .unwrap();
    let mappings = gateway
        .redaction
        .mappings
        .intern(&Principal::new("owner"), &[SECRET.into()])
        .await
        .unwrap();
    let json = serde_json::json!([{"type": "image", "source": {"data": SECRET}}]);
    for (block, expected) in [
        (ContentBlock::Unknown { raw: json.clone() }, json),
        (
            ContentBlock::Text {
                text: std::sync::Arc::new(SECRET.into()),
                cache_control: None,
            },
            Value::String(SECRET.into()),
        ),
    ] {
        let (request, _) = platform_output_roundtrip(vec![block]).await;
        let mut request = std::sync::Arc::new(request);
        gateway
            .redaction
            .protect(&Principal::new("owner"), &mut request, None)
            .await
            .unwrap();
        let protected: Value = serde_json::from_str(
            &expected
                .to_string()
                .replace(SECRET, &mappings.mappings[0].reference),
        )
        .unwrap();
        assert_eq!(agent_tool_payload(&request), (&protected, Some(false)));
    }
}

#[derive(Default)]
struct RejectFirstOutput {
    calls: AtomicUsize,
}

#[async_trait]
impl AgentOutputValidator for RejectFirstOutput {
    async fn validate(
        &self,
        _context: &AgentOutputValidationContext,
        transcript: &[AiItem],
        output: Value,
    ) -> Result<Value, AgentRunError> {
        assert_eq!(
            transcript.last().map(|message| message.role),
            Some(Role::Assistant)
        );
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(AgentRunError::new(
                "provenance_invalid",
                "source is not verified",
            ))
        } else {
            Ok(output)
        }
    }
}

#[derive(Default)]
struct RejectBeforeCommit {
    turn_id: Mutex<Option<AgentTurnId>>,
}

#[async_trait]
impl AgentOutputValidator for RejectBeforeCommit {
    async fn validate(
        &self,
        _context: &AgentOutputValidationContext,
        _transcript: &[AiItem],
        output: Value,
    ) -> Result<Value, AgentRunError> {
        Ok(output)
    }

    async fn before_commit(
        &self,
        context: &AgentOutputValidationContext,
        _transcript: &[AiItem],
        _output: &Value,
    ) -> Result<(), AgentRunError> {
        *self.turn_id.lock() = Some(context.turn_id.clone());
        Err(AgentRunError::new(
            "media_store_failed",
            "Media Artifact retention could not be extended",
        ))
    }
}

struct DenyToolAuthorizer;

#[async_trait]
impl AgentToolAuthorizer for DenyToolAuthorizer {
    async fn authorize(
        &self,
        _principal: &Principal,
        _definition_id: &AgentDefinitionId,
        _model_id: &str,
    ) -> Result<(), AgentRunError> {
        Err(AgentRunError::new("revoked", "revoked"))
    }
}

fn definition() -> AgentDefinitionSpec {
    AgentDefinitionSpec {
        id: AgentDefinitionId::new("research"),
        slug: AgentSlug::new("research"),
        revision: 1,
        description: "Research a question".into(),
        instructions: "Research carefully.".into(),
        output_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        })),
        tools: vec![VersionedToolId {
            id: "echo".into(),
            version: 1,
        }],
        budgets: AgentBudgets {
            total_wall_time: std::time::Duration::from_secs(60),
            working_wall_time: std::time::Duration::from_secs(50),
            model_turns: 4,
            tool_calls: Some(4),
            tool_parallelism: Some(2),
            concurrent_runs: Some(2),
            total_tokens: Some(1_000),
            finalization_tokens: Some(100),
        },
        artifact_policy: ArtifactPolicy {
            max_artifacts: 1,
            max_bytes: 1024,
            allowed_mime_types: vec!["image/png".into()],
        },
        repair_attempts: 1,
        exposure: stravia_runtime_contract::agent::AgentDefinitionExposure::Public,
    }
}

async fn enabled_registry() -> AgentDefinitionRegistry {
    let registry = AgentDefinitionRegistry::default();
    registry
        .synchronize(vec![definition()])
        .await
        .expect("synchronize");
    registry
        .patch_config(
            &AgentDefinitionId::new("research"),
            AgentDefinitionConfig {
                enabled: true,
                model_id: Some("model-1".into()),
                thinking_level: Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium),
            },
        )
        .await
        .expect("enable Definition");
    registry
}

#[tokio::test]
async fn deferred_concurrent_agents_export_tool_wire_in_their_parent_debug_bundles()
-> anyhow::Result<()> {
    use crate::interaction_observation::{
        AdmissionFacts, BundleRequest, BundleResourceKind, ForestQuery, IngressStart,
        InteractionObservation, RunOutcome, RunStart,
    };
    use std::io::Read;
    use stravia_runtime_contract::hook::{PlatformTool, PlatformToolError, ToolExecutionContext};

    struct RemoteTool(Arc<stravia_web_access::LocalWeb>);

    #[async_trait]
    impl PlatformTool for RemoteTool {
        fn id(&self) -> ToolId {
            ToolId::new("echo")
        }

        fn external_name(&self) -> &str {
            "echo"
        }

        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        fn parallel_safe(&self) -> bool {
            true
        }

        async fn execute(
            &self,
            arguments: Value,
            _context: ToolExecutionContext,
        ) -> Result<Value, PlatformToolError> {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(arguments["url"].as_str().unwrap())
                .body(arguments["marker"].as_str().unwrap().as_bytes().to_vec())
                .map_err(|error| PlatformToolError::new(error.to_string()))?;
            let (_, body) = self
                .0
                .http_client()
                .fetch(request.into())
                .await
                .map_err(|error| PlatformToolError::new(error.to_string()))?;
            Ok(Value::String(String::from_utf8(body).unwrap()))
        }
    }

    let directory = tempfile::tempdir()?;
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let observation = InteractionObservation::new(
        Some(pool.clone()),
        None,
        directory.path().to_path_buf(),
        crate::history_retention::HistoryRetention::new(1),
        true,
        crate::generation_chain::test_chain().await,
    )
    .await;
    observation.set_debug_enabled(true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/tool", listener.local_addr()?);
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let app = axum::Router::new().route(
        "/tool",
        axum::routing::post(move |body: axum::body::Bytes| {
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                format!("remote:{}", std::str::from_utf8(&body).unwrap())
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let web = Arc::new(stravia_web_access::LocalWeb::new(
        stravia_web_access::OutboundProxyMode::Direct,
    )?);
    let mut drivers = Vec::new();
    let mut observers = Vec::new();
    for owner in ["agent-a", "agent-b"] {
        let observer = observation
            .observe_ingress(IngressStart {
                id: owner.into(),
                method: "POST".into(),
                path: "/v1/responses".into(),
                protocol: "responses".into(),
            })
            .admit(
                RunStart {
                    id: owner.into(),
                    principal: "owner".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "agent".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                AdmissionFacts {
                    client_request: AiRequest::new("agent", Vec::new()).into(),
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            );
        let mut tool_response = AiResponse::new("tools", "model-1");
        tool_response.extend_tool_calls((0..2).map(|index| {
            ToolCall {
                id: format!("call-{index}").into(),
                name: "echo".into(),
                arguments: serde_json::json!({
                    "url": url,
                    "marker": format!("{owner}-{index}"),
                })
                .to_string(),
            }
        }));
        tool_response.stop_reason = Some("tool_calls".into());
        let mut final_response = AiResponse::new("done", "model-1");
        final_response.push_output_text(r#"{"answer":"done"}"#);
        let runner = AgentRunner::new(
            enabled_registry().await,
            Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
                tool_response,
                final_response,
            ])),
            vec![Arc::new(crate::agent::PlatformToolAgentAdapter::new(
                Arc::new(RemoteTool(web.clone())),
                1,
            ))],
            Arc::new(crate::turn_chain::test_store().await),
        )?;
        // 请求接收与后台驱动位于不同任务，不能依赖消费者恰好还在原 task-local 中。
        let events = crate::interaction_observation::scope::scope(Some(observer.clone()), async {
            runner.run(AgentInput {
                principal: Principal::new("owner"),
                definition_id: AgentDefinitionId::new("research"),
                parent_turn_id: None,
                prompt: owner.into(),
                artifacts: Vec::new(),
                cancellation: CancellationToken::new(),
            })
        })
        .await;
        drivers.push(events);
        observers.push(observer);
    }
    for observer in &observers {
        observer.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
    }
    // 先持久化父请求终态，再驱动后台；后续远程字节仍必须归属原 Trace。
    observation.flush().await?;
    let drivers = drivers
        .into_iter()
        .map(|events| tokio::spawn(async move { events.collect::<Vec<_>>().await }))
        .collect::<Vec<_>>();
    for driver in drivers {
        let events = tokio::time::timeout(std::time::Duration::from_secs(10), driver).await??;
        assert!(
            matches!(events.last(), Some(AgentEvent::Completed(_))),
            "{events:?}"
        );
    }
    observation.flush().await?;
    let forest = observation.query_forest(ForestQuery::default()).await?;
    assert_eq!(
        forest.roots.len(),
        2,
        "tools must not create separate roots"
    );
    let mut captured_owners = std::collections::BTreeSet::new();
    for root in forest.roots {
        let interaction = &root.interactions[0];
        let ticket = observation
            .issue_bundle_ticket(BundleRequest {
                kind: BundleResourceKind::Interaction,
                resource_id: interaction.id.clone(),
                through_sequence: Some(forest.snapshot_sequence),
            })
            .await?;
        let token = ticket.download_url.rsplit('/').next().unwrap();
        let mut stream = observation.consume_bundle_ticket(token).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        let mut records = Vec::<Value>::new();
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            if entry.name()?.ends_with("events.jsonl") {
                let mut text = String::new();
                entry.read_to_string(&mut text)?;
                records.extend(
                    text.lines()
                        .map(serde_json::from_str::<Value>)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
        }
        let owner = records.first().expect("remote wire in parent bundle")["run_id"]
            .as_str()
            .unwrap();
        assert!(captured_owners.insert(owner.to_owned()));
        assert!(records.iter().all(|record| record["run_id"] == owner));
        for index in 0..2 {
            let request = format!("{owner}-{index}");
            let response = format!("remote:{request}");
            assert!(records.iter().any(|record| {
                record["direction"] == "upstream_request" && record["payload"] == request
            }));
            assert!(
                records.iter().any(|record| {
                    record["direction"] == "upstream_response" && record["payload"] == response
                }),
                "missing {response:?}: {records:#?}"
            );
        }
        let other = if owner == "agent-a" {
            "agent-b"
        } else {
            "agent-a"
        };
        assert!(!records.iter().any(|record| {
            record["payload"]
                .as_str()
                .is_some_and(|payload| payload.contains(other))
        }));
    }
    assert_eq!(captured_owners, ["agent-a".into(), "agent-b".into()].into());
    server.abort();
    observation.shutdown().await;
    pool.close().await;
    Ok(())
}

fn request_session_fingerprint(request: &AiRequest) -> String {
    crate::generation_chain::generation_session_fingerprint(request)
        .expect("Agent Model Turn must carry a generation session")
}

#[tokio::test]
async fn runner_executes_tool_loop_commits_turn_and_emits_one_terminal_event() {
    let mut tool_response = AiResponse::new("response-1", "model-1");
    tool_response.push_output_text("private scratch");
    tool_response.extend_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: r#"{"value":"observed"}"#.into(),
    }]);
    tool_response.stop_reason = Some("tool_calls".into());
    let mut final_response = AiResponse::new("response-2", "model-1");
    final_response.push_output_text(r#"{"answer":"done"}"#);
    final_response.stop_reason = Some("stop".into());
    final_response.usage.total_tokens = 10;
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        tool_response,
        final_response,
    ]));
    let turns: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::clone(&turns),
    )
    .expect("Agent Runner");

    let events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::PublicOutputDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![r#"{"answer":"done"}"#]
    );
    let result = match events.last().expect("terminal event") {
        AgentEvent::Completed(result) => result,
        other => panic!("unexpected terminal event: {other:?}"),
    };
    assert_eq!(result.output, serde_json::json!({"answer": "done"}));
    let chain = turns
        .materialize(
            &Principal::new("owner"),
            TurnNodeKind::Agent,
            &result.turn_id,
        )
        .await
        .expect("materialized Turn");
    assert_eq!(chain.len(), 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.reasoning.level == Some(stravia_runtime_contract::thinking::ThinkingLevel::Medium)
    }));
    assert_eq!(
        request_session_fingerprint(&requests[0]),
        request_session_fingerprint(&requests[1]),
        "every Model Turn in one Agent Turn must share its cache identity"
    );
    assert!(
        requests.iter().all(|request| {
            request
                .instructions
                .as_deref()
                .is_some_and(|instructions| instructions.contains(r#""required":["answer"]"#))
        }),
        "every Model Turn must receive the Definition output schema"
    );
    assert!(
        requests[1]
            .items
            .iter()
            .any(|message| message.role == Role::Tool)
    );
}

#[tokio::test]
async fn request_content_hook_processes_tool_loop_inputs_once_and_reuses_parent_turn_history() {
    use stravia_runtime_contract::hook::{
        ActionBatch, Hook, HookAction, HookDescriptor, HookEvent, HookSession, RequestPatch,
    };

    struct MarkInput;

    impl Hook for MarkInput {
        fn descriptor(&self) -> HookDescriptor {
            HookDescriptor::all("mark-agent-input")
        }

        fn create_session(&self, _context: &SessionContext) -> Box<dyn HookSession> {
            Box::new(MarkInput)
        }
    }

    #[async_trait]
    impl HookSession for MarkInput {
        async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
            let HookEvent::Request {
                current,
                input_start,
                ..
            } = event
            else {
                return Ok(ActionBatch::default());
            };
            let mut items = current.items[input_start..].to_vec();
            for item in &mut items {
                match &mut item.content {
                    MessageContent::Text(text) => Arc::make_mut(text).push_str(" [processed]"),
                    MessageContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                ContentBlock::Text { text, .. } => {
                                    Arc::make_mut(text).push_str(" [processed]");
                                }
                                ContentBlock::ToolResult { content, .. } => {
                                    if let Some(Value::String(text)) = content.get_mut("value") {
                                        text.push_str(" [processed]");
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                RequestPatch::SpliceInput {
                    range: 0..items.len(),
                    items,
                },
            ))))
        }
    }

    let mut tool_response = AiResponse::new("response-1", "model-1");
    tool_response.push_output_text("private scratch");
    tool_response.extend_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: r#"{"value":"observed"}"#.into(),
    }]);
    tool_response.stop_reason = Some("tool_calls".into());
    let mut first_response = AiResponse::new("response-2", "model-1");
    first_response.push_output_text(r#"{"answer":"first"}"#);
    let mut child_response = AiResponse::new("response-3", "model-1");
    child_response.push_output_text(r#"{"answer":"child"}"#);
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        tool_response,
        first_response,
        child_response,
    ]));
    let turns: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(EchoTool)],
        turns,
    )
    .expect("Agent Runner")
    .with_hook_runtime(HookRuntime::new(vec![Arc::new(MarkInput)]));
    let first_events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    let first = match first_events.last().expect("first terminal event") {
        AgentEvent::Completed(result) => result,
        other => panic!("unexpected first terminal event: {other:?}"),
    };
    assert_eq!(first.output, serde_json::json!({"answer": "first"}));
    assert!(first_events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolFinished {
            is_error: false,
            ..
        }
    )));

    let child_events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: Some(first.turn_id.clone()),
            prompt: "follow-up".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    let child = match child_events.last().expect("child terminal event") {
        AgentEvent::Completed(result) => result,
        other => panic!("unexpected child terminal event: {other:?}"),
    };
    assert_eq!(child.output, serde_json::json!({"answer": "child"}));

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].items.len(), 1);
    assert_eq!(
        requests[0].items[0].content.to_text(),
        "question [processed]"
    );
    assert_eq!(
        requests[1].items[0].content.to_text(),
        "question [processed]"
    );
    let scratch = requests[1]
        .items
        .iter()
        .find(|item| item.content.to_text().contains("private scratch"))
        .expect("previous model output becomes input");
    assert_eq!(scratch.content.to_text(), "private scratch [processed]");
    assert_eq!(
        agent_tool_payload(&requests[1]),
        (
            &serde_json::json!({"value": "observed [processed]"}),
            Some(false)
        )
    );
    assert_eq!(
        serde_json::to_value(&requests[2].items[..requests[1].items.len()]).unwrap(),
        serde_json::to_value(&requests[1].items).unwrap(),
        "a child Turn must reuse the exact processed tool-loop history"
    );
    let newly_processed = &requests[2].items[requests[1].items.len()..];
    assert_eq!(newly_processed.len(), 2);
    assert_eq!(
        newly_processed[0].content.to_text(),
        r#"{"answer":"first"} [processed]"#,
        "the parent's final model output first becomes Request Hook input on resume"
    );
    assert_eq!(
        newly_processed[1].content.to_text(),
        "follow-up [processed]"
    );
}

#[tokio::test]
async fn model_turn_budget_reserves_a_no_tool_finalization_turn() {
    let mut tool_response = AiResponse::new("response-1", "model-1");
    tool_response.extend_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: r#"{"value":"observed"}"#.into(),
    }]);
    tool_response.stop_reason = Some("tool_calls".into());
    let mut final_response = AiResponse::new("response-2", "model-1");
    final_response.push_output_text(r#"{"answer":"best available"}"#);
    final_response.stop_reason = Some("stop".into());
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        tool_response,
        final_response,
    ]));
    let mut spec = definition();
    spec.budgets.model_turns = 2;
    let registry = AgentDefinitionRegistry::default();
    registry.synchronize(vec![spec]).await.expect("synchronize");
    registry
        .patch_config(
            &AgentDefinitionId::new("research"),
            AgentDefinitionConfig {
                enabled: true,
                model_id: Some("model-1".into()),
                thinking_level: None,
            },
        )
        .await
        .expect("enable Definition");
    let runner = AgentRunner::new(
        registry,
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner");

    let events = runner
        .run_ephemeral(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    let result = match events.last() {
        Some(AgentEvent::Partial(result)) => result,
        other => panic!("unexpected terminal event: {other:?}"),
    };
    assert_eq!(
        result.output,
        serde_json::json!({"answer": "best available"})
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].tools.as_ref().is_none_or(Vec::is_empty),
        "reserved finalization turn must not expose tools"
    );
}

#[tokio::test]
async fn ephemeral_run_returns_validated_output_without_committing_an_agent_turn() {
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text(r#"{"answer":"ephemeral"}"#);
    let turns: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let runner = AgentRunner::new(
        enabled_registry().await,
        Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
            response,
        ])),
        vec![Arc::new(EchoTool)],
        Arc::clone(&turns),
    )
    .expect("Agent Runner");

    let events = runner
        .run_ephemeral(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    let result = match events.last().expect("terminal") {
        AgentEvent::Completed(result) => result,
        other => panic!("unexpected terminal: {other:?}"),
    };
    assert_eq!(result.output, serde_json::json!({"answer": "ephemeral"}));
    assert!(
        turns
            .materialize(
                &Principal::new("owner"),
                TurnNodeKind::Agent,
                &result.turn_id,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn transcript_aware_output_validation_runs_before_commit_and_can_repair_once() {
    let mut first = AiResponse::new("response-1", "model-1");
    first.items.push(AiItem::reasoning(
        vec!["summary".into()],
        Vec::new(),
        Some("opaque-reasoning".into()),
    ));
    first.push_output_text(r#"{"answer":"unverified"}"#);
    let mut repaired = AiResponse::new("response-2", "model-1");
    repaired.push_output_text(r#"{"answer":"verified"}"#);
    let validator = Arc::new(RejectFirstOutput::default());
    let model = Arc::new(OrdinalRecordingExecutor {
        inner: crate::agent::InMemoryModelTurnExecutor::scripted([first, repaired]),
        ordinals: Mutex::new(Vec::new()),
        user_turns: Mutex::new(Vec::new()),
        dialogue: Mutex::new(Vec::new()),
        windows: Mutex::new(Vec::new()),
        candidates: Mutex::new(Vec::new()),
        logical_turns: Mutex::new(Vec::new()),
        facts: Mutex::new(Vec::new()),
    });
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner")
    .with_output_validator(AgentDefinitionId::new("research"), 1, validator.clone());

    let events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    let result = match events.last().expect("terminal") {
        AgentEvent::Completed(result) => result,
        other => panic!("unexpected terminal: {other:?}"),
    };
    assert_eq!(result.output, serde_json::json!({"answer": "verified"}));
    assert_eq!(validator.calls.load(Ordering::SeqCst), 2);
    assert_eq!(*model.ordinals.lock(), vec![Some(1), Some(1)]);
    assert_eq!(*model.user_turns.lock(), vec![Some(1), Some(2)]);
    let requests = model.inner.requests();
    assert_eq!(
        request_session_fingerprint(&requests[0]),
        request_session_fingerprint(&requests[1]),
        "schema repair must preserve the Agent Turn cache identity"
    );
    let replay = &requests[1].items;
    assert!(
        replay.iter().any(|item| {
            matches!(
                &item.content,
                MessageContent::Blocks(blocks)
                    if matches!(blocks.as_slice(), [ContentBlock::Reasoning { .. }])
            )
        }),
        "reasoning output must remain a standalone canonical item: {replay:?}"
    );
}

#[tokio::test]
async fn before_commit_failure_does_not_leave_a_durable_agent_turn() {
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text(r#"{"answer":"verified"}"#);
    let validator = Arc::new(RejectBeforeCommit::default());
    let turns: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let runner = AgentRunner::new(
        enabled_registry().await,
        Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
            response,
        ])),
        vec![Arc::new(EchoTool)],
        Arc::clone(&turns),
    )
    .expect("Agent Runner")
    .with_output_validator(AgentDefinitionId::new("research"), 1, validator.clone());
    let principal = Principal::new("owner");

    let events = runner
        .run(AgentInput {
            principal: principal.clone(),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(AgentEvent::Failed { error }) if error.code == "media_store_failed"
    ));
    let turn_id = validator
        .turn_id
        .lock()
        .clone()
        .expect("validator should observe Turn ID");
    assert!(
        turns
            .materialize(&principal, TurnNodeKind::Agent, &turn_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancellation_after_before_commit_starts_still_commits_agent_turn() {
    struct PausedBeforeCommit {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<AgentTurnId>>>,
        release: tokio::sync::Semaphore,
    }

    #[async_trait]
    impl AgentOutputValidator for PausedBeforeCommit {
        async fn validate(
            &self,
            _context: &AgentOutputValidationContext,
            _transcript: &[AiItem],
            output: Value,
        ) -> Result<Value, AgentRunError> {
            Ok(output)
        }

        async fn before_commit(
            &self,
            context: &AgentOutputValidationContext,
            _transcript: &[AiItem],
            _output: &Value,
        ) -> Result<(), AgentRunError> {
            self.entered
                .lock()
                .take()
                .expect("pre-commit runs once")
                .send(context.turn_id.clone())
                .expect("announce pre-commit start");
            let _permit = self.release.acquire().await.expect("release pre-commit");
            Ok(())
        }
    }

    let (entered, started) = tokio::sync::oneshot::channel();
    let validator = Arc::new(PausedBeforeCommit {
        entered: Mutex::new(Some(entered)),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text(r#"{"answer":"verified"}"#);
    let turns: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let runner = AgentRunner::new(
        enabled_registry().await,
        Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
            response,
        ])),
        vec![Arc::new(EchoTool)],
        Arc::clone(&turns),
    )
    .expect("Agent Runner")
    .with_output_validator(AgentDefinitionId::new("research"), 1, validator.clone());
    let principal = Principal::new("owner");
    let cancellation = CancellationToken::new();
    let stream = runner.run(AgentInput {
        principal: principal.clone(),
        definition_id: AgentDefinitionId::new("research"),
        parent_turn_id: None,
        prompt: "question".into(),
        artifacts: Vec::new(),
        cancellation: cancellation.clone(),
    });
    let run = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
    let turn_id = started.await.expect("pre-commit has started");
    cancellation.cancel();
    validator.release.add_permits(1);
    let events = run.await.expect("Agent Run completes");
    let chain = turns
        .materialize(&principal, TurnNodeKind::Agent, &turn_id)
        .await
        .expect("started pre-commit must finish its durable Agent Turn commit");
    assert_eq!(chain.len(), 1);
    assert!(matches!(events.last(), Some(AgentEvent::Completed(_))));
}

#[tokio::test]
async fn live_authorization_failure_prevents_tool_execution() {
    let mut tool_response = AiResponse::new("response-1", "model-1");
    tool_response.extend_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: r#"{"value":"must-not-run"}"#.into(),
    }]);
    tool_response.stop_reason = Some("tool_calls".into());
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        tool_response,
    ]));
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner")
    .with_tool_authorizer(Arc::new(DenyToolAuthorizer));

    let events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Failed { error })
            if error.code == "tool_authorization_failed"
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolFinished { .. }))
    );
}
#[tokio::test]
async fn run_does_not_start_until_the_event_stream_is_polled() {
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text(r#"{"answer":"done"}"#);
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        response,
    ]));
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner");

    let events = runner.run(AgentInput {
        principal: Principal::new("owner"),
        definition_id: AgentDefinitionId::new("research"),
        parent_turn_id: None,
        prompt: "question".into(),
        artifacts: Vec::new(),
        cancellation: CancellationToken::new(),
    });
    assert!(model.requests().is_empty());
    drop(events);
    assert!(model.requests().is_empty());
}

#[tokio::test]
async fn hard_token_limit_rejects_output_without_publishing_it() {
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text(r#"{"answer":"over budget"}"#);
    response.usage.total_tokens = 1_001;
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        response,
    ]));
    let runner = AgentRunner::new(
        enabled_registry().await,
        model,
        vec![Arc::new(EchoTool)],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner");

    let events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PublicOutputDelta { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Failed { error }) if error.code == "token_limit"
    ));
}

#[tokio::test]
async fn child_turn_reuses_parent_transcript_without_mutating_parent() {
    let mut first = AiResponse::new("response-1", "model-1");
    first.push_output_text(r#"{"answer":"first"}"#);
    let mut second = AiResponse::new("response-2", "model-1");
    second.push_output_text(r#"{"answer":"second"}"#);
    let mut independent = AiResponse::new("response-3", "model-1");
    independent.push_output_text(r#"{"answer":"independent"}"#);
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        first,
        independent,
        second,
    ]));
    let turns: Arc<dyn TurnChainStore> = Arc::new(crate::turn_chain::test_store().await);
    let registry = enabled_registry().await;
    let runner = AgentRunner::new(
        registry.clone(),
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::clone(&turns),
    )
    .expect("Agent Runner");
    let principal = Principal::new("owner");
    let first_events = runner
        .run(AgentInput {
            principal: principal.clone(),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "first question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    let parent = match first_events.last().expect("terminal") {
        AgentEvent::Completed(result) => result.turn_id.clone(),
        other => panic!("unexpected terminal: {other:?}"),
    };
    let independent_events = runner
        .run(AgentInput {
            principal: principal.clone(),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "independent question".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        independent_events.last(),
        Some(AgentEvent::Completed(_))
    ));
    let mut revised = definition();
    revised.revision = 2;
    revised.instructions = "New instructions.".into();
    registry
        .synchronize(vec![revised])
        .await
        .expect("Definition revision");
    registry
        .synchronize(Vec::new())
        .await
        .expect("retire current Definition");
    let second_events = runner
        .run(AgentInput {
            principal: principal.clone(),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: Some(parent.clone()),
            prompt: "follow-up".into(),
            artifacts: Vec::new(),
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;
    let child = match second_events.last().expect("terminal") {
        AgentEvent::Completed(result) => result.turn_id.clone(),
        other => panic!("unexpected terminal: {other:?}"),
    };

    let parent_chain = turns
        .materialize(&principal, TurnNodeKind::Agent, &parent)
        .await
        .expect("parent");
    let child_chain = turns
        .materialize(&principal, TurnNodeKind::Agent, &child)
        .await
        .expect("child");
    assert_eq!(parent_chain.len(), 1);
    assert_eq!(child_chain.len(), 2);
    let requests = model.requests();
    let expected_system = model_instructions(&definition());
    assert_eq!(requests[2].items.len(), 3);
    assert_eq!(
        request_session_fingerprint(&requests[0]),
        request_session_fingerprint(&requests[2]),
        "a child Agent Turn must preserve the root cache identity"
    );
    assert_ne!(
        request_session_fingerprint(&requests[0]),
        request_session_fingerprint(&requests[1]),
        "independent Agent roots must not share cache identity"
    );
    assert_eq!(
        requests[2].instructions.as_deref(),
        Some(expected_system.as_str())
    );
}

type RecordedPromptIdentity = (Option<String>, Option<i64>, Option<String>, Option<i64>);

struct OrdinalRecordingExecutor {
    inner: crate::agent::InMemoryModelTurnExecutor,
    ordinals: Mutex<Vec<Option<u64>>>,
    user_turns: Mutex<Vec<Option<u64>>>,
    dialogue: Mutex<Vec<Option<u64>>>,
    windows: Mutex<Vec<(Option<uuid::Uuid>, Option<u64>)>>,
    candidates: Mutex<Vec<Option<crate::generation_chain::HistoryBoundaryCandidate>>>,
    logical_turns: Mutex<Vec<(Option<uuid::Uuid>, Option<i64>)>>,
    facts: Mutex<Vec<RecordedPromptIdentity>>,
}

#[async_trait]
impl ModelTurnExecutor for OrdinalRecordingExecutor {
    async fn execute(
        &self,
        input: TurnInput,
    ) -> Result<crate::model_turn::ModelTurn, stravia_runtime_contract::model_turn::ModelTurnError>
    {
        self.ordinals.lock().push(input.user_prompt_index);
        self.user_turns.lock().push(input.user_turn_index);
        self.dialogue.lock().push(input.dialogue_prompt_index);
        self.windows
            .lock()
            .push((input.context_window_id, input.context_window_index));
        self.candidates
            .lock()
            .push(input.history_boundary_candidate.clone());
        self.logical_turns
            .lock()
            .push((input.logical_turn_id, input.logical_turn_started_at_ms));
        self.facts.lock().push((
            input.user_prompt_id.clone(),
            input.user_prompt_created_at_ms,
            input.client_conversation_id.clone(),
            input.client_conversation_created_at_ms,
        ));
        self.inner.execute(input).await
    }
}

#[tokio::test]
async fn agent_prompt_ordinals_follow_forks_across_runner_recreation_and_tool_loops() {
    let mut tool_response = AiResponse::new("tool", "model-1");
    tool_response.extend_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "echo".into(),
        arguments: "{}".into(),
    }]);
    tool_response.stop_reason = Some("tool_calls".into());
    let response = || {
        let mut response = AiResponse::new("answer", "model-1");
        response.push_output_text(r#"{"answer":"done"}"#);
        response
    };
    let model = Arc::new(OrdinalRecordingExecutor {
        inner: crate::agent::InMemoryModelTurnExecutor::scripted([
            tool_response,
            response(),
            response(),
            response(),
            response(),
            response(),
        ]),
        ordinals: Mutex::new(Vec::new()),
        user_turns: Mutex::new(Vec::new()),
        dialogue: Mutex::new(Vec::new()),
        windows: Mutex::new(Vec::new()),
        candidates: Mutex::new(Vec::new()),
        logical_turns: Mutex::new(Vec::new()),
        facts: Mutex::new(Vec::new()),
    });
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let registry = enabled_registry().await;
    let make_runner = || {
        AgentRunner::new(
            registry.clone(),
            model.clone(),
            vec![Arc::new(EchoTool)],
            Arc::new(crate::turn_chain::SqlTurnChainStore::sqlite(
                pool.clone(),
                crate::history_retention::HistoryRetention::for_tests(),
            )),
        )
        .unwrap()
    };
    let run = |runner: AgentRunner, principal: Principal, parent_turn_id: Option<AgentTurnId>| async move {
        runner
            .run(AgentInput {
                principal,
                definition_id: AgentDefinitionId::new("research"),
                parent_turn_id,
                prompt: "question".into(),
                artifacts: Vec::new(),
                cancellation: CancellationToken::new(),
            })
            .collect::<Vec<_>>()
            .await
    };
    let completed = |events: Vec<AgentEvent>| match events.last().unwrap() {
        AgentEvent::Completed(result) => result.turn_id.clone(),
        other => panic!("unexpected terminal: {other:?}"),
    };
    let owner = Principal::new("owner");
    let root = completed(run(make_runner(), owner.clone(), None).await);
    let child = completed(run(make_runner(), owner.clone(), Some(root.clone())).await);
    let _sibling = completed(run(make_runner(), owner.clone(), Some(root.clone())).await);
    let _grandchild = completed(run(make_runner(), owner.clone(), Some(child)).await);
    let _independent = completed(run(make_runner(), Principal::new("other"), None).await);
    let denied = run(make_runner(), Principal::new("other"), Some(root)).await;
    assert!(matches!(denied.last(), Some(AgentEvent::Failed { .. })));
    assert_eq!(
        *model.ordinals.lock(),
        vec![Some(1), Some(1), Some(2), Some(2), Some(3), Some(1)]
    );
    assert_eq!(
        *model.user_turns.lock(),
        vec![Some(1), Some(1), Some(2), Some(2), Some(3), Some(1)]
    );
    assert_eq!(
        *model.dialogue.lock(),
        vec![Some(1), Some(1), Some(2), Some(2), Some(3), Some(1)]
    );
    let windows = model.windows.lock();
    assert!(windows[0].0.is_some());
    assert_eq!(windows[0].1, Some(0));
    assert!(windows[1..5].iter().all(|window| *window == windows[0]));
    assert_eq!(windows[5].1, Some(0));
    assert_ne!(windows[5].0, windows[0].0);
    drop(windows);
    let candidates = model.candidates.lock();
    let first = candidates[0].as_ref().unwrap();
    let second = candidates[1].as_ref().unwrap();
    assert_eq!(first.source_item_gap, 0);
    assert!(second.source_item_gap > 0);
    assert_eq!(first.source_admission_id, second.source_admission_id);
    assert_ne!(first.id, second.id);
    assert_ne!(
        candidates[2].as_ref().unwrap().id,
        candidates[3].as_ref().unwrap().id
    );
    assert_ne!(
        candidates[0].as_ref().unwrap().window_id,
        candidates[5].as_ref().unwrap().window_id
    );
    drop(candidates);
    let facts = model.facts.lock();
    let logical_turns = model.logical_turns.lock();
    for (logical, fact) in logical_turns.iter().zip(facts.iter()) {
        assert_eq!(logical.0.map(|id| id.to_string()), fact.0);
        assert_eq!(logical.1, fact.1);
    }
    assert_eq!(logical_turns[0], logical_turns[1]);
    assert_ne!(logical_turns[0].0, logical_turns[2].0);
    assert_ne!(logical_turns[2].0, logical_turns[3].0);
    assert_eq!(facts[0], facts[1], "tool loops reuse the admission facts");
    assert_eq!(facts[0].0, facts[0].2);
    assert_eq!(facts[0].1, facts[0].3);
    assert!(facts[0].1.is_some());
    for fact in &facts[2..5] {
        assert_eq!(fact.2, facts[0].2);
        assert_eq!(fact.3, facts[0].3);
        assert_ne!(fact.0, facts[0].0);
        assert!(fact.1.is_some());
        uuid::Uuid::parse_str(fact.0.as_deref().unwrap()).unwrap();
    }
    assert_ne!(facts[2].0, facts[3].0);
    assert_ne!(facts[2].0, facts[4].0);
    assert_ne!(facts[5].2, facts[0].2);
    assert_eq!(facts[5].0, facts[5].2);
    uuid::Uuid::parse_str(facts[0].0.as_deref().unwrap()).unwrap();
}
