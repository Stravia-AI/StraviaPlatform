use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use stravia_runtime_contract::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, ReadExposureScope, ToolExecutionContext,
    ToolId,
};
use stravia_web_access_contract::{
    DEFAULT_SEARCH_RESULTS, STRAVIA_READ_TOOL_ID, STRAVIA_READ_TOOL_NAME,
    read_path::{
        ReadTarget, decode_read_input, is_history_path, model_input_schema, parse_read_input,
    },
};

use super::{SearchRequest, WebAccessError};

pub(crate) fn internal_platform_tools(gateway: &crate::Gateway) -> Vec<Arc<dyn PlatformTool>> {
    vec![Arc::new(InternalReadTool {
        gateway: gateway.clone(),
        reader: crate::mcp::read::ReadTool::internal(gateway),
    })]
}

struct InternalReadTool {
    gateway: crate::Gateway,
    reader: crate::mcp::read::ReadTool,
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
        mut context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        let request = match decode_read_input(arguments) {
            Ok(request) => request,
            Err(error) => {
                return Ok(web_access_error_output(WebAccessError::invalid(format!(
                    "invalid StraviaRead arguments: {error}"
                ))));
            }
        };
        if is_history_path(&request.path) {
            context.read_scope = ReadExposureScope::FULL;
            return self.reader.read_input(request, context).await;
        }
        let target = parse_read_input(&request)
            .map_err(|error| PlatformToolError::new(error.to_string()))?;
        // 搜索续接由内部路由器统一拒绝（需要 Turn 类型），媒体续接与资源读取一并交给它。
        let ReadTarget::Search(search) = target else {
            context.read_scope = ReadExposureScope::FULL;
            return self.reader.read_target(target, context).await;
        };
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
        Ok(PlatformToolOutput::ok(value))
    }
}

fn web_access_error_output(error: WebAccessError) -> PlatformToolOutput {
    PlatformToolOutput::error(serde_json::json!({
        "error": {
            "code": error.code,
            "message": error.message,
        }
    }))
}
