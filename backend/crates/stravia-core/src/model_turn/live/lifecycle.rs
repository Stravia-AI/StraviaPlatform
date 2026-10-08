use std::sync::atomic::{AtomicU8, Ordering};

use super::{
    AttemptFailure, AttemptRoutePolicy, PreparedAttempt, UPSTREAM_FAILURE_MASK,
    UPSTREAM_NOT_STARTED, UPSTREAM_STARTED, UpstreamLocalWork, VendorDriverReady,
    VendorPublishedResult, VendorTerminal, classify_vendor_error, classify_vendor_kind,
    classify_vendor_operation_error, interruption_error, mark_upstream_operation_finished,
    request_contains_video, stream_delta_failure, target_namespace,
};
use crate::Gateway;
use crate::model_turn::provider::AttemptObservation;
use crate::model_turn::support::ai_response_to_deltas;
use crate::model_turn::{CanonicalEvent, ModelTurnError};
use crate::plugin::{
    VendorCallContext, VendorEvent, VendorExecution, VendorPublicationFence, VendorRequest,
};
use crate::router::{RouteAttemptReservation, SelectedTarget, selected_target_key};
use stravia_runtime_contract::Deadline;
use stravia_runtime_contract::protocol::ir::{AiRequest, AiStreamDelta, request::MediaRoutingMode};
use stravia_vendor_runtime::RuntimeEvent;
use stravia_vendor_sdk::OperationOutput;

mod precommit;
use precommit::PrecommitBuffer;

/// 单一 owner 保存整个 attempt（含 recovery）的输出发布进度。
pub(super) struct OutputLifecycle<'a> {
    gateway: &'a Gateway,
    principal: &'a stravia_runtime_contract::Principal,
    parent_cancellation: &'a stravia_runtime_contract::CancellationToken,
    operation_cancellation: &'a stravia_runtime_contract::CancellationToken,
    deadline: &'a Deadline,
    policy: &'a AttemptRoutePolicy,
    target: &'a SelectedTarget,
    output: &'a tokio::sync::mpsc::Sender<VendorPublishedResult>,
    ready: Option<tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>>,
    committed: bool,
    streamed: bool,
    last_publication: Option<VendorPublicationFence>,
    reservation: Option<RouteAttemptReservation>,
}

impl<'a> OutputLifecycle<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        gateway: &'a Gateway,
        principal: &'a stravia_runtime_contract::Principal,
        parent_cancellation: &'a stravia_runtime_contract::CancellationToken,
        operation_cancellation: &'a stravia_runtime_contract::CancellationToken,
        deadline: &'a Deadline,
        policy: &'a AttemptRoutePolicy,
        target: &'a SelectedTarget,
        output: &'a tokio::sync::mpsc::Sender<VendorPublishedResult>,
        ready: tokio::sync::oneshot::Sender<Result<VendorDriverReady, AttemptFailure>>,
    ) -> Self {
        Self {
            gateway,
            principal,
            parent_cancellation,
            operation_cancellation,
            deadline,
            policy,
            target,
            output,
            ready: Some(ready),
            committed: false,
            streamed: false,
            last_publication: None,
            reservation: None,
        }
    }

    pub(super) fn committed(&self) -> bool {
        self.committed
    }

    fn reserve(&mut self) {
        if !self.committed && self.reservation.is_none() {
            self.reservation = Some(self.policy.state.reservation(
                self.policy.context.clone(),
                selected_target_key(self.target),
                self.policy.epoch,
                self.policy.probe,
            ));
        }
    }

    pub(super) async fn run(
        &mut self,
        prepared: &mut PreparedAttempt,
        request: &AiRequest,
        attempt: &AttemptObservation,
        first_token_ms: &mut Option<i64>,
    ) -> Result<VendorTerminal, AttemptFailure> {
        Operation {
            lifecycle: self,
            attempt,
            first_token_ms,
            precommit: PrecommitBuffer::default(),
            emitted_delta: false,
            pending_failure: None,
        }
        .run(prepared, request)
        .await
    }

    /// 发布权威 typed 结果；策略 accounting 仍由 driver 负责。
    /// 失败时已记录 interruption 并通知尚待 ready 的 caller，返回 None 阻止成功 accounting。
    pub(super) async fn publish_terminal(
        &mut self,
        terminal: VendorTerminal,
        attempt: &AttemptObservation,
        first_token_ms: &mut Option<i64>,
    ) -> Option<i64> {
        self.reserve();
        let (event, publication) = match terminal {
            VendorTerminal::Infer(response, publication) => {
                attempt.confirm_usage(&response.usage);
                (CanonicalEvent::Completed(response), publication)
            }
            VendorTerminal::Compact(response, publication) => {
                if let Some(usage) = &response.usage {
                    attempt.confirm_usage(usage);
                }
                (CanonicalEvent::Compacted(Box::new(response)), publication)
            }
        };
        let first_commit = !self.committed;
        let timing = *first_token_ms.get_or_insert_with(|| attempt.elapsed_ms());
        let result = send_vendor_output(
            self.output,
            &publication,
            self.parent_cancellation,
            self.operation_cancellation,
            self.deadline,
            Ok(event),
        )
        .await;
        match result {
            Ok(()) => {
                if first_commit {
                    attempt.record_first_token();
                }
                Some(timing)
            }
            Err(error) => {
                attempt.finish("interrupted", None, Some(error.code.clone()), None, None);
                if first_commit && let Some(ready) = self.ready.take() {
                    let _ = ready.send(Err(AttemptFailure::terminal(error.code, error.message)));
                }
                None
            }
        }
    }

    /// driver 成功 accounting 完成后，才完成 reservation 与 ready 通知。
    pub(super) fn complete(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            reservation.complete();
        }
        if !self.committed
            && let Some(ready) = self.ready.take()
        {
            let _ = ready.send(Ok(VendorDriverReady {
                streamed: self.streamed,
            }));
        }
    }

    pub(super) async fn publish_failure(
        &mut self,
        failure: AttemptFailure,
        terminal_output: tokio::sync::mpsc::OwnedPermit<VendorPublishedResult>,
        observer: Option<&crate::interaction_observation::RunObserver>,
    ) {
        if self.committed {
            let error = failure.finish(observer);
            if let Some(publication) = self.last_publication.as_ref()
                && let Err(error) =
                    send_vendor_terminal_error(terminal_output, publication, error).await
            {
                tracing::debug!(code = %error.code, "Vendor terminal failure publication revoked");
            }
        } else if let Some(ready) = self.ready.take() {
            let _ = ready.send(Err(failure));
        }
    }
}

/// 每次 operation 的局部状态不得重置跨 recovery 的发布进度。
struct Operation<'a, 'b> {
    lifecycle: &'a mut OutputLifecycle<'b>,
    attempt: &'a AttemptObservation,
    first_token_ms: &'a mut Option<i64>,
    emitted_delta: bool,
    precommit: PrecommitBuffer,
    pending_failure: Option<AttemptFailure>,
}

impl Operation<'_, '_> {
    async fn run(
        &mut self,
        prepared: &mut PreparedAttempt,
        request: &AiRequest,
    ) -> Result<VendorTerminal, AttemptFailure> {
        let gateway = self.lifecycle.gateway;
        let principal = self.lifecycle.principal;
        let parent_cancellation = self.lifecycle.parent_cancellation;
        let operation_cancellation = self.lifecycle.operation_cancellation;
        let deadline = self.lifecycle.deadline.clone();
        let policy = self.lifecycle.policy;
        let target = self.lifecycle.target;
        let output = self.lifecycle.output;
        let attempt = self.attempt;
        prepared
            .upstream_state
            .store(UPSTREAM_NOT_STARTED, Ordering::Release);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
        let mut context = VendorCallContext::new(operation_cancellation.clone(), deadline.clone());
        let send_admission = crate::rpm::SendAdmission {
            admission: gateway.rpm_admission.clone(),
            root: prepared.root_request.clone(),
            key: crate::rpm::DestinationKey::for_target(target),
            cancellation: operation_cancellation.clone(),
            deadline: deadline.clone(),
            failure: prepared.send_failure.clone(),
            sent: prepared.sent.clone(),
            upstream_state: Some(prepared.upstream_state.clone()),
            eligibility: Some(crate::rpm::runtime::SendEligibility {
                storage: gateway.storage.clone(),
                routes: gateway.model_cache.clone(),
                admitted_component: prepared.pinned_execution.pinned_component(),
                admitted_provider_id: prepared.pinned_execution.descriptor().provider_id.clone(),
                route_id: prepared.route.model_id.clone(),
                target: target.destination.clone(),
                principal: principal.clone(),
                authorization: prepared.authorization,
                health: policy.state.clone(),
                target_key: selected_target_key(target),
                epoch: policy.epoch,
                single_attempt: policy.probe,
                capability: if prepared.compact {
                    stravia_vendor_sdk::Capability::Compact
                } else {
                    stravia_vendor_sdk::Capability::Infer
                },
                requires_video: request_contains_video(request),
                requires_image: request
                    .meta
                    .media_routing
                    .as_ref()
                    .is_some_and(|plan| plan.mode == MediaRoutingMode::Native),
            }),
        };
        context.send_admission = Some(send_admission.clone());
        context.root_request = prepared.root_request.clone();
        context.events = Some(event_tx);
        context.observer = prepared.observer.clone();
        context.model_turn_id = Some(prepared.model_turn_id.clone());
        context.attempt_id = Some(attempt.id.clone());
        context.client_headers = prepared.client_headers.clone();
        context.metadata = prepared.metadata.clone();
        context.websocket_affinity = prepared.websocket_affinity.clone();
        context.response_continuation_available = prepared.response_continuation_available.clone();

        let kind = if prepared.compact {
            stravia_vendor_sdk::Operation::Compact
        } else {
            stravia_vendor_sdk::Operation::Infer
        };
        let execution_handle = match prepared.execution.take() {
            Some(execution) => execution,
            None => {
                let mut execution = gateway
                    .prepare_vendor_execution_with_lease(
                        &prepared.pinned_execution,
                        &prepared.route.provider_id,
                        Some(&prepared.actual_model),
                        kind,
                        &context,
                    )
                    .await
                    .map_err(|_| {
                        if operation_cancellation.is_cancelled() || deadline.is_exceeded() {
                            AttemptFailure::terminal(
                                interruption_error(&deadline).code,
                                interruption_error(&deadline).message,
                            )
                        } else {
                            AttemptFailure::reroutable(
                                "provider_unavailable",
                                "Vendor execution could not be reprepared",
                            )
                        }
                    })?;
                gateway
                    .select_vendor_protocol(&mut execution, request, &context)
                    .await
                    .map_err(classify_vendor_error)?;
                execution
            }
        };
        prepared.attempt_credential_version = Some(execution_handle.credential_version());
        let connection_changed = execution_handle.protocol().trim() != prepared.protocol_hint
            || target_namespace(
                &prepared.route.provider_id,
                &execution_handle.descriptor().provider_id,
                execution_handle.provider(),
                execution_handle.oauth_connection_id(),
                &prepared.route.target_id,
                &prepared.actual_model,
                execution_handle.use_proxy(),
            ) != prepared.namespace;
        if connection_changed {
            return Err(if self.lifecycle.committed {
                AttemptFailure::terminal(
                    "vendor_snapshot_changed",
                    "Vendor compatibility changed after output publication began",
                )
            } else {
                AttemptFailure::reroutable(
                    "vendor_snapshot_changed",
                    "Vendor compatibility changed while repreparing the operation",
                )
            });
        }

        let mut request = request.clone();
        request.model.clone_from(&prepared.dispatch_model);
        crate::media::ingest::materialize_request(
            gateway,
            principal,
            &mut request,
            prepared.route.egress,
        )
        .await
        .map_err(|error| {
            AttemptFailure::terminal("attachment_delivery_failed", error.to_string())
        })?;
        let vendor_request = if prepared.compact {
            VendorRequest::Compact(request)
        } else {
            VendorRequest::Infer(request)
        };
        let compact = prepared.compact;
        let execution = gateway.execute_prepared_vendor(execution_handle, vendor_request, context);
        tokio::pin!(execution);
        let mut operation_result = None;

        loop {
            tokio::select! {
                biased;
                _ = parent_cancellation.cancelled() => {
                    operation_cancellation.cancel();
                    let interruption = interruption_error(&deadline);
                    return Err(if interruption.code == "deadline_exceeded"
                        && prepared.upstream_state.load(Ordering::Acquire)
                            & UPSTREAM_FAILURE_MASK
                            == UPSTREAM_STARTED
                    {
                        AttemptFailure::upstream(
                            stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
                            None,
                            interruption.code,
                            interruption.message,
                            None,
                        )
                    } else {
                        AttemptFailure::terminal(interruption.code, interruption.message)
                    });
                }
                () = deadline.wait() => {
                    operation_cancellation.cancel();
                    return Err(if prepared.upstream_state.load(Ordering::Acquire)
                        & UPSTREAM_FAILURE_MASK
                        == UPSTREAM_STARTED
                    {
                        AttemptFailure::upstream(
                            stravia_runtime_contract::protocol::ir::AiErrorKind::Timeout,
                            None,
                            "deadline_exceeded",
                            "Model Turn deadline exceeded",
                            None,
                        )
                    } else {
                        AttemptFailure::terminal(
                            "deadline_exceeded",
                            "Model Turn deadline exceeded",
                        )
                    });
                }
                _ = output.closed() => {
                    operation_cancellation.cancel();
                    return Err(AttemptFailure::terminal("cancelled", "Model Turn consumer disconnected"));
                }
                event = event_rx.recv() => match event {
                    Some(event) => self.receive(event, prepared.preserve_upstream_error, &prepared.upstream_state).await?,
                    None => break,
                },
                result = &mut execution => {
                    mark_upstream_operation_finished(&prepared.upstream_state, &result, &deadline);
                    operation_result = Some(result);
                    break;
                }
            }
        }

        if operation_result.is_none() {
            let result = execution.await;
            mark_upstream_operation_finished(&prepared.upstream_state, &result, &deadline);
            operation_result = Some(result);
        }
        while let Some(event) = event_rx.recv().await {
            self.receive(
                event,
                prepared.preserve_upstream_error,
                &prepared.upstream_state,
            )
            .await?;
        }
        if let Some(error) = send_admission.failure.lock().take() {
            let error = error.model_error();
            let mut failure = AttemptFailure::terminal(&error.code, &error.message);
            failure.error = Box::new(error);
            return Err(failure);
        }
        if let Some(failure) = self.pending_failure.take() {
            return Err(failure);
        }
        let VendorExecution {
            output: result,
            publication,
            protocol,
        } = operation_result
            .expect("operation result")
            .map_err(|error| {
                classify_vendor_operation_error(error, &prepared.upstream_state, &deadline)
            })?;
        if protocol.trim() != prepared.protocol_hint {
            return Err(if self.lifecycle.committed {
                AttemptFailure::terminal(
                    "vendor_snapshot_changed",
                    "Vendor compatibility changed after output publication began",
                )
            } else {
                AttemptFailure::reroutable(
                    "vendor_snapshot_changed",
                    "Vendor compatibility changed while preparing the operation",
                )
            });
        }

        match (compact, result) {
            (false, OperationOutput::Infer(response)) => {
                if prepared.request.meta.redaction.has_provider_proof() {
                    // 上游画像不含宿主解析出的 Target control，证明仍需保留本轮实际派发的控制。
                    let target_control = prepared.request.reasoning.target_control.take();
                    let effective_controls =
                        crate::generation_chain::apply_provider_effective_response(
                            &mut prepared.request,
                            &response,
                        )
                        .is_some();
                    prepared.request.reasoning.target_control = target_control;
                    if effective_controls {
                        prepared
                            .request
                            .meta
                            .redaction
                            .observe_provider_controls(&prepared.request);
                    }
                }
                if !self.emitted_delta {
                    // Synthetic deltas are observable and may become persisted history before the
                    // terminal response is consumed. Externalize output media first so neither path
                    // can publish provider bytes instead of the stable Artifact Reference.
                    let mut projected = response.as_ref().clone();
                    let normalization = tokio::select! {
                        biased;
                        _ = parent_cancellation.cancelled() => {
                            operation_cancellation.cancel();
                            return Err(AttemptFailure::terminal(
                                interruption_error(&deadline).code,
                                interruption_error(&deadline).message,
                            ));
                        }
                        () = deadline.wait() => {
                            operation_cancellation.cancel();
                            return Err(AttemptFailure::terminal(
                                "deadline_exceeded",
                                "Model Turn deadline exceeded",
                            ));
                        }
                        result = crate::media::ingest::normalize_response(
                            gateway,
                            principal,
                            &mut projected,
                            operation_cancellation,
                        ) => result,
                    };
                    normalization.map_err(|error| {
                        AttemptFailure::terminal("output_media_ingest_failed", error.to_string())
                    })?;
                    let deltas = ai_response_to_deltas(&projected)
                        .into_iter()
                        .map(|delta| (delta, publication.clone()))
                        .collect();
                    self.commit(deltas, false).await?;
                } else if !self.lifecycle.committed {
                    self.commit(Vec::new(), true).await?;
                }
                Ok(VendorTerminal::Infer(response, publication))
            }
            (true, OperationOutput::Compact(response)) => {
                Ok(VendorTerminal::Compact(response, publication))
            }
            _ => Err(AttemptFailure::terminal(
                "vendor_output_invalid",
                "Vendor plugin returned an output for the wrong operation",
            )),
        }
    }

    async fn receive(
        &mut self,
        vendor_event: VendorEvent,
        preserve_upstream_error: bool,
        upstream_state: &AtomicU8,
    ) -> Result<(), AttemptFailure> {
        let gateway = self.lifecycle.gateway;
        let principal = self.lifecycle.principal;
        let parent_cancellation = self.lifecycle.parent_cancellation;
        let operation_cancellation = self.lifecycle.operation_cancellation;
        let deadline = self.lifecycle.deadline;
        vendor_event.publication.ensure_current().map_err(|_| {
            AttemptFailure::terminal("cancelled", "Vendor result can no longer be published")
        })?;
        let VendorEvent { event, publication } = vendor_event;
        match event {
            RuntimeEvent::UpstreamStarted => {
                // 插件事件可能先于 Host RPM 准入；只有宿主实际发送才能标记开始。
            }
            RuntimeEvent::Delta(mut delta) => {
                let _local_work = UpstreamLocalWork::begin(upstream_state, deadline.clone());
                if matches!(&delta, AiStreamDelta::ItemDone { .. }) {
                    let normalization = tokio::select! {
                        biased;
                        _ = parent_cancellation.cancelled() => {
                            operation_cancellation.cancel();
                            return Err(AttemptFailure::terminal(
                                interruption_error(deadline).code,
                                interruption_error(deadline).message,
                            ));
                        }
                        () = deadline.wait() => {
                            operation_cancellation.cancel();
                            return Err(AttemptFailure::terminal(
                                "deadline_exceeded",
                                "Model Turn deadline exceeded",
                            ));
                        }
                        result = crate::media::ingest::normalize_stream_delta(
                            gateway,
                            principal,
                            &mut delta,
                            operation_cancellation,
                        ) => result,
                    };
                    normalization.map_err(|error| {
                        AttemptFailure::terminal("output_media_ingest_failed", error.to_string())
                    })?;
                }
                self.emitted_delta = true;
                self.lifecycle.streamed = true;
                if matches!(&delta, AiStreamDelta::Usage(_)) {
                    // 尚未提交输出的失败也保留上游已报告用量，不提前确认累计中的快照。
                    self.attempt.observe_delta(&delta);
                }
                if !self.lifecycle.committed {
                    if matches!(
                        delta,
                        AiStreamDelta::StreamError { .. } | AiStreamDelta::UnexpectedEof
                    ) {
                        self.pending_failure =
                            Some(stream_delta_failure(&delta, preserve_upstream_error));
                        return Ok(());
                    }
                    let commits = self.precommit.push(delta, publication)?;
                    if commits {
                        self.commit(Vec::new(), true).await?;
                    }
                } else {
                    self.send_delta(delta, &publication).await?;
                }
            }
            RuntimeEvent::Completed | RuntimeEvent::Compacted => {
                // The typed return value is authoritative and is fenced by
                // execute_vendor after the guest has returned. Event terminals are
                // deliberately held rather than published early.
            }
            RuntimeEvent::Failed {
                kind,
                message,
                upstream_status,
            } => {
                self.pending_failure.get_or_insert_with(|| {
                    classify_vendor_kind(kind, upstream_status, Some(message))
                });
            }
        }
        Ok(())
    }

    async fn commit(
        &mut self,
        deltas: Vec<(AiStreamDelta, VendorPublicationFence)>,
        emitted_by_plugin: bool,
    ) -> Result<(), AttemptFailure> {
        let first_commit = !self.lifecycle.committed;
        self.lifecycle.reserve();
        let mut buffered = self.precommit.take();
        buffered.extend(deltas);
        for (index, (delta, publication)) in buffered.into_iter().enumerate() {
            self.send_delta(delta, &publication).await?;
            if first_commit && index == 0 {
                self.attempt.record_first_token();
                *self.first_token_ms = Some(self.attempt.elapsed_ms());
                self.lifecycle.committed = true;
                if let Some(ready) = self.lifecycle.ready.take() {
                    let _ = ready.send(Ok(VendorDriverReady {
                        streamed: self.lifecycle.streamed || emitted_by_plugin,
                    }));
                }
            }
        }
        Ok(())
    }

    async fn send_delta(
        &mut self,
        delta: AiStreamDelta,
        publication: &VendorPublicationFence,
    ) -> Result<(), AttemptFailure> {
        if !matches!(&delta, AiStreamDelta::Usage(_)) || !self.emitted_delta {
            self.attempt.observe_delta(&delta);
        }
        send_vendor_output(
            self.lifecycle.output,
            publication,
            self.lifecycle.parent_cancellation,
            self.lifecycle.operation_cancellation,
            self.lifecycle.deadline,
            Ok(CanonicalEvent::Delta(delta)),
        )
        .await
        .map_err(|error| AttemptFailure::terminal(error.code, error.message))?;
        self.lifecycle.last_publication = Some(publication.clone());
        Ok(())
    }
}

async fn send_vendor_output(
    output: &tokio::sync::mpsc::Sender<VendorPublishedResult>,
    publication: &VendorPublicationFence,
    parent_cancellation: &stravia_runtime_contract::CancellationToken,
    operation_cancellation: &stravia_runtime_contract::CancellationToken,
    deadline: &Deadline,
    event: Result<CanonicalEvent, ModelTurnError>,
) -> Result<(), ModelTurnError> {
    let permit = tokio::select! {
        biased;
        _ = publication.cancelled() => {
            return Err(ModelTurnError::new("cancelled", "Vendor result can no longer be published"));
        }
        _ = parent_cancellation.cancelled() => {
            operation_cancellation.cancel();
            return Err(interruption_error(deadline));
        }
        () = deadline.wait() => {
            operation_cancellation.cancel();
            return Err(ModelTurnError::new("deadline_exceeded", "Model Turn deadline exceeded"));
        }
        permit = output.reserve() => permit.map_err(|_| {
            operation_cancellation.cancel();
            ModelTurnError::new("cancelled", "Model Turn consumer disconnected")
        })?,
    };
    let guard = publication.write_fence().await.map_err(|_| {
        ModelTurnError::new("cancelled", "Vendor result can no longer be published")
    })?;
    permit.send(VendorPublishedResult {
        result: event,
        publication: publication.clone(),
    });
    drop(guard);
    Ok(())
}

async fn send_vendor_terminal_error(
    permit: tokio::sync::mpsc::OwnedPermit<VendorPublishedResult>,
    publication: &VendorPublicationFence,
    error: ModelTurnError,
) -> Result<(), ModelTurnError> {
    let guard = publication.terminal_write_fence().await.map_err(|_| {
        ModelTurnError::new("cancelled", "Vendor result can no longer be published")
    })?;
    drop(permit.send(VendorPublishedResult {
        result: Err(error),
        publication: publication.clone(),
    }));
    drop(guard);
    Ok(())
}
