use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use stravia_runtime_contract::hook::{
    PlatformTool, PlatformToolError, PlatformToolOutput, ToolExecutionContext, ToolId,
};
use stravia_runtime_contract::protocol::ir::ContentBlock;
use stravia_web_access_contract::{
    DEFAULT_SEARCH_RESULTS, STRAVIA_READ_TOOL_ID, STRAVIA_READ_TOOL_NAME,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    url: String,
}

pub(crate) fn decode_query_url(url: &str) -> Option<String> {
    let query = url.strip_prefix("query://")?;
    // Encode delimiters before using form decoding: the entire suffix is search text.
    let encoded = format!("q={}", query.replace('&', "%26"));
    Some(
        url::form_urlencoded::parse(encoded.as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default(),
    )
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
            "Read a URL. query:// followed by URL-encoded search text performs basic public web retrieval, never a research Agent. Public HTTP(S) pages return Markdown; images and files follow platform artifact rules. Artifact references support download or an explicit question parameter.",
        )
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "url": { "type": "string" } },
            "required": ["url"],
            "additionalProperties": false
        })
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
        let request: ReadRequest = match serde_json::from_value(arguments) {
            Ok(request) => request,
            Err(error) => {
                return Ok(web_access_error_output(WebAccessError::invalid(format!(
                    "invalid StraviaRead arguments: {error}"
                ))));
            }
        };
        let Some(query) = decode_query_url(&request.url) else {
            return crate::mcp::read::execute_internal_read(&self.gateway, request.url, context)
                .await;
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
                    query,
                    max_results: DEFAULT_SEARCH_RESULTS,
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_urls_decode_search_text_without_interpreting_url_parameters() {
        assert_eq!(
            decode_query_url("query://Rust%20%26%20C%2B%2B?year=2026"),
            Some("Rust & C++?year=2026".into())
        );
        assert_eq!(decode_query_url("query://a&b"), Some("a&b".into()));
        assert_eq!(decode_query_url("https://example.com/?question=test"), None);
    }
}
