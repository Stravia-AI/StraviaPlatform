use super::*;

pub(super) enum FollowupModelTurn {
    Turn(Box<crate::agent::ModelTurn>),
    HookResponse(Box<HookResponsePlan>),
    StreamError(stravia_runtime_contract::protocol::ir::AiError),
}

/// Delivery plan for a Hook-produced response: prepared through
/// `Phase::AwaitingDelivery`. The caller owns send and settle.
pub(super) struct HookResponsePlan {
    pub response: AiResponse,
    pub staged_delivery: ProjectedDeltaBatch,
    pub pending_generation_chain: Option<Box<crate::generation_chain::GenerationChainWrite>>,
}

/// Why Hook response preparation stopped. Each caller renders it on its own
/// channel: the first leg produces a wire `Response`, a hidden leg surfaces an
/// `AiError`.
pub(super) enum HookRespondError {
    /// Client-output hooks returned a control other than Continue/Respond.
    Control(Box<stravia_runtime_contract::hook::HookControl>),
    /// Hook runtime, projection, or phase failure.
    Failure(String),
}

/// Run state borrowed while preparing a Hook-produced response.
pub(super) struct HookRespondParts<'a> {
    pub request: &'a AiRequest,
    pub ingress: ProtocolId,
    pub inference_run: &'a mut crate::hook::InferenceRun,
    pub projection: &'a mut ClientProjectionSession,
    pub generation: &'a mut GenerationChainRun,
    pub ledger: &'a RunLedger,
    pub phase: &'a mut PhaseTracker,
}

/// Shared Hook response leg preparation: hook routing, client-output hooks,
/// response-id stamping, Model Leg projection, and Generation Chain staging.
pub(super) async fn prepare_hook_response(
    mut response: AiResponse,
    parts: HookRespondParts<'_>,
) -> Result<HookResponsePlan, HookRespondError> {
    let HookRespondParts {
        request,
        ingress,
        inference_run,
        projection,
        generation,
        ledger,
        phase,
    } = parts;
    inference_run.set_route(stravia_runtime_contract::hook::RouteContext {
        model_id: request.model.clone(),
        provider_id: "hook".into(),
        target_id: "hook".into(),
        egress: ingress,
    });
    match inference_run.on_client_output(&mut response).await {
        Ok(stravia_runtime_contract::hook::HookControl::Continue) => {}
        Ok(stravia_runtime_contract::hook::HookControl::Respond(replacement)) => {
            response = *replacement;
        }
        Ok(control) => return Err(HookRespondError::Control(Box::new(control))),
        Err(error) => return Err(HookRespondError::Failure(error.to_string())),
    }
    if ingress == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
        && let Some(write) = generation.write.as_ref()
    {
        response.id = write.id().to_owned();
    }
    projection.begin_model_leg(
        thinking_carrier_facts(ingress, ingress),
        inference_run.exposed_tool_names(),
        None,
    );
    let staged_delivery = projection
        .project_staged(&mut response, &[])
        .await
        .map_err(|error| HookRespondError::Failure(error.to_string()))?;
    let pending_generation_chain = generation.write.take().and_then(|mut write| {
        write.observe_effective(request.clone());
        let mut staged_response = response.clone();
        ledger.apply_hidden_rounds(&mut staged_response);
        response.usage = staged_response.usage.clone();
        let staged = write.stage(
            &mut staged_response,
            &crate::generation_chain::GenerationSource::Hook { protocol: ingress },
            None,
        );
        response.vendor = staged_response.vendor;
        staged.then_some(write).map(Box::new)
    });
    for next in [Phase::SemanticComplete, Phase::AwaitingDelivery] {
        phase.transition(next).map_err(HookRespondError::Failure)?;
    }
    Ok(HookResponsePlan {
        response,
        staged_delivery,
        pending_generation_chain,
    })
}

pub(super) struct FollowupLeg<'a> {
    pub executor: &'a dyn ModelTurnExecutor,
    pub headers: &'a HeaderMap,
    pub request: &'a mut AiRequest,
    pub ingress: ProtocolId,
    pub request_context: &'a RequestContext,
    pub ledger: &'a RunLedger,
    pub inference_run: &'a mut crate::hook::InferenceRun,
    pub projection: &'a mut ClientProjectionSession,
    pub phase: &'a mut PhaseTracker,
    pub generation: &'a mut GenerationChainRun,
    pub fixed_media_plan:
        Option<&'a stravia_runtime_contract::protocol::ir::request::MediaRoutingPlan>,
}

fn hook_stream_error(
    control: stravia_runtime_contract::hook::HookControl,
) -> stravia_runtime_contract::protocol::ir::AiError {
    use stravia_runtime_contract::protocol::ir::AiError;
    use stravia_runtime_contract::protocol::ir::AiErrorKind;

    match control {
        stravia_runtime_contract::hook::HookControl::Reject(rejection) => {
            let kind = match rejection.status {
                401 => AiErrorKind::AuthenticationError,
                403 => AiErrorKind::AuthorizationError,
                400..=499 => AiErrorKind::InvalidRequest,
                _ => AiErrorKind::Unknown,
            };
            AiError::new(kind, rejection.message)
                .with_status(rejection.status)
                .with_raw(serde_json::json!({"code": rejection.code}))
        }
        stravia_runtime_contract::hook::HookControl::StreamAbort { message } => {
            AiError::new(AiErrorKind::Unknown, message)
        }
        stravia_runtime_contract::hook::HookControl::Continue
        | stravia_runtime_contract::hook::HookControl::Respond(_) => {
            AiError::new(AiErrorKind::Unknown, "invalid hidden-round Hook control")
        }
    }
}

pub(super) async fn acquire_followup_model_turn(
    leg: FollowupLeg<'_>,
) -> Result<FollowupModelTurn, RoundOutcome> {
    let FollowupLeg {
        executor,
        headers,
        request,
        ingress,
        request_context,
        ledger,
        inference_run,
        projection,
        phase,
        generation,
        fixed_media_plan,
    } = leg;
    if request_context.cancellation.is_cancelled() {
        return Err(buffered_response(error_response(499, "request cancelled")));
    }
    match inference_run.on_request(request).await {
        Ok(stravia_runtime_contract::hook::HookControl::Continue) => {}
        Ok(stravia_runtime_contract::hook::HookControl::Respond(response)) => {
            let plan = prepare_hook_response(
                *response,
                HookRespondParts {
                    request,
                    ingress,
                    inference_run,
                    projection,
                    generation,
                    ledger,
                    phase,
                },
            )
            .await;
            return Ok(match plan {
                Ok(plan) => FollowupModelTurn::HookResponse(Box::new(plan)),
                Err(HookRespondError::Control(control)) => {
                    FollowupModelTurn::StreamError(hook_stream_error(*control))
                }
                Err(HookRespondError::Failure(message)) => FollowupModelTurn::StreamError(
                    stravia_runtime_contract::protocol::ir::AiError::new(
                        stravia_runtime_contract::protocol::ir::AiErrorKind::Unknown,
                        message,
                    ),
                ),
            });
        }
        Ok(control) => return Ok(FollowupModelTurn::StreamError(hook_stream_error(control))),
        Err(error) => {
            return Ok(FollowupModelTurn::StreamError(
                stravia_runtime_contract::protocol::ir::AiError::new(
                    stravia_runtime_contract::protocol::ir::AiErrorKind::Unknown,
                    error.to_string(),
                ),
            ));
        }
    }
    if let Some(plan) = fixed_media_plan {
        request.meta.media_routing = Some(plan.clone());
    }
    if !stabilize_media_generation_chain(generation, request) {
        return Ok(FollowupModelTurn::StreamError(
            stravia_runtime_contract::protocol::ir::AiError::new(
                stravia_runtime_contract::protocol::ir::AiErrorKind::Unknown,
                "Media bridge could not prepare the hidden request",
            ),
        ));
    }
    enter_phase(phase, Phase::Selecting).map_err(|response| buffered_response(*response))?;
    let (turn, effective_request) = acquire_turn(
        executor,
        headers,
        request,
        request_context,
        ledger,
        inference_run,
        generation,
    )
    .await?;
    *request = effective_request;
    inference_run.set_route(turn.route.clone());
    enter_phase(phase, Phase::Calling).map_err(|response| buffered_response(*response))?;
    Ok(FollowupModelTurn::Turn(Box::new(turn)))
}
