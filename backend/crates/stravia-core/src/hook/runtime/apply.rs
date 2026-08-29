use super::*;

pub(super) fn action_kind(action: &HookAction) -> &'static str {
    match action {
        HookAction::PatchRequest(_) => "patch_request",
        HookAction::PatchResponse(_) => "patch_response",
        HookAction::PatchToolResult(_) => "patch_tool_result",
        HookAction::ExposeTool(_) => "expose_tool",
        HookAction::Respond(_) => "respond",
        HookAction::Reject(_) => "reject",
        HookAction::StreamAbort { .. } => "stream_abort",
    }
}

pub(super) async fn invoke_hook(
    session: &mut dyn HookSession,
    event: HookEvent<'_>,
    hook_id: &HookId,
    cancellation: crate::proxy::context::CancellationToken,
) -> Result<ActionBatch, HookError> {
    let event_kind = event.kind();
    let started = std::time::Instant::now();
    let (result, failure_kind) = match await_with_cancellation(
        std::panic::AssertUnwindSafe(session.handle(event)).catch_unwind(),
        cancellation,
    )
    .await
    {
        Err(()) => {
            tracing::warn!(
                hook_id = %hook_id,
                event = ?event_kind,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error_kind = "cancelled",
                "hook failed"
            );
            return Err(HookError::Cancelled {
                hook_id: hook_id.clone(),
            });
        }
        Ok(Ok(result)) => (result, None),
        Ok(Err(_)) => (Err("hook panicked".into()), Some("panic")),
    };
    match &result {
        Ok(batch) => {
            let actions: Vec<&'static str> = batch.actions.iter().map(action_kind).collect();
            tracing::info!(
                hook_id = %hook_id,
                event = ?event_kind,
                elapsed_ms = started.elapsed().as_millis() as u64,
                action_count = actions.len(),
                actions = ?actions,
                "hook completed"
            );
        }
        Err(_) => tracing::warn!(
            hook_id = %hook_id,
            event = ?event_kind,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error_kind = failure_kind.unwrap_or("returned"),
            "hook failed"
        ),
    }
    result.map_err(|message| HookError::Failed {
        hook_id: hook_id.clone(),
        message,
    })
}

pub(super) async fn await_with_cancellation<F, T>(
    future: F,
    cancellation: crate::proxy::context::CancellationToken,
) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => Ok(result),
        _ = cancellation.cancelled() => Err(()),
    }
}

pub(super) fn should_skip_for_partial(
    descriptor: &HookDescriptor,
    original: &ContextSnapshot,
) -> bool {
    descriptor.requires_full_context
        && matches!(original.completeness, ContextCompleteness::Partial { .. })
}

pub(super) fn apply_response_actions(
    hook_id: &HookId,
    event: EventKind,
    response: &mut AiResponse,
    batch: ActionBatch,
) -> Result<(HookControl, bool), HookError> {
    let original = response.clone();
    let mut staged = response.clone();
    let mut control = None;
    let modified = batch
        .actions
        .iter()
        .any(|action| matches!(action, HookAction::PatchResponse(_)));

    for action in batch.actions {
        match action {
            HookAction::PatchResponse(patch) => {
                apply_response_patch(&mut staged, patch)
                    .map_err(|message| invalid_action(hook_id, event, message))?;
            }
            HookAction::Reject(rejection) if event == EventKind::UpstreamResponse => {
                set_control(hook_id, event, &mut control, HookControl::Reject(rejection))?;
            }
            HookAction::Respond(replacement) if event == EventKind::UpstreamResponse => {
                set_control(
                    hook_id,
                    event,
                    &mut control,
                    HookControl::Respond(replacement),
                )?;
            }
            HookAction::StreamAbort { message } => set_control(
                hook_id,
                event,
                &mut control,
                HookControl::StreamAbort { message },
            )?,
            HookAction::PatchRequest(_)
            | HookAction::PatchToolResult(_)
            | HookAction::ExposeTool(_)
            | HookAction::Respond(_)
            | HookAction::Reject(_) => {
                return Err(invalid_action(
                    hook_id,
                    event,
                    "action is not valid during this response stage",
                ));
            }
        }
    }
    validate_response_protected_fields(&original, &staged)
        .map_err(|message| invalid_action(hook_id, event, message))?;
    *response = staged;
    Ok((control.unwrap_or(HookControl::Continue), modified))
}

pub(super) fn apply_response_patch(
    response: &mut AiResponse,
    patch: ResponsePatch,
) -> Result<(), String> {
    match patch {
        ResponsePatch::ReplaceCanonical(next) => *response = *next,
        ResponsePatch::SetContent(content) => response.replace_output_text(content),
        ResponsePatch::SetReasoning(reasoning) => {
            let replacement = reasoning.clone().unwrap_or_default();
            let mut found = false;
            for item in &mut response.items {
                if item.set_reasoning_text(if found { "" } else { &replacement }) {
                    found = true;
                }
            }
            if !found && let Some(reasoning) = reasoning {
                response.push_reasoning(reasoning, None);
            }
        }
        ResponsePatch::ReplaceItems(items) => response.items = items,
        ResponsePatch::SetEmbeddingOutput(output) => {
            if response.embedding_output.is_none() {
                return Err("embedding output is only writable for embedding responses".into());
            }
            response.embedding_output = Some(output);
        }
        ResponsePatch::SetToolArguments { call_id, arguments } => {
            serde_json::from_str::<serde_json::Value>(&arguments)
                .map_err(|error| format!("tool arguments are not valid JSON: {error}"))?;
            let mut found = false;
            for call in response.tool_calls_mut() {
                if call.id == call_id {
                    call.arguments = arguments.clone();
                    found = true;
                }
            }
            if !found {
                return Err(format!("tool call not found: {call_id}"));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_response_protected_fields(
    original: &AiResponse,
    candidate: &AiResponse,
) -> Result<(), String> {
    if original.id != candidate.id {
        return Err("response id is read-only".into());
    }
    if original.model != candidate.model {
        return Err("response model is read-only".into());
    }
    if original.stop_reason != candidate.stop_reason {
        return Err("stop reason is read-only".into());
    }
    let original_signatures = original
        .protected_reasoning_signatures()
        .collect::<Vec<_>>();
    let candidate_signatures = candidate
        .protected_reasoning_signatures()
        .collect::<Vec<_>>();
    if original_signatures != candidate_signatures {
        return Err("reasoning signature is read-only".into());
    }
    if original.embedding_output.is_some() && candidate.embedding_output.is_none() {
        return Err("embedding output cannot be removed".into());
    }
    if serde_json::to_value(&original.usage).ok() != serde_json::to_value(&candidate.usage).ok() {
        return Err("usage is read-only".into());
    }
    if serde_json::to_value(&original.error).ok() != serde_json::to_value(&candidate.error).ok() {
        return Err("response error is read-only".into());
    }
    if tool_call_identities(original) != tool_call_identities(candidate) {
        return Err("tool call ownership, ids, and names are read-only".into());
    }
    for call in candidate.tool_calls() {
        serde_json::from_str::<serde_json::Value>(&call.arguments)
            .map_err(|error| format!("tool arguments are not valid JSON: {error}"))?;
    }
    Ok(())
}

pub(super) fn tool_call_identities(response: &AiResponse) -> Vec<(String, String)> {
    response
        .tool_calls()
        .map(|call| (call.id.clone(), call.name.clone()))
        .collect()
}

pub(super) fn apply_tool_result_actions(
    hook_id: &HookId,
    result: &mut PlatformToolResult,
    batch: ActionBatch,
) -> Result<HookControl, HookError> {
    let mut staged = result.clone();
    for action in batch.actions {
        match action {
            HookAction::PatchToolResult(ToolResultPatch::SetContent(content)) => {
                staged.content = content;
            }
            HookAction::PatchToolResult(ToolResultPatch::SetError(is_error)) => {
                staged.is_error = is_error;
            }
            HookAction::PatchToolResult(ToolResultPatch::SetMetadata(metadata)) => {
                staged.metadata = metadata;
            }
            _ => {
                return Err(invalid_action(
                    hook_id,
                    EventKind::ToolResult,
                    "action is not valid during ToolResult",
                ));
            }
        }
    }
    *result = staged;
    Ok(HookControl::Continue)
}
pub(super) fn apply_request_actions(
    hook_id: &HookId,
    request: &mut AiRequest,
    current: &mut ContextSnapshot,
    exposed_tools: &mut HashMap<String, ToolId>,
    exposed_tool_specs: &mut HashMap<String, ToolSpec>,
    registry: &PlatformToolRegistry,
    batch: ActionBatch,
) -> Result<HookControl, HookError> {
    let mut staged_request = request.clone();
    let mut staged_context = current.clone();
    let mut staged_tools = exposed_tools.clone();
    let mut staged_tool_specs = exposed_tool_specs.clone();
    let mut protected_specs: HashMap<String, ToolSpec> = staged_request
        .tools
        .iter()
        .flatten()
        .map(|tool| (tool.name.clone(), tool.clone()))
        .collect();
    protected_specs.extend(staged_tool_specs.clone());
    let mut control = None;

    for action in batch.actions {
        match action {
            HookAction::PatchRequest(patch) => {
                apply_request_patch(&mut staged_request, &mut staged_context, *patch)
                    .map_err(|message| invalid_action(hook_id, EventKind::Request, message))?;
            }
            HookAction::ExposeTool(tool_id) => {
                if staged_tools.values().any(|exposed| exposed == &tool_id) {
                    continue;
                }
                let existing_names: HashSet<String> = staged_request
                    .tools
                    .iter()
                    .flatten()
                    .map(|tool| tool.name.clone())
                    .chain(staged_tools.keys().cloned())
                    .collect();
                let exposed = registry
                    .expose(&tool_id, &existing_names)
                    .map_err(|error| {
                        invalid_action(hook_id, EventKind::Request, error.to_string())
                    })?;
                protected_specs.insert(exposed.provider_name.clone(), exposed.spec.clone());
                staged_request
                    .tools
                    .get_or_insert_with(Vec::new)
                    .push(exposed.spec.clone());
                staged_tools.insert(exposed.provider_name.clone(), tool_id);
                staged_tool_specs.insert(exposed.provider_name, exposed.spec);
            }
            HookAction::Respond(response) => set_control(
                hook_id,
                EventKind::Request,
                &mut control,
                HookControl::Respond(response),
            )?,
            HookAction::Reject(rejection) => set_control(
                hook_id,
                EventKind::Request,
                &mut control,
                HookControl::Reject(rejection),
            )?,
            HookAction::StreamAbort { .. } => {
                return Err(invalid_action(
                    hook_id,
                    EventKind::Request,
                    "StreamAbort is not valid during Request",
                ));
            }
            HookAction::PatchResponse(_) | HookAction::PatchToolResult(_) => {
                return Err(invalid_action(
                    hook_id,
                    EventKind::Request,
                    "response and tool-result patches are not valid during Request",
                ));
            }
        }
    }

    *request = staged_request;
    *current = staged_context;
    *exposed_tools = staged_tools;
    *exposed_tool_specs = staged_tool_specs;
    Ok(control.unwrap_or(HookControl::Continue))
}

pub(super) fn apply_request_patch(
    request: &mut AiRequest,
    current: &mut ContextSnapshot,
    patch: RequestPatch,
) -> Result<(), String> {
    match patch {
        RequestPatch::ReplaceCanonical(next) => {
            if next.stream.enabled != request.stream.enabled
                || next.stream.include_usage != request.stream.include_usage
            {
                return Err("a hook cannot change the negotiated StreamConfig".into());
            }
            if !request_metadata_patch_allowed(&request.meta, &next.meta) {
                return Err("request metadata is read-only".into());
            }
            let protected_metadata = request.meta.clone();
            let media_routing = next.meta.media_routing.clone();
            *request = *next;
            request.meta = protected_metadata;
            request.meta.media_routing = media_routing;
            let completeness = ContextCompleteness::from_request(request);
            current.update_from_request(request, completeness);
        }
        RequestPatch::SetModel(model) => {
            if model.trim().is_empty() {
                return Err("model cannot be empty".into());
            }
            request.model = model;
        }
        RequestPatch::SetSystem(system) => {
            request.instructions = system.clone();
            current.system = system;
        }
        RequestPatch::SetGeneration(generation) => request.generation = generation,
        RequestPatch::ReplaceTools(tools) => request.tools = tools,
        RequestPatch::SetToolChoice(tool_choice) => request.tool_choice = tool_choice,
        RequestPatch::SetEmbeddingInput(input) => {
            let Some(embedding) = request.embedding.as_mut() else {
                return Err("embedding input is only writable for embedding requests".into());
            };
            embedding.input = input;
        }
        RequestPatch::SetProtocolExtension(extension) => {
            request.ext = extension.map(|extension| *extension);
            current.completeness = ContextCompleteness::from_request(request);
        }
        RequestPatch::ReplaceContextSpans(rewrites) => {
            current
                .apply_rewrites(&rewrites)
                .map_err(|error| error.to_string())?;
            current.write_to_request(request);
        }
    }
    Ok(())
}
pub(super) fn request_metadata_patch_allowed(
    left: &crate::protocol::ir::RequestMetadata,
    right: &crate::protocol::ir::RequestMetadata,
) -> bool {
    left.source_protocol == right.source_protocol
        && (right.raw.is_none() || request_metadata_raw_equal(left, right))
        && sanitized_vendor_map_allowed(&left.vendor.ingress, &right.vendor.ingress)
        && sanitized_vendor_map_allowed(
            &left.vendor.passthrough_safe,
            &right.vendor.passthrough_safe,
        )
        && (right.vendor.egress.is_empty() || left.vendor.egress == right.vendor.egress)
}

pub(super) fn sanitized_vendor_map_allowed(
    original: &std::collections::HashMap<String, serde_json::Value>,
    candidate: &std::collections::HashMap<String, serde_json::Value>,
) -> bool {
    for (key, value) in candidate {
        if crate::hook::is_secret_key(key) {
            return false;
        }
        let Some(original_value) = original.get(key) else {
            return false;
        };
        if !sanitized_vendor_value_allowed(original_value, value) {
            return false;
        }
    }
    original
        .keys()
        .all(|key| crate::hook::is_secret_key(key) || candidate.contains_key(key))
}

pub(super) fn sanitized_vendor_value_allowed(
    original: &serde_json::Value,
    candidate: &serde_json::Value,
) -> bool {
    match (original, candidate) {
        (serde_json::Value::Object(original), serde_json::Value::Object(candidate)) => {
            for (key, value) in candidate {
                if crate::hook::is_secret_key(key) {
                    return false;
                }
                let Some(original_value) = original.get(key) else {
                    return false;
                };
                if !sanitized_vendor_value_allowed(original_value, value) {
                    return false;
                }
            }
            original
                .keys()
                .all(|key| crate::hook::is_secret_key(key) || candidate.contains_key(key))
        }
        (serde_json::Value::Array(original), serde_json::Value::Array(candidate)) => {
            original.len() == candidate.len()
                && original
                    .iter()
                    .zip(candidate)
                    .all(|(left, right)| sanitized_vendor_value_allowed(left, right))
        }
        _ => original == candidate,
    }
}

pub(super) fn request_metadata_raw_equal(
    left: &crate::protocol::ir::RequestMetadata,
    right: &crate::protocol::ir::RequestMetadata,
) -> bool {
    match (&left.raw, &right.raw) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.body == right.body
                && left.headers == right.headers
                && left.method == right.method
                && left.path == right.path
        }
        _ => false,
    }
}

pub(super) fn redact_vendor_map(
    values: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, serde_json::Value> {
    values
        .iter()
        .filter(|(key, _)| !crate::hook::is_secret_key(key))
        .map(|(key, value)| (key.clone(), crate::hook::redact_vendor_value(value)))
        .collect()
}

pub(super) fn hook_request_view(request: &AiRequest) -> AiRequest {
    let mut view = request.clone();
    view.meta.raw = None;
    view.meta.vendor.egress.clear();
    view.meta.vendor.ingress = redact_vendor_map(&view.meta.vendor.ingress);
    view.meta.vendor.passthrough_safe = redact_vendor_map(&view.meta.vendor.passthrough_safe);
    view
}

pub(super) fn set_control(
    hook_id: &HookId,
    event: EventKind,
    current: &mut Option<HookControl>,
    next: HookControl,
) -> Result<(), HookError> {
    if current.is_some() {
        return Err(invalid_action(
            hook_id,
            event,
            "an action batch can contain only one control action",
        ));
    }
    *current = Some(next);
    Ok(())
}

pub(super) fn invalid_action(
    hook_id: &HookId,
    event: EventKind,
    message: impl Into<String>,
) -> HookError {
    HookError::InvalidAction {
        hook_id: hook_id.clone(),
        event,
        message: message.into(),
    }
}
