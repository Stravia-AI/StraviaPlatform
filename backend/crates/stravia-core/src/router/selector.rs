//! Layered Route Target selection and failure policy.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::db::models::{RouteSelectionStrategy, Target};
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::ProtocolExt;

#[derive(Debug, Clone)]
pub struct SelectedTarget {
    pub provider_id: String,
    pub model: String,
    pub priority: i32,
    pub first_token_timeout_ms: i64,
    pub target_retry_budget: i32,
    pub target_cooldown_ms: i64,
    pub thinking_level_map: Vec<crate::thinking::ThinkingLevelMapping>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttemptFailureDisposition {
    RetrySame { delay: Duration },
    TryNextTarget,
    Stop,
}

#[derive(Debug, Clone)]
pub struct AttemptFailureSignal {
    pub kind: AiErrorKind,
    pub client_output_committed: bool,
    pub retry_after: Option<Duration>,
    pub now_ms: u64,
    pub jitter_sample: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationIdentity {
    GenerationParent(String),
    PromptCacheKey(String),
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct TargetSchedulingSnapshot {
    pub(super) target_key: String,
    pub(super) input_tokens_24h: Option<i64>,
    pub(super) output_tokens_24h: Option<i64>,
    pub(super) cache_read_tokens_24h: Option<i64>,
    pub(super) cache_write_tokens_24h: Option<i64>,
    pub(super) attempts_1h: i64,
    pub(super) successes_1h: i64,
    pub(super) successful_output_tokens_1h: Option<i64>,
    pub(super) successful_upstream_ms_1h: Option<i64>,
    pub(super) cost_input: Option<f64>,
    pub(super) cost_output: Option<f64>,
    pub(super) cost_cache_read: Option<f64>,
    pub(super) cost_cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteSchedulingSnapshot {
    pub(super) targets: Vec<TargetSchedulingSnapshot>,
}

/// Evidence `router::selection` assembles for one selection. Fields stay
/// router-private so callers can only obtain it through `RouteSelector`.
#[derive(Debug, Clone)]
pub struct RouteAttemptContext {
    pub(super) principal: String,
    pub(super) route_id: String,
    pub(super) conversation: Option<ConversationIdentity>,
    pub(super) conversation_affinity_target: Option<String>,
    pub(super) cache_affinity_target: Option<String>,
    pub(super) estimated_uncached_input_tokens: u64,
    pub(super) now_ms: u64,
}

/// Observable runtime state of one Route Target, owned by `RoutePolicyState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetRuntimeState {
    /// No cooldown record: the target takes part in scheduling. This is a
    /// scheduling default, not a measured-health verdict.
    Available,
    /// Inside the cooldown window; not eligible for selection.
    CoolingDown,
    /// Cooldown expired; the next selected request may probe the target.
    HalfOpen,
    /// A half-open probe is in flight; concurrent policies must fall back.
    Probing,
}

/// Point-in-time runtime status for one target key.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetRuntimeStatus {
    pub state: TargetRuntimeState,
    pub cooldown_remaining_ms: Option<u64>,
}

#[derive(Clone)]
pub struct RoutePolicyState {
    origin: Instant,
    inner: Arc<Mutex<RoutePolicyStateInner>>,
}

/// Cooldown bookkeeping for one target. Entries are never removed, so the
/// globally monotonically increasing `epoch` survives recovery: a success or
/// failure recorded against an older epoch can never clear or rewrite a newer
/// cooldown generation (no ABA on delete/recreate).
#[derive(Debug, Clone)]
struct TargetRuntime {
    /// Cooldown expiry in `RoutePolicyState::now_ms` terms; `0` = not cooling.
    cooldown_until: u64,
    /// Generation of the latest cooldown write.
    epoch: u64,
    /// Epoch claimed by the in-flight half-open probe, if any. Only ever
    /// equals the entry's own `epoch`, so it doubles as probe identity.
    probe_epoch: Option<u64>,
}

pub struct RouteAttemptReservation {
    state: RoutePolicyState,
    context: RouteAttemptContext,
    target_key: String,
    epoch: u64,
    probe: bool,
    active: bool,
}

#[derive(Default)]
struct RoutePolicyStateInner {
    targets: HashMap<String, TargetRuntime>,
    next_epoch: u64,
    in_flight_input: HashMap<String, u64>,
    conversation_targets: HashMap<ConversationAffinityKey, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConversationAffinityKey {
    principal: String,
    route_id: String,
    identity: ConversationIdentity,
}

impl RoutePolicyStateInner {
    fn next_epoch(&mut self) -> u64 {
        self.next_epoch += 1;
        self.next_epoch
    }

    // Late outcomes cannot overwrite a newer cooldown or probe generation.
    fn cool_target(&mut self, target_key: &str, epoch: u64, cooldown_ms: i64, now_ms: u64) {
        let current = self.targets.get(target_key);
        if current.map_or(0, |runtime| runtime.epoch) != epoch
            || (cooldown_ms <= 0 && current.is_none())
        {
            return;
        }
        let next = TargetRuntime {
            cooldown_until: if cooldown_ms > 0 {
                now_ms.saturating_add(cooldown_ms as u64)
            } else {
                0
            },
            epoch: self.next_epoch(),
            probe_epoch: None,
        };
        if let Some(runtime) = self.targets.get_mut(target_key) {
            *runtime = next;
        } else {
            self.targets.insert(target_key.to_owned(), next);
        }
    }

    /// Releases a half-open probe slot so the target returns to `HalfOpen`
    /// instead of staying `Probing` forever. Only clears when the slot is
    /// still held by this exact probe generation.
    fn release_probe(&mut self, target_key: &str, epoch: u64) {
        if let Some(runtime) = self.targets.get_mut(target_key)
            && runtime.probe_epoch == Some(epoch)
        {
            runtime.probe_epoch = None;
        }
    }
}

impl Default for RoutePolicyState {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            inner: Arc::new(Mutex::new(RoutePolicyStateInner::default())),
        }
    }
}

impl RoutePolicyState {
    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    /// Observable runtime status of one target key. A key with no runtime
    /// record reports `Available` — eligible for scheduling, not a measured
    /// health verdict.
    pub fn target_status(&self, target_key: &str) -> TargetRuntimeStatus {
        self.target_status_at(target_key, self.now_ms())
    }

    fn target_status_at(&self, target_key: &str, now_ms: u64) -> TargetRuntimeStatus {
        let inner = self.inner.lock();
        let Some(runtime) = inner.targets.get(target_key) else {
            return TargetRuntimeStatus {
                state: TargetRuntimeState::Available,
                cooldown_remaining_ms: None,
            };
        };
        if runtime.cooldown_until == 0 {
            return TargetRuntimeStatus {
                state: TargetRuntimeState::Available,
                cooldown_remaining_ms: None,
            };
        }
        if runtime.cooldown_until > now_ms {
            return TargetRuntimeStatus {
                state: TargetRuntimeState::CoolingDown,
                cooldown_remaining_ms: Some(runtime.cooldown_until - now_ms),
            };
        }
        TargetRuntimeStatus {
            state: if runtime.probe_epoch.is_some() {
                TargetRuntimeState::Probing
            } else {
                TargetRuntimeState::HalfOpen
            },
            cooldown_remaining_ms: None,
        }
    }

    /// The single success path: clears the target's cooldown when the
    /// recorded generation still owns it, releases the in-flight input
    /// reservation, and stores conversation affinity. A success carrying a
    /// stale `epoch` can never clear a newer cooldown or an in-flight probe.
    pub fn record_success(&self, context: &RouteAttemptContext, target_key: &str, epoch: u64) {
        let mut inner = self.inner.lock();
        if let Some(runtime) = inner.targets.get_mut(target_key)
            && runtime.epoch == epoch
        {
            runtime.cooldown_until = 0;
            runtime.probe_epoch = None;
        }
        release_reservation(
            &mut inner.in_flight_input,
            target_key,
            context.estimated_uncached_input_tokens,
        );
        if let Some(identity) = context.conversation.clone() {
            inner.conversation_targets.insert(
                ConversationAffinityKey {
                    principal: context.principal.clone(),
                    route_id: context.route_id.clone(),
                    identity,
                },
                target_key.to_owned(),
            );
        }
    }

    /// Records a failed upstream outcome only if its attempt generation is current.
    /// A zero cooldown disables the cooldown/probe cycle.
    pub fn record_failure(&self, target_key: &str, epoch: u64, cooldown_ms: i64) {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock();
        inner.cool_target(target_key, epoch, cooldown_ms, now_ms);
    }

    pub fn reservation(
        &self,
        context: RouteAttemptContext,
        target_key: String,
        epoch: u64,
        probe: bool,
    ) -> RouteAttemptReservation {
        RouteAttemptReservation {
            state: self.clone(),
            context,
            target_key,
            epoch,
            probe,
            active: true,
        }
    }

    fn release(&self, context: &RouteAttemptContext, target_key: &str, epoch: u64, probe: bool) {
        let mut inner = self.inner.lock();
        release_reservation(
            &mut inner.in_flight_input,
            target_key,
            context.estimated_uncached_input_tokens,
        );
        if probe {
            inner.release_probe(target_key, epoch);
        }
    }
}

impl RouteAttemptReservation {
    pub fn complete(mut self) {
        self.active = false;
    }
}

impl Drop for RouteAttemptReservation {
    fn drop(&mut self) {
        if self.active {
            self.state
                .release(&self.context, &self.target_key, self.epoch, self.probe);
        }
    }
}

pub struct RouteAttemptPolicy {
    context: RouteAttemptContext,
    state: RoutePolicyState,
    ordered: std::vec::IntoIter<SelectedTarget>,
    current_target_key: Option<String>,
    /// Runtime generation of the selected target; `0` when no record existed
    /// at selection time.
    current_epoch: u64,
    /// `true` while this policy holds the target's half-open probe slot.
    current_probe: bool,
    retries_used: i32,
}

impl RouteAttemptPolicy {
    /// Construction is router-private: callers receive a ready policy from
    /// `RouteSelector::select` and only drive it.
    pub(super) fn new(
        strategy: &str,
        targets: &[Target],
        context: RouteAttemptContext,
        snapshot: &RouteSchedulingSnapshot,
        state: RoutePolicyState,
    ) -> Self {
        // `context.now_ms` is the authoritative evidence clock — tests inject a
        // virtual clock through it — while `state.now_ms()` floors it so a
        // stale signal can never rewind the cooldown window in production.
        let now_ms = context.now_ms.max(state.now_ms());
        let (in_flight, preferred) = {
            let inner = state.inner.lock();
            let preferred = context
                .conversation
                .as_ref()
                .and_then(|identity| {
                    inner
                        .conversation_targets
                        .get(&ConversationAffinityKey {
                            principal: context.principal.clone(),
                            route_id: context.route_id.clone(),
                            identity: identity.clone(),
                        })
                        .cloned()
                        .or_else(|| context.conversation_affinity_target.clone())
                })
                .or_else(|| {
                    context
                        .conversation
                        .is_none()
                        .then(|| context.cache_affinity_target.clone())
                        .flatten()
                });
            (inner.in_flight_input.clone(), preferred)
        };
        let snapshots = snapshot
            .targets
            .iter()
            .map(|item| (item.target_key.as_str(), item))
            .collect::<HashMap<_, _>>();
        let mut priority_groups = BTreeMap::<Reverse<i32>, Vec<&Target>>::new();
        let inner = state.inner.lock();
        for target in targets {
            if !target.enabled {
                continue;
            }
            let key = target_key(target);
            // Actively-cooling targets are ineligible. Half-open and probing
            // targets stay candidates: `next_healthy` is the single authority
            // that claims the one probe slot while actually selecting.
            if inner
                .targets
                .get(&key)
                .is_some_and(|runtime| runtime.cooldown_until > now_ms)
            {
                continue;
            }
            priority_groups
                .entry(Reverse(target.priority))
                .or_default()
                .push(target);
        }
        drop(inner);
        let strategy = strategy
            .parse::<RouteSelectionStrategy>()
            .unwrap_or_default();
        let mut ordered = Vec::with_capacity(targets.len());
        for group in priority_groups.into_values() {
            let group = order_group(strategy.clone(), group, &snapshots, &in_flight);
            ordered.extend(group.into_iter().map(to_selected));
        }
        if let Some(preferred) = preferred
            && let Some(index) = ordered
                .iter()
                .position(|target| selected_target_key(target) == preferred)
        {
            let preferred = ordered.remove(index);
            ordered.insert(0, preferred);
        }
        Self {
            context,
            state,
            ordered: ordered.into_iter(),
            current_target_key: None,
            current_epoch: 0,
            current_probe: false,
            retries_used: 0,
        }
    }

    /// The evidence context this policy was assembled with.
    pub fn context(&self) -> &RouteAttemptContext {
        &self.context
    }

    /// The shared cooldown/reservation/affinity state behind this policy.
    pub fn state(&self) -> &RoutePolicyState {
        &self.state
    }

    pub fn retain(&mut self, predicate: impl FnMut(&SelectedTarget) -> bool) {
        self.ordered = self
            .ordered
            .by_ref()
            .filter(predicate)
            .collect::<Vec<_>>()
            .into_iter();
    }

    pub fn is_empty(&self) -> bool {
        self.ordered.as_slice().is_empty()
    }

    pub fn next_healthy(&mut self) -> Option<SelectedTarget> {
        self.skip_current();
        for target in self.ordered.by_ref() {
            let key = selected_target_key(&target);
            // Same floored clock as construction: the request's evidence clock
            // may be a virtual test clock; `state.now_ms()` floors it.
            let now_ms = self.context.now_ms.max(self.state.now_ms());
            let mut inner = self.state.inner.lock();
            let mut probe_epoch = None;
            match inner.targets.get(&key) {
                Some(runtime) if runtime.cooldown_until > 0 => {
                    if runtime.cooldown_until > now_ms {
                        // Actively cooling: never eligible.
                        continue;
                    }
                    if runtime.probe_epoch.is_some() {
                        // Another request already holds the single probe.
                        continue;
                    }
                    // Claim and fence the probe atomically. A cancelled probe's
                    // late result must not affect its replacement.
                    let epoch = inner.next_epoch();
                    let runtime = inner.targets.get_mut(&key).expect("target entry");
                    runtime.epoch = epoch;
                    runtime.probe_epoch = Some(epoch);
                    probe_epoch = Some(epoch);
                }
                _ => {}
            }
            *inner.in_flight_input.entry(key.clone()).or_default() = inner
                .in_flight_input
                .get(&key)
                .copied()
                .unwrap_or_default()
                .saturating_add(self.context.estimated_uncached_input_tokens);
            self.current_epoch = probe_epoch.unwrap_or_else(|| {
                inner
                    .targets
                    .get(&key)
                    .map(|runtime| runtime.epoch)
                    .unwrap_or(0)
            });
            self.current_probe = probe_epoch.is_some();
            self.current_target_key = Some(key);
            self.retries_used = 0;
            return Some(target);
        }
        None
    }

    /// Rechecks the selected attempt after asynchronous preparation/backoff.
    /// Another request may have cooled the target since it was selected.
    pub fn retry_current(&mut self) -> bool {
        let Some(key) = self.current_target_key.as_deref() else {
            return false;
        };
        let allowed = {
            let inner = self.state.inner.lock();
            match inner.targets.get(key) {
                None => self.current_epoch == 0 && !self.current_probe,
                Some(runtime) => {
                    runtime.epoch == self.current_epoch
                        && (runtime.cooldown_until == 0
                            || (self.current_probe
                                && runtime.probe_epoch == Some(self.current_epoch)))
                }
            }
        };
        if !allowed {
            self.skip_current();
        }
        allowed
    }

    pub fn skip_current(&mut self) {
        if let Some(key) = self.current_target_key.take() {
            self.release_current(&key);
        }
        self.current_probe = false;
    }

    pub fn accept_current(&mut self) {
        self.current_target_key = None;
        self.current_probe = false;
    }

    /// Epoch of the target generation currently selected by this policy.
    /// Callers pass it to `RoutePolicyState::record_success` and
    /// `RouteAttemptReservation` so stale in-flight attempts cannot rewrite
    /// newer state.
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    /// `true` when the currently selected attempt is the half-open probe.
    pub fn current_is_probe(&self) -> bool {
        self.current_probe
    }

    pub fn record_failure(
        &mut self,
        target: &SelectedTarget,
        failure: AttemptFailureSignal,
    ) -> AttemptFailureDisposition {
        let AttemptFailureSignal {
            kind,
            client_output_committed,
            retry_after,
            now_ms,
            jitter_sample,
        } = failure;
        let key = selected_target_key(target);
        // Never trust a stale signal timestamp over the evidence clock or the
        // state's own clock — the floor keeps cooldown writes monotonic.
        let now_ms = now_ms.max(self.context.now_ms).max(self.state.now_ms());
        let can_fail_over = transient_failure(&kind) || kind == AiErrorKind::QuotaExceeded;
        if self.current_probe {
            // Every failed probe waits a complete cooldown, without retrying.
            // Error classification still controls whether this request may fail over.
            self.abandon_target(&key, target.target_cooldown_ms, now_ms);
            return if can_fail_over && !client_output_committed {
                AttemptFailureDisposition::TryNextTarget
            } else {
                AttemptFailureDisposition::Stop
            };
        }
        if client_output_committed {
            if can_fail_over {
                self.abandon_target(&key, target.target_cooldown_ms, now_ms);
            } else {
                self.skip_current();
            }
            return AttemptFailureDisposition::Stop;
        }
        if transient_failure(&kind) {
            if !self.retry_current() {
                return AttemptFailureDisposition::TryNextTarget;
            }
            if self.retries_used < target.target_retry_budget {
                let cap_ms = 500_u64
                    .saturating_mul(1_u64 << self.retries_used.min(4) as u32)
                    .min(8_000);
                self.retries_used += 1;
                let delay = if kind == AiErrorKind::RateLimitError {
                    retry_after.unwrap_or_else(|| jitter(cap_ms, jitter_sample))
                } else {
                    jitter(cap_ms, jitter_sample)
                };
                return AttemptFailureDisposition::RetrySame { delay };
            }
            self.abandon_target(&key, target.target_cooldown_ms, now_ms);
            return AttemptFailureDisposition::TryNextTarget;
        }
        if kind == AiErrorKind::QuotaExceeded {
            self.abandon_target(&key, target.target_cooldown_ms, now_ms);
            return AttemptFailureDisposition::TryNextTarget;
        }
        self.release_current(&key);
        self.current_target_key = None;
        AttemptFailureDisposition::Stop
    }

    fn abandon_target(&mut self, key: &str, cooldown_ms: i64, now_ms: u64) {
        let mut inner = self.state.inner.lock();
        release_reservation(
            &mut inner.in_flight_input,
            key,
            self.context.estimated_uncached_input_tokens,
        );
        inner.cool_target(key, self.current_epoch, cooldown_ms, now_ms);
        self.current_target_key = None;
        self.current_probe = false;
    }

    /// Releases the input reservation and returns a held probe slot to
    /// `HalfOpen`, so cancellation or a semantic `Stop` never leaves the
    /// target stuck in `Probing` nor marks it `Available`.
    fn release_current(&self, key: &str) {
        let mut inner = self.state.inner.lock();
        release_reservation(
            &mut inner.in_flight_input,
            key,
            self.context.estimated_uncached_input_tokens,
        );
        if self.current_probe {
            inner.release_probe(key, self.current_epoch);
        }
    }
}

impl Drop for RouteAttemptPolicy {
    fn drop(&mut self) {
        if let Some(key) = self.current_target_key.take() {
            self.release_current(&key);
        }
    }
}

fn order_group<'a>(
    strategy: RouteSelectionStrategy,
    mut group: Vec<&'a Target>,
    snapshots: &HashMap<&str, &TargetSchedulingSnapshot>,
    in_flight: &HashMap<String, u64>,
) -> Vec<&'a Target> {
    if strategy == RouteSelectionStrategy::LatencyPreference {
        let valid = group
            .iter()
            .filter(|target| {
                snapshots
                    .get(target_key(target).as_str())
                    .is_some_and(|snapshot| latency_score(snapshot).is_some())
            })
            .count();
        if valid >= 2 {
            group.sort_by(|left, right| {
                let left_score = snapshots
                    .get(target_key(left).as_str())
                    .and_then(|snapshot| latency_score(snapshot));
                let right_score = snapshots
                    .get(target_key(right).as_str())
                    .and_then(|snapshot| latency_score(snapshot));
                right_score
                    .partial_cmp(&left_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            return group;
        }
    }
    let weights = traffic_weights(&group, snapshots);
    group.sort_by(|left, right| {
        traffic_score(left, snapshots, in_flight, weights)
            .partial_cmp(&traffic_score(right, snapshots, in_flight, weights))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    group
}

#[derive(Clone, Copy)]
struct TrafficWeights {
    cache_read: f64,
    output: f64,
    cache_write: f64,
}

fn traffic_weights(
    group: &[&Target],
    snapshots: &HashMap<&str, &TargetSchedulingSnapshot>,
) -> TrafficWeights {
    let priced = group
        .iter()
        .filter_map(|target| snapshots.get(target_key(target).as_str()).copied())
        .filter(|snapshot| {
            snapshot.cost_input.is_some_and(|value| value > 0.0) && snapshot.cost_output.is_some()
        })
        .collect::<Vec<_>>();
    if priced.is_empty() {
        return TrafficWeights {
            cache_read: 0.1,
            output: 5.0,
            cache_write: 6.0,
        };
    }
    TrafficWeights {
        cache_read: average_price_ratio(&priced, |snapshot| snapshot.cost_cache_read)
            .unwrap_or(0.1),
        output: average_price_ratio(&priced, |snapshot| snapshot.cost_output).unwrap_or(5.0),
        cache_write: average_price_ratio(&priced, |snapshot| snapshot.cost_cache_write)
            .unwrap_or(6.0),
    }
}

fn average_price_ratio(
    priced: &[&TargetSchedulingSnapshot],
    select: impl Fn(&TargetSchedulingSnapshot) -> Option<f64>,
) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0_u64;
    for snapshot in priced {
        if let (Some(input), Some(value)) = (snapshot.cost_input, select(snapshot)) {
            sum += value / input;
            count += 1;
        }
    }
    (count > 0).then_some(sum / count as f64)
}

fn traffic_score(
    target: &Target,
    snapshots: &HashMap<&str, &TargetSchedulingSnapshot>,
    in_flight: &HashMap<String, u64>,
    weights: TrafficWeights,
) -> f64 {
    let key = target_key(target);
    let (input, output, cache_read, cache_write) = snapshots
        .get(key.as_str())
        .map(|snapshot| {
            (
                snapshot.input_tokens_24h.unwrap_or_default(),
                snapshot.output_tokens_24h.unwrap_or_default(),
                snapshot.cache_read_tokens_24h.unwrap_or_default(),
                snapshot.cache_write_tokens_24h.unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    let uncached_input = input.saturating_sub(cache_read).max(0);
    weights.cache_read * cache_read.max(0) as f64
        + uncached_input as f64
        + weights.output * output.max(0) as f64
        + weights.cache_write * cache_write.max(0) as f64
        + in_flight.get(&key).copied().unwrap_or_default() as f64
}

fn latency_score(snapshot: &TargetSchedulingSnapshot) -> Option<f64> {
    let (Some(successful_output_tokens), Some(successful_upstream_ms)) = (
        snapshot.successful_output_tokens_1h,
        snapshot.successful_upstream_ms_1h,
    ) else {
        return None;
    };
    if snapshot.successes_1h < 20 || snapshot.attempts_1h == 0 || successful_upstream_ms == 0 {
        return None;
    }
    let success_rate = snapshot.successes_1h as f64 / snapshot.attempts_1h as f64;
    let output_tokens_per_second =
        successful_output_tokens as f64 / (successful_upstream_ms as f64 / 1_000.0);
    Some(success_rate * output_tokens_per_second)
}

fn transient_failure(kind: &AiErrorKind) -> bool {
    matches!(
        kind,
        AiErrorKind::Timeout
            | AiErrorKind::ServerError
            | AiErrorKind::ServiceUnavailable
            | AiErrorKind::RateLimitError
            | AiErrorKind::ModelNotAvailable
            | AiErrorKind::StreamMidError
            | AiErrorKind::UnexpectedEof
    )
}

fn jitter(cap_ms: u64, sample: f64) -> Duration {
    Duration::from_secs_f64((cap_ms as f64 / 1_000.0) * sample.clamp(0.0, 1.0))
}

fn release_reservation(reservations: &mut HashMap<String, u64>, key: &str, amount: u64) {
    let Some(current) = reservations.get_mut(key) else {
        return;
    };
    *current = current.saturating_sub(amount);
    if *current == 0 {
        reservations.remove(key);
    }
}

pub fn conversation_identity(request: &AiRequest) -> Option<ConversationIdentity> {
    super::continuation::parent_id_from_request(request)
        .map(ConversationIdentity::GenerationParent)
        .or_else(|| {
            let Some(ProtocolExt::OpenResponses(extension)) = request.ext.as_ref() else {
                return None;
            };
            extension
                .prompt_cache_key
                .as_ref()
                .map(|value| ConversationIdentity::PromptCacheKey(value.clone()))
        })
}

pub fn selected_target_key(target: &SelectedTarget) -> String {
    format!("{}:{}", target.provider_id, target.model)
}

fn target_key(target: &Target) -> String {
    format!("{}:{}", target.provider_id, target.model)
}

fn to_selected(target: &Target) -> SelectedTarget {
    SelectedTarget {
        provider_id: target.provider_id.clone(),
        model: target.model.clone(),
        priority: target.priority,
        first_token_timeout_ms: target.first_token_timeout_ms,
        target_retry_budget: target.target_retry_budget,
        target_cooldown_ms: target.target_cooldown_ms,
        thinking_level_map: target.thinking_level_map.0.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{
        DEFAULT_FIRST_TOKEN_TIMEOUT_MS, DEFAULT_TARGET_COOLDOWN_MS, DEFAULT_TARGET_RETRY_BUDGET,
    };

    fn target(provider_id: &str, priority: i32) -> Target {
        Target {
            id: format!("target-{provider_id}"),
            model_id: "route".into(),
            provider_id: provider_id.into(),
            model: "model".into(),
            enabled: true,
            priority,
            first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
            target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
            target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
            created_at: String::new(),
            thinking_level_map: sqlx::types::Json(Vec::new()),
        }
    }

    fn context(now_ms: u64) -> RouteAttemptContext {
        RouteAttemptContext {
            principal: "principal".into(),
            route_id: "route".into(),
            conversation: None,
            conversation_affinity_target: None,
            cache_affinity_target: None,
            estimated_uncached_input_tokens: 0,
            now_ms,
        }
    }

    fn next_provider(policy: &mut RouteAttemptPolicy) -> Option<String> {
        policy.next_healthy().map(|target| target.provider_id)
    }

    #[test]
    fn higher_priority_groups_are_exhausted_before_lower_groups() {
        let state = RoutePolicyState::default();
        let targets = vec![
            target("low", 0),
            target("high-a", 100_000),
            target("high-b", 100_000),
        ];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );

        assert_eq!(next_provider(&mut policy).as_deref(), Some("high-a"));
        assert_eq!(next_provider(&mut policy).as_deref(), Some("high-b"));
        assert_eq!(next_provider(&mut policy).as_deref(), Some("low"));
    }

    #[test]
    fn disabled_targets_never_enter_attempt_or_affinity_order() {
        let state = RoutePolicyState::default();
        let mut disabled = target("disabled", i32::MAX);
        disabled.enabled = false;
        let targets = vec![target("enabled", -1), disabled];
        let mut attempt_context = context(0);
        attempt_context.conversation_affinity_target = Some("disabled:model".into());
        attempt_context.cache_affinity_target = Some("disabled:model".into());
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            attempt_context,
            &RouteSchedulingSnapshot::default(),
            state,
        );

        assert_eq!(next_provider(&mut policy).as_deref(), Some("enabled"));
        assert_eq!(next_provider(&mut policy), None);
    }

    #[test]
    fn signed_priority_groups_remain_descending() {
        let state = RoutePolicyState::default();
        let targets = vec![
            target("minimum", i32::MIN),
            target("negative", -1),
            target("maximum", i32::MAX),
        ];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );

        assert_eq!(
            [
                next_provider(&mut policy),
                next_provider(&mut policy),
                next_provider(&mut policy),
            ],
            [
                Some("maximum".into()),
                Some("negative".into()),
                Some("minimum".into()),
            ]
        );
    }

    #[test]
    fn conversation_affinity_precedes_priority_and_suppresses_cache_affinity() {
        let state = RoutePolicyState::default();
        let targets = vec![
            target("primary", 10),
            target("conversation", 0),
            target("cache", 0),
        ];
        let identity = ConversationIdentity::PromptCacheKey("chat-a".into());
        let mut first_context = context(0);
        first_context.conversation = Some(identity.clone());
        state.record_success(&first_context, "conversation:model", 0);

        let mut next_context = context(1);
        next_context.conversation = Some(identity);
        next_context.conversation_affinity_target = Some("primary:model".into());
        next_context.cache_affinity_target = Some("cache:model".into());
        let mut next = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            next_context,
            &RouteSchedulingSnapshot::default(),
            state,
        );
        assert_eq!(next_provider(&mut next).as_deref(), Some("conversation"));
    }

    #[test]
    fn generation_parent_identity_uses_the_canonical_cross_protocol_marker() {
        let mut request = AiRequest::new("route", Vec::new());
        request.meta.vendor.ingress.insert(
            "previous_response_id".into(),
            serde_json::Value::String("parent-a".into()),
        );
        request.ext = Some(ProtocolExt::OpenResponses(
            stravia_runtime_contract::protocol::ir::OpenResponsesExt {
                prompt_cache_key: Some("cache-key".into()),
                ..Default::default()
            },
        ));

        assert_eq!(
            conversation_identity(&request),
            Some(ConversationIdentity::GenerationParent("parent-a".into()))
        );
    }

    #[test]
    fn affinity_isolated_by_identity_principal_and_route_and_cache_only_fills_identity_gap() {
        let state = RoutePolicyState::default();
        let targets = vec![target("primary", 10), target("affinity", 0)];
        let mut recorded_context = context(0);
        recorded_context.conversation =
            Some(ConversationIdentity::GenerationParent("parent-a".into()));
        state.record_success(&recorded_context, "affinity:model", 0);

        for (principal, route_id, identity) in [
            (
                "principal",
                "route",
                ConversationIdentity::GenerationParent("parent-b".into()),
            ),
            (
                "other-principal",
                "route",
                ConversationIdentity::GenerationParent("parent-a".into()),
            ),
            (
                "principal",
                "other-route",
                ConversationIdentity::GenerationParent("parent-a".into()),
            ),
        ] {
            let mut isolated = context(1);
            isolated.principal = principal.into();
            isolated.route_id = route_id.into();
            isolated.conversation = Some(identity);
            let mut policy = RouteAttemptPolicy::new(
                "traffic_equalization",
                &targets,
                isolated,
                &RouteSchedulingSnapshot::default(),
                state.clone(),
            );
            assert_eq!(next_provider(&mut policy).as_deref(), Some("primary"));
        }

        let mut cache_context = context(1);
        cache_context.cache_affinity_target = Some("affinity:model".into());
        let mut cache = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            cache_context,
            &RouteSchedulingSnapshot::default(),
            state,
        );
        assert_eq!(next_provider(&mut cache).as_deref(), Some("affinity"));
    }

    #[test]
    fn traffic_equalization_counts_uncached_input_once_and_in_flight_reservations() {
        let state = RoutePolicyState::default();
        let targets = vec![target("busy", 0), target("idle", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![TargetSchedulingSnapshot {
                target_key: "busy:model".into(),
                input_tokens_24h: Some(1_000),
                cache_read_tokens_24h: Some(900),
                ..Default::default()
            }],
        };
        let mut first_context = context(0);
        first_context.estimated_uncached_input_tokens = 200;
        let mut first = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            first_context,
            &snapshot,
            state.clone(),
        );
        assert_eq!(next_provider(&mut first).as_deref(), Some("idle"));

        let mut second = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &snapshot,
            state,
        );
        assert_eq!(next_provider(&mut second).as_deref(), Some("busy"));
    }

    #[test]
    fn detached_stream_reservation_is_released_when_completion_is_dropped() {
        let state = RoutePolicyState::default();
        let targets = vec![target("busy", 0), target("idle", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![TargetSchedulingSnapshot {
                target_key: "busy:model".into(),
                input_tokens_24h: Some(100),
                ..Default::default()
            }],
        };
        let mut attempt_context = context(0);
        attempt_context.estimated_uncached_input_tokens = 200;
        let mut first = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            attempt_context.clone(),
            &snapshot,
            state.clone(),
        );
        assert_eq!(next_provider(&mut first).as_deref(), Some("idle"));
        first.accept_current();

        let reservation = state.reservation(attempt_context, "idle:model".into(), 0, false);
        drop(reservation);

        let mut next = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &snapshot,
            state,
        );
        assert_eq!(next_provider(&mut next).as_deref(), Some("idle"));
    }

    #[test]
    fn traffic_equalization_averages_available_price_ratios_and_falls_back_per_dimension() {
        let state = RoutePolicyState::default();
        let targets = vec![target("cached", 0), target("output", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "cached:model".into(),
                    cache_read_tokens_24h: Some(100),
                    cost_input: Some(2.0),
                    cost_output: Some(10.0),
                    cost_cache_read: None,
                    cost_cache_write: Some(12.0),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output:model".into(),
                    output_tokens_24h: Some(3),
                    cost_input: Some(4.0),
                    cost_output: Some(20.0),
                    cost_cache_read: Some(0.8),
                    cost_cache_write: None,
                    ..Default::default()
                },
            ],
        };
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &snapshot,
            state,
        );
        assert_eq!(next_provider(&mut policy).as_deref(), Some("output"));

        let fallback_snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "cached:model".into(),
                    cache_write_tokens_24h: Some(1),
                    cost_input: Some(2.0),
                    cost_output: Some(10.0),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output:model".into(),
                    output_tokens_24h: Some(1),
                    cost_input: Some(4.0),
                    cost_output: Some(20.0),
                    ..Default::default()
                },
            ],
        };
        let mut fallback = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &fallback_snapshot,
            RoutePolicyState::default(),
        );
        assert_eq!(next_provider(&mut fallback).as_deref(), Some("output"));
    }

    #[test]
    fn latency_preference_requires_two_targets_with_twenty_successes() {
        let state = RoutePolicyState::default();
        let targets = vec![target("slow", 0), target("fast", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "slow:model".into(),
                    attempts_1h: 20,
                    successes_1h: 20,
                    successful_output_tokens_1h: Some(2_000),
                    successful_upstream_ms_1h: Some(20_000),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "fast:model".into(),
                    attempts_1h: 25,
                    successes_1h: 20,
                    successful_output_tokens_1h: Some(4_000),
                    successful_upstream_ms_1h: Some(10_000),
                    ..Default::default()
                },
            ],
        };
        let mut policy =
            RouteAttemptPolicy::new("latency_preference", &targets, context(0), &snapshot, state);
        assert_eq!(next_provider(&mut policy).as_deref(), Some("fast"));
    }

    #[test]
    fn latency_preference_falls_back_to_traffic_when_fewer_than_two_targets_have_data() {
        let state = RoutePolicyState::default();
        let targets = vec![target("sampled", 0), target("cold", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "sampled:model".into(),
                    input_tokens_24h: Some(100),
                    attempts_1h: 20,
                    successes_1h: 20,
                    successful_output_tokens_1h: Some(2_000),
                    successful_upstream_ms_1h: Some(1_000),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "cold:model".into(),
                    attempts_1h: 19,
                    successes_1h: 19,
                    successful_output_tokens_1h: Some(19_000),
                    successful_upstream_ms_1h: Some(1_000),
                    ..Default::default()
                },
            ],
        };
        let mut policy =
            RouteAttemptPolicy::new("latency_preference", &targets, context(0), &snapshot, state);
        assert_eq!(next_provider(&mut policy).as_deref(), Some("cold"));
    }

    #[test]
    fn transient_failures_retry_same_target_then_cool_it_down() {
        let state = RoutePolicyState::default();
        let mut configured = target("flaky", 0);
        configured.target_retry_budget = 1;
        configured.target_cooldown_ms = 120_000;
        let targets = vec![configured, target("fallback", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(10),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        let flaky = policy.next_healthy().expect("first Target");
        assert_eq!(flaky.provider_id, "flaky");
        assert_eq!(
            policy.record_failure(
                &flaky,
                AttemptFailureSignal {
                    kind: AiErrorKind::Timeout,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: 10,
                    jitter_sample: 1.0,
                },
            ),
            AttemptFailureDisposition::RetrySame {
                delay: Duration::from_millis(500)
            }
        );
        assert_eq!(
            policy.record_failure(
                &flaky,
                AttemptFailureSignal {
                    kind: AiErrorKind::Timeout,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: 1_000,
                    jitter_sample: 1.0,
                },
            ),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(next_provider(&mut policy).as_deref(), Some("fallback"));

        let mut new_request = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(120_999),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        assert_eq!(next_provider(&mut new_request).as_deref(), Some("fallback"));

        let mut after_cooldown = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(121_001),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        assert_eq!(next_provider(&mut after_cooldown).as_deref(), Some("flaky"));
    }

    #[test]
    fn default_retry_budget_uses_capped_exponential_full_jitter() {
        let state = RoutePolicyState::default();
        let targets = vec![target("flaky", 0), target("fallback", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        let flaky = policy.next_healthy().expect("first Target");
        for cap_ms in [500, 1_000, 2_000, 4_000, 8_000] {
            assert_eq!(
                policy.record_failure(
                    &flaky,
                    AttemptFailureSignal {
                        kind: AiErrorKind::ServerError,
                        client_output_committed: false,
                        retry_after: None,
                        now_ms: 0,
                        jitter_sample: 1.0,
                    },
                ),
                AttemptFailureDisposition::RetrySame {
                    delay: Duration::from_millis(cap_ms),
                }
            );
        }
        assert_eq!(
            policy.record_failure(
                &flaky,
                AttemptFailureSignal {
                    kind: AiErrorKind::ServerError,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: 0,
                    jitter_sample: 1.0,
                },
            ),
            AttemptFailureDisposition::TryNextTarget
        );
    }

    #[test]
    fn retry_after_overrides_jitter_and_quota_moves_without_retry() {
        let state = RoutePolicyState::default();
        let targets = vec![target("first", 0), target("second", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        let first = policy.next_healthy().expect("first Target");
        assert_eq!(
            policy.record_failure(
                &first,
                AttemptFailureSignal {
                    kind: AiErrorKind::RateLimitError,
                    client_output_committed: false,
                    retry_after: Some(Duration::from_secs(9)),
                    now_ms: 0,
                    jitter_sample: 0.0,
                },
            ),
            AttemptFailureDisposition::RetrySame {
                delay: Duration::from_secs(9)
            }
        );
        assert_eq!(
            policy.record_failure(
                &first,
                AttemptFailureSignal {
                    kind: AiErrorKind::QuotaExceeded,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: 0,
                    jitter_sample: 0.0,
                },
            ),
            AttemptFailureDisposition::TryNextTarget
        );
    }

    #[test]
    fn committed_or_semantic_failures_stop_the_request() {
        let state = RoutePolicyState::default();
        let targets = vec![target("first", 0), target("second", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        let first = policy.next_healthy().expect("first Target");
        assert_eq!(
            policy.record_failure(
                &first,
                AttemptFailureSignal {
                    kind: AiErrorKind::ServerError,
                    client_output_committed: true,
                    retry_after: None,
                    now_ms: 0,
                    jitter_sample: 0.0,
                },
            ),
            AttemptFailureDisposition::Stop
        );
        assert_eq!(
            policy.record_failure(
                &first,
                AttemptFailureSignal {
                    kind: AiErrorKind::InvalidRequest,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: 0,
                    jitter_sample: 0.0,
                },
            ),
            AttemptFailureDisposition::Stop
        );
    }

    fn policy_at(state: &RoutePolicyState, targets: &[Target], now_ms: u64) -> RouteAttemptPolicy {
        RouteAttemptPolicy::new(
            "traffic_equalization",
            targets,
            context(now_ms),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        )
    }

    fn failure_at(kind: AiErrorKind, now_ms: u64) -> AttemptFailureSignal {
        AttemptFailureSignal {
            kind,
            client_output_committed: false,
            retry_after: None,
            now_ms,
            jitter_sample: 0.0,
        }
    }

    fn cooled_targets() -> (RoutePolicyState, Vec<Target>) {
        let state = RoutePolicyState::default();
        let mut recovering = target("recovering", 1);
        recovering.target_cooldown_ms = 1_000;
        let targets = vec![recovering, target("fallback", 0)];
        let mut opener = policy_at(&state, &targets, 10_000);
        let selected = opener.next_healthy().expect("initial target");
        assert_eq!(
            opener.record_failure(&selected, failure_at(AiErrorKind::QuotaExceeded, 10_000)),
            AttemptFailureDisposition::TryNextTarget
        );
        (state, targets)
    }

    #[test]
    fn half_open_probe_failure_restarts_full_cooldown_without_retries() {
        let (state, targets) = cooled_targets();
        assert_eq!(
            state.target_status_at("recovering:model", 11_000).state,
            TargetRuntimeState::HalfOpen
        );
        let mut probe = policy_at(&state, &targets, 11_000);
        let selected = probe.next_healthy().expect("probe");
        assert_eq!(selected.provider_id, "recovering");
        assert_eq!(
            probe.record_failure(&selected, failure_at(AiErrorKind::Timeout, 11_000)),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(
            state.target_status_at("recovering:model", 11_000),
            TargetRuntimeStatus {
                state: TargetRuntimeState::CoolingDown,
                cooldown_remaining_ms: Some(1_000)
            }
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 11_999)).as_deref(),
            Some("fallback")
        );
        let mut second_probe = policy_at(&state, &targets, 12_000);
        let selected = second_probe.next_healthy().expect("next probe");
        assert_eq!(selected.provider_id, "recovering");
        assert_eq!(
            second_probe.record_failure(&selected, failure_at(AiErrorKind::InvalidRequest, 12_000)),
            AttemptFailureDisposition::Stop
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 12_999)).as_deref(),
            Some("fallback")
        );
    }

    #[test]
    fn simultaneous_requests_share_one_half_open_probe() {
        let (state, targets) = cooled_targets();
        let barrier = std::sync::Barrier::new(8);
        let selected = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let mut policy = policy_at(&state, &targets, 11_000);
                        barrier.wait();
                        let selected = next_provider(&mut policy);
                        // Keep every selection alive until all requests have competed.
                        barrier.wait();
                        selected
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("selection worker"))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            selected
                .iter()
                .filter(|provider| provider.as_deref() == Some("recovering"))
                .count(),
            1
        );
        assert_eq!(
            selected
                .iter()
                .filter(|provider| provider.as_deref() == Some("fallback"))
                .count(),
            7
        );
    }

    #[test]
    fn half_open_success_restores_normal_concurrent_scheduling() {
        let (state, targets) = cooled_targets();
        let mut probe = policy_at(&state, &targets, 11_000);
        assert_eq!(next_provider(&mut probe).as_deref(), Some("recovering"));
        state.record_success(probe.context(), "recovering:model", probe.current_epoch());
        probe.accept_current();
        assert_eq!(
            state.target_status("recovering:model").state,
            TargetRuntimeState::Available
        );
        let mut first = policy_at(&state, &targets, 11_001);
        let mut second = policy_at(&state, &targets, 11_001);
        assert_eq!(next_provider(&mut first).as_deref(), Some("recovering"));
        assert_eq!(next_provider(&mut second).as_deref(), Some("recovering"));
    }

    #[test]
    fn cancelled_probes_release_the_slot_and_late_outcomes_cannot_replace_the_next_probe() {
        let (state, targets) = cooled_targets();
        let mut first = policy_at(&state, &targets, 11_000);
        assert_eq!(next_provider(&mut first).as_deref(), Some("recovering"));
        let old_context = first.context().clone();
        let old_epoch = first.current_epoch();
        let reservation = state.reservation(
            old_context.clone(),
            "recovering:model".into(),
            old_epoch,
            true,
        );
        first.accept_current();
        drop(reservation);
        assert_eq!(
            state.target_status_at("recovering:model", 11_000).state,
            TargetRuntimeState::HalfOpen
        );
        let mut replacement = policy_at(&state, &targets, 11_000);
        assert_eq!(
            next_provider(&mut replacement).as_deref(),
            Some("recovering")
        );
        state.record_success(&old_context, "recovering:model", old_epoch);
        state.record_failure("recovering:model", old_epoch, 1_000);
        assert_eq!(
            state.target_status_at("recovering:model", 11_000).state,
            TargetRuntimeState::Probing
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 11_000)).as_deref(),
            Some("fallback")
        );
        drop(replacement);
        assert_eq!(
            state.target_status_at("recovering:model", 11_000).state,
            TargetRuntimeState::HalfOpen
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 11_000)).as_deref(),
            Some("recovering")
        );
    }

    #[test]
    fn newer_cooldown_blocks_old_retries_and_ignores_late_success() {
        let state = RoutePolicyState::default();
        let targets = vec![target("recovering", 1), target("fallback", 0)];
        let mut old = policy_at(&state, &targets, 10_000);
        let mut opener = policy_at(&state, &targets, 10_000);
        let old_target = old.next_healthy().expect("old request");
        let selected = opener.next_healthy().expect("concurrent request");
        assert_eq!(
            old.record_failure(&old_target, failure_at(AiErrorKind::Timeout, 10_000)),
            AttemptFailureDisposition::RetrySame {
                delay: Duration::ZERO
            }
        );
        assert_eq!(
            opener.record_failure(&selected, failure_at(AiErrorKind::QuotaExceeded, 10_000)),
            AttemptFailureDisposition::TryNextTarget
        );
        assert!(!old.retry_current());
        state.record_success(old.context(), "recovering:model", old.current_epoch());
        assert_eq!(
            state.target_status_at("recovering:model", 10_001).state,
            TargetRuntimeState::CoolingDown
        );
        assert_eq!(next_provider(&mut old).as_deref(), Some("fallback"));
    }

    #[test]
    fn disabled_cooldown_never_creates_a_probe_gate() {
        let state = RoutePolicyState::default();
        let mut configured = target("recovering", 1);
        configured.target_cooldown_ms = 0;
        configured.target_retry_budget = 0;
        let targets = vec![configured];
        let mut opener = policy_at(&state, &targets, 10_000);
        let selected = opener.next_healthy().expect("target");
        assert_eq!(
            opener.record_failure(&selected, failure_at(AiErrorKind::Timeout, 10_000)),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(
            state.target_status("recovering:model").state,
            TargetRuntimeState::Available
        );
        let mut first = policy_at(&state, &targets, 10_001);
        let mut second = policy_at(&state, &targets, 10_001);
        assert_eq!(next_provider(&mut first).as_deref(), Some("recovering"));
        assert_eq!(next_provider(&mut second).as_deref(), Some("recovering"));
    }

    #[test]
    fn half_open_targets_only_probe_when_eligible_and_actually_selected() {
        let (state, mut targets) = cooled_targets();
        targets[0].enabled = false;
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 11_000)).as_deref(),
            Some("fallback")
        );
        assert_eq!(
            state.target_status_at("recovering:model", 11_000).state,
            TargetRuntimeState::HalfOpen
        );
        targets[0].enabled = true;
        let mut filtered = policy_at(&state, &targets, 11_000);
        filtered.retain(|target| target.provider_id != "recovering");
        assert_eq!(next_provider(&mut filtered).as_deref(), Some("fallback"));
        assert_eq!(
            state.target_status_at("recovering:model", 11_000).state,
            TargetRuntimeState::HalfOpen
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 11_000)).as_deref(),
            Some("recovering")
        );
    }

    #[test]
    fn expired_cooldown_allows_a_single_probe_while_concurrent_policies_fall_back() {
        let state = RoutePolicyState::default();
        let mut cooling = target("recovering", 0);
        cooling.target_retry_budget = 0;
        cooling.target_cooldown_ms = 120_000;
        let targets = vec![cooling, target("fallback", 0)];

        let mut opener = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        let recovering = opener.next_healthy().expect("first Target");
        assert_eq!(recovering.provider_id, "recovering");
        assert_eq!(
            opener.record_failure(
                &recovering,
                AttemptFailureSignal {
                    kind: AiErrorKind::ServiceUnavailable,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: 0,
                    jitter_sample: 0.0,
                },
            ),
            AttemptFailureDisposition::TryNextTarget
        );
        drop(opener);

        // Cooldown elapsed: exactly one policy may probe the recovering target;
        // a concurrent policy over the same shared state must fall back.
        let mut probe = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(121_000),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        let mut rival = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(121_000),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        assert_eq!(next_provider(&mut probe).as_deref(), Some("recovering"));
        assert_eq!(next_provider(&mut rival).as_deref(), Some("fallback"));
        assert_eq!(next_provider(&mut rival), None);
    }
}
