use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;

use crate::db::models::{
    ApiKeyStats, ApiKeyWithBindings, CreateApiKey, CreateProviderRecord, CreateWebProvider,
    ModelStats, OAuthCredential, Provider, ProviderCredentialVersion, ProviderStats, PutRoute,
    RouteConfig, StatsOverview, StatsSeries, StatsWindow, UpdateApiKey, UpdateProvider,
    UpdateWebProvider, UpsertOAuthCredential, WebAccessSettings, WebProvider,
};
use crate::pricing::{Amount, CostBasis, CurrencyKind, CustomCurrency, SubscriptionPeriod};
use crate::provider_models::{
    ModelChangePage, ModelChangeQuery, ModelChangeRecord, NewProviderModelRecord,
    ProviderModelMutation, ProviderModelReconciliation, ProviderModelRecord, ProviderModelRestore,
    ProviderModelSelectionPolicy, ProviderModelSyncState, ProviderModelSyncSummary,
    ProviderModelThinkingMapValidator, RestoreProviderModel, model_id_match_key,
};
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct ProviderTestResult {
    pub success: bool,
    pub tested_at: String,
}

/// Conditional write-back for an explicit Provider connection check.
/// `credentials_verified` is the plugin's explicit authentication evidence:
/// only a check that actually verified credentials may clear an invalid marker.
#[derive(Debug, Clone)]
pub struct ProviderConnectionCheckWrite {
    pub success: bool,
    pub credentials_verified: bool,
}

#[derive(Debug, Clone)]
pub struct ApiKeyAccessRecord {
    pub id: String,
    pub name: String,
    pub is_enabled: bool,
    pub expires_at: Option<String>,
    pub rpm_limit: Option<i32>,
    /// Display-currency nanos; `None` means unlimited.
    pub spend_limit_nanos: Option<i64>,
    pub transparent_injection_enabled: bool,
    pub inject_media_understanding: bool,
    pub inject_web_search: bool,
    pub inject_media_generation: bool,
}

#[derive(Debug, Clone)]
pub enum StorageBackend {
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone)]
pub struct StorageHealth {
    pub backend: StorageBackend,
    pub can_connect: bool,
    pub schema_compatible: bool,
    pub writable: bool,
}

#[async_trait]
pub trait ProviderStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<Provider>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>>;
    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider>;
    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
    /// Conditional on the credential generation captured when the check started.
    /// Returns false when a newer credential/configuration generation won, so a
    /// late check can never overwrite fresher evidence.
    async fn record_test_result(
        &self,
        provider_id: &str,
        expected: ProviderCredentialVersion,
        result: ProviderConnectionCheckWrite,
    ) -> anyhow::Result<bool>;
    /// ADR-0073 条件写：仅当凭据代际未变时将 Provider 标记为凭据失效。
    /// 返回 true 表示已标记；代际不匹配（凭据已被新证据替换）返回 false。
    /// 写失败由调用方记 `tracing::warn!`，不改变原请求结果。
    async fn mark_credential_invalid(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
    ) -> anyhow::Result<bool>;
    /// 新凭据证据出现时清除失效标记。Provider 更新与测试成功在各自写
    /// 路径内联处理；此方法用于不改 Provider 行的证据（OAuth 刷新成功）。
    async fn clear_credential_invalid(&self, id: &str) -> anyhow::Result<()>;
    /// 调度快照装配用：返回凭据失效的 Provider id 集合。
    async fn credential_invalid_provider_ids(&self) -> anyhow::Result<HashSet<String>>;
    async fn guarded_allowance_keys(&self, id: &str) -> anyhow::Result<Vec<String>>;
    async fn replace_guarded_allowance_keys(&self, id: &str, keys: &[String])
    -> anyhow::Result<()>;
    async fn write_allowance_suspension(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
        state: &crate::db::models::AllowanceSuspensionState,
        expected_guarded_keys: &[String],
    ) -> anyhow::Result<bool>;
    async fn allowance_suspension(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<crate::db::models::AllowanceSuspensionState>>;
    async fn allowance_suspended_provider_ids(&self) -> anyhow::Result<HashSet<String>>;
}

#[cfg(test)]
pub(crate) async fn verify_oauth_refresh_recovery_storage_contract(
    oauth: &dyn OAuthCredentialStore,
    provider_id: &str,
) -> anyhow::Result<()> {
    let input = |access_token: &str, refresh_token: &str| UpsertOAuthCredential {
        driver_key: "openai-codex".into(),
        scheme: "oauth".into(),
        access_token: access_token.into(),
        refresh_token: Some(refresh_token.into()),
        expires_at: Some("2000-01-01T00:00:00Z".into()),
        ..Default::default()
    };
    let original = oauth
        .upsert(provider_id, input("original-access", "original-refresh"))
        .await?;
    let lease = oauth
        .try_begin_refresh(provider_id, original.status_version)
        .await?
        .expect("connected credential can acquire a refresh lease");
    oauth
        .recover_stale_refreshing(Duration::from_secs(120))
        .await?;
    let active = oauth.get(provider_id).await?.expect("active credential");
    assert_eq!(active.status, "refreshing", "active lease is not stale");
    assert_eq!(active.status_version, lease.status_version);
    assert!(
        oauth
            .try_begin_refresh(provider_id, lease.status_version)
            .await?
            .is_none(),
        "an active refresh lease cannot be taken over"
    );
    assert!(
        !oauth
            .list_expiring(Duration::from_secs(300))
            .await?
            .iter()
            .any(|credential| credential.provider_id == provider_id)
    );
    assert!(
        oauth
            .fail_refresh(
                provider_id,
                lease.status_version,
                "temporary upstream failure"
            )
            .await?
    );
    let failed = oauth
        .get(provider_id)
        .await?
        .expect("credential is retained");
    assert_eq!(failed.status, "error");
    assert_eq!(failed.access_token, original.access_token);
    assert_eq!(failed.refresh_token, original.refresh_token);
    assert_eq!(failed.connection_id, original.connection_id);
    assert_eq!(
        failed.last_error.as_deref(),
        Some("temporary upstream failure")
    );
    assert!(
        oauth
            .list_expiring(Duration::from_secs(300))
            .await?
            .iter()
            .any(|credential| credential.provider_id == provider_id),
        "temporary refresh failure remains eligible for background retry"
    );
    assert!(
        oauth
            .try_begin_refresh(provider_id, original.status_version)
            .await?
            .is_none(),
        "retry must use the current credential version"
    );
    let retry = oauth
        .try_begin_refresh(provider_id, failed.status_version)
        .await?
        .expect("failed refresh can acquire a new lease");
    assert!(
        !oauth
            .fail_refresh(provider_id, lease.status_version, "stale failure")
            .await?
    );
    assert!(
        !oauth
            .cancel_refresh(provider_id, lease.status_version)
            .await?
    );
    assert!(
        oauth
            .complete_refresh(
                provider_id,
                lease.status_version,
                input("stale-access", "stale-refresh"),
            )
            .await
            .is_err(),
        "an obsolete lease cannot replace the retry's credentials"
    );
    let refreshed = oauth
        .complete_refresh(
            provider_id,
            retry.status_version,
            input("new-access", "new-refresh"),
        )
        .await?;
    assert_eq!(refreshed.status, "connected");
    assert_eq!(refreshed.connection_id, original.connection_id);
    assert_eq!(refreshed.access_token, "new-access");
    assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(refreshed.last_error, None);
    assert!(refreshed.last_refresh_at.is_some());
    let stored = oauth.get(provider_id).await?.expect("refreshed credential");
    assert_eq!(stored.access_token, refreshed.access_token);
    assert_eq!(stored.refresh_token, refreshed.refresh_token);
    assert_eq!(stored.status_version, refreshed.status_version);
    Ok(())
}

#[cfg(test)]
pub(crate) async fn verify_allowance_storage_contract(
    store: &dyn ProviderStore,
    oauth: &dyn OAuthCredentialStore,
    provider: Provider,
) -> anyhow::Result<()> {
    use crate::db::models::AllowanceSuspensionState;
    let id = &provider.id;
    let keys = vec!["daily".to_owned(), "weekly".to_owned()];
    let duplicate_keys = vec!["weekly".to_owned(), "daily".to_owned(), "weekly".to_owned()];
    store
        .replace_guarded_allowance_keys(id, &duplicate_keys)
        .await?;
    assert_eq!(store.guarded_allowance_keys(id).await?, keys);
    let mut state = AllowanceSuspensionState {
        suspended: true,
        suspended_at: Some("2026-10-02T00:00:00Z".into()),
        triggered_keys: keys.clone(),
        earliest_reset_at: Some(1000),
        evidence_completed_at: 100,
    };
    let mut version = ProviderCredentialVersion {
        provider_revision: provider.revision,
        oauth_status_version: None,
    };
    assert!(
        store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    assert!(store.allowance_suspended_provider_ids().await?.contains(id));
    let unchanged = store.get(id).await?.unwrap();
    assert_eq!(unchanged.revision, provider.revision);
    assert_eq!(unchanged.credential_status, provider.credential_status);
    assert_eq!(
        unchanged.credential_invalid_at,
        provider.credential_invalid_at
    );
    assert_eq!(unchanged.is_enabled, provider.is_enabled);
    state.evidence_completed_at = 99;
    state.suspended = false;
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    state.evidence_completed_at = 100;
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    state.evidence_completed_at = 101;
    state.suspended = true;
    state.suspended_at = Some("2026-10-02T01:00:00Z".into());
    assert!(
        store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    assert_eq!(
        store
            .allowance_suspension(id)
            .await?
            .unwrap()
            .suspended_at
            .as_deref(),
        Some("2026-10-02T00:00:00Z")
    );
    version.provider_revision += 1;
    state.evidence_completed_at = 102;
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    version.provider_revision -= 1;
    oauth
        .upsert(
            id,
            UpsertOAuthCredential {
                driver_key: "driver".into(),
                scheme: "authorization_code".into(),
                access_token: "token".into(),
                refresh_token: None,
                expires_at: None,
                resource_url: None,
                subject_id: None,
                scopes: None,
                meta: None,
            },
        )
        .await?;
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    let oauth_version = oauth.get(id).await?.unwrap().status_version;
    version.oauth_status_version = Some(oauth_version + 1);
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    version.oauth_status_version = Some(oauth_version);
    assert!(
        store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    assert!(store.mark_credential_invalid(id, version).await?);
    version.provider_revision = store.get(id).await?.unwrap().revision;
    let remaining = vec!["weekly".to_owned()];
    store.replace_guarded_allowance_keys(id, &remaining).await?;
    let saved = store.allowance_suspension(id).await?.unwrap();
    assert!(saved.suspended);
    assert_eq!(saved.triggered_keys, remaining);
    assert_eq!(saved.suspended_at.as_deref(), Some("2026-10-02T00:00:00Z"));
    assert_eq!(saved.earliest_reset_at, None);
    state.evidence_completed_at = 103;
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    store.replace_guarded_allowance_keys(id, &[]).await?;
    let saved = store.allowance_suspension(id).await?.unwrap();
    assert!(!saved.suspended);
    assert_eq!(saved.suspended_at, None);
    assert_eq!(saved.earliest_reset_at, None);
    assert!(saved.triggered_keys.is_empty());
    assert!(store.get(id).await?.unwrap().credential_invalid());
    assert!(!store.allowance_suspended_provider_ids().await?.contains(id));
    store.replace_guarded_allowance_keys(id, &keys).await?;
    assert!(
        store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    state.evidence_completed_at = 104;
    state.suspended = false;
    state.suspended_at = None;
    state.triggered_keys.clear();
    state.earliest_reset_at = None;
    assert!(
        store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    assert!(!store.allowance_suspended_provider_ids().await?.contains(id));
    assert!(store.get(id).await?.unwrap().credential_invalid());
    state.evidence_completed_at = 105;
    state.suspended = true;
    state.suspended_at = Some("2026-10-02T02:00:00Z".into());
    state.triggered_keys = keys.clone();
    assert!(
        store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    assert_eq!(
        store.allowance_suspension(id).await?.unwrap().suspended_at,
        state.suspended_at
    );
    store.delete(id).await?;
    assert!(store.guarded_allowance_keys(id).await?.is_empty());
    assert!(store.allowance_suspension(id).await?.is_none());
    assert!(!store.allowance_suspended_provider_ids().await?.contains(id));
    assert!(
        !store
            .write_allowance_suspension(id, version, &state, &keys)
            .await?
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebAccessApiKeyPermissions {
    pub api_key_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct WebAccessRuntimeConfig {
    pub settings: WebAccessSettings,
    pub web_providers: Vec<WebProvider>,
    pub api_key_permissions: WebAccessApiKeyPermissions,
}

#[async_trait]
pub trait WebProviderStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<WebProvider>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<WebProvider>>;
    async fn create(&self, input: CreateWebProvider) -> anyhow::Result<WebProvider>;
    async fn update(&self, id: &str, input: UpdateWebProvider) -> anyhow::Result<WebProvider>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()>;
    async fn load_settings(&self) -> anyhow::Result<WebAccessSettings>;
    /// Loads Web Access settings, providers, API-key authorization, and Codex
    /// dependencies together from one consistent database snapshot.
    async fn load_runtime_config(&self, api_key_id: &str)
    -> anyhow::Result<WebAccessRuntimeConfig>;
    async fn save_settings(&self, settings: &WebAccessSettings) -> anyhow::Result<()>;
}

pub(crate) fn validate_web_access_provider_lists(
    providers: &[WebProvider],
    settings: &WebAccessSettings,
) -> anyhow::Result<()> {
    validate_web_access_priority_list(providers, &settings.search_provider_ids, true)?;
    validate_web_access_priority_list(providers, &settings.fetch_provider_ids, false)?;
    Ok(())
}

fn validate_web_access_priority_list(
    providers: &[WebProvider],
    ids: &[String],
    search: bool,
) -> anyhow::Result<()> {
    for id in ids {
        let provider = providers
            .iter()
            .find(|provider| provider.id.as_str() == id.as_str())
            .with_context(|| format!("Web Provider not found: {id}"))?;
        let capabilities = provider
            .capabilities()
            .with_context(|| format!("unsupported Web Provider kind: {}", provider.kind))?;
        if (search && !capabilities.search) || (!search && !capabilities.fetch) {
            anyhow::bail!(
                "Web Provider {} does not support {}",
                provider.name,
                if search { "Search" } else { "Fetch" }
            );
        }
    }
    Ok(())
}

#[async_trait]
pub trait RouteStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<RouteConfig>>;
    async fn list_active(&self) -> anyhow::Result<Vec<RouteConfig>>;
    async fn get(&self, route_id: &str) -> anyhow::Result<Option<RouteConfig>>;
    /// `targets: None` changes only Route fields; Target rows must not be read or rewritten.
    /// `Some` atomically replaces the set while retaining IDs for matching destinations.
    async fn put(&self, route: PutRoute) -> anyhow::Result<RouteConfig>;
    async fn delete(&self, route_id: &str) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ProviderModelStore: Send + Sync {
    async fn list_for_provider(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderModelRecord>>;
    async fn get(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<Option<ProviderModelRecord>>;
    /// 按精确 model_id 查找；未命中时回退到“`/` 最右段 + 忽略大小写”的宽松匹配。
    ///
    /// `/v1/models` 同步的清单 ID 可能带命名空间前缀或大小写差异
    /// （`zhipuai/glm-4.6` vs `glm-4.6`、`GLM-4.6`），而路由 Target 的 model
    /// 保留用户输入，读取侧用宽松匹配提高命中率。多个清单 ID 归一到同一
    /// 匹配键时视为歧义并返回 None，保持错误可见；写入路径仍走 `get` 保证身份精确。
    async fn find(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> anyhow::Result<Option<ProviderModelRecord>> {
        if let Some(record) = self.get(provider_id, model_id.trim()).await? {
            return Ok(Some(record));
        }
        let needle = model_id_match_key(model_id);
        if needle.is_empty() {
            return Ok(None);
        }
        let mut matches = self
            .list_for_provider(provider_id)
            .await?
            .into_iter()
            .filter(|record| model_id_match_key(&record.model_id) == needle);
        let matched = matches.next();
        if matches.next().is_some() {
            return Ok(None);
        }
        Ok(matched)
    }
    async fn apply_reconciliation(
        &self,
        provider_id: &str,
        reconciliation: ProviderModelReconciliation,
        validate_map: &ProviderModelThinkingMapValidator<'_>,
    ) -> anyhow::Result<()>;
    async fn create(
        &self,
        input: NewProviderModelRecord,
        change_record: Option<ModelChangeRecord>,
    ) -> anyhow::Result<ProviderModelMutation>;
    async fn update_metadata(
        &self,
        provider_id: &str,
        model_id: &str,
        metadata: crate::provider_models::ProviderModelMetadata,
        snapshot_state: crate::provider_models::SnapshotState,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation>;
    /// 原子写回来源字段、取消固定、最新 Target 的 Generated Mapping 与配置变更通知。
    /// 返回同一事务内准备好的完整运行时快照，提交后不再执行可失败的读取。
    /// `validate_map` 使用新规格校验每个关联 Target，包括未变化的手工映射。
    /// `before_commit` 在最终提交前复检 Vendor 许可；失败则全部回滚。
    async fn restore(
        &self,
        provider_id: &str,
        model_id: &str,
        input: RestoreProviderModel,
        validate_map: &ProviderModelThinkingMapValidator<'_>,
        before_commit: &(dyn Fn() -> anyhow::Result<()> + Send + Sync),
    ) -> anyhow::Result<ProviderModelRestore>;
    async fn update_selection_policy(
        &self,
        provider_id: &str,
        model_id: &str,
        policy: ProviderModelSelectionPolicy,
        expected_revision: i64,
        change_record: Option<ModelChangeRecord>,
    ) -> anyhow::Result<ProviderModelMutation>;
    async fn sync_state(&self, provider_id: &str) -> anyhow::Result<ProviderModelSyncState>;
    async fn set_automatic_sync(
        &self,
        provider_id: &str,
        enabled: bool,
    ) -> anyhow::Result<ProviderModelSyncState>;
    async fn save_sync_failure(
        &self,
        provider_id: &str,
        summary: ProviderModelSyncSummary,
        record: ModelChangeRecord,
    ) -> anyhow::Result<()>;
    async fn list_changes(&self, query: ModelChangeQuery) -> anyhow::Result<ModelChangePage>;
    async fn get_change(&self, id: &str) -> anyhow::Result<Option<ModelChangeRecord>>;
    async fn prune_changes(&self, before: &str) -> anyhow::Result<()>;
    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
}

/// Durable cumulative spend for one API Key. A missing row is zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiKeySpend {
    pub spent_custom_nanos: i64,
    pub spent_usd_unconverted_nanos: i64,
    pub reset_at: Option<String>,
}

/// One append-only Provider cost-plan effective record.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCostPlan {
    pub id: String,
    pub provider_id: String,
    /// UTC epoch milliseconds; equals the write time.
    pub effective_from: i64,
    pub basis: CostBasis,
    pub period: Option<SubscriptionPeriod>,
    /// Frozen dual amount; `None` for usage-based plans.
    pub price: Option<Amount>,
    pub price_currency: Option<CurrencyKind>,
    pub created_at: String,
}

/// Admin input before freezing the dual amount and write timestamp.
#[derive(Debug, Clone)]
pub struct NewProviderCostPlan {
    pub basis: CostBasis,
    pub period: Option<SubscriptionPeriod>,
    pub price: Option<Decimal>,
    pub currency: Option<CurrencyKind>,
}

#[async_trait]
pub trait MoneyStore: Send + Sync {
    async fn custom_currency(&self) -> anyhow::Result<Option<CustomCurrency>>;
    /// First setup converts every non-null spend limit by `first_rate` in the
    /// same transaction; later saves keep `first_rate`/`configured_at`.
    async fn save_custom_currency(
        &self,
        symbol: &str,
        usd_rate: Decimal,
    ) -> anyhow::Result<CustomCurrency>;
    async fn provider_cost_plans(&self, provider_id: &str)
    -> anyhow::Result<Vec<ProviderCostPlan>>;
    /// Includes plans whose Provider was deleted, ordered by provider then time.
    async fn all_provider_cost_plans(&self) -> anyhow::Result<Vec<ProviderCostPlan>>;
    async fn append_provider_cost_plan(
        &self,
        provider_id: &str,
        input: NewProviderCostPlan,
    ) -> anyhow::Result<ProviderCostPlan>;
    async fn current_provider_cost_plan(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderCostPlan>> {
        Ok(super::money::current_provider_cost_plan(
            self.provider_cost_plans(provider_id).await?,
        ))
    }
    /// The plan in effect per Provider, selected with the same rule as
    /// [`MoneyStore::current_provider_cost_plan`].
    async fn current_provider_cost_plans(
        &self,
    ) -> anyhow::Result<HashMap<String, ProviderCostPlan>> {
        Ok(super::money::current_provider_cost_plans(
            self.all_provider_cost_plans().await?,
        ))
    }
    /// A missing row returns zero, never an error.
    async fn api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend>;
    /// Resets the cumulative columns only; details and stats stay untouched.
    async fn reset_api_key_spend(&self, api_key_id: &str) -> anyhow::Result<ApiKeySpend>;
}

struct UnsupportedMoneyStore;

static UNSUPPORTED_MONEY: UnsupportedMoneyStore = UnsupportedMoneyStore;

#[async_trait]
impl MoneyStore for UnsupportedMoneyStore {
    async fn custom_currency(&self) -> anyhow::Result<Option<CustomCurrency>> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }

    async fn save_custom_currency(
        &self,
        _symbol: &str,
        _usd_rate: Decimal,
    ) -> anyhow::Result<CustomCurrency> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }

    async fn provider_cost_plans(
        &self,
        _provider_id: &str,
    ) -> anyhow::Result<Vec<ProviderCostPlan>> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }

    async fn all_provider_cost_plans(&self) -> anyhow::Result<Vec<ProviderCostPlan>> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }

    async fn append_provider_cost_plan(
        &self,
        _provider_id: &str,
        _input: NewProviderCostPlan,
    ) -> anyhow::Result<ProviderCostPlan> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }

    async fn api_key_spend(&self, _api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }

    async fn reset_api_key_spend(&self, _api_key_id: &str) -> anyhow::Result<ApiKeySpend> {
        anyhow::bail!("selected storage backend does not support money configuration")
    }
}

#[async_trait]
pub trait ApiKeyStore: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<ApiKeyWithBindings>>;
    async fn get(&self, id: &str) -> anyhow::Result<Option<ApiKeyWithBindings>>;
    async fn create(&self, input: CreateApiKey) -> anyhow::Result<ApiKeyWithBindings>;
    async fn update(&self, id: &str, input: UpdateApiKey) -> anyhow::Result<ApiKeyWithBindings>;
    async fn delete(&self, id: &str) -> anyhow::Result<()>;
    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
    async fn exists_by_key(&self, key: &str, exclude_id: Option<&str>) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait AuthAccessStore: Send + Sync {
    async fn find_api_key(&self, raw_key: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>>;
    async fn find_api_key_by_id(&self, id: &str) -> anyhow::Result<Option<ApiKeyAccessRecord>>;
    /// An existing API key without model bindings has unrestricted model access.
    async fn model_access_allowed(&self, api_key_id: &str, model_id: &str) -> anyhow::Result<bool>;
    /// An empty list represents unrestricted access, not an empty allowlist.
    async fn list_bound_model_ids(&self, api_key_id: &str) -> anyhow::Result<Vec<String>>;
}

#[derive(Debug, Clone, Default)]
pub struct RouteSchedulingUsage {
    pub targets: Vec<crate::router::TargetSchedulingSnapshot>,
    pub stale: bool,
}

#[async_trait]
pub trait UsageStatsStore: Send + Sync {
    async fn route_scheduling_snapshot(&self) -> RouteSchedulingUsage;
    async fn stats_overview(&self, window: StatsWindow) -> anyhow::Result<StatsOverview>;
    /// Coarsen UTC base buckets over the complete selected interval to at most
    /// 512 presentation buckets, aligned to its start when explicitly selected.
    async fn stats_series(
        &self,
        window: StatsWindow,
        bucket_ms: i64,
    ) -> anyhow::Result<Vec<StatsSeries>>;
    async fn stats_by_model(&self, window: StatsWindow) -> anyhow::Result<Vec<ModelStats>>;
    async fn stats_by_provider(&self, window: StatsWindow) -> anyhow::Result<Vec<ProviderStats>>;
    async fn stats_by_api_key(&self, window: StatsWindow) -> anyhow::Result<Vec<ApiKeyStats>>;
}

#[async_trait]
pub trait OAuthCredentialStore: Send + Sync {
    async fn get(&self, provider_id: &str) -> anyhow::Result<Option<OAuthCredential>>;
    async fn upsert(
        &self,
        provider_id: &str,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential>;
    async fn delete(&self, provider_id: &str) -> anyhow::Result<()>;
    async fn try_begin_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<Option<OAuthCredential>>;
    async fn cancel_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
    ) -> anyhow::Result<bool>;
    async fn complete_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        input: UpsertOAuthCredential,
    ) -> anyhow::Result<OAuthCredential>;
    async fn fail_refresh(
        &self,
        provider_id: &str,
        expected_version: i32,
        error_message: &str,
    ) -> anyhow::Result<bool>;
    async fn list_expiring(&self, before: Duration) -> anyhow::Result<Vec<OAuthCredential>>;
    async fn recover_stale_refreshing(&self, timeout: Duration) -> anyhow::Result<u64>;
}

#[derive(Clone)]
pub struct AdminIdentityRecord {
    pub username: Option<String>,
    pub password_hash: Option<String>,
    pub jwt_secret: String,
    pub credential_revision: i64,
}

#[derive(Clone)]
pub struct AdminSessionRecord {
    pub id: String,
    pub credential_revision: i64,
    pub refresh_hash: String,
    pub expires_at: i64,
    pub revoked: bool,
}

pub struct NewAdminIdentity<'a> {
    pub username: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub jwt_secret: &'a str,
}

pub struct NewAdminSession<'a> {
    pub id: &'a str,
    pub credential_revision: i64,
    pub refresh_hash: &'a str,
    pub expires_at: i64,
}

#[async_trait]
pub trait AdminIdentityStore: Send + Sync {
    async fn load_identity(&self) -> anyhow::Result<Option<AdminIdentityRecord>>;
    async fn create_identity(&self, identity: NewAdminIdentity<'_>) -> anyhow::Result<bool>;
    async fn create_session(&self, session: NewAdminSession<'_>) -> anyhow::Result<bool>;
    async fn load_session_by_id(&self, id: &str) -> anyhow::Result<Option<AdminSessionRecord>>;
    async fn load_session_by_refresh_hash(
        &self,
        refresh_hash: &str,
    ) -> anyhow::Result<Option<AdminSessionRecord>>;
    async fn rotate_refresh(
        &self,
        id: &str,
        expected_refresh_hash: &str,
        new_refresh_hash: &str,
    ) -> anyhow::Result<bool>;
    async fn revoke_session(&self, id: &str) -> anyhow::Result<()>;
    async fn update_credentials_and_revoke_all(
        &self,
        expected_revision: i64,
        username: &str,
        password_hash: &str,
    ) -> anyhow::Result<bool>;
    async fn recover_credentials_and_revoke_all(
        &self,
        username: &str,
        password_hash: &str,
    ) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait StorageBootstrap: Send + Sync {
    async fn health(&self) -> anyhow::Result<StorageHealth>;
}

pub trait Storage: Send + Sync {
    fn vendor_plugins(&self) -> &crate::plugin::PluginStore;
    fn providers(&self) -> &dyn ProviderStore;
    fn web_providers(&self) -> Option<&dyn WebProviderStore> {
        None
    }
    fn routes(&self) -> &dyn RouteStore;
    fn provider_models(&self) -> &dyn ProviderModelStore;
    fn settings(&self) -> &dyn SettingsStore;
    fn money(&self) -> &dyn MoneyStore {
        &UNSUPPORTED_MONEY
    }
    fn api_keys(&self) -> Option<&dyn ApiKeyStore> {
        None
    }
    fn auth(&self) -> Option<&dyn AuthAccessStore> {
        None
    }
    fn admin_identity(&self) -> Option<&dyn AdminIdentityStore> {
        None
    }
    fn usage_stats(&self) -> &dyn UsageStatsStore;
    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore;
    fn bootstrap(&self) -> &dyn StorageBootstrap;
}

pub type DynStorage = Arc<dyn Storage>;
