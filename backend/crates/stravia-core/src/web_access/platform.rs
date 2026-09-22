use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use stravia_runtime_contract::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, ToolExecutionContext, ToolId,
};
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_web_access_contract::{
    DEFAULT_SEARCH_RESULTS, STRAVIA_READ_TOOL_ID, STRAVIA_READ_TOOL_NAME,
    read_path::{ReadInput, ReadTarget, input_schema, parse_read_path},
};

use super::{SearchRequest, WebAccessError};

pub(crate) fn internal_platform_tools(gateway: &crate::Gateway) -> Vec<Arc<dyn PlatformTool>> {
    vec![Arc::new(InternalReadTool {
        gateway: gateway.clone(),
    })]
}

struct InternalReadTool {
    gateway: crate::Gateway,
}

#[async_trait]
impl PlatformTool for InternalReadTool {
    fn id(&self) -> ToolId {
        ToolId::new(STRAVIA_READ_TOOL_ID)
    }

    fn external_name(&self) -> &str {
        STRAVIA_READ_TOOL_NAME
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Read a single path. search:// followed by percent-encoded search text performs basic public web retrieval, never a research Agent. Public HTTP(S) pages return Markdown; images and files follow platform artifact rules. HTTP(S) resource options use #stravia?, while Artifact Reference options use ?question= or ?download=1.",
        )
    }

    fn parameters(&self) -> Value {
        input_schema()
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        self.execute_result(arguments, context)
            .await
            .and_then(output_value)
    }

    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        let request: ReadInput = match serde_json::from_value(arguments) {
            Ok(request) => request,
            Err(error) => {
                return Ok(web_access_error_output(WebAccessError::invalid(format!(
                    "invalid StraviaRead arguments: {error}"
                ))));
            }
        };
        let target = parse_read_path(&request.path)
            .map_err(|error| PlatformToolError::new(error.to_string()))?;
        let ReadTarget::Search(search) = target else {
            return crate::mcp::read::execute_internal_read(&self.gateway, request.path, context)
                .await;
        };
        if search.previous_path.is_some() {
            return Err(PlatformToolError::new(
                "Internal retrieval does not accept previous_path",
            ));
        }
        if !crate::mcp::read::networking_available(&self.gateway, &context.principal).await {
            return Err(PlatformToolError::new(
                "Networking capability is unavailable",
            ));
        }
        let response = match self
            .gateway
            .web_access()
            .search_in_run(
                &context.run_id,
                context.principal.api_key_id(),
                SearchRequest {
                    query: search.query,
                    max_results: DEFAULT_SEARCH_RESULTS,
                    allowed_domains: search.allowed_domains.unwrap_or_default(),
                },
            )
            .await
        {
            Ok(response) => response,
            Err(error) => return Ok(web_access_error_output(error)),
        };
        let value = serde_json::to_value(response).map_err(|error| {
            PlatformToolError::new(format!("StraviaRead result encoding failed: {error}"))
        })?;
        Ok(PlatformToolOutput {
            content: vec![ContentBlock::Unknown { raw: value }],
            is_error: false,
            metadata: serde_json::Map::new(),
        })
    }
}

fn web_access_error_output(error: WebAccessError) -> PlatformToolOutput {
    PlatformToolOutput {
        content: vec![ContentBlock::Unknown {
            raw: serde_json::json!({
                "error": {
                    "code": error.code,
                    "message": error.message,
                }
            }),
        }],
        is_error: true,
        metadata: serde_json::Map::new(),
    }
}

fn output_value(output: PlatformToolOutput) -> Result<Value, PlatformToolError> {
    let Some(ContentBlock::Unknown { raw }) = output.content.into_iter().next() else {
        return Err(PlatformToolError::new(
            "StraviaRead returned no structured output",
        ));
    };
    if output.is_error {
        Err(PlatformToolError::new(raw.to_string()))
    } else {
        Ok(raw)
    }
}
