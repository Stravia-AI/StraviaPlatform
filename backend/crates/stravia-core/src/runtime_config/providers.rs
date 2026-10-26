use std::collections::HashSet;

use async_trait::async_trait;

use super::RuntimeConfig;
use crate::db::models::{
    AllowanceSuspensionState, CreateProviderRecord, Provider, ProviderCredentialVersion,
    UpdateProvider,
};
use crate::storage::traits::{ProviderConnectionCheckWrite, ProviderStore};

#[async_trait]
impl ProviderStore for RuntimeConfig {
    async fn list(&self) -> anyhow::Result<Vec<Provider>> {
        self.source.providers().list().await
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Provider>> {
        self.read(
            |values| values.providers.get(id).cloned(),
            |values, provider| {
                values.providers.insert(id.to_owned(), provider);
            },
            || self.source.providers().get(id),
        )
        .await
    }

    async fn create(&self, input: CreateProviderRecord) -> anyhow::Result<Provider> {
        let mutation = self.projection.begin();
        let result = self.source.providers().create(input).await;
        mutation.finish();
        result
    }

    async fn update(&self, id: &str, input: UpdateProvider) -> anyhow::Result<Provider> {
        let mutation = self.projection.begin();
        let result = self.source.providers().update(id, input).await;
        mutation.finish();
        result
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let mutation = self.projection.begin();
        let result = self.source.providers().delete(id).await;
        mutation.finish();
        result
    }

    async fn exists_by_name(&self, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
        self.source
            .providers()
            .exists_by_name(name, exclude_id)
            .await
    }

    async fn record_test_result(
        &self,
        provider_id: &str,
        expected: ProviderCredentialVersion,
        result: ProviderConnectionCheckWrite,
    ) -> anyhow::Result<bool> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .providers()
            .record_test_result(provider_id, expected, result)
            .await;
        mutation.finish();
        result
    }

    async fn mark_credential_invalid(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
    ) -> anyhow::Result<bool> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .providers()
            .mark_credential_invalid(id, expected)
            .await;
        mutation.finish();
        result
    }

    async fn clear_credential_invalid(&self, id: &str) -> anyhow::Result<()> {
        let mutation = self.projection.begin();
        let result = self.source.providers().clear_credential_invalid(id).await;
        mutation.finish();
        result
    }

    async fn credential_invalid_provider_ids(&self) -> anyhow::Result<HashSet<String>> {
        self.read(
            |values| values.invalid_providers.clone(),
            |values, providers| values.invalid_providers = Some(providers),
            || self.source.providers().credential_invalid_provider_ids(),
        )
        .await
    }

    async fn guarded_allowance_keys(&self, id: &str) -> anyhow::Result<Vec<String>> {
        self.source.providers().guarded_allowance_keys(id).await
    }

    async fn replace_guarded_allowance_keys(
        &self,
        id: &str,
        keys: &[String],
    ) -> anyhow::Result<()> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .providers()
            .replace_guarded_allowance_keys(id, keys)
            .await;
        mutation.finish();
        result
    }

    async fn write_allowance_suspension(
        &self,
        id: &str,
        expected: ProviderCredentialVersion,
        state: &AllowanceSuspensionState,
        expected_guarded_keys: &[String],
    ) -> anyhow::Result<bool> {
        let mutation = self.projection.begin();
        let result = self
            .source
            .providers()
            .write_allowance_suspension(id, expected, state, expected_guarded_keys)
            .await;
        mutation.finish();
        result
    }

    async fn allowance_suspension(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<AllowanceSuspensionState>> {
        self.source.providers().allowance_suspension(id).await
    }

    async fn allowance_suspended_provider_ids(&self) -> anyhow::Result<HashSet<String>> {
        self.read(
            |values| values.suspended_providers.clone(),
            |values, providers| values.suspended_providers = Some(providers),
            || self.source.providers().allowance_suspended_provider_ids(),
        )
        .await
    }
}
