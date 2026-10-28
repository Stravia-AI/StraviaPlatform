use std::sync::Arc;

use chrono::{Duration, SecondsFormat, Utc};
use stravia_core::db::models::{CreateProviderRecord, UpdateProvider};
use stravia_core::provider_models::{
    ModelChangeItem, ModelChangeOutcome, ModelChangeQuery, ModelChangeRecord, ModelChangeSource,
    ModelChangeType, ModelSpecificationDiff, ProviderModelPresence, ProviderModelSyncSummary,
};
use stravia_core::storage::sql::config::SqlBackendConfig;
use stravia_core::storage::{DynStorage, MemoryStorage, PostgresStorage, SqliteStorage};
use stravia_core::{Gateway, config::GatewayConfig};

fn provider_input(name: &str) -> CreateProviderRecord {
    CreateProviderRecord {
        name: name.into(),
        vendor: None,
        protocol: "openai-compatible".into(),
        base_url: "http://127.0.0.1:1".into(),
        preset_key: None,
        channel: None,
        models_source: None,
        static_models: None,
        api_key: String::new(),
        adapter_credentials: "{}".into(),
        vendor_options: "{}".into(),
        auth_mode: "apikey".into(),
        use_proxy: false,
    }
}

fn record(id: &str, provider_id: &str, checked_at: &str) -> ModelChangeRecord {
    ModelChangeRecord {
        id: id.into(),
        provider_id: provider_id.into(),
        provider_name: "Original service".into(),
        provider_deleted: false,
        source: ModelChangeSource::ManualSync,
        checked_at: checked_at.into(),
        outcome: ModelChangeOutcome::Success,
        initial_sync: false,
        error: None,
        changes: vec![
            ModelChangeItem {
                model_id: "vendor/ÉCLAIR_%".into(),
                name: "Éclair model".into(),
                types: vec![ModelChangeType::SpecificationUpdated],
                specification: vec![ModelSpecificationDiff {
                    field: "context".into(),
                    before: serde_json::Value::Null,
                    after: serde_json::json!(0),
                }],
                before_presence: Some(ProviderModelPresence::Present),
                after_presence: Some(ProviderModelPresence::Present),
                before_deprecated: Some(false),
                after_deprecated: Some(false),
                before_policy: None,
                after_policy: None,
            },
            ModelChangeItem {
                model_id: "unrelated".into(),
                name: "Other model".into(),
                types: vec![ModelChangeType::Missing],
                specification: Vec::new(),
                before_presence: Some(ProviderModelPresence::Present),
                after_presence: Some(ProviderModelPresence::Missing),
                before_deprecated: Some(false),
                after_deprecated: Some(false),
                before_policy: None,
                after_policy: None,
            },
        ],
    }
}

// The test host seeds confirmed historic operations through storage; every
// observation below uses the same public AdminService as the management API.
async fn seed(gateway: &Gateway, record: ModelChangeRecord) -> anyhow::Result<()> {
    let summary = ProviderModelSyncSummary {
        added: 0,
        missing: 1,
        restored: 0,
        deprecated: 0,
        checked_at: record.checked_at.clone(),
        source: record.source,
        record_id: Some(record.id.clone()),
        deprecation_cleared: 0,
        specification_updated: 1,
        outcome: record.outcome,
        error: record.error.clone(),
    };
    gateway
        .storage
        .provider_models()
        .save_sync_failure(&record.provider_id.clone(), summary, record)
        .await
}

async fn history_contract(storage: DynStorage) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        storage,
    )
    .await?;
    let result = history_assertions(&gateway).await;
    gateway.shutdown().await;
    result
}

async fn history_assertions(gateway: &Gateway) -> anyhow::Result<()> {
    let provider = gateway
        .storage
        .providers()
        .create(provider_input("Original service"))
        .await?;
    let now = Utc::now();
    let current = (now - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let expired = (now - Duration::days(90) - Duration::minutes(1))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    seed(gateway, record("expired", &provider.id, &expired)).await?;
    for id in ["a", "b", "c"] {
        seed(gateway, record(id, &provider.id, &current)).await?;
    }
    let first = gateway
        .admin()
        .list_model_changes(ModelChangeQuery {
            provider_id: Some(provider.id.clone()),
            limit: Some(1),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        first
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        ["c"]
    );
    assert!(gateway.admin().get_model_change("expired").await.is_err());

    // Insertions ahead of the page boundary cannot shift keyset pagination,
    // including a newly committed record with exactly the same timestamp.
    seed(gateway, record("d", &provider.id, &current)).await?;
    let second = gateway
        .admin()
        .list_model_changes(ModelChangeQuery {
            provider_id: Some(provider.id.clone()),
            cursor: first.next_cursor,
            limit: Some(1),
            ..Default::default()
        })
        .await?;
    assert_eq!(second.records[0].id, "b");
    let third = gateway
        .admin()
        .list_model_changes(ModelChangeQuery {
            provider_id: Some(provider.id.clone()),
            cursor: second.next_cursor,
            limit: Some(1),
            ..Default::default()
        })
        .await?;
    assert_eq!(third.records[0].id, "a");
    assert!(third.next_cursor.is_none());

    let filtered = gateway
        .admin()
        .list_model_changes(ModelChangeQuery {
            provider_id: Some(provider.id.clone()),
            q: Some("éclair_%".into()),
            source: Some(ModelChangeSource::ManualSync),
            change_type: Some(ModelChangeType::SpecificationUpdated),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        filtered
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        ["d", "c", "b", "a"]
    );
    assert_eq!(filtered.records[0].changes[1].model_id, "unrelated");
    assert_eq!(
        filtered.records[0].changes[0].specification[0].before,
        serde_json::Value::Null
    );
    assert_eq!(
        filtered.records[0].changes[0].specification[0].after,
        serde_json::json!(0)
    );
    let no_wildcard = gateway
        .admin()
        .list_model_changes(ModelChangeQuery {
            q: Some("%absent".into()),
            ..Default::default()
        })
        .await?;
    assert!(no_wildcard.records.is_empty());

    let last_check = gateway
        .admin()
        .provider_model_sync_state(&provider.id)
        .await?
        .last_check;
    gateway
        .admin()
        .update_provider_model_sync_state(
            &provider.id,
            stravia_core::provider_models::UpdateProviderModelSyncState {
                automatic_sync: false,
            },
        )
        .await?;
    let state = gateway
        .admin()
        .provider_model_sync_state(&provider.id)
        .await?;
    assert!(!state.automatic_sync);
    assert_eq!(state.last_check, last_check);

    // A post-commit error updates the saved outcome, not the already confirmed
    // model facts, Provider name snapshot, or identity of the operation.
    let mut partial = record("a", &provider.id, &current);
    partial.outcome = ModelChangeOutcome::PartialSuccess;
    partial.error = Some("Post-commit recovery failed".into());
    partial.provider_name = "Later name must not replace the snapshot".into();
    partial.changes.clear();
    seed(gateway, partial).await?;
    let saved = gateway.admin().get_model_change("a").await?;
    assert_eq!(saved.outcome, ModelChangeOutcome::PartialSuccess);
    assert_eq!(saved.error.as_deref(), Some("Post-commit recovery failed"));
    assert_eq!(saved.provider_name, "Original service");
    assert_eq!(saved.changes, record("a", &provider.id, &current).changes);
    let failures = gateway
        .admin()
        .list_model_changes(ModelChangeQuery {
            change_type: Some(ModelChangeType::SyncFailed),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        failures
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );

    gateway
        .storage
        .providers()
        .update(
            &provider.id,
            UpdateProvider {
                name: Some("Renamed service".into()),
                ..Default::default()
            },
        )
        .await?;
    let historical = gateway.admin().get_model_change("a").await?;
    assert_eq!(historical.provider_name, "Original service");
    assert!(!historical.provider_deleted);
    gateway.admin().delete_provider(&provider.id).await?;
    let historical = gateway.admin().get_model_change("a").await?;
    assert!(historical.provider_deleted);
    assert_eq!(
        historical.changes,
        record("a", &provider.id, &current).changes
    );
    let mut deleted_failure = record("deleted-failure", &provider.id, &current);
    deleted_failure.outcome = ModelChangeOutcome::Failure;
    deleted_failure.error = Some("Provider changed during discovery".into());
    deleted_failure.changes.clear();
    seed(gateway, deleted_failure).await?;
    let deleted_failure = gateway.admin().get_model_change("deleted-failure").await?;
    assert!(deleted_failure.provider_deleted);
    assert_eq!(deleted_failure.outcome, ModelChangeOutcome::Failure);
    assert_eq!(deleted_failure.provider_name, "Original service");
    let replacement = gateway
        .storage
        .providers()
        .create(provider_input("Original service"))
        .await?;
    assert_ne!(replacement.id, provider.id);
    assert!(
        gateway
            .admin()
            .list_model_changes(ModelChangeQuery {
                provider_id: Some(replacement.id),
                ..Default::default()
            })
            .await?
            .records
            .is_empty()
    );

    // Explicit cleanup has a deterministic strict-older-than boundary, without
    // changing the product's 90-day lifetime or racing the wall clock.
    let before = (now - Duration::hours(2)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let older = (now - Duration::hours(2) - Duration::milliseconds(1))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let replacement = gateway
        .storage
        .providers()
        .create(provider_input("Cleanup service"))
        .await?;
    seed(gateway, record("at-cutoff", &replacement.id, &before)).await?;
    seed(gateway, record("before-cutoff", &replacement.id, &older)).await?;
    gateway
        .storage
        .provider_models()
        .prune_changes(&before)
        .await?;
    assert!(
        gateway
            .admin()
            .get_model_change("before-cutoff")
            .await
            .is_err()
    );
    assert_eq!(
        gateway.admin().get_model_change("at-cutoff").await?.id,
        "at-cutoff"
    );
    assert_eq!(gateway.admin().get_model_change("a").await?.id, "a");
    let retained = (now - Duration::days(90) + Duration::minutes(1))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    seed(
        gateway,
        record("just-inside-retention", &replacement.id, &retained),
    )
    .await?;
    assert_eq!(
        gateway
            .admin()
            .get_model_change("just-inside-retention")
            .await?
            .checked_at,
        retained
    );
    Ok(())
}

#[tokio::test]
async fn memory_admin_history_preserves_batches_and_retention() -> anyhow::Result<()> {
    history_contract(Arc::new(MemoryStorage::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )))
    .await
}

#[tokio::test]
async fn sqlite_admin_history_preserves_batches_and_retention() -> anyhow::Result<()> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await?;
    stravia_core::migrations::migrate_sqlite(&pool, None).await?;
    history_contract(Arc::new(SqliteStorage::from_pool(pool.clone()))).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_upgrade_keeps_existing_services_without_inventing_history() -> anyhow::Result<()> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await?;
    let migrations = sqlx::migrate!("./migrations/sqlite");
    let prefix = sqlx::migrate::Migrator::with_migrations(
        migrations
            .iter()
            .filter(|migration| migration.version < 19)
            .cloned()
            .collect(),
    );
    prefix.run(pool.migration_pool()).await?;
    sqlx::query("INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('existing-service', 'Existing service', 'openai-compatible', 'http://127.0.0.1:1', '')")
        .execute(&pool)
        .await?;
    let storage: DynStorage = Arc::new(SqliteStorage::from_pool(pool.clone()));
    stravia_core::migrations::migrate_sqlite(&pool, None).await?;
    upgrade_contract(storage, "existing-service").await?;
    pool.close().await;
    Ok(())
}

async fn upgrade_contract(storage: DynStorage, existing_id: &str) -> anyhow::Result<()> {
    let fresh = storage
        .providers()
        .create(provider_input("New service"))
        .await?;
    let directory = tempfile::tempdir()?;
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        storage,
    )
    .await?;
    let existing_state = gateway
        .admin()
        .provider_model_sync_state(existing_id)
        .await?;
    assert!(existing_state.automatic_sync);
    assert!(existing_state.has_successful_sync);
    assert!(existing_state.last_check.is_none());
    let fresh_state = gateway.admin().provider_model_sync_state(&fresh.id).await?;
    assert!(fresh_state.automatic_sync);
    assert!(!fresh_state.has_successful_sync);
    assert!(fresh_state.last_check.is_none());
    assert!(
        gateway
            .admin()
            .list_model_changes(Default::default())
            .await?
            .records
            .is_empty()
    );
    gateway.shutdown().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires DB_URL pointing to an isolated PostgreSQL test server"]
async fn postgres_admin_history_preserves_batches_and_retention() -> anyhow::Result<()> {
    let url = std::env::var("DB_URL")?;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("stravia_model_history_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    let result = async {
        let migrations = sqlx::migrate!("./migrations/postgres");
        let prefix = sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|migration| migration.version < 19)
                .cloned()
                .collect(),
        );
        prefix.run(pool.migration_pool()).await?;
        sqlx::query("INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('existing-service', 'Existing service', 'openai-compatible', 'http://127.0.0.1:1', '')")
            .execute(&pool)
            .await?;
        let mut storage_url = url::Url::parse(&url)?;
        storage_url
            .query_pairs_mut()
            .append_pair("options", &format!("-c search_path={schema}"));
        let postgres =
            PostgresStorage::connect(SqlBackendConfig::with_url(storage_url.to_string())).await?;
        let storage: DynStorage = Arc::new(postgres.clone());
        stravia_core::migrations::migrate_postgres(&pool, None).await?;
        upgrade_contract(storage.clone(), "existing-service").await?;
        let result = history_contract(storage).await;
        postgres.pool().close().await;
        result
    }
    .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}
