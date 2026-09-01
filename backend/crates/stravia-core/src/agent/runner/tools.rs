use super::*;

fn tool_authorization_error() -> AgentRunError {
    AgentRunError::new(
        "tool_authorization_failed",
        "Agent Tool authorization failed",
    )
}

fn tool_cancelled_error() -> AgentRunError {
    AgentRunError::new("cancelled", "Agent Tool cancelled")
}

pub(super) struct ToolExecutionRequest<'a> {
    pub(super) hooks: Option<Arc<tokio::sync::Mutex<InferenceRun>>>,
    pub(super) allowlist: &'a [VersionedToolId],
    pub(super) principal: &'a Principal,
    pub(super) definition_id: &'a AgentDefinitionId,
    pub(super) model_id: &'a str,
    pub(super) turn_id: &'a AgentTurnId,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) deadline: Instant,
    pub(super) calls: Vec<ToolCall>,
    pub(super) parallelism: Option<u32>,
    pub(super) ordinal_offset: u32,
    pub(super) events: &'a mpsc::Sender<AgentEvent>,
}

impl AgentRunner {
    pub(super) async fn execute_tools(
        &self,
        request: ToolExecutionRequest<'_>,
    ) -> Result<Vec<AiItem>, AgentRunError> {
        let ToolExecutionRequest {
            hooks,
            allowlist,
            principal,
            definition_id,
            model_id,
            turn_id,
            cancellation,
            deadline,
            calls,
            parallelism,
            ordinal_offset,
            events,
        } = request;
        let authorizer = self.tool_authorizer.clone();
        let all_parallel_safe = calls.iter().all(|call| {
            self.tools
                .resolve_model_name(allowlist, &call.name)
                .is_some_and(|(_, tool)| tool.parallel_safe())
        });
        let parallelism = if all_parallel_safe {
            parallelism
                .map(|value| value as usize)
                .unwrap_or_else(|| calls.len().max(1))
        } else {
            1
        };
        let executions = calls.into_iter().enumerate().map(|(index, call)| {
            let ordinal = ordinal_offset + index as u32 + 1;
            let selected = self.tools.resolve_model_name(allowlist, &call.name);
            let principal = principal.clone();
            let definition_id = definition_id.clone();
            let model_id = model_id.to_owned();
            let turn_id = turn_id.clone();
            let cancellation = cancellation.clone();
            let events = events.clone();
            let hooks = hooks.clone();
            let authorizer = authorizer.clone();
            async move {
                let (tool_id, tool) = selected.ok_or_else(|| {
                    AgentRunError::new(
                        "tool_not_allowed",
                        format!("Model requested unavailable Tool {}", call.name),
                    )
                })?;
                send_event(
                    &events,
                    AgentEvent::ToolStarted {
                        tool: tool_id.clone(),
                        ordinal,
                    },
                )
                .await?;
                let arguments = serde_json::from_str(&call.arguments).map_err(|error| {
                    AgentRunError::new(
                        "invalid_tool_arguments",
                        format!("invalid arguments for {}: {error}", call.name),
                    )
                })?;
                if cancellation.is_cancelled() {
                    return Err(tool_cancelled_error());
                }
                let result = tokio::select! {
                    _ = events.closed() => {
                        cancellation.cancel();
                        Ok(Err(crate::agent::AgentToolError::new(
                            "cancelled",
                            "Agent Run consumer disconnected",
                        )))
                    }
                    _ = cancellation.cancelled() => {
                        Ok(Err(crate::agent::AgentToolError::new(
                            "cancelled",
                            "Agent Tool cancelled",
                        )))
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        Ok(Err(crate::agent::AgentToolError::new(
                            "deadline_exceeded",
                            "Agent Tool deadline exceeded",
                        )))
                    }
                    result = async {
                        if let Some(authorizer) = authorizer
                            && authorizer
                                .authorize(&principal, &definition_id, &model_id)
                                .await
                                .is_err()
                        {
                            cancellation.cancel();
                            return Err(tool_authorization_error());
                        }
                        Ok(tool.execute(
                            AgentToolContext {
                                principal,
                                turn_id,
                                cancellation: cancellation.clone(),
                                deadline,
                            },
                            arguments,
                        ).await)
                    } => result,
                };
                let result = match result {
                    Err(error) => return Err(error),
                    Ok(result) => result,
                };
                let mut result = match result {
                    Ok(output) => PlatformToolResult {
                        tool_id: ToolId::new(tool_id.id.clone()),
                        call_id: call.id.clone(),
                        content: output,
                        is_error: false,
                        metadata: serde_json::Map::new(),
                    },
                    Err(error) => PlatformToolResult {
                        tool_id: ToolId::new(tool_id.id.clone()),
                        call_id: call.id.clone(),
                        content: serde_json::json!({
                            "code": error.code,
                            "message": error.message
                        }),
                        is_error: true,
                        metadata: serde_json::Map::new(),
                    },
                };
                if let Some(hooks) = &hooks {
                    let mut hooks = hooks.lock().await;
                    match hooks.on_tool_result(&mut result).await.map_err(|error| {
                        AgentRunError::new("hook_policy_error", error.to_string())
                    })? {
                        HookControl::Continue => {}
                        HookControl::Reject(rejection) => {
                            return Err(AgentRunError::new(rejection.code, rejection.message));
                        }
                        HookControl::Respond(_) | HookControl::StreamAbort { .. } => {
                            return Err(AgentRunError::new(
                                "agent_tool_policy_violation",
                                "Hook cannot replace or abort an Agent Tool result",
                            ));
                        }
                    }
                }
                let content = result.content;
                let is_error = result.is_error;
                send_event(
                    &events,
                    AgentEvent::ToolFinished {
                        tool: tool_id,
                        ordinal,
                        is_error,
                    },
                )
                .await?;
                Ok::<_, AgentRunError>((
                    ordinal,
                    AiItem {
                        role: Role::Tool,
                        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                            tool_use_id: call.id.clone(),
                            content,
                            is_error: Some(is_error),
                            cache_control: None,
                        }]),
                        tool_calls: None,
                        tool_call_id: Some(call.id),
                        meta: None,
                    },
                ))
            }
        });
        let outcomes = stream::iter(executions)
            .buffer_unordered(parallelism.max(1))
            .collect::<Vec<_>>()
            .await;
        let mut results = Vec::with_capacity(outcomes.len());
        let mut first_error = None;
        for outcome in outcomes {
            match outcome {
                Ok(result) => results.push(result),
                Err(error) if error.code == "tool_authorization_failed" => return Err(error),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        results.sort_by_key(|(ordinal, _)| *ordinal);
        Ok(results.into_iter().map(|(_, message)| message).collect())
    }
}

pub(super) async fn send_event(
    events: &mpsc::Sender<AgentEvent>,
    event: AgentEvent,
) -> Result<(), AgentRunError> {
    events
        .send(event)
        .await
        .map_err(|_| AgentRunError::new("cancelled", "Agent Run consumer disconnected"))
}
