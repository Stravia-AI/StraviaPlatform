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
    ConversationIdentity, CooldownAttempts, PricingProbe, RouteAttemptContext, RouteAttemptPolicy,
    RoutePolicyState, RouteSchedulingSnapshot, TargetPricing, TargetSchedulingSnapshot,
    conversation_identity, selected_target_key, target_key,
};

/// Why selection could not produce a policy.
#[derive(Debug)]
pub(crate) enum SelectionError {
    UnconfiguredRoute,
    /// Scheduling evidence (usage stats, provider-model cost join) failed to load.
    SchedulingEvidence(anyhow::Error),
    /// Eligibility filtering left the Route without any Target to attempt.
    NoEligibleTarget,
    /// Allowance suspension mixed with other eligibility exclusions.
    MixedAllowanceExclusions,
    AllowanceSuspended,
    /// A fixed Media routing plan excluded every candidate Target.
    MediaPlanExhausted,
}

#[derive(Clone)]
pub(crate) struct RouteSelector {
    storage: DynStorage,
    cache_affinity: CacheAffinity,
    continuation: Arc<dyn ContinuationLookup>,
    policy_state: RoutePolicyState,
    /// Live currency snapshot shared with the Gateway; custom-currency price
    /// fields are converted into USD at this rate before deriving weights.
    custom_currency: Arc<parking_lot::RwLock<Option<Arc<crate::pricing::CustomCurrency>>>>,
}

impl RouteSelector {
    pub(crate) fn new(
        storage: DynStorage,
        cache_affinity: CacheAffinity,
        continuation: Arc<dyn ContinuationLookup>,
        policy_state: RoutePolicyState,
        custom_currency: Arc<parking_lot::RwLock<Option<Arc<crate::pricing::CustomCurrency>>>>,
    ) -> Self {
        Self {
            storage,
            cache_affinity,
            continuation,
            policy_state,
            custom_currency,
        }
    }

    /// Assemble a ready-to-drive `RouteAttemptPolicy` for one request.
    ///
    /// ADR-0034 precedence is enforced by the produced policy: Target
    /// Continuation and Conversation/Cache Affinity enter as preferred targets,
    /// cooldown and health stay eligibility filters, and Target Priority + the
    /// route's scheduling strategy order the remainder.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        target = "stravia::perf",
        name = "router.select",
        skip_all,
        fields(status)
    )]
    pub(crate) async fn select(
        &self,
        principal: &Principal,
        route: &RouteConfig,
        request: &AiRequest,
        media_plan: Option<&MediaRoutingPlan>,
        observer: Option<&RunObserver>,
        estimated_input_tokens: u64,
        cooldown: &CooldownAttempts,
        parent_target: &crate::router::GenerationParentTarget,
    ) -> Result<RouteAttemptPolicy, SelectionError> {
        let eligible = self.modality_eligible_targets(route, false).await?;
        let conversation = conversation_identity(request);
        let conversation_affinity_target = match (&conversation, parent_target) {
            (
                Some(ConversationIdentity::GenerationParent(_)),
                crate::router::GenerationParentTarget::Unresolved,
            ) => self.continuation.preferred_target(principal, request).await,
            (
                Some(ConversationIdentity::GenerationParent(id)),
                resolved @ crate::router::GenerationParentTarget::Resolved(_),
            ) => resolved.preferred_target(principal, id),
            _ => None,
        };
        let snapshot = self
            .scheduling_snapshot(&route.targets, observer)
            .await
            .map_err(SelectionError::SchedulingEvidence)?;
        let context = RouteAttemptContext {
            cooldown: cooldown.clone(),
            principal: principal.continuation_key(),
            route_id: route.id.clone().into(),
            conversation,
            conversation_affinity_target,
            cache_affinity_target: self
                .cache_affinity
                .preferred_target(principal, &route.id, request)
                .await,
            estimated_uncached_input_tokens: estimated_input_tokens,
            now_ms: self.policy_state.now_ms(),
        };
        let mut policy = RouteAttemptPolicy::new(
            &route.balance,
            &route.targets,
            context,
            &snapshot,
            self.policy_state.clone(),
        );
        policy.retain(|target| eligible.contains(&selected_target_key(target)));
        // 计划耗尽只指计划与 Route 的可用 Target 无交集；冷却、凭据失效等健康过滤
        // 造成的空集是 Target 暂不可用，不能报成输入模态不被支持。
        let plan_overlaps_route = media_plan.is_none_or(|plan| {
            route.targets.iter().any(|target| {
                let key = target_key(target.provider_id().as_str(), target.model().as_str());
                target.enabled && eligible.contains(&key) && plan.target_keys.contains(&key)
            })
        });
        if let Some(plan) = media_plan {
            policy.retain(|target| plan.target_keys.contains(&selected_target_key(target)));
        }
        if policy.is_empty() {
            let error = self
                .empty_selection_error(route, &snapshot)
                .await
                .map_err(SelectionError::SchedulingEvidence)?;
            return Err(match error {
                SelectionError::NoEligibleTarget if !plan_overlaps_route => {
                    SelectionError::MediaPlanExhausted
                }
                error => error,
            });
        }
        Ok(policy)
    }

    /// Assemble scheduling for an operation that has no generation history,
    /// continuation, or prompt-cache identity (for example full search or
    /// media generation). It uses the same Target eligibility, priority,
    /// cooldown, retry, and scheduling evidence as model turns.
    #[tracing::instrument(
        target = "stravia::perf",
        name = "router.select_independent",
        skip_all,
        fields(status)
    )]
    pub(crate) async fn select_independent(
        &self,
        principal: &Principal,
        route: &RouteConfig,
        estimated_input_tokens: u64,
        requires_image: bool,
        observer: Option<&RunObserver>,
        cooldown: &CooldownAttempts,
    ) -> Result<RouteAttemptPolicy, SelectionError> {
        let eligible = self
            .modality_eligible_targets(route, requires_image)
            .await?;
        let snapshot = self
            .scheduling_snapshot(&route.targets, observer)
            .await
            .map_err(SelectionError::SchedulingEvidence)?;
        let context = RouteAttemptContext {
            cooldown: cooldown.clone(),
            principal: principal.continuation_key(),
            route_id: route.id.clone().into(),
            conversation: None,
            conversation_affinity_target: None,
            cache_affinity_target: None,
            estimated_uncached_input_tokens: estimated_input_tokens,
            now_ms: self.policy_state.now_ms(),
        };
        let mut policy = RouteAttemptPolicy::new(
            &route.balance,
            &route.targets,
            context,
            &snapshot,
            self.policy_state.clone(),
        );
        policy.retain(|target| eligible.contains(&selected_target_key(target)));
        if policy.is_empty() {
            return Err(self
                .empty_selection_error(route, &snapshot)
                .await
                .map_err(SelectionError::SchedulingEvidence)?);
        }
        Ok(policy)
    }

    async fn modality_eligible_targets(
        &self,
        route: &RouteConfig,
        requires_image: bool,
    ) -> Result<std::collections::HashSet<String>, SelectionError> {
        let modality = route
            .output_modality
            .ok_or(SelectionError::UnconfiguredRoute)?;
        let mut eligible = std::collections::HashSet::new();
        for target in route.targets.iter().filter(|target| target.enabled) {
            let model = self
                .storage
                .provider_models()
                .find(target.provider_id().as_str(), target.model().as_str())
                .await
                .map_err(SelectionError::SchedulingEvidence)?;
            if model.is_some_and(|model| {
                model.metadata.supports_route_output_modality(modality)
                    && (!requires_image
                        || model
                            .metadata
                            .modalities
                            .as_ref()
                            .is_some_and(|modalities| {
                                stravia_media::platform::supports_image(&modalities.input)
                            }))
            }) {
                eligible.insert(target_key(
                    target.provider_id().as_str(),
                    target.model().as_str(),
                ));
            }
        }
        Ok(eligible)
    }

    async fn empty_selection_error(
        &self,
        route: &RouteConfig,
        snapshot: &RouteSchedulingSnapshot,
    ) -> anyhow::Result<SelectionError> {
        let mut any_paused = false;
        let mut any_unpaused = false;
        for target in route.targets.iter().filter(|target| target.enabled) {
            let provider_id = target.provider_id().as_str();
            if !snapshot.allowance_suspended_providers.contains(provider_id) {
                if any_paused {
                    return Ok(SelectionError::MixedAllowanceExclusions);
                }
                any_unpaused = true;
                continue;
            }
            any_paused = true;
            if any_unpaused
                || snapshot.credential_invalid_providers.contains(provider_id)
                || self
                    .policy_state
                    .target_status(&target_key(provider_id, target.model().as_str()))
                    .cooldown_remaining_ms
                    .is_some()
                || !self
                    .storage
                    .providers()
                    .get(provider_id)
                    .await?
                    .is_some_and(|provider| provider.is_enabled)
            {
                return Ok(SelectionError::MixedAllowanceExclusions);
            }
        }
        Ok(if any_paused {
            SelectionError::AllowanceSuspended
        } else {
            SelectionError::NoEligibleTarget
        })
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
        let providers = self.storage.providers();
        let (credential_invalid_providers, allowance_suspended_providers) = tokio::try_join!(
            providers.credential_invalid_provider_ids(),
            providers.allowance_suspended_provider_ids(),
        )?;
        let mut snapshot = RouteSchedulingSnapshot {
            targets: usage.targets,
            credential_invalid_providers,
            allowance_suspended_providers,
        };
        for target in targets {
            let key = target_key(target.provider_id().as_str(), target.model().as_str());
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
            let model = target.model().as_str();
            let provider_id = target.provider_id().as_str();
            let pricing = match self.policy_state.pricing(provider_id, model).await {
                PricingProbe::Hit(pricing) => pricing,
                PricingProbe::Miss(generation) => {
                    let loaded = self
                        .storage
                        .provider_models()
                        .find(provider_id, model)
                        .await?
                        .and_then(|record| record.metadata.cost)
                        .map(|cost| TargetPricing {
                            cost: Some(Arc::new(cost)),
                        });
                    self.policy_state
                        .store_pricing(generation, provider_id, model, loaded.clone())
                        .await;
                    loaded
                }
            };
            let Some(pricing) = pricing else {
                continue;
            };
            let Some(cost) = pricing.cost else {
                continue;
            };
            // Custom-currency fields enter the USD ratio at the current rate;
            // the cached price itself stays as entered.
            let currency = self.custom_currency.read().clone();
            let currency = currency.as_deref();
            let usd = |field| {
                cost.base_price_in_usd(field, currency)
                    .and_then(|value| value.to_f64())
            };
            let target_snapshot = &mut snapshot.targets[index];
            target_snapshot.cost_input = usd(crate::pricing::PriceField::Input);
            target_snapshot.cost_output = usd(crate::pricing::PriceField::Output);
            target_snapshot.cost_cache_read = usd(crate::pricing::PriceField::CacheRead);
            target_snapshot.cost_cache_write = usd(crate::pricing::PriceField::CacheWrite);
            target_snapshot.pricing = Some(cost);
        }
        Ok(snapshot)
    }
}

pub(crate) fn estimate_uncached_input_tokens(request: &AiRequest) -> u64 {
    struct CountBytes(u64);
    impl std::io::Write for CountBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // 独立 instructions 和工具定义同样占用模型上下文；不能只按短用户消息过滤。
    // 只累计序列化字节，避免为长系统提示词和工具 schema 再分配完整 JSON 缓冲。
    let mut count = CountBytes(0);
    serde_json::to_writer(
        &mut count,
        &(&request.items, &request.instructions, &request.tools),
    )
    .expect("canonical prompt fields serialize to an infallible byte counter");
    count.0.div_ceil(4)
}

#[cfg(test)]
mod tests {
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
        AiErrorKind, AiItem, MessageContent, Role, Usage,
    };

    fn target(provider: &str, priority: i32) -> TargetConfig {
        TargetConfig {
            output_modality_mismatch: false,
            route_output_modalities: Vec::new(),
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
            model_name: None,
        }
    }

    fn route(targets: Vec<TargetConfig>) -> RouteConfig {
        RouteConfig {
            output_modality: Some(crate::db::models::RouteOutputModality::Text),
            image_generation_bound: false,
            id: "route-id".into(),
            model_id: "route".into(),
            display_name: None,
            balance: "traffic_equalization".into(),

            is_enabled: true,
            created_at: String::new(),
            supported_thinking_levels: Vec::new(),
            context_window: None,
            supports_image_input: false,
            targets,
            default_thinking_level: None,
            billing: None,
        }
    }

    fn user_item(text: &str) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Text(std::sync::Arc::new(text.into())),
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
            _parent: &crate::router::GenerationParentTarget,
            _request: &mut Arc<AiRequest>,
            _full_fallback: &mut Option<Arc<AiRequest>>,
        ) -> Option<String> {
            None
        }
    }

    struct Fixture {
        storage: DynStorage,
        selector: RouteSelector,
        policy_state: RoutePolicyState,
        cache_affinity: CacheAffinity,
    }

    fn fixture(storage: DynStorage, continuation: Arc<dyn ContinuationLookup>) -> Fixture {
        let runtime_cache = crate::runtime_cache::RuntimeCache::tinyufo(1024 * 1024);
        let cache_affinity = CacheAffinity::new(runtime_cache.clone());
        let policy_state = RoutePolicyState::new(runtime_cache);
        Fixture {
            storage: storage.clone(),
            selector: RouteSelector::new(
                storage,
                cache_affinity.clone(),
                continuation,
                policy_state.clone(),
                Default::default(),
            ),
            policy_state,
            cache_affinity,
        }
    }

    fn memory() -> DynStorage {
        Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new()))
    }

    async fn register_targets(storage: &DynStorage, route: &RouteConfig) {
        let models = storage.provider_models();
        for target in &route.targets {
            if models
                .get(target.provider_id(), target.model())
                .await
                .expect("fixture model lookup")
                .is_some()
            {
                continue;
            }
            let mutation = models
                .create(
                    NewProviderModelRecord {
                        provider_id: target.provider_id().clone().into(),
                        model_id: target.model().clone().into(),
                        source_kind: ProviderModelSourceKind::Manual,
                        snapshot_state: SnapshotState::Local,
                        source_metadata: None,
                        metadata_source_provider_id: None,
                        presence: ProviderModelPresence::Present,
                        selection_policy: ProviderModelSelectionPolicy::Auto,
                        metadata: ProviderModelMetadata {
                            modalities: Some(crate::provider_models::ModelModalities::text_only()),
                            ..Default::default()
                        },
                    },
                    None,
                )
                .await
                .expect("register route target");
            assert!(matches!(mutation, ProviderModelMutation::Applied(_)));
        }
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
            window: crate::db::models::StatsWindow,
        ) -> anyhow::Result<crate::db::models::StatsOverview> {
            self.delegate.stats_overview(window).await
        }

        async fn stats_series(
            &self,
            window: crate::db::models::StatsWindow,
            bucket_ms: i64,
        ) -> anyhow::Result<Vec<crate::db::models::StatsSeries>> {
            self.delegate.stats_series(window, bucket_ms).await
        }

        async fn stats_by_model(
            &self,
            window: crate::db::models::StatsWindow,
        ) -> anyhow::Result<Vec<crate::db::models::ModelStats>> {
            self.delegate.stats_by_model(window).await
        }

        async fn stats_by_provider(
            &self,
            window: crate::db::models::StatsWindow,
        ) -> anyhow::Result<Vec<crate::db::models::ProviderStats>> {
            self.delegate.stats_by_provider(window).await
        }

        async fn stats_by_api_key(
            &self,
            window: crate::db::models::StatsWindow,
        ) -> anyhow::Result<Vec<crate::db::models::ApiKeyStats>> {
            self.delegate.stats_by_api_key(window).await
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
            validate_map: &crate::provider_models::ProviderModelThinkingMapValidator<'_>,
        ) -> anyhow::Result<()> {
            self.delegate
                .apply_reconciliation(provider_id, reconciliation, validate_map)
                .await
        }

        async fn create(
            &self,
            input: NewProviderModelRecord,
            change_record: Option<crate::provider_models::ModelChangeRecord>,
        ) -> anyhow::Result<ProviderModelMutation> {
            ProviderModelStore::create(&self.delegate, input, change_record).await
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

        async fn restore(
            &self,
            provider_id: &str,
            model_id: &str,
            input: crate::provider_models::RestoreProviderModel,
            validate_map: &crate::provider_models::ProviderModelThinkingMapValidator<'_>,
            before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
        ) -> anyhow::Result<crate::provider_models::ProviderModelRestore> {
            ProviderModelStore::restore(
                &self.delegate,
                provider_id,
                model_id,
                input,
                validate_map,
                before_commit,
            )
            .await
        }

        async fn update_selection_policy(
            &self,
            provider_id: &str,
            model_id: &str,
            policy: ProviderModelSelectionPolicy,
            expected_revision: i64,
            change_record: Option<crate::provider_models::ModelChangeRecord>,
        ) -> anyhow::Result<ProviderModelMutation> {
            ProviderModelStore::update_selection_policy(
                &self.delegate,
                provider_id,
                model_id,
                policy,
                expected_revision,
                change_record,
            )
            .await
        }

        async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool> {
            ProviderModelStore::delete_manual(&self.delegate, provider_id, model_id).await
        }

        async fn sync_state(
            &self,
            provider_id: &str,
        ) -> anyhow::Result<crate::provider_models::ProviderModelSyncState> {
            self.delegate.sync_state(provider_id).await
        }

        async fn set_automatic_sync(
            &self,
            provider_id: &str,
            enabled: bool,
        ) -> anyhow::Result<crate::provider_models::ProviderModelSyncState> {
            self.delegate.set_automatic_sync(provider_id, enabled).await
        }

        async fn save_sync_failure(
            &self,
            provider_id: &str,
            summary: crate::provider_models::ProviderModelSyncSummary,
            record: crate::provider_models::ModelChangeRecord,
        ) -> anyhow::Result<()> {
            self.delegate
                .save_sync_failure(provider_id, summary, record)
                .await
        }

        async fn list_changes(
            &self,
            query: crate::provider_models::ModelChangeQuery,
        ) -> anyhow::Result<crate::provider_models::ModelChangePage> {
            self.delegate.list_changes(query).await
        }

        async fn get_change(
            &self,
            id: &str,
        ) -> anyhow::Result<Option<crate::provider_models::ModelChangeRecord>> {
            self.delegate.get_change(id).await
        }

        async fn prune_changes(&self, before: &str) -> anyhow::Result<()> {
            self.delegate.prune_changes(before).await
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

        fn money(&self) -> &dyn crate::storage::traits::MoneyStore {
            self.delegate.money()
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
        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
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

        let mut first = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
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

        let mut second = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("second select");
        assert_eq!(next_provider(&mut second).as_deref(), Some("healthy"));
        assert_eq!(next_provider(&mut second), None);
    }

    #[tokio::test]
    async fn stored_conversation_affinity_is_preferred_over_priority() {
        let fixture = fixture(memory(), no_continuation());
        let mut request = request();
        request.parameters.cache.key = Some("chat-a".into());
        fixture.policy_state.record_success(
            &RouteAttemptContext {
                cooldown: CooldownAttempts::default(),
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

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request,
                    None,
                    None,
                    estimate_uncached_input_tokens(&request),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("affinity"));
    }

    #[tokio::test]
    async fn cache_affinity_applies_only_without_conversation_identity() {
        let fixture = fixture(memory(), no_continuation());
        let seeded = request();
        fixture
            .cache_affinity
            .record_success(
                &principal(),
                "route-id",
                &seeded,
                "affinity:model",
                &large_usage(),
            )
            .await;
        let route = route(vec![target("primary", 10), target("affinity", 0)]);

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &seeded,
                    None,
                    None,
                    estimate_uncached_input_tokens(&seeded),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("affinity"));
        drop(policy);

        // A conversation identity suppresses the cache hint entirely.
        let mut identified = request();
        identified.parameters.cache.key = Some("chat-b".into());
        let mut identified_policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &identified,
                    None,
                    None,
                    estimate_uncached_input_tokens(&identified),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("select with conversation");
        assert_eq!(
            next_provider(&mut identified_policy).as_deref(),
            Some("primary")
        );
    }

    #[tokio::test]
    async fn parent_target_preference_does_not_apply_to_prompt_cache_identity() {
        let fixture = fixture(
            memory(),
            Arc::new(StaticContinuation(Some("continued:model".into()))),
        );
        let route = route(vec![target("primary", 10), target("continued", 0)]);
        let mut request = request();
        request.parameters.cache.key = Some("chat-c".into());
        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request,
                    None,
                    None,
                    estimate_uncached_input_tokens(&request),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("select prompt-cache conversation");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("primary"));
    }

    #[tokio::test]
    async fn independent_selection_carries_model_target_without_generation_hints() {
        let fixture = fixture(
            memory(),
            Arc::new(StaticContinuation(Some("continued:model".into()))),
        );
        let route = route(vec![target("research", 10), target("continued", 0)]);

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select_independent(
                    &principal(),
                    &route,
                    12_345,
                    false,
                    None,
                    &CooldownAttempts::default(),
                )
                .await
        }
        .expect("independent selection");
        let selected = policy.next_healthy().expect("Model Target");

        assert_eq!(selected.provider_id().as_str(), "research");
        assert_eq!(selected.model().as_str(), "model");
    }

    fn responses_request(items: Vec<AiItem>) -> AiRequest {
        let mut request = AiRequest::new("route", items);
        request.meta.source_protocol =
            Some(stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24);
        request
    }

    fn source(provider: &str) -> crate::generation_chain::GenerationSource {
        crate::generation_chain::GenerationSource::Target(crate::history_marker::ThinkingSource {
            namespace: format!("{provider}:model"),
            protocol: Some(
                stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24.into(),
            ),
            actual_model: "model".into(),
            target_id: format!("{provider}:model"),
            authority: None,
            issued: Vec::new(),
        })
    }

    #[tokio::test]
    async fn resolved_explicit_discovered_and_native_parents_prefer_the_actual_final_target() {
        let chain = crate::generation_chain::test_chain().await;
        let owner = principal();
        let mut root = chain
            .begin(
                owner.clone(),
                responses_request(vec![user_item("question")]),
            )
            .await
            .expect("root");
        let mut intermediate =
            stravia_runtime_contract::protocol::ir::AiResponse::new("hidden", "model");
        intermediate.push_output_text("hidden answer");
        assert!(root.stage(&mut intermediate, &source("earlier"), None));
        let mut response =
            stravia_runtime_contract::protocol::ir::AiResponse::new("final", "model");
        response.push_output_text("answer");
        assert!(root.stage(&mut response, &source("continued"), None));
        root.persist().await.expect("delivered root");

        for mode in ["explicit", "discovered", "native"] {
            let mut request = if mode == "explicit" {
                responses_request(vec![user_item("follow-up")])
            } else {
                let mut items = vec![user_item("question")];
                items.extend(response.items.clone());
                if mode == "discovered" {
                    items.push(user_item("follow-up"));
                }
                responses_request(items)
            };
            if mode == "explicit" {
                crate::router::stamp_previous_response_id(&mut request, root.id());
            }
            let child = if mode == "native" {
                chain.begin_native_compaction(owner.clone(), request).await
            } else {
                chain.begin(owner.clone(), request).await
            }
            .expect("resolved parent");
            assert_eq!(child.parent_id(), Some(root.id()), "{mode}");
            let fixture = fixture(memory(), chain.continuation_lookup());
            let route = route(vec![
                target("primary", 10),
                target("earlier", 5),
                target("continued", 0),
            ]);
            let mut policy = {
                register_targets(&fixture.storage, &route).await;
                fixture
                    .selector
                    .select(
                        &owner,
                        &route,
                        child.request(),
                        None,
                        None,
                        estimate_uncached_input_tokens(child.request()),
                        &CooldownAttempts::default(),
                        &child.parent_target(),
                    )
                    .await
            }
            .expect("select resolved parent");
            assert_eq!(
                next_provider(&mut policy).as_deref(),
                Some("continued"),
                "{mode}"
            );
        }

        let mut foreign = responses_request(vec![user_item("follow-up")]);
        crate::router::stamp_previous_response_id(&mut foreign, root.id());
        assert!(matches!(
            chain.begin(Principal::new("another-owner"), foreign).await,
            Err(crate::generation_chain::BeginError::PreviousResponseNotFound),
        ));
        let mut full_history = vec![user_item("question")];
        full_history.extend(response.items);
        full_history.push(user_item("follow-up"));
        let foreign = chain
            .begin(
                Principal::new("another-owner"),
                responses_request(full_history),
            )
            .await
            .expect("foreign history becomes a new root");
        assert_eq!(foreign.parent_id(), None);
        let fixture = fixture(memory(), chain.continuation_lookup());
        let route = route(vec![target("primary", 10), target("continued", 0)]);
        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &Principal::new("another-owner"),
                    &route,
                    foreign.request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(foreign.request()),
                    &CooldownAttempts::default(),
                    &foreign.parent_target(),
                )
                .await
        }
        .expect("select isolated principal");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("primary"));
    }

    #[tokio::test]
    async fn parent_from_unknown_expiry_adapter_still_prefers_its_actual_target() {
        use stravia_runtime_contract::turn_chain::{
            TurnChainStore, TurnCommit, TurnCommitError, TurnNode, TurnNodeId, TurnNodeKind,
            TurnUnavailable,
        };

        // A supported custom store may use the trait's default unknown expiry
        // while its durable materialization still enforces real node validity.
        struct UnknownExpiryStore(crate::turn_chain::SqlTurnChainStore);

        #[async_trait]
        impl TurnChainStore for UnknownExpiryStore {
            async fn materialize(
                &self,
                principal: &Principal,
                kind: TurnNodeKind,
                id: &TurnNodeId,
            ) -> Result<Vec<TurnNode>, TurnUnavailable> {
                self.0.materialize(principal, kind, id).await
            }

            async fn commit(&self, commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError> {
                self.0.commit(commit).await
            }

            async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
                self.0.sweep_expired().await
            }
        }

        let chain = crate::generation_chain::GenerationChain::from_turn_chain(
            Arc::new(UnknownExpiryStore(crate::turn_chain::test_store().await)),
            std::time::Duration::from_secs(7 * 24 * 60 * 60),
            None,
            crate::runtime_cache::RuntimeCache::tinyufo(16 * 1024 * 1024),
        );
        let owner = principal();
        let mut root = chain
            .begin(
                owner.clone(),
                responses_request(vec![user_item("question")]),
            )
            .await
            .expect("root on custom store");
        let mut response =
            stravia_runtime_contract::protocol::ir::AiResponse::new("answer", "model");
        response.push_output_text("answer");
        assert!(root.stage(&mut response, &source("continued"), None));
        root.persist().await.expect("persist custom-store root");
        let mut request = responses_request(vec![user_item("follow-up")]);
        crate::router::stamp_previous_response_id(&mut request, root.id());
        let child = chain
            .begin(owner.clone(), request)
            .await
            .expect("custom-store parent");
        let fixture = fixture(memory(), chain.continuation_lookup());
        let route = route(vec![target("primary", 10), target("continued", 0)]);
        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &owner,
                    &route,
                    child.request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(child.request()),
                    &CooldownAttempts::default(),
                    &child.parent_target(),
                )
                .await
        }
        .expect("select custom-store continuation");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("continued"));
    }

    #[tokio::test]
    async fn resolved_hook_parent_without_target_uses_normal_route_priority() {
        let chain = crate::generation_chain::test_chain().await;
        let owner = principal();
        let mut root = chain
            .begin(
                owner.clone(),
                responses_request(vec![user_item("question")]),
            )
            .await
            .expect("root");
        let mut response = stravia_runtime_contract::protocol::ir::AiResponse::new("hook", "model");
        response.push_output_text("Hook answer");
        assert!(root.stage(
            &mut response,
            &crate::generation_chain::GenerationSource::Hook {
                protocol: stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24,
            },
            None
        ));
        root.persist().await.expect("Hook root");
        let mut request = responses_request(vec![user_item("follow-up")]);
        crate::router::stamp_previous_response_id(&mut request, root.id());
        let child = chain
            .begin(owner.clone(), request)
            .await
            .expect("Hook parent");
        // The resolved absence is authoritative even if an independent resolver
        // would provide a Target.
        let fixture = fixture(
            memory(),
            Arc::new(StaticContinuation(Some("continued:model".into()))),
        );
        let route = route(vec![target("primary", 10), target("continued", 0)]);
        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &owner,
                    &route,
                    child.request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(child.request()),
                    &CooldownAttempts::default(),
                    &child.parent_target(),
                )
                .await
        }
        .expect("select Hook continuation");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("primary"));
    }

    #[tokio::test]
    async fn resolved_parent_evidence_cannot_revive_expired_mismatched_or_disabled_targets() {
        let mut request = request();
        crate::router::stamp_previous_response_id(&mut request, "actual-parent");
        for (parent_id, expires_at, enabled, present) in [
            ("actual-parent", std::time::Instant::now(), true, true),
            (
                "other-parent",
                std::time::Instant::now() + std::time::Duration::from_secs(60),
                true,
                true,
            ),
            (
                "actual-parent",
                std::time::Instant::now() + std::time::Duration::from_secs(60),
                false,
                true,
            ),
            (
                "actual-parent",
                std::time::Instant::now() + std::time::Duration::from_secs(60),
                true,
                false,
            ),
        ] {
            let fixture = fixture(
                memory(),
                Arc::new(StaticContinuation(Some("continued:model".into()))),
            );
            let mut targets = vec![target("primary", 10)];
            if present {
                let mut continued = target("continued", 0);
                continued.enabled = enabled;
                targets.push(continued);
            }
            let route = route(targets);
            let evidence = crate::router::GenerationParentTarget::route_evidence(
                &principal(),
                parent_id,
                "continued:model",
                expires_at,
            );
            let mut policy = {
                register_targets(&fixture.storage, &route).await;
                fixture
                    .selector
                    .select(
                        &principal(),
                        &route,
                        &request,
                        None,
                        None,
                        estimate_uncached_input_tokens(&request),
                        &CooldownAttempts::default(),
                        &evidence,
                    )
                    .await
            }
            .expect("select without invalid preference");
            assert_eq!(next_provider(&mut policy).as_deref(), Some("primary"));
        }
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

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request,
                    None,
                    None,
                    estimate_uncached_input_tokens(&request),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
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

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    Some(&plan),
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("native"));
        assert_eq!(next_provider(&mut policy), None);

        let empty_plan = MediaRoutingPlan {
            mode: MediaRoutingMode::Native,
            target_keys: vec!["missing:model".into()],
            source_artifact_ids: Vec::new(),
        };
        let error = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    Some(&empty_plan),
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .err()
        .expect("plan without candidates");
        assert!(matches!(error, SelectionError::MediaPlanExhausted));
    }

    #[tokio::test]
    async fn cooling_plan_targets_are_not_reported_as_media_plan_exhaustion() {
        let fixture = fixture(memory(), no_continuation());
        let mut first_target = target("first", 10);
        first_target.target_retry_budget = 0;
        let mut second_target = target("second", 0);
        second_target.target_retry_budget = 0;
        let route = route(vec![first_target, second_target]);
        let plan = MediaRoutingPlan {
            mode: MediaRoutingMode::Native,
            target_keys: vec!["first:model".into(), "second:model".into()],
            source_artifact_ids: Vec::new(),
        };
        register_targets(&fixture.storage, &route).await;
        let principal = principal();
        let request = request();
        let estimate = estimate_uncached_input_tokens(&request);
        let parent = crate::router::GenerationParentTarget::Unresolved;

        let mut policy = fixture
            .selector
            .select(
                &principal,
                &route,
                &request,
                Some(&plan),
                None,
                estimate,
                &CooldownAttempts::default(),
                &parent,
            )
            .await
            .expect("both plan targets are healthy");
        for _ in 0..2 {
            let failed = policy.next_healthy().expect("plan target");
            assert_eq!(
                policy.record_failure(
                    &failed,
                    AttemptFailureSignal {
                        kind: AiErrorKind::Timeout,
                        client_output_committed: false,
                        retry_after: None,
                        now_ms: fixture.policy_state.now_ms(),
                        jitter_sample: 0.5,
                    },
                ),
                AttemptFailureDisposition::TryNextTarget
            );
        }
        drop(policy);

        // 计划内 Target 全部冷却是健康问题，不是输入模态不被支持；新请求不继承冷却例外。
        let error = fixture
            .selector
            .select(
                &principal,
                &route,
                &request,
                Some(&plan),
                None,
                estimate,
                &CooldownAttempts::default(),
                &parent,
            )
            .await
            .err()
            .expect("every plan target is cooling");
        assert!(matches!(error, SelectionError::NoEligibleTarget));
    }

    #[tokio::test]
    async fn route_without_enabled_targets_is_a_typed_no_target_error() {
        let fixture = fixture(memory(), no_continuation());
        let mut disabled = target("disabled", 0);
        disabled.enabled = false;
        let route = route(vec![disabled]);

        let error = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .err()
        .expect("no eligible target");
        assert!(matches!(error, SelectionError::NoEligibleTarget));
    }

    #[tokio::test]
    async fn provider_model_lookup_failure_is_a_typed_evidence_error() {
        let storage = Arc::new(FixtureStorage::new().failing_provider_models());
        let fixture = fixture(storage, no_continuation());
        let route = route(vec![target("a", 0)]);

        let error = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
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
                input_tokens_1h: Some(1_000_000),
                ..Default::default()
            }],
        }));
        let fixture = fixture(storage, no_continuation());
        let route = route(vec![target("busy", 0), target("idle", 0)]);

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
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
                    input_tokens_1h: Some(1_000_000),
                    uncached_input_tokens_1h: Some(0),
                    cache_read_tokens_1h: Some(1_000_000),
                    ..Default::default()
                },
                TargetSchedulingSnapshot {
                    target_key: "output_heavy:model".into(),
                    output_tokens_1h: Some(100_000),
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
                .create(
                    NewProviderModelRecord {
                        provider_id: provider.into(),
                        model_id: "model".into(),
                        source_kind: ProviderModelSourceKind::Discovered,
                        snapshot_state: SnapshotState::Imported {
                            source: crate::provider_models::SourceStamp::Discovery,
                            pinned: std::collections::BTreeSet::new(),
                        },
                        source_metadata: None,
                        metadata_source_provider_id: None,
                        presence: ProviderModelPresence::Present,
                        selection_policy: ProviderModelSelectionPolicy::Auto,
                        metadata: ProviderModelMetadata {
                            cost: Some(cost.clone()),
                            modalities: Some(crate::provider_models::ModelModalities::text_only()),
                            ..Default::default()
                        },
                    },
                    None,
                )
                .await
                .expect("provider model created");
            assert!(matches!(applied, ProviderModelMutation::Applied(_)));
        }
        let fixture = fixture(storage, no_continuation());
        let route = route(vec![
            target("cache_read_heavy", 0),
            target("output_heavy", 0),
        ]);

        let mut policy = {
            register_targets(&fixture.storage, &route).await;
            fixture
                .selector
                .select(
                    &principal(),
                    &route,
                    &request(),
                    None,
                    None,
                    estimate_uncached_input_tokens(&request()),
                    &CooldownAttempts::default(),
                    &crate::router::GenerationParentTarget::Unresolved,
                )
                .await
        }
        .expect("select");
        assert_eq!(next_provider(&mut policy).as_deref(), Some("output_heavy"));
    }

    fn priced_model(provider_id: &str, input: u64, output: u64) -> NewProviderModelRecord {
        NewProviderModelRecord {
            provider_id: provider_id.into(),
            model_id: "model".into(),
            source_kind: ProviderModelSourceKind::Manual,
            snapshot_state: SnapshotState::Local,
            source_metadata: None,
            metadata_source_provider_id: None,
            presence: ProviderModelPresence::Present,
            selection_policy: ProviderModelSelectionPolicy::Auto,
            metadata: ProviderModelMetadata {
                cost: Some(ModelCost {
                    prices: PriceComponents {
                        input: Some(Decimal::from(input)),
                        output: Some(Decimal::from(output)),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    async fn snapshot_costs(
        fixture: &Fixture,
        route: &RouteConfig,
    ) -> Vec<(String, Option<f64>, Option<f64>)> {
        fixture
            .selector
            .scheduling_snapshot(&route.targets, None)
            .await
            .expect("snapshot")
            .targets
            .iter()
            .map(|target| {
                (
                    target.target_key.clone(),
                    target.cost_input,
                    target.cost_output,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn pricing_cache_serves_until_invalidation_boundary() {
        let storage = Arc::new(FixtureStorage::new());
        let fixture = fixture(storage.clone(), no_continuation());
        let route = route(vec![target("p", 0)]);
        let models = storage.provider_models();

        // Negative result is cached: no write has happened yet.
        let costs = snapshot_costs(&fixture, &route).await;
        assert_eq!(costs, vec![("p:model".to_string(), None, None)]);
        let applied = models
            .create(priced_model("p", 1, 2), None)
            .await
            .expect("create");
        assert!(matches!(applied, ProviderModelMutation::Applied(_)));

        // The write's invalidation has not landed, so the cached negative
        // answer still serves.
        let costs = snapshot_costs(&fixture, &route).await;
        assert_eq!(costs, vec![("p:model".to_string(), None, None)]);

        fixture.policy_state.clear_pricing();
        let costs = snapshot_costs(&fixture, &route).await;
        assert_eq!(costs, vec![("p:model".to_string(), Some(1.0), Some(2.0))]);

        // A price update writes through storage but stays invisible until the
        // invalidation boundary clears the shared cache.
        let record = models.get("p", "model").await.unwrap().unwrap();
        let mut updated = priced_model("p", 7, 9);
        updated.metadata.provider = record.metadata.provider.clone();
        models
            .update_metadata(
                "p",
                "model",
                updated.metadata,
                record.snapshot_state.clone(),
                record.revision,
            )
            .await
            .expect("update_metadata");
        let costs = snapshot_costs(&fixture, &route).await;
        assert_eq!(costs, vec![("p:model".to_string(), Some(1.0), Some(2.0))]);
        fixture.policy_state.clear_pricing();
        let costs = snapshot_costs(&fixture, &route).await;
        assert_eq!(costs, vec![("p:model".to_string(), Some(7.0), Some(9.0))]);

        // Deleting the manual record takes effect at the same boundary.
        assert!(models.delete_manual("p", "model").await.unwrap());
        fixture.policy_state.clear_pricing();
        let costs = snapshot_costs(&fixture, &route).await;
        assert_eq!(costs, vec![("p:model".to_string(), None, None)]);
    }

    /// Real `AdminService` writes must drop this Gateway's pricing cache before
    /// returning, so the next selection observes the new price state without
    /// waiting for the config epoch poll.
    #[tokio::test]
    async fn admin_provider_model_writes_invalidate_pricing_immediately() {
        use crate::config::GatewayConfig;
        use crate::db::models::{CreateProvider, ProviderCredentialInput, ProviderSourceInput};
        use crate::provider_models::{CreateManualProviderModel, UpdateProviderModel};

        let data_dir = tempfile::tempdir().expect("data dir");
        let gateway = crate::Gateway::from_storage(
            GatewayConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..GatewayConfig::default()
            },
            Arc::new(MemoryStorage::new(Vec::new(), Vec::new(), Vec::new())),
        )
        .await
        .expect("gateway");
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: Some("Pricing Provider".into()),
                source: ProviderSourceInput::Custom {
                    vendor: "custom".into(),
                    channel: "default".into(),
                    protocol: Some("openai-compatible".into()),
                    base_url: "http://127.0.0.1:9".into(),
                    models_source: None,
                    static_models: None,
                },
                credential: ProviderCredentialInput::None,
                vendor_options: Default::default(),
                use_proxy: false,
            })
            .await
            .expect("provider");
        let selector = RouteSelector::new(
            gateway.storage.clone(),
            CacheAffinity::default(),
            no_continuation(),
            gateway.route_policy_state.clone(),
            gateway.custom_currency.clone(),
        );
        let route = route(vec![target(&provider.id, 0)]);
        let key = format!("{}:model", provider.id);
        let costs = |snapshot: RouteSchedulingSnapshot| {
            snapshot
                .targets
                .iter()
                .find(|target| target.target_key == key)
                .map(|target| (target.cost_input, target.cost_output))
        };

        // The unknown model is probed once and cached as a negative entry.
        let snapshot = selector
            .scheduling_snapshot(&route.targets, None)
            .await
            .expect("snapshot");
        assert_eq!(costs(snapshot), Some((None, None)));

        gateway
            .admin()
            .create_manual_provider_model(
                &provider.id,
                "model",
                CreateManualProviderModel {
                    template_id: None,
                    metadata: serde_json::json!({
                        "id": "model",
                        "cost": { "input": 1.5, "output": 3.0 }
                    }),
                },
            )
            .await
            .expect("create manual model");
        let snapshot = selector
            .scheduling_snapshot(&route.targets, None)
            .await
            .expect("snapshot after create");
        assert_eq!(costs(snapshot), Some((Some(1.5), Some(3.0))));

        let detail = gateway
            .admin()
            .get_provider_model(&provider.id, "model")
            .await
            .expect("detail");
        gateway
            .admin()
            .update_provider_model(
                &provider.id,
                "model",
                UpdateProviderModel {
                    changes: std::collections::BTreeMap::from([
                        (
                            "cost.input".to_string(),
                            serde_json::json!({"amount": "4.0", "currency": "usd"}),
                        ),
                        (
                            "cost.output".to_string(),
                            serde_json::json!({"amount": "8.0", "currency": "usd"}),
                        ),
                    ]),
                    revision: detail.revision,
                },
            )
            .await
            .expect("update model");
        let snapshot = selector
            .scheduling_snapshot(&route.targets, None)
            .await
            .expect("snapshot after update");
        assert_eq!(costs(snapshot), Some((Some(4.0), Some(8.0))));

        gateway
            .admin()
            .delete_manual_provider_model(&provider.id, "model")
            .await
            .expect("delete model");
        let snapshot = selector
            .scheduling_snapshot(&route.targets, None)
            .await
            .expect("snapshot after delete");
        assert_eq!(costs(snapshot), Some((None, None)));

        gateway.shutdown().await;
    }
}
