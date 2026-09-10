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

mod canonical_stream;
mod claim;
mod completion;
mod delivery;
mod errors;
mod followup;
mod projection;
mod stream;
mod util;
use self::canonical_stream::ai_response_to_deltas;
use self::claim::*;
use self::completion::*;
use self::delivery::{
    BufferedDeliveryProgress, DeliveryAdapter, DeliveryProgress, after_body_delivery,
};
use self::errors::*;
pub(super) use self::errors::{error_response, hook_failure_response};
use self::followup::{FollowupModelTurn, acquire_followup_model_turn};
use self::projection::*;
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
#[cfg(test)]
use crate::db::models::Provider;
use crate::error::{AccessDenial, AuthFailure, GatewayError};
use crate::interaction_observation::{IngressObserver, RunEvent, RunStart};
use crate::model_turn::StreamResponseAccumulator;
#[cfg(test)]
use crate::provider::VendorRegistry;
#[cfg(test)]
use crate::provider::vendor::Vendor;
use crate::proxy::context::RequestContext;
use crate::proxy::security::{ClientCredential, Security};
use stravia_runtime_contract::model_turn::CanonicalEvent;
#[cfg(test)]
use stravia_runtime_contract::protocol::ids::Protocol;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::AiResponse;
use stravia_runtime_contract::protocol::ir::request::MediaRoutingMode;

#[cfg(test)]
fn resolve_vendor_adapter(provider: &Provider, protocol: Protocol) -> Option<Arc<dyn Vendor>> {
    let registry = VendorRegistry::global();
    let vendor_id = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty());

    if vendor_id.is_none() && protocol == Protocol::OpenResponses {
        return registry
            .get_vendor(crate::provider::registry::protocol_default_vendor(protocol))
            .cloned();
    }

    registry
        .get_vendor(vendor_id.unwrap_or("custom"))
        .cloned()
        .or_else(|| {
            registry
                .get_vendor(crate::provider::registry::protocol_default_vendor(protocol))
                .cloned()
        })
}

#[cfg(test)]
fn is_openai_generation_target(
    vendor: Option<&str>,
    preset_key: Option<&str>,
    _ingress: ProtocolId,
    is_embedding_request: bool,
) -> bool {
    if is_embedding_request {
        return false;
    }

    vendor
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty())
        .is_some_and(|vendor| vendor.eq_ignore_ascii_case("openai"))
        && preset_key.map(str::trim).is_none_or(|preset_key| {
            preset_key.is_empty() || preset_key.eq_ignore_ascii_case("openai")
        })
}

pub(super) enum RoundOutcome {
    Deliver {
        response: Response,
        delivery: DeliveryState,
    },
    NextRound {
        run: Option<Box<crate::hook::InferenceRun>>,
        phase: Option<PhaseTracker>,
    },
}

impl RoundOutcome {
    fn with_lifecycle(self, run: crate::hook::InferenceRun, phase: PhaseTracker) -> Self {
        match self {
            Self::NextRound { .. } => Self::NextRound {
                run: Some(Box::new(run)),
                phase: Some(phase),
            },
            other => other,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryState {
    Buffered,
    Live,
}

pub(super) fn buffered_response(response: Response) -> RoundOutcome {
    RoundOutcome::Deliver {
        response,
        delivery: DeliveryState::Buffered,
    }
}

pub(super) fn buffered_completion(response: Response) -> RoundOutcome {
    RoundOutcome::Deliver {
        response,
        delivery: DeliveryState::Buffered,
    }
}

pub(super) fn live_response(response: Response) -> RoundOutcome {
    RoundOutcome::Deliver {
        response,
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

fn generation_commit_flag(
    request_context: &RequestContext,
) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    request_context
        .extensions
        .get::<super::RunTerminalContext>()
        .expect("Inference Run terminal context")
        .generation_committed
}

fn stage_visible_response(request_context: &RequestContext, response: &AiResponse) {
    let Some(mut terminal) = request_context
        .extensions
        .get::<super::RunTerminalContext>()
    else {
        return;
    };
    terminal.client_output = response.items.clone();
    terminal.visible_text.extend(
        response
            .items
            .iter()
            .filter_map(|item| item.output_text_ref().or_else(|| item.refusal_ref()))
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned),
    );
    request_context.extensions.insert(terminal);
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
    egress: ProtocolId,
    encrypted_content_requested: bool,
) -> crate::protocol::transform::ThinkingCarrierFacts {
    crate::protocol::transform::ProtocolTransform::global()
        .bind(ingress, egress)
        .expect("Inference Run uses a registered protocol pair")
        .thinking_carrier_facts(encrypted_content_requested)
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
        let identity = format!("artifact_reference=\"https://stravia/artifact/{source_id}\"");
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
                        if text.starts_with("[stravia_media ") && text.contains(&identity)
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
    let ingress_observer = ctx
        .extensions
        .take::<IngressObserver>()
        .expect("Inference Run ingress observer");
    ingress_observer.record_debug(|| RunEvent::Checkpoint {
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
    let ingress_capabilities = crate::protocol::registry::ProtocolRegistry::global()
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
    ingress_observer.record_debug(|| RunEvent::Checkpoint {
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
        let controls = crate::compaction::NativeCompactionControls::classify(&request);
        let begin = if controls.requested() {
            gw.generation_chains
                .begin_native_compaction(principal.clone(), request)
                .await
        } else {
            gw.generation_chains.begin(principal.clone(), request).await
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
    let (compact_parent_id, compact_root_id, compact_has_new_user) = if compact {
        let prepared = match gw
            .generation_chains
            .prepare_compaction(principal.clone(), request)
            .await
        {
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
        )
    } else {
        (None, None, None)
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
    ingress_observer.record_debug(|| RunEvent::Checkpoint {
        stage: "artifact_normalized_request".into(),
        model_turn_id: None,
        attempt_id: None,
        payload: checkpoint_payload(&ingress_observer, &request),
    });
    ingress_observer.record_debug(|| RunEvent::Checkpoint {
        stage: "restored_request".into(),
        model_turn_id: None,
        attempt_id: None,
        payload: checkpoint_payload(&ingress_observer, &request),
    });
    if marker_resolution.restored_protected_thinking_segments > 0 {
        request.meta.vendor.ingress.insert(
            "__stravia_opaque_context_required".into(),
            serde_json::Value::Bool(true),
        );
    }
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
    let canonical_fingerprint =
        match serde_json::to_value(&client_request).and_then(|mut canonical| {
            canonical.sort_all_objects();
            serde_json::to_vec(&canonical)
        }) {
            Ok(canonical) => stravia_runtime_contract::protocol::ir::canonical::hash_hex(
                &stravia_runtime_contract::protocol::ir::canonical::hash_bytes(&canonical),
            ),
            Err(error) => {
                ingress_observer.record(RunEvent::ObservationGap {
                    reason: format!("canonical_fingerprint_serialization: {error}"),
                });
                format!("unavailable:{}", ctx.request_id)
            }
        };
    let (route_id, model_display_name) = gw
        .model_cache
        .read()
        .await
        .models
        .iter()
        .find(|model| model.model_id == request.model)
        .map(|model| {
            (
                model.id.clone(),
                Some(model.effective_display_name().to_owned()),
            )
        })
        .unwrap_or_else(|| (request.model.clone(), None));
    let observer = ingress_observer.admit(RunStart {
        id: ctx.request_id.clone(),
        principal: principal.api_key_id().to_owned(),
        api_key_id: Some(principal.api_key_id().to_owned()),
        api_key_name: Some(api_key_name),
        generation_root_id: generation_root_id.clone(),
        generation_parent_id: generation_parent_id.clone(),
        has_new_user,
        canonical_fingerprint,
        route_id,
        model_display_name,
        ingress_protocol: ingress.to_string(),
    });
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
    ctx.extensions.insert(super::RunTerminalContext {
        generation_node_id,
        generation_root_id,
        generation_committed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        waiting_client: false,
        visible_text: Vec::new(),
        client_input: Arc::new(client_request.items.clone()),
        client_output: Vec::new(),
        compaction: gw.compaction.clone(),
        principal: principal.clone(),
        compaction_records: compaction_records.clone(),
    });
    if generation_chain_write
        .as_ref()
        .is_none_or(|write| write.parent_id().is_none())
        && !client_request
            .items
            .iter()
            .any(stravia_runtime_contract::protocol::ir::AiItem::is_compaction)
    {
        observer.observe_client_input(&client_request.items);
    }
    ctx.extensions.insert(compaction_records.clone());
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
    };
    let session_context = stravia_runtime_contract::hook::SessionContext {
        tools_fixed: false,
        request_id: ctx.request_id.clone(),
        run_id: format!("run-{}", uuid::Uuid::new_v4()),
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
    })
    .await;
    phase.finish();
    if response.status().is_success() {
        let (delivery_admission, background_admission) = split_admission(admission);
        let response = wrap_delivery(response, delivery_admission);
        let store = Arc::clone(&gw.history_markers);
        let principal = generation.principal.clone();
        let extensions = ctx.extensions.clone();
        let lifecycle = gw.lifecycle.clone();
        after_body_delivery(response, async move {
            let references = extensions
                .get::<PublishedPlatformExecutions>()
                .unwrap_or_default()
                .references;
            if references.is_empty() {
                drop(background_admission);
                return;
            }
            lifecycle.spawn(async move {
                for reference in references {
                    let _ = store.wait_terminal(&principal, &reference).await;
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
    } = context;
    let mut fixed_media_plan = request.meta.media_routing.clone();
    'round: loop {
        if request.meta.media_routing.is_none() {
            request.meta.media_routing = fixed_media_plan.clone();
        }
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
                    let mut response = *response;
                    let run = inference_run.as_mut().expect("buffered Inference Run");
                    run.set_route(stravia_runtime_contract::hook::RouteContext {
                        model_id: request.model.clone(),
                        provider_id: "hook".into(),
                        target_id: "hook".into(),
                        egress: ingress,
                    });
                    match run.on_client_output(&mut response).await {
                        Ok(stravia_runtime_contract::hook::HookControl::Continue) => {}
                        Ok(stravia_runtime_contract::hook::HookControl::Respond(replacement)) => {
                            response = *replacement;
                        }
                        Ok(control) => {
                            return render_hook_control(control, ingress, request.stream.enabled);
                        }
                        Err(error) => return hook_failure_response(error),
                    }
                    if ingress == stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24
                        && let Some(write) = generation_chain.write.as_ref()
                    {
                        response.id = write.id().to_owned();
                    }
                    let projection_session = projection
                        .as_mut()
                        .expect("buffered Client Projection session");
                    projection_session.begin_model_leg(
                        thinking_carrier_facts(ingress, ingress, false),
                        run.exposed_tool_names(),
                    );
                    let observer = ctx
                        .extensions
                        .get::<crate::interaction_observation::RunObserver>()
                        .expect("admitted Inference Run observer");
                    observer.record_debug(|| RunEvent::Checkpoint {
                        stage: "response_after_hook".into(),
                        model_turn_id: None,
                        attempt_id: None,
                        payload: checkpoint_payload(&observer, &response),
                    });
                    if let Err(error) = projection_session.project_staged(&mut response, &[]).await
                    {
                        return hook_failure_response(error);
                    }
                    observer.record_debug(|| RunEvent::Checkpoint {
                        stage: "client_projection_event".into(),
                        model_turn_id: None,
                        attempt_id: None,
                        payload: checkpoint_payload(&observer, &response),
                    });
                    stage_visible_response(ctx, &response);
                    let marker_delivery = projection_session.take_staged_delivery();
                    let pending_generation_chain =
                        generation_chain.write.take().and_then(|mut write| {
                            write.observe_effective(request.clone());
                            write
                                .stage(
                                    &mut response,
                                    &crate::generation_chain::GenerationSource::Hook {
                                        protocol: ingress,
                                    },
                                    None,
                                )
                                .then_some(write)
                        });
                    if let Err(response) = enter_phase(phase, Phase::SemanticComplete) {
                        return *response;
                    }
                    if let Err(response) = enter_phase(phase, Phase::AwaitingDelivery) {
                        return *response;
                    }
                    let response = match projection_session.prepare_upload_delivery(&response).await
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
                    let generation_committed = generation_commit_flag(ctx);
                    return after_body_delivery(response, async move {
                        if let Err(error) = projection
                            .report_delivery(marker_delivery, ProjectionDelivery::Sent)
                            .await
                        {
                            tracing::error!(
                                "failed to publish delivered Hook history markers: {error}"
                            );
                            return;
                        }
                        if let Some(mut pending) = pending_generation_chain {
                            match pending.persist().await {
                                Ok(()) => generation_committed
                                    .store(true, std::sync::atomic::Ordering::Release),
                                Err(error) => tracing::error!(
                                    "failed to commit delivered Hook Generation Chain node: {error}"
                                ),
                            }
                        }
                    });
                }
                Ok(control) => {
                    return render_hook_control(control, ingress, request.stream.enabled);
                }
                Err(error) => return hook_failure_response(error),
            }
        }
        match (&fixed_media_plan, request.meta.media_routing.clone()) {
            (None, Some(plan)) => fixed_media_plan = Some(plan),
            (Some(plan), _) => request.meta.media_routing = Some(plan.clone()),
            (None, None) => {}
        }
        if request
            .meta
            .media_routing
            .as_ref()
            .is_some_and(|plan| plan.mode == MediaRoutingMode::Bridge)
            && !stabilize_media_generation_chain(&generation_chain, request)
        {
            return coded_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "media_response_chain_invalid",
                "Media bridge could not prepare the request",
            );
        }
        if let Some(write) = generation_chain.write.as_mut() {
            write.observe_effective(request.clone());
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
        })
        .await;
        match outcome {
            RoundOutcome::NextRound {
                run,
                phase: next_phase,
            } => {
                if let Some(run) = run {
                    *inference_run = Some(*run);
                }
                if let Some(next_phase) = next_phase {
                    *phase = next_phase;
                }
                continue 'round;
            }
            RoundOutcome::Deliver { response, delivery } => {
                *delivery_state = delivery;
                return response;
            }
        }
    }
}

async fn acquire_turn(
    executor: &dyn ModelTurnExecutor,
    headers: &HeaderMap,
    request: &AiRequest,
    request_context: &RequestContext,
    inference_run: &mut crate::hook::InferenceRun,
    generation: &GenerationChainRun,
) -> Result<(ModelTurn, AiRequest), RoundOutcome> {
    let make_input = |effective_request: AiRequest| {
        let observer = request_context
            .extensions
            .get::<crate::interaction_observation::RunObserver>()
            .expect("admitted Inference Run observer");
        observer.record_debug(|| RunEvent::Checkpoint {
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
        input.compaction_records = request_context
            .extensions
            .get::<crate::model_turn::CompactionPublications>()
            .unwrap_or_default();
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
                && !crate::compaction::NativeCompactionControls::classify(&effective_request)
                    .requested() =>
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
        generation,
        headers,
        projection,
    } = input;
    let (turn, effective_request) = match acquire_turn(
        executor.as_ref(),
        headers,
        request,
        request_context,
        inference_run.as_mut().expect("buffered Inference Run"),
        &generation,
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
        })
        .await;
    }

    let projection_session = projection
        .as_mut()
        .expect("buffered Client Projection session");
    projection_session.begin_model_leg(
        thinking_carrier_facts(
            ingress,
            turn.route.egress,
            turn.reasoning_encrypted_content_requested,
        ),
        inference_run
            .as_ref()
            .expect("buffered Inference Run before projection")
            .exposed_tool_names(),
    );

    let route = turn.route.clone();
    let streamed = turn.streamed;
    let completion_context = CompletionContext::from_model_turn(
        gateway.clone(),
        generation,
        ingress,
        &turn.target,
        turn.route.egress,
        turn.model_turn_id.clone(),
        request_context
            .extensions
            .get::<crate::interaction_observation::RunObserver>()
            .expect("admitted Inference Run observer"),
    );
    let mut output = turn.output;
    let mut completed_response = None;
    let mut streamed_response = streamed.then(StreamResponseAccumulator::default);
    if streamed {
        let mut terminal_deltas = Vec::new();
        let mut hook_leg = stream::HookLegGuard::new(
            inference_run
                .as_mut()
                .expect("buffered Inference Run stream"),
        );
        while let Some(event) = output.next().await {
            match event {
                Ok(CanonicalEvent::Delta(delta)) => {
                    if let stravia_runtime_contract::protocol::ir::AiStreamDelta::StreamError {
                        error,
                    } = &delta
                        && let Some(outcome) = compaction_stream_error_outcome(request, error)
                    {
                        return outcome;
                    }
                    let (terminal, deltas) = stream::partition_terminal_deltas(vec![delta]);
                    terminal_deltas.extend(terminal);
                    let transformed =
                        match stream::transform_stream_deltas(hook_leg.run_mut(), deltas) {
                            Ok(deltas) => deltas,
                            Err(error) => {
                                return buffered_response(hook_failure_response(error));
                            }
                        };
                    streamed_response
                        .as_mut()
                        .expect("stream accumulator")
                        .apply_all(&transformed);
                }
                Ok(CanonicalEvent::Completed(completed)) => {
                    completed_response = Some(*completed);
                    break;
                }
                Ok(CanonicalEvent::Compacted(_)) => {
                    return model_turn_error_outcome(
                        stravia_runtime_contract::model_turn::ModelTurnError::new(
                            "unexpected_compaction_terminal",
                            "Generation received a standalone compact result",
                        ),
                    );
                }
                Err(error) => return model_turn_error_outcome(error),
            }
        }
        let flushed = match hook_leg.close().await {
            Ok(flushed) => flushed,
            Err(error) => return buffered_response(hook_failure_response(error)),
        };
        streamed_response
            .as_mut()
            .expect("stream accumulator")
            .apply_all(&flushed);
        streamed_response
            .as_mut()
            .expect("stream accumulator")
            .apply_all(&terminal_deltas);
    } else {
        while let Some(event) = output.next().await {
            match event {
                Ok(CanonicalEvent::Delta(_)) => {}
                Ok(CanonicalEvent::Completed(completed)) => {
                    completed_response = Some(*completed);
                    break;
                }
                Ok(CanonicalEvent::Compacted(_)) => {
                    return model_turn_error_outcome(
                        stravia_runtime_contract::model_turn::ModelTurnError::new(
                            "unexpected_compaction_terminal",
                            "Generation received a standalone compact result",
                        ),
                    );
                }
                Err(error) => return model_turn_error_outcome(error),
            }
        }
    }
    let Some(mut completed_response) = completed_response else {
        return model_turn_error_outcome(
            stravia_runtime_contract::model_turn::ModelTurnError::new(
                "model_stream_incomplete",
                "Model Turn ended without a completion",
            ),
        );
    };
    let mut response = streamed_response
        .map(StreamResponseAccumulator::into_ai_response)
        .unwrap_or_else(|| completed_response.clone());
    if streamed
        && let Err(error) = completion::reconcile_completed_media(
            &mut response,
            std::mem::take(&mut completed_response.items),
        )
    {
        return model_turn_error_outcome(
            stravia_runtime_contract::model_turn::ModelTurnError::new(
                "output_media_reconciliation_failed",
                error,
            ),
        );
    }
    if response.usage.prompt_tokens == 0 && response.usage.completion_tokens == 0 {
        response.usage = completed_response.usage;
    }
    if response.id.is_empty() {
        response.id = completed_response.id;
    }
    if response.stop_reason.is_none() {
        response.stop_reason = completed_response.stop_reason;
    }
    let upstream_response_id = (!response.id.is_empty()).then(|| response.id.clone());
    let completed = match complete_canonical_response(
        &completion_context,
        CompletionInput {
            request_context,
            request,
            run: inference_run
                .as_mut()
                .expect("buffered Inference Run completion"),
            phase,
            response,
            upstream_response_id,
            early_platform_executions: Vec::new(),
            projection: projection_session,
        },
    )
    .await
    {
        CompletionOutcome::PlatformOnly(continuation) => {
            let marker_delivery = projection_session.take_staged_delivery();
            match projection_session
                .report_delivery(marker_delivery, ProjectionDelivery::Sent)
                .await
            {
                Ok(references) => {
                    let mut published = request_context
                        .extensions
                        .get::<PublishedPlatformExecutions>()
                        .unwrap_or_default();
                    published.references.extend(references);
                    request_context.extensions.insert(published);
                }
                Err(error) => {
                    return buffered_response(render_completion_failure(
                        CompletionFailure::hook(error, ClientOutputCommit::Pending),
                        ingress,
                        request.stream.enabled,
                    ));
                }
            }
            if let Err(failure) = continuation
                .finish(
                    &completion_context,
                    request_context,
                    request,
                    inference_run
                        .as_mut()
                        .expect("buffered Inference Run continuation"),
                    phase,
                )
                .await
            {
                return buffered_response(render_completion_failure(
                    failure,
                    ingress,
                    request.stream.enabled,
                ));
            }
            return RoundOutcome::NextRound {
                run: None,
                phase: None,
            };
        }
        CompletionOutcome::Ready(lease) => match (*lease).prepare(phase) {
            Ok(delivery) => delivery,
            Err(failure) => {
                return buffered_response(render_completion_failure(
                    failure,
                    ingress,
                    request.stream.enabled,
                ));
            }
        },
        CompletionOutcome::Failed(failure) => {
            return buffered_response(render_completion_failure(
                failure,
                ingress,
                request.stream.enabled,
            ));
        }
    };

    let PreparedDelivery {
        response: prepared_response,
        pending_generation_chain,
        background_executions,
        started_executions,
    } = completed;
    let marker_delivery = projection_session.take_staged_delivery();
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
    if !marker_delivery.is_empty()
        || !background_executions.is_empty()
        || !started_executions.is_empty()
        || pending_generation_chain.is_some()
    {
        let mut marker_context = completion_context.clone();
        let gateway = gateway.clone();
        let generation_committed = generation_commit_flag(request_context);
        let request_context = request_context.clone();
        let mut projection_session = projection
            .take()
            .expect("delivered buffered Client Projection session");
        let mut pending_generation_chain = pending_generation_chain;
        let run = if !background_executions.is_empty() || !started_executions.is_empty() {
            inference_run.take()
        } else {
            None
        };
        delivered.response = after_body_delivery(delivered.response, async move {
            marker_context.mark_client_output_committed();
            let published_references = match projection_session
                .report_delivery(marker_delivery, ProjectionDelivery::Sent)
                .await
            {
                Ok(references) => references,
                Err(error) => {
                    tracing::error!(
                        "failed to publish delivered buffered history markers: {error}"
                    );
                    return;
                }
            };
            if !published_references.is_empty() {
                let mut published = request_context
                    .extensions
                    .get::<PublishedPlatformExecutions>()
                    .unwrap_or_default();
                published.references.extend(published_references);
                request_context.extensions.insert(published);
            }
            let mut started_executions = started_executions;
            if !background_executions.is_empty() {
                started_executions.extend(gateway.start_history_marker_executions(
                    marker_context.principal().clone(),
                    background_executions,
                ));
            }
            if !started_executions.is_empty() {
                if let Some(run) = run {
                    gateway.spawn_started_history_marker_executions(started_executions, run);
                } else {
                    tracing::error!(
                        "delivered history markers have Platform executions but no Inference Run"
                    );
                }
            }
            if let Some(write) = pending_generation_chain.as_mut() {
                match write.persist().await {
                    Ok(()) => {
                        generation_committed.store(true, std::sync::atomic::Ordering::Release)
                    }
                    Err(error) => {
                        tracing::error!("failed to commit delivered Generation Chain node: {error}")
                    }
                }
            }
        });
    }
    stage_visible_response(request_context, &prepared_response);
    buffered_completion(delivered.response)
}

// StreamResponseAccumulator and ensure_tool_index are in accumulator.rs.

#[cfg(test)]
mod openai_generation_target_tests {
    use super::*;
    use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
    fn unlabelled_provider() -> Provider {
        Provider {
            id: "provider".into(),
            name: "Custom Provider".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "https://example.com/v1".into(),
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            adapter_credentials: r#"{"apiKey":"secret"}"#.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn unlabelled_open_responses_target_uses_openai_vendor_adapter() {
        let adapter = resolve_vendor_adapter(&unlabelled_provider(), Protocol::OpenResponses)
            .expect("Open Responses vendor adapter");

        assert_eq!(adapter.vendor_id(), "openai");
    }

    #[test]
    fn unlabelled_chat_target_keeps_custom_vendor_adapter() {
        let adapter = resolve_vendor_adapter(&unlabelled_provider(), Protocol::OpenAICompatible)
            .expect("custom vendor adapter");

        assert_eq!(adapter.vendor_id(), "custom");
    }

    #[test]
    fn unlabelled_open_responses_target_does_not_enable_generation_transport() {
        assert!(!is_openai_generation_target(
            None,
            None,
            OPEN_RESPONSES_2026_04_24,
            false
        ));
    }

    #[test]
    fn unlabelled_chat_target_does_not_change_protocol_negotiation() {
        assert!(!is_openai_generation_target(
            None,
            None,
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false
        ));
    }

    #[test]
    fn explicit_openai_target_keeps_generation_transport() {
        assert!(is_openai_generation_target(
            Some("openai"),
            Some("openai"),
            stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            false
        ));
    }

    #[test]
    fn embeddings_never_use_responses_generation_transport() {
        assert!(!is_openai_generation_target(
            Some("openai"),
            None,
            OPEN_RESPONSES_2026_04_24,
            true
        ));
    }

    #[test]
    fn catalog_openai_vendor_does_not_enable_generation_transport() {
        assert!(!is_openai_generation_target(
            Some("openai"),
            Some("meta"),
            OPEN_RESPONSES_2026_04_24,
            false
        ));
    }
}
