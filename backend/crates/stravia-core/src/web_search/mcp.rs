use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::hook::Principal;
use crate::mcp::{McpContext, McpTool, McpToolError, McpToolOutput};
use crate::proxy::context::CancellationToken;

pub(crate) fn tools(gateway: &crate::Gateway) -> Vec<Arc<dyn McpTool>> {
    vec![Arc::new(McpWebSearch {
        gateway: gateway.clone(),
    })]
}

struct McpWebSearch {
    gateway: crate::Gateway,
}

#[async_trait]
impl McpTool for McpWebSearch {
    fn name(&self) -> &str {
        super::platform::PUBLIC_WEB_SEARCH_TOOL_NAME
    }

    fn description(&self) -> Option<&str> {
        Some("Search the public web and return a complete sourced report.")
    }

    fn input_schema(&self) -> Value {
        super::input_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "turn_id": { "type": "string" },
                "completion": { "type": "string", "enum": ["complete", "partial"] },
                "report": super::local::search_report_schema()
            },
            "required": ["turn_id", "completion", "report"],
            "additionalProperties": false
        }))
    }

    fn deadline(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn available(&self, context: &McpContext) -> Result<bool, McpToolError> {
        let Some(api_keys) = self.gateway.storage.api_keys() else {
            return Ok(false);
        };
        let key = api_keys
            .get(&context.api_key_id)
            .await
            .map_err(|error| McpToolError::new("mcp_access_check_failed", error.to_string()))?;
        if !key.is_some_and(|key| key.is_enabled && key.mcp_access_enabled) {
            return Ok(false);
        }
        Ok(super::platform::is_available(
            &self.gateway,
            &Principal::new(context.api_key_id.clone()),
        )
        .await)
    }

    async fn call(
        &self,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        match super::execute_public_search(
            &self.gateway,
            arguments,
            Principal::new(context.api_key_id.clone()),
            CancellationToken::new(),
            None,
        )
        .await
        {
            Ok(result) => Ok(McpToolOutput::success(result)),
            Err(error) => Ok(McpToolOutput::execution_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::web_search::{
        BackendOutput, SearchBackend, SearchBackendInput, SearchCompletion, SearchEvidence,
        SearchEvidenceSet, SearchReport, SearchReportValidator, SearchSource,
        WebSearchBackendDraft, WebSearchBackendKind, WebSearchConfig, WebSearchRunner,
    };

    struct SuccessfulBackend;

    #[async_trait]
    impl SearchBackend for SuccessfulBackend {
        fn kind(&self) -> WebSearchBackendKind {
            WebSearchBackendKind::Local
        }

        async fn run(
            &self,
            input: SearchBackendInput,
        ) -> Result<BackendOutput, crate::web_search::WebSearchError> {
            let source_id = format!("source-{}-1", input.turn_id);
            Ok(BackendOutput {
                completion: SearchCompletion::Complete,
                partial_cause: None,
                report: SearchReport {
                    answer: format!("Verified answer [{source_id}]"),
                    sources: vec![SearchSource {
                        id: source_id,
                        url: "https://8.8.8.8/source".into(),
                        title: Some("Source".into()),
                    }],
                    limitations: Vec::new(),
                },
                evidence: SearchEvidenceSet::from_evidence([SearchEvidence {
                    url: "https://8.8.8.8/source".into(),
                    title: Some("Source".into()),
                }]),
                usage: Default::default(),
                model_turns: 1,
                tool_calls: 2,
            })
        }
    }

    #[tokio::test]
    async fn direct_mcp_call_returns_the_terminal_structured_search_result() {
        use crate::web_search::config::SettingsWebSearchConfigStore;

        let directory = tempfile::tempdir().expect("temporary directory");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let key = gateway
            .admin()
            .create_api_key(crate::db::models::CreateApiKey {
                key: None,
                name: "Search MCP key".into(),
                concurrency_limit: None,
                expires_at: None,
                mcp_access_enabled: false,
                transparent_injection_enabled: false,
                inject_web_search: false,
                inject_media_understanding: false,
                model_ids: Vec::new(),
            })
            .await
            .expect("API key");
        let config_store = Arc::new(SettingsWebSearchConfigStore::new(gateway.storage.clone()));
        config_store
            .save(&WebSearchConfig {
                revision: 1,
                enabled: true,
                backend: Some(WebSearchBackendDraft::Local {
                    model_id: Some("search-model".into()),
                }),
                max_turns: 12,
                total_time_seconds: 600,
                updated_at: "2026-08-11T00:00:00Z".into(),
            })
            .await
            .expect("Search config");
        let backend = Arc::new(SuccessfulBackend);
        let runner = WebSearchRunner::new(
            config_store.clone(),
            Arc::new(crate::turn_chain::test_store().await),
            backend.clone(),
            backend,
            Arc::new(SearchReportValidator),
            Duration::from_secs(7 * 24 * 60 * 60),
            Arc::new(crate::web_search::AllowSearchRun),
        );
        *gateway.web_search_runner_state.write().await = Some(runner);

        let registered = tools(&gateway);
        assert_eq!(
            registered
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            ["web_search"]
        );
        let context = McpContext::new(key.id);
        assert!(
            !registered[0]
                .available(&context)
                .await
                .expect("MCP-disabled availability")
        );
        gateway
            .admin()
            .update_api_key(
                &context.api_key_id,
                crate::db::models::UpdateApiKey {
                    key: None,
                    name: None,
                    concurrency_limit: None,
                    is_enabled: None,
                    mcp_access_enabled: Some(true),
                    transparent_injection_enabled: None,
                    inject_web_search: None,
                    inject_media_understanding: None,
                    expires_at: None,
                    model_ids: None,
                },
            )
            .await
            .expect("enable MCP access");
        assert!(
            registered[0]
                .available(&context)
                .await
                .expect("availability")
        );
        let output = registered[0]
            .call(serde_json::json!({ "query": "Search the claim" }), &context)
            .await
            .expect("MCP tool outcome");

        assert!(!output.is_error);
        assert_eq!(output.structured_content["completion"], "complete");
        assert!(
            output.structured_content["turn_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("wst_"))
        );
        assert_eq!(
            output.structured_content["report"]["sources"][0]["url"],
            "https://8.8.8.8/source"
        );

        config_store
            .save(&WebSearchConfig {
                revision: 2,
                enabled: false,
                backend: Some(WebSearchBackendDraft::Local {
                    model_id: Some("search-model".into()),
                }),
                max_turns: 12,
                total_time_seconds: 600,
                updated_at: "2026-08-11T00:01:00Z".into(),
            })
            .await
            .expect("disabled Search config");
        assert!(
            !registered[0]
                .available(&context)
                .await
                .expect("disabled availability")
        );
    }
}
