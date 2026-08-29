use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::hook::Principal;
use crate::mcp::{McpContext, McpTool, McpToolError, McpToolOutput};

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
        super::platform::MEDIA_TOOL_NAME
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Understand static JPEG, PNG, or WebP Artifacts using OCR, description, comparison, or visual reasoning.",
        )
    }

    fn input_schema(&self) -> Value {
        super::platform::input_schema()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(super::platform::output_schema())
    }
    fn await_cancellation_cleanup(&self) -> bool {
        true
    }

    fn deadline(&self) -> Duration {
        Duration::from_secs(120)
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
        let (cancellation, deadline) = context.execution().ok_or_else(|| {
            McpToolError::new(
                "media_execution_context_missing",
                "Media MCP execution context is unavailable",
            )
        })?;
        match super::platform::execute_until(
            &self.gateway,
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
        let schema = super::super::platform::output_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"]["report"].is_object());
        assert_eq!(schema["required"][0], "turn_id");
    }
}
