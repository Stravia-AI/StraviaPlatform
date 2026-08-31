use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::*;
use crate::agent::{
    AgentBudgets, AgentDefinitionConfig, AgentDefinitionSpec, AgentSlug, ArtifactPolicy,
};

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
    ) -> Result<Value, super::super::AgentToolError> {
        Ok(input)
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
        *self.turn_id.lock().expect("turn ID") = Some(context.turn_id.clone());
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

struct ImageArtifactStore {
    path: std::path::PathBuf,
}

#[async_trait]
impl ArtifactStore for ImageArtifactStore {
    async fn create_upload(
        &self,
        _principal: &Principal,
        _request: crate::agent::ArtifactUploadRequest,
    ) -> Result<crate::agent::ArtifactUpload, crate::agent::ArtifactError> {
        Err(crate::agent::ArtifactError::Storage("not used".into()))
    }

    async fn upload_part(
        &self,
        _principal: &Principal,
        _upload_id: &str,
        _upload_token: &str,
        _part_number: u32,
        _bytes: crate::agent::ArtifactByteStream,
    ) -> Result<crate::agent::UploadedArtifactPart, crate::agent::ArtifactError> {
        Err(crate::agent::ArtifactError::Storage("not used".into()))
    }

    async fn complete_upload(
        &self,
        _principal: &Principal,
        _upload_id: &str,
        _upload_token: &str,
        _parts: &[crate::agent::UploadedArtifactPart],
    ) -> Result<crate::agent::ArtifactRef, crate::agent::ArtifactError> {
        Err(crate::agent::ArtifactError::Storage("not used".into()))
    }

    async fn open(
        &self,
        _principal: &Principal,
        id: &ArtifactId,
    ) -> Result<crate::agent::ArtifactReader, crate::agent::ArtifactError> {
        Ok(crate::agent::ArtifactReader {
            artifact: crate::agent::ArtifactRef {
                id: id.clone(),
                mime_type: "image/png".into(),
                size: 3,
            },
            source: ArtifactSource::LocalPath(self.path.clone()),
        })
    }

    async fn extend_retention(
        &self,
        _principal: &Principal,
        _id: &ArtifactId,
        _retention: Duration,
    ) -> Result<(), crate::agent::ArtifactError> {
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, crate::agent::ArtifactError> {
        Ok(0)
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
            total_wall_time: Duration::from_secs(60),
            working_wall_time: Duration::from_secs(50),
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
        exposure: crate::agent::AgentDefinitionExposure::Public,
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
            },
        )
        .await
        .expect("enable Definition");
    registry
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
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        first, repaired,
    ]));
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
    let requests = model.requests();
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
        .expect("turn ID")
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
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        first, second,
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
    assert_eq!(requests[1].items.len(), 3);
    assert_eq!(
        requests[1].instructions.as_deref(),
        Some(expected_system.as_str())
    );
}
#[tokio::test]
async fn runner_materializes_artifact_as_canonical_multimodal_input() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("image.png");
    tokio::fs::write(&path, b"png")
        .await
        .expect("image fixture");
    let mut response = AiResponse::new("response-1", "model-1");
    response.push_output_text(r#"{"answer":"image"}"#);
    let model = Arc::new(crate::agent::InMemoryModelTurnExecutor::scripted([
        response,
    ]));
    let runner = AgentRunner::new(
        enabled_registry().await,
        model.clone(),
        vec![Arc::new(EchoTool)],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner")
    .with_artifact_store(Some(Arc::new(ImageArtifactStore { path })));

    let events = runner
        .run(AgentInput {
            principal: Principal::new("owner"),
            definition_id: AgentDefinitionId::new("research"),
            parent_turn_id: None,
            prompt: "describe".into(),
            artifacts: vec![ArtifactId::new("artifact-1")],
            cancellation: CancellationToken::new(),
        })
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(events.last(), Some(AgentEvent::Completed(_))));
    let requests = model.requests();
    let MessageContent::Blocks(blocks) = &requests[0].items[0].content else {
        panic!("expected multimodal blocks");
    };
    assert!(matches!(
        &blocks[1],
        ContentBlock::Image {
            source: MediaSource::Base64 { media_type, data },
            ..
        } if media_type == "image/png" && data == "cG5n"
    ));
}
