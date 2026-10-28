use serde_json::{Value, json};
use stravia_core::db::models::{PutRoute, RouteConfig, RouteOutputModality};
use stravia_core::storage::sql::config::SqlBackendConfig;
use stravia_core::storage::{PostgresStorage, SqliteStorage, Storage};

const HISTORICAL_ROUTES: &str = r#"
INSERT INTO providers (id, name, protocol, base_url, api_key)
VALUES ('provider', 'Existing Provider', 'openai-compatible', 'http://127.0.0.1:1', 'synthetic');
INSERT INTO models (id, model_id, display_name, balance, is_enabled, default_thinking_level)
VALUES ('enabled-key', 'enabled-route', 'Enabled Route', 'latency_preference', TRUE, 'high'),
       ('disabled-key', 'disabled-route', 'Disabled Route', 'traffic_equalization', FALSE, NULL);
INSERT INTO model_backends (id, model_id, provider_id, model, enabled, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms)
VALUES ('active-target', 'enabled-key', 'provider', 'dual-model', TRUE, 7, 12345, 2, 54321),
       ('backup-target', 'enabled-key', 'provider', 'image-model', FALSE, -3, 23456, 3, 65432),
       ('disabled-route-target', 'disabled-key', 'provider', 'image-model', TRUE, 9, 34567, 4, 76543);
INSERT INTO settings (name, value) VALUES ('media_generation_config', '{"enabled":false,"image":{"route_id":"disabled-route"}}');
"#;

fn rewrite(route: &RouteConfig, output_modality: Option<RouteOutputModality>) -> PutRoute {
    PutRoute {
        id: Some(route.id.clone()),
        model_id: route.model_id.clone(),
        display_name: route.display_name.clone(),
        output_modality,
        selection_strategy: route.balance.clone(),
        is_enabled: route.is_enabled,
        targets: None,
        default_thinking_level: route.default_thinking_level.as_deref().map(|level| {
            serde_json::from_value(json!(level)).expect("valid historical Thinking Level")
        }),
        billing: None,
    }
}

async fn upgrade_and_preserve(storage: &dyn Storage) -> anyhow::Result<()> {
    let enabled = storage.routes().get("enabled-route").await?.unwrap();
    let disabled = storage.routes().get("disabled-route").await?.unwrap();
    assert_eq!(enabled.id.as_str(), "enabled-key");
    assert_eq!(disabled.id.as_str(), "disabled-key");
    assert!(enabled.is_enabled);
    assert!(!disabled.is_enabled);
    assert_eq!(enabled.display_name.as_deref(), Some("Enabled Route"));
    assert_eq!(enabled.balance, "latency_preference");
    assert_eq!(enabled.default_thinking_level.as_deref(), Some("high"));
    assert_eq!(disabled.balance, "traffic_equalization");
    assert_eq!(disabled.default_thinking_level, None);
    assert_eq!(enabled.output_modality, Some(RouteOutputModality::Text));
    assert_eq!(disabled.output_modality, Some(RouteOutputModality::Text));
    assert_eq!(enabled.targets.len(), 2);
    assert_eq!(disabled.targets.len(), 1);
    for (target, id, model, enabled, priority, timeout, retries, cooldown) in [
        (
            &enabled.targets[0],
            "active-target",
            "dual-model",
            true,
            7,
            12345,
            2,
            54321,
        ),
        (
            &enabled.targets[1],
            "backup-target",
            "image-model",
            false,
            -3,
            23456,
            3,
            65432,
        ),
        (
            &disabled.targets[0],
            "disabled-route-target",
            "image-model",
            true,
            9,
            34567,
            4,
            76543,
        ),
    ] {
        assert_eq!(target.id.as_str(), id);
        assert_eq!(target.provider_id().as_str(), "provider");
        assert_eq!(target.model().as_str(), model);
        assert_eq!(target.enabled, enabled);
        assert_eq!(target.priority, priority);
        assert_eq!(target.first_token_timeout_ms, timeout);
        assert_eq!(target.target_retry_budget, retries);
        assert_eq!(target.target_cooldown_ms, cooldown);
    }
    let original_enabled = serde_json::to_value(&enabled)?;
    let original_disabled = serde_json::to_value(&disabled)?;
    let mut toggle = rewrite(&enabled, enabled.output_modality);
    toggle.is_enabled = false;
    let toggled = storage.routes().put(toggle).await?;
    assert_eq!(toggled.output_modality, Some(RouteOutputModality::Text));
    let mut ordinary = rewrite(&toggled, toggled.output_modality);
    ordinary.is_enabled = true;
    let unchanged = storage.routes().put(ordinary).await?;
    assert_eq!(serde_json::to_value(&unchanged)?, original_enabled);

    let repaired = storage
        .routes()
        .put(rewrite(&disabled, Some(RouteOutputModality::Image)))
        .await?;
    assert!(
        !repaired.is_enabled,
        "reconfiguration must not re-enable a disabled Route"
    );
    let mut expected = original_disabled;
    expected["output_modality"] = json!("image");
    assert_eq!(serde_json::to_value(&repaired)?, expected);
    assert_eq!(
        storage
            .routes()
            .get("enabled-route")
            .await?
            .unwrap()
            .output_modality,
        Some(RouteOutputModality::Text)
    );

    let repaired = storage
        .routes()
        .put(rewrite(&enabled, Some(RouteOutputModality::Text)))
        .await?;
    let mut expected = original_enabled;
    expected["output_modality"] = json!("text");
    assert_eq!(serde_json::to_value(&repaired)?, expected);
    assert_reference(storage).await
}

async fn assert_reference(storage: &dyn Storage) -> anyhow::Result<()> {
    let reference: Value = serde_json::from_str(
        &storage
            .settings()
            .get("media_generation_config")
            .await?
            .unwrap(),
    )?;
    assert_eq!(
        reference,
        json!({"enabled": false, "image": {"route_id": "disabled-route"}})
    );
    Ok(())
}

async fn assert_reconstructed(storage: &dyn Storage) -> anyhow::Result<()> {
    let enabled = storage.routes().get("enabled-route").await?.unwrap();
    let disabled = storage.routes().get("disabled-route").await?.unwrap();
    assert_eq!(enabled.output_modality, Some(RouteOutputModality::Text));
    assert_eq!(disabled.output_modality, Some(RouteOutputModality::Image));
    assert!(enabled.is_enabled);
    assert!(!disabled.is_enabled);
    assert_eq!(enabled.targets[0].id.as_str(), "active-target");
    assert_eq!(disabled.targets[0].id.as_str(), "disabled-route-target");
    assert_reference(storage).await
}

async fn sqlite_upgrade(version: i64) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(directory.path().join("routes.db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    let migrations = sqlx::migrate!("./migrations/sqlite");
    let prefix = sqlx::migrate::Migrator::with_migrations(
        migrations
            .iter()
            .filter(|migration| migration.version <= version)
            .cloned()
            .collect(),
    );
    prefix.run(pool.migration_pool()).await?;
    sqlx::raw_sql(HISTORICAL_ROUTES).execute(&pool).await?;
    if version >= 21 {
        sqlx::query("INSERT INTO models (id, model_id, output_modality) VALUES ('image-key', 'configured-image', 'image')")
            .execute(&pool)
            .await?;
    }
    stravia_core::migrations::migrate_sqlite(&pool, None).await?;
    let storage = SqliteStorage::from_pool(pool.clone());
    upgrade_and_preserve(&storage).await?;
    if version >= 21 {
        assert_eq!(
            storage
                .routes()
                .get("configured-image")
                .await?
                .unwrap()
                .output_modality,
            Some(RouteOutputModality::Image)
        );
    }
    for invalid in ["", "audio", "TEXT"] {
        assert!(
            sqlx::query("UPDATE models SET output_modality = ? WHERE id = 'enabled-key'")
                .bind(invalid)
                .execute(&pool)
                .await
                .is_err()
        );
    }
    drop(storage);
    pool.close().await;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    stravia_core::migrations::migrate_sqlite(&pool, None).await?;
    assert_reconstructed(&SqliteStorage::from_pool(pool.clone())).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_upgrade_defaults_unconfigured_routes_to_text_and_preserves_identity()
-> anyhow::Result<()> {
    for version in [20, 21] {
        sqlite_upgrade(version).await?;
    }
    Ok(())
}

async fn postgres_upgrade(version: i64) -> anyhow::Result<()> {
    let url = std::env::var("DB_URL")?;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("stravia_route_modality_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    let result = async {
        let migrations = sqlx::migrate!("./migrations/postgres");
        let prefix = sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        );
        prefix.run(pool.migration_pool()).await?;
        sqlx::raw_sql(HISTORICAL_ROUTES).execute(&pool).await?;
        if version >= 21 {
            sqlx::query("INSERT INTO models (id, model_id, output_modality) VALUES ('image-key', 'configured-image', 'image')")
                .execute(&pool)
                .await?;
        }
        stravia_core::migrations::migrate_postgres(&pool, None).await?;
        let mut storage_url = url::Url::parse(&url)?;
        storage_url
            .query_pairs_mut()
            .append_pair("options", &format!("-c search_path={schema}"));
        let config = SqlBackendConfig::with_url(storage_url.to_string());
        let storage = PostgresStorage::connect(config.clone()).await?;
        let assertions = async {
            upgrade_and_preserve(&storage).await?;
            if version >= 21 {
                assert_eq!(
                    storage.routes().get("configured-image").await?.unwrap().output_modality,
                    Some(RouteOutputModality::Image)
                );
            }
            anyhow::Ok(())
        }.await;
        storage.pool().close().await;
        assertions?;
        for invalid in ["", "audio", "TEXT"] {
            assert!(
                sqlx::query("UPDATE models SET output_modality = $1 WHERE id = 'enabled-key'")
                    .bind(invalid)
                    .execute(&pool)
                    .await
                    .is_err()
            );
        }
        stravia_core::migrations::migrate_postgres(&pool, None).await?;
        let reconstructed = PostgresStorage::connect(config).await?;
        let assertions = assert_reconstructed(&reconstructed).await;
        reconstructed.pool().close().await;
        assertions
    }
    .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

#[tokio::test]
#[ignore = "requires DB_URL pointing to an isolated PostgreSQL test server"]
async fn postgres_upgrade_defaults_unconfigured_routes_to_text_and_preserves_identity()
-> anyhow::Result<()> {
    for version in [20, 21] {
        postgres_upgrade(version).await?;
    }
    Ok(())
}
