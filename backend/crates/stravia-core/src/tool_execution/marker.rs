use std::time::Instant;

use crate::{Gateway, history_marker, hook::PlatformToolRegistry, interaction_observation};
use std::sync::Arc;
use stravia_runtime_contract::{
    CancellationToken,
    hook::{PlatformToolCall, PlatformToolResult, ToolExecutionContext},
    protocol::ir::ToolResultContentKind,
};

pub(crate) struct DetachedPlatformExecution {
    tools: PlatformToolRegistry,
    call: PlatformToolCall,
    context: ToolExecutionContext,
    published_cancellation: CancellationToken,
    limit: std::time::Duration,
    accepted_at: tokio::time::Instant,
}

impl DetachedPlatformExecution {
    pub(crate) fn new(
        tools: PlatformToolRegistry,
        call: PlatformToolCall,
        context: ToolExecutionContext,
        published_cancellation: CancellationToken,
    ) -> Self {
        let limit = tools.execution_limit(&call.tool_id);
        Self {
            tools,
            call,
            context,
            published_cancellation,
            limit,
            accepted_at: tokio::time::Instant::now(),
        }
    }

    pub(crate) fn activity(&self) -> &str {
        self.tools
            .activity_label(&self.call.tool_id)
            .unwrap_or("Running a platform tool")
    }

    pub(crate) fn limit(&self) -> std::time::Duration {
        self.limit
    }

    pub(crate) fn accepted_at(&self) -> tokio::time::Instant {
        self.accepted_at
    }

    pub(crate) fn call(&self) -> &PlatformToolCall {
        &self.call
    }

    pub(crate) async fn execute(self, gateway: &Gateway) -> super::ToolDelivery {
        let Self {
            tools,
            call,
            context,
            published_cancellation,
            ..
        } = self;
        // 参数解析失败交回模型修正，原始字节不得进入领域执行。
        let arguments = match serde_json::from_str(&call.call.arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return super::ToolDelivery {
                    system_cancelled: false,
                    result: PlatformToolResult {
                        tool_id: call.tool_id,
                        call_id: call.call.id.to_string(),
                        content: serde_json::Value::String(format!(
                            "invalid tool arguments: {error}"
                        )),
                        content_kind: ToolResultContentKind::Json,
                        is_error: true,
                        metadata: serde_json::Map::new(),
                    },
                };
            }
        };
        let source = super::ExecutionSource::InferenceRun {
            run_id: context.run_id.clone(),
            published_cancellation,
        };
        tools
            .execute(
                gateway,
                &call.tool_id,
                call.call.id.to_string(),
                arguments,
                context,
                source,
            )
            .await
    }
}

pub(crate) struct PreparedMarkerExecution {
    marker_reference: String,
    owner_id: String,
    execution_deadline_unix_ms: i64,
    execution: DetachedPlatformExecution,
    observer: Option<interaction_observation::RunObserver>,
    model_turn_id: String,
}

pub(crate) struct StartedMarkerDelivery {
    marker_reference: String,
    raw_result:
        tokio::sync::oneshot::Receiver<Option<stravia_runtime_contract::hook::PlatformToolResult>>,
    transformed_result:
        tokio::sync::oneshot::Sender<stravia_runtime_contract::hook::PlatformToolResult>,
    persisted: tokio::sync::oneshot::Receiver<Result<(), history_marker::HistoryMarkerError>>,
}

impl StartedMarkerDelivery {
    pub(crate) async fn result(
        &mut self,
    ) -> Result<
        Option<stravia_runtime_contract::hook::PlatformToolResult>,
        history_marker::HistoryMarkerError,
    > {
        (&mut self.raw_result).await.map_err(|_| {
            history_marker::HistoryMarkerError::Storage(format!(
                "Platform delivery {} ended before producing a result",
                self.marker_reference
            ))
        })
    }

    pub(crate) async fn finish(
        self,
        result: Option<stravia_runtime_contract::hook::PlatformToolResult>,
    ) -> Result<(), history_marker::HistoryMarkerError> {
        if let Some(result) = result {
            self.transformed_result.send(result).map_err(|_| {
                history_marker::HistoryMarkerError::Storage(format!(
                    "Platform delivery {} ended before processing its result",
                    self.marker_reference
                ))
            })?;
        }
        self.persisted.await.map_err(|_| {
            history_marker::HistoryMarkerError::Storage(format!(
                "Platform delivery {} ended before persisting its result",
                self.marker_reference
            ))
        })?
    }
}

struct RawMarkerDelivery {
    call: stravia_runtime_contract::protocol::ir::ToolCall,
    result: stravia_runtime_contract::hook::PlatformToolResult,
    system_cancelled: bool,
}

pub(crate) fn delivery_processing_error(
    result: &mut stravia_runtime_contract::hook::PlatformToolResult,
) {
    result.content = serde_json::json!({"error":{
        "code":"delivery_processing_failed",
        "message":"Tool result delivery processing did not complete. The tool may have executed; query an existing execution reference for its independent final result."
    }});
    result.content_kind = stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json;
    result.is_error = true;
    result.metadata.clear();
}

impl Gateway {
    pub(crate) async fn prepare_marker_execution(
        &self,
        principal: &stravia_runtime_contract::Principal,
        execution: DetachedPlatformExecution,
        observer: Option<interaction_observation::RunObserver>,
        model_turn_id: String,
    ) -> Result<
        (history_marker::HistoryMarker, PreparedMarkerExecution),
        history_marker::HistoryMarkerError,
    > {
        let remaining = execution
            .limit()
            .saturating_sub(execution.accepted_at().elapsed());
        let marker = self
            .history_markers
            .create_platform(
                principal,
                history_marker::PlatformMarkerInput {
                    tool_id: execution.call().tool_id.to_string(),
                    call: execution.call().call.clone(),
                    activity: execution.activity().to_owned(),
                    execution_limit: remaining,
                },
            )
            .await?;
        let owner_id = stravia_runtime_contract::identifier::new_id();
        if self
            .history_markers
            .claim_execution(principal, &marker.reference, &owner_id, execution.limit())
            .await?
            != history_marker::ClaimOutcome::Claimed
        {
            return Err(history_marker::HistoryMarkerError::Storage(
                "new Platform Tool delivery could not be claimed".into(),
            ));
        }
        let execution_deadline_unix_ms = self
            .history_markers
            .resolve(principal, &marker.reference)
            .await?
            .and_then(|resolved| resolved.execution_deadline_unix_ms)
            .ok_or_else(|| {
                history_marker::HistoryMarkerError::Storage(
                    "claimed Platform delivery is missing its persisted deadline".into(),
                )
            })?;
        let prepared = PreparedMarkerExecution {
            marker_reference: marker.reference.clone(),
            owner_id,
            execution_deadline_unix_ms,
            execution,
            observer,
            model_turn_id,
        };
        Ok((marker, prepared))
    }

    async fn execute_marker_delivery(
        &self,
        prepared: PreparedMarkerExecution,
    ) -> RawMarkerDelivery {
        use interaction_observation::RunEvent;

        let call = prepared.execution.call().call.clone();
        let observer = prepared.observer;
        let started = Instant::now();
        if let Some(observer) = observer.as_ref() {
            observer.record(RunEvent::PlatformToolStarted {
                model_turn_id: prepared.model_turn_id.clone(),
                tool_id: call.id.to_string(),
                name: call.name.clone(),
                input: Some(
                    serde_json::from_str(&call.arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                ),
            });
        }
        let remaining_ms = prepared
            .execution_deadline_unix_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis());
        let accepted_at = prepared.execution.accepted_at();
        let result = if remaining_ms <= 0 {
            super::ToolDelivery {
                system_cancelled: false,
                result: stravia_runtime_contract::hook::PlatformToolResult {
                    tool_id: stravia_runtime_contract::hook::ToolId::new("deadline"),
                    call_id: call.id.to_string(),
                    content_kind:
                        stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                    content: serde_json::Value::String(
                        "Platform tool execution reached its registered deadline.".into(),
                    ),
                    is_error: true,
                    metadata: serde_json::Map::new(),
                },
            }
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms as u64),
                crate::tool_execution::scope_execution(
                    accepted_at,
                    Instant::now() + std::time::Duration::from_millis(remaining_ms as u64),
                    interaction_observation::scope::scope(
                        observer.clone(),
                        prepared.execution.execute(self),
                    ),
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => super::ToolDelivery {
                    system_cancelled: false,
                    result: stravia_runtime_contract::hook::PlatformToolResult {
                        tool_id: stravia_runtime_contract::hook::ToolId::new("deadline"),
                        call_id: call.id.to_string(),
                        content_kind:
                            stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                        content: serde_json::Value::String(
                            "Platform tool execution reached its registered deadline.".into(),
                        ),
                        is_error: true,
                        metadata: serde_json::Map::new(),
                    },
                },
            }
        };
        if let Some(observer) = observer.as_ref() {
            observer.record(RunEvent::PlatformToolFinished {
                model_turn_id: prepared.model_turn_id.clone(),
                tool_id: call.id.to_string(),
                status: if result.result.is_error {
                    "failed"
                } else {
                    "completed"
                }
                .into(),
                duration_ms: started.elapsed().as_millis().min(i64::MAX as u128) as i64,
                content: Some(result.result.content.clone()),
            });
        }
        RawMarkerDelivery {
            call,
            result: result.result,
            system_cancelled: result.system_cancelled,
        }
    }

    async fn persist_history_marker_result(
        store: &dyn history_marker::HistoryMarkerStore,
        principal: &stravia_runtime_contract::Principal,
        marker_reference: &str,
        owner_id: &str,
        call: stravia_runtime_contract::protocol::ir::ToolCall,
        result: stravia_runtime_contract::hook::PlatformToolResult,
    ) -> Result<(), history_marker::HistoryMarkerError> {
        let state = if result.is_error {
            history_marker::PlatformExecutionState::Failed
        } else {
            history_marker::PlatformExecutionState::Completed
        };
        store
            .finish_execution(
                principal,
                marker_reference,
                owner_id,
                state,
                history_marker::HiddenHistorySegment::Platform {
                    call,
                    result: stravia_runtime_contract::protocol::ir::ContentBlock::ToolResult {
                        tool_use_id: result.call_id.into(),
                        content: result.content,
                        content_kind: Some(result.content_kind),
                        is_error: Some(result.is_error),
                        cache_control: None,
                    },
                },
            )
            .await
    }

    pub(crate) fn start_history_marker_executions(
        &self,
        principal: stravia_runtime_contract::Principal,
        prepared: Vec<PreparedMarkerExecution>,
    ) -> Vec<StartedMarkerDelivery> {
        prepared.into_iter()
            .map(|prepared| {
                let marker_reference = prepared.marker_reference.clone();
                let owner_id = prepared.owner_id.clone();
                let store = Arc::clone(&self.history_markers);
                let principal = principal.clone();
                let task_marker_reference = marker_reference.clone();
                let gateway = self.background_clone();
                let (raw_tx, raw_result) = tokio::sync::oneshot::channel();
                let (transformed_result, transformed_rx) = tokio::sync::oneshot::channel();
                let (persisted_tx, persisted) = tokio::sync::oneshot::channel();
                let root_request = crate::rpm::current_root_request();
                self.lifecycle
                    .spawn(crate::rpm::scope_root_request(root_request, async move {
                        let RawMarkerDelivery { call, result, system_cancelled } = gateway.execute_marker_delivery(prepared).await;
                        let result = if system_cancelled {
                            // This is the module's determined cancellation, not
                            // an arbitrary domain payload. It never needs a Hook.
                            let _ = raw_tx.send(None);
                            result
                        } else {
                            let tool_id = result.tool_id.clone();
                            let call_id = result.call_id.clone();
                            match raw_tx.send(Some(result)) {
                                Ok(()) => match transformed_rx.await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        let mut result = stravia_runtime_contract::hook::PlatformToolResult {
                                            tool_id,
                                            call_id,
                                            content: serde_json::Value::Null,
                                            content_kind: stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json,
                                            is_error: true,
                                            metadata: serde_json::Map::new(),
                                        };
                                        delivery_processing_error(&mut result);
                                        result
                                    }
                                },
                                Err(Some(mut undelivered)) => {
                                    delivery_processing_error(&mut undelivered);
                                    undelivered
                                }
                                Err(None) => unreachable!("ordinary delivery always contains a result"),
                            }
                        };
                        let persisted = Self::persist_history_marker_result(
                            store.as_ref(),
                            &principal,
                            &task_marker_reference,
                            &owner_id,
                            call,
                            result,
                        )
                        .await;
                        if let Err(error) = &persisted {
                            tracing::error!(marker_reference = %task_marker_reference, error = %error, "failed to persist Platform delivery result");
                        }
                        let _ = persisted_tx.send(persisted);
                    }));
                StartedMarkerDelivery {
                    marker_reference,
                    raw_result,
                    transformed_result,
                    persisted,
                }
            })
            .collect()
    }
}
