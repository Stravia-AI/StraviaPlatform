use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;

use crate::db::models::{
    ApiKeyStats, ApiKeyWithBindings, CreateApiKey, CreateProviderRecord, CreateWebProvider,
    ModelStats, OAuthCredential, Provider, ProviderCredentialVersion, ProviderStats, PutRoute,
    Route, StatsOverview,
    StatsSeries, UpdateApiKey, UpdateProvider, UpdateWebProvider, UpsertOAuthCredential,
    WebAccessSettings, WebProvider,
};
use crate::provider_models::{
    NewProviderModelRecord, ProviderModelMutation, ProviderModelReconciliation,
    ProviderModelRecord, ProviderModelSelectionPolicy, model_id_match_key,
};

#[derive(Debug, Clone)]
pub struct ProviderTestResult {
    pub success: bool,
    pub tested_at: String,
}

#[derive(Debug, Clone)]
pub struct ApiKeyAccessRecord {
    pub id: String,
    pub name: String,
    pub is_enabled: bool,
    pub expires_at: Option<String>,
    pub concurrency_limit: Option<i32>,
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
    async fn record_test_result(
        &self,
        provider_id: &str,
        result: ProviderTestResult,
    ) -> anyhow::Result<()>;
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
    async fn list(&self) -> anyhow::Result<Vec<Route>>;
    async fn list_active(&self) -> anyhow::Result<Vec<Route>>;
    async fn get(&self, route_id: &str) -> anyhow::Result<Option<Route>>;
    async fn put(&self, route: PutRoute) -> anyhow::Result<Route>;
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
    ) -> anyhow::Result<()>;
    async fn create(&self, input: NewProviderModelRecord) -> anyhow::Result<ProviderModelMutation>;
    async fn update_metadata(
        &self,
        provider_id: &str,
        model_id: &str,
        metadata: crate::provider_models::ProviderModelMetadata,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation>;
    async fn update_selection_policy(
        &self,
        provider_id: &str,
        model_id: &str,
        policy: ProviderModelSelectionPolicy,
        expected_revision: i64,
    ) -> anyhow::Result<ProviderModelMutation>;
    async fn delete_manual(&self, provider_id: &str, model_id: &str) -> anyhow::Result<bool>;
}

#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()>;
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
    async fn stats_overview(&self, hours: Option<i64>) -> anyhow::Result<StatsOverview>;
    /// Buckets are aligned to the caller's wall clock: `tz_offset_ms` is added to
    /// `started_at` before integer bucket division and subtracted back, so a day
    /// bucket boundary lands on local midnight while `bucket_start` stays a real
    /// instant.
    async fn stats_series(
        &self,
        hours: i64,
        bucket_ms: i64,
        tz_offset_ms: i64,
    ) -> anyhow::Result<Vec<StatsSeries>>;
    async fn stats_by_model(&self, hours: Option<i64>) -> anyhow::Result<Vec<ModelStats>>;
    async fn stats_by_provider(&self, hours: Option<i64>) -> anyhow::Result<Vec<ProviderStats>>;
    async fn stats_by_api_key(&self, hours: Option<i64>) -> anyhow::Result<Vec<ApiKeyStats>>;
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
