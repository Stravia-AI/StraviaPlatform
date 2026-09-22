//! Dispatcher: single orchestration point that drives a request through the
//! full proxy pipeline.
//!
//! `orchestrate` owns the internal Inference Run lifecycle. Ingress thin shells
//! call `dispatcher::dispatch_pipeline`, which submits normalized `RunInput`.
//!
//! Pipeline:
//!   1. Authenticate the caller and begin Generation Chain Write.
//!   2. Run Request hooks before resolving and authorizing the final model.
//!   3. Execute one shared Model Turn after hooks stabilize the effective request.
//!   4. Run response/tool/client-output hooks and deliver the committed result.

mod claim;
mod completion;
mod delivery;
mod errors;
mod followup;
mod ledger;
mod leg;
mod projection;
mod settlement;
mod stream;
mod util;
use self::claim::*;
use self::completion::*;
use self::delivery::{
    BufferedDeliveryProgress, DeliveryAdapter, DeliveryProgress, after_body_delivery,
};
use self::errors::*;
pub(super) use self::errors::{error_response, hook_failure_response};
use self::followup::{
    FollowupLeg, FollowupModelTurn, HookRespondError, HookRespondParts, HookResponsePlan,
    acquire_followup_model_turn, prepare_hook_response,
};
pub(super) use self::ledger::RunLedger;
use self::leg::*;
use self::projection::*;
use self::settlement::{
    PendingGenerationChainWrite, Settlement, report_projected_delivery, settle,
};
use self::util::{client_session_id, forwarded_client_headers};
use super::{Phase, PhaseTracker, RunInput};
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::Gateway;
use crate::agent::ModelTurn;
use crate::agent::ModelTurnExecutor;
use crate::agent::TurnInput;
use crate::error::{AccessDenial, AuthFailure, GatewayError};
use crate::interaction_observation::{AdmissionFacts, IngressObserver, RunEvent, RunStart};
use crate::model_turn::support::ai_response_to_deltas as canonical_ai_response_to_deltas;
use crate::proxy::context::RequestContext;
use crate::proxy::security::{ClientCredential, Security};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_runtime_contract::model_turn::CanonicalEvent;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::request::MediaRoutingMode;

/// Terminal result of the shared Model Turn path: the wire response plus how
/// the body was delivered. Hidden Model Legs continue inside
/// `execute_shared_model_turn`, so the outcome always reaches the caller as a
/// finished response.
pub(super) struct RoundOutcome {
    pub response: Box<Response>,
    pub delivery: DeliveryState,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryState {
    Buffered,
    Live,
}

pub(super) fn buffered_response(response: Response) -> RoundOutcome {
    RoundOutcome {
        response: Box::new(response),
        delivery: DeliveryState::Buffered,
    }
}

pub(super) fn buffered_completion(response: Response) -> RoundOutcome {
    RoundOutcome {
        response: Box::new(response),
        delivery: DeliveryState::Buffered,
    }
}

pub(super) fn live_response(response: Response) -> RoundOutcome {
    RoundOutcome {
        response: Box::new(response),
        delivery: DeliveryState::Live,
    }
}

fn reject_before_admission(
    observer: &mut Option<IngressObserver>,
    stage: &str,
    code: &str,
    response: Response,
) -> Response {
    if let Some(observer) = observer.take() {
        return crate::proxy::ingress::observation::reject(observer, stage, code, response);
    }
    response
}

trait ObservationRecorder {
    fn record_event(&self, event: RunEvent);
}

impl ObservationRecorder for IngressObserver {
    fn record_event(&self, event: RunEvent) {
        self.record(event);
    }
}

impl ObservationRecorder for crate::interaction_observation::RunObserver {
    fn record_event(&self, event: RunEvent) {
        self.record(event);
    }
}

fn checkpoint_payload<R: ObservationRecorder, T: serde::Serialize + ?Sized>(
    recorder: &R,
    value: &T,
) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_else(|error| {
        recorder.record_event(RunEvent::ObservationGap {
            reason: format!("checkpoint_serialization: {error}"),
        });
        serde_json::json!({ "unavailable": "checkpoint_serialization" })
    })
}

fn ai_response_to_deltas(
    response: &AiResponse,
) -> Vec<stravia_runtime_contract::protocol::ir::AiStreamDelta> {
    use stravia_runtime_contract::protocol::ir::AiStreamDelta;

    let mut deltas = canonical_ai_response_to_deltas(response);
    let mut response_profile = serde_json::Map::new();
    for key in [
        "__open_responses_effective_request",
        "__open_responses_response_profile",
    ] {
        if let Some(profile) = response
            .vendor
            .ingress
            .get(key)
            .and_then(serde_json::Value::as_object)
        {
            response_profile.extend(profile.clone());
        }
    }
    if !response_profile.is_empty() {
        deltas.insert(
            0,
            AiStreamDelta::ResponseMetadata {
                metadata: serde_json::Value::Object(response_profile),
            },
        );
    }
    let usage_index = deltas
        .iter()
        .position(|delta| matches!(delta, AiStreamDelta::Usage(_)))
        .unwrap_or_else(|| deltas.len().saturating_sub(1));
    if let Some(metadata) = response.vendor.ingress.get("__google_response_metadata") {
        deltas.insert(
            usage_index,
            AiStreamDelta::Unknown {
                raw: serde_json::json!({"__google_response_metadata": metadata}).to_string(),
            },
        );
    }
    if let Some(terminal) = response.vendor.egress.get("__open_responses_terminal")
        && let Some(status) = terminal.get("status").and_then(serde_json::Value::as_str)
    {
        let done_index = deltas.len().saturating_sub(1);
        deltas.insert(
            done_index,
            AiStreamDelta::ResponseTerminal {
                status: status.to_owned(),
                incomplete_details: terminal
                    .get("incomplete_details")
                    .filter(|value| !value.is_null())
                    .cloned(),
            },
        );
    }
    deltas
}

fn visible_delta_text(
    delta: &stravia_runtime_contract::protocol::ir::AiStreamDelta,
) -> Option<&str> {
    match delta {
        stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDelta(text)
        | stravia_runtime_contract::protocol::ir::AiStreamDelta::TextDeltaWithMetadata {
            text,
            ..
        }
        | stravia_runtime_contract::protocol::ir::AiStreamDelta::RefusalDelta(text)
        | stravia_runtime_contract::protocol::ir::AiStreamDelta::RefusalDeltaWithIndex {
            text,
            ..
        } => Some(text),
        _ => None,
    }
}

fn enter_phase(phase: &mut PhaseTracker, next: Phase) -> Result<(), Box<Response>> {
    phase
        .transition(next)
        .map_err(|error| Box::new(hook_failure_response(error)))
}

fn thinking_carrier_facts(
    ingress: ProtocolId,
    egress: Option<ProtocolId>,
) -> stravia_protocol_codec::transform::ThinkingCarrierFacts {
    match egress {
        Some(egress) => stravia_protocol_codec::transform::ProtocolTransform::global()
            .bind(ingress, egress)
            .expect("Inference Run uses a registered protocol pair")
            .thinking_carrier_facts(),
        None => stravia_protocol_codec::transform::ThinkingCarrierFacts {
            indexed: true,
            may_be_protected: true,
            stream_unprotected_summaries: false,
        },
    }
}

/// Materialized Generation Chain state owned by the Inference Run while the
/// Model Turn Executor prepares only the selected target's continuation.
#[derive(Clone)]
pub(super) struct GenerationChainRun {
    principal: stravia_runtime_contract::Principal,
    write: Option<crate::generation_chain::GenerationChainWrite>,
    client_request: AiRequest,
    previous_response_id: Option<String>,
    compaction_source_generation_id: Option<String>,
    vendor_publications: Vec<crate::plugin::VendorPublicationFence>,
}

struct DispatchContext<'a> {
    gw: Gateway,
    executor: Arc<dyn ModelTurnExecutor>,
    headers: HeaderMap,
    request: &'a mut AiRequest,
    ingress: ProtocolId,
    ctx: &'a mut RequestContext,
    inference_run: &'a mut Option<crate::hook::InferenceRun>,
    phase: &'a mut PhaseTracker,
    generation: &'a mut GenerationChainRun,
    projection: &'a mut Option<ClientProjectionSession>,
    ledger: &'a RunLedger,
}

struct SharedModelTurnInput<'a> {
    executor: Arc<dyn ModelTurnExecutor>,
    gateway: &'a Gateway,
    request: &'a mut AiRequest,
    ingress: ProtocolId,
    request_context: &'a RequestContext,
    inference_run: &'a mut Option<crate::hook::InferenceRun>,
    phase: &'a mut PhaseTracker,
    generation: GenerationChainRun,
    headers: &'a HeaderMap,
    projection: &'a mut Option<ClientProjectionSession>,
    ledger: &'a RunLedger,
}

fn stabilize_media_generation_chain(
    generation: &GenerationChainRun,
    rewritten: &AiRequest,
) -> bool {
    let Some(plan) = rewritten.meta.media_routing.as_ref() else {
        return true;
    };
    if plan.mode != MediaRoutingMode::Bridge {
        return true;
    }
    let client_delta = generation
        .write
        .as_ref()
        .map_or(&generation.client_request, |write| write.request_delta());
    let image_count = client_delta
        .items
        .iter()
        .filter_map(|message| match &message.content {
            stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter(|block| {
            matches!(
                block,
                stravia_runtime_contract::protocol::ir::ContentBlock::Image { .. }
            )
        })
        .count();
    if image_count == 0 {
        return true;
    }
    if image_count != plan.source_artifact_ids.len() {
        return false;
    }
    plan.source_artifact_ids.iter().all(|source_id| {
        let identity = format!("[sm:sa:{source_id} ");
        rewritten
            .items
            .iter()
            .filter_map(|message| match &message.content {
                stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) => {
                    Some(blocks)
                }
                _ => None,
            })
            .flatten()
            .any(|block| {
                matches!(
                    block,
                    stravia_runtime_contract::protocol::ir::ContentBlock::Text { text, .. }
                        if text.starts_with(&identity)
                )
            })
    })
}

pub(super) async fn orchestrate(
    input: RunInput,
    inference_run: &mut Option<crate::hook::InferenceRun>,
    phase: &mut PhaseTracker,
) -> Response {
    let RunInput {
        gateway: gw,
        executor,
        headers,
        envelope: _,
        request,
        ingress,
        context: mut ctx,
    } = input;
    let mut ingress_observer = ctx
        .extensions
        .take::<IngressObserver>()
        .expect("Inference Run ingress observer");
    ingress_observer.set_model(&request.model);
    ingress_observer.record_debug(|| RunEvent::Content {
        stage: "decoded_request".into(),
        model_turn_id: None,
        attempt_id: None,
        payload: request.debug_value().unwrap_or_else(|_| {
            ingress_observer.record(RunEvent::ObservationGap {
                reason: "decoded_request_serialization".into(),
            });
            serde_json::json!({ "unavailable": "decoded_request_serialization" })
        }),
    });
    let mut request = request;
    if let Some(session_id) = client_session_id(&headers, &request) {
        crate::generation_chain::set_generation_session_id(&mut request, session_id);
    }
    let mut client_request = request.clone();
    let ingress_capabilities = stravia_protocol_codec::registry::ProtocolRegistry::global()
        .capabilities(&ingress)
        .expect("registered ingress protocol");
    let request_kind = if ingress_capabilities.embeddings {
        stravia_runtime_contract::hook::RequestKind::Embeddings
    } else {
        stravia_runtime_contract::hook::RequestKind::Generation
    };
    if let Some(stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(extension)) =
        request.ext.as_ref()
        && extension.background == Some(true)
    {
        let response = parameter_error_response(
            StatusCode::BAD_REQUEST,
            "unsupported_feature",
            "background",
            "Responses background mode is not supported.",
        );
        return reject_before_admission(
            &mut Some(ingress_observer),
            "protocol",
            "unsupported_feature",
            response,
        );
    }
    let previous_response_id = match request.ext.as_ref() {
        Some(stravia_runtime_contract::protocol::ir::ProtocolExt::OpenResponses(extension)) => {
            extension.previous_response_id.clone()
        }
        _ => None,
    };
    let credential = ClientCredential::from_inference_headers(&headers);
    let authenticated_principal = match Security::new(gw.storage.auth())
        .authenticated_principal(&credential)
        .await
    {
        Ok(principal) => principal,
        Err(error) => {
            let response = inference_access_error_response(error);
            return reject_before_admission(
                &mut Some(ingress_observer),
                "authentication",
                "unauthorized",
                response,
            );
        }
    };
    let concurrency_limit = authenticated_principal.concurrency_limit;
    let api_key_name = authenticated_principal.api_key_name;
    let principal = authenticated_principal.principal;
    ingress_observer.set_authenticated_source(principal.api_key_id(), &api_key_name);
    if let Err(error) =
        crate::media::ingest::normalize_request(&gw, &principal, &mut request, &ctx.cancellation)
            .await
    {
        return reject_before_admission(
            &mut Some(ingress_observer),
            "attachments",
            "attachment_ingest_failed",
            coded_error_response(
                StatusCode::BAD_REQUEST,
                "attachment_ingest_failed",
                &error.to_string(),
            ),
        );
    }
    client_request.clone_from(&request);
    ingress_observer.record_debug(|| RunEvent::Content {
        stage: "artifact_normalized_request".into(),
        model_turn_id: None,
        attempt_id: None,
        payload: checkpoint_payload(&ingress_observer, &request),
    });
    ctx.auth_subject = Some(crate::proxy::context::AuthSubject {
        api_key_id: Some(principal.api_key_id().to_owned()),
        label: Some(api_key_name.clone()),
    });
    let compact = ctx.extensions.get::<crate::model_turn::ModelTurnPurpose>()
        == Some(crate::model_turn::ModelTurnPurpose::Compact);
    let generation_chain_write = if !compact
        && matches!(
            request_kind,
            stravia_runtime_contract::hook::RequestKind::Generation
        ) {
        let native_compaction_requested =
            stravia_protocol_codec::codec::compaction::native_compaction_requested(&request);
        let begin = tokio::select! {
            begin = async {
                if native_compaction_requested {
                    gw.generation_chains
                        .begin_native_compaction(principal.clone(), request)
                        .await
                } else {
                    gw.generation_chains.begin(principal.clone(), request).await
                }
            } => begin,
            _ = ctx.cancellation.cancelled() => {
                let deadline = ctx.deadline.is_exceeded();
                let code = if deadline { "deadline_exceeded" } else { "cancelled" };
                let response = if deadline {
                    error_response(504, "request deadline exceeded")
                } else {
                    error_response(499, "request cancelled")
                };
                return reject_before_admission(
                    &mut Some(ingress_observer),
                    "generation_chain",
                    code,
                    response,
                );
            }
        };
        match begin {
            Ok(mut write) => {
                if crate::generation_chain::generation_session_fingerprint(write.request())
                    .is_none()
                {
                    let root_id = write.root_id().to_owned();
                    crate::generation_chain::set_generation_session_id(
                        write.request_mut(),
                        root_id,
                    );
                }
                request = write.request().clone();
                Some(write)
            }
            Err(error) => {
                let code = error.to_string();
                let response = coded_error_response(StatusCode::BAD_REQUEST, &code, &code);
                return reject_before_admission(
                    &mut Some(ingress_observer),
                    "protocol",
                    &code,
                    response,
                );
            }
        }
    } else {
        None
    };
    let (compact_parent_id, compact_root_id, compact_has_new_user, compact_pending_tool_result) =
        if compact {
            let prepare = tokio::select! {
                prepare = gw
                    .generation_chains
                    .prepare_compaction(principal.clone(), request) => prepare,
                _ = ctx.cancellation.cancelled() => {
                    let deadline = ctx.deadline.is_exceeded();
                    let code = if deadline { "deadline_exceeded" } else { "cancelled" };
                    let response = if deadline {
                        error_response(504, "request deadline exceeded")
                    } else {
                        error_response(499, "request cancelled")
                    };
                    return reject_before_admission(
                        &mut Some(ingress_observer),
                        "generation_chain",
                        code,
                        response,
                    );
                }
            };
            let prepared = match prepare {
                Ok(prepared) => prepared,
                Err(error) => {
                    let code = error.to_string();
                    return reject_before_admission(
                        &mut Some(ingress_observer),
                        "protocol",
                        &code,
                        coded_error_response(StatusCode::BAD_REQUEST, &code, &code),
                    );
                }
            };
            request = prepared.request;
            (
                prepared.parent_id,
                prepared.root_id,
                Some(prepared.has_new_user),
                prepared.has_matching_pending_tool_result,
            )
        } else {
            (None, None, None, false)
        };
    let compaction_source_generation_id = if compact && compact_parent_id.is_some() {
        compact_parent_id.clone()
    } else if !compact
        && generation_chain_write
            .as_ref()
            .and_then(|write| write.parent_id())
            .is_some()
    {
        generation_chain_write
            .as_ref()
            .and_then(|write| write.parent_id())
            .map(str::to_owned)
    } else if compact
        || (ingress == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
            && client_request.items.iter().any(|item| {
                item.role == stravia_runtime_contract::protocol::ir::Role::Assistant
                    || item.is_compaction()
            }))
    {
        match gw
            .generation_chains
            .compaction_source(&principal, &client_request)
            .await
        {
            Ok(source) => source,
            Err(error) => {
                let code = error.to_string();
                return reject_before_admission(
                    &mut Some(ingress_observer),
                    "protocol",
                    &code,
                    coded_error_response(StatusCode::BAD_REQUEST, &code, &code),
                );
            }
        }
    } else {
        None
    };
    let execution_window = ctx.deadline.remaining();
    let marker_resolution = match crate::history_marker::resolve_request_markers(
        gw.history_markers.as_ref(),
        &principal,
        &mut request,
    )
    .await
    {
        Ok(resolution) => resolution,
        Err(error) => {
            let response = coded_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_marker_unavailable",
                &error.to_string(),
            );
            return reject_before_admission(
                &mut Some(ingress_observer),
                "restoration",
                "history_marker_unavailable",
                response,
            );
        }
    };
    if let Err(error) =
        crate::media::ingest::normalize_request(&gw, &principal, &mut request, &ctx.cancellation)
            .await
    {
        return reject_before_admission(
            &mut Some(ingress_observer),
            "attachments",
            "attachment_ingest_failed",
            coded_error_response(
                StatusCode::BAD_REQUEST,
                "attachment_ingest_failed",
                &error.to_string(),
            ),
        );
    }
    ingress_observer.record_debug(|| RunEvent::Content {
        stage: "artifact_normalized_request".into(),
        model_turn_id: None,
        attempt_id: None,
        payload: checkpoint_payload(&ingress_observer, &request),
    });
    ingress_observer.record_debug(|| RunEvent::Content {
        stage: "restored_request".into(),
        model_turn_id: None,
        attempt_id: None,
        payload: checkpoint_payload(&ingress_observer, &request),
    });
    ctx.deadline = crate::proxy::context::Deadline::from_now(execution_window);
    let admission = if marker_resolution.restored_platform_segments > 0 {
        tokio::select! {
            admission = gw.principal_admission.acquire_wait(&principal, concurrency_limit) => admission,
            _ = ctx.cancellation.cancelled() => {
                let response = error_response(499, "request cancelled");
                return reject_before_admission(
                    &mut Some(ingress_observer),
                    "admission",
                    "cancelled",
                    response,
                );
            }
        }
    } else {
        gw.principal_admission
            .acquire(&principal, concurrency_limit)
    };
    let admission = match admission {
        Ok(admission) => admission,
        Err(error) => {
            let response = inference_access_error_response(error);
            return reject_before_admission(
                &mut Some(ingress_observer),
                "admission",
                "concurrency_limit",
                response,
            );
        }
    };
    let inherited_media_turns = generation_chain_write
        .as_ref()
        .map(|write| write.inherited_media_turns().to_vec())
        .unwrap_or_default();
    let generation_root_id = generation_chain_write
        .as_ref()
        .map(|write| write.root_id().to_owned())
        .or(compact_root_id);
    let generation_parent_id = generation_chain_write
        .as_ref()
        .and_then(|write| write.parent_id().map(ToOwned::to_owned))
        .or(compact_parent_id);
    let generation_node_id = generation_chain_write
        .as_ref()
        .map(|write| write.id().to_owned());
    let has_new_user = compact_has_new_user.unwrap_or_else(|| {
        generation_chain_write.as_ref().map_or_else(
            || {
                request
                    .items
                    .iter()
                    .any(|item| item.role == stravia_runtime_contract::protocol::ir::Role::User)
            },
            |write| {
                write
                    .request_delta()
                    .items
                    .iter()
                    .any(|item| item.role == stravia_runtime_contract::protocol::ir::Role::User)
            },
        )
    });
    let (route_id, model_display_name) = gw
        .model_cache
        .read()
        .await
        .resolve(&request.model)
        .map(|model| {
            (
                model.id.clone(),
                Some(model.effective_display_name().to_owned()),
            )
        })
        .unwrap_or_else(|| (request.model.clone(), None));
    // Admission carries the received client items plus the facts Generation
    // Chain already confirmed; Run Attribution derives every grouping signal.
    let observer = ingress_observer.admit(
        RunStart {
            id: ctx.request_id.clone(),
            principal: principal.api_key_id().to_owned(),
            api_key_id: Some(principal.api_key_id().to_owned()),
            api_key_name: Some(api_key_name),
            route_id,
            model_display_name,
            ingress_protocol: ingress.to_string(),
        },
        AdmissionFacts {
            client_request: client_request.clone(),
            has_new_user,
            has_matching_pending_tool_result: has_new_user
                && (compact_pending_tool_result
                    || generation_chain_write
                        .as_ref()
                        .is_some_and(|write| write.has_matching_pending_tool_result())),
            generation_root_id: generation_root_id.clone(),
            generation_parent_id: generation_parent_id.clone(),
        },
    );
    // Use the received request snapshot, never restored history or hook-added results.
    // Publication waits for the observer's credential-protected preview boundary.
    observer.capture_client_tool_results(&client_request.items);
    if has_new_user {
        observer.capture_input_preview(&client_request.items);
    }
    if let Some(root_id) = generation_root_id.clone() {
        observer.record(RunEvent::GenerationAssociated {
            root_id,
            parent_id: generation_parent_id,
            has_new_user,
        });
    }
    if (compact
        || generation_chain_write
            .as_ref()
            .is_some_and(|write| write.crosses_compaction_boundary()))
        && client_request
            .items
            .iter()
            .any(stravia_runtime_contract::protocol::ir::AiItem::is_compaction)
    {
        match gw
            .compaction
            .resolve(&principal, &client_request.items)
            .await
        {
            Ok(Some(source)) => {
                for registration_id in source.record_ids {
                    observer.record(RunEvent::NativeCompactionAssociated {
                        source_generation_id: source.source_generation_id.clone(),
                        source_operation_id: Some(source.operation_id.clone()),
                        registration_id,
                    });
                }
            }
            Ok(None) => {}
            Err(error) => {
                return coded_error_response(
                    StatusCode::BAD_REQUEST,
                    error.code(),
                    &error.to_string(),
                );
            }
        }
    }
    ctx.extensions.insert(observer.clone());
    let compaction_records = crate::model_turn::CompactionPublications::default();
    // The ledger is constructed once at admission; a malformed run fails here
    // instead of at later extension lookups.
    let ledger = RunLedger::new(
        super::RunTerminalContext::new(
            generation_node_id,
            generation_root_id,
            client_request.items.clone(),
            gw.compaction.clone(),
            principal.clone(),
            compaction_records.clone(),
        ),
        compaction_records.clone(),
    );
    ctx.extensions.insert(ledger.clone());
    if compact {
        let mut turn_input = TurnInput::new(principal.clone(), request)
            .with_execution(ctx.cancellation.clone(), ctx.deadline.at())
            .with_observer(observer.clone())
            .with_extra_headers(forwarded_client_headers(&headers));
        turn_input.purpose = crate::model_turn::ModelTurnPurpose::Compact;
        turn_input.compaction_records = compaction_records.clone();
        turn_input.compaction_source_generation_id = compaction_source_generation_id;
        let mut turn = match executor.execute(turn_input).await {
            Ok(turn) => turn,
            Err(error) => {
                return model_turn_error_response(error);
            }
        };
        let response = match turn.output.next().await {
            Some(Ok(CanonicalEvent::Compacted(response))) => {
                axum::Json(response.wire).into_response()
            }
            Some(Err(error)) => {
                return model_turn_error_response(error);
            }
            _ => {
                return coded_error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    "Compact operation ended without a native result",
                );
            }
        };
        phase.finish();
        let (delivery_admission, background_admission) = split_admission(admission);
        drop(background_admission);
        return wrap_delivery(response, delivery_admission);
    }
    let mut generation = GenerationChainRun {
        principal: principal.clone(),
        write: generation_chain_write,
        client_request,
        previous_response_id: previous_response_id.clone(),
        compaction_source_generation_id,
        vendor_publications: Vec::new(),
    };
    let session_context = stravia_runtime_contract::hook::SessionContext {
        tools_fixed: false,
        request_id: ctx.request_id.clone(),
        run_id: stravia_runtime_contract::identifier::new_id(),
        request_kind,
        ingress,
        transport: stravia_runtime_contract::hook::TransportKind::Http,
        inherited_media_turns,
        principal,
        cancellation: ctx.cancellation.clone(),
        response_id: generation.write.as_ref().map(|write| write.id().to_owned()),
        previous_response_id,
    };
    *inference_run = Some(
        match gw.hook_runtime().begin(
            session_context,
            &request,
            stravia_runtime_contract::hook::ContextCompleteness::from_request(&request),
        ) {
            Ok(run) => run,
            Err(error) => return hook_failure_response(error),
        },
    );
    let mut projection = Some(
        ClientProjectionSession::new(
            Arc::clone(&gw.history_markers),
            generation.principal.clone(),
            ingress,
        )
        .with_upload_gateway(gw.clone()),
    );
    let response = dispatch_pipeline_inner(DispatchContext {
        gw: gw.clone(),
        executor,
        headers,
        request: &mut request,
        ingress,
        ctx: &mut ctx,
        inference_run: &mut *inference_run,
        phase: &mut *phase,
        generation: &mut generation,
        projection: &mut projection,
        ledger: &ledger,
    })
    .await;
    phase.finish();
    if response.status().is_success() {
        let (delivery_admission, background_admission) = split_admission(admission);
        let response = wrap_delivery(response, delivery_admission);
        let store = Arc::clone(&gw.history_markers);
        let principal = generation.principal.clone();
        let lifecycle = gw.lifecycle.clone();
        after_body_delivery(response, async move {
            let references = ledger.published_executions();
            if references.is_empty() {
                drop(background_admission);
                return;
            }
            lifecycle.spawn(async move {
                for reference in references {
                    if let Err(error) = store.wait_terminal(&principal, &reference).await {
                        tracing::debug!(%reference, %error, "background execution wait failed");
                    }
                }
                drop(background_admission);
            });
        })
    } else {
        response
    }
}

async fn dispatch_pipeline_inner(context: DispatchContext<'_>) -> Response {
    let DispatchContext {
        gw,
        executor,
        headers,
        request,
        ingress,
        ctx,
        inference_run,
        phase,
        generation,
        projection,
        ledger,
    } = context;
    let mut delivery = DeliveryState::Buffered;
    let response = Box::pin(dispatch_round(
        DispatchContext {
            gw: gw.clone(),
            executor,
            headers,
            request: &mut *request,
            ingress,
            ctx: &mut *ctx,
            inference_run: &mut *inference_run,
            phase: &mut *phase,
            generation: &mut *generation,
            projection: &mut *projection,
            ledger,
        },
        &mut delivery,
    ))
    .await;
    if delivery == DeliveryState::Buffered
        && let Some(inference_run) = inference_run.as_mut()
        && let Err(error) = inference_run.flush_stream()
    {
        return hook_failure_response(error);
    }
    response
}
async fn dispatch_round(
    context: DispatchContext<'_>,
    delivery_state: &mut DeliveryState,
) -> Response {
    let DispatchContext {
        gw,
        executor,
        headers,
        request,
        ingress,
        ctx,
        inference_run,
        phase,
        generation: generation_chain,
        projection,
        ledger,
    } = context;
    let fixed_media_plan = request.meta.media_routing.clone();
    // Request hooks run before the route is selected so a hook may change the
    // model or synthesize a response. Authorization is applied to the resulting
    // model below.
    let request_hook_pending = match phase.current() {
        Phase::Request => true,
        Phase::HiddenRound => true,
        current => {
            return hook_failure_response(format!(
                "Inference Run entered request handling in {current:?}"
            ));
        }
    };
    if ctx.cancellation.is_cancelled() {
        return error_response(499, "request cancelled");
    }
    if request_hook_pending {
        let request_hook_result = inference_run
            .as_mut()
            .expect("buffered Inference Run")
            .on_request(request)
            .await;
        match request_hook_result {
            Ok(stravia_runtime_contract::hook::HookControl::Continue) => {}
            Ok(stravia_runtime_contract::hook::HookControl::Respond(response)) => {
                let run = inference_run.as_mut().expect("buffered Inference Run");
                let observer = ctx
                    .extensions
                    .get::<crate::interaction_observation::RunObserver>()
                    .expect("admitted Inference Run observer");
                let plan = match prepare_hook_response(
                    *response,
                    HookRespondParts {
                        request,
                        ingress,
                        inference_run: run,
                        projection: projection
                            .as_mut()
                            .expect("buffered Client Projection session"),
                        generation: generation_chain,
                        ledger,
                        phase,
                        observer: Some(&observer),
                    },
                )
                .await
                {
                    Ok(plan) => plan,
                    Err(HookRespondError::Control(control)) => {
                        return render_hook_control(*control, ingress, request.stream.enabled);
                    }
                    Err(HookRespondError::Failure(message)) => {
                        return hook_failure_response(message);
                    }
                };
                let HookResponsePlan {
                    response,
                    staged_delivery,
                    pending_generation_chain,
                } = plan;
                ledger.stage_visible_response(ingress, &response);
                let response = match projection
                    .as_mut()
                    .expect("buffered Client Projection session")
                    .prepare_upload_delivery(&response)
                    .await
                {
                    Ok(std::borrow::Cow::Borrowed(_)) => response,
                    Ok(std::borrow::Cow::Owned(delivered)) => delivered,
                    Err(error) => return hook_failure_response(error),
                };
                let response = render_hook_control(
                    stravia_runtime_contract::hook::HookControl::Respond(Box::new(response)),
                    ingress,
                    request.stream.enabled,
                );
                let mut projection = projection
                    .take()
                    .expect("delivered Hook Client Projection session");
                let gateway = gw.clone();
                let ledger = ledger.clone();
                let observer = observer.clone();
                return after_body_delivery(response, async move {
                    settle(
                        &gateway,
                        &mut projection,
                        &ledger,
                        &observer,
                        ingress,
                        Settlement {
                            staged_delivery: Some(staged_delivery),
                            pending_generation_chain: pending_generation_chain.map(|chain| *chain),
                            ..Default::default()
                        },
                    )
                    .await;
                });
            }
            Ok(control) => {
                return render_hook_control(control, ingress, request.stream.enabled);
            }
            Err(error) => return hook_failure_response(error),
        }
    }
    // Pin the entry media plan: hidden Model Legs re-apply it inside the
    // leg loop rather than letting per-leg stripping leak back.
    if let Some(plan) = &fixed_media_plan {
        request.meta.media_routing = Some(plan.clone());
    }
    if request
        .meta
        .media_routing
        .as_ref()
        .is_some_and(|plan| plan.mode == MediaRoutingMode::Bridge)
        && !stabilize_media_generation_chain(generation_chain, request)
    {
        return coded_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "media_response_chain_invalid",
            "Media bridge could not prepare the request",
        );
    }
    if let Err(response) = enter_phase(phase, Phase::Selecting) {
        return *response;
    }

    let outcome = execute_shared_model_turn(SharedModelTurnInput {
        executor: Arc::clone(&executor),
        gateway: &gw,
        request,
        ingress,
        request_context: ctx,
        inference_run,
        phase,
        generation: generation_chain.clone(),
        headers: &headers,
        projection,
        ledger,
    })
    .await;
    *delivery_state = outcome.delivery;
    *outcome.response
}

async fn acquire_turn(
    executor: &dyn ModelTurnExecutor,
    headers: &HeaderMap,
    request: &AiRequest,
    request_context: &RequestContext,
    ledger: &RunLedger,
    inference_run: &mut crate::hook::InferenceRun,
    generation: &mut GenerationChainRun,
) -> Result<(ModelTurn, AiRequest), RoundOutcome> {
    let make_input = |effective_request: AiRequest| {
        let observer = request_context
            .extensions
            .get::<crate::interaction_observation::RunObserver>()
            .expect("admitted Inference Run observer");
        observer.record_debug(|| RunEvent::Content {
            stage: "effective_model_request".into(),
            model_turn_id: None,
            attempt_id: None,
            payload: checkpoint_payload(&observer, &effective_request),
        });
        let mut input = TurnInput::new(generation.principal.clone(), effective_request)
            .with_execution(
                request_context.cancellation.clone(),
                request_context.deadline.at(),
            )
            .with_observer(
                request_context
                    .extensions
                    .get::<crate::interaction_observation::RunObserver>()
                    .expect("admitted Inference Run observer"),
            )
            .with_extra_headers(forwarded_client_headers(headers));
        input.compaction_records = ledger.compaction_records.clone();
        input.compaction_source_generation_id = generation.compaction_source_generation_id.clone();
        input
    };

    let mut effective_request = request.clone();
    let turn = match executor
        .execute(make_input(effective_request.clone()))
        .await
    {
        Ok(turn) => turn,
        Err(error)
            if error.code == "tools_unsupported"
                && !stravia_web_search::native_web_search_requested(&effective_request)
                && !stravia_protocol_codec::codec::compaction::native_compaction_requested(
                    &effective_request,
                ) =>
        {
            let original_tools = effective_request.tools.clone();
            inference_run.remove_exposed_tools(&mut effective_request);
            if effective_request.tools == original_tools {
                return Err(model_turn_execute_failure(error));
            }
            executor
                .execute(make_input(effective_request.clone()))
                .await
                .map_err(model_turn_execute_failure)?
        }
        Err(error) => return Err(model_turn_execute_failure(error)),
    };
    if let Some(write) = generation.write.as_mut() {
        write.observe_effective(effective_request.clone());
    }
    Ok((turn, effective_request))
}

async fn execute_shared_model_turn(input: SharedModelTurnInput<'_>) -> RoundOutcome {
    let SharedModelTurnInput {
        executor,
        gateway,
        request,
        ingress,
        request_context,
        inference_run,
        phase,
        mut generation,
        headers,
        projection,
        ledger,
    } = input;
    let (turn, effective_request) = match acquire_turn(
        executor.as_ref(),
        headers,
        request,
        request_context,
        ledger,
        inference_run.as_mut().expect("buffered Inference Run"),
        &mut generation,
    )
    .await
    {
        Ok(turn) => turn,
        Err(outcome) => return outcome,
    };
    *request = effective_request;
    inference_run
        .as_mut()
        .expect("Inference Run before Model Turn output")
        .set_route(turn.route.clone());
    if let Err(response) = enter_phase(phase, Phase::Calling) {
        return buffered_response(*response);
    }

    if request.stream.enabled {
        return stream::handle_model_turn_stream(stream::ModelTurnStreamInput {
            turn,
            executor: Arc::clone(&executor),
            gateway: gateway.clone(),
            headers: headers.clone(),
            ingress,
            request_context: request_context.clone(),
            request: request.clone(),
            generation,
            inference_run: inference_run.take().expect("live Inference Run"),
            phase: std::mem::replace(phase, PhaseTracker::at(Phase::Finished)),
            projection: projection.take().expect("live Client Projection session"),
            ledger: ledger.clone(),
        })
        .await;
    }

    let fixed_media_plan = request.meta.media_routing.clone();
    let observer = request_context
        .extensions
        .get::<crate::interaction_observation::RunObserver>()
        .expect("admitted Inference Run observer")
        .clone();
    let mut turn = turn;
    let prepared = 'legs: loop {
        let projection_session = projection
            .as_mut()
            .expect("buffered Client Projection session");
        let run = inference_run.as_mut().expect("buffered Inference Run");
        let mut leg = ModelLegConsume::begin(
            LegEnv {
                gateway,
                generation: &generation,
                ingress,
                observer: &observer,
            },
            &turn,
            run,
            projection_session,
            LegPolicy {
                emit_live: false,
                early_platform: true,
            },
        );
        let mut hook_leg = HookLegGuard::new(run);
        let mut ops = LegOps::Buffered(BufferedLegOps { ledger });
        let mut output = turn.output;
        while let Some(event) = output.next().await {
            match leg.feed(hook_leg.run_mut(), event) {
                LegReaction::Absorbed => {}
                LegReaction::Emit(deltas) => {
                    match leg
                        .perform_emit(&mut ops, hook_leg.run_mut(), projection_session, deltas)
                        .await
                    {
                        LegFlow::Failed(failure) => {
                            return render_leg_failure(
                                failure,
                                request,
                                ingress,
                                false,
                                ClientOutputCommit::Pending,
                            );
                        }
                        LegFlow::Open | LegFlow::Disrupted(_) | LegFlow::Faulted => {}
                    }
                }
                LegReaction::Ended => break,
                LegReaction::Failed(failure) => {
                    return render_leg_failure(
                        failure,
                        request,
                        ingress,
                        false,
                        ClientOutputCommit::Pending,
                    );
                }
            }
        }
        match leg
            .seal(&mut ops, projection_session, hook_leg.close().await, false)
            .await
        {
            SealOutcome::Ready => {}
            SealOutcome::Failed(failure) => {
                return render_leg_failure(
                    failure,
                    request,
                    ingress,
                    false,
                    ClientOutputCommit::Pending,
                );
            }
            SealOutcome::Disrupted(_) | SealOutcome::Aborted => {
                unreachable!("buffered Model Leg emit cannot disrupt")
            }
        }
        match leg
            .advance(
                &mut ops,
                LegParts {
                    request: &mut *request,
                    run: hook_leg.run_mut(),
                    phase: &mut *phase,
                    projection: &mut *projection_session,
                    ledger,
                },
                FollowupEnv {
                    executor: executor.as_ref(),
                    headers,
                    request_context,
                    generation: &mut generation,
                    fixed_media_plan: fixed_media_plan.as_ref(),
                },
            )
            .await
        {
            LegAdvance::Ready(prepared) => break 'legs *prepared,
            LegAdvance::NextLeg(next) => {
                turn = *next;
            }
            LegAdvance::HookResponse(plan) => {
                let HookResponsePlan {
                    response,
                    staged_delivery,
                    pending_generation_chain,
                } = *plan;
                ledger.stage_visible_response(ingress, &response);
                let response = match projection_session.prepare_upload_delivery(&response).await {
                    Ok(std::borrow::Cow::Borrowed(_)) => response,
                    Ok(std::borrow::Cow::Owned(delivered)) => delivered,
                    Err(error) => return buffered_response(hook_failure_response(error)),
                };
                let response = render_hook_control(
                    stravia_runtime_contract::hook::HookControl::Respond(Box::new(response)),
                    ingress,
                    false,
                );
                let mut projection_session = projection
                    .take()
                    .expect("delivered Hook Client Projection session");
                let gateway = gateway.clone();
                let ledger = ledger.clone();
                let observer = observer.clone();
                return buffered_response(after_body_delivery(response, async move {
                    settle(
                        &gateway,
                        &mut projection_session,
                        &ledger,
                        &observer,
                        ingress,
                        Settlement {
                            staged_delivery: Some(staged_delivery),
                            pending_generation_chain: pending_generation_chain.map(|chain| *chain),
                            ..Default::default()
                        },
                    )
                    .await;
                }));
            }
            LegAdvance::StreamError(error) => {
                return buffered_response(buffered_stream_error_response(&error));
            }
            LegAdvance::Outcome(outcome) => return outcome,
            LegAdvance::Failed(failure) => {
                return render_leg_failure(
                    failure,
                    request,
                    ingress,
                    false,
                    ClientOutputCommit::Pending,
                );
            }
            LegAdvance::Disrupted(_) | LegAdvance::Aborted => {
                unreachable!("buffered Model Leg emit cannot disrupt")
            }
        }
    };
    let route = turn.route.clone();

    let PreparedDelivery {
        response: prepared_response,
        staged_delivery,
        pending_generation_chain,
        background_executions,
        started_executions,
    } = prepared;
    let projection_session = projection
        .as_mut()
        .expect("buffered Client Projection session");
    let mut delivery = if request.stream.enabled {
        DeliveryAdapter::buffered_stream(ingress, route.egress)
    } else {
        DeliveryAdapter::non_stream(ingress, route.egress)
    };
    let mut delivered = match delivery
        .deliver_projected(&prepared_response, StatusCode::OK, projection_session)
        .await
    {
        Ok(delivered) => delivered,
        Err(error) => {
            return buffered_response(render_completion_failure(
                CompletionFailure::hook(error, ClientOutputCommit::Pending),
                ingress,
                request.stream.enabled,
            ));
        }
    };
    if delivered.progress != BufferedDeliveryProgress::Prepared {
        return buffered_response(delivered.response);
    }
    if !staged_delivery.is_empty()
        || !background_executions.is_empty()
        || !started_executions.is_empty()
        || pending_generation_chain.is_some()
    {
        let gateway = gateway.clone();
        let ledger = ledger.clone();
        let observer = request_context
            .extensions
            .get::<crate::interaction_observation::RunObserver>()
            .expect("admitted Inference Run observer");
        let mut projection_session = projection
            .take()
            .expect("delivered buffered Client Projection session");
        let run = if !background_executions.is_empty() || !started_executions.is_empty() {
            inference_run.take()
        } else {
            None
        };
        delivered.response = after_body_delivery(delivered.response, async move {
            settle(
                &gateway,
                &mut projection_session,
                &ledger,
                &observer,
                ingress,
                Settlement {
                    staged_delivery: Some(staged_delivery),
                    background_executions,
                    started_executions,
                    run,
                    pending_generation_chain,
                    ..Default::default()
                },
            )
            .await;
        });
    }
    ledger.stage_visible_response(ingress, &prepared_response);
    buffered_completion(delivered.response)
}
