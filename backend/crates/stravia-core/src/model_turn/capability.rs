use std::collections::BTreeMap;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::{AiError, AiErrorKind};
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

use super::provider::{AttemptCostContext, AttemptObservation};

pub(crate) struct VendorRouteExecution {
    pub(crate) output: OperationOutput,
    pub(crate) publication: VendorPublicationFence,
    pub(crate) provider_id: String,
    pub(crate) upstream_model: String,
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
                retention_detached: false,
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
                    retention_detached: false,
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
    /// Re-read both the Route and raw model declaration at the send boundary.
    /// Prepared executions and cached selection hints never grant eligibility.
    pub(crate) async fn route_target_output_matches(
        &self,
        route_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<bool> {
        let route = self
            .storage
            .routes()
            .get(route_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Route is unavailable"))?;
        let modality = route.output_modality.ok_or_else(|| {
            anyhow::anyhow!(
                "Route output modality is unconfigured; select and save an output modality"
            )
        })?;
        if !route.is_enabled
            || !route.targets.iter().any(|target| {
                target.enabled
                    && target.provider_id().as_str() == provider_id
                    && target.model().as_str() == model_id
            })
        {
            return Ok(false);
        }
        Ok(self
            .storage
            .provider_models()
            .find(provider_id, model_id)
            .await?
            .is_some_and(|model| model.metadata.supports_route_output_modality(modality)))
    }

    /// Output mismatches leave scheduling eligibility first. Every remaining
    /// enabled destination must still pass the existing adapter checks.
    pub(crate) async fn validate_vendor_route_capability(
        &self,
        route: &RouteConfig,
        capability: Capability,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(route.is_enabled, "Route is disabled");
        let modality = route.output_modality.ok_or_else(|| {
            anyhow::anyhow!(
                "Route output modality is unconfigured; select and save an output modality"
            )
        })?;
        anyhow::ensure!(
            capability != Capability::MediaImage
                || modality == crate::db::models::RouteOutputModality::Image,
            "Image generation requires an image-output Route"
        );
        anyhow::ensure!(
            route.targets.iter().any(|target| target.enabled),
            "Route has no enabled Target"
        );

        let mut matching_targets = 0;
        for target in route.targets.iter().filter(|target| target.enabled) {
            let model = self
                .storage
                .provider_models()
                .find(target.provider_id().as_str(), target.model().as_str())
                .await?;
            if !model
                .as_ref()
                .is_some_and(|model| model.metadata.supports_route_output_modality(modality))
            {
                continue;
            }
            matching_targets += 1;
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
            super::validate_target_capability(
                channel,
                target.model().as_str(),
                model.as_ref(),
                capability,
            )?;
        }
        anyhow::ensure!(
            matching_targets > 0,
            "Route has no output-modality-matching Target"
        );
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
        let cancellation = context.cancellation.clone();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled.into()),
            result = async {
        let (capability, operation) = match &request {
            VendorRequest::Search(_) => (Capability::Search, Operation::Search),
            VendorRequest::MediaImage(_) => (Capability::MediaImage, Operation::MediaImage),
            _ => anyhow::bail!("independent Vendor Routes support only search and image media"),
        };
        self.validate_vendor_route_capability(route, capability)
            .await?;

        let requires_image =
            matches!(&request, VendorRequest::MediaImage(request) if !request.references.is_empty());
        let estimated_input_tokens = estimated_input_tokens(&request);
        let mut capability_observation =
            CapabilityObservation::new(context.observer.clone(), route, estimated_input_tokens);

        let selector = RouteSelector::new(
            self.storage.clone(),
            self.cache_affinity.clone(),
            self.generation_chains.continuation_lookup(),
            self.route_policy_state.clone(),
            self.custom_currency.clone(),
        );
        let mut last_error: Option<RouteFailure> = None;
        let mut vendor_leases: BTreeMap<String, PreparedVendorExecution> = BTreeMap::new();
        let mut excluded = std::collections::HashSet::new();
        'targets: loop {
            let mut policy = loop {
                let version = self.rpm_admission.version();
                let routes = self.model_cache.read().await;
                let Some(current_route) = routes
                    .models
                    .iter()
                    .find(|current| current.id == route.id && current.is_enabled)
                else {
                    break 'targets;
                };
                let selection = selector
                    .select_independent(
                        principal,
                        current_route,
                        estimated_input_tokens,
                        requires_image,
                        context.observer.as_ref(),
                        &context.root_request.cooldown,
                    )
                    .await;
                drop(routes);
                let mut policy = match selection {
                    Ok(policy) => policy,
                    Err(
                        SelectionError::NoEligibleTarget | SelectionError::MixedAllowanceExclusions,
                    ) if last_error.is_some() => break 'targets,
                    Err(error) => return Err(selection_error(error)),
                };
                let wait = self.rpm_admission.filter_candidates(
                    &mut policy,
                    &context.root_request,
                    &excluded,
                );
                let Some((next, preferred)) = wait else {
                    if !policy.is_empty() {
                        break policy;
                    }
                    break 'targets;
                };
                if let Err(error) = self
                    .rpm_admission
                    .wait(
                        &context.root_request,
                        next,
                        preferred,
                        &context.cancellation,
                        &context.deadline,
                        version,
                    )
                    .await
                {
                    if matches!(
                        error,
                        crate::rpm::runtime::AdmissionError::Exhausted { .. }
                            | crate::rpm::runtime::AdmissionError::Deadline
                    ) && last_error.as_ref().is_some_and(|failure| {
                        failure.retry_kind == Some(AiErrorKind::QuotaExceeded)
                    }) {
                        break 'targets;
                    }
                    return Err(error.model_error().into());
                }
            };
            // A Route owns one admission lease per Vendor it has actually reached.
            // Later Targets for that Vendor freeze new connection snapshots under
            // the same component and cancellation boundary; untouched Vendors stay lazy.
            let Some(target) = policy.next_healthy() else {
                break;
            };
            let mut prepared: Option<PreparedVendorExecution> = None;
            let mut auth_recovered = false;
            loop {
                if context.cancellation.is_cancelled() {
                    return Err(RuntimeError::Cancelled.into());
                }
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
                let attempt = {
                    AttemptObservation::new(
                        context.observer.clone(),
                        capability_observation.id.clone(),
                        selected_target_key(&target),
                        provider.id.clone(),
                        provider.name.clone(),
                        target.model().clone().into(),
                        provider.protocol.clone(),
                        provider.base_url.clone(),
                        None,
                        AttemptCostContext {
                            pricing: target.pricing.clone(),
                            cost_basis: provider.cost_basis,
                            billing: route.billing.clone(),
                            api_key_id: Some(principal.api_key_id().to_owned()),
                            currency: self.custom_currency_snapshot(),
                        },
                    )
                    .with_usage_ledger(context.root_request.confirmed_usage.clone())
                };
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
                                    Some(target.model().as_str()),
                                    operation,
                                    &context,
                                )
                                .await
                            } else {
                                self.prepare_vendor_execution(
                                    target.provider_id().as_str(),
                                    Some(target.model().as_str()),
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
                if context.cancellation.is_cancelled() {
                    return Err(RuntimeError::Cancelled.into());
                }
                if !policy.retry_current() {
                    break;
                }
                if !self.route_target_output_matches(
                    &route.model_id,
                    target.provider_id().as_str(),
                    target.model().as_str(),
                ).await? {
                    break;
                }
                let sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let result = if let Some(failure) = preparation_failure {
                    Err(failure)
                } else {
                    let mut attempt_context = clone_context(&context);
                    attempt_context.send_admission = Some(crate::rpm::SendAdmission {
                        admission: self.rpm_admission.clone(),
                        root: context.root_request.clone(),
                        key: crate::rpm::DestinationKey::for_target(&target),
                        cancellation: context.cancellation.clone(),
                        deadline: context.deadline.clone(),
                        failure: std::sync::Arc::default(),
                        sent: sent.clone(),
                        upstream_state: None,
                        eligibility: Some(crate::rpm::runtime::SendEligibility {
                            storage: self.storage.clone(),
                            routes: self.model_cache.clone(),
                            admitted_component: prepared
                                .as_ref()
                                .expect("prepared execution exists after admission")
                                .pinned_component(),
                            admitted_provider_id: prepared
                                .as_ref()
                                .expect("prepared execution exists after admission")
                                .descriptor()
                                .provider_id
                                .clone(),
                            route_id: route.id.to_string(),
                            target: target.destination.clone(),
                            principal: principal.clone(),
                            authorization: super::ModelTurnAuthorization::CapabilityGrant,
                            health: policy.state().clone(),
                            target_key: selected_target_key(&target),
                            epoch: policy.current_epoch(),
                            single_attempt: policy.current_is_probe(),
                            capability,
                            requires_video: false,
                            requires_image,
                        }),
                    });
                    attempt_context.model_turn_id = Some(capability_observation.id.clone());
                    attempt_context.attempt_id = Some(attempt.id.clone());
                    self.execute_independent_attempt(
                        prepared
                            .as_mut()
                            .expect("prepared execution exists after admission"),
                        clone_request(&request),
                        attempt_context,
                        &vendor_leases,
                        &mut auth_recovered,
                        || {
                            policy.state().try_record_recovery_failure(
                                &selected_target_key(&target),
                                policy.current_epoch(),
                                target.target_retry_budget,
                                target.target_cooldown_ms,
                            )
                        },
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
                            // 本地协议契约失败，不是上游错误。
                            attempt.finish(
                                "failed",
                                None,
                                Some("vendor_output_invalid".into()),
                                None,
                                Some(&crate::interaction_observation::FailureDiagnostic {
                                    source: Some("platform".into()),
                                    code: Some("vendor_output_invalid".into()),
                                    message: Some(
                                        "Vendor returned an output for the wrong operation".into(),
                                    ),
                                    status_code: None,
                                    upstream_code: None,
                                }),
                            );
                            anyhow::bail!("Vendor returned an output for the wrong operation");
                        }
                        if let Some(usage) = operation_usage(&execution.output) {
                            attempt.confirm_usage(usage);
                        } else {
                            attempt.record_unreported_usage();
                        }
                        attempt.finish("completed", None, None, None, None);
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
                            upstream_model: upstream_model.into(),
                            target_id,
                        });
                    }
                    Err(failure) => {
                        if let Some(error) = failure.error.downcast_ref::<super::ModelTurnError>()
                            && matches!(
                                error.code.as_str(),
                                "target_rpm_busy" | "target_ineligible" | "target_output_modality_mismatch"
                            )
                        {
                            if !sent.load(std::sync::atomic::Ordering::Acquire) {
                                policy.release_unsent_exception();
                            }
                            // 未发送的本地失败：来源固定为 platform，不能计入上游错误。
                            attempt.finish(
                                "failed",
                                None,
                                Some(error.code.clone()),
                                None,
                                Some(&crate::interaction_observation::FailureDiagnostic {
                                    source: Some("platform".into()),
                                    code: Some(error.code.clone()),
                                    message: Some(error.message.clone()),
                                    status_code: None,
                                    upstream_code: None,
                                }),
                            );
                            policy.skip_current();
                            if matches!(error.code.as_str(), "target_ineligible" | "target_output_modality_mismatch") {
                                excluded.insert(selected_target_key(&target));
                                last_error = Some(failure);
                            }
                            continue 'targets;
                        }
                        // ADR-0073：上游确认的凭据拒绝按执行快照的凭据代际
                        // 条件写 Provider 失效；失败只记 warn，不影响重试决策。
                        if let Some(execution) = &prepared
                            && execution.oauth_connection_id().is_none()
                            && crate::plugin::execution::is_credential_rejection(&failure.error)
                        {
                            self.mark_provider_credential_invalid(
                                target.provider_id().as_str(),
                                execution.credential_version(),
                            )
                            .await;
                        }
                        let diagnostic = failure.diagnostic();
                        attempt.finish(
                            "failed",
                            diagnostic.status_code,
                            Some(failure.code.to_owned()),
                            None,
                            Some(&diagnostic),
                        );
                        let Some(kind) = failure.retry_kind.clone() else {
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
                        let retry_after = failure.retry_after;
                        last_error = Some(failure);
                        match disposition {
                            AttemptFailureDisposition::RetrySame { delay } => {
                                // Retry-After 在首 Token 窗口内时完整等待；
                                // 总 deadline 放不下时保留上游错误并切换 Target。
                                if retry_after.is_some() && delay >= context.deadline.remaining() {
                                    policy.skip_current();
                                    break;
                                }
                                wait_for_retry(delay, &context, &vendor_leases).await?;
                            }
                            AttemptFailureDisposition::TryNextTarget => break,
                            AttemptFailureDisposition::Stop => {
                                return Err(last_error.expect("failed attempt").error);
                            }
                        }
                    }
                }
            }
            excluded.insert(selected_target_key(&target));
        }

        Err(last_error
            .map(|failure| failure.error)
            .unwrap_or_else(|| anyhow::anyhow!("Route has no eligible Target")))
            } => result,
        }
    }

    async fn execute_independent_attempt(
        &self,
        prepared: &mut PreparedVendorExecution,
        request: VendorRequest,
        mut context: VendorCallContext,
        route_leases: &BTreeMap<String, PreparedVendorExecution>,
        auth_recovered: &mut bool,
        allow_recovery: impl FnOnce() -> bool,
    ) -> Result<crate::plugin::VendorExecution, RouteFailure> {
        if prepared.oauth_connection_id().is_none() {
            return self
                .execute_independent_attempt_once(
                    prepared.clone(),
                    request,
                    &mut context,
                    route_leases,
                )
                .await;
        }
        match self
            .execute_independent_attempt_once(
                prepared.clone(),
                request.clone(),
                &mut context,
                route_leases,
            )
            .await
        {
            Ok(execution) => return Ok(execution),
            Err(failure) if crate::plugin::execution::is_credential_rejection(&failure.error) => {
                if *auth_recovered || !allow_recovery() {
                    self.mark_provider_credential_invalid(
                        prepared.provider_id(),
                        prepared.credential_version(),
                    )
                    .await;
                    return Err(failure);
                }
            }
            Err(failure) => return Err(failure),
        }
        self.admin()
            .recover_provider_auth_with_lease(
                prepared.provider_id(),
                prepared,
                context.cancellation.clone(),
                context.deadline.clone(),
            )
            .await
            .map_err(|error| RouteFailure::new(error, true))?;
        *auth_recovered = true;
        let refreshed = self
            .prepare_vendor_execution_with_lease(
                prepared,
                prepared.provider_id(),
                prepared.provider().model.as_deref(),
                request.operation(),
                &context,
            )
            .await
            .map_err(|error| RouteFailure::new(error, false))?;
        // Keep the refreshed connection snapshot for ordinary retries too, while
        // retaining the Target's original component lease and recovery boundary.
        *prepared = refreshed;
        // 独立能力只在完整结果返回后提交，恢复可重发，但不重放失败事件。
        let result = self
            .execute_independent_attempt_once(prepared.clone(), request, &mut context, route_leases)
            .await;
        if let Err(failure) = &result
            && crate::plugin::execution::is_credential_rejection(&failure.error)
        {
            self.mark_provider_credential_invalid(
                prepared.provider_id(),
                prepared.credential_version(),
            )
            .await;
        }
        result
    }

    async fn execute_independent_attempt_once(
        &self,
        prepared: PreparedVendorExecution,
        request: VendorRequest,
        context: &mut VendorCallContext,
        route_leases: &BTreeMap<String, PreparedVendorExecution>,
    ) -> Result<crate::plugin::VendorExecution, RouteFailure> {
        let (events, mut receiver) = tokio::sync::mpsc::channel(32);
        let admission_failure = context
            .send_admission
            .as_ref()
            .map(|admission| admission.failure.clone());
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
        if let Some(failure) = admission_failure.and_then(|failure| failure.lock().take()) {
            return Err(RouteFailure::new(failure.model_error().into(), false));
        }
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
    /// 上游请求是否已经发出。取消、未发送的本地失败不得计入上游错误。
    upstream_started: bool,
}

impl RouteFailure {
    fn new(error: anyhow::Error, upstream_started: bool) -> Self {
        let runtime = error.downcast_ref::<RuntimeError>();
        let upstream_status = runtime.and_then(RuntimeError::upstream_status);
        let retry_kind = runtime
            .filter(|runtime| upstream_started && runtime.is_upstream_failure())
            .map(|runtime| {
                runtime.model_error_kind().unwrap_or_else(|| {
                    upstream_status
                        .map(|status| AiError::kind_from_status(status, None))
                        .unwrap_or(AiErrorKind::Unknown)
                })
            })
            // 上下文超限与内容过滤是请求级错误，换 Target 只会重复同一失败。
            .filter(|kind| {
                !matches!(
                    kind,
                    AiErrorKind::ContextLengthExceeded | AiErrorKind::ContentFiltered
                )
            })
            .map(|kind| {
                // 错误类别可能合并不同 HTTP 状态；同 Target 重试范围不能因此扩大，
                // 但仍然是上游失败，按非瞬时失败直接换 Target。
                let transient_status = upstream_status
                    .is_none_or(|status| matches!(status, 408 | 429 | 500 | 502 | 503 | 529));
                if kind.is_retryable() && !transient_status {
                    AiErrorKind::Unknown
                } else {
                    kind
                }
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
            upstream_started,
        }
    }

    /// 尝试行错误来源。上游可用率只统计真正由上游产生、或上游已开始后
    /// 超时的失败；本地准备失败、未发送与取消保持 platform。
    /// `retry_kind` 会过滤请求级 4xx 且不表达取消，不能作为来源判定。
    fn diagnostic(&self) -> crate::interaction_observation::FailureDiagnostic {
        use stravia_vendor_sdk::ErrorKind;
        let runtime = self.error.downcast_ref::<RuntimeError>();
        let upstream = runtime.is_some_and(|runtime| {
            runtime.is_upstream_failure()
                || crate::plugin::execution::is_credential_rejection(&self.error)
                || (self.upstream_started
                    && matches!(
                        runtime,
                        RuntimeError::DeadlineExceeded
                            | RuntimeError::Plugin {
                                kind: ErrorKind::DeadlineExceeded,
                                ..
                            }
                    ))
        });
        crate::interaction_observation::FailureDiagnostic {
            source: Some(if upstream { "upstream" } else { "platform" }.into()),
            code: Some(self.code.into()),
            message: Some(
                runtime
                    .and_then(RuntimeError::diagnostic_message)
                    .map_or_else(|| self.error.to_string(), str::to_owned),
            ),
            status_code: runtime.and_then(RuntimeError::upstream_status),
            upstream_code: None,
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
    cloned.root_request = context.root_request.clone();
    cloned.send_admission = context.send_admission.clone();
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
        SelectionError::UnconfiguredRoute => anyhow::anyhow!(
            "Route output modality is unconfigured; select and save an output modality"
        ),
        SelectionError::SchedulingEvidence(source) => {
            source.context("Route scheduling is unavailable")
        }
        SelectionError::NoEligibleTarget => anyhow::anyhow!("Route has no eligible Target"),
        SelectionError::MixedAllowanceExclusions => {
            anyhow::Error::new(super::mixed_allowance_exclusions_error())
        }
        SelectionError::AllowanceSuspended => {
            anyhow::Error::new(super::allowance_suspended_error())
        }
        SelectionError::MediaPlanExhausted => anyhow::anyhow!("Route has no eligible Target"),
    }
}
