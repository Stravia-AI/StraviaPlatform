use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::{McpContext, McpTool, McpToolError, McpToolOutput};
use stravia_runtime_contract::Principal;

pub(crate) fn tools(gateway: &crate::Gateway) -> Vec<Arc<dyn McpTool>> {
    vec![Arc::new(McpMediaUnderstanding {
        gateway: gateway.clone(),
    })]
}

struct McpMediaUnderstanding {
    gateway: crate::Gateway,
}

#[async_trait]
impl McpTool for McpMediaUnderstanding {
    fn name(&self) -> &str {
        stravia_media::platform::MEDIA_TOOL_NAME
    }

    fn description(&self) -> Option<&str> {
        Some(stravia_media::platform::MEDIA_TOOL_DESCRIPTION)
    }

    fn input_schema(&self) -> Value {
        stravia_media::platform::input_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(stravia_media::platform::output_schema())
    }
    fn await_cancellation_cleanup(&self) -> bool {
        true
    }

    fn deadline(&self) -> Duration {
        stravia_media::MEDIA_TOTAL_WALL_TIME
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
        Ok(stravia_media::platform::is_available(
            &super::runtime(&self.gateway),
            &Principal::new(context.api_key_id.clone()),
        )
        .await)
    }

    async fn call(
        &self,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        let (cancellation, deadline) = context.execution().ok_or_else(|| {
            McpToolError::new(
                "media_execution_context_missing",
                "Media MCP execution context is unavailable",
            )
        })?;
        match stravia_media::platform::execute_until(
            &super::runtime(&self.gateway),
            arguments,
            Principal::new(context.api_key_id.clone()),
            cancellation,
            deadline,
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

    #[test]
    fn mcp_media_schema_returns_full_report_contract() {
        let schema = stravia_media::platform::output_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"]["report"].is_object());
        assert_eq!(schema["required"][0], "turn_id");
    }
}
