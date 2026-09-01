use super::context::ParentAgentContext;
use super::tools::{ToolExecutionRequest, send_event};
use super::*;

impl AgentRunner {
    pub fn new(
        definitions: AgentDefinitionRegistry,
        model: Arc<dyn ModelTurnExecutor>,
        tools: Vec<Arc<dyn AgentTool>>,
        turns: Arc<dyn TurnChainStore>,
    ) -> Result<Self, AgentRunError> {
        Ok(Self {
            definitions,
            model,
            tools: AgentToolRegistry::new(tools)
                .map_err(|error| AgentRunError::new(error.code, error.message))?,
            run_limits: Arc::new(Mutex::new(HashMap::new())),
            run_lifecycles: Arc::from([]),
            tool_authorizer: None,
            turns,
            artifacts: None,
            output_validators: Arc::new(HashMap::new()),
            capability_model_authorizations: Arc::new(HashMap::new()),
            hooks: None,
        })
    }

    pub(crate) fn with_hook_runtime(mut self, hooks: HookRuntime) -> Self {
        self.hooks = Some(hooks);
        self
    }
    pub fn with_artifact_store(mut self, artifacts: Option<Arc<dyn ArtifactStore>>) -> Self {
        self.artifacts = artifacts;
        self
    }

    pub(crate) fn with_tool_authorizer(mut self, authorizer: Arc<dyn AgentToolAuthorizer>) -> Self {
        self.tool_authorizer = Some(authorizer);
        self
    }

    pub(crate) fn with_run_lifecycles(
        mut self,
        run_lifecycles: Vec<Arc<dyn AgentRunLifecycle>>,
    ) -> Self {
        self.run_lifecycles = run_lifecycles.into();
        self
    }

    pub fn with_output_validator(
        mut self,
        definition_id: AgentDefinitionId,
        revision: u32,
        validator: Arc<dyn AgentOutputValidator>,
    ) -> Self {
        Arc::make_mut(&mut self.output_validators).insert((definition_id, revision), validator);
        self
    }
    pub(crate) fn with_capability_model_authorization(
        mut self,
        definition_id: AgentDefinitionId,
        revision: u32,
        authorization: CapabilityModelAuthorization,
    ) -> Self {
        Arc::make_mut(&mut self.capability_model_authorizations)
            .insert((definition_id, revision), authorization);
        self
    }

    pub(crate) async fn definition_model(&self, id: &AgentDefinitionId) -> Option<String> {
        self.definitions
            .get_current(id)
            .await
            .ok()
            .filter(|record| record.config.enabled)
            .and_then(|record| record.config.model_id)
    }

    pub(crate) async fn definition_model_with_thinking_level(
        &self,
        id: &AgentDefinitionId,
    ) -> Option<(String, crate::thinking::ThinkingLevel)> {
        self.definitions
            .get_current(id)
            .await
            .ok()
            .filter(|record| record.config.enabled)
            .and_then(|record| record.config.model_id.zip(record.config.thinking_level))
    }

    pub(crate) fn validate_definition_tools(
        &self,
        definition: &crate::agent::AgentDefinitionSpec,
    ) -> Result<(), AgentRunError> {
        self.tools
            .model_specs(&definition.tools)
            .map(|_| ())
            .map_err(|error| AgentRunError::new(error.code, error.message))
    }

    pub fn run(&self, input: AgentInput) -> AgentEventStream {
        self.run_with_policy(input, AgentCommitPolicy::CommitAgentTurn, None)
    }

    #[cfg(test)]
    pub(super) fn run_ephemeral(&self, input: AgentInput) -> AgentEventStream {
        self.run_with_policy(input, AgentCommitPolicy::Ephemeral, None)
    }

    pub(crate) fn run_ephemeral_resolved(
        &self,
        input: AgentInput,
        definition_revision: u32,
        model_id: String,
        limits: AgentRunLimits,
    ) -> AgentEventStream {
        self.run_with_policy(
            input,
            AgentCommitPolicy::Ephemeral,
            Some(ResolvedAgentExecution {
                definition_revision,
                model_id,
                limits,
            }),
        )
    }

    fn run_with_policy(
        &self,
        input: AgentInput,
        commit_policy: AgentCommitPolicy,
        resolved: Option<ResolvedAgentExecution>,
    ) -> AgentEventStream {
        let runner = self.clone();
        let (events, receiver) = mpsc::channel(32);
        let driver = stream::once(async move {
            let terminal = match runner
                .execute(input, commit_policy, resolved, &events)
                .await
            {
                Ok(result) if result.completion == AgentCompletion::Completed => {
                    AgentEvent::Completed(result)
                }
                Ok(result) => AgentEvent::Partial(result),
                Err(error) => AgentEvent::Failed { error },
            };
            let _ = events.send(terminal).await;
            None::<AgentEvent>
        })
        .filter_map(futures::future::ready);
        Box::pin(stream::select(ReceiverStream::new(receiver), driver))
    }

    async fn execute(
        &self,
        input: AgentInput,
        commit_policy: AgentCommitPolicy,
        resolved: Option<ResolvedAgentExecution>,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<AgentResult, AgentRunError> {
        let cancellation = input.cancellation.clone();
        let ParentAgentContext {
            mut transcript,
            snapshot: parent_snapshot,
            root_turn_id,
        } = tokio::select! {
            _ = events.closed() => {
                cancellation.cancel();
                return Err(AgentRunError::new(
                    "cancelled",
                    "Agent Run consumer disconnected",
                ));
            }
            _ = cancellation.cancelled() => {
                return Err(AgentRunError::new("cancelled", "Agent Run cancelled"));
            }
            result = self.load_parent_context(
                &input.principal,
                input.parent_turn_id.as_ref(),
            ) => result?,
        };
        let (record, model_id, thinking_level) = if let Some(resolved) = resolved {
            if input.parent_turn_id.is_some() {
                return Err(AgentRunError::new(
                    "ephemeral_parent_not_allowed",
                    "Resolved ephemeral Agent execution cannot use an Agent parent Turn",
                ));
            }
            let mut spec = self
                .definitions
                .load_revision(&input.definition_id, resolved.definition_revision)
                .await
                .map_err(|error| {
                    AgentRunError::new("definition_revision_unavailable", error.to_string())
                })?;
            spec.budgets.model_turns = spec.budgets.model_turns.min(resolved.limits.max_turns);
            spec.budgets.total_wall_time =
                spec.budgets.total_wall_time.min(resolved.limits.total_time);
            spec.budgets.working_wall_time = spec
                .budgets
                .working_wall_time
                .min(spec.budgets.total_wall_time.mul_f64(0.8));
            (
                crate::agent::AgentDefinitionRecord {
                    spec,
                    spec_hash: String::new(),
                    config: crate::agent::AgentDefinitionConfig::default(),
                },
                resolved.model_id,
                None,
            )
        } else if let Some(snapshot) = parent_snapshot {
            if snapshot.definition_id != input.definition_id {
                return Err(AgentRunError::new(
                    "parent_turn_definition_mismatch",
                    "Parent Turn belongs to a different Agent Definition",
                ));
            }
            if self
                .definitions
                .get_current(&snapshot.definition_id)
                .await
                .is_ok_and(|current| !current.config.enabled)
            {
                return Err(AgentRunError::new(
                    "definition_disabled",
                    "Agent Definition is disabled",
                ));
            }
            let spec = self
                .definitions
                .load_revision(&snapshot.definition_id, snapshot.definition_revision)
                .await
                .map_err(|error| {
                    AgentRunError::new("definition_revision_unavailable", error.to_string())
                })?;
            (
                crate::agent::AgentDefinitionRecord {
                    spec,
                    spec_hash: String::new(),
                    config: crate::agent::AgentDefinitionConfig::default(),
                },
                snapshot.model_id,
                snapshot.thinking_level,
            )
        } else {
            let record = self
                .definitions
                .get_current(&input.definition_id)
                .await
                .map_err(|error| AgentRunError::new("definition_unavailable", error.to_string()))?;
            if !record.config.enabled {
                return Err(AgentRunError::new(
                    "definition_disabled",
                    "Agent Definition is disabled",
                ));
            }
            let model_id = record.config.model_id.clone().ok_or_else(|| {
                AgentRunError::new("model_unavailable", "Agent Definition has no bound Model")
            })?;
            let thinking_level = record.config.thinking_level;
            (record, model_id, thinking_level)
        };
        let started_at = Instant::now();
        let deadline = started_at + record.spec.budgets.total_wall_time;
        let working_deadline = started_at + record.spec.budgets.working_wall_time;
        let limiter = if let Some(concurrent_runs) = record.spec.budgets.concurrent_runs {
            let mut limits = self.run_limits.lock().await;
            Some(Arc::clone(
                limits
                    .entry((record.spec.id.clone(), record.spec.revision))
                    .or_insert_with(|| Arc::new(Semaphore::new(concurrent_runs as usize))),
            ))
        } else {
            None
        };
        let _run_permit = if let Some(limiter) = limiter {
            Some(tokio::select! {
                _ = events.closed() => {
                    return Err(AgentRunError::new(
                        "cancelled",
                        "Agent Run consumer disconnected",
                    ));
                }
                _ = cancellation.cancelled() => {
                    return Err(AgentRunError::new("cancelled", "Agent Run cancelled"));
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    cancellation.cancel();
                    return Err(AgentRunError::new(
                        "deadline_exceeded",
                        "Agent Run deadline exceeded while waiting for capacity",
                    ));
                }
                permit = limiter.acquire_owned() => permit.map_err(|_| {
                    AgentRunError::new("runner_unavailable", "Agent Runner concurrency gate closed")
                })?,
            })
        } else {
            None
        };
        let allowed_tools = self
            .tools
            .model_specs(&record.spec.tools)
            .map_err(|error| AgentRunError::new(error.code, error.message))?;
        let turn_id = AgentTurnId::agent();
        // Child turns append to the same prompt prefix; keep provider cache routing on the root
        // identity while independent Agent chains remain isolated.
        let generation_session_id = root_turn_id.as_ref().unwrap_or(&turn_id);
        let mut _run_guards = Vec::with_capacity(self.run_lifecycles.len());
        for lifecycle in self.run_lifecycles.iter() {
            _run_guards.push(lifecycle.start(&input.principal, &turn_id).await?);
        }
        send_event(
            events,
            AgentEvent::RunStarted {
                turn_id: turn_id.clone(),
            },
        )
        .await?;

        let mut content = vec![ContentBlock::Text {
            text: input.prompt.clone(),
            cache_control: None,
        }];
        let artifact_blocks = tokio::select! {
            _ = events.closed() => {
                cancellation.cancel();
                return Err(AgentRunError::new(
                    "cancelled",
                    "Agent Run consumer disconnected",
                ));
            }
            _ = cancellation.cancelled() => {
                return Err(AgentRunError::new("cancelled", "Agent Run cancelled"));
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                cancellation.cancel();
                return Err(AgentRunError::new(
                    "deadline_exceeded",
                    "Agent Run deadline exceeded while loading Artifacts",
                ));
            }
            result = self.load_artifact_blocks(
                &input.principal,
                &input.artifacts,
                &record.spec.artifact_policy,
            ) => result?,
        };
        content.extend(artifact_blocks);
        transcript.push(AiItem {
            role: Role::User,
            content: if content.len() == 1 {
                MessageContent::Text(input.prompt.clone())
            } else {
                MessageContent::Blocks(content)
            },
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        });
        let mut model_transcript = self
            .hydrate_transcript(&input.principal, &transcript)
            .await?;
        let model_instructions = model_instructions(&record.spec);
        let mut initial_request = AiRequest::new(&model_id, model_transcript.clone());
        initial_request.instructions = Some(model_instructions.clone());
        initial_request.reasoning.level = thinking_level;
        initial_request.tools = (!allowed_tools.is_empty()).then_some(allowed_tools.clone());
        crate::generation_chain::set_generation_session_id(
            &mut initial_request,
            generation_session_id.as_str(),
        );
        let hooks = self
            .hooks
            .as_ref()
            .map(|runtime| {
                runtime.begin(
                    SessionContext {
                        request_id: format!("agent-request-{}", turn_id.as_str()),
                        run_id: turn_id.as_str().to_owned(),
                        request_kind: RequestKind::Generation,
                        ingress: crate::protocol::ids::OPEN_RESPONSES_2026_04_24,
                        transport: TransportKind::Http,
                        principal: input.principal.clone(),
                        cancellation: cancellation.clone(),
                        inherited_media_turns: Vec::new(),
                        response_id: None,
                        previous_response_id: None,
                    },
                    &initial_request,
                    ContextCompleteness::Full,
                )
            })
            .transpose()
            .map_err(|error| AgentRunError::new("hook_policy_error", error.to_string()))?
            .map(|run| Arc::new(tokio::sync::Mutex::new(run)));
        let capability_authorization = self
            .capability_model_authorizations
            .get(&(record.spec.id.clone(), record.spec.revision))
            .copied();
        let mut total_usage = Usage::default();
        let mut model_turns = 0_u32;
        let mut tool_calls = 0_u32;
        let mut repair_attempts = 0_u32;
        let mut partial = false;
        let mut finalizing = false;

        loop {
            if events.is_closed() {
                cancellation.cancel();
                return Err(AgentRunError::new(
                    "cancelled",
                    "Agent Run consumer disconnected",
                ));
            }
            if Instant::now() >= deadline {
                cancellation.cancel();
                return Err(AgentRunError::new(
                    "deadline_exceeded",
                    "Agent Run deadline exceeded",
                ));
            }
            if model_turns >= record.spec.budgets.model_turns {
                return Err(AgentRunError::new(
                    "model_turn_limit",
                    "Agent Run exhausted its Model Turn budget",
                ));
            }
            if !finalizing
                && (Instant::now() >= working_deadline
                    || model_turns.saturating_add(1) >= record.spec.budgets.model_turns)
            {
                partial = true;
                finalizing = true;
                append_finalization_instruction(&mut transcript, &mut model_transcript);
            }
            model_turns += 1;
            send_event(
                events,
                AgentEvent::ModelStepStarted {
                    ordinal: model_turns,
                },
            )
            .await?;

            let mut request = AiRequest::new(&model_id, model_transcript.clone());
            request.instructions = Some(model_instructions.clone());
            request.reasoning.level = thinking_level;
            crate::generation_chain::set_generation_session_id(
                &mut request,
                generation_session_id.as_str(),
            );
            request.parallel_tool_calls = Some(
                record
                    .spec
                    .budgets
                    .tool_parallelism
                    .is_none_or(|value| value > 1),
            );
            let turn_tools = if finalizing {
                Vec::new()
            } else {
                allowed_tools.clone()
            };
            request.tools = (!turn_tools.is_empty()).then_some(turn_tools);
            let fixed_model = request.model.clone();
            let fixed_tools = request.tools.clone();
            let hook_response = if let Some(hooks) = &hooks {
                let mut hooks = hooks.lock().await;
                let control = hooks
                    .on_request(&mut request)
                    .await
                    .map_err(|error| AgentRunError::new("hook_policy_error", error.to_string()))?;
                if hooks.has_exposed_tools()
                    || request.model != fixed_model
                    || request.tools != fixed_tools
                {
                    return Err(AgentRunError::new(
                        "agent_tool_policy_violation",
                        "Hook cannot change the Model or Tool allowlist of an Agent Run",
                    ));
                }
                match control {
                    HookControl::Continue => None,
                    HookControl::Respond(response) => Some(*response),
                    HookControl::Reject(rejection) => {
                        return Err(AgentRunError::new(rejection.code, rejection.message));
                    }
                    HookControl::StreamAbort { message } => {
                        return Err(AgentRunError::new("hook_stream_aborted", message));
                    }
                }
            } else {
                None
            };
            let turn_deadline = if finalizing {
                deadline
            } else {
                working_deadline
            };
            let response_result = if let Some(response) = hook_response {
                Ok(response)
            } else {
                self.execute_model_turn(
                    {
                        let mut turn_input =
                            TurnInput::new(input.principal.clone(), request.clone())
                                .with_execution(cancellation.clone(), turn_deadline);
                        if capability_authorization.is_some() {
                            turn_input = turn_input
                                .with_authorization(ModelTurnAuthorization::CapabilityGrant);
                        }
                        turn_input
                    },
                    events,
                    hooks.as_ref(),
                )
                .await
            };
            let mut response = match response_result {
                Err(error)
                    if !finalizing
                        && error.code == "deadline_exceeded"
                        && Instant::now() >= working_deadline =>
                {
                    partial = true;
                    finalizing = true;
                    append_finalization_instruction(&mut transcript, &mut model_transcript);
                    continue;
                }
                result => result?,
            };
            if let Some(hooks) = &hooks {
                let mut hooks = hooks.lock().await;
                let outcome = hooks
                    .on_upstream_response_outcome(&request, &mut response)
                    .await
                    .map_err(|error| AgentRunError::new("hook_policy_error", error.to_string()))?;
                response = match outcome.control {
                    HookControl::Continue => response,
                    HookControl::Respond(replacement) => *replacement,
                    HookControl::Reject(rejection) => {
                        return Err(AgentRunError::new(rejection.code, rejection.message));
                    }
                    HookControl::StreamAbort { message } => {
                        return Err(AgentRunError::new("hook_stream_aborted", message));
                    }
                };
                hooks.next_round();
            }
            accumulate_usage(&mut total_usage, &response.usage);
            if record
                .spec
                .budgets
                .total_tokens
                .is_some_and(|limit| total_usage.total_tokens > limit)
            {
                return Err(AgentRunError::new(
                    "token_limit",
                    "Agent Run exceeded its total Token budget",
                ));
            }
            send_event(
                events,
                AgentEvent::UsageUpdated {
                    usage: total_usage.clone(),
                },
            )
            .await?;
            transcript.extend(response.items.iter().cloned());
            model_transcript.extend(response.items.iter().cloned());

            let response_tool_calls = response.tool_calls().cloned().collect::<Vec<_>>();
            if !response_tool_calls.is_empty() {
                if finalizing {
                    return Err(AgentRunError::new(
                        "tool_call_during_finalization",
                        "Model requested a Tool after Tool use was disabled",
                    ));
                }
                let working_tokens_exhausted = match (
                    record.spec.budgets.total_tokens,
                    record.spec.budgets.finalization_tokens,
                ) {
                    (Some(total), Some(finalization)) => {
                        total_usage.total_tokens >= total.saturating_sub(finalization)
                    }
                    _ => false,
                };
                let next_call_count = tool_calls.saturating_add(response_tool_calls.len() as u32);
                let tool_calls_exhausted = record
                    .spec
                    .budgets
                    .tool_calls
                    .is_some_and(|limit| next_call_count > limit);
                if Instant::now() >= working_deadline
                    || working_tokens_exhausted
                    || tool_calls_exhausted
                    || model_turns.saturating_add(1) >= record.spec.budgets.model_turns
                {
                    partial = true;
                    finalizing = true;
                    append_finalization_instruction(&mut transcript, &mut model_transcript);
                    continue;
                }
                let results = self
                    .execute_tools(ToolExecutionRequest {
                        hooks: hooks.clone(),
                        allowlist: &record.spec.tools,
                        principal: &input.principal,
                        definition_id: &input.definition_id,
                        model_id: &model_id,
                        turn_id: &turn_id,
                        cancellation: &cancellation,
                        deadline: working_deadline,
                        calls: response_tool_calls,
                        parallelism: record.spec.budgets.tool_parallelism,
                        ordinal_offset: tool_calls,
                        events,
                    })
                    .await?;
                tool_calls = next_call_count;
                transcript.extend(results.clone());
                model_transcript.extend(results);
                continue;
            }

            let completion = if partial {
                AgentCompletion::Partial
            } else {
                AgentCompletion::Completed
            };
            let response_text = response.output_text();
            let output = match parse_output(&response_text, record.spec.output_schema.as_ref()) {
                Ok(output) => {
                    if let Some(validator) = self
                        .output_validators
                        .get(&(record.spec.id.clone(), record.spec.revision))
                    {
                        validator
                            .validate(
                                &AgentOutputValidationContext {
                                    principal: input.principal.clone(),
                                    turn_id: turn_id.clone(),
                                    definition_id: record.spec.id.clone(),
                                    definition_revision: record.spec.revision,
                                    completion,
                                },
                                &transcript,
                                output,
                            )
                            .await
                    } else {
                        Ok(output)
                    }
                }
                Err(error) => Err(error),
            };
            if input.cancellation.is_cancelled() {
                return Err(AgentRunError::new("cancelled", "Agent Run cancelled"));
            }
            match output {
                Ok(output) => {
                    if !response_text.is_empty() {
                        send_event(
                            events,
                            AgentEvent::PublicOutputDelta {
                                text: response_text,
                            },
                        )
                        .await?;
                    }
                    let result = AgentResult {
                        turn_id: turn_id.clone(),
                        completion,
                        output,
                        usage: total_usage,
                    };
                    if commit_policy == AgentCommitPolicy::CommitAgentTurn {
                        // Validators may durably retain evidence. Once pre-commit starts,
                        // Turn commit owns completion so cancellation cannot orphan that state.
                        if input.cancellation.is_cancelled() {
                            return Err(AgentRunError::new("cancelled", "Agent Run cancelled"));
                        }
                        if let Some(validator) = self
                            .output_validators
                            .get(&(record.spec.id.clone(), record.spec.revision))
                        {
                            validator
                                .before_commit(
                                    &AgentOutputValidationContext {
                                        principal: input.principal.clone(),
                                        turn_id: turn_id.clone(),
                                        definition_id: record.spec.id.clone(),
                                        definition_revision: record.spec.revision,
                                        completion,
                                    },
                                    &transcript,
                                    &result.output,
                                )
                                .await?;
                        }
                        self.commit_turn(
                            &input,
                            &record,
                            &model_id,
                            thinking_level,
                            &turn_id,
                            &transcript,
                            &result,
                        )
                        .await?;
                    }
                    return Ok(result);
                }
                Err(error)
                    if repair_attempts < record.spec.repair_attempts
                        && record
                            .spec
                            .budgets
                            .total_tokens
                            .is_none_or(|limit| total_usage.total_tokens < limit) =>
                {
                    repair_attempts += 1;
                    finalizing = true;
                    let instruction = user_instruction(format!(
                        "Your answer did not satisfy the required output schema: {}. Return only corrected JSON.",
                        error.message
                    ));
                    transcript.push(instruction.clone());
                    model_transcript.push(instruction);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn execute_model_turn(
        &self,
        input: TurnInput,
        events: &mpsc::Sender<AgentEvent>,
        hooks: Option<&Arc<tokio::sync::Mutex<InferenceRun>>>,
    ) -> Result<AiResponse, AgentRunError> {
        let deadline = input.deadline;
        let cancellation = input.cancellation.clone();
        let turn = self
            .model
            .execute(input)
            .await
            .map_err(|error| AgentRunError::new(error.code, error.message))?;
        if let Some(hooks) = hooks {
            hooks.lock().await.set_route(turn.route.clone());
        }
        let mut stream = turn.output;
        loop {
            let event = tokio::select! {
                _ = events.closed() => {
                    return Err(AgentRunError::new(
                        "cancelled",
                        "Agent Run consumer disconnected",
                    ));
                }
                _ = cancellation.cancelled() => {
                    return Err(AgentRunError::new("cancelled", "Agent Run was cancelled"));
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(AgentRunError::new(
                        "deadline_exceeded",
                        "Agent Model Turn deadline exceeded",
                    ));
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event.map_err(|error| AgentRunError::new(error.code, error.message))? {
                CanonicalEvent::Delta(_) => {}
                CanonicalEvent::Completed(response) => return Ok(*response),
            }
        }
        Err(AgentRunError::new(
            "model_stream_incomplete",
            "Model Turn ended without a completion",
        ))
    }

    async fn commit_turn(
        &self,
        input: &AgentInput,
        record: &crate::agent::AgentDefinitionRecord,
        model_id: &str,
        thinking_level: Option<crate::thinking::ThinkingLevel>,
        turn_id: &AgentTurnId,
        transcript: &[AiItem],
        result: &AgentResult,
    ) -> Result<(), AgentRunError> {
        self.turns
            .commit(TurnCommit {
                id: turn_id.clone(),
                parent_id: input.parent_turn_id.clone(),
                principal: input.principal.clone(),
                kind: TurnNodeKind::Agent,
                payload_version: 1,
                payload: serde_json::json!({
                    "definition_id": record.spec.id.as_str(),
                    "definition_revision": record.spec.revision,
                    "model_id": model_id,
                    "thinking_level": thinking_level,
                    "transcript": transcript,
                    "completion": result.completion,
                    "output": result.output,
                    "usage": result.usage,
                }),
                idle_ttl: Duration::from_secs(7 * 24 * 60 * 60),
                reusable_prefix: None,
            })
            .await
            .map_err(|error| AgentRunError::new("turn_commit_failed", error.to_string()))?;
        Ok(())
    }
}

pub(super) fn model_instructions(spec: &AgentDefinitionSpec) -> String {
    let Some(schema) = spec.output_schema.as_ref() else {
        return spec.instructions.clone();
    };
    format!(
        "{}\n\nReturn only JSON matching this output schema:\n{schema}",
        spec.instructions
    )
}

fn append_finalization_instruction(
    transcript: &mut Vec<AiItem>,
    model_transcript: &mut Vec<AiItem>,
) {
    let instruction = user_instruction(
        "The working budget is exhausted. Return the best possible final answer now without calling tools.",
    );
    transcript.push(instruction.clone());
    model_transcript.push(instruction);
}

fn user_instruction(text: impl Into<String>) -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Text(text.into()),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

fn accumulate_usage(total: &mut Usage, turn: &Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(turn.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(turn.completion_tokens);
    total.total_tokens = total.total_tokens.saturating_add(turn.total_tokens);
    total.cache_read_tokens = add_optional_usage(total.cache_read_tokens, turn.cache_read_tokens);
    total.cache_creation_tokens =
        add_optional_usage(total.cache_creation_tokens, turn.cache_creation_tokens);
    if let Some(turn_tools) = turn.server_tool_use.as_ref() {
        let tools = total.server_tool_use.get_or_insert_with(Default::default);
        tools.web_search_requests = tools
            .web_search_requests
            .saturating_add(turn_tools.web_search_requests);
        tools.web_fetch_requests = tools
            .web_fetch_requests
            .saturating_add(turn_tools.web_fetch_requests);
    }
}

fn add_optional_usage(total: Option<u32>, turn: Option<u32>) -> Option<u32> {
    match (total, turn) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}
