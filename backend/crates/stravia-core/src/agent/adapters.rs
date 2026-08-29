use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;

use super::{
    AgentDefinitionId, AgentDefinitionRegistry, AgentEvent, AgentInput, AgentRunner, AgentTool,
    AgentToolContext, AgentToolError, AgentTurnId, VersionedToolId,
};
use crate::Gateway;
use crate::hook::{
    ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookSession, PlatformTool,
    PlatformToolError, Principal, RequestKind, ResponsePatch, SessionContext, ToolExecutionContext,
    ToolId,
};
use crate::mcp::{McpContext, McpTool, McpToolError, McpToolOutput};
use crate::protocol::ir::ContentBlock;
use crate::proxy::context::CancellationToken;
use crate::proxy::security::Security;

const MAX_AGENT_CALL_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_AGENT_CALL_TURN_ID_BYTES: usize = 128;
const MAX_AGENT_CALL_ARTIFACT_ID_BYTES: usize = 128;
const MAX_AGENT_CALL_ARTIFACTS: usize = 16;
const MAX_AGENT_CALL_INPUT_BYTES: usize = MAX_AGENT_CALL_PROMPT_BYTES;

pub struct PlatformToolAgentAdapter {
    tool: Arc<dyn PlatformTool>,
    id: VersionedToolId,
    description: String,
}

impl PlatformToolAgentAdapter {
    pub fn new(tool: Arc<dyn PlatformTool>, version: u32) -> Self {
        let description = tool
            .description()
            .unwrap_or(tool.external_name())
            .to_owned();
        Self {
            id: VersionedToolId {
                id: tool.external_name().to_owned(),
                version,
            },
            tool,
            description,
        }
    }

    pub(crate) fn with_id(tool: Arc<dyn PlatformTool>, id: VersionedToolId) -> Self {
        let description = tool
            .description()
            .unwrap_or(tool.external_name())
            .to_owned();
        Self {
            tool,
            id,
            description,
        }
    }
}

#[async_trait]
impl AgentTool for PlatformToolAgentAdapter {
    fn id(&self) -> VersionedToolId {
        self.id.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.tool.parameters()
    }
    fn parallel_safe(&self) -> bool {
        self.tool.parallel_safe()
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError> {
        let output = self
            .tool
            .execute_result(
                input,
                ToolExecutionContext {
                    request_id: context.turn_id.to_string(),
                    run_id: context.turn_id.to_string(),
                    principal: context.principal,
                    cancellation: context.cancellation,
                    progress: None,
                },
            )
            .await
            .map_err(|error| AgentToolError::new("platform_tool_failed", error.message))?;
        let content = blocks_to_value(output.content);
        if output.is_error {
            Err(AgentToolError::new(
                "platform_tool_error",
                content.to_string(),
            ))
        } else {
            Ok(content)
        }
    }
}

pub struct McpToolAgentAdapter {
    tool: Arc<dyn McpTool>,
    id: VersionedToolId,
    description: String,
}

impl McpToolAgentAdapter {
    pub fn new(tool: Arc<dyn McpTool>, version: u32) -> Self {
        Self {
            id: VersionedToolId {
                id: tool.name().to_owned(),
                version,
            },
            description: tool.description().unwrap_or(tool.name()).to_owned(),
            tool,
        }
    }
}

#[async_trait]
impl AgentTool for McpToolAgentAdapter {
    fn id(&self) -> VersionedToolId {
        self.id.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.tool.input_schema()
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError> {
        let mcp = McpContext::new(context.principal.api_key_id().to_owned());
        if !self
            .tool
            .available(&mcp)
            .await
            .map_err(|error| AgentToolError::new(error.code, error.message))?
        {
            return Err(AgentToolError::new(
                "mcp_tool_unavailable",
                "MCP Agent Tool is unavailable for this principal",
            ));
        }
        let output = self
            .tool
            .call(input, &mcp)
            .await
            .map_err(|error| AgentToolError::new(error.code, error.message))?;
        if output.is_error {
            Err(AgentToolError::new(
                "mcp_tool_error",
                output.structured_content.to_string(),
            ))
        } else {
            Ok(output.structured_content)
        }
    }
}
#[derive(Clone)]
pub struct RemoteMcpToolSource {
    pub namespace: String,
    pub endpoint: String,
    pub bearer_token: Option<String>,
    pub version: u32,
}

pub async fn discover_remote_mcp_tools(
    source: RemoteMcpToolSource,
) -> Result<Vec<Arc<dyn AgentTool>>, AgentToolError> {
    if source.namespace.trim().is_empty()
        || source.endpoint.trim().is_empty()
        || source.version == 0
    {
        return Err(AgentToolError::new(
            "invalid_mcp_source",
            "MCP namespace, endpoint, and version must be valid",
        ));
    }
    let mut client = connect_remote_mcp(&source).await?;
    let remote_tools = client
        .peer()
        .list_all_tools()
        .await
        .map_err(|error| AgentToolError::new("mcp_discovery_failed", error.to_string()))?;
    let _ = client.close().await;
    Ok(remote_tools
        .into_iter()
        .map(|tool| {
            Arc::new(RemoteMcpAgentTool {
                id: VersionedToolId {
                    id: format!("{}.{}", source.namespace, tool.name),
                    version: source.version,
                },
                remote_name: tool.name.into_owned(),
                description: tool
                    .description
                    .map(|description| description.into_owned())
                    .unwrap_or_else(|| "Remote MCP Tool".into()),
                input_schema: Value::Object(tool.input_schema.as_ref().clone()),
                source: source.clone(),
            }) as Arc<dyn AgentTool>
        })
        .collect())
}

struct RemoteMcpAgentTool {
    id: VersionedToolId,
    remote_name: String,
    description: String,
    input_schema: Value,
    source: RemoteMcpToolSource,
}

#[async_trait]
impl AgentTool for RemoteMcpAgentTool {
    fn id(&self) -> VersionedToolId {
        self.id.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(
        &self,
        context: AgentToolContext,
        input: Value,
    ) -> Result<Value, AgentToolError> {
        let arguments = input.as_object().cloned().ok_or_else(|| {
            AgentToolError::new(
                "invalid_mcp_arguments",
                "MCP Tool arguments must be an object",
            )
        })?;
        let operation = async {
            let mut client = connect_remote_mcp(&self.source).await?;
            let result = client
                .call_tool(
                    CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments),
                )
                .await
                .map_err(|error| AgentToolError::new("mcp_call_failed", error.to_string()))?;
            let _ = client.close().await;
            let output = result
                .structured_content
                .unwrap_or_else(|| serde_json::to_value(result.content).unwrap_or(Value::Null));
            if result.is_error.unwrap_or(false) {
                Err(AgentToolError::new("mcp_tool_error", output.to_string()))
            } else {
                Ok(output)
            }
        };
        tokio::select! {
            _ = context.cancellation.cancelled() => {
                Err(AgentToolError::new("cancelled", "Remote MCP Tool cancelled"))
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(context.deadline)) => {
                context.cancellation.cancel();
                Err(AgentToolError::new("deadline_exceeded", "Remote MCP Tool deadline exceeded"))
            }
            result = operation => result,
        }
    }
}

async fn connect_remote_mcp(
    source: &RemoteMcpToolSource,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, AgentToolError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(source.endpoint.clone());
    if let Some(token) = source.bearer_token.as_ref() {
        config = config.auth_header(token.clone());
    }
    ().serve(StreamableHttpClientTransport::from_config(config))
        .await
        .map_err(|error| AgentToolError::new("mcp_connect_failed", error.to_string()))
}

pub(crate) struct AgentCallPlatformTool {
    definition_id: AgentDefinitionId,
    internal_id: ToolId,
    external_name: String,
    description: String,
    runner: AgentRunner,
}

impl AgentCallPlatformTool {
    pub(crate) fn new(
        definition_id: AgentDefinitionId,
        slug: &str,
        description: String,
        runner: AgentRunner,
    ) -> Self {
        Self {
            internal_id: ToolId::new(format!("agent-definition:{}", definition_id.as_str())),
            external_name: format!("agent_{slug}"),
            definition_id,
            description,
            runner,
        }
    }
}

#[async_trait]
impl PlatformTool for AgentCallPlatformTool {
    fn id(&self) -> ToolId {
        self.internal_id.clone()
    }

    fn external_name(&self) -> &str {
        &self.external_name
    }

    fn description(&self) -> Option<&str> {
        Some(&self.description)
    }

    fn parameters(&self) -> Value {
        agent_call_input_schema()
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        run_agent_call(
            &self.runner,
            self.definition_id.clone(),
            context.principal,
            context.cancellation,
            arguments,
        )
        .await
        .map_err(|error| PlatformToolError::new(error.message))
    }
}

pub(crate) struct AgentCallMcpTool {
    definition_id: AgentDefinitionId,
    name: String,
    description: String,
    runner: AgentRunner,
    gateway: Gateway,
}

impl AgentCallMcpTool {
    pub(crate) fn new(
        definition_id: AgentDefinitionId,
        slug: &str,
        description: String,
        runner: AgentRunner,
        gateway: Gateway,
    ) -> Self {
        Self {
            definition_id,
            name: format!("agent_{slug}"),
            description,
            runner,
            gateway,
        }
    }
}
#[async_trait]
impl McpTool for AgentCallMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> Option<&str> {
        Some(&self.description)
    }

    fn input_schema(&self) -> Value {
        agent_call_input_schema()
    }

    async fn available(&self, context: &McpContext) -> Result<bool, McpToolError> {
        let Some(api_keys) = self.gateway.storage.api_keys() else {
            return Ok(false);
        };
        let key = api_keys
            .get(&context.api_key_id)
            .await
            .map_err(|error| McpToolError::new("mcp_access_check_failed", error.to_string()))?;
        let Some(key) = key else {
            return Ok(false);
        };
        if !key.is_enabled || !key.mcp_access_enabled {
            return Ok(false);
        }

        let Some(model_id) = self.runner.definition_model(&self.definition_id).await else {
            return Ok(false);
        };
        let model = self
            .gateway
            .model_cache
            .read()
            .await
            .models
            .iter()
            .find(|model| model.id == model_id)
            .cloned();
        let Some(model) = model else {
            return Ok(false);
        };
        Ok(Security::new(self.gateway.storage.auth())
            .authorize_principal_model(&Principal::new(context.api_key_id.clone()), &model)
            .await
            .is_ok())
    }

    async fn call(
        &self,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        let output = run_agent_call(
            &self.runner,
            self.definition_id.clone(),
            Principal::new(context.api_key_id.clone()),
            CancellationToken::new(),
            arguments,
        )
        .await
        .map_err(|error| McpToolError::new("agent_call_failed", error.message))?;
        Ok(McpToolOutput::success(output))
    }
}

async fn run_agent_call(
    runner: &AgentRunner,
    definition_id: AgentDefinitionId,
    principal: Principal,
    cancellation: CancellationToken,
    arguments: Value,
) -> Result<Value, super::AgentRunError> {
    let ParsedAgentCallInput {
        prompt,
        parent_turn_id,
        artifacts,
    } = parse_agent_call_input(arguments)?;
    let mut events = runner.run(AgentInput {
        principal,
        definition_id,
        parent_turn_id,
        prompt,
        artifacts,
        cancellation,
    });
    while let Some(event) = events.next().await {
        match event {
            AgentEvent::Completed(result) | AgentEvent::Partial(result) => {
                return Ok(serde_json::json!({
                    "turn_id": result.turn_id.as_str(),
                    "completion": result.completion,
                    "output": result.output,
                }));
            }
            AgentEvent::Failed { error } => return Err(error),
            _ => {}
        }
    }
    Err(super::AgentRunError::new(
        "agent_stream_incomplete",
        "Agent Run ended without a terminal event",
    ))
}

struct ParsedAgentCallInput {
    prompt: String,
    parent_turn_id: Option<AgentTurnId>,
    artifacts: Vec<super::ArtifactId>,
}

fn parse_agent_call_input(arguments: Value) -> Result<ParsedAgentCallInput, super::AgentRunError> {
    let object = arguments.as_object().ok_or_else(|| {
        super::AgentRunError::new("invalid_agent_input", "Agent input must be an object")
    })?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "prompt" | "previous_turn_id" | "artifacts"))
    {
        return Err(super::AgentRunError::new(
            "invalid_agent_input",
            "Agent input contains an unsupported field",
        ));
    }

    let prompt = object
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            super::AgentRunError::new("invalid_agent_input", "prompt must be a non-empty string")
        })?;
    let previous_turn_id = match object.get("previous_turn_id") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    super::AgentRunError::new(
                        "invalid_agent_input",
                        "previous_turn_id must be a non-empty string",
                    )
                })?,
        ),
    };
    let artifact_values = object
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            super::AgentRunError::new(
                "invalid_agent_input",
                "artifacts must be an array of artifact objects",
            )
        })?;
    if artifact_values.len() > MAX_AGENT_CALL_ARTIFACTS {
        return Err(super::AgentRunError::new(
            "invalid_agent_input",
            format!("artifacts must contain at most {MAX_AGENT_CALL_ARTIFACTS} items"),
        ));
    }

    let mut total_bytes = 0;
    validate_agent_call_string(
        &mut total_bytes,
        "prompt",
        prompt,
        MAX_AGENT_CALL_PROMPT_BYTES,
    )?;
    if let Some(previous_turn_id) = previous_turn_id {
        validate_agent_call_string(
            &mut total_bytes,
            "previous_turn_id",
            previous_turn_id,
            MAX_AGENT_CALL_TURN_ID_BYTES,
        )?;
    }

    let mut artifact_ids = Vec::with_capacity(artifact_values.len());
    for (index, artifact) in artifact_values.iter().enumerate() {
        let artifact = artifact.as_object().ok_or_else(|| {
            super::AgentRunError::new(
                "invalid_agent_input",
                format!("artifacts[{index}] must be an object"),
            )
        })?;
        if artifact.keys().any(|key| key.as_str() != "artifact_id") {
            return Err(super::AgentRunError::new(
                "invalid_agent_input",
                format!("artifacts[{index}] contains an unsupported field"),
            ));
        }
        let artifact_id = artifact
            .get("artifact_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                super::AgentRunError::new(
                    "invalid_agent_input",
                    format!("artifacts[{index}].artifact_id must be a non-empty string"),
                )
            })?;
        validate_agent_call_string(
            &mut total_bytes,
            &format!("artifacts[{index}].artifact_id"),
            artifact_id,
            MAX_AGENT_CALL_ARTIFACT_ID_BYTES,
        )?;
        artifact_ids.push(artifact_id);
    }

    Ok(ParsedAgentCallInput {
        prompt: prompt.to_owned(),
        parent_turn_id: previous_turn_id.map(AgentTurnId::new),
        artifacts: artifact_ids
            .into_iter()
            .map(super::ArtifactId::new)
            .collect(),
    })
}

fn validate_agent_call_string(
    total_bytes: &mut usize,
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), super::AgentRunError> {
    if value.len() > max_bytes {
        return Err(super::AgentRunError::new(
            "invalid_agent_input",
            format!("{field} exceeds the {max_bytes}-byte limit"),
        ));
    }
    *total_bytes = total_bytes.checked_add(value.len()).ok_or_else(|| {
        super::AgentRunError::new("invalid_agent_input", "Agent input byte length overflow")
    })?;
    if *total_bytes > MAX_AGENT_CALL_INPUT_BYTES {
        return Err(super::AgentRunError::new(
            "invalid_agent_input",
            format!("Agent input strings exceed the {MAX_AGENT_CALL_INPUT_BYTES}-byte limit"),
        ));
    }
    Ok(())
}

fn blocks_to_value(blocks: Vec<ContentBlock>) -> Value {
    if let [ContentBlock::Unknown { raw }] = blocks.as_slice() {
        raw.clone()
    } else if let [ContentBlock::Text { text, .. }] = blocks.as_slice() {
        Value::String(text.clone())
    } else {
        serde_json::to_value(blocks).unwrap_or(Value::Null)
    }
}

fn agent_call_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_AGENT_CALL_PROMPT_BYTES
            },
            "previous_turn_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_AGENT_CALL_TURN_ID_BYTES
            },
            "artifacts": {
                "type": "array",
                "maxItems": MAX_AGENT_CALL_ARTIFACTS,
                "items": {
                    "type": "object",
                    "properties": {
                        "artifact_id": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_AGENT_CALL_ARTIFACT_ID_BYTES
                        }
                    },
                    "required": ["artifact_id"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["prompt", "artifacts"],
        "additionalProperties": false
    })
}

pub(crate) struct AgentDefinitionHook {
    definitions: AgentDefinitionRegistry,
}

impl AgentDefinitionHook {
    pub(crate) fn new(definitions: AgentDefinitionRegistry) -> Self {
        Self { definitions }
    }
}

impl Hook for AgentDefinitionHook {
    fn descriptor(&self) -> HookDescriptor {
        HookDescriptor {
            id: crate::hook::HookId::new("agent-definitions"),
            request_kinds: vec![RequestKind::Generation],
            event_kinds: vec![
                EventKind::Request,
                EventKind::ToolResult,
                EventKind::ClientOutput,
            ],
            requires_full_context: false,
            max_buffered_bytes: 0,
            max_delayed_events: 0,
        }
    }

    fn create_session(&self, context: &SessionContext) -> Box<dyn HookSession> {
        Box::new(AgentDefinitionHookSession {
            definitions: self.definitions.clone(),
            expose_turn_ids: context.ingress == crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
            turn_ids: Vec::new(),
        })
    }
}

struct AgentDefinitionHookSession {
    definitions: AgentDefinitionRegistry,
    expose_turn_ids: bool,
    turn_ids: Vec<String>,
}

#[async_trait]
impl HookSession for AgentDefinitionHookSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        match event {
            HookEvent::Request { .. } => {
                let actions = self
                    .definitions
                    .list_public()
                    .await
                    .into_iter()
                    .filter(|record| record.config.enabled && record.config.model_id.is_some())
                    .map(|record| {
                        HookAction::ExposeTool(ToolId::new(format!(
                            "agent-definition:{}",
                            record.spec.id.as_str()
                        )))
                    })
                    .collect();
                Ok(ActionBatch { actions })
            }
            HookEvent::ToolResult { result, .. }
                if self.expose_turn_ids
                    && result.tool_id.as_str().starts_with("agent-definition:") =>
            {
                if let Some(turn_id) = result.content.get("turn_id").and_then(Value::as_str) {
                    self.turn_ids.push(turn_id.to_owned());
                }
                Ok(ActionBatch::default())
            }
            HookEvent::ClientOutput { response, .. } if !self.turn_ids.is_empty() => {
                let mut response = response.clone();
                response
                    .items
                    .extend(self.turn_ids.drain(..).map(|turn_id| {
                        crate::protocol::ir::AiItem::unknown(serde_json::json!({
                            "id": format!("agent_{turn_id}"),
                            "type": "stravia:agent_result",
                            "status": "completed",
                            "turn_id": turn_id,
                        }))
                    }));
                Ok(ActionBatch::one(HookAction::PatchResponse(
                    ResponsePatch::ReplaceCanonical(Box::new(response)),
                )))
            }
            _ => Ok(ActionBatch::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(
        prompt: String,
        previous_turn_id: Option<String>,
        artifacts: Vec<String>,
    ) -> Value {
        let mut value = serde_json::json!({
            "prompt": prompt,
            "artifacts": artifacts
                .into_iter()
                .map(|artifact_id| serde_json::json!({"artifact_id": artifact_id}))
                .collect::<Vec<_>>()
        });
        if let Some(previous_turn_id) = previous_turn_id {
            value["previous_turn_id"] = Value::String(previous_turn_id);
        }
        value
    }

    #[test]
    fn agent_call_schema_declares_input_limits() {
        let schema = agent_call_input_schema();
        let properties = &schema["properties"];
        assert_eq!(
            properties["prompt"]["maxLength"],
            MAX_AGENT_CALL_PROMPT_BYTES
        );
        assert_eq!(
            properties["previous_turn_id"]["maxLength"],
            MAX_AGENT_CALL_TURN_ID_BYTES
        );
        assert_eq!(
            properties["artifacts"]["maxItems"],
            MAX_AGENT_CALL_ARTIFACTS
        );
        assert_eq!(
            properties["artifacts"]["items"]["properties"]["artifact_id"]["maxLength"],
            MAX_AGENT_CALL_ARTIFACT_ID_BYTES
        );
    }

    #[test]
    fn parse_rejects_oversized_prompt_before_materializing_agent_input() {
        let error = parse_agent_call_input(arguments(
            "x".repeat(MAX_AGENT_CALL_PROMPT_BYTES + 1),
            None,
            Vec::new(),
        ))
        .err()
        .expect("oversized prompt must be rejected");
        assert_eq!(error.code, "invalid_agent_input");
    }

    #[test]
    fn parse_rejects_oversized_previous_turn_id() {
        let error = parse_agent_call_input(arguments(
            "question".into(),
            Some("t".repeat(MAX_AGENT_CALL_TURN_ID_BYTES + 1)),
            Vec::new(),
        ))
        .err()
        .expect("oversized previous turn ID must be rejected");
        assert_eq!(error.code, "invalid_agent_input");
    }

    #[test]
    fn parse_rejects_oversized_artifact_id() {
        let error = parse_agent_call_input(arguments(
            "question".into(),
            None,
            vec!["a".repeat(MAX_AGENT_CALL_ARTIFACT_ID_BYTES + 1)],
        ))
        .err()
        .expect("oversized artifact ID must be rejected");
        assert_eq!(error.code, "invalid_agent_input");
    }

    #[test]
    fn parse_rejects_excess_artifacts_before_materializing_agent_input() {
        let error = parse_agent_call_input(arguments(
            "question".into(),
            None,
            (0..=MAX_AGENT_CALL_ARTIFACTS)
                .map(|index| format!("artifact-{index}"))
                .collect(),
        ))
        .err()
        .expect("excess artifacts must be rejected");
        assert_eq!(error.code, "invalid_agent_input");
    }

    #[test]
    fn parse_rejects_prompt_and_ids_over_total_byte_limit() {
        let error = parse_agent_call_input(arguments(
            "x".repeat(MAX_AGENT_CALL_INPUT_BYTES - MAX_AGENT_CALL_TURN_ID_BYTES),
            Some("t".repeat(MAX_AGENT_CALL_TURN_ID_BYTES)),
            vec!["artifact".to_string()],
        ))
        .err()
        .expect("input byte budget must be enforced");
        assert_eq!(error.code, "invalid_agent_input");
    }
}
