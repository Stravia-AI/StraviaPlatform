use std::collections::BTreeMap;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_vendor_runtime::{RuntimeError, RuntimeEvent};
use stravia_vendor_sdk::{Capability, Operation, OperationOutput};

use crate::Gateway;
use crate::db::models::RouteConfig;
use crate::plugin::execution::PreparedVendorExecution;
use crate::plugin::{VendorCallContext, VendorEvent, VendorPublicationFence, VendorRequest};
use crate::router::selector::AttemptFailureSignal;
use crate::router::{
    AttemptFailureDisposition, RouteSelector, SelectionError, selected_target_key,
};

use super::provider::AttemptObservation;

pub(crate) struct VendorRouteExecution {
    pub(crate) output: OperationOutput,
    pub(crate) publication: VendorPublicationFence,
    pub(crate) provider_id: String,
    pub(crate) upstream_model: Option<String>,
    pub(crate) target_id: String,
}

/// Accounts one independent capability invocation as a request while keeping
/// its upstream attempts under the same durable observation identity.
///
/// Capability routes do not construct a live `ModelTurn`, so they must own the
/// equivalent start/finish boundary themselves. The drop fallback covers every
/// early error and cancellation path without manufacturing token usage.
struct CapabilityObservation {
    observer: Option<crate::interaction_observation::RunObserver>,
    id: String,
    finished: bool,
}

impl CapabilityObservation {
    fn new(
        observer: Option<crate::interaction_observation::RunObserver>,
        route: &RouteConfig,
        estimated_input_tokens: u64,
    ) -> Self {
        let id = observer
            .as_ref()
            .map(|_| stravia_runtime_contract::identifier::new_id())
            .unwrap_or_default();
        if let Some(observer) = &observer {
            observer.record(crate::interaction_observation::RunEvent::ModelTurnStarted {
                model_turn_id: id.clone(),
                route_id: route.id.clone().into(),
                model_display_name: route.display_name.clone(),
                estimated_input_tokens: i64::try_from(estimated_input_tokens).ok(),
            });
        }
        Self {
            observer,
            id,
            finished: false,
        }
    }

    fn finish(&mut self, status: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(observer) = &self.observer {
            observer.record(
                crate::interaction_observation::RunEvent::ModelTurnFinished {
                    model_turn_id: self.id.clone(),
                    status: status.to_owned(),
                },
            );
        }
    }
}

impl Drop for CapabilityObservation {
    fn drop(&mut self) {
        self.finish("failed");
    }
}

impl Gateway {
    /// Validates every enabled destination. A mixed Route is rejected rather
    /// than silently filtering targets and changing its scheduling policy.
    pub(crate) async fn validate_vendor_route_capability(
        &self,
        route: &RouteConfig,
        capability: Capability,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(route.is_enabled, "Route is disabled");
        anyhow::ensure!(
            route.targets.iter().any(|target| target.enabled),
            "Route has no enabled Target"
        );

        for target in route.targets.iter().filter(|target| target.enabled) {
            let provider = self
                .storage
                .providers()
                .get(target.provider_id().as_str())
                .await?
                .ok_or_else(|| anyhow::anyhow!("Target Provider is unavailable"))?;
            anyhow::ensure!(provider.is_enabled, "Target Provider is disabled");
            let vendor_id = provider
                .vendor
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Target Provider has no Vendor plugin"))?;
            let descriptor = self.vendor_plugins.descriptor(vendor_id)?;
            let channel_id = provider.channel.as_deref().unwrap_or("default");
            let channel = descriptor
                .channels
                .iter()
                .find(|channel| channel.id == channel_id)
                .ok_or_else(|| anyhow::anyhow!("Target Provider channel is unavailable"))?;
            anyhow::ensure!(
                channel.capabilities.contains(&capability),
                "Target Provider channel does not support {}",
                capability.as_str()
            );

            match target.model().map(|model| model.as_str()) {
                Some(model) => {
                    anyhow::ensure!(
                        !model.trim().is_empty()
                            && (capability != Capability::MediaImage || model.trim() != "*"),
                        "Target Provider Model is invalid"
                    );
                    let model = self
                        .storage
                        .provider_models()
                        .find(target.provider_id().as_str(), model)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Target Provider Model is unavailable"))?;
                    anyhow::ensure!(
                        model.effective_available(),
                        "Target Provider Model is unavailable"
                    );
                    let model_capabilities = model
                        .metadata
                        .extensions
                        .get("capabilities")
                        .and_then(serde_json::Value::as_array);
                    if capability == Capability::MediaImage || model_capabilities.is_some() {
                        anyhow::ensure!(
                            model_capabilities.is_some_and(|capabilities| {
                                capabilities.iter().any(|value| {
                                    value.as_str() == Some(capability.as_str())
                                        || (capability == Capability::MediaImage
                                            && value.as_str() == Some("image_output"))
                                })
                            }),
                            "Target Provider Model does not support {}",
                            capability.as_str()
                        );
                    }
                }
                None => {
                    anyhow::ensure!(
                        capability == Capability::Search && !channel.search_model_required,
                        "Target requires a Provider Model for {}",
                        capability.as_str()
                    );
                }
            }
        }
        Ok(())
    }

    /// Executes one independent full-search or image operation through the
    /// same Route policy as model turns. Only a typed upstream failure after a
    /// real `UpstreamStarted` event is eligible for retry or failover.
    pub(crate) async fn execute_vendor_route(
        &self,
        principal: &Principal,
        route: &RouteConfig,
        request: VendorRequest,
        context: VendorCallContext,
    ) -> anyhow::Result<VendorRouteExecution> {
        let (capability, operation) = match &request {
            VendorRequest::Search(_) => (Capability::Search, Operation::Search),
            VendorRequest::MediaImage(_) => (Capability::MediaImage, Operation::MediaImage),
            _ => anyhow::bail!("independent Vendor Routes support only search and image media"),
        };
        self.validate_vendor_route_capability(route, capability)
            .await?;

        let estimated_input_tokens = estimated_input_tokens(&request);
        let mut capability_observation =
            CapabilityObservation::new(context.observer.clone(), route, estimated_input_tokens);

        let selector = RouteSelector::new(
            self.storage.clone(),
            self.cache_affinity.clone(),
            self.generation_chains.continuation_lookup(),
            self.route_policy_state.clone(),
        );
        let mut policy = selector
            .select_independent(
                principal,
                route,
                estimated_input_tokens,
                context.observer.as_ref(),
            )
            .await
            .map_err(selection_error)?;
        let mut last_error = None;
        // A Route owns one admission lease per Vendor it has actually reached.
        // Later Targets for that Vendor freeze new connection snapshots under
        // the same component and cancellation boundary; untouched Vendors stay lazy.
        let mut vendor_leases: BTreeMap<String, PreparedVendorExecution> = BTreeMap::new();

        while let Some(target) = policy.next_healthy() {
            let mut prepared: Option<PreparedVendorExecution> = None;
            loop {
                if !policy.retry_current() {
                    break;
                }
                let provider = tokio::select! {
                    biased;
                    _ = any_vendor_cancelled(&vendor_leases) => {
                        return Err(RuntimeError::Cancelled.into());
                    }
                    result = self.storage.providers().get(target.provider_id().as_str()) => result?,
                }
                .ok_or_else(|| anyhow::anyhow!("Target Provider is unavailable"))?;
                let attempt = target.model().map(|model| {
                    AttemptObservation::new(
                        context.observer.clone(),
                        capability_observation.id.clone(),
                        selected_target_key(&target),
                        provider.id.clone(),
                        provider.name.clone(),
                        model.clone().into(),
                        provider.protocol.clone(),
                        provider.base_url.clone(),
                        None,
                    )
                });
                let preparation_failure = if prepared.is_none() {
                    let vendor_id = provider
                        .vendor
                        .as_deref()
                        .map(str::trim)
                        .filter(|vendor| !vendor.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("Target Provider has no Vendor plugin"))?;
                    let existing_lease = vendor_leases.get(&vendor_id).cloned();
                    let preparation = tokio::select! {
                        biased;
                        _ = any_vendor_cancelled(&vendor_leases) => {
                            Err(RuntimeError::Cancelled.into())
                        }
                        result = async {
                            if let Some(lease) = existing_lease.as_ref() {
                                self.prepare_vendor_execution_with_lease(
                                    lease,
                                    target.provider_id().as_str(),
                                    target.model().map(|model| model.as_str()),
                                    operation,
                                    &context,
                                )
                                .await
                            } else {
                                self.prepare_vendor_execution(
                                    target.provider_id().as_str(),
                                    target.model().map(|model| model.as_str()),
                                    operation,
                                    &context,
                                )
                                .await
                            }
                        } => result,
                    };
                    match preparation {
                        Ok(execution) => {
                            vendor_leases
                                .entry(execution.vendor_id().to_owned())
                                .or_insert_with(|| execution.clone());
                            prepared = Some(execution);
                            None
                        }
                        Err(error) => Some(RouteFailure::new(error, false)),
                    }
                } else {
                    None
                };
                let result = if let Some(failure) = preparation_failure {
                    Err(failure)
                } else {
                    let mut attempt_context = clone_context(&context);
                    if let Some(attempt) = &attempt {
                        attempt_context.model_turn_id = Some(capability_observation.id.clone());
                        attempt_context.attempt_id = Some(attempt.id.clone());
                    }
                    self.execute_independent_attempt(
                        prepared
                            .as_ref()
                            .expect("prepared execution exists after admission")
                            .clone(),
                        clone_request(&request),
                        attempt_context,
                        &vendor_leases,
                    )
                    .await
                };
                match result {
                    Ok(execution) => {
                        let output_matches = matches!(
                            (&request, &execution.output),
                            (VendorRequest::Search(_), OperationOutput::Search(_))
                                | (VendorRequest::MediaImage(_), OperationOutput::MediaImage(_))
                        );
                        if !output_matches {
                            if let Some(attempt) = &attempt {
                                attempt.finish(
                                    "failed",
                                    None,
                                    Some("vendor_output_invalid".into()),
                                    None,
                                );
                            }
                            anyhow::bail!("Vendor returned an output for the wrong operation");
                        }
                        if let Some(usage) = operation_usage(&execution.output)
                            && let Some(attempt) = &attempt
                        {
                            attempt.confirm_usage(usage);
                        }
                        if let Some(attempt) = &attempt {
                            attempt.finish("completed", None, None, None);
                        }
                        let target_id = selected_target_key(&target);
                        policy.state().record_success(
                            policy.context(),
                            &target_id,
                            policy.current_epoch(),
                        );
                        policy.accept_current();
                        capability_observation.finish("completed");
                        let (provider_id, upstream_model) = target.destination.into_parts();
                        return Ok(VendorRouteExecution {
                            output: execution.output,
                            publication: execution.publication,
                            provider_id: provider_id.into(),
                            upstream_model: upstream_model.map(Into::into),
                            target_id,
                        });
                    }
                    Err(failure) => {
                        // ADR-0073：上游确认的凭据拒绝按执行快照的凭据代际
                        // 条件写 Provider 失效；失败只记 warn，不影响重试决策。
                        if let Some(execution) = &prepared
                            && crate::plugin::execution::is_credential_rejection(&failure.error)
                        {
                            self.mark_provider_credential_invalid(
                                target.provider_id().as_str(),
                                execution.credential_version(),
                            )
                            .await;
                        }
                        if let Some(attempt) = &attempt {
                            attempt.finish(
                                "failed",
                                failure
                                    .error
                                    .downcast_ref::<RuntimeError>()
                                    .and_then(RuntimeError::upstream_status),
                                Some(failure.code.to_owned()),
                                None,
                            );
                        }
                        let Some(kind) = failure.retry_kind else {
                            return Err(failure.error);
                        };
                        let disposition = policy.record_failure(
                            &target,
                            AttemptFailureSignal {
                                kind,
                                client_output_committed: false,
                                retry_after: failure.retry_after,
                                now_ms: self.route_policy_state.now_ms(),
                                jitter_sample: rand::random(),
                            },
                        );
                        last_error = Some(failure.error);
                        match disposition {
                            AttemptFailureDisposition::RetrySame { delay } => {
                                wait_for_retry(delay, &context, &vendor_leases).await?;
                            }
                            AttemptFailureDisposition::TryNextTarget => break,
                            AttemptFailureDisposition::Stop => {
                                return Err(last_error.expect("failed attempt"));
                            }
                        }
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Route has no eligible Target")))
    }

    async fn execute_independent_attempt(
        &self,
        prepared: PreparedVendorExecution,
        request: VendorRequest,
        mut context: VendorCallContext,
        route_leases: &BTreeMap<String, PreparedVendorExecution>,
    ) -> Result<crate::plugin::VendorExecution, RouteFailure> {
        let (events, mut receiver) = tokio::sync::mpsc::channel(32);
        context.events = Some(events);
        let execution = self.execute_prepared_vendor(prepared, request, context);
        tokio::pin!(execution);
        let mut result = None;
        let mut upstream_started = false;
        let mut event_failure = None;

        loop {
            tokio::select! {
                biased;
                _ = any_vendor_cancelled(route_leases) => {
                    result = Some(Err(RuntimeError::Cancelled.into()));
                    break;
                }
                event = receiver.recv() => match event {
                    Some(event) => consume_independent_event(
                        event,
                        &mut upstream_started,
                        &mut event_failure,
                    ),
                    None => break,
                },
                completed = &mut execution => {
                    result = Some(completed);
                    break;
                }
            }
        }
        if result.is_none() {
            result = Some(execution.await);
        }
        while let Ok(event) = receiver.try_recv() {
            consume_independent_event(event, &mut upstream_started, &mut event_failure);
        }
        let result = result
            .expect("Vendor execution result")
            .and_then(|execution| {
                if let Some(error) = event_failure {
                    Err(error)
                } else {
                    Ok(execution)
                }
            });
        result.map_err(|error| RouteFailure::new(error, upstream_started))
    }
}

fn consume_independent_event(
    event: VendorEvent,
    upstream_started: &mut bool,
    failure: &mut Option<anyhow::Error>,
) {
    if event.publication.ensure_current().is_err() {
        failure.get_or_insert_with(|| anyhow::anyhow!("Vendor result can no longer be published"));
        return;
    }
    match event.event {
        RuntimeEvent::UpstreamStarted => *upstream_started = true,
        RuntimeEvent::Failed {
            kind,
            message,
            upstream_status,
        } => {
            failure.get_or_insert_with(|| {
                anyhow::Error::new(RuntimeError::from_guest(kind, message, upstream_status))
            });
        }
        _ => {
            failure.get_or_insert_with(|| {
                anyhow::anyhow!("Vendor emitted an invalid event for an independent operation")
            });
        }
    }
}

struct RouteFailure {
    error: anyhow::Error,
    retry_kind: Option<AiErrorKind>,
    retry_after: Option<Duration>,
    code: &'static str,
}

impl RouteFailure {
    fn new(error: anyhow::Error, upstream_started: bool) -> Self {
        let runtime = error.downcast_ref::<RuntimeError>();
        let upstream_status = runtime.and_then(RuntimeError::upstream_status);
        let retry_kind = runtime
            .filter(|runtime| upstream_started && runtime.is_upstream_failure())
            .and_then(RuntimeError::model_error_kind)
            .filter(|kind| {
                let safe_kind = kind.is_retryable() || *kind == AiErrorKind::QuotaExceeded;
                let safe_status = !kind.is_retryable()
                    || upstream_status
                        .is_none_or(|status| matches!(status, 408 | 429 | 500 | 502 | 503 | 529));
                safe_kind && safe_status
            });
        let retry_after = runtime.and_then(RuntimeError::retry_after);
        let code = match runtime {
            Some(RuntimeError::Cancelled)
            | Some(RuntimeError::Plugin {
                kind: stravia_vendor_sdk::ErrorKind::Cancelled,
                ..
            }) => "cancelled",
            Some(RuntimeError::DeadlineExceeded)
            | Some(RuntimeError::Plugin {
                kind: stravia_vendor_sdk::ErrorKind::DeadlineExceeded,
                ..
            }) => "deadline_exceeded",
            _ if retry_kind.is_some() => "vendor_upstream_failed",
            _ => "vendor_operation_failed",
        };
        Self {
            error,
            retry_kind,
            retry_after,
            code,
        }
    }
}

fn operation_usage(
    output: &OperationOutput,
) -> Option<&stravia_runtime_contract::protocol::ir::Usage> {
    match output {
        OperationOutput::Search(response) => response.usage.as_ref(),
        OperationOutput::MediaImage(response) => response.usage.as_ref(),
        _ => None,
    }
}

fn clone_request(request: &VendorRequest) -> VendorRequest {
    request.clone()
}

fn clone_context(context: &VendorCallContext) -> VendorCallContext {
    let mut cloned = VendorCallContext::new(context.cancellation.clone(), context.deadline.clone());
    cloned.observer = context.observer.clone();
    cloned.model_turn_id = context.model_turn_id.clone();
    cloned.attempt_id = context.attempt_id.clone();
    cloned.websocket_affinity = context.websocket_affinity.clone();
    cloned.response_continuation_available = context.response_continuation_available.clone();
    cloned.client_headers = context.client_headers.clone();
    cloned.metadata = context.metadata.clone();
    cloned
}

fn estimated_input_tokens(request: &VendorRequest) -> u64 {
    let bytes = match request {
        VendorRequest::Search(request) => request.query.len(),
        VendorRequest::MediaImage(request) => request
            .references
            .iter()
            .fold(request.prompt.len(), |total, reference| {
                total.saturating_add(reference.bytes.len())
            }),
        _ => 0,
    };
    bytes.div_ceil(4) as u64
}

async fn wait_for_retry(
    delay: Duration,
    context: &VendorCallContext,
    vendor_leases: &BTreeMap<String, PreparedVendorExecution>,
) -> anyhow::Result<()> {
    tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => Err(RuntimeError::Cancelled.into()),
        _ = any_vendor_cancelled(vendor_leases) => Err(RuntimeError::Cancelled.into()),
        () = context.deadline.wait() => {
            Err(RuntimeError::DeadlineExceeded.into())
        }
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

async fn any_vendor_cancelled(vendor_leases: &BTreeMap<String, PreparedVendorExecution>) {
    if vendor_leases.is_empty() {
        std::future::pending::<()>().await;
    }
    let mut cancellations = vendor_leases
        .values()
        .map(PreparedVendorExecution::cancelled)
        .collect::<FuturesUnordered<_>>();
    let _ = cancellations.next().await;
}

fn selection_error(error: SelectionError) -> anyhow::Error {
    match error {
        SelectionError::SchedulingEvidence(source) => {
            source.context("Route scheduling is unavailable")
        }
        SelectionError::NoEligibleTarget => anyhow::anyhow!("Route has no eligible Target"),
        SelectionError::MediaPlanExhausted => anyhow::anyhow!("Route has no eligible Target"),
    }
}
