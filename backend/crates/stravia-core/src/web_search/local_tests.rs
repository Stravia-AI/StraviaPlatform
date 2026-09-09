use stravia_web_search::local::*;
use stravia_web_search::*;

use async_trait::async_trait;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

use std::sync::atomic::{AtomicUsize, Ordering};
use stravia_runtime_contract::agent::AgentDefinitionId;
use stravia_runtime_contract::protocol::ir::Role;

use crate::agent::AgentDefinitionRegistry;
use crate::agent::AgentRunner;
use crate::agent::ModelTurnExecutor;
use crate::agent::PlatformToolAgentAdapter;
use crate::agent::TurnInput;
use stravia_runtime_contract::agent::AgentDefinitionConfig;
use stravia_runtime_contract::agent::VersionedToolId;
use stravia_runtime_contract::hook::PlatformTool;
use stravia_runtime_contract::hook::PlatformToolError;
use stravia_runtime_contract::hook::ToolExecutionContext;
use stravia_runtime_contract::hook::ToolId;
use stravia_runtime_contract::model_turn::CanonicalEvent;
use stravia_runtime_contract::model_turn::ModelTurnError;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::ToolCall;

use crate::web_access::WEB_FETCH_NAME;
use crate::web_access::WEB_SEARCH_NAME;
use stravia_web_access_contract::WEB_FETCH_TOOL_ID;
use stravia_web_access_contract::WEB_SEARCH_TOOL_ID;

struct SchemaRepairModel {
    turns: Arc<AtomicUsize>,
}

#[async_trait]
impl ModelTurnExecutor for SchemaRepairModel {
    async fn execute(&self, input: TurnInput) -> Result<crate::agent::ModelTurn, ModelTurnError> {
        let search_turn_id = input
            .request
            .items
            .iter()
            .find(|message| message.role == Role::User)
            .and_then(message_text)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .and_then(|input| input["turn_id"].as_str().map(SearchTurnId::new))
            .ok_or_else(|| ModelTurnError::new("invalid_test_input", "missing Search Turn"))?;
        let turn = self.turns.fetch_add(1, Ordering::SeqCst);
        let marker = format!("source-{search_turn_id}-1");
        let invalid_report = serde_json::json!({
            "answer": format!("Verified claim [{marker}]"),
            "sources": [{
                "id": marker,
                "url": "https://8.8.8.8/source",
                "title": "Verified"
            }]
        });
        let mut response = AiResponse::new(format!("response-{turn}"), "model-1");
        match turn {
            0 => {
                response.extend_tool_calls(vec![ToolCall {
                    id: "search-1".into(),
                    name: input
                        .request
                        .tools
                        .as_ref()
                        .and_then(|tools| tools.first())
                        .expect("search tool")
                        .name
                        .clone(),
                    arguments: serde_json::json!({"query": "verified claim"}).to_string(),
                }]);
                response.stop_reason = Some("tool_calls".into());
            }
            1 => {
                response.push_output_text(invalid_report.to_string());
                response.stop_reason = Some("stop".into());
            }
            _ if input
                .request
                .instructions
                .as_deref()
                .is_some_and(|instructions| {
                    instructions.contains(r#""limitations""#)
                        && instructions.contains(r#""required""#)
                }) =>
            {
                let mut report = invalid_report;
                report["limitations"] = Value::Array(Vec::new());
                response.push_output_text(report.to_string());
                response.stop_reason = Some("stop".into());
            }
            _ => {
                response.push_output_text(invalid_report.to_string());
                response.stop_reason = Some("stop".into());
            }
        }
        Ok(crate::agent::ModelTurn::in_memory(
            stravia_runtime_contract::hook::RouteContext {
                model_id: input.request.model.clone(),
                provider_id: "in-memory".into(),
                target_id: "in-memory".into(),
                egress: stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24,
            },
            input.request,
            [Ok(CanonicalEvent::Completed(Box::new(response)))],
        ))
    }
}

struct SearchLeaf {
    id: &'static str,
    name: &'static str,
}

#[async_trait]
impl PlatformTool for SearchLeaf {
    fn id(&self) -> ToolId {
        ToolId::new(self.id)
    }

    fn external_name(&self) -> &str {
        self.name
    }

    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _arguments: Value,
        _context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        Ok(serde_json::json!({
            "results": [{
                "url": "https://8.8.8.8/source",
                "status": "success",
                "title": "Verified"
            }]
        }))
    }
}

#[tokio::test]
async fn local_backend_repairs_schema_without_native_structured_outputs() {
    let definitions = AgentDefinitionRegistry::default();
    definitions
        .synchronize(vec![local_search_definition()])
        .await
        .expect("Local Search Definition");
    definitions
        .patch_config(
            &AgentDefinitionId::new(LOCAL_SEARCH_DEFINITION_ID),
            AgentDefinitionConfig {
                enabled: true,
                model_id: Some("model-1".into()),
                thinking_level: None,
            },
        )
        .await
        .expect("Local Search config");
    let turns = Arc::new(AtomicUsize::new(0));
    let evidence = Arc::new(LocalSearchEvidenceStore::default());
    let validator = Arc::new(SearchReportValidator);
    let runner = AgentRunner::new(
        definitions,
        Arc::new(SchemaRepairModel {
            turns: Arc::clone(&turns),
        }),
        vec![
            Arc::new(PlatformToolAgentAdapter::with_id(
                Arc::new(SearchLeaf {
                    id: WEB_SEARCH_TOOL_ID,
                    name: WEB_SEARCH_NAME,
                }),
                VersionedToolId {
                    id: WEB_SEARCH_TOOL_ID.into(),
                    version: 1,
                },
            )),
            Arc::new(PlatformToolAgentAdapter::with_id(
                Arc::new(SearchLeaf {
                    id: WEB_FETCH_TOOL_ID,
                    name: WEB_FETCH_NAME,
                }),
                VersionedToolId {
                    id: WEB_FETCH_TOOL_ID.into(),
                    version: 1,
                },
            )),
        ],
        Arc::new(crate::turn_chain::test_store().await),
    )
    .expect("Agent Runner")
    .with_output_validator(
        AgentDefinitionId::new(LOCAL_SEARCH_DEFINITION_ID),
        LOCAL_SEARCH_DEFINITION_REVISION,
        Arc::new(LocalSearchOutputValidator::new(
            Arc::clone(&validator),
            Arc::clone(&evidence),
        )),
    );
    let backend = LocalSearchBackend::new(Arc::new(super::host::LocalAgentHost(runner)), evidence);
    let turn_id = SearchTurnId::new("wst_schema_prompt");

    let output = backend
        .run(SearchBackendInput {
            turn_id: turn_id.clone(),
            principal: stravia_runtime_contract::Principal::new("owner"),
            query: "Search the claim".into(),
            policy: stravia_web_search::WebSearchRunPolicy::default(),
            ancestors: Vec::new(),
            binding: stravia_web_search::ResolvedWebSearchBackend::Local {
                model_id: "model-1".into(),
            },
            definition_revision: Some(LOCAL_SEARCH_DEFINITION_REVISION),
            local_limits: Some(stravia_web_search::LocalSearchLimits {
                max_turns: 4,
                total_time: Duration::from_secs(60),
            }),
            cancellation: stravia_runtime_contract::CancellationToken::new(),
        })
        .await
        .expect("schema-aware Local Search");

    assert_eq!(turns.load(Ordering::SeqCst), 3);
    assert_eq!(output.completion, SearchCompletion::Complete);
    assert!(output.report.limitations.is_empty());
    assert_eq!(output.report.sources[0].url, "https://8.8.8.8/source");
}
