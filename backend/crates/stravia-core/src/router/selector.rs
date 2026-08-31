//! Target selection strategies for the proxy routing layer.
//!
//! # Architecture
//!
//! Each strategy implements the [`RoutingStrategy`] trait and returns an
//! ordered `Vec<SelectedTarget>` — the dispatcher tries them in order and
//! stops on the first successful upstream response.
//!
//! | Strategy   | Description                                             |
//! |------------|---------------------------------------------------------|
//! | `weighted` | Weighted reservoir sampling (default)                   |
//! | `priority` | Priority groups; random within a group                  |
//! | `cooldown` | Deprioritises recently-used targets (round-robin style) |
//! | `latency`  | Ascending EMA response-latency order                    |
//!
//! # Usage
//!
//! ```rust,ignore
//! // Dispatcher
//! let ordered = TargetSelector::select_ordered(&route.balance, &targets);
//! // After a successful call (for stateful strategies):
//! TargetSelector::record_selected(&route.balance, &target_key);
//! TargetSelector::record_latency(&route.balance, &target_key, elapsed_ms);
//! ```

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::db::models::{RouteSelectionStrategy, Target};

// ── SelectedTarget ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SelectedTarget {
    pub provider_id: String,
    pub model: String,
    pub thinking_level_map: Vec<crate::thinking::ThinkingLevelMapping>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptFailureDisposition {
    Retry,
    Stop,
}

/// Shared Target ordering, health, and retry/failover policy for every Route
/// operation. Attempt budgets and backoff remain intentionally unspecified.
pub struct RouteAttemptPolicy {
    balance: String,
    ordered: std::vec::IntoIter<SelectedTarget>,
}

impl RouteAttemptPolicy {
    pub fn new(balance: &str, targets: &[Target]) -> Self {
        Self {
            balance: balance.to_owned(),
            ordered: TargetSelector::select_ordered(balance, targets).into_iter(),
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

    /// Promotes one currently eligible Target while retaining the balance
    /// strategy's relative order for every other Target.
    pub fn prefer(
        &mut self,
        preferred_target_key: Option<&str>,
        health: &crate::router::health::HealthRegistry,
    ) {
        let Some(preferred_target_key) = preferred_target_key else {
            return;
        };
        let mut ordered = self.ordered.by_ref().collect::<Vec<_>>();
        let Some(index) = ordered.iter().position(|target| {
            selected_target_key(target) == preferred_target_key
                && health.is_healthy(preferred_target_key)
        }) else {
            self.ordered = ordered.into_iter();
            return;
        };
        let preferred = ordered.remove(index);
        ordered.insert(0, preferred);
        self.ordered = ordered.into_iter();
    }

    pub fn is_empty(&self) -> bool {
        self.ordered.as_slice().is_empty()
    }

    pub fn next_healthy(
        &mut self,
        health: &crate::router::health::HealthRegistry,
    ) -> Option<SelectedTarget> {
        self.ordered
            .by_ref()
            .find(|target| health.is_healthy(&selected_target_key(target)))
    }

    pub fn record_success(
        &self,
        health: &crate::router::health::HealthRegistry,
        target: &SelectedTarget,
        latency_ms: f64,
    ) {
        let target_key = selected_target_key(target);
        health.record_success(&target_key);
        TargetSelector::record_selected(&self.balance, &target_key);
        TargetSelector::record_latency(&self.balance, &target_key, latency_ms);
    }

    pub fn record_failure(
        &self,
        health: &crate::router::health::HealthRegistry,
        target: &SelectedTarget,
        retryable: bool,
        client_output_committed: bool,
    ) -> AttemptFailureDisposition {
        health.record_failure(&selected_target_key(target));
        if retryable && !client_output_committed {
            AttemptFailureDisposition::Retry
        } else {
            AttemptFailureDisposition::Stop
        }
    }
}

pub fn selected_target_key(target: &SelectedTarget) -> String {
    format!("{}:{}", target.provider_id, target.model)
}

// ── RoutingStrategy trait ─────────────────────────────────────────────────────

/// Produces an ordered list of targets to try, from most to least preferred.
pub trait RoutingStrategy: Send + Sync {
    fn select_ordered(&self, targets: &[Target]) -> Vec<SelectedTarget>;
}

// ── Weighted ──────────────────────────────────────────────────────────────────

pub struct WeightedStrategy;

impl RoutingStrategy for WeightedStrategy {
    fn select_ordered(&self, targets: &[Target]) -> Vec<SelectedTarget> {
        let refs: Vec<&Target> = targets.iter().filter(|t| t.weight > 0).collect();
        weighted_shuffle(&refs)
            .into_iter()
            .map(to_selected)
            .collect()
    }
}

// ── Priority ──────────────────────────────────────────────────────────────────

pub struct PriorityStrategy;

impl RoutingStrategy for PriorityStrategy {
    fn select_ordered(&self, targets: &[Target]) -> Vec<SelectedTarget> {
        let mut groups: BTreeMap<i32, Vec<&Target>> = BTreeMap::new();
        for t in targets {
            groups.entry(t.priority).or_default().push(t);
        }
        groups
            .into_values()
            .flat_map(|group| group.into_iter().map(to_selected))
            .collect()
    }
}

// ── Cooldown ──────────────────────────────────────────────────────────────────

/// Cooldown window: a target is fully "cooled" after this duration.
const COOLDOWN: Duration = Duration::from_secs(60);

/// Process-wide cooldown state. Tracks when each target was last selected.
pub struct CooldownStrategy {
    last_selected: RwLock<HashMap<String, Instant>>,
}

impl CooldownStrategy {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<CooldownStrategy> = OnceLock::new();
        INSTANCE.get_or_init(|| CooldownStrategy {
            last_selected: RwLock::new(HashMap::new()),
        })
    }

    /// Mark `target_key` as just selected.
    pub fn record_selected(&self, target_key: &str) {
        if let Ok(mut map) = self.last_selected.write() {
            map.insert(target_key.to_string(), Instant::now());
        }
    }
}

impl RoutingStrategy for CooldownStrategy {
    fn select_ordered(&self, targets: &[Target]) -> Vec<SelectedTarget> {
        let map = self.last_selected.read().unwrap_or_else(|p| p.into_inner());
        let mut scored: Vec<(&Target, Duration)> = targets
            .iter()
            .map(|t| {
                let key = target_key(t);
                // How long since this target was last selected (capped at COOLDOWN).
                // Targets never selected are treated as fully cooled.
                let cooled_for = map
                    .get(&key)
                    .map(|inst| inst.elapsed().min(COOLDOWN))
                    .unwrap_or(COOLDOWN);
                (t, cooled_for)
            })
            .collect();
        // Coolest (longest unused) first.
        scored.sort_by_key(|(_, cooled_for)| Reverse(*cooled_for));
        scored.into_iter().map(|(t, _)| to_selected(t)).collect()
    }
}

// ── Latency ───────────────────────────────────────────────────────────────────

/// EMA smoothing factor: 20% weight to new observations.
const LATENCY_ALPHA: f64 = 0.2;

/// Process-wide latency state. Tracks EMA response latency per target.
pub struct LatencyStrategy {
    ema: RwLock<HashMap<String, f64>>,
}

impl LatencyStrategy {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<LatencyStrategy> = OnceLock::new();
        INSTANCE.get_or_init(|| LatencyStrategy {
            ema: RwLock::new(HashMap::new()),
        })
    }

    /// Record a new latency observation for `target_key`.
    pub fn record_latency(&self, target_key: &str, latency_ms: f64) {
        if let Ok(mut map) = self.ema.write() {
            let entry = map.entry(target_key.to_string()).or_insert(latency_ms);
            *entry = LATENCY_ALPHA * latency_ms + (1.0 - LATENCY_ALPHA) * (*entry);
        }
    }
}

impl RoutingStrategy for LatencyStrategy {
    fn select_ordered(&self, targets: &[Target]) -> Vec<SelectedTarget> {
        let map = self.ema.read().unwrap_or_else(|p| p.into_inner());
        let mut scored: Vec<(&Target, f64)> = targets
            .iter()
            .map(|t| {
                let key = target_key(t);
                // No observation yet → 0 ms (unobserved targets go first).
                let ema = map.get(&key).copied().unwrap_or(0.0);
                (t, ema)
            })
            .collect();
        // Fastest first.
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(t, _)| to_selected(t)).collect()
    }
}

// ── TargetSelector (public entry point) ───────────────────────────────────────

pub struct TargetSelector;

impl TargetSelector {
    /// Return targets ordered by the named balance. Unrecognised balance
    /// strings fall back to `weighted`.
    pub fn select_ordered(balance: &str, targets: &[Target]) -> Vec<SelectedTarget> {
        match RouteSelectionStrategy::from_str(balance).unwrap_or_default() {
            RouteSelectionStrategy::Weighted => WeightedStrategy.select_ordered(targets),
            RouteSelectionStrategy::Priority => PriorityStrategy.select_ordered(targets),
            RouteSelectionStrategy::Cooldown => CooldownStrategy::global().select_ordered(targets),
            RouteSelectionStrategy::Latency => LatencyStrategy::global().select_ordered(targets),
        }
    }

    /// Record that `target_key` was successfully selected.
    /// Only meaningful for the `cooldown` balance; a no-op for others.
    pub fn record_selected(balance: &str, target_key: &str) {
        if RouteSelectionStrategy::from_str(balance).unwrap_or_default()
            == RouteSelectionStrategy::Cooldown
        {
            CooldownStrategy::global().record_selected(target_key);
        }
    }

    /// Record observed response latency for `target_key`.
    /// Only meaningful for the `latency` balance; a no-op for others.
    pub fn record_latency(balance: &str, target_key: &str, latency_ms: f64) {
        if RouteSelectionStrategy::from_str(balance).unwrap_or_default()
            == RouteSelectionStrategy::Latency
        {
            LatencyStrategy::global().record_latency(target_key, latency_ms);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[inline]
fn target_key(t: &Target) -> String {
    format!("{}:{}", t.provider_id, t.model)
}

#[inline]
fn to_selected(t: &Target) -> SelectedTarget {
    SelectedTarget {
        provider_id: t.provider_id.clone(),
        model: t.model.clone(),
        thinking_level_map: t.thinking_level_map.0.clone(),
    }
}

fn weighted_shuffle<'a>(targets: &[&'a Target]) -> Vec<&'a Target> {
    if targets.is_empty() {
        return vec![];
    }
    let mut items: Vec<(&Target, f64)> = targets
        .iter()
        .map(|t| {
            let weight = t.weight.max(1) as f64;
            let key = rand::random::<f64>().powf(1.0 / weight);
            (*t, key)
        })
        .collect();
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    items.into_iter().map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::health::HealthRegistry;

    fn target(provider_id: &str, priority: i32) -> Target {
        Target {
            id: format!("target-{provider_id}"),
            model_id: "route".into(),
            provider_id: provider_id.into(),
            model: "image-model".into(),
            weight: 1,
            priority,
            created_at: String::new(),
            thinking_level_map: sqlx::types::Json(Vec::new()),
        }
    }

    #[test]
    fn route_attempt_policy_retries_only_before_client_output_commit() {
        let health = HealthRegistry::new();
        let targets = vec![target("first", 0), target("second", 1)];
        let mut attempts = RouteAttemptPolicy::new("priority", &targets);

        let first = attempts.next_healthy(&health).expect("first Target");
        assert_eq!(first.provider_id, "first");
        assert_eq!(
            attempts.record_failure(&health, &first, true, false),
            AttemptFailureDisposition::Retry
        );
        assert_eq!(
            attempts
                .next_healthy(&health)
                .expect("failover Target")
                .provider_id,
            "second"
        );

        let mut committed_attempt = RouteAttemptPolicy::new("priority", &targets);
        let first = committed_attempt
            .next_healthy(&health)
            .expect("first Target");
        assert_eq!(
            committed_attempt.record_failure(&health, &first, true, true),
            AttemptFailureDisposition::Stop
        );
    }

    #[test]
    fn preferred_healthy_target_moves_first_without_reordering_fallbacks() {
        let health = HealthRegistry::new();
        let targets = vec![target("first", 0), target("second", 1), target("third", 2)];
        let mut attempts = RouteAttemptPolicy::new("priority", &targets);
        attempts.prefer(Some("second:image-model"), &health);
        assert_eq!(
            attempts
                .next_healthy(&health)
                .expect("preferred Target")
                .provider_id,
            "second"
        );
        assert_eq!(
            attempts
                .next_healthy(&health)
                .expect("first fallback Target")
                .provider_id,
            "first"
        );
        assert_eq!(
            attempts
                .next_healthy(&health)
                .expect("second fallback Target")
                .provider_id,
            "third"
        );

        let mut unavailable_preference = RouteAttemptPolicy::new("priority", &targets);
        unavailable_preference.prefer(Some("removed:image-model"), &health);
        assert_eq!(
            unavailable_preference
                .next_healthy(&health)
                .expect("balanced Target")
                .provider_id,
            "first"
        );
    }
}
