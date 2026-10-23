//! Layered Route Target selection and failure policy.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use parking_lot::Mutex;

use crate::db::identity::{ProviderId, TargetDestination, UpstreamModelId};
use crate::db::models::{RouteSelectionStrategy, TargetConfig};
use crate::runtime_cache::RuntimeCache;
use stravia_runtime_contract::protocol::ir::AiErrorKind;
use stravia_runtime_contract::protocol::ir::AiRequest;

#[derive(Debug, Clone)]
pub struct SelectedTarget {
    pub destination: TargetDestination,

    pub priority: i32,
    pub first_token_timeout_ms: i64,
    pub target_retry_budget: i32,
    pub target_cooldown_ms: i64,
    pub thinking_level_map: Vec<crate::thinking::ThinkingLevelMapping>,
}

impl SelectedTarget {
    pub fn provider_id(&self) -> &ProviderId {
        self.destination.provider_id()
    }

    pub fn model(&self) -> &UpstreamModelId {
        self.destination.model()
    }
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

#[derive(Clone, Copy)]
enum RetryAfterPolicy {
    Wait(Duration),
    Cooldown(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationIdentity {
    GenerationParent(String),
    PromptCacheKey(String),
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct TargetSchedulingSnapshot {
    pub(super) target_key: String,
    pub(super) input_tokens_1h: Option<i64>,
    pub(super) uncached_input_tokens_1h: Option<i64>,
    pub(super) output_tokens_1h: Option<i64>,
    pub(super) cache_read_tokens_1h: Option<i64>,
    pub(super) cache_write_tokens_1h: Option<i64>,
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
    /// ADR-0073：凭据失效的 Provider id 集合。失效 Provider 的全部 Target
    /// 在资格装配时被排除，与 `is_enabled` 同一层，不进入熔断状态机。
    pub(super) credential_invalid_providers: HashSet<String>,
    pub(super) allowance_suspended_providers: HashSet<String>,
}

/// Evidence `router::selection` assembles for one selection. Fields stay
/// router-private so callers can only obtain it through `RouteSelector`.
#[derive(Debug, Clone)]
pub struct RouteAttemptContext {
    pub(super) cooldown: CooldownAttempts,
    pub(super) principal: String,
    pub(super) route_id: String,
    pub(super) conversation: Option<ConversationIdentity>,
    pub(super) conversation_affinity_target: Option<String>,
    pub(super) cache_affinity_target: Option<String>,
    pub(super) estimated_uncached_input_tokens: u64,
    pub(super) now_ms: u64,
}

/// 同一个客户端根请求共享，隐藏续轮不能重新领取冷却例外。
#[derive(Debug, Clone, Default)]
pub(crate) struct CooldownAttempts {
    inner: Arc<Mutex<CooldownAttemptsInner>>,
}

#[derive(Debug, Default)]
struct CooldownAttemptsInner {
    selected: HashSet<String>,
    consumed: HashSet<String>,
    last_selected: Option<String>,
}

impl CooldownAttempts {
    fn claim(&self, key: &str) -> bool {
        self.inner.lock().consumed.insert(key.to_owned())
    }

    fn selected(&self, key: &str) {
        let mut inner = self.inner.lock();
        inner.selected.insert(key.to_owned());
        inner.last_selected = Some(key.to_owned());
    }

    fn available(&self, key: &str, preferred: Option<&str>) -> bool {
        let inner = self.inner.lock();
        (preferred == Some(key) || inner.selected.contains(key)) && !inner.consumed.contains(key)
    }
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
    runtime_cache: RuntimeCache,
    pricing_namespace: Arc<str>,
}

/// Failure and cooldown bookkeeping for one target. Entries are never removed, so the
/// globally monotonically increasing `epoch` survives recovery: a success or
/// failure recorded against an older epoch can never clear or rewrite a newer
/// cooldown generation (no ABA on delete/recreate).
#[derive(Debug, Clone, Default)]
struct TargetRuntime {
    consecutive_failures: u32,
    /// Cooldown expiry in `RoutePolicyState::now_ms` terms; `0` = not cooling.
    cooldown_until: u64,
    retry_after_until: u64,
    /// Generation of the latest cooldown write.
    epoch: u64,
    /// Epoch claimed by the in-flight half-open probe, if any. Only ever
    /// equals the entry's own `epoch`, so it doubles as probe identity.
    probe_epoch: Option<u64>,
    load: TargetLoad,
}

#[derive(Debug, Clone, Copy, Default)]
struct TargetLoad {
    attempts: u64,
    input_tokens: u64,
    last_selected: u64,
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
    selection_sequence: u64,
    conversation_targets: HashMap<ConversationAffinityKey, String>,
    /// `pricing_generation` bumps on every clear so a load that raced an
    /// invalidation can never reinsert stale data.
    pricing_generation: u64,
}

/// Pricing fields `scheduling_snapshot` needs from one Provider Model record.
/// Router-private: only ever produced and consumed inside `crate::router`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct TargetPricing {
    pub(super) cost_input: Option<f64>,
    pub(super) cost_output: Option<f64>,
    pub(super) cost_cache_read: Option<f64>,
    pub(super) cost_cache_write: Option<f64>,
}

/// Result of a `RoutePolicyState::pricing` probe.
pub(super) enum PricingProbe {
    /// Nothing cached: the caller must load and hand the carried generation
    /// back to `RoutePolicyState::store_pricing`.
    Miss(u64),
    /// `Some` prices, or `None` for a known not-found/no-cost result.
    Hit(Option<TargetPricing>),
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

    // 重试与跨请求失败只在这里累计；迟到结果不能改写新的冷却或探测世代。
    fn record_failure(
        &mut self,
        target_key: &str,
        epoch: u64,
        retry_budget: i32,
        cooldown_ms: i64,
        now_ms: u64,
        retry_after: Option<RetryAfterPolicy>,
    ) -> Option<u32> {
        let current = self.targets.get(target_key);
        if current.map_or(0, |runtime| runtime.epoch) != epoch {
            return None;
        }
        let consecutive_failures = current
            .map_or(0, |runtime| runtime.consecutive_failures)
            .saturating_add(1);
        let cooldown_ms = match retry_after {
            Some(RetryAfterPolicy::Cooldown(delay)) => delay.as_millis() as u64,
            _ => cooldown_ms.max(0) as u64,
        };
        let should_cool = cooldown_ms > 0
            && (matches!(retry_after, Some(RetryAfterPolicy::Cooldown(_)))
                || consecutive_failures > retry_budget.max(0) as u32
                || current.is_some_and(|runtime| runtime.probe_epoch == Some(epoch)));
        let load = current.map_or_else(TargetLoad::default, |runtime| runtime.load);
        let next = TargetRuntime {
            consecutive_failures,
            retry_after_until: match retry_after {
                Some(RetryAfterPolicy::Wait(delay)) => current
                    .map_or(0, |runtime| runtime.retry_after_until)
                    .max(now_ms.saturating_add(delay.as_millis().min(u64::MAX as u128) as u64)),
                Some(RetryAfterPolicy::Cooldown(_)) => now_ms.saturating_add(cooldown_ms),
                None => current.map_or(0, |runtime| runtime.retry_after_until),
            },
            cooldown_until: if should_cool {
                now_ms.saturating_add(cooldown_ms)
            } else {
                0
            },
            epoch: if should_cool {
                self.next_epoch()
            } else {
                epoch
            },
            probe_epoch: None,
            load,
        };
        if let Some(runtime) = self.targets.get_mut(target_key) {
            *runtime = next;
        } else {
            self.targets.insert(target_key.to_owned(), next);
        }
        Some(consecutive_failures)
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

#[cfg(test)]
impl Default for RoutePolicyState {
    fn default() -> Self {
        Self::new(RuntimeCache::tinyufo(1024 * 1024))
    }
}

impl RoutePolicyState {
    pub(crate) fn new(runtime_cache: RuntimeCache) -> Self {
        Self {
            origin: Instant::now(),
            inner: Arc::new(Mutex::new(RoutePolicyStateInner::default())),
            runtime_cache,
            pricing_namespace: uuid::Uuid::new_v4().to_string().into(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    pub(crate) fn permits_send(&self, target_key: &str, epoch: u64, single_attempt: bool) -> bool {
        let inner = self.inner.lock();
        inner.targets.get(target_key).map_or(epoch == 0, |runtime| {
            runtime.epoch == epoch
                && runtime.retry_after_until <= self.now_ms()
                && (runtime.cooldown_until == 0 || single_attempt)
        })
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
            runtime.consecutive_failures = 0;
            runtime.cooldown_until = 0;
            runtime.probe_epoch = None;
        }
        release_reservation(
            &mut inner.targets,
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

    /// 累计当前世代的一次上游失败，返回连续失败数；迟到结果返回 None。
    /// 超过重试预算才冷却，半开探测失败立即重新冷却；0 冷却仅关闭调度门禁。
    pub fn record_failure(
        &self,
        target_key: &str,
        epoch: u64,
        retry_budget: i32,
        cooldown_ms: i64,
    ) -> Option<u32> {
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock();
        inner.record_failure(target_key, epoch, retry_budget, cooldown_ms, now_ms, None)
    }

    pub(crate) fn record_failure_with_retry_after(
        &self,
        target: &SelectedTarget,
        epoch: u64,
        retry_after: Option<Duration>,
    ) {
        let target_key = selected_target_key(target);
        let now_ms = self.now_ms();
        let mut inner = self.inner.lock();
        inner.record_failure(
            &target_key,
            epoch,
            target.target_retry_budget,
            target.target_cooldown_ms,
            now_ms,
            retry_after_policy(target, retry_after),
        );
    }

    /// Provider 内部恢复会吞掉原失败，只有仍可继续时在这里计数。
    /// 达到阈值或失去资格时不计数，由执行器接收原错误并统一收口。
    pub(crate) fn try_record_recovery_failure(
        &self,
        target_key: &str,
        epoch: u64,
        retry_budget: i32,
        cooldown_ms: i64,
    ) -> bool {
        let mut inner = self.inner.lock();
        let current = inner.targets.get(target_key);
        if current.map_or(0, |runtime| runtime.epoch) != epoch
            || current.is_some_and(|runtime| runtime.cooldown_until != 0)
            || current.map_or(0, |runtime| runtime.consecutive_failures)
                >= retry_budget.max(0) as u32
        {
            return false;
        }
        inner.record_failure(
            target_key,
            epoch,
            retry_budget,
            cooldown_ms,
            self.now_ms(),
            None,
        );
        true
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
            &mut inner.targets,
            target_key,
            context.estimated_uncached_input_tokens,
        );
        if probe {
            inner.release_probe(target_key, epoch);
        }
    }

    /// Probe the shared pricing cache for `(provider_id, requested model)`.
    /// On `Miss` the caller loads outside the lock and stores the result under
    /// the carried generation; `Hit` covers both priced and negative entries.
    pub(super) async fn pricing(&self, provider_id: &str, model: &str) -> PricingProbe {
        let generation = self.inner.lock().pricing_generation;
        let key = self.pricing_key(generation, provider_id, model);
        let cached = self
            .runtime_cache
            .get::<Option<TargetPricing>>("pricing", &key)
            .await;
        let current_generation = self.inner.lock().pricing_generation;
        if current_generation != generation {
            return PricingProbe::Miss(current_generation);
        }
        match cached {
            Some(pricing) => PricingProbe::Hit(*pricing),
            None => PricingProbe::Miss(generation),
        }
    }

    /// Store a freshly loaded pricing result under the generation it was
    /// loaded against. A generation mismatch means an invalidation landed
    /// while the load was in flight, so the possibly-stale value is dropped.
    pub(super) async fn store_pricing(
        &self,
        generation: u64,
        provider_id: &str,
        model: &str,
        pricing: Option<TargetPricing>,
    ) {
        if self.inner.lock().pricing_generation != generation {
            return;
        }
        // A concurrent clear may land during the put. Its new generation has
        // a different key, so the late write can never become a current hit.
        let key = self.pricing_key(generation, provider_id, model);
        self.runtime_cache
            .put(
                "pricing",
                &key,
                Arc::new(pricing),
                std::mem::size_of::<Option<TargetPricing>>(),
                Duration::from_secs(86_400),
            )
            .await;
    }

    fn pricing_key(&self, generation: u64, provider_id: &str, model: &str) -> String {
        format!(
            "{}:{generation}:{}:{provider_id}{model}",
            self.pricing_namespace,
            provider_id.len()
        )
    }

    /// Drop every cached price: a local admin write committed or a config
    /// epoch change was observed. Bumping the generation keeps loads that started
    /// before this call from reinserting stale entries.
    pub(crate) fn clear_pricing(&self) {
        let mut inner = self.inner.lock();
        inner.pricing_generation = inner.pricing_generation.wrapping_add(1);
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

struct RouteCandidate {
    target: SelectedTarget,
    key: String,
    traffic: f64,
    latency: Option<f64>,
}

pub struct RouteAttemptPolicy {
    context: RouteAttemptContext,
    state: RoutePolicyState,
    ordered: Vec<RouteCandidate>,
    strategy: RouteSelectionStrategy,
    current_target_key: Option<String>,
    /// Runtime generation of the selected target; `0` when no record existed
    /// at selection time.
    current_epoch: u64,
    /// `true` while this policy holds the target's half-open probe slot.
    current_probe: bool,
    current_exception: bool,
    preferred: Option<String>,
}

impl RouteAttemptPolicy {
    /// Construction is router-private: callers receive a ready policy from
    /// `RouteSelector::select` and only drive it.
    pub(super) fn new(
        strategy: &str,
        targets: &[TargetConfig],
        context: RouteAttemptContext,
        snapshot: &RouteSchedulingSnapshot,
        state: RoutePolicyState,
    ) -> Self {
        // `context.now_ms` is the authoritative evidence clock — tests inject a
        // virtual clock through it — while `state.now_ms()` floors it so a
        // stale signal can never rewind the cooldown window in production.
        let now_ms = context.now_ms.max(state.now_ms());
        let preferred = {
            let inner = state.inner.lock();
            context
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
                })
                .or_else(|| context.cooldown.inner.lock().last_selected.clone())
        };
        let snapshots = snapshot
            .targets
            .iter()
            .map(|item| (item.target_key.as_str(), item))
            .collect::<HashMap<_, _>>();
        let mut priority_groups = BTreeMap::<Reverse<i32>, Vec<&TargetConfig>>::new();
        let inner = state.inner.lock();
        for target in targets {
            if !target.enabled {
                continue;
            }
            // ADR-0073：凭据失效的 Provider 不参与调度——失效是持久凭据
            // 属性，不是冷却；只有新凭据证据能恢复资格。
            if snapshot
                .credential_invalid_providers
                .contains(target.provider_id().as_str())
                || snapshot
                    .allowance_suspended_providers
                    .contains(target.provider_id().as_str())
            {
                continue;
            }
            let key = persisted_target_key(target);
            // Actively-cooling targets are ineligible. Half-open and probing
            // targets stay candidates: `next_healthy` is the single authority
            // that claims the one probe slot while actually selecting.
            if inner
                .targets
                .get(&key)
                .is_some_and(|runtime| runtime.cooldown_until > now_ms)
                && !context.cooldown.available(&key, preferred.as_deref())
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
            let weights = traffic_weights(&group, &snapshots);
            ordered.extend(group.into_iter().map(|target| {
                let key = persisted_target_key(target);
                let snapshot = snapshots.get(key.as_str()).copied();
                RouteCandidate {
                    target: to_selected(target),
                    traffic: traffic_score(snapshot, weights),
                    latency: snapshot.and_then(latency_score),
                    key,
                }
            }));
        }
        Self {
            context,
            state,
            ordered,
            strategy,
            current_target_key: None,
            current_epoch: 0,
            current_probe: false,
            current_exception: false,
            preferred,
        }
    }

    pub(crate) fn preferred_target_key(&self) -> Option<&str> {
        self.preferred.as_deref()
    }

    pub fn candidates(&self) -> impl Iterator<Item = &SelectedTarget> {
        self.ordered.iter().map(|candidate| &candidate.target)
    }

    /// 仅观察当前健康资格；RPM 等待不能提前占用半开槽或冷却例外。
    pub(crate) fn retain_eligible(&mut self, excluded: &HashSet<String>) {
        let inner = self.state.inner.lock();
        let now_ms = self.context.now_ms.max(self.state.now_ms());
        let cooldown = &self.context.cooldown;
        let preferred = self.preferred.as_deref();
        self.ordered.retain(|candidate| {
            let key = &candidate.key;
            if excluded.contains(key) {
                return false;
            }
            let Some(runtime) = inner.targets.get(key) else {
                return true;
            };
            if runtime.retry_after_until > now_ms {
                return false;
            }
            if runtime.cooldown_until == 0 {
                return true;
            }
            if runtime.cooldown_until > now_ms {
                return cooldown.available(key, preferred);
            }
            runtime.probe_epoch.is_none()
        });
    }

    /// 尚未发送时归还本策略领取的机会；调用者必须以发送状态作证。
    pub fn release_unsent_exception(&mut self) {
        if self.current_exception
            && let Some(key) = self.current_target_key.as_ref()
        {
            self.context.cooldown.inner.lock().consumed.remove(key);
            self.current_exception = false;
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

    pub fn retain(&mut self, mut predicate: impl FnMut(&SelectedTarget) -> bool) {
        self.ordered
            .retain(|candidate| predicate(&candidate.target));
    }

    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    pub fn next_healthy(&mut self) -> Option<SelectedTarget> {
        self.skip_current();
        let now_ms = self.context.now_ms.max(self.state.now_ms());
        let mut inner = self.state.inner.lock();
        let cooldown = &self.context.cooldown;
        let preferred = self.preferred.as_deref();
        self.ordered.retain(|candidate| {
            let Some(runtime) = inner.targets.get(&candidate.key) else {
                return true;
            };
            runtime.retry_after_until <= now_ms
                && (runtime.cooldown_until == 0
                    || if runtime.cooldown_until > now_ms {
                        cooldown.available(&candidate.key, preferred)
                    } else {
                        runtime.probe_epoch.is_none()
                    })
        });
        let priority = self.ordered.iter().map(|item| item.target.priority).max()?;
        let latency = self.strategy == RouteSelectionStrategy::LatencyPreference
            && self
                .ordered
                .iter()
                .filter(|item| item.target.priority == priority && item.latency.is_some())
                .take(2)
                .count()
                == 2;
        // 排序和占位共用同一临界区；提前准备的 policy 不能沿用旧的在途快照。
        let index = self
            .ordered
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                let left_load = inner
                    .targets
                    .get(&left.key)
                    .map_or_else(TargetLoad::default, |item| item.load);
                let right_load = inner
                    .targets
                    .get(&right.key)
                    .map_or_else(TargetLoad::default, |item| item.load);
                let affinity = (preferred == Some(right.key.as_str()))
                    .cmp(&(preferred == Some(left.key.as_str())));
                affinity
                    .then_with(|| right.target.priority.cmp(&left.target.priority))
                    .then_with(|| {
                        if latency {
                            right
                                .latency
                                .partial_cmp(&left.latency)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        } else {
                            left_load.attempts.cmp(&right_load.attempts).then_with(|| {
                                (left.traffic + left_load.input_tokens as f64)
                                    .partial_cmp(&(right.traffic + right_load.input_tokens as f64))
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            })
                        }
                    })
                    .then_with(|| left_load.last_selected.cmp(&right_load.last_selected))
            })
            .map(|(index, _)| index)?;
        let RouteCandidate { target, key, .. } = self.ordered.remove(index);
        let mut probe = false;
        let mut exception = false;
        if let Some(runtime) = inner.targets.get(&key)
            && runtime.cooldown_until > 0
        {
            if runtime.cooldown_until > now_ms {
                // 各 policy 可能共享同一 RootRequest，领取前仍须原子确认一次性名额。
                if !cooldown.claim(&key) {
                    drop(inner);
                    return self.next_healthy();
                }
                exception = true;
            } else {
                let epoch = inner.next_epoch();
                let runtime = inner.targets.get_mut(&key).expect("target entry");
                runtime.epoch = epoch;
                runtime.probe_epoch = Some(epoch);
                probe = true;
            }
        }
        inner.selection_sequence = inner.selection_sequence.saturating_add(1);
        let sequence = inner.selection_sequence;
        let runtime = inner.targets.entry(key.clone()).or_default();
        runtime.load.attempts = runtime.load.attempts.saturating_add(1);
        runtime.load.input_tokens = runtime
            .load
            .input_tokens
            .saturating_add(self.context.estimated_uncached_input_tokens);
        runtime.load.last_selected = sequence;
        self.current_epoch = runtime.epoch;
        self.current_probe = probe;
        self.current_exception = exception;
        cooldown.selected(&key);
        self.current_target_key = Some(key);
        Some(target)
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
                    runtime.retry_after_until <= self.context.now_ms.max(self.state.now_ms())
                        && runtime.epoch == self.current_epoch
                        && (runtime.cooldown_until == 0
                            || self.current_exception
                            || (self.current_probe
                                && runtime.probe_epoch == Some(self.current_epoch)))
                }
            }
        };
        let allowed = if !allowed && !self.current_exception {
            let inner = self.state.inner.lock();
            if let Some(runtime) = inner.targets.get(key)
                && runtime.cooldown_until > self.context.now_ms.max(self.state.now_ms())
                && runtime.retry_after_until <= self.context.now_ms.max(self.state.now_ms())
                && self.context.cooldown.claim(key)
            {
                self.current_epoch = runtime.epoch;
                self.current_exception = true;
                true
            } else {
                false
            }
        } else {
            allowed
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
        self.current_exception = false;
    }

    pub fn accept_current(&mut self) {
        self.current_target_key = None;
        self.current_probe = false;
        self.current_exception = false;
    }

    /// Epoch of the target generation currently selected by this policy.
    /// Callers pass it to `RoutePolicyState::record_success` and
    /// `RouteAttemptReservation` so stale in-flight attempts cannot rewrite
    /// newer state.
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    /// 半开探测与本请求的冷却例外都只允许一次实际尝试，禁用 transport 内部恢复。
    pub fn current_is_probe(&self) -> bool {
        self.current_probe || self.current_exception
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
        if matches!(
            kind,
            AiErrorKind::ContextLengthExceeded | AiErrorKind::ContentFiltered
        ) {
            self.skip_current();
            return AttemptFailureDisposition::Stop;
        }
        let retry_after = retry_after_policy(target, retry_after);
        let retry_after_wait = match retry_after {
            Some(RetryAfterPolicy::Wait(delay)) => Some(delay),
            _ => None,
        };
        let key = selected_target_key(target);
        // Never trust a stale signal timestamp over the evidence clock or the
        // state's own clock — the floor keeps cooldown writes monotonic.
        let now_ms = now_ms.max(self.context.now_ms).max(self.state.now_ms());
        let failures = self.state.inner.lock().record_failure(
            &key,
            self.current_epoch,
            target.target_retry_budget,
            target.target_cooldown_ms,
            now_ms,
            retry_after,
        );
        // 调用方只传入上游失败；除已提交输出和请求级错误外，任何上游错误都换 Target。
        if client_output_committed {
            self.skip_current();
            return AttemptFailureDisposition::Stop;
        }
        if !self.current_probe
            && !self.current_exception
            && !matches!(retry_after, Some(RetryAfterPolicy::Cooldown(_)))
            && (transient_failure(&kind)
                || kind == AiErrorKind::QuotaExceeded && retry_after_wait.is_some())
            && let Some(failures) = failures
            && failures <= target.target_retry_budget.max(0) as u32
            && (retry_after_wait.is_some() || self.retry_current())
        {
            let cap_ms = 500_u64
                .saturating_mul(1_u64 << (failures - 1).min(4))
                .min(8_000);
            let delay = retry_after_wait.unwrap_or_else(|| jitter(cap_ms, jitter_sample));
            return AttemptFailureDisposition::RetrySame { delay };
        }
        self.skip_current();
        AttemptFailureDisposition::TryNextTarget
    }

    /// Releases the input reservation and returns a held probe slot to
    /// `HalfOpen`, so cancellation or a semantic `Stop` never leaves the
    /// target stuck in `Probing` nor marks it `Available`.
    fn release_current(&self, key: &str) {
        let mut inner = self.state.inner.lock();
        release_reservation(
            &mut inner.targets,
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

#[derive(Clone, Copy)]
struct TrafficWeights {
    cache_read: f64,
    output: f64,
    cache_write: f64,
}

fn traffic_weights(
    group: &[&TargetConfig],
    snapshots: &HashMap<&str, &TargetSchedulingSnapshot>,
) -> TrafficWeights {
    let priced = group
        .iter()
        .filter_map(|target| {
            snapshots
                .get(persisted_target_key(target).as_str())
                .copied()
        })
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

fn traffic_score(snapshot: Option<&TargetSchedulingSnapshot>, weights: TrafficWeights) -> f64 {
    let (uncached_input, output, cache_read, cache_write) = snapshot
        .map(|snapshot| {
            (
                // 缓存读取完全未知时不给缓存折扣，但不改写确认用量的未知语义。
                snapshot
                    .uncached_input_tokens_1h
                    .or(snapshot.input_tokens_1h)
                    .unwrap_or_default(),
                snapshot.output_tokens_1h.unwrap_or_default(),
                snapshot.cache_read_tokens_1h.unwrap_or_default(),
                snapshot.cache_write_tokens_1h.unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    weights.cache_read * cache_read.max(0) as f64
        + uncached_input.max(0) as f64
        + weights.output * output.max(0) as f64
        + weights.cache_write * cache_write.max(0) as f64
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

fn retry_after_policy(
    target: &SelectedTarget,
    retry_after: Option<Duration>,
) -> Option<RetryAfterPolicy> {
    retry_after.map(|delay| {
        if target.first_token_timeout_ms > 0
            && delay > Duration::from_millis(target.first_token_timeout_ms as u64)
        {
            RetryAfterPolicy::Cooldown(delay.min(Duration::from_millis(
                target.target_cooldown_ms.max(0) as u64,
            )))
        } else {
            RetryAfterPolicy::Wait(delay)
        }
    })
}

fn release_reservation(targets: &mut HashMap<String, TargetRuntime>, key: &str, amount: u64) {
    let Some(runtime) = targets.get_mut(key) else {
        return;
    };
    runtime.load.attempts = runtime.load.attempts.saturating_sub(1);
    runtime.load.input_tokens = runtime.load.input_tokens.saturating_sub(amount);
}

pub fn conversation_identity(request: &AiRequest) -> Option<ConversationIdentity> {
    super::continuation::parent_id_from_request(request)
        .map(ConversationIdentity::GenerationParent)
        .or_else(|| {
            request
                .parameters
                .cache
                .key
                .as_ref()
                .map(|value| ConversationIdentity::PromptCacheKey(value.clone()))
        })
}

pub(crate) fn target_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}:{model}")
}

pub fn selected_target_key(target: &SelectedTarget) -> String {
    target_key(target.provider_id().as_str(), target.model().as_str())
}

fn persisted_target_key(target: &TargetConfig) -> String {
    target_key(target.provider_id().as_str(), target.model().as_str())
}

fn to_selected(target: &TargetConfig) -> SelectedTarget {
    SelectedTarget {
        destination: target.destination.clone(),
        priority: target.priority,
        first_token_timeout_ms: target.first_token_timeout_ms,
        target_retry_budget: target.target_retry_budget,
        target_cooldown_ms: target.target_cooldown_ms,
        thinking_level_map: target.thinking_level_map.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{
        DEFAULT_FIRST_TOKEN_TIMEOUT_MS, DEFAULT_TARGET_COOLDOWN_MS, DEFAULT_TARGET_RETRY_BUDGET,
    };

    fn target(provider_id: &str, priority: i32) -> TargetConfig {
        TargetConfig {
            output_modality_mismatch: false,
            route_output_modalities: Vec::new(),
            id: format!("target-{provider_id}").into(),
            model_id: "route".into(),
            destination: crate::db::identity::TargetDestination::Model {
                provider_id: provider_id.into(),
                model_id: "model".into(),
            },
            enabled: true,
            priority,
            first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
            target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
            target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
            created_at: String::new(),
            thinking_level_map: Vec::new(),
            model_name: None,
        }
    }

    #[test]
    fn target_keys_preserve_provider_and_model_identity() {
        assert_eq!(target_key("provider", "model"), "provider:model");
        assert_ne!(
            target_key("provider", "model"),
            target_key("provider", "other-model")
        );
        assert_ne!(
            target_key("provider", "model"),
            target_key("other-provider", "model")
        );
    }

    fn context(now_ms: u64) -> RouteAttemptContext {
        RouteAttemptContext {
            cooldown: CooldownAttempts::default(),
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
        policy
            .next_healthy()
            .map(|target| target.destination.into_parts().0.into())
    }

    #[test]
    fn route_scheduling_prepared_uncached_requests_spread_despite_history() {
        let state = RoutePolicyState::default();
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "b:model".into(),
                    input_tokens_1h: Some(1_000_000),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "c:model".into(),
                    input_tokens_1h: Some(1_000_000),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut policies = (0..12)
            .map(|_| {
                RouteAttemptPolicy::new(
                    "traffic_equalization",
                    &targets,
                    context(0),
                    &snapshot,
                    state.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut allocations = BTreeMap::new();
        for policy in &mut policies {
            *allocations
                .entry(next_provider(policy).unwrap())
                .or_insert(0) += 1;
        }
        assert_eq!(
            allocations,
            BTreeMap::from([("a".into(), 4), ("b".into(), 4), ("c".into(), 4)])
        );
    }

    #[test]
    fn cache_affinity_precedes_priority_and_concurrent_hourly_load() {
        for cached_priority in [0, 1] {
            let targets = vec![target("fresh", 1), target("cached", cached_priority)];
            let snapshot = RouteSchedulingSnapshot {
                targets: vec![TargetSchedulingSnapshot {
                    target_key: "cached:model".into(),
                    input_tokens_1h: Some(100),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let mut request = context(0);
            request.cache_affinity_target = Some("cached:model".into());
            let state = RoutePolicyState::default();
            let mut policies = (0..12)
                .map(|_| {
                    RouteAttemptPolicy::new(
                        "traffic_equalization",
                        &targets,
                        request.clone(),
                        &snapshot,
                        state.clone(),
                    )
                })
                .collect::<Vec<_>>();
            for policy in &mut policies {
                assert_eq!(next_provider(policy).as_deref(), Some("cached"));
            }
        }
    }

    #[test]
    fn idle_ties_rotate_after_success_and_cancellation_without_leaking_reservations() {
        let state = RoutePolicyState::default();
        let targets = vec![target("a", 0), target("b", 0)];
        let mut selected = Vec::new();
        for completed in [true, false, true, false] {
            let request = context(0);
            let mut policy = RouteAttemptPolicy::new(
                "traffic_equalization",
                &targets,
                request.clone(),
                &RouteSchedulingSnapshot::default(),
                state.clone(),
            );
            let target = policy.next_healthy().unwrap();
            selected.push(target.provider_id().to_string());
            if completed {
                let epoch = policy.current_epoch();
                policy.accept_current();
                state.record_success(&request, &selected_target_key(&target), epoch);
            }
        }
        assert_eq!(selected, ["a", "b", "a", "b"]);
    }

    #[test]
    fn simultaneous_policies_reserve_targets_atomically() {
        let state = RoutePolicyState::default();
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];
        let barrier = std::sync::Barrier::new(12);
        let policies = std::thread::scope(|scope| {
            let workers = (0..12)
                .map(|_| {
                    let state = state.clone();
                    let targets = &targets;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut policy = RouteAttemptPolicy::new(
                            "traffic_equalization",
                            targets,
                            context(0),
                            &RouteSchedulingSnapshot::default(),
                            state,
                        );
                        barrier.wait();
                        let selected = next_provider(&mut policy).unwrap();
                        (policy, selected)
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut allocations = BTreeMap::new();
        for (_, selected) in &policies {
            *allocations.entry(selected.as_str()).or_insert(0) += 1;
        }
        assert_eq!(allocations, BTreeMap::from([("a", 4), ("b", 4), ("c", 4)]));
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

    // ADR-0073：凭据失效 Provider 的 Target 在资格装配时被排除，
    // 与 enabled=false 同一层，不受优先级影响。
    #[test]
    fn credential_invalid_provider_targets_never_enter_attempt_order() {
        let state = RoutePolicyState::default();
        let targets = vec![target("dead", i32::MAX), target("alive", -1)];
        let snapshot = RouteSchedulingSnapshot {
            targets: Vec::new(),
            credential_invalid_providers: ["dead".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let mut policy = RouteAttemptPolicy::new(
            "traffic_equalization",
            &targets,
            context(0),
            &snapshot,
            state,
        );

        assert_eq!(next_provider(&mut policy).as_deref(), Some("alive"));
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
        super::super::continuation::stamp_previous_response_id(&mut request, "parent-a");
        request.parameters.cache.key = Some("cache-key".into());

        assert_eq!(
            conversation_identity(&request),
            Some(ConversationIdentity::GenerationParent("parent-a".into()))
        );
    }

    #[test]
    fn conversation_affinity_isolated_by_identity_principal_and_route() {
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
    }

    #[test]
    fn traffic_equalization_counts_uncached_input_once_and_in_flight_reservations() {
        let state = RoutePolicyState::default();
        let targets = vec![target("busy", 0), target("idle", 0)];
        let snapshot = RouteSchedulingSnapshot {
            targets: vec![TargetSchedulingSnapshot {
                target_key: "busy:model".into(),
                input_tokens_1h: Some(1_000),
                uncached_input_tokens_1h: Some(100),
                cache_read_tokens_1h: Some(900),
                ..Default::default()
            }],
            ..Default::default()
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
                input_tokens_1h: Some(100),
                ..Default::default()
            }],
            ..Default::default()
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
                    cache_read_tokens_1h: Some(100),
                    cost_input: Some(2.0),
                    cost_output: Some(10.0),
                    cost_cache_read: None,
                    cost_cache_write: Some(12.0),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output:model".into(),
                    output_tokens_1h: Some(3),
                    cost_input: Some(4.0),
                    cost_output: Some(20.0),
                    cost_cache_read: Some(0.8),
                    cost_cache_write: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
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
                    cache_write_tokens_1h: Some(1),
                    cost_input: Some(2.0),
                    cost_output: Some(10.0),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output:model".into(),
                    output_tokens_1h: Some(1),
                    cost_input: Some(4.0),
                    cost_output: Some(20.0),
                    ..Default::default()
                },
            ],
            ..Default::default()
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
            ..Default::default()
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
                    input_tokens_1h: Some(100),
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
            ..Default::default()
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
        assert_eq!(flaky.provider_id().as_str(), "flaky");
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

    #[tokio::test(start_paused = true)]
    async fn retry_after_waits_within_first_token_window_and_otherwise_cools() {
        for streamed in [false, true] {
            for (first_token_ms, cooldown_ms, retry_after, wait_ms, cooling, kind) in [
                (
                    60_000,
                    1_000,
                    Duration::from_secs(3),
                    3_000,
                    false,
                    AiErrorKind::RateLimitError,
                ),
                (
                    60_000,
                    1_000,
                    Duration::from_secs(3),
                    3_000,
                    false,
                    AiErrorKind::ServiceUnavailable,
                ),
                (
                    60_000,
                    120_000,
                    Duration::from_secs(60),
                    60_000,
                    false,
                    AiErrorKind::QuotaExceeded,
                ),
                (
                    60_000,
                    120_000,
                    Duration::from_secs(61),
                    61_000,
                    true,
                    AiErrorKind::QuotaExceeded,
                ),
                (
                    60_000,
                    120_000,
                    Duration::from_secs(7095),
                    120_000,
                    true,
                    AiErrorKind::RateLimitError,
                ),
                (
                    60_000,
                    0,
                    Duration::from_secs(7095),
                    0,
                    true,
                    AiErrorKind::RateLimitError,
                ),
                (
                    0,
                    1_000,
                    Duration::from_secs(70),
                    70_000,
                    false,
                    AiErrorKind::RateLimitError,
                ),
                (
                    60_000,
                    120_000,
                    Duration::MAX,
                    120_000,
                    true,
                    AiErrorKind::RateLimitError,
                ),
            ] {
                let state = RoutePolicyState::default();
                let mut configured = target("limited", 0);
                configured.first_token_timeout_ms = first_token_ms;
                configured.target_cooldown_ms = cooldown_ms;
                let targets = [configured];
                let mut policy = policy_at(&state, &targets, 0);
                let limited = policy.next_healthy().expect("initial Target");
                if streamed {
                    state.record_failure_with_retry_after(
                        &limited,
                        policy.current_epoch(),
                        Some(retry_after),
                    );
                } else {
                    assert_eq!(
                        policy.record_failure(
                            &limited,
                            AttemptFailureSignal {
                                retry_after: Some(retry_after),
                                ..failure_at(kind, 0)
                            },
                        ),
                        if cooling {
                            AttemptFailureDisposition::TryNextTarget
                        } else {
                            AttemptFailureDisposition::RetrySame {
                                delay: Duration::from_millis(wait_ms),
                            }
                        },
                    );
                }
                drop(policy);
                if cooling && wait_ms > 0 {
                    let status = state.target_status("limited:model");
                    assert_eq!(status.state, TargetRuntimeState::CoolingDown);
                    assert_eq!(status.cooldown_remaining_ms, Some(wait_ms));
                }

                if wait_ms > 0 {
                    tokio::time::advance(Duration::from_millis(wait_ms - 1)).await;
                    let mut waiting = policy_at(&state, &targets, wait_ms - 1);
                    waiting.retain_eligible(&HashSet::new());
                    assert!(waiting.is_empty(), "must respect the effective wait");
                    assert!(
                        policy_at(&state, &targets, wait_ms - 1)
                            .next_healthy()
                            .is_none()
                    );
                    tokio::time::advance(Duration::from_millis(1)).await;
                }
                let mut recovered = policy_at(&state, &targets, wait_ms);
                recovered.retain_eligible(&HashSet::new());
                assert_eq!(
                    next_provider(&mut recovered).as_deref(),
                    Some("limited"),
                    "streamed={streamed}, cooldown_ms={cooldown_ms}, retry_after={retry_after:?}",
                );
            }
        }
    }

    #[test]
    fn committed_output_and_request_level_failures_stop_the_request() {
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
        for kind in [
            AiErrorKind::ContextLengthExceeded,
            AiErrorKind::ContentFiltered,
        ] {
            assert_eq!(
                policy.record_failure(
                    &first,
                    AttemptFailureSignal {
                        kind,
                        client_output_committed: false,
                        retry_after: None,
                        now_ms: 0,
                        jitter_sample: 0.0,
                    },
                ),
                AttemptFailureDisposition::Stop
            );
        }
    }

    #[test]
    fn non_transient_upstream_failures_try_the_next_target() {
        for kind in [
            AiErrorKind::AuthenticationError,
            AiErrorKind::AuthorizationError,
            AiErrorKind::InvalidRequest,
            AiErrorKind::NotFoundError,
            AiErrorKind::Unknown,
        ] {
            let state = RoutePolicyState::default();
            let targets = vec![target("first", 0), target("second", 0)];
            let mut policy = policy_at(&state, &targets, 0);
            let first = policy.next_healthy().expect("first Target");
            assert_eq!(
                policy.record_failure(
                    &first,
                    AttemptFailureSignal {
                        kind: kind.clone(),
                        client_output_committed: false,
                        retry_after: None,
                        now_ms: 0,
                        jitter_sample: 0.0,
                    },
                ),
                AttemptFailureDisposition::TryNextTarget,
                "{kind}"
            );
            assert_eq!(
                state.target_status(&selected_target_key(&first)).state,
                TargetRuntimeState::Available,
                "{kind} 只计入共享失败数，未超过重试预算不得冷却"
            );
        }
    }

    #[test]
    fn auth_failures_cool_only_after_the_shared_threshold() {
        for kind in [
            AiErrorKind::AuthenticationError,
            AiErrorKind::AuthorizationError,
        ] {
            let state = RoutePolicyState::default();
            let mut configured = target("denied", 0);
            configured.target_retry_budget = 1;
            configured.target_cooldown_ms = 120_000;
            let targets = vec![configured, target("fallback", 0)];
            for (attempt, expected) in [
                (0, TargetRuntimeState::Available),
                (1, TargetRuntimeState::CoolingDown),
            ] {
                let mut policy = policy_at(&state, &targets, attempt * 1_000);
                let denied = policy.next_healthy().expect("first Target");
                assert_eq!(denied.provider_id().as_str(), "denied", "{kind} #{attempt}");
                assert_eq!(
                    policy.record_failure(
                        &denied,
                        AttemptFailureSignal {
                            kind: kind.clone(),
                            client_output_committed: false,
                            retry_after: None,
                            now_ms: attempt * 1_000,
                            jitter_sample: 0.0,
                        },
                    ),
                    AttemptFailureDisposition::TryNextTarget,
                    "{kind} #{attempt}"
                );
                assert_eq!(next_provider(&mut policy).as_deref(), Some("fallback"));
                assert_eq!(
                    state.target_status(&selected_target_key(&denied)).state,
                    expected,
                    "{kind} #{attempt}"
                );
            }
            let mut next_request = policy_at(&state, &targets, 2_000);
            assert_eq!(
                next_provider(&mut next_request).as_deref(),
                Some("fallback"),
                "{kind}"
            );
        }
    }

    fn policy_at(
        state: &RoutePolicyState,
        targets: &[TargetConfig],
        now_ms: u64,
    ) -> RouteAttemptPolicy {
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

    #[test]
    fn stream_and_internal_failures_share_the_target_threshold() {
        let state = RoutePolicyState::default();
        let targets = vec![target("recovering", 1), target("fallback", 0)];
        for _ in 0..3 {
            let mut request = policy_at(&state, &targets, 0);
            let selected = request.next_healthy().expect("target");
            assert_eq!(selected.provider_id().as_str(), "recovering");
            state.record_failure("recovering:model", request.current_epoch(), 5, 120_000);
            assert_eq!(
                state.target_status("recovering:model").state,
                TargetRuntimeState::Available
            );
        }
        let mut request = policy_at(&state, &targets, 0);
        let selected = request.next_healthy().expect("target");
        assert_eq!(selected.provider_id().as_str(), "recovering");
        for _ in 0..2 {
            assert_eq!(
                request.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
                AttemptFailureDisposition::RetrySame {
                    delay: Duration::ZERO
                }
            );
        }
        assert_eq!(
            request.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 0)).as_deref(),
            Some("fallback")
        );
    }

    #[test]
    fn completed_request_resets_shared_failures_but_cancellation_does_not() {
        let state = RoutePolicyState::default();
        let mut configured = target("recovering", 1);
        configured.target_retry_budget = 1;
        let targets = vec![configured, target("fallback", 0)];
        let mut first = policy_at(&state, &targets, 0);
        let selected = first.next_healthy().expect("target");
        assert!(matches!(
            first.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
            AttemptFailureDisposition::RetrySame { .. }
        ));
        drop(first);

        let mut successful = policy_at(&state, &targets, 0);
        assert_eq!(
            next_provider(&mut successful).as_deref(),
            Some("recovering")
        );
        state.record_success(
            successful.context(),
            "recovering:model",
            successful.current_epoch(),
        );
        successful.accept_current();

        let mut request = policy_at(&state, &targets, 0);
        let selected = request.next_healthy().expect("target after success");
        assert_eq!(selected.provider_id().as_str(), "recovering");
        assert!(matches!(
            request.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
            AttemptFailureDisposition::RetrySame { .. }
        ));
        drop(request);
        // 取消另一个已选中但尚未完成的请求，不应抹去上一次失败。
        let mut cancelled = policy_at(&state, &targets, 0);
        assert_eq!(next_provider(&mut cancelled).as_deref(), Some("recovering"));
        drop(cancelled);
        let mut last = policy_at(&state, &targets, 0);
        let selected = last.next_healthy().expect("last available attempt");
        assert_eq!(
            last.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 0)).as_deref(),
            Some("fallback")
        );
    }

    #[test]
    fn concurrent_request_failures_reach_one_shared_threshold() {
        let state = RoutePolicyState::default();
        let targets = vec![target("recovering", 1), target("fallback", 0)];
        let barrier = std::sync::Barrier::new(6);
        std::thread::scope(|scope| {
            for _ in 0..6 {
                let state = &state;
                let targets = &targets;
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut request = policy_at(state, targets, 0);
                    assert_eq!(next_provider(&mut request).as_deref(), Some("recovering"));
                    barrier.wait();
                    state.record_failure("recovering:model", request.current_epoch(), 5, 120_000);
                });
            }
        });
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 0)).as_deref(),
            Some("fallback")
        );
    }

    #[test]
    fn provider_recovery_and_outer_failures_do_not_double_count_the_threshold() {
        let state = RoutePolicyState::default();
        let mut configured = target("recovering", 1);
        configured.target_retry_budget = 2;
        let targets = vec![configured, target("fallback", 0)];
        let mut request = policy_at(&state, &targets, 0);
        let selected = request.next_healthy().expect("target");
        assert!(state.try_record_recovery_failure("recovering:model", 0, 2, 120_000));
        drop(request);
        let mut request = policy_at(&state, &targets, 0);
        assert_eq!(next_provider(&mut request).as_deref(), Some("recovering"));
        assert!(matches!(
            request.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
            AttemptFailureDisposition::RetrySame { .. }
        ));
        assert!(!state.try_record_recovery_failure("recovering:model", 0, 2, 120_000));
        assert_eq!(
            state.target_status("recovering:model").state,
            TargetRuntimeState::Available
        );
        assert_eq!(
            request.record_failure(&selected, failure_at(AiErrorKind::Timeout, 0)),
            AttemptFailureDisposition::TryNextTarget
        );
        assert_eq!(
            next_provider(&mut policy_at(&state, &targets, 0)).as_deref(),
            Some("fallback")
        );
    }

    fn cooled_targets() -> (RoutePolicyState, Vec<TargetConfig>) {
        let state = RoutePolicyState::default();
        let mut recovering = target("recovering", 1);
        recovering.target_retry_budget = 0;
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
        assert_eq!(selected.provider_id().as_str(), "recovering");
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
        assert_eq!(selected.provider_id().as_str(), "recovering");
        assert_eq!(
            second_probe.record_failure(&selected, failure_at(AiErrorKind::InvalidRequest, 12_000)),
            AttemptFailureDisposition::TryNextTarget
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
        state.record_failure("recovering:model", old_epoch, 0, 1_000);
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
    fn newer_cooldown_preserves_selected_exception_and_rejects_stale_success() {
        let state = RoutePolicyState::default();
        let mut recovering = target("recovering", 1);
        recovering.target_retry_budget = 1;
        let targets = vec![recovering, target("fallback", 0)];
        let mut old = policy_at(&state, &targets, 10_000);
        let mut opener = policy_at(&state, &targets, 10_000);
        let old_target = old.next_healthy().expect("old request");
        let original_epoch = old.current_epoch();
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
        assert!(old.retry_current());
        state.record_success(old.context(), "recovering:model", original_epoch);
        assert_eq!(
            state.target_status_at("recovering:model", 10_001).state,
            TargetRuntimeState::CoolingDown
        );
        let exception_epoch = old.current_epoch();
        assert_eq!(
            old.record_failure(&old_target, failure_at(AiErrorKind::Timeout, 10_001)),
            AttemptFailureDisposition::TryNextTarget
        );
        state.record_success(old.context(), "recovering:model", exception_epoch);
        assert_eq!(
            state.target_status_at("recovering:model", 10_002).state,
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
        filtered.retain(|target| target.provider_id().as_str() != "recovering");
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
    fn expired_cooldown_remains_exclusive_when_load_balancing_changes_probe_order() {
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
        assert_eq!(recovering.provider_id().as_str(), "recovering");
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

        let mut policies = (0..3)
            .map(|_| {
                RouteAttemptPolicy::new(
                    "traffic_equalization",
                    &targets,
                    context(121_000),
                    &RouteSchedulingSnapshot::default(),
                    state.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut allocations = BTreeMap::new();
        for policy in &mut policies {
            *allocations
                .entry(next_provider(policy).unwrap())
                .or_insert(0) += 1;
        }
        assert_eq!(
            allocations,
            BTreeMap::from([("recovering".into(), 1), ("fallback".into(), 2)])
        );
    }

    #[tokio::test]
    async fn stale_pricing_load_cannot_repopulate_after_invalidation() {
        let state = RoutePolicyState::default();
        let stale = TargetPricing {
            cost_input: Some(1.0),
            ..Default::default()
        };
        let fresh = TargetPricing {
            cost_input: Some(9.0),
            ..Default::default()
        };

        // A load that probed before invalidation must not reinsert afterwards.
        let PricingProbe::Miss(stale_generation) = state.pricing("p", "m").await else {
            panic!("empty cache must miss")
        };
        state.clear_pricing();
        state
            .store_pricing(stale_generation, "p", "m", Some(stale))
            .await;
        assert!(
            matches!(state.pricing("p", "m").await, PricingProbe::Miss(_)),
            "pre-invalidation load must not repopulate the cache"
        );

        // Negative results cache identically to priced ones until the next
        // invalidation.
        let PricingProbe::Miss(generation) = state.pricing("p", "missing").await else {
            panic!("missing model must miss")
        };
        state.store_pricing(generation, "p", "missing", None).await;
        assert!(matches!(
            state.pricing("p", "missing").await,
            PricingProbe::Hit(None)
        ));

        let PricingProbe::Miss(generation) = state.pricing("p", "m").await else {
            panic!("post-clear cache must miss")
        };
        state.store_pricing(generation, "p", "m", Some(fresh)).await;
        match state.pricing("p", "m").await {
            PricingProbe::Hit(Some(pricing)) => assert_eq!(pricing.cost_input, Some(9.0)),
            _ => panic!("stored price must hit"),
        }
        state.clear_pricing();
        assert!(
            matches!(state.pricing("p", "m").await, PricingProbe::Miss(_))
                && matches!(state.pricing("p", "missing").await, PricingProbe::Miss(_)),
            "clear must drop positive and negative entries"
        );
    }
}
