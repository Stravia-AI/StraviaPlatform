use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn required_target_model_migration_preserves_model_targets_and_rpm_settings() {
    let mut db = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    for migration in [
        include_str!("../migrations/sqlite/0001_baseline.sql"),
        include_str!("../migrations/sqlite/0002_data_contracts.sql"),
        include_str!("../migrations/sqlite/0003_estimated_input_tokens.sql"),
        include_str!("../migrations/sqlite/0004_observation_recovery_indexes.sql"),
        include_str!("../migrations/sqlite/0005_credential_custom_rules.sql"),
        include_str!("../migrations/sqlite/0006_model_specification.sql"),
        include_str!("../migrations/sqlite/0007_history_items.sql"),
        include_str!("../migrations/sqlite/0008_observation_storage.sql"),
        include_str!("../migrations/sqlite/0009_rpm_admission.sql"),
        include_str!("../migrations/sqlite/0010_rpm_destination_pool.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&mut db).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO providers (id, name, protocol, base_url, api_key) VALUES ('provider', 'Provider', 'openai', 'http://localhost', 'test');
         INSERT INTO models (id, model_id) VALUES ('route', 'search'), ('orphan', 'old-search');
         INSERT INTO model_backends (id, model_id, provider_id, model, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, enabled)
         VALUES ('keep', 'route', 'provider', 'real-model', -7, 123, 2, 456, 0),
                ('remove', 'route', 'provider', NULL, 0, 60000, 5, 120000, 1),
                ('remove-only', 'orphan', 'provider', NULL, 0, 60000, 5, 120000, 1);",
    )
    .execute(&mut db)
    .await
    .unwrap();
    let retained = json!({"provider_id": "provider", "model": "real-model", "rpm_limit": null, "rpm_pool_id": "pool"});
    let settings = json!({
        "preferred_target_wait_ms": 987,
        "destinations": [
            {"provider_id": "provider", "model": null, "rpm_limit": 3},
            retained.clone()
        ],
        "pools": [{"id": "pool", "name": "Shared", "rpm_limit": 9}]
    });
    sqlx::query("INSERT INTO settings (name, value) VALUES ('rpm_admission', ?)")
        .bind(settings.to_string())
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/sqlite/0011_remove_provider_only_targets.sql"
    ))
    .execute(&mut db)
    .await
    .unwrap();

    let targets: Vec<(String, String, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT id, model, priority, first_token_timeout_ms, target_retry_budget, target_cooldown_ms, enabled FROM model_backends",
    )
    .fetch_all(&mut db)
    .await
    .unwrap();
    assert_eq!(
        targets,
        vec![("keep".into(), "real-model".into(), -7, 123, 2, 456, 0)]
    );
    let routes: Vec<String> = sqlx::query_scalar("SELECT id FROM models ORDER BY id")
        .fetch_all(&mut db)
        .await
        .unwrap();
    assert_eq!(routes, vec!["orphan", "route"]);
    let value: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE name = 'rpm_admission'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    let mut expected = settings;
    expected["destinations"] = json!([retained]);
    assert_eq!(serde_json::from_str::<Value>(&value).unwrap(), expected);

    for model in [None, Some(""), Some("   ")] {
        assert!(sqlx::query("INSERT INTO model_backends (id, model_id, provider_id, model) VALUES ('invalid', 'route', 'provider', ?)")
            .bind(model)
            .execute(&mut db)
            .await
            .is_err());
    }
    assert!(
        sqlx::query("UPDATE model_backends SET priority = 2147483648 WHERE id = 'keep'")
            .execute(&mut db)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE model_backends SET provider_id = 'missing' WHERE id = 'keep'")
            .execute(&mut db)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM models WHERE id = 'route'")
        .execute(&mut db)
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM model_backends")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}
