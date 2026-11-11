use super::*;
use crate::{
    db::models::{CreateProviderRecord, UpdateProvider},
    storage::SqliteStorage,
};

pub(super) async fn source() -> anyhow::Result<(tempfile::TempDir, sqlx::SqlitePool, DynStorage)> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::migrations::migrate_sqlite(&pool, None).await?;
    let source: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
    Ok((directory, pool, source))
}

fn provider() -> CreateProviderRecord {
    CreateProviderRecord {
        name: "runtime provider".into(),
        vendor: Some("custom".into()),
        protocol: "openai-compatible".into(),
        base_url: "http://127.0.0.1:9".into(),
        preset_key: None,
        channel: Some("default".into()),
        models_source: None,
        static_models: None,
        api_key: "local-test-credential".into(),
        adapter_credentials: "{}".into(),
        vendor_options: "{\"old\":true}".into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
    }
}

#[tokio::test]
async fn provider_changes_and_restart_keep_committed_configuration() -> anyhow::Result<()> {
    let (_directory, _pool, source) = source().await?;
    let config = RuntimeConfig::attach(source.clone()).await?;
    let provider = config.providers().create(provider()).await?;
    assert!(
        config
            .providers()
            .get(&provider.id)
            .await?
            .unwrap()
            .is_enabled
    );
    config
        .providers()
        .update(
            &provider.id,
            UpdateProvider {
                base_url: Some("http://127.0.0.1:19".into()),
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    let current = config.providers().get(&provider.id).await?.unwrap();
    assert!(!current.is_enabled);
    assert_eq!(current.base_url, "http://127.0.0.1:19");
    let restarted = RuntimeConfig::attach(source.clone()).await?;
    assert!(
        !restarted
            .providers()
            .get(&provider.id)
            .await?
            .unwrap()
            .is_enabled
    );
    config.providers().delete(&provider.id).await?;
    assert!(config.providers().get(&provider.id).await?.is_none());
    assert!(
        RuntimeConfig::attach(source)
            .await?
            .providers()
            .get(&provider.id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn failed_setting_write_preserves_durable_value_and_propagates_error() -> anyhow::Result<()> {
    let (_directory, pool, source) = source().await?;
    let config = RuntimeConfig::attach(source).await?;
    let key = crate::outbound_proxy::SETTINGS_KEY;
    config.settings().set(key, "original").await?;
    assert_eq!(
        config.settings().get(key).await?.as_deref(),
        Some("original")
    );
    let mut connection = pool.acquire_write().await?;
    sqlx::query("CREATE TRIGGER reject_proxy BEFORE UPDATE ON settings WHEN NEW.name = 'outbound_proxy' BEGIN SELECT RAISE(ABORT, 'test rejection'); END").execute(&mut *connection).await?;
    connection.finish().await?;
    assert!(config.settings().set(key, "rejected").await.is_err());
    assert_eq!(
        config.settings().get(key).await?.as_deref(),
        Some("original")
    );
    Ok(())
}

#[tokio::test]
async fn conditional_credential_and_allowance_changes_preserve_storage_contract()
-> anyhow::Result<()> {
    let (_directory, _pool, source) = source().await?;
    let config = RuntimeConfig::attach(source).await?;
    let provider = config.providers().create(provider()).await?;
    crate::storage::traits::verify_allowance_storage_contract(
        config.providers(),
        config.oauth_credentials(),
        provider,
    )
    .await
}
