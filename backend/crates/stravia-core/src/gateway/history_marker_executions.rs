use super::*;

pub(crate) struct HistoryMarkerExecutionJob {
    pub(crate) marker_reference: String,
    pub(crate) owner_id: String,
    pub(crate) execution_deadline_unix_ms: i64,
    pub(crate) execution: hook::DetachedPlatformExecution,
    pub(crate) observer: Option<interaction_observation::RunObserver>,
    pub(crate) model_turn_id: String,
}

pub(crate) struct StartedHistoryMarkerExecution {
    marker_reference: String,
    raw_result: tokio::sync::oneshot::Receiver<RawHistoryMarkerExecution>,
    transformed_result: tokio::sync::oneshot::Sender<hook::PlatformToolResult>,
}

#[derive(Clone)]
struct RawHistoryMarkerExecution {
    call: protocol::ir::ToolCall,
    result: hook::PlatformToolResult,
}

impl Gateway {
    async fn execute_history_marker_job(
        job: HistoryMarkerExecutionJob,
    ) -> RawHistoryMarkerExecution {
        use interaction_observation::RunEvent;

        let call = job.execution.call().call.clone();
        let observer = job.observer;
        let started = Instant::now();
        if let Some(observer) = observer.as_ref() {
            observer.record(RunEvent::PlatformToolStarted {
                model_turn_id: job.model_turn_id.clone(),
                tool_id: call.id.clone(),
                name: call.name.clone(),
                input: Some(
                    serde_json::from_str(&call.arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                ),
            });
            if observer.debug_enabled() {
                observer.record_debug(|| RunEvent::Checkpoint {
                    stage: "platform_tool_call".into(),
                    model_turn_id: Some(job.model_turn_id.clone()),
                    attempt_id: None,
                    payload: serde_json::json!(&call),
                });
            }
        }
        let remaining_ms = job
            .execution_deadline_unix_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis());
        let result = if remaining_ms <= 0 {
            hook::PlatformToolResult {
                tool_id: hook::ToolId::new("deadline"),
                call_id: call.id.clone(),
                content_kind: protocol::ir::ToolResultContentKind::Json,
                content: serde_json::Value::String(
                    "Platform tool execution reached its registered deadline.".into(),
                ),
                is_error: true,
                metadata: serde_json::Map::new(),
            }
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms as u64),
                interaction_observation::scope::scope(observer.clone(), job.execution.execute()),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => hook::PlatformToolResult {
                    tool_id: hook::ToolId::new("deadline"),
                    call_id: call.id.clone(),
                    content_kind: protocol::ir::ToolResultContentKind::Json,
                    content: serde_json::Value::String(
                        "Platform tool execution reached its registered deadline.".into(),
                    ),
                    is_error: true,
                    metadata: serde_json::Map::new(),
                },
            }
        };
        if let Some(observer) = observer.as_ref() {
            observer.record(RunEvent::PlatformToolFinished {
                model_turn_id: job.model_turn_id.clone(),
                tool_id: call.id.clone(),
                status: if result.is_error {
                    "failed"
                } else {
                    "completed"
                }
                .into(),
                duration_ms: started.elapsed().as_millis().min(i64::MAX as u128) as i64,
                content: Some(result.content.clone()),
            });
            if observer.debug_enabled() {
                observer.record_debug(|| RunEvent::Checkpoint {
                    stage: "platform_tool_result".into(),
                    model_turn_id: Some(job.model_turn_id),
                    attempt_id: None,
                    payload: serde_json::json!({
                        "tool_id": result.tool_id.as_str(),
                        "call_id": result.call_id,
                        "content": result.content,
                        "is_error": result.is_error,
                        "metadata": result.metadata,
                    }),
                });
            }
        }
        RawHistoryMarkerExecution { call, result }
    }

    async fn persist_history_marker_result(
        store: &dyn history_marker::HistoryMarkerStore,
        principal: &hook::Principal,
        marker_reference: &str,
        owner_id: &str,
        raw: RawHistoryMarkerExecution,
        result: hook::PlatformToolResult,
    ) {
        let state = if result.is_error {
            history_marker::PlatformExecutionState::Failed
        } else {
            history_marker::PlatformExecutionState::Completed
        };
        if let Err(error) = store
            .finish_execution(
                principal,
                marker_reference,
                owner_id,
                state,
                history_marker::HiddenHistorySegment::Platform {
                    call: raw.call,
                    result: result.content_block(),
                },
            )
            .await
        {
            tracing::error!(
                marker_reference,
                error = %error,
                "failed to persist Platform Tool terminal result"
            );
        }
    }

    pub(crate) fn start_history_marker_executions(
        &self,
        principal: hook::Principal,
        jobs: Vec<HistoryMarkerExecutionJob>,
    ) -> Vec<StartedHistoryMarkerExecution> {
        jobs.into_iter()
            .map(|job| {
                let marker_reference = job.marker_reference.clone();
                let owner_id = job.owner_id.clone();
                let store = Arc::clone(&self.history_markers);
                let principal = principal.clone();
                let task_marker_reference = marker_reference.clone();
                let execution_gate = Arc::clone(&self.history_marker_execution_gate);
                let parallel_safe = job.execution.parallel_safe();
                let (raw_tx, raw_result) = tokio::sync::oneshot::channel();
                let (transformed_result, transformed_rx) = tokio::sync::oneshot::channel();
                self.lifecycle.spawn(async move {
                    let raw = if parallel_safe {
                        let _permit = execution_gate.read().await;
                        Self::execute_history_marker_job(job).await
                    } else {
                        let _permit = execution_gate.write().await;
                        Self::execute_history_marker_job(job).await
                    };
                    if raw_tx.send(raw.clone()).is_err() {
                        let result = raw.result.clone();
                        Self::persist_history_marker_result(
                            store.as_ref(),
                            &principal,
                            &task_marker_reference,
                            &owner_id,
                            raw,
                            result,
                        )
                        .await;
                        return;
                    }
                    let result = transformed_rx.await.unwrap_or_else(|_| raw.result.clone());
                    Self::persist_history_marker_result(
                        store.as_ref(),
                        &principal,
                        &task_marker_reference,
                        &owner_id,
                        raw,
                        result,
                    )
                    .await;
                });
                StartedHistoryMarkerExecution {
                    marker_reference,
                    raw_result,
                    transformed_result,
                }
            })
            .collect()
    }

    async fn finish_history_marker_executions(
        executions: Vec<StartedHistoryMarkerExecution>,
        run: &mut hook::InferenceRun,
    ) {
        for execution in executions {
            let Ok(mut raw) = execution.raw_result.await else {
                continue;
            };
            let hook_failure = match run.on_tool_result(&mut raw.result).await {
                Ok(hook::HookControl::Continue) => None,
                Ok(
                    hook::HookControl::Respond(_)
                    | hook::HookControl::Reject(_)
                    | hook::HookControl::StreamAbort { .. },
                ) => Some("ToolResult Hook attempted response control".to_owned()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = hook_failure {
                tracing::error!(
                    marker_reference = execution.marker_reference,
                    error,
                    "ToolResult Hook failed for background Platform Tool"
                );
                raw.result.content = serde_json::Value::String(
                    "Platform tool result processing failed before persistence.".into(),
                );
                raw.result.is_error = true;
            }
            let _ = execution.transformed_result.send(raw.result);
        }
    }

    pub(crate) async fn run_history_marker_executions(
        &self,
        principal: hook::Principal,
        jobs: Vec<HistoryMarkerExecutionJob>,
        run: &mut hook::InferenceRun,
    ) {
        let executions = self.start_history_marker_executions(principal, jobs);
        Self::finish_history_marker_executions(executions, run).await;
    }

    pub(crate) async fn run_started_history_marker_executions(
        &self,
        executions: Vec<StartedHistoryMarkerExecution>,
        run: &mut hook::InferenceRun,
    ) {
        Self::finish_history_marker_executions(executions, run).await;
    }

    pub(crate) fn spawn_started_history_marker_executions(
        &self,
        executions: Vec<StartedHistoryMarkerExecution>,
        mut run: hook::InferenceRun,
    ) {
        self.lifecycle.spawn(async move {
            Self::finish_history_marker_executions(executions, &mut run).await;
        });
    }
}
