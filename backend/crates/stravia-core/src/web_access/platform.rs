use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use stravia_runtime_contract::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, ToolExecutionContext, ToolId,
};
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_web_access_contract::{
    DEFAULT_SEARCH_RESULTS, STRAVIA_READ_TOOL_ID, STRAVIA_READ_TOOL_NAME,
    read_path::{ReadTarget, decode_read_input, model_input_schema, parse_read_input},
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
            "Read a single path with optional body (object or null); omit it for ordinary reads. For basic public web retrieval use path stravia://search and body.question as plain text, optionally body.allowed_domains as an array of domains; this never starts a research Agent. Public HTTP(S) pages return Markdown and body.question does not answer questions about HTML. Images and files follow platform artifact rules; use body.question for media. Resource paths accept :raw, :download or :1-50 selectors. For media continuation use a turn path with body.question and optional body.attachments. Research continuation is not supported here.",
        )
    }

    fn parameters(&self) -> Value {
        model_input_schema()
    }

    fn strict_schema(&self) -> bool {
        false
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
        let request = match decode_read_input(arguments) {
            Ok(request) => request,
            Err(error) => {
                return Ok(web_access_error_output(WebAccessError::invalid(format!(
                    "invalid StraviaRead arguments: {error}"
                ))));
            }
        };
        let target = parse_read_input(&request)
            .map_err(|error| PlatformToolError::new(error.to_string()))?;
        // 搜索续接由内部路由器统一拒绝（需要 Turn 类型），媒体续接与资源读取一并交给它。
        let ReadTarget::Search(search) = target else {
            return crate::mcp::read::execute_internal_read(&self.gateway, request, context).await;
        };
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
