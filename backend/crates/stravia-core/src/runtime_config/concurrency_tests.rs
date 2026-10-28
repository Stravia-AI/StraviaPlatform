use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

struct ControlledStorage {
    source: DynStorage,
    pause_read: AtomicBool,
    pause_write: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl ControlledStorage {
    fn new(source: DynStorage) -> Self {
        Self {
            source,
            pause_read: AtomicBool::new(false),
            pause_write: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        }
    }

    async fn reached(&self) -> anyhow::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.entered.notified()).await?;
        Ok(())
    }
}

#[async_trait]
impl SettingsStore for ControlledStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let value = self.source.settings().get(key).await?;
        if self.pause_read.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(value)
    }

    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.source.settings().set(key, value).await?;
        if self.pause_write.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

impl Storage for ControlledStorage {
    fn vendor_plugins(&self) -> &PluginStore {
        self.source.vendor_plugins()
    }
    fn providers(&self) -> &dyn ProviderStore {
        self.source.providers()
    }
    fn routes(&self) -> &dyn RouteStore {
        self.source.routes()
    }
    fn provider_models(&self) -> &dyn ProviderModelStore {
        self.source.provider_models()
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn money(&self) -> &dyn crate::storage::traits::MoneyStore {
        self.source.money()
    }
    fn usage_stats(&self) -> &dyn UsageStatsStore {
        self.source.usage_stats()
    }
    fn oauth_credentials(&self) -> &dyn OAuthCredentialStore {
        self.source.oauth_credentials()
    }
    fn bootstrap(&self) -> &dyn StorageBootstrap {
        self.source.bootstrap()
    }
}

#[tokio::test]
async fn old_read_cannot_repopulate_after_a_committed_write() -> anyhow::Result<()> {
    let (_directory, _pool, source) = super::tests::source().await?;
    let controlled = Arc::new(ControlledStorage::new(source));
    let config = RuntimeConfig::attach(controlled.clone()).await?;
    let key = crate::outbound_proxy::SETTINGS_KEY;
    config.settings().set(key, "old").await?;
    controlled.pause_read.store(true, Ordering::SeqCst);
    let reading = {
        let config = config.clone();
        tokio::spawn(async move { config.settings().get(key).await })
    };
    controlled.reached().await?;
    config.settings().set(key, "new").await?;
    controlled.release.notify_one();
    assert_eq!(reading.await??.as_deref(), Some("old"));
    assert_eq!(config.settings().get(key).await?.as_deref(), Some("new"));
    assert_eq!(config.settings().get(key).await?.as_deref(), Some("new"));
    Ok(())
}

#[tokio::test]
async fn cancelled_commit_does_not_freeze_an_unconfirmed_snapshot() -> anyhow::Result<()> {
    let (_directory, _pool, source) = super::tests::source().await?;
    let controlled = Arc::new(ControlledStorage::new(source.clone()));
    let config = RuntimeConfig::attach(controlled.clone()).await?;
    let key = crate::outbound_proxy::SETTINGS_KEY;
    config.settings().set(key, "old").await?;
    assert_eq!(config.settings().get(key).await?.as_deref(), Some("old"));
    controlled.pause_write.store(true, Ordering::SeqCst);
    let writing = {
        let config = config.clone();
        tokio::spawn(async move { config.settings().set(key, "committed").await })
    };
    controlled.reached().await?;
    writing.abort();
    assert!(writing.await.unwrap_err().is_cancelled());
    assert_eq!(
        config.settings().get(key).await?.as_deref(),
        Some("committed")
    );
    // 数据库执行结果未知时，后续读取不能重新固化一个可能早于迟到提交的值。
    source.settings().set(key, "authoritative").await?;
    assert_eq!(
        config.settings().get(key).await?.as_deref(),
        Some("authoritative")
    );
    Ok(())
}
