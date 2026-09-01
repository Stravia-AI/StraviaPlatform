use sqlx::{PgPool, SqlitePool, migrate::Migrator};

// Keep equivalent schema changes in both backend directories; embedded migrations are the runtime source.
static SQLITE_MIGRATOR: Migrator = sqlx::migrate!("./migrations/sqlite");
static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");

pub async fn migrate_sqlite(pool: &SqlitePool) -> anyhow::Result<()> {
    SQLITE_MIGRATOR.run(pool).await?;
    Ok(())
}

pub async fn migrate_postgres(pool: &PgPool) -> anyhow::Result<()> {
    POSTGRES_MIGRATOR.run(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{POSTGRES_MIGRATOR, SQLITE_MIGRATOR};
    use sha2::{Digest, Sha384};
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn embedded_migrations_use_platform_independent_line_endings() {
        for migration in SQLITE_MIGRATOR.iter().chain(POSTGRES_MIGRATOR.iter()) {
            assert!(
                !migration.sql.as_str().as_bytes().contains(&b'\r'),
                "migration {} contains CRLF line endings",
                migration.version
            );
        }
    }
    #[test]
    fn applied_media_permission_migrations_remain_immutable() {
        const APPLIED_SQLITE_SQL: &str = "ALTER TABLE api_keys\n\
ADD COLUMN allow_media_understanding INTEGER NOT NULL DEFAULT 0;\n";
        const APPLIED_POSTGRES_SQL: &str = "ALTER TABLE api_keys\n\
ADD COLUMN allow_media_understanding BOOLEAN NOT NULL DEFAULT FALSE;\n";

        for (migrator, applied_sql) in [
            (&SQLITE_MIGRATOR, APPLIED_SQLITE_SQL),
            (&POSTGRES_MIGRATOR, APPLIED_POSTGRES_SQL),
        ] {
            let migration = migrator
                .iter()
                .find(|migration| migration.version == 11)
                .expect("Media Understanding permission migration");

            assert_eq!(
                migration.checksum.as_ref(),
                Sha384::digest(applied_sql.as_bytes()).as_slice()
            );
        }
    }

    async fn migrate_sqlite_range(pool: &sqlx::SqlitePool, min_version: i64, max_version: i64) {
        for migration in SQLITE_MIGRATOR.iter().filter(|migration| {
            migration.version >= min_version && migration.version <= max_version
        }) {
            sqlx::raw_sql(migration.sql.as_ref())
                .execute(pool)
                .await
                .unwrap_or_else(|error| {
                    panic!("migration {} must apply: {error}", migration.version)
                });
        }
    }

    async fn migrate_postgres_range(pool: &sqlx::PgPool, min_version: i64, max_version: i64) {
        for migration in POSTGRES_MIGRATOR.iter().filter(|migration| {
            migration.version >= min_version && migration.version <= max_version
        }) {
            sqlx::raw_sql(migration.sql.as_ref())
                .execute(pool)
                .await
                .unwrap_or_else(|error| {
                    panic!("migration {} must apply: {error}", migration.version)
                });
        }
    }

    #[tokio::test]
    async fn web_search_migration_prefills_only_one_codex_binding_and_prunes_priorities() {
        for codex_count in 0..=2 {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("SQLite pool");
            migrate_sqlite_range(&pool, 1, 9).await;
            sqlx::query(
                "INSERT INTO web_providers (id, name, kind, api_key, provider_id)
                 VALUES ('exa-source', 'Exa source', 'exa', 'exa-key', NULL)",
            )
            .execute(&pool)
            .await
            .expect("Exa source");
            for index in 0..codex_count {
                sqlx::query(
                    "INSERT INTO providers (
                        id, name, protocol, base_url, api_key, auth_mode
                     ) VALUES (?, ?, 'openai-responses', 'https://example.com', '', 'oauth')",
                )
                .bind(format!("provider-{index}"))
                .bind(format!("Codex Provider {index}"))
                .execute(&pool)
                .await
                .expect("legacy Codex Provider");
                sqlx::query(
                    "INSERT INTO web_providers (id, name, kind, api_key, provider_id)
                     VALUES (?, ?, 'codex', NULL, ?)",
                )
                .bind(format!("legacy-codex-{index}"))
                .bind(format!("Legacy Codex {index}"))
                .bind(format!("provider-{index}"))
                .execute(&pool)
                .await
                .expect("legacy Codex source");
            }
            let mut priority = vec!["exa-source".to_string()];
            priority.extend((0..codex_count).map(|index| format!("legacy-codex-{index}")));
            sqlx::query(
                "INSERT INTO settings (name, value) VALUES
                 ('web_access_search_provider_ids', ?),
                 ('web_access_fetch_provider_ids', ?)",
            )
            .bind(serde_json::to_string(&priority).unwrap())
            .bind(serde_json::to_string(&priority).unwrap())
            .execute(&pool)
            .await
            .expect("legacy priorities");
            sqlx::query(
                "INSERT INTO turn_chain_nodes (
                    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at
                 ) VALUES
                    ('root-turn', 'response', NULL, 'api_key:test', 1, '{}', 1, 1000),
                    ('child-turn', 'agent', 'root-turn', 'api_key:test', 1, '{}', 2, 1000)",
            )
            .execute(&pool)
            .await
            .expect("legacy Turn chain");

            migrate_sqlite_range(&pool, 10, 10).await;

            let providers = sqlx::query_as::<_, (String, String)>(
                "SELECT id, kind FROM web_providers ORDER BY id",
            )
            .fetch_all(&pool)
            .await
            .expect("migrated providers");
            assert_eq!(providers, vec![("exa-source".into(), "exa".into())]);
            let columns = sqlx::query_scalar::<_, String>(
                "SELECT name FROM pragma_table_info('web_providers') ORDER BY cid",
            )
            .fetch_all(&pool)
            .await
            .expect("migrated Web Provider columns");
            assert!(!columns.iter().any(|column| column == "provider_id"));
            let child_parent = sqlx::query_scalar::<_, String>(
                "SELECT parent_id FROM turn_chain_nodes WHERE id = 'child-turn'",
            )
            .fetch_one(&pool)
            .await
            .expect("migrated child Turn");
            assert_eq!(child_parent, "root-turn");
            for key in [
                "web_access_search_provider_ids",
                "web_access_fetch_provider_ids",
            ] {
                let value =
                    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = ?")
                        .bind(key)
                        .fetch_one(&pool)
                        .await
                        .expect("migrated priority");
                assert_eq!(
                    serde_json::from_str::<Vec<String>>(&value).unwrap(),
                    ["exa-source"]
                );
            }
            let config = sqlx::query_scalar::<_, String>(
                "SELECT value FROM settings WHERE name = 'web_research_config'",
            )
            .fetch_optional(&pool)
            .await
            .expect("migrated config");
            if codex_count == 1 {
                let config: serde_json::Value =
                    serde_json::from_str(&config.expect("single Codex draft")).unwrap();
                assert_eq!(config["enabled"], false);
                assert_eq!(config["backend"]["kind"], "codex");
                assert_eq!(config["backend"]["provider_id"], "provider-0");
                assert!(config["backend"]["upstream_model"].is_null());
            } else {
                assert!(config.is_none());
            }
        }
    }

    #[tokio::test]
    async fn media_understanding_permission_defaults_false_and_preserves_existing_keys() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite");

        migrate_sqlite_range(&pool, 1, 10).await;
        sqlx::query(
            "INSERT INTO api_keys (id, token, name, allow_web_research) VALUES ('existing', 'sk-existing', 'Existing', 1)",
        )
        .execute(&pool)
        .await
        .expect("existing API key");

        migrate_sqlite_range(&pool, 11, 11).await;

        let permission = sqlx::query_scalar::<_, bool>(
            "SELECT allow_media_understanding FROM api_keys WHERE id = 'existing'",
        )
        .fetch_one(&pool)
        .await
        .expect("Media Understanding permission");
        assert!(!permission);
    }

    #[tokio::test]
    async fn advanced_capability_migration_preserves_injection_intent_and_invalidates_legacy_turns()
    {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite");
        migrate_sqlite_range(&pool, 1, 17).await;
        sqlx::query(
            "INSERT INTO api_keys (
                id, token, name, web_search_injection_enabled,
                allow_web_research, allow_media_understanding
             ) VALUES
                ('search', 'sk-search', 'Search', 1, 1, 0),
                ('media', 'sk-media', 'Media', 0, 0, 1),
                ('none', 'sk-none', 'None', 0, 1, 0)",
        )
        .execute(&pool)
        .await
        .expect("legacy API keys");
        sqlx::query(
            "INSERT INTO settings (name, value) VALUES (
                'web_research_config',
                '{\"revision\":7,\"enabled\":false,\"backend\":null,\"max_turns\":8,\"total_time_seconds\":240,\"updated_at\":\"\"}'
             )",
        )
        .execute(&pool)
        .await
        .expect("legacy Web Search config");
        sqlx::query(
            "INSERT INTO turn_chain_nodes (
                id, kind, parent_id, principal, payload_version, payload, created_at, expires_at
             ) VALUES
                ('response-turn', 'response', NULL, 'key', 1, '{}', 1, 100),
                ('research-turn', 'web_research', NULL, 'key', 1, '{}', 1, 100)",
        )
        .execute(&pool)
        .await
        .expect("legacy Turns");

        migrate_sqlite_range(&pool, 18, 18).await;

        let permissions = sqlx::query_as::<_, (String, bool, bool, bool)>(
            "SELECT id, transparent_injection_enabled, inject_media_understanding, inject_web_search
             FROM api_keys
             WHERE id IN ('search', 'media', 'none')
             ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("migrated injection permissions");
        assert_eq!(
            permissions,
            vec![
                ("media".into(), true, true, false),
                ("none".into(), false, false, false),
                ("search".into(), true, false, true),
            ]
        );
        let columns = sqlx::query_scalar::<_, String>(
            "SELECT name FROM pragma_table_info('api_keys') ORDER BY cid",
        )
        .fetch_all(&pool)
        .await
        .expect("API key columns");
        for removed in [
            "web_search_injection_enabled",
            "allow_web_research",
            "allow_media_understanding",
        ] {
            assert!(!columns.iter().any(|column| column == removed));
        }
        let turn_columns = sqlx::query_scalar::<_, String>(
            "SELECT name FROM pragma_table_info('turn_chain_nodes') ORDER BY cid",
        )
        .fetch_all(&pool)
        .await
        .expect("Turn columns");
        for preserved in [
            "prefix_namespace",
            "prefix_fingerprint",
            "prefix_item_count",
            "prefix_completed_at",
        ] {
            assert!(turn_columns.iter().any(|column| column == preserved));
        }
        let config = sqlx::query_scalar::<_, String>(
            "SELECT value FROM settings WHERE name = 'web_search_config'",
        )
        .fetch_one(&pool)
        .await
        .expect("migrated Web Search config");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&config).unwrap()["revision"],
            7
        );
        assert!(
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM settings WHERE name = 'web_research_config'",
            )
            .fetch_optional(&pool)
            .await
            .expect("legacy setting lookup")
            .is_none()
        );
        let turns = sqlx::query_as::<_, (String, String)>(
            "SELECT id, kind FROM turn_chain_nodes ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("migrated Turns");
        assert_eq!(turns, vec![("response-turn".into(), "response".into())]);
    }

    #[tokio::test]
    async fn media_derivative_mapping_is_write_once_and_artifact_backed() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite");
        migrate_sqlite_range(&pool, 1, 12).await;

        for (id, mime_type) in [("source", "image/png"), ("derivative", "image/jpeg")] {
            sqlx::query(
                "INSERT INTO artifacts (id, principal, mime_type, size, backend_key, state, expires_at, created_at) VALUES (?, 'api-key:key', ?, 1, ?, 'ready', 100, 1)",
            )
            .bind(id)
            .bind(mime_type)
            .bind(format!("objects/{id}"))
            .execute(&pool)
            .await
            .expect("Artifact");
        }

        sqlx::query(
            "INSERT INTO media_derivatives (principal, source_artifact_id, derivative_artifact_id, created_at) VALUES ('api-key:key', 'source', 'derivative', 1)",
        )
        .execute(&pool)
        .await
        .expect("first mapping");
        let duplicate = sqlx::query(
            "INSERT INTO media_derivatives (principal, source_artifact_id, derivative_artifact_id, created_at) VALUES ('api-key:key', 'source', 'derivative', 2)",
        )
        .execute(&pool)
        .await;
        assert!(duplicate.is_err());

        sqlx::query("DELETE FROM artifacts WHERE id = 'source'")
            .execute(&pool)
            .await
            .expect("source deletion");
        let mappings = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_derivatives")
            .fetch_one(&pool)
            .await
            .expect("mapping count");
        assert_eq!(mappings, 0);
    }

    #[tokio::test]
    async fn reusable_prefix_migration_removes_anonymous_history_without_backfill() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite");
        migrate_sqlite_range(&pool, 1, 14).await;

        sqlx::query(
            "INSERT INTO turn_chain_nodes
                (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at)
             VALUES
                ('anonymous-turn', 'response', NULL, 'anonymous', 1, '{}', 1, 100),
                ('authenticated-turn', 'response', NULL, 'api-key:owner', 1, '{}', 1, 100)",
        )
        .execute(&pool)
        .await
        .expect("legacy Turn nodes");
        sqlx::query(
            "INSERT INTO artifacts
                (id, principal, mime_type, size, backend_key, state, expires_at, created_at)
             VALUES
                ('anonymous-artifact', 'anonymous', 'image/png', 1, 'anonymous', 'ready', 100, 1),
                ('authenticated-artifact', 'api-key:owner', 'image/png', 1, 'owner', 'ready', 100, 1)",
        )
        .execute(&pool)
        .await
        .expect("legacy Artifacts");
        sqlx::query(
            "INSERT INTO artifact_uploads
                (id, artifact_id, principal, token_hash, declared_size, received_size, expires_at, created_at)
             VALUES
                ('anonymous-upload', 'anonymous-artifact', 'anonymous', 'upload-token', 1, 0, 100, 1)",
        )
        .execute(&pool)
        .await
        .expect("legacy upload");

        migrate_sqlite_range(&pool, 15, 15).await;

        for table in ["turn_chain_nodes", "artifacts", "artifact_uploads"] {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE principal = 'anonymous'"
            )))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|error| panic!("query {table}: {error}"));
            assert_eq!(count, 0, "{table} must not retain anonymous data");
        }
        let preserved = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<i64>)>(
            "SELECT id, prefix_namespace, prefix_fingerprint, prefix_item_count
             FROM turn_chain_nodes
             WHERE principal = 'api-key:owner'",
        )
        .fetch_one(&pool)
        .await
        .expect("authenticated Turn node");
        assert_eq!(
            preserved,
            ("authenticated-turn".into(), None, None, None),
            "the upgrade must not backfill old Turn nodes"
        );
        let authenticated_artifacts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE principal = 'api-key:owner'")
                .fetch_one(&pool)
                .await
                .expect("authenticated Artifact count");
        assert_eq!(authenticated_artifacts, 1);
    }
    #[tokio::test]
    async fn image_generation_removal_migration_drops_legacy_state() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite");
        migrate_sqlite_range(&pool, 1, 15).await;
        sqlx::query(
            "INSERT INTO providers (id, name, protocol, base_url, api_key)
             VALUES ('provider', 'Provider', 'openai', 'https://example.com', 'key')",
        )
        .execute(&pool)
        .await
        .expect("legacy Provider");
        sqlx::query(
            "INSERT INTO models (id, name, operation, target_provider, target_model)
             VALUES ('image-model', 'Image Model', 'image_generation', 'provider', 'gpt-image')",
        )
        .execute(&pool)
        .await
        .expect("legacy image model");
        sqlx::query(
            "INSERT INTO settings (name, value) VALUES ('default_image_route_id', 'image-model')",
        )
        .execute(&pool)
        .await
        .expect("legacy default image model");

        migrate_sqlite_range(&pool, 16, 16).await;

        let model_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM models WHERE id = 'image-model'")
                .fetch_one(&pool)
                .await
                .expect("model count");
        assert_eq!(model_count, 0);
        let setting_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM settings WHERE name = 'default_image_route_id'",
        )
        .fetch_one(&pool)
        .await
        .expect("setting count");
        assert_eq!(setting_count, 0);
        for (table, column) in [
            ("models", "operation"),
            ("api_keys", "image_rpm"),
            ("api_keys", "image_rpd"),
            ("api_keys", "allow_image_generation"),
            ("artifacts", "insecure_transport"),
        ] {
            let columns = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                "SELECT name FROM pragma_table_info('{table}')"
            )))
            .fetch_all(&pool)
            .await
            .unwrap_or_else(|error| panic!("columns for {table}: {error}"));
            assert!(!columns.iter().any(|value| value == column));
        }
        for table in [
            "artifact_delivery_tokens",
            "image_capability_drifts",
            "image_continuations",
            "image_generation_attempts",
            "image_generation_runs",
        ] {
            let exists: Option<String> = sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_optional(&pool)
            .await
            .unwrap_or_else(|error| panic!("table {table}: {error}"));
            assert!(exists.is_none(), "{table} must be removed");
        }
    }
    #[tokio::test]
    async fn principal_concurrency_migration_removes_windows_and_rejects_non_positive_limits() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite");
        migrate_sqlite_range(&pool, 1, 16).await;
        sqlx::query(
            "INSERT INTO api_keys (id, token, name, rpm, rpd, tpm, tpd)
             VALUES ('existing', 'sk-existing', 'Existing', 1, 2, 3, 4)",
        )
        .execute(&pool)
        .await
        .expect("legacy API key");

        migrate_sqlite_range(&pool, 17, 17).await;

        let columns =
            sqlx::query_scalar::<_, String>("SELECT name FROM pragma_table_info('api_keys')")
                .fetch_all(&pool)
                .await
                .expect("API key columns");
        for column in ["rpm", "rpd", "tpm", "tpd"] {
            assert!(!columns.iter().any(|value| value == column));
        }
        assert!(columns.iter().any(|value| value == "concurrency_limit"));
        let limit: Option<i32> =
            sqlx::query_scalar("SELECT concurrency_limit FROM api_keys WHERE id = 'existing'")
                .fetch_one(&pool)
                .await
                .expect("migrated concurrency limit");
        assert_eq!(limit, None);
        assert!(
            sqlx::query("UPDATE api_keys SET concurrency_limit = 0 WHERE id = 'existing'")
                .execute(&pool)
                .await
                .is_err(),
            "zero concurrency limit must violate the database constraint"
        );
    }

    #[test]
    fn principal_concurrency_migration_is_present_for_both_backends() {
        for migrator in [&SQLITE_MIGRATOR, &POSTGRES_MIGRATOR] {
            let migration = migrator
                .iter()
                .find(|migration| migration.version == 17)
                .expect("Principal Concurrency Limit migration");
            for clause in [
                "DROP COLUMN rpm",
                "DROP COLUMN rpd",
                "DROP COLUMN tpm",
                "DROP COLUMN tpd",
                "ADD COLUMN concurrency_limit",
            ] {
                assert!(
                    migration.sql.as_str().contains(clause),
                    "missing `{clause}`"
                );
            }
        }
    }

    #[tokio::test]
    async fn revisioned_catalog_migration_preserves_provider_configuration() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        migrate_sqlite_range(&pool, 1, 19).await;
        sqlx::query(
            "INSERT INTO providers (
                id, name, vendor, protocol, base_url, models_source, api_key, auth_mode
             ) VALUES (
                'anthropic-provider', 'Anthropic', 'anthropic', 'anthropic-messages',
                'https://api.anthropic.com', 'ai://models.dev/anthropic', 'secret', 'apikey'
             )",
        )
        .execute(&pool)
        .await
        .expect("legacy catalog provider");
        sqlx::query(
            "INSERT INTO providers (
                id, name, vendor, protocol, base_url, models_source, api_key, auth_mode
             ) VALUES (
                'unidentified-provider', 'Unidentified', 'not-a-provider', 'openai-compatible',
                'https://example.invalid', 'ai://models.dev/not-a-provider', 'secret', 'apikey'
             )",
        )
        .execute(&pool)
        .await
        .expect("unidentified legacy catalog provider");
        sqlx::query(
            "INSERT INTO provider_models (
                provider_id, model_id, source_kind, presence, selection_policy, metadata_json
             ) VALUES (
                'anthropic-provider', 'claude', 'discovered', 'present', 'auto',
                '{\"id\":\"claude\",\"name\":\"Claude\"}'
             )",
        )
        .execute(&pool)
        .await
        .expect("Provider Model snapshot");

        migrate_sqlite_range(&pool, 20, 20).await;

        let provider = sqlx::query_as::<_, (Option<String>, Option<String>, String, String)>(
            "SELECT preset_key, models_source, api_key, base_url
             FROM providers WHERE id = 'anthropic-provider'",
        )
        .fetch_one(&pool)
        .await
        .expect("migrated provider");
        assert_eq!(
            provider,
            (
                Some("anthropic".to_string()),
                Some("catalog".to_string()),
                "secret".to_string(),
                "https://api.anthropic.com".to_string(),
            )
        );
        let unidentified = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT preset_key, models_source FROM providers WHERE id = 'unidentified-provider'",
        )
        .fetch_one(&pool)
        .await
        .expect("unidentified legacy provider");
        assert_eq!(
            unidentified,
            (None, Some("ai://models.dev/not-a-provider".to_string()))
        );
        let metadata = sqlx::query_scalar::<_, String>(
            "SELECT metadata_json FROM provider_models
             WHERE provider_id = 'anthropic-provider' AND model_id = 'claude'",
        )
        .fetch_one(&pool)
        .await
        .expect("unchanged Provider Model snapshot");
        assert_eq!(metadata, r#"{"id":"claude","name":"Claude"}"#);
    }

    #[tokio::test]
    async fn route_target_migration_preserves_a_legacy_primary_target() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        migrate_sqlite_range(&pool, 1, 26).await;
        sqlx::query(
            "INSERT INTO providers (
                id, name, protocol, base_url, api_key, auth_mode
             ) VALUES (
                'provider-1', 'Provider 1', 'openai-compatible',
                'https://example.com', '', 'apikey'
             )",
        )
        .execute(&pool)
        .await
        .expect("legacy Provider");
        sqlx::query(
            "INSERT INTO models (
                id, name, balance, target_provider, target_model
             ) VALUES (
                'route-storage-id', 'ClientRoute', 'weighted', 'provider-1', 'upstream-model'
             )",
        )
        .execute(&pool)
        .await
        .expect("legacy Route");

        migrate_sqlite_range(&pool, 27, 27).await;

        let target = sqlx::query_as::<_, (String, String)>(
            "SELECT provider_id, model FROM model_backends WHERE model_id = 'route-storage-id'",
        )
        .fetch_one(&pool)
        .await
        .expect("migrated Target");
        assert_eq!(target, ("provider-1".into(), "upstream-model".into()));
        let columns = sqlx::query_scalar::<_, String>(
            "SELECT name FROM pragma_table_info('models') ORDER BY cid",
        )
        .fetch_all(&pool)
        .await
        .expect("Route columns");
        assert!(!columns.iter().any(|column| column == "target_provider"));
        assert!(!columns.iter().any(|column| column == "target_model"));
    }

    #[tokio::test]
    async fn web_access_adapter_migration_seeds_local_and_prunes_removed_providers() {
        for (legacy_ids, expected_remote_ids) in [
            (vec!["brave-source"], Vec::<&str>::new()),
            (vec!["exa-source"], vec!["exa-source"]),
            (vec!["brave-source", "exa-source"], vec!["exa-source"]),
        ] {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("SQLite pool");
            migrate_sqlite_range(&pool, 1, 27).await;
            sqlx::query(
                "INSERT INTO web_providers (id, name, kind, api_key)
                 VALUES
                    ('brave-source', 'Brave source', 'brave', 'brave-key'),
                    ('exa-source', 'Exa source', 'exa', 'exa-key')",
            )
            .execute(&pool)
            .await
            .expect("legacy Web Providers");
            let priority = serde_json::to_string(&legacy_ids).expect("legacy priority");
            for key in [
                "web_access_search_provider_ids",
                "web_access_fetch_provider_ids",
            ] {
                sqlx::query("INSERT INTO settings (name, value) VALUES (?, ?)")
                    .bind(key)
                    .bind(&priority)
                    .execute(&pool)
                    .await
                    .expect("legacy Web Access priority");
            }

            migrate_sqlite_range(&pool, 28, 28).await;

            let local = sqlx::query_as::<_, (String, bool, Option<String>, String)>(
                "SELECT id, use_proxy, api_key, local_engines
                 FROM web_providers WHERE kind = 'local'",
            )
            .fetch_one(&pool)
            .await
            .expect("seeded Local Web Provider");
            assert!(!local.1);
            assert!(local.2.is_none());
            let engines: serde_json::Value =
                serde_json::from_str(&local.3).expect("Local Search Engine config");
            for engine in ["google", "bing", "brave", "baidu"] {
                assert_eq!(engines[engine]["enabled"], true, "{engine}");
            }
            for engine in ["360", "sogou_weixin", "google_scholar"] {
                assert_eq!(engines[engine]["enabled"], false, "{engine}");
            }

            let kinds =
                sqlx::query_scalar::<_, String>("SELECT kind FROM web_providers ORDER BY kind")
                    .fetch_all(&pool)
                    .await
                    .expect("migrated Web Provider kinds");
            assert_eq!(kinds, ["exa", "local"]);

            for key in [
                "web_access_search_provider_ids",
                "web_access_fetch_provider_ids",
            ] {
                let value =
                    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = ?")
                        .bind(key)
                        .fetch_one(&pool)
                        .await
                        .expect("migrated Web Access priority");
                let ids = serde_json::from_str::<Vec<String>>(&value).expect("priority IDs");
                let expected = if expected_remote_ids.is_empty() {
                    vec![local.0.clone()]
                } else {
                    expected_remote_ids
                        .iter()
                        .map(|id| (*id).to_string())
                        .collect()
                };
                assert_eq!(ids, expected);
            }

            assert!(
                sqlx::query(
                    "INSERT INTO web_providers (id, name, kind, api_key)
                     VALUES ('removed', 'Removed', 'tavily', 'secret')",
                )
                .execute(&pool)
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn postgres_web_access_adapter_migration_matches_sqlite_when_configured() {
        let Some(url) = std::env::var("DB_URL")
            .ok()
            .or_else(|| std::env::var("DATABASE_URL").ok())
        else {
            return;
        };
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("PostgreSQL admin pool");

        for (legacy_ids, expected_ids) in [
            (vec!["brave-source"], vec!["web-provider-local"]),
            (vec!["exa-source"], vec!["exa-source"]),
            (vec!["brave-source", "exa-source"], vec!["exa-source"]),
        ] {
            let schema = format!(
                "stravia_web_access_migration_test_{}",
                uuid::Uuid::new_v4().simple()
            );
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&admin)
                .await
                .expect("create isolated PostgreSQL schema");
            let options: sqlx::postgres::PgConnectOptions =
                url.parse().expect("PostgreSQL connection options");
            let options = options.options([("search_path", schema.as_str())]);
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await
                .expect("isolated PostgreSQL pool");
            migrate_postgres_range(&pool, 1, 27).await;
            sqlx::query(
                "INSERT INTO web_providers (id, name, kind, api_key)
                 VALUES
                    ('brave-source', 'Brave source', 'brave', 'brave-key'),
                    ('exa-source', 'Exa source', 'exa', 'exa-key')",
            )
            .execute(&pool)
            .await
            .expect("legacy Web Providers");
            let priority = serde_json::to_string(&legacy_ids).expect("legacy priority");
            for key in [
                "web_access_search_provider_ids",
                "web_access_fetch_provider_ids",
            ] {
                sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2)")
                    .bind(key)
                    .bind(&priority)
                    .execute(&pool)
                    .await
                    .expect("legacy Web Access priority");
            }

            migrate_postgres_range(&pool, 28, 28).await;

            let local = sqlx::query_as::<_, (bool, Option<String>, serde_json::Value)>(
                "SELECT use_proxy, api_key, local_engines
                 FROM web_providers WHERE kind = 'local'",
            )
            .fetch_one(&pool)
            .await
            .expect("seeded Local Web Provider");
            assert!(!local.0);
            assert!(local.1.is_none());
            assert_eq!(local.2["google"]["enabled"], true);
            assert_eq!(local.2["google_scholar"]["enabled"], false);

            let kinds =
                sqlx::query_scalar::<_, String>("SELECT kind FROM web_providers ORDER BY kind")
                    .fetch_all(&pool)
                    .await
                    .expect("migrated Web Provider kinds");
            assert_eq!(kinds, ["exa", "local"]);
            for key in [
                "web_access_search_provider_ids",
                "web_access_fetch_provider_ids",
            ] {
                let value =
                    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE name = $1")
                        .bind(key)
                        .fetch_one(&pool)
                        .await
                        .expect("migrated Web Access priority");
                assert_eq!(
                    serde_json::from_str::<Vec<String>>(&value).expect("priority IDs"),
                    expected_ids
                );
            }
            assert!(
                sqlx::query(
                    "INSERT INTO web_providers (id, name, kind, api_key)
                     VALUES ('removed', 'Removed', 'tavily', 'secret')",
                )
                .execute(&pool)
                .await
                .is_err()
            );

            pool.close().await;
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                .execute(&admin)
                .await
                .expect("drop isolated PostgreSQL schema");
        }
    }
}
