use async_trait::async_trait;

use super::RuntimeConfig;
use crate::storage::traits::SettingsStore;

#[async_trait]
impl SettingsStore for RuntimeConfig {
    async fn get_or_insert(&self, key: &str, value: &str) -> anyhow::Result<String> {
        let mutation = self.projection.begin();
        let result = self.source.settings().get_or_insert(key, value).await;
        mutation.finish();
        result
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        if key != crate::outbound_proxy::SETTINGS_KEY {
            return self.source.settings().get(key).await;
        }
        self.read(
            |values| values.settings.get(key).cloned(),
            |values, value| {
                values.settings.insert(key.to_owned(), value);
            },
            || self.source.settings().get(key),
        )
        .await
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mutation = self.projection.begin();
        let result = self.source.settings().set(key, value).await;
        mutation.finish();
        result
    }
}
