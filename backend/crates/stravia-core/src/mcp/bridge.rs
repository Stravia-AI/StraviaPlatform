use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use stravia_runtime_contract::{
    Principal,
    hook::{PlatformTool, ReadExposureScope, ToolExecutionContext},
    protocol::ir::AiRequest,
};

use super::{McpContext, McpToolError, McpToolOutput};
use crate::{
    Gateway,
    interaction_observation::{AdmissionFacts, IngressStart, RunOutcome, RunStart},
};

/// 检查指定 API Key 是否处于启用状态且允许访问内置 MCP 端点。
pub(crate) async fn api_key_allows_mcp(
    gateway: &Gateway,
    api_key_id: &str,
) -> Result<bool, McpToolError> {
    let Some(keys) = gateway.storage.api_keys() else {
        return Ok(false);
    };
    let key = keys.get(api_key_id).await.map_err(|_| {
        McpToolError::new("mcp_access_check_failed", "MCP access could not be checked")
    })?;
    Ok(key.is_some_and(|key| key.is_enabled && key.mcp_access_enabled))
}

/// 通过 ADR-0082 有界同步等待与后台交接管线执行平台工具，并统一映射 MCP 输出与取消错误码。
pub(crate) async fn call_platform_tool<T: PlatformTool + Clone>(
    gateway: &Gateway,
    tool: &T,
    arguments: Value,
    context: &McpContext,
    default_deadline: Duration,
    failure_code: &'static str,
) -> Result<McpToolOutput, McpToolError> {
    let (cancellation, deadline) = context.execution().unwrap_or_else(|| {
        (
            context.request_cancellation().unwrap_or_default(),
            Instant::now() + default_deadline,
        )
    });
    let caller_cancellation = cancellation.clone();
    let run_id = stravia_runtime_contract::identifier::new_id();
    let execution_context = ToolExecutionContext {
        request_id: run_id.clone(),
        run_id,
        interaction_id: None,
        principal: Principal::new(context.api_key_id.clone()),
        read_scope: ReadExposureScope::FULL,
        cancellation,
        progress: None,
    };
    let result = crate::tool_execution::scope_request(
        deadline,
        gateway.dispatch_platform_tool(
            Arc::new(tool.clone()),
            arguments,
            execution_context,
            crate::tool_execution::ExecutionSource::Mcp,
        ),
    )
    .await;
    match result {
        Ok(delivery) => {
            let result = delivery.output;
            let (value, _) = crate::hook::tool::blocks_to_value(result.content)
                .map_err(|error| McpToolError::new("result_encoding_failed", error.to_string()))?;
            Ok(if result.is_error {
                McpToolOutput::execution_error(value)
            } else {
                McpToolOutput::success(value)
            })
        }
        Err(error) => {
            let code = if caller_cancellation.is_cancelled() {
                "cancelled"
            } else {
                failure_code
            };
            Ok(McpToolOutput::execution_error(json!({
                "error": {
                    "code": code,
                    "message": error.to_string(),
                }
            })))
        }
    }
}

pub(crate) struct StandaloneMcpObservation {
    pub(crate) run_id: String,
    pub(crate) principal_id: String,
    pub(crate) tool_name: String,
    pub(crate) route_id: String,
    pub(crate) model_display_name: Option<String>,
    pub(crate) client_request: AiRequest,
}

/// 独立 MCP 调用的观测作用域：存在外层推理观测时直接复用，否则为本次调用建立并收尾独立 Run。
pub(crate) struct StandaloneMcpObservationScope {
    observer: crate::interaction_observation::RunObserver,
    standalone: bool,
}

impl StandaloneMcpObservationScope {
    pub(crate) fn enter(
        gateway: &Gateway,
        build_meta: impl FnOnce() -> StandaloneMcpObservation,
    ) -> Self {
        let inherited = crate::interaction_observation::scope::current();
        let standalone = inherited.is_none();
        let observer = inherited.unwrap_or_else(|| {
            let meta = build_meta();
            gateway
                .observation
                .observe_ingress(IngressStart {
                    id: meta.run_id.clone(),
                    method: "MCP".into(),
                    path: format!("tools/call/{}", meta.tool_name),
                    protocol: "mcp".into(),
                })
                .admit(
                    RunStart {
                        id: meta.run_id,
                        principal: meta.principal_id.clone(),
                        api_key_id: Some(meta.principal_id),
                        api_key_name: None,
                        route_id: meta.route_id,
                        model_display_name: meta.model_display_name,
                        ingress_protocol: "mcp".into(),
                    },
                    AdmissionFacts {
                        client_request: Arc::new(meta.client_request),
                        has_new_user: true,
                        has_matching_pending_tool_result: false,
                        generation_root_id: None,
                        generation_parent_id: None,
                    },
                )
        });
        Self {
            observer,
            standalone,
        }
    }

    pub(crate) fn observer(&self) -> crate::interaction_observation::RunObserver {
        self.observer.clone()
    }

    pub(crate) async fn run<T, E, Fut, Classify>(
        self,
        work: Fut,
        classify: Classify,
    ) -> Result<T, E>
    where
        Fut: Future<Output = Result<T, E>>,
        Classify: FnOnce(&Result<T, E>) -> (&'static str, Option<String>),
    {
        let result =
            crate::interaction_observation::scope::scope(Some(self.observer.clone()), work).await;
        if self.standalone {
            let (status, terminal_reason) = classify(&result);
            self.observer.finish(RunOutcome {
                client_output_committed: false,
                delivery: None,
                delivery_completed_at: None,
                status: status.into(),
                terminal_reason,
                generation_node_id: None,
                generation_root_id: None,
            });
        }
        result
    }
}
