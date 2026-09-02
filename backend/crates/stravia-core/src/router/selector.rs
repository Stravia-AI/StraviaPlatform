//! Layered Route Target selection and failure policy.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::models::{RouteSelectionStrategy, Target};
use crate::protocol::ir::{AiErrorKind, AiRequest, ProtocolExt};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationIdentity {
    GenerationParent(String),
    PromptCacheKey(String),
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct TargetSchedulingSnapshot {
    pub target_key: String,
    pub input_tokens_24h: i64,
    pub output_tokens_24h: i64,
    pub cache_read_tokens_24h: i64,
    pub cache_write_tokens_24h: i64,
    pub attempts_1h: i64,
    pub successes_1h: i64,
    pub successful_output_tokens_1h: i64,
    pub successful_upstream_ms_1h: i64,
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteSchedulingSnapshot {
    pub targets: Vec<TargetSchedulingSnapshot>,
}

#[derive(Debug, Clone)]
pub struct RouteAttemptContext {
    pub principal: String,
    pub route_id: String,
    pub conversation: Option<ConversationIdentity>,
    pub conversation_affinity_target: Option<String>,
    pub cache_affinity_target: Option<String>,
    pub estimated_uncached_input_tokens: u64,
    pub now_ms: u64,
}

#[derive(Clone)]
pub struct RoutePolicyState {
    origin: Instant,
    inner: Arc<Mutex<RoutePolicyStateInner>>,
}

pub struct RouteAttemptReservation {
    state: RoutePolicyState,
    context: RouteAttemptContext,
    target_key: String,
    active: bool,
}

#[derive(Default)]
struct RoutePolicyStateInner {
    cooldown_until: HashMap<String, u64>,
    in_flight_input: HashMap<String, u64>,
    conversation_targets: HashMap<ConversationAffinityKey, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConversationAffinityKey {
    principal: String,
    route_id: String,
    identity: ConversationIdentity,
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

    pub fn record_success(&self, context: &RouteAttemptContext, target_key: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    pub fn reservation(
        &self,
        context: RouteAttemptContext,
        target_key: String,
    ) -> RouteAttemptReservation {
        RouteAttemptReservation {
            state: self.clone(),
            context,
            target_key,
            active: true,
        }
    }

    fn release(&self, context: &RouteAttemptContext, target_key: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        release_reservation(
            &mut inner.in_flight_input,
            target_key,
            context.estimated_uncached_input_tokens,
        );
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
            self.state.release(&self.context, &self.target_key);
        }
    }
}

pub struct RouteAttemptPolicy {
    context: RouteAttemptContext,
    state: RoutePolicyState,
    ordered: std::vec::IntoIter<SelectedTarget>,
    current_target_key: Option<String>,
    retries_used: i32,
}

impl RouteAttemptPolicy {
    pub fn new(
        strategy: &str,
        targets: &[Target],
        context: RouteAttemptContext,
        snapshot: &RouteSchedulingSnapshot,
        state: RoutePolicyState,
    ) -> Self {
        let (cooldowns, in_flight, preferred) = {
            let mut inner = state
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner
                .cooldown_until
                .retain(|_, expires_at| *expires_at > context.now_ms);
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
            (
                inner.cooldown_until.clone(),
                inner.in_flight_input.clone(),
                preferred,
            )
        };
        let snapshots = snapshot
            .targets
            .iter()
            .map(|item| (item.target_key.as_str(), item))
            .collect::<HashMap<_, _>>();
        let mut priority_groups = BTreeMap::<Reverse<i32>, Vec<&Target>>::new();
        for target in targets {
            let key = target_key(target);
            if cooldowns
                .get(&key)
                .is_some_and(|expires_at| *expires_at > context.now_ms)
            {
                continue;
            }
            priority_groups
                .entry(Reverse(target.priority))
                .or_default()
                .push(target);
        }
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
            retries_used: 0,
        }
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

    pub fn next_healthy(
        &mut self,
        health: &crate::router::health::HealthRegistry,
    ) -> Option<SelectedTarget> {
        while let Some(target) = self.ordered.next() {
            let key = selected_target_key(&target);
            let cooling_down = self
                .state
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cooldown_until
                .get(&key)
                .is_some_and(|expires_at| *expires_at > self.context.now_ms);
            if cooling_down || !health.is_healthy(&key) {
                continue;
            }
            let mut inner = self
                .state
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *inner.in_flight_input.entry(key.clone()).or_default() = inner
                .in_flight_input
                .get(&key)
                .copied()
                .unwrap_or_default()
                .saturating_add(self.context.estimated_uncached_input_tokens);
            self.current_target_key = Some(key);
            self.retries_used = 0;
            return Some(target);
        }
        None
    }

    pub fn record_success(
        &mut self,
        health: &crate::router::health::HealthRegistry,
        target: &SelectedTarget,
    ) {
        let key = selected_target_key(target);
        health.record_success(&key);
        self.state.record_success(&self.context, &key);
        self.current_target_key = None;
    }

    pub fn skip_current(&mut self) {
        if let Some(key) = self.current_target_key.take() {
            self.release_reservation(&key);
        }
    }

    pub fn accept_current(&mut self) {
        self.current_target_key = None;
    }

    pub fn record_failure(
        &mut self,
        health: &crate::router::health::HealthRegistry,
        target: &SelectedTarget,
        kind: AiErrorKind,
        client_output_committed: bool,
        retry_after: Option<Duration>,
        now_ms: u64,
        jitter_sample: f64,
    ) -> AttemptFailureDisposition {
        let key = selected_target_key(target);
        health.record_failure(&key);
        if client_output_committed {
            self.release_reservation(&key);
            self.current_target_key = None;
            return AttemptFailureDisposition::Stop;
        }
        if transient_failure(&kind) {
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
        self.release_reservation(&key);
        self.current_target_key = None;
        AttemptFailureDisposition::Stop
    }

    fn abandon_target(&mut self, key: &str, cooldown_ms: i64, now_ms: u64) {
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        release_reservation(
            &mut inner.in_flight_input,
            key,
            self.context.estimated_uncached_input_tokens,
        );
        if cooldown_ms > 0 {
            inner
                .cooldown_until
                .insert(key.to_owned(), now_ms.saturating_add(cooldown_ms as u64));
        }
        self.current_target_key = None;
    }

    fn release_reservation(&self, key: &str) {
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        release_reservation(
            &mut inner.in_flight_input,
            key,
            self.context.estimated_uncached_input_tokens,
        );
    }
}

impl Drop for RouteAttemptPolicy {
    fn drop(&mut self) {
        if let Some(key) = self.current_target_key.take() {
            self.release_reservation(&key);
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
                snapshot.input_tokens_24h,
                snapshot.output_tokens_24h,
                snapshot.cache_read_tokens_24h,
                snapshot.cache_write_tokens_24h,
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
    if snapshot.successes_1h < 20
        || snapshot.attempts_1h == 0
        || snapshot.successful_upstream_ms_1h == 0
    {
        return None;
    }
    let success_rate = snapshot.successes_1h as f64 / snapshot.attempts_1h as f64;
    let output_tokens_per_second = snapshot.successful_output_tokens_1h as f64
        / (snapshot.successful_upstream_ms_1h as f64 / 1_000.0);
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
    crate::model_turn::parent_id_from_request(request)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{
        DEFAULT_FIRST_TOKEN_TIMEOUT_MS, DEFAULT_TARGET_COOLDOWN_MS, DEFAULT_TARGET_RETRY_BUDGET,
    };
    use crate::router::health::HealthRegistry;

    fn target(provider_id: &str, priority: i32) -> Target {
        Target {
            id: format!("target-{provider_id}"),
            model_id: "route".into(),
            provider_id: provider_id.into(),
            model: "model".into(),
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

    fn next_provider(policy: &mut RouteAttemptPolicy, health: &HealthRegistry) -> Option<String> {
        policy.next_healthy(health).map(|target| target.provider_id)
    }

    #[test]
    fn higher_priority_groups_are_exhausted_before_lower_groups() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
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

        assert_eq!(
            next_provider(&mut policy, &health).as_deref(),
            Some("high-a")
        );
        assert_eq!(
            next_provider(&mut policy, &health).as_deref(),
            Some("high-b")
        );
        assert_eq!(next_provider(&mut policy, &health).as_deref(), Some("low"));
    }

    #[test]
    fn conversation_affinity_precedes_priority_and_suppresses_cache_affinity() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![
            target("primary", 10),
            target("conversation", 0),
            target("cache", 0),
        ];
        let identity = ConversationIdentity::PromptCacheKey("chat-a".into());
        let mut first_context = context(0);
        first_context.conversation = Some(identity.clone());
        let mut first = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            first_context,
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        let conversation = targets
            .iter()
            .find(|target| target.provider_id == "conversation")
            .unwrap();
        first.record_success(&health, &super::to_selected(conversation));

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
        assert_eq!(
            next_provider(&mut next, &health).as_deref(),
            Some("conversation")
        );
    }

    #[test]
    fn generation_parent_identity_uses_the_canonical_cross_protocol_marker() {
        let mut request = AiRequest::new("route", Vec::new());
        request.meta.vendor.ingress.insert(
            "previous_response_id".into(),
            serde_json::Value::String("parent-a".into()),
        );
        request.ext = Some(ProtocolExt::OpenResponses(
            crate::protocol::ir::OpenResponsesExt {
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
        let health = HealthRegistry::new();
        let targets = vec![target("primary", 10), target("affinity", 0)];
        let mut recorded_context = context(0);
        recorded_context.conversation =
            Some(ConversationIdentity::GenerationParent("parent-a".into()));
        let mut recorded = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            recorded_context,
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        recorded.record_success(&health, &to_selected(&targets[1]));

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
            assert_eq!(
                next_provider(&mut policy, &health).as_deref(),
                Some("primary")
            );
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
        assert_eq!(
            next_provider(&mut cache, &health).as_deref(),
            Some("affinity")
        );
    }

    #[test]
    fn traffic_equalization_counts_uncached_input_once_and_in_flight_reservations() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("busy", 0), target("idle", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![TargetSchedulingSnapshot {
                target_key: "busy:model".into(),
                input_tokens_24h: 1_000,
                cache_read_tokens_24h: 900,
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
        assert_eq!(next_provider(&mut first, &health).as_deref(), Some("idle"));

        let mut second = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &snapshot,
            state,
        );
        assert_eq!(next_provider(&mut second, &health).as_deref(), Some("busy"));
    }

    #[test]
    fn detached_stream_reservation_is_released_when_completion_is_dropped() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("busy", 0), target("idle", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![TargetSchedulingSnapshot {
                target_key: "busy:model".into(),
                input_tokens_24h: 100,
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
        assert_eq!(next_provider(&mut first, &health).as_deref(), Some("idle"));
        first.accept_current();

        let reservation = state.reservation(attempt_context, "idle:model".into());
        drop(reservation);

        let mut next = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &snapshot,
            state,
        );
        assert_eq!(next_provider(&mut next, &health).as_deref(), Some("idle"));
    }

    #[test]
    fn traffic_equalization_averages_available_price_ratios_and_falls_back_per_dimension() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("cached", 0), target("output", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "cached:model".into(),
                    cache_read_tokens_24h: 100,
                    cost_input: Some(2.0),
                    cost_output: Some(10.0),
                    cost_cache_read: None,
                    cost_cache_write: Some(12.0),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output:model".into(),
                    output_tokens_24h: 3,
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
        assert_eq!(
            next_provider(&mut policy, &health).as_deref(),
            Some("output")
        );

        let fallback_snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "cached:model".into(),
                    cache_write_tokens_24h: 1,
                    cost_input: Some(2.0),
                    cost_output: Some(10.0),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output:model".into(),
                    output_tokens_24h: 1,
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
        assert_eq!(
            next_provider(&mut fallback, &health).as_deref(),
            Some("output")
        );
    }

    #[test]
    fn latency_preference_requires_two_targets_with_twenty_successes() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("slow", 0), target("fast", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "slow:model".into(),
                    attempts_1h: 20,
                    successes_1h: 20,
                    successful_output_tokens_1h: 2_000,
                    successful_upstream_ms_1h: 20_000,
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "fast:model".into(),
                    attempts_1h: 25,
                    successes_1h: 20,
                    successful_output_tokens_1h: 4_000,
                    successful_upstream_ms_1h: 10_000,
                    ..Default::default()
                },
            ],
        };
        let mut policy =
            RouteAttemptPolicy::new("latency_preference", &targets, context(0), &snapshot, state);
        assert_eq!(next_provider(&mut policy, &health).as_deref(), Some("fast"));
    }

    #[test]
    fn latency_preference_falls_back_to_traffic_when_fewer_than_two_targets_have_data() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("sampled", 0), target("cold", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "sampled:model".into(),
                    input_tokens_24h: 100,
                    attempts_1h: 20,
                    successes_1h: 20,
                    successful_output_tokens_1h: 2_000,
                    successful_upstream_ms_1h: 1_000,
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "cold:model".into(),
                    attempts_1h: 19,
                    successes_1h: 19,
                    successful_output_tokens_1h: 19_000,
                    successful_upstream_ms_1h: 1_000,
                    ..Default::default()
                },
            ],
        };
        let mut policy =
            RouteAttemptPolicy::new("latency_preference", &targets, context(0), &snapshot, state);
        assert_eq!(next_provider(&mut policy, &health).as_deref(), Some("cold"));
    }

    #[test]
    fn transient_failures_retry_same_target_then_cool_it_down() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
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
        let flaky = policy.next_healthy(&health).expect("first Target");
        assert_eq!(flaky.provider_id, "flaky");
        assert_eq!(
            policy.record_failure(&health, &flaky, AiErrorKind::Timeout, false, None, 10, 1.0),
            AttemptFailureDisposition::RetrySame {
                delay: Duration::from_millis(500)
            }
        );
        assert_eq!(
            policy.record_failure(
                &health,
                &flaky,
                AiErrorKind::Timeout,
                false,
                None,
                1_000,
                1.0,
            ),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(
            next_provider(&mut policy, &health).as_deref(),
            Some("fallback")
        );

        let mut new_request = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(120_999),
            &RouteSchedulingSnapshot::default(),
            state.clone(),
        );
        assert_eq!(
            next_provider(&mut new_request, &health).as_deref(),
            Some("fallback")
        );

        let mut after_cooldown = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(121_001),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        assert_eq!(
            next_provider(&mut after_cooldown, &health).as_deref(),
            Some("flaky")
        );
    }

    #[test]
    fn default_retry_budget_uses_capped_exponential_full_jitter() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("flaky", 0), target("fallback", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        let flaky = policy.next_healthy(&health).expect("first Target");
        for cap_ms in [500, 1_000, 2_000, 4_000, 8_000] {
            assert_eq!(
                policy.record_failure(
                    &health,
                    &flaky,
                    AiErrorKind::ServerError,
                    false,
                    None,
                    0,
                    1.0,
                ),
                AttemptFailureDisposition::RetrySame {
                    delay: Duration::from_millis(cap_ms),
                }
            );
        }
        assert_eq!(
            policy.record_failure(
                &health,
                &flaky,
                AiErrorKind::ServerError,
                false,
                None,
                0,
                1.0,
            ),
            AttemptFailureDisposition::TryNextTarget
        );
    }

    #[test]
    fn retry_after_overrides_jitter_and_quota_moves_without_retry() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("first", 0), target("second", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        let first = policy.next_healthy(&health).expect("first Target");
        assert_eq!(
            policy.record_failure(
                &health,
                &first,
                AiErrorKind::RateLimitError,
                false,
                Some(Duration::from_secs(9)),
                0,
                0.0,
            ),
            AttemptFailureDisposition::RetrySame {
                delay: Duration::from_secs(9)
            }
        );
        assert_eq!(
            policy.record_failure(
                &health,
                &first,
                AiErrorKind::QuotaExceeded,
                false,
                None,
                0,
                0.0
            ),
            AttemptFailureDisposition::TryNextTarget
        );
    }

    #[test]
    fn committed_or_semantic_failures_stop_the_request() {
        let state = RoutePolicyState::default();
        let health = HealthRegistry::new();
        let targets = vec![target("first", 0), target("second", 0)];
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &RouteSchedulingSnapshot::default(),
            state,
        );
        let first = policy.next_healthy(&health).expect("first Target");
        assert_eq!(
            policy.record_failure(
                &health,
                &first,
                AiErrorKind::ServerError,
                true,
                None,
                0,
                0.0,
            ),
            AttemptFailureDisposition::Stop
        );
        assert_eq!(
            policy.record_failure(
                &health,
                &first,
                AiErrorKind::InvalidRequest,
                false,
                None,
                0,
                0.0
            ),
            AttemptFailureDisposition::Stop
        );
    }
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
