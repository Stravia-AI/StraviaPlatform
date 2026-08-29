use super::*;

impl HookRuntime {
    pub fn new(hooks: Vec<Arc<dyn Hook>>) -> Self {
        Self::with_tools(hooks, PlatformToolRegistry::default())
    }

    pub fn with_tools(hooks: Vec<Arc<dyn Hook>>, tools: PlatformToolRegistry) -> Self {
        Self {
            hooks: hooks.into(),
            tools,
        }
    }

    pub fn descriptors(&self) -> Vec<HookDescriptor> {
        self.hooks.iter().map(|hook| hook.descriptor()).collect()
    }

    pub(crate) fn begin(
        &self,
        context: SessionContext,
        request: &AiRequest,
        completeness: ContextCompleteness,
    ) -> Result<InferenceRun, HookError> {
        let sessions = self
            .hooks
            .iter()
            .map(|hook| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let descriptor = hook.descriptor();
                    RuntimeSession {
                        descriptor,
                        session: hook.create_session(&context),
                        delayed_events: 0,
                        held_variants: Vec::new(),
                    }
                }))
                .map_err(|_| HookError::Runtime {
                    message: "hook session creation panicked".into(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let original = ContextSnapshot::from_request(request, completeness);
        Ok(InferenceRun {
            cancellation: context.cancellation.clone(),
            context,
            original: original.clone(),
            current: original,
            sessions,
            route: None,
            round: 0,
            tools: self.tools.clone(),
            exposed_tools: HashMap::new(),
            exposed_tool_specs: HashMap::new(),
            skips: Vec::new(),
            stream_leg_open: false,
        })
    }
}

struct RuntimeSession {
    descriptor: HookDescriptor,
    session: Box<dyn HookSession>,
    delayed_events: usize,
    held_variants: Vec<SemanticVariant>,
}
pub(crate) struct InferenceRun {
    cancellation: crate::proxy::context::CancellationToken,
    context: SessionContext,
    original: ContextSnapshot,
    current: ContextSnapshot,
    sessions: Vec<RuntimeSession>,
    route: Option<RouteContext>,
    pub(super) round: u32,
    tools: PlatformToolRegistry,
    exposed_tools: HashMap<String, ToolId>,
    exposed_tool_specs: HashMap<String, ToolSpec>,
    skips: Vec<HookSkip>,
    stream_leg_open: bool,
}

#[derive(Clone)]
pub(crate) struct DetachedPlatformExecution {
    tools: PlatformToolRegistry,
    call: PlatformToolCall,
    context: ToolExecutionContext,
    limit: std::time::Duration,
}

impl DetachedPlatformExecution {
    pub(crate) fn activity(&self) -> &str {
        self.tools
            .activity_label(&self.call.tool_id)
            .unwrap_or("Running a platform tool")
    }

    pub(crate) fn limit(&self) -> std::time::Duration {
        self.limit
    }

    pub(crate) fn parallel_safe(&self) -> bool {
        self.tools.parallel_safe(&self.call.tool_id)
    }

    pub(crate) fn call(&self) -> &PlatformToolCall {
        &self.call
    }

    pub(crate) async fn execute(self) -> PlatformToolResult {
        let Self {
            tools,
            call,
            context,
            ..
        } = self;
        tools
            .execute(
                &call.tool_id,
                call.call.id.clone(),
                serde_json::from_str(&call.call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.call.arguments.clone())),
                context,
            )
            .await
    }
}

impl InferenceRun {
    pub fn set_route(&mut self, route: RouteContext) {
        self.route = Some(route);
    }

    pub fn next_round(&mut self) {
        self.round += 1;
    }

    pub fn requires_terminal_buffering(&self) -> bool {
        self.sessions.iter().any(|session| {
            session.session.requires_terminal_buffering()
                && (session
                    .descriptor
                    .accepts(self.context.request_kind, EventKind::UpstreamResponse)
                    || session
                        .descriptor
                        .accepts(self.context.request_kind, EventKind::ClientOutput))
        })
    }

    pub(crate) fn detached_platform_execution(
        &self,
        call: PlatformToolCall,
        cancellation: crate::proxy::context::CancellationToken,
    ) -> DetachedPlatformExecution {
        let limit = self.tools.execution_limit(&call.tool_id);
        DetachedPlatformExecution {
            tools: self.tools.clone(),
            call,
            context: ToolExecutionContext {
                request_id: self.context.request_id.clone(),
                run_id: self.context.run_id.clone(),
                principal: self.context.principal.clone(),
                cancellation,
                progress: None,
            },
            limit,
        }
    }
    pub async fn on_request(&mut self, request: &mut AiRequest) -> Result<HookControl, HookError> {
        for index in 0..self.sessions.len() {
            let accepts = self.sessions[index]
                .descriptor
                .accepts(self.context.request_kind, EventKind::Request);
            if !accepts {
                continue;
            }
            if should_skip_for_partial(&self.sessions[index].descriptor, &self.current) {
                self.skips.push(HookSkip {
                    hook_id: self.sessions[index].descriptor.id.clone(),
                    event: EventKind::Request,
                    reason: "requires_full_context_on_partial".into(),
                });
                continue;
            }

            let hook_view = hook_request_view(request);
            let event = HookEvent::Request {
                session: &self.context,
                original: &self.original,
                current: &hook_view,
                context: &self.current,
                route: self.route.as_ref(),
                round: self.round,
            };
            let runtime_session = &mut self.sessions[index];
            let hook_id = runtime_session.descriptor.id.clone();
            let batch = invoke_hook(
                &mut *runtime_session.session,
                event,
                &hook_id,
                self.cancellation.clone(),
            )
            .await?;
            let control = apply_request_actions(
                &runtime_session.descriptor.id,
                request,
                &mut self.current,
                &mut self.exposed_tools,
                &mut self.exposed_tool_specs,
                &self.tools,
                batch,
            )?;
            if !matches!(control, HookControl::Continue) {
                return Ok(control);
            }
        }
        Ok(HookControl::Continue)
    }

    #[cfg(test)]
    pub async fn on_upstream_response(
        &mut self,
        request: &AiRequest,
        response: &mut AiResponse,
    ) -> Result<HookControl, HookError> {
        Ok(self
            .on_upstream_response_outcome(request, response)
            .await?
            .control)
    }

    pub(crate) async fn on_upstream_response_outcome(
        &mut self,
        request: &AiRequest,
        response: &mut AiResponse,
    ) -> Result<ResponseHookOutcome, HookError> {
        let route = self.route.as_ref().ok_or_else(|| HookError::Runtime {
            message: "route context is required before UpstreamResponse".into(),
        })?;
        let hook_request = hook_request_view(request);
        let mut modified = false;
        for index in 0..self.sessions.len() {
            let accepts = self.sessions[index]
                .descriptor
                .accepts(self.context.request_kind, EventKind::UpstreamResponse);
            if !accepts {
                continue;
            }
            if should_skip_for_partial(&self.sessions[index].descriptor, &self.current) {
                self.skips.push(HookSkip {
                    hook_id: self.sessions[index].descriptor.id.clone(),
                    event: EventKind::UpstreamResponse,
                    reason: "requires_full_context_on_partial".into(),
                });
                continue;
            }
            let classified = self.classify_tool_calls(response);
            let event = HookEvent::UpstreamResponse {
                session: &self.context,
                request: &hook_request,
                response,
                classified: &classified,
                route,
                round: self.round,
            };
            let runtime_session = &mut self.sessions[index];
            let hook_id = runtime_session.descriptor.id.clone();
            let batch = invoke_hook(
                &mut *runtime_session.session,
                event,
                &hook_id,
                self.cancellation.clone(),
            )
            .await?;
            let (control, patched) = apply_response_actions(
                &runtime_session.descriptor.id,
                EventKind::UpstreamResponse,
                response,
                batch,
            )?;
            modified |= patched;
            if !matches!(control, HookControl::Continue) {
                return Ok(ResponseHookOutcome { control, modified });
            }
        }
        Ok(ResponseHookOutcome {
            control: HookControl::Continue,
            modified,
        })
    }

    pub async fn on_tool_result(
        &mut self,
        result: &mut PlatformToolResult,
    ) -> Result<HookControl, HookError> {
        let route = self.route.as_ref().ok_or_else(|| HookError::Runtime {
            message: "route context is required before ToolResult".into(),
        })?;
        for index in 0..self.sessions.len() {
            let accepts = self.sessions[index]
                .descriptor
                .accepts(self.context.request_kind, EventKind::ToolResult);
            if !accepts {
                continue;
            }
            if should_skip_for_partial(&self.sessions[index].descriptor, &self.current) {
                self.skips.push(HookSkip {
                    hook_id: self.sessions[index].descriptor.id.clone(),
                    event: EventKind::ToolResult,
                    reason: "requires_full_context_on_partial".into(),
                });
                continue;
            }
            let event = HookEvent::ToolResult {
                session: &self.context,
                result,
                route,
                round: self.round,
            };
            let runtime_session = &mut self.sessions[index];
            let hook_id = runtime_session.descriptor.id.clone();
            let batch = invoke_hook(
                &mut *runtime_session.session,
                event,
                &hook_id,
                self.cancellation.clone(),
            )
            .await?;
            let control = apply_tool_result_actions(&runtime_session.descriptor.id, result, batch)?;
            if !matches!(control, HookControl::Continue) {
                return Ok(control);
            }
        }
        Ok(HookControl::Continue)
    }

    pub async fn on_client_output(
        &mut self,
        response: &mut AiResponse,
    ) -> Result<HookControl, HookError> {
        Ok(self.on_client_output_outcome(response).await?.control)
    }

    pub(crate) async fn on_client_output_outcome(
        &mut self,
        response: &mut AiResponse,
    ) -> Result<ResponseHookOutcome, HookError> {
        let route = self.route.as_ref().ok_or_else(|| HookError::Runtime {
            message: "route context is required before ClientOutput".into(),
        })?;
        let mut modified = false;
        for index in 0..self.sessions.len() {
            let accepts = self.sessions[index]
                .descriptor
                .accepts(self.context.request_kind, EventKind::ClientOutput);
            if !accepts {
                continue;
            }
            if should_skip_for_partial(&self.sessions[index].descriptor, &self.current) {
                self.skips.push(HookSkip {
                    hook_id: self.sessions[index].descriptor.id.clone(),
                    event: EventKind::ClientOutput,
                    reason: "requires_full_context_on_partial".into(),
                });
                continue;
            }
            let event = HookEvent::ClientOutput {
                session: &self.context,
                response,
                route,
                round: self.round,
            };
            let runtime_session = &mut self.sessions[index];
            let hook_id = runtime_session.descriptor.id.clone();
            let batch = invoke_hook(
                &mut *runtime_session.session,
                event,
                &hook_id,
                self.cancellation.clone(),
            )
            .await?;
            let (control, patched) = apply_response_actions(
                &runtime_session.descriptor.id,
                EventKind::ClientOutput,
                response,
                batch,
            )?;
            modified |= patched;
            if !matches!(control, HookControl::Continue) {
                return Ok(ResponseHookOutcome { control, modified });
            }
        }
        Ok(ResponseHookOutcome {
            control: HookControl::Continue,
            modified,
        })
    }

    pub fn has_exposed_tools(&self) -> bool {
        !self.exposed_tools.is_empty()
    }

    pub(crate) fn is_exposed_tool(&self, name: &str) -> bool {
        self.exposed_tools.contains_key(name)
    }

    pub(crate) fn could_be_exposed_tool_prefix(&self, name: &str) -> bool {
        self.exposed_tools
            .keys()
            .any(|registered| registered.starts_with(name))
    }

    pub(crate) fn remove_exposed_tools(&self, request: &mut AiRequest) {
        let Some(tools) = request.tools.as_mut() else {
            return;
        };
        tools.retain(|tool| !self.exposed_tools.contains_key(&tool.name));
        if tools.is_empty() {
            request.tools = None;
        }
    }

    pub fn classify_tool_calls(&self, response: &AiResponse) -> ClassifiedToolCalls {
        let mut classified = ClassifiedToolCalls::default();
        for call in response.tool_calls() {
            if let Some(tool_id) = self.exposed_tools.get(&call.name) {
                classified.platform.push(PlatformToolCall {
                    tool_id: tool_id.clone(),
                    call: call.clone(),
                });
            } else {
                classified.client.push(call.clone());
            }
        }
        classified
    }

    fn begin_stream_leg(&mut self) -> Result<(), HookError> {
        if self.stream_leg_open {
            return Ok(());
        }
        // Mark the leg open before third-party code runs so every failure
        // path can still perform the matching close callbacks exactly once.
        self.stream_leg_open = true;
        for runtime_session in &mut self.sessions {
            let hook_id = runtime_session.descriptor.id.clone();
            let _ = invoke_stream_callback(
                &mut *runtime_session.session,
                &hook_id,
                "begin",
                |transformer| transformer.begin(),
            )?;
            runtime_session.delayed_events = 0;
            runtime_session.held_variants.clear();
        }
        Ok(())
    }

    pub fn transform_stream(
        &mut self,
        delta: AiStreamDelta,
    ) -> Result<Vec<AiStreamDelta>, HookError> {
        self.begin_stream_leg()?;
        let terminal = matches!(
            delta,
            AiStreamDelta::Done { .. }
                | AiStreamDelta::StreamError { .. }
                | AiStreamDelta::UnexpectedEof
        );
        let mut output = if terminal {
            self.flush_stream()?
        } else {
            Vec::new()
        };
        let current = self.current.clone();
        let transformed = transform_through(
            &mut self.sessions,
            self.context.request_kind,
            &current,
            &mut self.skips,
            vec![delta],
        )?;
        output.extend(transformed);
        Ok(output)
    }

    pub fn flush_stream(&mut self) -> Result<Vec<AiStreamDelta>, HookError> {
        if !self.stream_leg_open {
            return Ok(Vec::new());
        }
        let output = self.drain_stream(true);
        // drain_stream attempts every close callback, even after an error.
        // Do not permit a later cleanup path to close this leg a second time.
        self.stream_leg_open = false;
        output
    }

    fn drain_stream(&mut self, close: bool) -> Result<Vec<AiStreamDelta>, HookError> {
        let mut output = Vec::new();
        let mut first_error = None;
        let current = self.current.clone();
        for index in 0..self.sessions.len() {
            let accepts = self.sessions[index]
                .descriptor
                .accepts(self.context.request_kind, EventKind::Stream);
            if !accepts {
                continue;
            }
            if should_skip_for_partial(&self.sessions[index].descriptor, &current) {
                record_skip(
                    &mut self.skips,
                    &self.sessions[index].descriptor.id,
                    EventKind::Stream,
                    "requires_full_context_on_partial",
                );
                continue;
            }
            let flushed = {
                let runtime_session = &mut self.sessions[index];
                let hook_id = runtime_session.descriptor.id.clone();
                let max_buffered_bytes = runtime_session.descriptor.max_buffered_bytes;
                let events = match invoke_stream_callback(
                    &mut *runtime_session.session,
                    &hook_id,
                    if close { "close" } else { "flush" },
                    |transformer| {
                        if close {
                            transformer.close()
                        } else {
                            transformer.flush()
                        }
                    },
                ) {
                    Ok(Some(events)) => events,
                    Ok(None) => continue,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        runtime_session.delayed_events = 0;
                        runtime_session.held_variants.clear();
                        continue;
                    }
                };
                if events.iter().any(|event| !is_semantic(event)) {
                    first_error.get_or_insert(invalid_action(
                        &hook_id,
                        EventKind::Stream,
                        "a transformer may flush only semantic events",
                    ));
                    runtime_session.delayed_events = 0;
                    runtime_session.held_variants.clear();
                    continue;
                }
                if events.iter().any(|event| {
                    semantic_variant(event)
                        .is_none_or(|variant| !runtime_session.held_variants.contains(&variant))
                }) {
                    first_error.get_or_insert(invalid_action(
                        &hook_id,
                        EventKind::Stream,
                        "flush changed the semantic variant or tool index",
                    ));
                    runtime_session.delayed_events = 0;
                    runtime_session.held_variants.clear();
                    continue;
                }
                let buffered_bytes = match invoke_stream_callback(
                    &mut *runtime_session.session,
                    &hook_id,
                    "buffered_bytes",
                    |transformer| Ok(transformer.buffered_bytes()),
                ) {
                    Ok(Some(buffered_bytes)) => buffered_bytes,
                    Ok(None) => 0,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        runtime_session.delayed_events = 0;
                        runtime_session.held_variants.clear();
                        continue;
                    }
                };
                if buffered_bytes > max_buffered_bytes {
                    first_error.get_or_insert(invalid_action(
                        &hook_id,
                        EventKind::Stream,
                        format!(
                            "buffered {buffered_bytes} bytes, exceeding limit {max_buffered_bytes}"
                        ),
                    ));
                    runtime_session.delayed_events = 0;
                    runtime_session.held_variants.clear();
                    continue;
                }
                if buffered_bytes != 0 {
                    first_error.get_or_insert(invalid_action(
                        &hook_id,
                        EventKind::Stream,
                        "flush must leave the transformer buffer empty",
                    ));
                    runtime_session.delayed_events = 0;
                    runtime_session.held_variants.clear();
                    continue;
                }
                runtime_session.delayed_events = 0;
                runtime_session.held_variants.clear();
                events
            };
            if flushed.is_empty() {
                continue;
            }
            match transform_through(
                &mut self.sessions[index + 1..],
                self.context.request_kind,
                &current,
                &mut self.skips,
                flushed,
            ) {
                Ok(transformed) => output.extend(transformed),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(output),
        }
    }
}

fn invoke_stream_callback<T>(
    session: &mut dyn HookSession,
    hook_id: &HookId,
    operation: &'static str,
    callback: impl FnOnce(&mut dyn StreamTransformer) -> Result<T, String>,
) -> Result<Option<T>, HookError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(transformer) = session.stream_transformer() else {
            return Ok(None);
        };
        callback(transformer).map(Some)
    }))
    .map_err(|_| HookError::Failed {
        hook_id: hook_id.clone(),
        message: format!("stream transformer {operation} panicked"),
    })?
    .map_err(|message| HookError::Failed {
        hook_id: hook_id.clone(),
        message,
    })
}

fn record_skip(skips: &mut Vec<HookSkip>, hook_id: &HookId, event: EventKind, reason: &str) {
    if skips
        .iter()
        .any(|skip| skip.hook_id == *hook_id && skip.event == event && skip.reason == reason)
    {
        return;
    }
    skips.push(HookSkip {
        hook_id: hook_id.clone(),
        event,
        reason: reason.into(),
    });
}

fn transform_through(
    sessions: &mut [RuntimeSession],
    request_kind: RequestKind,
    current: &ContextSnapshot,
    skips: &mut Vec<HookSkip>,
    mut events: Vec<AiStreamDelta>,
) -> Result<Vec<AiStreamDelta>, HookError> {
    for runtime_session in sessions {
        if !runtime_session
            .descriptor
            .accepts(request_kind, EventKind::Stream)
        {
            continue;
        }
        if should_skip_for_partial(&runtime_session.descriptor, current) {
            record_skip(
                skips,
                &runtime_session.descriptor.id,
                EventKind::Stream,
                "requires_full_context_on_partial",
            );
            continue;
        }
        let hook_id = runtime_session.descriptor.id.clone();
        let max_buffered_bytes = runtime_session.descriptor.max_buffered_bytes;
        let max_delayed_events = runtime_session.descriptor.max_delayed_events;
        let mut next = Vec::new();
        for event in events {
            let semantic = is_semantic(&event);
            let expected_variant = semantic_variant(&event);
            let Some(directive) = invoke_stream_callback(
                &mut *runtime_session.session,
                &hook_id,
                "transform",
                |transformer| transformer.transform(&event),
            )?
            else {
                next.push(event);
                continue;
            };
            if !semantic && !matches!(directive, StreamDirective::Pass) {
                return Err(invalid_action(
                    &hook_id,
                    EventKind::Stream,
                    "structural stream events are read-only",
                ));
            }
            let was_hold = matches!(directive, StreamDirective::Hold);
            match directive {
                StreamDirective::Pass => next.push(event),
                StreamDirective::Emit(mut output) | StreamDirective::Replace(mut output) => {
                    if !preserve_stream_coordinates(&event, &mut output)
                        || output.iter().any(|delta| {
                            !is_semantic(delta) || semantic_variant(delta) != expected_variant
                        })
                    {
                        return Err(invalid_action(
                            &hook_id,
                            EventKind::Stream,
                            "a transformer may change only the same semantic variant, output coordinates, and tool index",
                        ));
                    }
                    next.extend(output);
                    runtime_session.delayed_events = 0;
                }
                StreamDirective::Hold => {
                    runtime_session.delayed_events += 1;
                    if let Some(variant) = expected_variant {
                        runtime_session.held_variants.push(variant);
                    }
                }
                StreamDirective::Drop => {
                    runtime_session.delayed_events = 0;
                }
            }
            let buffered_bytes = invoke_stream_callback(
                &mut *runtime_session.session,
                &hook_id,
                "buffered_bytes",
                |transformer| Ok(transformer.buffered_bytes()),
            )?
            .unwrap_or(0);
            if buffered_bytes > max_buffered_bytes {
                return Err(invalid_action(
                    &hook_id,
                    EventKind::Stream,
                    format!(
                        "buffered {buffered_bytes} bytes, exceeding limit {max_buffered_bytes}"
                    ),
                ));
            }
            if runtime_session.delayed_events > max_delayed_events {
                return Err(invalid_action(
                    &hook_id,
                    EventKind::Stream,
                    format!(
                        "delayed {} events, exceeding limit {max_delayed_events}",
                        runtime_session.delayed_events
                    ),
                ));
            }
            if !was_hold && buffered_bytes == 0 {
                runtime_session.held_variants.clear();
            }
        }
        events = next;
    }
    Ok(events)
}

pub(super) fn preserve_stream_coordinates(
    source: &AiStreamDelta,
    output: &mut [AiStreamDelta],
) -> bool {
    match source {
        AiStreamDelta::TextDeltaWithMetadata {
            output_index: Some(output_index),
            content_index: Some(content_index),
            ..
        } => output.iter_mut().all(|delta| match delta {
            AiStreamDelta::TextDelta(text) => {
                *delta = AiStreamDelta::TextDeltaWithMetadata {
                    text: std::mem::take(text),
                    logprobs: Vec::new(),
                    obfuscation: None,
                    output_index: Some(*output_index),
                    content_index: Some(*content_index),
                };
                true
            }
            AiStreamDelta::TextDeltaWithMetadata {
                output_index: next_output,
                content_index: next_content,
                ..
            } => {
                if next_output.is_some_and(|next| next != *output_index)
                    || next_content.is_some_and(|next| next != *content_index)
                {
                    return false;
                }
                *next_output = Some(*output_index);
                *next_content = Some(*content_index);
                true
            }
            _ => true,
        }),
        AiStreamDelta::ThinkingDeltaWithMetadata {
            output_index: Some(output_index),
            content_index: Some(content_index),
            ..
        } => output.iter_mut().all(|delta| match delta {
            AiStreamDelta::ThinkingDelta(text) => {
                *delta = AiStreamDelta::ThinkingDeltaWithMetadata {
                    text: std::mem::take(text),
                    obfuscation: None,
                    output_index: Some(*output_index),
                    content_index: Some(*content_index),
                };
                true
            }
            AiStreamDelta::ThinkingDeltaWithMetadata {
                output_index: next_output,
                content_index: next_content,
                ..
            } => {
                if next_output.is_some_and(|next| next != *output_index)
                    || next_content.is_some_and(|next| next != *content_index)
                {
                    return false;
                }
                *next_output = Some(*output_index);
                *next_content = Some(*content_index);
                true
            }
            _ => true,
        }),
        AiStreamDelta::ReasoningSummaryDelta {
            output_index: Some(output_index),
            content_index: Some(content_index),
            ..
        } => output.iter_mut().all(|delta| match delta {
            AiStreamDelta::ThinkingDelta(text) => {
                *delta = AiStreamDelta::ReasoningSummaryDelta {
                    text: std::mem::take(text),
                    obfuscation: None,
                    output_index: Some(*output_index),
                    content_index: Some(*content_index),
                };
                true
            }
            AiStreamDelta::ReasoningSummaryDelta {
                output_index: next_output,
                content_index: next_content,
                ..
            } => {
                if next_output.is_some_and(|next| next != *output_index)
                    || next_content.is_some_and(|next| next != *content_index)
                {
                    return false;
                }
                *next_output = Some(*output_index);
                *next_content = Some(*content_index);
                true
            }
            _ => true,
        }),
        AiStreamDelta::RefusalDeltaWithIndex {
            output_index,
            content_index,
            ..
        } => output.iter_mut().all(|delta| match delta {
            AiStreamDelta::RefusalDelta(text) => {
                *delta = AiStreamDelta::RefusalDeltaWithIndex {
                    text: std::mem::take(text),
                    output_index: *output_index,
                    content_index: *content_index,
                };
                true
            }
            AiStreamDelta::RefusalDeltaWithIndex {
                output_index: next_output,
                content_index: next_content,
                ..
            } => *next_output == *output_index && *next_content == *content_index,
            _ => true,
        }),
        _ => true,
    }
}

pub(super) fn semantic_variant(delta: &AiStreamDelta) -> Option<SemanticVariant> {
    match delta {
        AiStreamDelta::TextDelta(_) | AiStreamDelta::TextDeltaWithMetadata { .. } => {
            Some(SemanticVariant::Text)
        }
        AiStreamDelta::RefusalDelta(_) | AiStreamDelta::RefusalDeltaWithIndex { .. } => {
            Some(SemanticVariant::Refusal)
        }
        AiStreamDelta::ThinkingDelta(_) | AiStreamDelta::ThinkingDeltaWithMetadata { .. } => {
            Some(SemanticVariant::Thinking)
        }
        AiStreamDelta::ReasoningSummaryDelta { .. } => Some(SemanticVariant::ReasoningSummary),
        AiStreamDelta::ToolCallDelta { index, .. } => Some(SemanticVariant::ToolCall(*index)),
        _ => None,
    }
}
