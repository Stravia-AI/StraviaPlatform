use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use stravia_runtime_contract::{
    CancellationToken, Principal,
    hook::{
        ActionBatch, EventKind, Hook, HookAction, HookDescriptor, HookEvent, HookId, HookRejection,
        HookSession, PlatformTool, PlatformToolError, PlatformToolOutput, RequestKind,
        RequestPatch, SessionContext, ToolExecutionContext, ToolId,
    },
    protocol::ir::{ContentBlock, ToolChoice},
};

use super::{GenerationError, config, execution};
use crate::{
    Gateway,
    mcp::{McpContext, McpTool, McpToolError, McpToolOutput},
    proxy::security::Security,
};

const TOOL_ID: &str = "media-generation";
const TOOL_NAME: &str = "generate";
const LIMIT: Duration = Duration::from_secs(15 * 60);
const DESCRIPTION: &str = "Generate one image from a prompt, optionally using up to five ordered JPEG/PNG/WebP reference images (owned plain stravia://artifacts/<artifact-id> paths without selectors, or public HTTP(S) image URLs; at most 32 MiB and 25 megapixels each). aspect_ratio and resolution are preferences mapped by the selected image Provider and are not exact guarantees. Returns a stable Artifact path, actual image dimensions, and a 15-minute bearer download_url. Display the image using Markdown ![description](download_url \"stravia://artifacts/<artifact-id>\"), substituting the returned download_url and path; keep the stable path in the title for download recovery and reuse it as a reference image. No extra Read is needed to display a fresh result; use StraviaRead with the path plus the :download selector when a new download link is needed. If link issuance fails after storage, the structured error includes the retained artifact; do not regenerate it. Waits up to 60 seconds, then returns an execution path and status; read that path with StraviaRead and optionally set wait to a number of seconds for status and the original result or error. Cancelling a query stops only its wait. Local cancellation does not guarantee upstream generation or quota consumption stops; retries may repeat generation and consume quota.";

pub(crate) fn input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "type":{"type":"string","enum":["image"]},
            "input":{
                "type":"object",
                "properties":{
                    "prompt":{"type":"string","minLength":1},
                    "aspect_ratio":{"type":"string","enum":["1:1","3:4","4:3","9:16","16:9"]},
                    "resolution":{"type":"string","enum":["1K","2K","4K"]},
                    "reference_images":{"type":"array","maxItems":super::execution::MAX_REFERENCE_IMAGES,"items":{"type":"string","minLength":1}}
                },
                "required":["prompt"],
                "additionalProperties":false
            }
        },
        "required":["type","input"],
        "additionalProperties":false
    })
}

#[derive(Clone)]
pub(crate) struct GenerateTool {
    gateway: Gateway,
}

impl GenerateTool {
    pub(crate) fn new(gateway: &Gateway) -> Self {
        Self {
            gateway: gateway.clone(),
        }
    }

    async fn enabled(&self, principal: &Principal) -> Result<bool, GenerationError> {
        Security::new(self.gateway.storage.auth())
            .authorize_principal_capability(principal)
            .await
            .map_err(|_| {
                GenerationError::new("authorization_failed", "Principal authorization failed")
            })?;
        Ok(config::load(&self.gateway).await?.enabled)
    }

    async fn run(
        &self,
        arguments: Value,
        principal: Principal,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Result<Value, GenerationError> {
        if !self.enabled(&principal).await? {
            return Err(GenerationError::new(
                "media_generation_disabled",
                "Media generation is disabled",
            ));
        }
        execution::generate(&self.gateway, arguments, principal, cancellation, deadline).await
    }

    async fn run_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
        deadline: Instant,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        // 生图 future 包含素材收存和 Vendor 路由状态；在公开执行包装边界固定其
        // 地址，避免 debug 构建逐层内联搬移大型 future 耗尽工作线程栈。
        let result =
            Box::pin(self.run(arguments, context.principal, context.cancellation, deadline)).await;
        let is_error = result.is_err();
        Ok(PlatformToolOutput {
            content: vec![ContentBlock::Unknown {
                raw: result.unwrap_or_else(error_value),
            }],
            is_error,
            metadata: Default::default(),
        })
    }
}

fn error_value(error: GenerationError) -> Value {
    let mut value = json!({"error":{"code":error.code,"message":error.message}});
    if let Some(artifact) = error.artifact {
        value["artifact"] = artifact;
    }
    value
}

fn error_output(error: GenerationError) -> PlatformToolOutput {
    PlatformToolOutput {
        content: vec![ContentBlock::Unknown {
            raw: error_value(error),
        }],
        is_error: true,
        metadata: Default::default(),
    }
}

#[async_trait]
impl PlatformTool for GenerateTool {
    fn id(&self) -> ToolId {
        ToolId::new(TOOL_ID)
    }
    fn external_name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> Option<&str> {
        Some(DESCRIPTION)
    }
    fn activity_label(&self) -> &str {
        "Generating media"
    }
    fn execution_limit(&self) -> Option<Duration> {
        Some(LIMIT)
    }
    fn execution_policy(
        &self,
        _arguments: &Value,
    ) -> stravia_runtime_contract::hook::PlatformExecutionPolicy {
        stravia_runtime_contract::hook::PlatformExecutionPolicy::Bounded
    }
    fn parameters(&self) -> Value {
        input_schema()
    }
    // Optional preferences must remain omittable; strict Responses schemas
    // require every property, which would change the public generate contract.
    fn strict_schema(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Value, PlatformToolError> {
        let result = PlatformTool::execute_result(self, arguments, context).await?;
        let (value, _) = crate::hook::tool::blocks_to_value(result.content)?;
        if result.is_error {
            Err(PlatformToolError::new(value.to_string()))
        } else {
            Ok(value)
        }
    }
    async fn execute_result(
        &self,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<PlatformToolOutput, PlatformToolError> {
        if let Err(error) = execution::validate_input(&arguments) {
            return Ok(error_output(error));
        }
        let deadline =
            crate::tool_execution::current_deadline().unwrap_or_else(|| Instant::now() + LIMIT);
        self.run_result(arguments, context, deadline).await
    }
}

#[async_trait]
impl McpTool for GenerateTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    fn description(&self) -> Option<&str> {
        Some(DESCRIPTION)
    }
    fn input_schema(&self) -> Value {
        input_schema()
    }
    fn deadline(&self) -> Duration {
        LIMIT
    }
    fn await_cancellation_cleanup(&self) -> bool {
        true
    }
    async fn available(&self, context: &McpContext) -> Result<bool, McpToolError> {
        let Some(keys) = self.gateway.storage.api_keys() else {
            return Ok(false);
        };
        let key = keys.get(&context.api_key_id).await.map_err(|_| {
            McpToolError::new("mcp_access_check_failed", "MCP access could not be checked")
        })?;
        if !key.is_some_and(|key| key.is_enabled && key.mcp_access_enabled) {
            return Ok(false);
        }
        self.enabled(&Principal::new(context.api_key_id.clone()))
            .await
            .map_err(|error| McpToolError::new(error.code, error.message))
    }
    async fn call(
        &self,
        arguments: Value,
        context: &McpContext,
    ) -> Result<McpToolOutput, McpToolError> {
        if let Err(error) = execution::validate_input(&arguments) {
            return Ok(McpToolOutput::execution_error(error_value(error)));
        }
        let (cancellation, deadline) = context
            .execution()
            .unwrap_or_else(|| (CancellationToken::new(), Instant::now() + LIMIT));
        let caller_cancellation = cancellation.clone();
        let run_id = stravia_runtime_contract::identifier::new_id();
        let execution_context = ToolExecutionContext {
            request_id: run_id.clone(),
            run_id,
            principal: Principal::new(context.api_key_id.clone()),
            read_scope: stravia_runtime_contract::hook::ReadExposureScope::FULL,
            cancellation,
            progress: None,
        };
        let result = crate::tool_execution::scope_request(
            deadline,
            self.gateway.dispatch_platform_tool(
                Arc::new(self.clone()),
                arguments,
                execution_context,
                crate::tool_execution::ExecutionSource::Mcp,
            ),
        )
        .await;
        match result {
            Ok(delivery) => {
                let result = delivery.output;
                let (value, _) =
                    crate::hook::tool::blocks_to_value(result.content).map_err(|error| {
                        McpToolError::new("result_encoding_failed", error.to_string())
                    })?;
                Ok(if result.is_error {
                    McpToolOutput::execution_error(value)
                } else {
                    McpToolOutput::success(value)
                })
            }
            Err(error) => Ok(McpToolOutput::execution_error(json!({"error":{
                "code": if caller_cancellation.is_cancelled() { "cancelled" } else { "generation_failed" },
                "message":error.to_string()
            }}))),
        }
    }
}

pub(crate) fn hook(tool: GenerateTool) -> Arc<dyn Hook> {
    Arc::new(GenerateHook(tool))
}

struct GenerateHook(GenerateTool);
impl Hook for GenerateHook {
    fn descriptor(&self) -> HookDescriptor {
        HookDescriptor {
            id: HookId::new(TOOL_ID),
            request_kinds: vec![RequestKind::Generation],
            event_kinds: vec![EventKind::Request],
            requires_full_context: false,
            max_buffered_bytes: 0,
            max_delayed_events: 0,
        }
    }
    fn create_session(&self, context: &SessionContext) -> Box<dyn HookSession> {
        Box::new(GenerateSession {
            tool: self.0.clone(),
            principal: context.principal.clone(),
            resolved: context.tools_fixed,
            forced_generate: false,
        })
    }
}
struct GenerateSession {
    tool: GenerateTool,
    principal: Principal,
    resolved: bool,
    forced_generate: bool,
}
#[async_trait]
impl HookSession for GenerateSession {
    async fn handle(&mut self, event: HookEvent<'_>) -> Result<ActionBatch, String> {
        let HookEvent::Request { current, .. } = event else {
            return Ok(ActionBatch::default());
        };
        if self.resolved {
            // The explicit selection is fulfilled by the hidden tool round.
            // Do not force another image when the model receives its result.
            if std::mem::take(&mut self.forced_generate) {
                return Ok(ActionBatch::one(HookAction::PatchRequest(Box::new(
                    RequestPatch::SetToolChoice(Some(ToolChoice::Auto)),
                ))));
            }
            return Ok(ActionBatch::default());
        }
        self.resolved = true;
        let explicit = current
            .tools
            .iter()
            .flatten()
            .any(|tool| tool.name == TOOL_NAME);
        self.forced_generate = explicit
            && matches!(&current.tool_choice, Some(ToolChoice::Named { name }) if name == TOOL_NAME);
        let inject = Security::new(self.tool.gateway.storage.auth())
            .generation_transparent_injection_enabled(&self.principal)
            .await
            .map_err(|error| error.to_string())?;
        if !explicit && !inject {
            return Ok(ActionBatch::default());
        }
        let enabled = self
            .tool
            .enabled(&self.principal)
            .await
            .map_err(|error| error.message)?;
        if !enabled {
            return Ok(if explicit {
                ActionBatch::one(HookAction::Reject(HookRejection {
                    status: 403,
                    code: "media_generation_disabled".into(),
                    message: "Media generation is disabled".into(),
                }))
            } else {
                ActionBatch::default()
            });
        }
        let mut actions = Vec::new();
        if explicit {
            let tools = current.tools.as_ref().map(|tools| {
                tools
                    .iter()
                    .filter(|tool| tool.name != TOOL_NAME)
                    .cloned()
                    .collect()
            });
            actions.push(HookAction::PatchRequest(Box::new(
                RequestPatch::ReplaceTools(tools),
            )));
        }
        actions.push(HookAction::ExposeTool(ToolId::new(TOOL_ID)));
        Ok(ActionBatch { actions })
    }
}
