//! Route Target selection assembly.
//!
//! `RouteSelector` is the single owner of the ADR-0034 input pipeline: it loads
//! every piece of selection evidence (Targets, Cache Affinity, Target
//! Continuation, usage/cost scheduling snapshot) and returns a
//! `RouteAttemptPolicy` that callers only drive. Callers never assemble
//! `RouteAttemptContext` themselves, so precedence cannot drift at call sites.

use std::sync::Arc;

use rust_decimal::prelude::ToPrimitive;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::request::MediaRoutingPlan;

use crate::db::models::{RouteConfig, TargetConfig};
use crate::interaction_observation::{RunEvent, RunObserver};
use crate::storage::DynStorage;

use super::cache_affinity::CacheAffinity;
use super::continuation::ContinuationLookup;
use super::selector::{
    ConversationIdentity, RouteAttemptContext, RouteAttemptPolicy, RoutePolicyState,
    RouteSchedulingSnapshot, TargetSchedulingSnapshot, conversation_identity, selected_target_key,
    target_key,
};

/// Why selection could not produce a policy.
#[derive(Debug)]
pub(crate) enum SelectionError {
    /// Scheduling evidence (usage stats, provider-model cost join) failed to load.
    SchedulingEvidence(anyhow::Error),
    /// Eligibility filtering left the Route without any Target to attempt.
    NoEligibleTarget,
    /// A fixed Media routing plan excluded every candidate Target.
    MediaPlanExhausted,
}

#[derive(Clone)]
pub(crate) struct RouteSelector {
    storage: DynStorage,
    cache_affinity: CacheAffinity,
    continuation: Arc<dyn ContinuationLookup>,
    policy_state: RoutePolicyState,
}

impl RouteSelector {
    pub(crate) fn new(
        storage: DynStorage,
        cache_affinity: CacheAffinity,
        continuation: Arc<dyn ContinuationLookup>,
        policy_state: RoutePolicyState,
    ) -> Self {
        Self {
            storage,
            cache_affinity,
            continuation,
            policy_state,
        }
    }

    /// Assemble a ready-to-drive `RouteAttemptPolicy` for one request.
    ///
    /// ADR-0034 precedence is enforced by the produced policy: Target
    /// Continuation and Conversation/Cache Affinity enter as hints, cooldown and
    /// health stay eligibility filters, and Target Priority + the route's
    /// scheduling strategy order the remainder.
    pub(crate) async fn select(
        &self,
        principal: &Principal,
        route: &RouteConfig,
        request: &AiRequest,
        media_plan: Option<&MediaRoutingPlan>,
        observer: Option<&RunObserver>,
    ) -> Result<RouteAttemptPolicy, SelectionError> {
        let conversation = conversation_identity(request);
        // Continuation evidence only exists for generation-parent conversations.
        let conversation_affinity_target = if matches!(
            conversation,
            Some(ConversationIdentity::GenerationParent(_))
        ) {
            self.continuation.preferred_target(principal, request).await
        } else {
            None
        };
        let snapshot = self
            .scheduling_snapshot(&route.targets, observer)
            .await
            .map_err(SelectionError::SchedulingEvidence)?;
        let context = RouteAttemptContext {
            principal: principal.continuation_key(),
            route_id: route.id.clone().into(),
            conversation,
            conversation_affinity_target,
            cache_affinity_target: self
                .cache_affinity
                .preferred_target(principal, &route.id, request),
            estimated_uncached_input_tokens: estimate_uncached_input_tokens(request),
            now_ms: self.policy_state.now_ms(),
        };
        let mut policy = RouteAttemptPolicy::new(
            &route.balance,
            &route.targets,
            context,
            &snapshot,
            self.policy_state.clone(),
        );
        if let Some(plan) = media_plan {
            policy.retain(|target| plan.target_keys.contains(&selected_target_key(target)));
            if policy.is_empty() {
                return Err(SelectionError::MediaPlanExhausted);
            }
        }
        if policy.is_empty() {
            return Err(SelectionError::NoEligibleTarget);
        }
        Ok(policy)
    }

    /// Assemble scheduling for an operation that has no generation history,
    /// continuation, or prompt-cache identity (for example full search or
    /// media generation). It uses the same Target eligibility, priority,
    /// cooldown, retry, and scheduling evidence as model turns.
    pub(crate) async fn select_independent(
        &self,
        principal: &Principal,
        route: &RouteConfig,
        estimated_input_tokens: u64,
        observer: Option<&RunObserver>,
    ) -> Result<RouteAttemptPolicy, SelectionError> {
        let snapshot = self
            .scheduling_snapshot(&route.targets, observer)
            .await
            .map_err(SelectionError::SchedulingEvidence)?;
        let context = RouteAttemptContext {
            principal: principal.continuation_key(),
            route_id: route.id.clone().into(),
            conversation: None,
            conversation_affinity_target: None,
            cache_affinity_target: None,
            estimated_uncached_input_tokens: estimated_input_tokens,
            now_ms: self.policy_state.now_ms(),
        };
        let policy = RouteAttemptPolicy::new(
            &route.balance,
            &route.targets,
            context,
            &snapshot,
            self.policy_state.clone(),
        );
        if policy.is_empty() {
            return Err(SelectionError::NoEligibleTarget);
        }
        Ok(policy)
    }

    /// Usage statistics per target joined with provider-model prices, plus a
    /// default entry per configured Target so every candidate has a snapshot.
    async fn scheduling_snapshot(
        &self,
        targets: &[TargetConfig],
        observer: Option<&RunObserver>,
    ) -> anyhow::Result<RouteSchedulingSnapshot> {
        let usage = self.storage.usage_stats().route_scheduling_snapshot().await;
        if usage.stale
            && let Some(observer) = observer
        {
            observer.record(RunEvent::ObservationGap {
                reason: "usage_stats_snapshot_stale".into(),
            });
        }
        let mut snapshot = RouteSchedulingSnapshot {
            targets: usage.targets,
            credential_invalid_providers: self
                .storage
                .providers()
                .credential_invalid_provider_ids()
                .await?,
        };
        for target in targets {
            let key = target_key(
                target.provider_id().as_str(),
                target.model().map(|model| model.as_str()),
            );
            let index = snapshot
                .targets
                .iter()
                .position(|item| item.target_key == key)
                .unwrap_or_else(|| {
                    snapshot.targets.push(TargetSchedulingSnapshot {
                        target_key: key.clone(),
                        ..Default::default()
                    });
                    snapshot.targets.len() - 1
                });
            let Some(model) = target.model().map(|model| model.as_str()) else {
                continue;
            };
            let Some(provider_model) = self
                .storage
                .provider_models()
                .find(target.provider_id().as_str(), model)
                .await?
            else {
                continue;
            };
            let Some(cost) = provider_model.metadata.cost else {
                continue;
            };
            let target_snapshot = &mut snapshot.targets[index];
            target_snapshot.cost_input = cost.prices.input.and_then(|value| value.to_f64());
            target_snapshot.cost_output = cost.prices.output.and_then(|value| value.to_f64());
            target_snapshot.cost_cache_read =
                cost.prices.cache_read.and_then(|value| value.to_f64());
            target_snapshot.cost_cache_write =
                cost.prices.cache_write.and_then(|value| value.to_f64());
        }
        Ok(snapshot)
    }
}

fn estimate_uncached_input_tokens(request: &AiRequest) -> u64 {
    serde_json::to_vec(&request.items)
        .map(|bytes| bytes.len().div_ceil(4) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use rust_decimal::Decimal;

    use super::*;
    use crate::db::models::{
        DEFAULT_FIRST_TOKEN_TIMEOUT_MS, DEFAULT_TARGET_COOLDOWN_MS, DEFAULT_TARGET_RETRY_BUDGET,
        TargetConfig,
    };
    use crate::provider_models::{
        ModelCost, NewProviderModelRecord, PriceComponents, ProviderModelMetadata,
        ProviderModelMutation, ProviderModelPresence, ProviderModelReconciliation,
        ProviderModelRecord, ProviderModelSelectionPolicy, ProviderModelSourceKind, SnapshotState,
    };
    use crate::router::continuation::ContinuationTarget;
    use crate::router::selector::{
        AttemptFailureDisposition, AttemptFailureSignal, ConversationIdentity, RouteAttemptContext,
        TargetSchedulingSnapshot,
    };
    use crate::storage::MemoryStorage;
    use crate::storage::traits::{
        AdminIdentityStore, ApiKeyStore, AuthAccessStore, OAuthCredentialStore, ProviderModelStore,
        ProviderStore, RouteSchedulingUsage, RouteStore, SettingsStore, Storage, StorageBootstrap,
        UsageStatsStore,
    };
    use stravia_runtime_contract::protocol::ir::request::MediaRoutingMode;
    use stravia_runtime_contract::protocol::ir::{
        AiErrorKind, AiItem, MessageContent, OpenResponsesExt, ProtocolExt, Role, Usage,
    };

    fn target(provider: &str, priority: i32) -> TargetConfig {
        TargetConfig {
            id: format!("target-{provider}").into(),
            model_id: "route".into(),
            destination: crate::db::identity::TargetDestination::Model {
                provider_id: provider.into(),
                model_id: "model".into(),
            },
            enabled: true,
            priority,
            first_token_timeout_ms: DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
            target_retry_budget: DEFAULT_TARGET_RETRY_BUDGET,
            target_cooldown_ms: DEFAULT_TARGET_COOLDOWN_MS,
            created_at: String::new(),
            thinking_level_map: Vec::new(),
        }
    }

    fn route(targets: Vec<TargetConfig>) -> RouteConfig {
        RouteConfig {
            id: "route-id".into(),
            model_id: "route".into(),
            display_name: None,
            balance: "traffic_equalization".into(),

            is_enabled: true,
            created_at: String::new(),
            supported_thinking_levels: Vec::new(),
            context_window: None,
            output_max_tokens: None,
            supports_image_input: false,
            targets,
            default_thinking_level: None,
        }
    }

    fn user_item(text: &str) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn request() -> AiRequest {
        AiRequest::new("route", vec![user_item("hello")])
    }

    fn principal() -> Principal {
        Principal::new("test")
    }

    /// Continuation lookup returning a fixed preferred target.
    struct StaticContinuation(Option<String>);

    #[async_trait]
    impl ContinuationLookup for StaticContinuation {
        async fn preferred_target(
            &self,
            _principal: &Principal,
            _request: &AiRequest,
        ) -> Option<String> {
            self.0.clone()
        }

        async fn prepare(
            &self,
            _principal: &Principal,
            _target: ContinuationTarget<'_>,
            _request: &mut AiRequest,
        ) -> Option<String> {
            None
        }
    }

    /// Continuation lookup counting `preferred_target` consultations.
    struct CountingContinuation(Arc<AtomicUsize>);

    #[async_trait]
    impl ContinuationLookup for CountingContinuation {
        async fn preferred_target(
            &self,
            _principal: &Principal,
            _request: &AiRequest,
        ) -> Option<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        }

        async fn prepare(
            &self,
            _principal: &Principal,
            _target: ContinuationTarget<'_>,
            _request: &mut AiRequest,
        ) -> Option<String> {
            None
        }
    }

    struct Fixture {
        selector: RouteSelector,
        policy_state: RoutePolicyState,
        cache_affinity: CacheAffinity,
    }

    fn fixture(storage: DynStorage, continuation: Arc<dyn ContinuationLookup>) -> Fixture {
        let cache_affinity = CacheAffinity::default();
        let policy_state = RoutePolicyState::default();
        Fixture {
            selector: RouteSelector::new(
                storage,
                cache_affinity.clone(),
                continuation,
                policy_state.clone(),
            ),
            policy_state,
            cache_affinity,
        }
    }

    fn memory() -> DynStorage {
        Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new()))
    }

    fn no_continuation() -> Arc<dyn ContinuationLookup> {
        Arc::new(StaticContinuation(None))
    }

    fn next_provider(policy: &mut RouteAttemptPolicy) -> Option<String> {
        policy
            .next_healthy()
            .map(|target| target.destination.into_parts().0.into())
    }

    fn large_usage() -> Usage {
        Usage {
            prompt_tokens: 20_000,
            required_components_known: true,
            ..Default::default()
        }
    }

    /// Storage backed by `MemoryStorage` whose usage snapshot and provider-model
    /// lookup can be shaped per test.
    struct FixtureStorage {
        delegate: MemoryStorage,
        usage: FixtureUsageStats,
        models: FixtureProviderModels,
    }

    impl FixtureStorage {
        fn new() -> Self {
            let delegate = MemoryStorage::new(Vec::new(), Vec::new(), Vec::new());
            Self {
                usage: FixtureUsageStats {
                    delegate: delegate.clone(),
                    snapshot: RouteSchedulingUsage::default(),
                },
                models: FixtureProviderModels {
                    delegate: delegate.clone(),
                    fail_find: false,
                },
                delegate,
            }
        }

        fn with_usage(mut self, snapshot: RouteSchedulingUsage) -> Self {
            self.usage.snapshot = snapshot;
            self
        }

        fn failing_provider_models(mut self) -> Self {
            self.models.fail_find = true;
            self
        }
    }

    struct FixtureUsageStats {
        delegate: MemoryStorage,
        snapshot: RouteSchedulingUsage,
    }

    #[async_trait]
    impl UsageStatsStore for FixtureUsageStats {
        async fn route_scheduling_snapshot(&self) -> RouteSchedulingUsage {
            self.snapshot.clone()
        }

        async fn stats_overview(
            &self,
            hours: Option<i64>,
        ) -> anyhow::Result<crate::db::models::StatsOverview> {
            self.delegate.stats_overview(hours).await
        }

        async fn stats_series(
            &self,
            hours: i64,
            bucket_ms: i64,
            tz_offset_ms: i64,
        ) -> anyhow::Result<Vec<crate::db::models::StatsSeries>> {
            self.delegate
                .stats_series(hours, bucket_ms, tz_offset_ms)
                .await
        }

        async fn stats_by_model(
            &self,
            hours: Option<i64>,
        ) -> anyhow::Result<Vec<crate::db::models::ModelStats>> {
            self.delegate.stats_by_model(hours).await
        }

        async fn stats_by_provider(
            &self,
            hours: Option<i64>,
        ) -> anyhow::Result<Vec<crate::db::models::ProviderStats>> {
            self.delegate.stats_by_provider(hours).await
        }

        async fn stats_by_api_key(
            &self,
            hours: Option<i64>,
        ) -> anyhow::Result<Vec<crate::db::models::ApiKeyStats>> {
            self.delegate.stats_by_api_key(hours).await
        }
    }

    struct FixtureProviderModels {
        delegate: MemoryStorage,
        fail_find: bool,
    }

    #[async_trait]
    impl ProviderModelStore for FixtureProviderModels {
        async fn list_for_provider(
            &self,
            provider_id: &str,
        ) -> anyhow::Result<Vec<ProviderModelRecord>> {
            self.delegate.list_for_provider(provider_id).await
        }

        async fn get(
            &self,
            provider_id: &str,
            model_id: &str,
        ) -> anyhow::Result<Option<ProviderModelRecord>> {
            ProviderModelStore::get(&self.delegate, provider_id, model_id).await
        }

        async fn find(
            &self,
            provider_id: &str,
            model_id: &str,
        ) -> anyhow::Result<Option<ProviderModelRecord>> {
            if self.fail_find {
                anyhow::bail!("provider model lookup unavailable");
            }
            ProviderModelStore::find(&self.delegate, provider_id, model_id).await
        }

        async fn apply_reconciliation(
            &self,
            provider_id: &str,
            reconciliation: ProviderModelReconciliation,
        ) -> anyhow::Result<()> {
            self.delegate
                .apply_reconciliation(provider_id, reconciliation)
                .await
        }

        async fn create(
            &self,
            input: NewProviderModelRecord,
        ) -> anyhow::Result<ProviderModelMutation> {
            ProviderModelStore::create(&self.delegate, input).await
        }

        async fn update_metadata(
            &self,
            provider_id: &str,
            model_id: &str,
            metadata: ProviderModelMetadata,
            snapshot_state: SnapshotState,
            expected_revision: i64,
        ) -> anyhow::Result<ProviderModelMutation> {
            ProviderModelStore::update_metadata(
                &self.delegate,
                provider_id,
                model_id,
                metadata,
                snapshot_state,
                expected_revision,
            )
            .await
        }

        async fn update_selection_policy(
            &self,
            provider_id: &str,
            model_id: &str,
            policy: ProviderModelSelectionPolicy,
            expected_revision: i64,
        ) -> anyhow::Result<ProviderModelMutation> {
            ProviderModelStore::update_selection_policy(
                &self.delegate,
                provider_id,
                model_id,
                policy,
                expected_revision,
            )
            .await
        }

        async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
            ProviderModelStore::delete_manual(&self.delegate, provider_id, model_id).await
        }
    }

    impl Storage for FixtureStorage {
        fn vendor_plugins(&self) -> &crate::plugin::PluginStore {
            self.delegate.vendor_plugins()
        }

        fn providers(&self) -> &dyn ProviderStore {
            self.delegate.providers()
        }

        fn web_providers(&self) -> Option<&dyn crate::storage::traits::WebProviderStore> {
            self.delegate.web_providers()
        }

        fn routes(&self) -> &dyn RouteStore {
            self.delegate.routes()
        }

        fn provider_models(&self) -> &dyn ProviderModelStore {
            &self.models
        }

        fn settings(&self) -> &dyn SettingsStore {
            self.delegate.settings()
        }

        fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
            self.delegate.api_keys()
        }

        fn auth(&self) -> Option<&dyn AuthAccessStore> {
            self.delegate.auth()
        }

        fn admin_identity(&self) -> Option<&dyn AdminIdentityStore> {
            self.delegate.admin_identity()
        }

        fn usage_stats(&self) -> &dyn UsageStatsStore {
            &self.usage
        }

        fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
            self.delegate.oauth_credentials()
        }

        fn bootstrap(&self) -> &dyn StorageBootstrap {
            self.delegate.bootstrap()
        }
    }

    #[tokio::test]
    async fn select_orders_targets_by_signed_priority() {
        let fixture = fixture(memory(), no_continuation());
        let route = route(vec![target("low", 0), target("high", 10)]);
        let mut policy = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .expect("select");

        assert_eq!(next_provider(&mut policy).as_deref(), Some("high"));
        assert_eq!(next_provider(&mut policy).as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn disabled_and_cooling_targets_are_not_eligible() {
        let fixture = fixture(memory(), no_continuation());
        let mut cooling = target("cooling", 10);
        cooling.target_retry_budget = 0;
        let route = route(vec![cooling, target("healthy", 0)]);

        let mut first = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .expect("first select");
        let failed = first.next_healthy().expect("cooling target selected first");
        assert_eq!(failed.provider_id().as_str(), "cooling");
        assert_eq!(
            first.record_failure(
                &failed,
                AttemptFailureSignal {
                    kind: AiErrorKind::ServiceUnavailable,
                    client_output_committed: false,
                    retry_after: None,
                    now_ms: fixture.policy_state.now_ms(),
                    jitter_sample: 0.5,
                },
            ),
            AttemptFailureDisposition::TryNextTarget
        );
        drop(first);

        let mut second = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .expect("second select");
        assert_eq!(next_provider(&mut second).as_deref(), Some("healthy"));
        assert_eq!(next_provider(&mut second), None);
    }

    #[tokio::test]
    async fn stored_conversation_affinity_is_preferred_over_priority() {
        let fixture = fixture(memory(), no_continuation());
        let mut request = request();
        request.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            prompt_cache_key: Some("chat-a".into()),
            ..Default::default()
        }));
        fixture.policy_state.record_success(
            &RouteAttemptContext {
                principal: principal().continuation_key(),
                route_id: "route-id".into(),
                conversation: Some(ConversationIdentity::PromptCacheKey("chat-a".into())),
                conversation_affinity_target: None,
                cache_affinity_target: None,
                estimated_uncached_input_tokens: 0,
                now_ms: 0,
            },
            "affinity:model",
            0,
        );
        let route = route(vec![target("primary", 10), target("affinity", 0)]);

        let mut policy = fixture
            .selector
            .select(&principal(), &route, &request, None, None)
            .await
            .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("affinity"));
    }

    #[tokio::test]
    async fn cache_affinity_applies_only_without_conversation_identity() {
        let fixture = fixture(memory(), no_continuation());
        let seeded = request();
        fixture.cache_affinity.record_success(
            &principal(),
            "route-id",
            &seeded,
            "affinity:model",
            &large_usage(),
        );
        let route = route(vec![target("primary", 10), target("affinity", 0)]);

        let mut policy = fixture
            .selector
            .select(&principal(), &route, &seeded, None, None)
            .await
            .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("affinity"));

        // A conversation identity suppresses the cache hint entirely.
        let mut identified = request();
        identified.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            prompt_cache_key: Some("chat-b".into()),
            ..Default::default()
        }));
        let mut identified_policy = fixture
            .selector
            .select(&principal(), &route, &identified, None, None)
            .await
            .expect("select with conversation");
        assert_eq!(
            next_provider(&mut identified_policy).as_deref(),
            Some("primary")
        );
    }

    #[tokio::test]
    async fn continuation_lookup_runs_only_for_generation_parents() {
        let consultations = Arc::new(AtomicUsize::new(0));
        let fixture = fixture(
            memory(),
            Arc::new(CountingContinuation(Arc::clone(&consultations))),
        );
        let route = route(vec![target("a", 0)]);

        let mut cache_key_only = request();
        cache_key_only.ext = Some(ProtocolExt::OpenResponses(OpenResponsesExt {
            prompt_cache_key: Some("chat-c".into()),
            ..Default::default()
        }));
        fixture
            .selector
            .select(&principal(), &route, &cache_key_only, None, None)
            .await
            .expect("select");
        assert_eq!(consultations.load(Ordering::SeqCst), 0);

        let mut parent = request();
        crate::router::stamp_previous_response_id(&mut parent, "parent-1");
        fixture
            .selector
            .select(&principal(), &route, &parent, None, None)
            .await
            .expect("select with parent");
        assert_eq!(consultations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn independent_selection_carries_provider_only_target_without_generation_hints() {
        let consultations = Arc::new(AtomicUsize::new(0));
        let fixture = fixture(
            memory(),
            Arc::new(CountingContinuation(Arc::clone(&consultations))),
        );
        let mut provider_only = target("research", 0);
        provider_only.destination = crate::db::identity::TargetDestination::ProviderOnly {
            provider_id: provider_only.provider_id().clone(),
        };
        let route = route(vec![provider_only]);

        let mut policy = fixture
            .selector
            .select_independent(&principal(), &route, 12_345, None)
            .await
            .expect("independent selection");
        let selected = policy.next_healthy().expect("Provider-only Target");

        assert_eq!(consultations.load(Ordering::SeqCst), 0);
        assert_eq!(selected.provider_id().as_str(), "research");
        assert!(selected.model().is_none());
    }

    #[tokio::test]
    async fn continuation_preferred_target_leads_the_attempt_order() {
        let fixture = fixture(
            memory(),
            Arc::new(StaticContinuation(Some("continued:model".into()))),
        );
        let mut request = request();
        crate::router::stamp_previous_response_id(&mut request, "parent-2");
        let route = route(vec![target("primary", 10), target("continued", 0)]);

        let mut policy = fixture
            .selector
            .select(&principal(), &route, &request, None, None)
            .await
            .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("continued"));
    }

    #[tokio::test]
    async fn media_plan_restricts_candidates_and_exhaustion_is_typed() {
        let fixture = fixture(memory(), no_continuation());
        let route = route(vec![target("native", 0), target("other", 0)]);
        let plan = MediaRoutingPlan {
            mode: MediaRoutingMode::Native,
            target_keys: vec!["native:model".into()],
            source_artifact_ids: Vec::new(),
        };

        let mut policy = fixture
            .selector
            .select(&principal(), &route, &request(), Some(&plan), None)
            .await
            .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("native"));
        assert_eq!(next_provider(&mut policy), None);

        let empty_plan = MediaRoutingPlan {
            mode: MediaRoutingMode::Native,
            target_keys: vec!["missing:model".into()],
            source_artifact_ids: Vec::new(),
        };
        let error = fixture
            .selector
            .select(&principal(), &route, &request(), Some(&empty_plan), None)
            .await
            .err()
            .expect("plan without candidates");
        assert!(matches!(error, SelectionError::MediaPlanExhausted));
    }

    #[tokio::test]
    async fn route_without_enabled_targets_is_a_typed_no_target_error() {
        let fixture = fixture(memory(), no_continuation());
        let mut disabled = target("disabled", 0);
        disabled.enabled = false;
        let route = route(vec![disabled]);

        let error = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .err()
            .expect("no eligible target");
        assert!(matches!(error, SelectionError::NoEligibleTarget));
    }

    #[tokio::test]
    async fn provider_model_lookup_failure_is_a_typed_evidence_error() {
        let storage = Arc::new(FixtureStorage::new().failing_provider_models());
        let fixture = fixture(storage, no_continuation());
        let route = route(vec![target("a", 0)]);

        let error = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .err()
            .expect("evidence failure");
        assert!(matches!(error, SelectionError::SchedulingEvidence(_)));
    }

    #[tokio::test]
    async fn usage_snapshot_drives_traffic_equalization_and_stays_usable_when_stale() {
        let storage = Arc::new(FixtureStorage::new().with_usage(RouteSchedulingUsage {
            stale: true,
            targets: vec![TargetSchedulingSnapshot {
                target_key: "busy:model".into(),
                input_tokens_24h: Some(1_000_000),
                ..Default::default()
            }],
        }));
        let fixture = fixture(storage, no_continuation());
        let route = route(vec![target("busy", 0), target("idle", 0)]);

        let mut policy = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .expect("stale snapshot still selects");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("idle"));
    }

    #[tokio::test]
    async fn provider_model_costs_feed_traffic_weights() {
        let storage = Arc::new(FixtureStorage::new().with_usage(RouteSchedulingUsage {
            stale: false,
            targets: vec![
                TargetSchedulingSnapshot {
                    target_key: "cache_read_heavy:model".into(),
                    input_tokens_24h: Some(1_000_000),
                    cache_read_tokens_24h: Some(1_000_000),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output_heavy:model".into(),
                    output_tokens_24h: Some(100_000),
                    ..Default::default()
                },
            ],
        }));
        // Cache-read-heavy usage only outweighs output usage once the joined
        // cost ratio (cache_read/input = 50) replaces the default 0.1 weight.
        let cost = ModelCost {
            prices: PriceComponents {
                input: Some(Decimal::from(1)),
                output: Some(Decimal::from(1)),
                cache_read: Some(Decimal::from(50)),
                ..Default::default()
            },
            ..Default::default()
        };
        for provider in ["cache_read_heavy", "output_heavy"] {
            let applied = storage
                .provider_models()
                .create(NewProviderModelRecord {
                    provider_id: provider.into(),
                    model_id: "model".into(),
                    source_kind: ProviderModelSourceKind::Discovered,
                    snapshot_state: SnapshotState::Imported {
                        source: crate::provider_models::SourceStamp::Discovery,
                    },
                    metadata_source_provider_id: None,
                    presence: ProviderModelPresence::Present,
                    selection_policy: ProviderModelSelectionPolicy::Auto,
                    metadata: ProviderModelMetadata {
                        cost: Some(cost.clone()),
                        ..Default::default()
                    },
                })
                .await
                .expect("provider model created");
            assert!(matches!(applied, ProviderModelMutation::Applied(_)));
        }
        let fixture = fixture(storage, no_continuation());
        let route = route(vec![
            target("cache_read_heavy", 0),
            target("output_heavy", 0),
        ]);

        let mut policy = fixture
            .selector
            .select(&principal(), &route, &request(), None, None)
            .await
            .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("output_heavy"));
    }
}
