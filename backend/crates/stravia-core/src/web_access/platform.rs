use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, Principal, ToolExecutionContext, ToolId,
};
use crate::protocol::ir::ContentBlock;

use super::{
    FetchRequest, SearchRequest, WEB_FETCH_NAME, WEB_SEARCH_NAME, WebAccessError, WebAccessService,
};

pub(crate) const WEB_SEARCH_TOOL_ID: &str = "web-access.search";
pub(crate) const WEB_FETCH_TOOL_ID: &str = "web-access.fetch";

pub(crate) fn internal_platform_tools(gateway: &crate::Gateway) -> Vec<Arc<dyn PlatformTool>> {
    let service = gateway.web_access();
    vec![
        Arc::new(WebSearchTool {
            service: service.clone(),
        }),
        Arc::new(WebFetchTool { service }),
    ]
}

struct WebSearchTool {
    service: WebAccessService,
}

#[async_trait]
impl PlatformTool for WebSearchTool {
    fn id(&self) -> ToolId {
        ToolId::new(WEB_SEARCH_TOOL_ID)
    }

    fn external_name(&self) -> &str {
        WEB_SEARCH_NAME
    }

    fn description(&self) -> Option<&str> {
        Some("Search the public web and return normalized results with source URLs.")
    }

    fn parameters(&self) -> Value {
        super::search_input_schema()
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
        let api_key_id = require_api_key(&context.principal)?;
        let request: SearchRequest = match serde_json::from_value(arguments) {
            Ok(request) => request,
            Err(error) => {
                return Ok(web_access_error_output(WebAccessError::invalid(format!(
                    "invalid web_search arguments: {error}"
                ))));
            }
        };
        let response = match self
            .service
            .search_in_run(&context.run_id, api_key_id, request)
            .await
        {
            Ok(response) => response,
            Err(error) => return Ok(web_access_error_output(error)),
        };
        let value = serde_json::to_value(response).map_err(|error| {
            PlatformToolError::new(format!("web_search result encoding failed: {error}"))
        })?;
        Ok(success_output(value))
    }
}

struct WebFetchTool {
    service: WebAccessService,
}

#[async_trait]
impl PlatformTool for WebFetchTool {
    fn id(&self) -> ToolId {
        ToolId::new(WEB_FETCH_TOOL_ID)
    }

    fn external_name(&self) -> &str {
        WEB_FETCH_NAME
    }

    fn description(&self) -> Option<&str> {
        Some("Fetch readable content from one or more public HTTP(S) URLs.")
    }

    fn parameters(&self) -> Value {
        super::fetch_input_schema()
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
        let api_key_id = require_api_key(&context.principal)?;
        let request: FetchRequest = match serde_json::from_value(arguments) {
            Ok(request) => request,
            Err(error) => {
                return Ok(web_access_error_output(WebAccessError::invalid(format!(
                    "invalid web_fetch arguments: {error}"
                ))));
            }
        };
        let response = match self
            .service
            .fetch_in_run(&context.run_id, api_key_id, request)
            .await
        {
            Ok(response) => response,
            Err(error) => return Ok(web_access_error_output(error)),
        };
        let is_error = response.is_execution_error();
        let value = serde_json::to_value(response).map_err(|error| {
            PlatformToolError::new(format!("web_fetch result encoding failed: {error}"))
        })?;
        Ok(PlatformToolOutput {
            content: vec![ContentBlock::Unknown { raw: value }],
            is_error,
            metadata: serde_json::Map::new(),
        })
    }
}

fn require_api_key(principal: &Principal) -> Result<&str, PlatformToolError> {
    Ok(principal.api_key_id())
}

fn success_output(value: Value) -> PlatformToolOutput {
    PlatformToolOutput {
        content: vec![ContentBlock::Unknown { raw: value }],
        is_error: false,
        metadata: serde_json::Map::new(),
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
            "Web Access returned no structured output",
        ));
    };
    if output.is_error {
        Err(PlatformToolError::new(raw.to_string()))
    } else {
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn internal_leaves_keep_bounded_search_and_fetch_contracts() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (gateway, _logs) = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let tools = internal_platform_tools(&gateway);

        assert_eq!(tools[0].id().as_str(), WEB_SEARCH_TOOL_ID);
        assert_eq!(tools[0].external_name(), "web_search");
        assert_eq!(
            tools[0].parameters()["properties"]["max_results"]["maximum"],
            20
        );
        assert_eq!(tools[1].id().as_str(), WEB_FETCH_TOOL_ID);
        assert_eq!(tools[1].external_name(), "web_fetch");
        assert_eq!(tools[1].parameters()["properties"]["urls"]["maxItems"], 20);
        assert_ne!(
            tools[0].id().as_str(),
            crate::web_search::PUBLIC_WEB_SEARCH_TOOL_ID
        );
        assert_ne!(
            tools[1].id().as_str(),
            crate::web_search::PUBLIC_WEB_SEARCH_TOOL_ID
        );
    }
}
