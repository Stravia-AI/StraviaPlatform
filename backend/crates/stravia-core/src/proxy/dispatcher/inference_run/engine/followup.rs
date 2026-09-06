use super::*;

pub(super) enum FollowupModelTurn {
    Turn(crate::agent::ModelTurn, Instant),
    HookResponse {
        response: AiResponse,
        pending_generation_chain: Option<crate::generation_chain::GenerationChainWrite>,
    },
    StreamError(crate::protocol::ir::AiError),
}

fn hook_stream_error(control: crate::hook::HookControl) -> crate::protocol::ir::AiError {
    use crate::protocol::ir::{AiError, AiErrorKind};

    match control {
        crate::hook::HookControl::Reject(rejection) => {
            let kind = match rejection.status {
                401 => AiErrorKind::AuthenticationError,
                403 => AiErrorKind::AuthorizationError,
                400..=499 => AiErrorKind::InvalidRequest,
                _ => AiErrorKind::ServerError,
            };
            AiError::new(kind, rejection.message)
                .with_status(rejection.status)
                .with_raw(serde_json::json!({"code": rejection.code}))
        }
        crate::hook::HookControl::StreamAbort { message } => {
            AiError::new(AiErrorKind::StreamMidError, message)
        }
        crate::hook::HookControl::Continue | crate::hook::HookControl::Respond(_) => {
            AiError::new(AiErrorKind::Unknown, "invalid hidden-round Hook control")
        }
    }
}

pub(super) async fn acquire_followup_model_turn(
    executor: &dyn ModelTurnExecutor,
    gateway: &Gateway,
    headers: &HeaderMap,
    request: &mut AiRequest,
    ingress: ProtocolId,
    request_context: &RequestContext,
    inference_run: &mut crate::hook::InferenceRun,
    projection: &mut ClientProjectionSession,
    phase: &mut PhaseTracker,
    principal: &crate::hook::Principal,
    generation: &GenerationChainRun,
    fixed_media_plan: Option<&crate::protocol::ir::request::MediaRoutingPlan>,
    start: Instant,
    request_extras: &RequestExtras,
) -> Result<FollowupModelTurn, RoundOutcome> {
    if request_context.cancellation.is_cancelled() {
        return Err(buffered_response(error_response(499, "request cancelled")));
    }
    match inference_run.on_request(request).await {
        Ok(crate::hook::HookControl::Continue) => {}
        Ok(crate::hook::HookControl::Respond(response)) => {
            let mut response = *response;
            inference_run.set_route(crate::hook::RouteContext {
                model_id: request.model.clone(),
                provider_id: "hook".into(),
                target_id: "hook".into(),
                egress: ingress,
            });
            match inference_run.on_client_output(&mut response).await {
                Ok(crate::hook::HookControl::Continue) => {}
                Ok(crate::hook::HookControl::Respond(replacement)) => {
                    response = *replacement;
                }
                Ok(control) => {
                    return Ok(FollowupModelTurn::StreamError(hook_stream_error(control)));
                }
                Err(error) => {
                    return Ok(FollowupModelTurn::StreamError(
                        crate::protocol::ir::AiError::new(
                            crate::protocol::ir::AiErrorKind::StreamMidError,
                            error.to_string(),
                        ),
                    ));
                }
            }
            if ingress == crate::protocol::ids::OPEN_RESPONSES_2026_04_24
                && let Some(write) = generation.write.as_ref()
            {
                response.id = write.id().to_owned();
            }
            projection.begin_model_leg(
                super::thinking_carrier_facts(ingress, ingress, false),
                inference_run.exposed_tool_names(),
            );
            match projection.project_staged(&mut response, &[]).await {
                Ok(_) => {}
                Err(error) => {
                    return Ok(FollowupModelTurn::StreamError(
                        crate::protocol::ir::AiError::new(
                            crate::protocol::ir::AiErrorKind::StreamMidError,
                            error.to_string(),
                        ),
                    ));
                }
            }
            let pending_generation_chain = generation.write.clone().and_then(|mut write| {
                write.observe_effective(request.clone());
                crate::generation_chain::mark_generation_target(
                    &mut response,
                    "hook",
                    ingress,
                    &request.model,
                    "",
                );
                let mut staged_response = response.clone();
                apply_hidden_rounds(request_context, &mut staged_response);
                response.usage = staged_response.usage.clone();
                let staged = write.stage(&mut staged_response, None);
                response.vendor = staged_response.vendor;
                staged.then_some(write)
            });
            if let Err(error) = phase.transition(Phase::SemanticComplete) {
                return Ok(FollowupModelTurn::StreamError(
                    crate::protocol::ir::AiError::new(
                        crate::protocol::ir::AiErrorKind::StreamMidError,
                        error,
                    ),
                ));
            }
            if let Err(error) = phase.transition(Phase::AwaitingDelivery) {
                return Ok(FollowupModelTurn::StreamError(
                    crate::protocol::ir::AiError::new(
                        crate::protocol::ir::AiErrorKind::StreamMidError,
                        error,
                    ),
                ));
            }
            return Ok(FollowupModelTurn::HookResponse {
                response,
                pending_generation_chain,
            });
        }
        Ok(control) => return Ok(FollowupModelTurn::StreamError(hook_stream_error(control))),
        Err(error) => {
            return Ok(FollowupModelTurn::StreamError(
                crate::protocol::ir::AiError::new(
                    crate::protocol::ir::AiErrorKind::StreamMidError,
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
            crate::protocol::ir::AiError::new(
                crate::protocol::ir::AiErrorKind::StreamMidError,
                "Media bridge could not prepare the hidden request",
            ),
        ));
    }
    enter_phase(phase, Phase::Selecting).map_err(|response| buffered_response(*response))?;
    let (turn, effective_request, turn_started) = acquire_turn(
        executor,
        gateway,
        headers,
        request,
        ingress,
        request_context,
        inference_run,
        principal,
        start,
        request_extras,
    )
    .await?;
    *request = effective_request;
    inference_run.set_route(turn.route.clone());
    enter_phase(phase, Phase::Calling).map_err(|response| buffered_response(*response))?;
    Ok(FollowupModelTurn::Turn(turn, turn_started))
}
