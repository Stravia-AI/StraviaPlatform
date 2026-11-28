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
        proxy_mode: "none".into(),
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
    let mut connection = pool.begin_write().await?;
    sqlx::query("CREATE TRIGGER reject_proxy BEFORE UPDATE ON settings WHEN NEW.name = 'outbound_proxy' BEGIN SELECT RAISE(ABORT, 'test rejection'); END").execute(&mut *connection).await?;
    connection.commit().await?;
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

#[tokio::test]
async fn api_key_auth_projection_caches_reads_and_invalidates_immediately() -> anyhow::Result<()> {
    use crate::db::models::{CreateApiKey, CreateTarget, PutRoute, RoutePurpose, UpdateApiKey};

    let (_directory, _pool, source) = source().await?;
    let config = RuntimeConfig::attach(source).await?;
    let target_provider = config.providers().create(provider()).await?;
    let route = config
        .routes()
        .put(PutRoute {
            id: None,
            model_id: "route-a".into(),
            display_name: Some("Route A".into()),
            purpose: Some(RoutePurpose::Inference),
            is_enabled: true,
            selection_strategy: "traffic_equalization".into(),
            targets: Some(vec![CreateTarget {
                provider_id: target_provider.id.clone(),
                model: "gpt-4o-mini".into(),
                enabled: true,
                priority: Some(1),
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                config_mode: None,
                thinking_level_map: Vec::new(),
            }]),
            default_thinking_level: None,
            billing: None,
        })
        .await?;
    let route_id = route.id.to_string();

    let api_keys = config.api_keys().expect("api keys");
    let auth = config.auth().expect("auth");
    let created = api_keys
        .create(CreateApiKey {
            name: "hot-key".into(),
            key: Some("sk-hot-secret-1".into()),
            rpm_limit: Some(60),
            spend_limit: Some("10.00".into()),
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            transparent_injections: Default::default(),
            expires_at: None,
            model_ids: vec![route_id.clone()],
        })
        .await?;

    // Warm caches by token, id, and bound models.
    let found = auth
        .find_api_key("sk-hot-secret-1")
        .await?
        .expect("key found by token");
    assert_eq!(found.id, created.id);
    assert!(found.is_enabled);
    assert_eq!(
        auth.find_api_key_by_id(&created.id)
            .await?
            .expect("key found by id")
            .name,
        "hot-key"
    );
    assert!(auth.model_access_allowed(&created.id, &route_id).await?);
    assert!(
        !auth
            .model_access_allowed(&created.id, "other-route")
            .await?
    );
    assert_eq!(
        auth.list_bound_model_ids(&created.id).await?,
        vec![route_id.clone()]
    );

    // Random unknown tokens must not pollute or grow the positive cache.
    for i in 0..32 {
        assert!(
            auth.find_api_key(&format!("sk-invalid-{i}"))
                .await?
                .is_none()
        );
    }

    // Disabling and rotating key must invalidate immediately (0ms staleness).
    api_keys
        .update(
            &created.id,
            UpdateApiKey {
                key: Some("sk-hot-secret-2".into()),
                name: None,
                rpm_limit: None,
                spend_limit: None,
                is_enabled: Some(false),
                mcp_access_enabled: None,
                transparent_injection_enabled: None,
                transparent_injections: None,
                expires_at: None,
                model_ids: None,
            },
        )
        .await?;
    assert!(auth.find_api_key("sk-hot-secret-1").await?.is_none());
    let rotated = auth
        .find_api_key("sk-hot-secret-2")
        .await?
        .expect("rotated key found");
    assert!(!rotated.is_enabled);

    // Custom currency setup converts spend_limit_nanos in DB and must invalidate cached records.
    let original_limit = rotated.spend_limit_nanos.expect("spend limit");
    config
        .money()
        .save_custom_currency("CNY", rust_decimal::Decimal::new(72, 1))
        .await?;
    let converted = auth
        .find_api_key("sk-hot-secret-2")
        .await?
        .expect("key after currency conversion");
    assert_ne!(converted.spend_limit_nanos, Some(original_limit));

    // Deleting the bound route cascades into api_key_models; cache must invalidate immediately.
    config.routes().delete("route-a").await?;
    assert!(auth.list_bound_model_ids(&created.id).await?.is_empty());
    assert!(auth.model_access_allowed(&created.id, "any-route").await?);

    // Deleting key must invalidate immediately across every lookup.
    api_keys.delete(&created.id).await?;
    assert!(auth.find_api_key("sk-hot-secret-2").await?.is_none());
    assert!(auth.find_api_key_by_id(&created.id).await?.is_none());
    assert!(!auth.model_access_allowed(&created.id, "any-route").await?);
    Ok(())
}
