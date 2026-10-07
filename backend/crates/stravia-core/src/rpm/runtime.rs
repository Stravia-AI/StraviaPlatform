use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use stravia_runtime_contract::{CancellationToken, Deadline};
use tokio::time::Instant;

use super::RpmConfig;

const WINDOW: Duration = Duration::from_secs(60);

tokio::task_local! {
    static ROOT_REQUEST: RootRequest;
}

pub(crate) fn current_root_request() -> RootRequest {
    ROOT_REQUEST.try_with(Clone::clone).unwrap_or_default()
}

pub(crate) async fn scope_root_request<F: Future>(root: RootRequest, future: F) -> F::Output {
    ROOT_REQUEST.scope(root, future).await
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DestinationKey(String, String);

impl DestinationKey {
    pub(crate) fn for_target(target: &crate::router::selector::SelectedTarget) -> Self {
        Self::for_destination(target.provider_id().as_str(), target.model().as_str())
    }

    fn for_destination(provider_id: &str, model: &str) -> Self {
        Self(provider_id.to_owned(), model.to_owned())
    }
}

#[derive(Clone, Default)]
pub(crate) struct RootRequest {
    wait: Arc<Mutex<RootWait>>,
    pub(crate) cooldown: crate::router::selector::CooldownAttempts,
}

#[derive(Default)]
struct RootWait {
    total: Duration,
    preferred: Duration,
    waiters: usize,
    started: Option<Instant>,
    preferred_waiters: usize,
    preferred_started: Option<Instant>,
}

#[cfg(test)]
type ValidationGate = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

#[derive(Clone, Default)]
pub(crate) struct TargetAdmission {
    inner: Arc<Mutex<State>>,
    changed: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(crate) wait_started: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(crate) validation_gate: Arc<Mutex<Option<ValidationGate>>>,
    #[cfg(test)]
    pub(crate) capture_send: Arc<Mutex<Option<tokio::sync::oneshot::Sender<SendAdmission>>>>,
}

#[derive(Default)]
struct State {
    config: RpmConfig,
    limits: HashMap<DestinationKey, i32>,
    windows: HashMap<DestinationKey, VecDeque<Instant>>,
    waiting_roots: usize,
    version: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum AdmissionError {
    Busy,
    Rejected(stravia_runtime_contract::model_turn::ModelTurnError),
    Exhausted { retry_after: Option<Duration> },
    QueueFull,
    Cancelled,
    Deadline,
}

impl AdmissionError {
    pub(crate) fn model_error(&self) -> stravia_runtime_contract::model_turn::ModelTurnError {
        use stravia_runtime_contract::model_turn::ModelTurnError;
        match self {
            Self::Rejected(error) => error.clone(),
            Self::Busy => ModelTurnError::new(
                "target_rpm_busy",
                "Target RPM allowance changed before sending",
            ),
            Self::Exhausted { retry_after } => {
                let mut error = ModelTurnError::new(
                    "target_rpm_exceeded",
                    "Target RPM admission wait budget exhausted",
                );
                error.retry_after_secs =
                    retry_after.map(|wait| wait.as_secs() + u64::from(wait.subsec_nanos() != 0));
                error
            }
            Self::QueueFull => {
                ModelTurnError::new("target_rpm_queue_full", "Target RPM waiting queue is full")
            }
            Self::Cancelled => ModelTurnError::new("cancelled", "Target RPM admission cancelled"),
            Self::Deadline => ModelTurnError::new(
                "deadline_exceeded",
                "Target RPM admission deadline exceeded",
            ),
        }
    }
}

impl TargetAdmission {
    /// Retains healthy, non-excluded candidates, then decides preferred/general RPM waiting.
    pub(crate) fn filter_candidates(
        &self,
        policy: &mut crate::router::RouteAttemptPolicy,
        root: &RootRequest,
        excluded: &std::collections::HashSet<String>,
    ) -> Option<(Instant, bool)> {
        policy.retain_eligible(excluded);
        let mut state = self.inner.lock();
        let now = Instant::now();
        let preferred_remaining = {
            let wait = root.wait.lock();
            let used = wait.preferred
                + wait
                    .preferred_started
                    .map_or(Duration::ZERO, |started| now - started);
            Duration::from_millis(state.config.preferred_wait_ms).saturating_sub(used)
        };
        if !preferred_remaining.is_zero()
            && let Some(preferred) = policy.preferred_target_key()
            && let Some(target) = policy
                .candidates()
                .iter()
                .find(|target| crate::router::selected_target_key(target) == preferred)
            && let Some(next) = Self::next_at(&mut state, &DestinationKey::for_target(target), now)
        {
            return Some((next, true));
        }
        let mut next = None;
        policy.retain(|target| {
            if let Some(at) = Self::next_at(&mut state, &DestinationKey::for_target(target), now) {
                next = Some(next.map_or(at, |previous: Instant| previous.min(at)));
                false
            } else {
                true
            }
        });
        if policy.is_empty() {
            next.map(|next| (next, false))
        } else {
            None
        }
    }

    pub(crate) fn configure(&self, config: RpmConfig) {
        let mut state = self.inner.lock();
        let mut limits = HashMap::new();
        for destination in &config.destinations {
            if let Some(limit) = destination.rpm_limit {
                limits.insert(
                    DestinationKey(destination.provider_id.clone(), destination.model.clone()),
                    limit,
                );
            }
        }
        // 不限期间不记录历史；只有始终受限的目的地保留窗口，数值调整不返还发送额度。
        state.windows.retain(|key, _| limits.contains_key(key));
        state.limits = limits;
        state.config = config;
        state.version += 1;
        drop(state);
        self.changed.notify_waiters();
    }

    pub(crate) fn targets_changed(&self) {
        self.inner.lock().version += 1;
        self.changed.notify_waiters();
    }

    fn next_at(state: &mut State, key: &DestinationKey, now: Instant) -> Option<Instant> {
        let &limit = state.limits.get(key)?;
        let window = state.windows.get_mut(key)?;
        while window
            .front()
            .is_some_and(|at| now.duration_since(*at) >= WINDOW)
        {
            window.pop_front();
        }
        if window.len() < limit as usize {
            return None;
        }
        // 降低上限时可能需多个记录离窗，而非仅等待最早一条。
        Some(window[window.len() - limit as usize] + WINDOW)
    }

    pub(crate) fn version(&self) -> u64 {
        self.inner.lock().version
    }

    fn record_send(state: &mut State, key: &DestinationKey, now: Instant) -> Result<(), Instant> {
        if let Some(at) = Self::next_at(state, key, now) {
            return Err(at);
        }
        if state.limits.contains_key(key) {
            if let Some(window) = state.windows.get_mut(key) {
                window.push_back(now);
            } else {
                state.windows.insert(key.clone(), VecDeque::from([now]));
            }
        }
        Ok(())
    }

    /// 等待一次真正可能改变资格的事件或时间点；每次醒来重新竞争，不预扣额度。
    pub(crate) async fn wait(
        &self,
        root: &RootRequest,
        next: Instant,
        preferred: bool,
        cancellation: &CancellationToken,
        deadline: &Deadline,
        observed_version: u64,
    ) -> Result<(), AdmissionError> {
        let notified = self.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let guard = {
            let mut state = self.inner.lock();
            let mut wait = root.wait.lock();
            let now = Instant::now();
            let total_used =
                wait.total + wait.started.map_or(Duration::ZERO, |started| now - started);
            let total_left =
                Duration::from_millis(state.config.total_wait_ms).saturating_sub(total_used);
            if total_left.is_zero() {
                return Err(AdmissionError::Exhausted {
                    retry_after: Some(next.saturating_duration_since(now)),
                });
            }
            // 配置或绑定可能在资格检查与注册通知之间更新，不能错过唤醒。
            if state.version != observed_version {
                return Ok(());
            }
            let preferred_used = wait.preferred
                + wait
                    .preferred_started
                    .map_or(Duration::ZERO, |started| now - started);
            let preferred_left = Duration::from_millis(state.config.preferred_wait_ms)
                .saturating_sub(preferred_used);
            if preferred && preferred_left.is_zero() {
                return Ok(());
            }
            if wait.waiters == 0 {
                if state.waiting_roots >= state.config.queue_capacity {
                    return Err(AdmissionError::QueueFull);
                }
                state.waiting_roots += 1;
                wait.started = Some(now);
            }
            wait.waiters += 1;
            if preferred {
                if wait.preferred_waiters == 0 {
                    wait.preferred_started = Some(now);
                }
                wait.preferred_waiters += 1;
            }
            WaitGuard {
                admission: self.clone(),
                root: root.clone(),
                preferred,
                until: now
                    + if preferred {
                        total_left.min(preferred_left)
                    } else {
                        total_left
                    },
            }
        };
        #[cfg(test)]
        self.wait_started.notify_one();
        let until = next.min(guard.until);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AdmissionError::Cancelled),
            () = deadline.wait() => Err(AdmissionError::Deadline),
            () = notified => Ok(()),
            () = tokio::time::sleep_until(until) => Ok(()),
        };
        drop(guard);
        result?;
        let state = self.inner.lock();
        let wait = root.wait.lock();
        let used = wait.total
            + wait
                .started
                .map_or(Duration::ZERO, |started| started.elapsed());
        // 窗口释放与预算耗尽同时发生时，旧 root 不能获得一次迟到发送。
        if used >= Duration::from_millis(state.config.total_wait_ms) {
            return Err(AdmissionError::Exhausted {
                retry_after: Some(next.saturating_duration_since(Instant::now())),
            });
        }
        Ok(())
    }
}

struct WaitGuard {
    admission: TargetAdmission,
    root: RootRequest,
    preferred: bool,
    until: Instant,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let mut state = self.admission.inner.lock();
        let mut wait = self.root.wait.lock();
        let now = Instant::now();
        if self.preferred {
            wait.preferred_waiters -= 1;
            if wait.preferred_waiters == 0
                && let Some(started) = wait.preferred_started.take()
            {
                wait.preferred += now - started;
            }
        }
        wait.waiters -= 1;
        if wait.waiters == 0 {
            if let Some(started) = wait.started.take() {
                wait.total += now - started;
            }
            state.waiting_roots -= 1;
        }
    }
}

enum SendDecision {
    Recheck,
    Admitted,
    Blocked(Instant),
}

/// 宿主 transport 在每次真实发送前调用；重试和复用连接不能绕过目的地额度。
#[derive(Clone)]
pub(crate) struct SendAdmission {
    pub(crate) admission: TargetAdmission,
    pub(crate) root: RootRequest,
    pub(crate) key: DestinationKey,
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Deadline,
    pub(crate) failure: Arc<Mutex<Option<AdmissionError>>>,
    pub(crate) sent: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) upstream_state: Option<Arc<std::sync::atomic::AtomicU8>>,
    pub(crate) eligibility: Option<SendEligibility>,
}

#[derive(Clone)]
pub(crate) struct SendEligibility {
    pub(crate) storage: crate::storage::DynStorage,
    pub(crate) routes: Arc<tokio::sync::RwLock<crate::router::RouteCache>>,
    pub(crate) admitted_component: stravia_vendor_runtime::LoadedPlugin,
    pub(crate) admitted_provider_id: String,
    pub(crate) route_id: String,
    pub(crate) target: crate::db::identity::TargetDestination,
    pub(crate) principal: stravia_runtime_contract::Principal,
    pub(crate) authorization: crate::model_turn::ModelTurnAuthorization,
    pub(crate) health: crate::router::RoutePolicyState,
    pub(crate) target_key: String,
    pub(crate) epoch: u64,
    pub(crate) single_attempt: bool,
    pub(crate) capability: stravia_vendor_sdk::Capability,
    pub(crate) requires_video: bool,
    pub(crate) requires_image: bool,
}

impl SendEligibility {
    async fn current_destination(&self) -> Result<DestinationKey, AdmissionError> {
        use stravia_runtime_contract::model_turn::ModelTurnError;
        let rejected = || {
            AdmissionError::Rejected(ModelTurnError::new(
                "target_ineligible",
                "Target is no longer eligible for sending",
            ))
        };
        let routes = self.routes.read().await;
        let route = routes
            .models
            .iter()
            .find(|route| route.id == self.route_id && route.is_enabled)
            .ok_or_else(rejected)?;
        let target = route
            .targets
            .iter()
            .find(|target| target.enabled && target.destination == self.target)
            .ok_or_else(rejected)?;
        let security = crate::proxy::security::Security::new(self.storage.auth());
        let access = match self.authorization {
            crate::model_turn::ModelTurnAuthorization::RouteBinding => {
                security
                    .authorize_principal_model(&self.principal, route)
                    .await
            }
            crate::model_turn::ModelTurnAuthorization::CapabilityGrant => {
                security
                    .authorize_principal_capability(&self.principal)
                    .await
            }
        };
        access.map_err(|error| {
            AdmissionError::Rejected(ModelTurnError::new(error.stable_code(), error.message()))
        })?;
        let destination =
            DestinationKey::for_destination(target.provider_id().as_str(), target.model().as_str());
        drop(routes);
        let provider = self
            .storage
            .providers()
            .get(self.target.provider_id().as_str())
            .await
            .map_err(|error| {
                AdmissionError::Rejected(ModelTurnError::new(
                    "provider_unavailable",
                    error.to_string(),
                ))
            })?
            .ok_or_else(rejected)?;
        if !provider.is_enabled || provider.credential_status == "invalid" {
            return Err(rejected());
        }
        if provider.vendor.as_deref().map(str::trim) != Some(self.admitted_provider_id.as_str()) {
            return Err(rejected());
        }
        let descriptor = self
            .admitted_component
            .descriptor()
            .provider(&self.admitted_provider_id)
            .ok_or_else(rejected)?;
        let channel = descriptor
            .channels
            .iter()
            .find(|channel| channel.id == provider.channel.as_deref().unwrap_or("default"))
            .ok_or_else(rejected)?;
        let model = self
            .storage
            .provider_models()
            .find(
                self.target.provider_id().as_str(),
                self.target.model().as_str(),
            )
            .await
            .map_err(|error| {
                AdmissionError::Rejected(ModelTurnError::new(
                    "model_unavailable",
                    error.to_string(),
                ))
            })?;
        crate::model_turn::validate_target_capability(
            channel,
            self.target.model().as_str(),
            model.as_ref(),
            self.capability,
        )
        .map_err(|_| rejected())?;
        {
            let metadata = model.as_ref().map(|model| &model.metadata);
            let capabilities = metadata
                .and_then(|metadata| metadata.extensions.get("capabilities"))
                .and_then(serde_json::Value::as_array);
            let supports = |capability: &str| {
                capabilities.is_some_and(|values| {
                    values
                        .iter()
                        .any(|value| value.as_str() == Some(capability))
                })
            };
            let modalities = metadata.map(|metadata| metadata.effective_modalities());
            if self.requires_video
                && !modalities.is_some_and(|modalities| {
                    modalities
                        .input
                        .iter()
                        .any(|input| input.eq_ignore_ascii_case("video"))
                })
            {
                return Err(rejected());
            }
            if self.requires_image
                && !(supports("image_input")
                    || modalities.is_some_and(|modalities| {
                        stravia_media::platform::supports_image(&modalities.input)
                    }))
            {
                return Err(rejected());
            }
        }
        Ok(destination)
    }
}

impl SendAdmission {
    pub(crate) async fn acquire(&self) -> Result<(), stravia_vendor_runtime::HostFailure> {
        let result = self.acquire_inner().await;
        result.map_err(|error| {
            let message = error.model_error().message;
            *self.failure.lock() = Some(error);
            stravia_vendor_runtime::HostFailure::new(
                stravia_vendor_sdk::ErrorKind::Invalid,
                message,
            )
        })
    }

    async fn acquire_inner(&self) -> Result<(), AdmissionError> {
        #[cfg(test)]
        if let Some(capture) = self.admission.capture_send.lock().take() {
            let _ = capture.send(self.clone());
        }
        loop {
            if self.cancellation.is_cancelled() {
                return Err(AdmissionError::Cancelled);
            }
            if self.deadline.is_exceeded() {
                return Err(AdmissionError::Deadline);
            }
            // Capture before any asynchronous eligibility read. A publication
            // during those reads invalidates both successful and rejected results.
            let version = self.admission.version();
            let current_key = match &self.eligibility {
                Some(eligibility) => eligibility.current_destination().await,
                None => Ok(self.key.clone()),
            };
            #[cfg(test)]
            {
                let gate = self.admission.validation_gate.lock().take();
                if let Some((entered, release)) = gate {
                    entered.notify_one();
                    release.notified().await;
                }
            }
            let routes = match &self.eligibility {
                Some(eligibility) => Some(eligibility.routes.read().await),
                None => None,
            };
            let admission = self.commit_send(version, current_key);
            drop(routes);
            match admission? {
                SendDecision::Recheck => continue,
                SendDecision::Admitted => {
                    *self.failure.lock() = None;
                    if let Some(state) = &self.upstream_state {
                        state.fetch_or(1, std::sync::atomic::Ordering::AcqRel);
                    }
                    return Ok(());
                }
                SendDecision::Blocked(_)
                    if !self.sent.load(std::sync::atomic::Ordering::Acquire) =>
                {
                    return Err(AdmissionError::Busy);
                }
                SendDecision::Blocked(next) => {
                    self.admission
                        .wait(
                            &self.root,
                            next,
                            false,
                            &self.cancellation,
                            &self.deadline,
                            version,
                        )
                        .await?
                }
            }
        }
    }

    fn commit_send(
        &self,
        version: u64,
        current_key: Result<DestinationKey, AdmissionError>,
    ) -> Result<SendDecision, AdmissionError> {
        let mut state = self.admission.inner.lock();
        if state.version != version {
            return Ok(SendDecision::Recheck);
        }
        let current_key = current_key?;
        if self.cancellation.is_cancelled() {
            return Err(AdmissionError::Cancelled);
        }
        if self.deadline.is_exceeded() {
            return Err(AdmissionError::Deadline);
        }
        if let Some(eligibility) = &self.eligibility {
            if !eligibility.health.permits_send(
                &eligibility.target_key,
                eligibility.epoch,
                eligibility.single_attempt,
            ) {
                return Err(if self.sent.load(std::sync::atomic::Ordering::Acquire) {
                    AdmissionError::Rejected(
                        stravia_runtime_contract::model_turn::ModelTurnError::new(
                            "target_ineligible",
                            "Target health changed before another upstream attempt",
                        ),
                    )
                } else {
                    AdmissionError::Busy
                });
            }
            if eligibility.single_attempt && self.sent.load(std::sync::atomic::Ordering::Acquire) {
                return Err(AdmissionError::Rejected(
                    stravia_runtime_contract::model_turn::ModelTurnError::new(
                        "target_ineligible",
                        "Cooling Target permits only one extra upstream attempt",
                    ),
                ));
            }
        }
        // 缓存发布被读锁阻止，并发发送共用此临界区；一次机会的占用与扣额原子完成。
        match TargetAdmission::record_send(&mut state, &current_key, Instant::now()) {
            Ok(()) => {
                self.sent.store(true, std::sync::atomic::Ordering::Release);
                Ok(SendDecision::Admitted)
            }
            Err(next) => Ok(SendDecision::Blocked(next)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpm::DestinationRpmLimit;

    #[test]
    fn limit_reduction_retains_each_destination_window() {
        let admission = TargetAdmission::default();
        let config = |limit| RpmConfig {
            destinations: ["a", "b"]
                .into_iter()
                .map(|provider| DestinationRpmLimit {
                    provider_id: provider.into(),
                    model: "model".into(),
                    rpm_limit: Some(limit),
                })
                .collect(),
            ..Default::default()
        };
        let a = DestinationKey::for_destination("a", "model");
        let b = DestinationKey::for_destination("b", "model");
        let now = Instant::now();
        admission.configure(config(2));
        {
            let mut state = admission.inner.lock();
            TargetAdmission::record_send(&mut state, &a, now).unwrap();
            TargetAdmission::record_send(&mut state, &b, now).unwrap();
            TargetAdmission::record_send(&mut state, &a, now + Duration::from_secs(1)).unwrap();
        }
        admission.configure(config(1));
        {
            let mut state = admission.inner.lock();
            assert_eq!(
                TargetAdmission::next_at(&mut state, &a, now + Duration::from_secs(1)),
                Some(now + Duration::from_secs(1) + WINDOW)
            );
            assert_eq!(
                TargetAdmission::next_at(&mut state, &b, now),
                Some(now + WINDOW)
            );
            assert_eq!(
                TargetAdmission::record_send(&mut state, &a, now + WINDOW),
                Err(now + Duration::from_secs(1) + WINDOW)
            );
            TargetAdmission::record_send(&mut state, &b, now + WINDOW).unwrap();
        }
    }
}
