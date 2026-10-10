use anyhow::{Context, ensure};
use reqwest::StatusCode;
use sqlx::{Connection, postgres::PgPoolOptions};
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use stravia_core::Gateway;
use stravia_core::admin::identity::AdminAuth;
use stravia_core::config::{GatewayConfig, SqlStorageConfig, StorageBackendKind};
use stravia_core::db::models::{
    CreateApiKey, CreateProvider, CreateRoute, CreateTarget, ProviderCredentialInput,
    ProviderSourceInput, PutRoute, UpdateRoute,
};
use stravia_core::provider_models::CreateManualProviderModel;
use stravia_server::{
    AdminMode, HttpAppConfig, build_http_app, standalone_local_origins, start_http_server,
};

async fn verify_outbound_proxy_migration(pool: &sqlx::PgPool, schema: &str) -> anyhow::Result<()> {
    let mut connection = pool.acquire().await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
        .execute(&mut *connection)
        .await?;
    let directory = PathBuf::from("backend/crates/stravia-core/migrations/postgres");
    let mut files = std::fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();
    for file in files.iter().filter(|path| {
        path.extension().is_some_and(|extension| extension == "sql")
            && path.file_name().unwrap().to_string_lossy().as_ref() < "0016"
    }) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(file)?))
            .execute(&mut *connection)
            .await?;
    }
    let migration = std::fs::read_to_string(directory.join("0016_outbound_proxy_settings.sql"))?;
    let legacy_config = serde_json::json!({
        "url": "socks5h://user:password@127.0.0.1:1080",
        "bypass": "localhost, .example.test, 127.0.0.1",
        "force_http1": true
    });
    let saved_config = serde_json::json!({
        "url": "https://127.0.0.1:8443", "bypass": "saved.test", "force_http1": false
    });
    // Each transaction starts from the same complete pre-cutover schema and rolls back
    // both its fixture and the actual migration, including all legacy-key deletions.
    for (case, global, legacy, saved, expected_enabled) in [
        ("disabled", Some("false"), true, false, false),
        ("enabled", Some("true"), true, false, true),
        ("one", Some(" 1\t"), true, false, true),
        ("yes", Some("\nYeS\r"), true, false, true),
        ("on", Some(" ON "), true, false, true),
        ("trimmed_true", Some("\tTrUe\n"), true, false, true),
        ("nbsp_true", Some("\u{00a0}true\u{00a0}"), true, false, true),
        (
            "mixed_unicode_true",
            Some("\u{3000}\u{2003}TrUe\u{2003}\u{3000}"),
            true,
            false,
            true,
        ),
        ("missing_global", None, true, false, false),
        ("missing_all_legacy", None, false, false, false),
        ("saved_config", Some("true"), true, true, true),
        ("new_only", None, false, true, true),
        ("saved_config_disabled", Some("false"), true, true, false),
        ("fresh", None, false, false, true),
    ] {
        let mut transaction = connection.begin().await?;
        // Old rows without any proxy settings used the missing global's false gate;
        // fresh user Providers are created only after migrations finish.
        if case != "fresh" {
            for (id, use_proxy) in [("migration-proxied", true), ("migration-direct", false)] {
                sqlx::query("INSERT INTO providers (id, name, protocol, base_url, api_key, use_proxy) VALUES ($1, $1, 'openai', 'http://127.0.0.1:8080', 'fixture-key', $2)")
                    .bind(id).bind(use_proxy).execute(&mut *transaction).await?;
            }
        }
        sqlx::query("INSERT INTO web_providers (id, name, kind, api_key, use_proxy) VALUES ('migration-web-proxied', 'Migration web proxied', 'exa', 'fixture-key', true), ('migration-web-direct', 'Migration web direct', 'exa', 'fixture-key', false)")
            .execute(&mut *transaction).await?;
        if legacy {
            for (name, value) in [
                ("proxy_url", legacy_config["url"].as_str().unwrap()),
                ("proxy_bypass", legacy_config["bypass"].as_str().unwrap()),
                (
                    "proxy_force_http1",
                    match case {
                        "nbsp_true" | "mixed_unicode_true" => global.unwrap(),
                        _ => "\tYeS\n",
                    },
                ),
            ] {
                sqlx::query("INSERT INTO settings (name, value) VALUES ($1, $2)")
                    .bind(name)
                    .bind(value)
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        if let Some(value) = global {
            sqlx::query("INSERT INTO settings (name, value) VALUES ('proxy_enabled', $1)")
                .bind(value)
                .execute(&mut *transaction)
                .await?;
        }
        if saved {
            sqlx::query("INSERT INTO settings (name, value) VALUES ('outbound_proxy', $1), ('update_use_proxy', 'false')")
                .bind(saved_config.to_string()).execute(&mut *transaction).await?;
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(migration.clone()))
            .execute(&mut *transaction)
            .await?;
        if case == "fresh" {
            for (id, use_proxy) in [("migration-proxied", true), ("migration-direct", false)] {
                sqlx::query("INSERT INTO providers (id, name, protocol, base_url, api_key, use_proxy) VALUES ($1, $1, 'openai', 'http://127.0.0.1:8080', 'fixture-key', $2)")
                    .bind(id).bind(use_proxy).execute(&mut *transaction).await?;
            }
        }
        let providers: Vec<(String, bool)> = sqlx::query_as("SELECT id, use_proxy FROM providers WHERE id IN ('migration-direct', 'migration-proxied') ORDER BY id")
            .fetch_all(&mut *transaction).await?;
        ensure!(
            providers
                == vec![
                    ("migration-direct".into(), false),
                    ("migration-proxied".into(), expected_enabled)
                ],
            "{case}: model proxy flags changed incorrectly"
        );
        let web: Vec<(String, bool)> =
            sqlx::query_as("SELECT id, use_proxy FROM web_providers ORDER BY id")
                .fetch_all(&mut *transaction)
                .await?;
        ensure!(
            web == vec![
                ("migration-web-direct".into(), false),
                ("migration-web-proxied".into(), true),
                ("web-provider-local".into(), false)
            ],
            "{case}: Web proxy flags changed"
        );
        let value: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE name = 'outbound_proxy'")
                .fetch_one(&mut *transaction)
                .await?;
        let expected_config = if saved {
            saved_config.clone()
        } else if legacy {
            legacy_config.clone()
        } else {
            serde_json::json!({"url": "", "bypass": "", "force_http1": false})
        };
        ensure!(
            serde_json::from_str::<serde_json::Value>(&value)? == expected_config,
            "{case}: proxy configuration was not preserved"
        );
        let update: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE name = 'update_use_proxy'")
                .fetch_one(&mut *transaction)
                .await?;
        let expected_update = if !saved && global.is_some() && expected_enabled {
            "true"
        } else {
            "false"
        };
        ensure!(
            update == expected_update,
            "{case}: update preference changed incorrectly"
        );
        let old_keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM settings WHERE name IN ('proxy_enabled', 'proxy_url', 'proxy_bypass', 'proxy_force_http1')")
            .fetch_one(&mut *transaction).await?;
        ensure!(old_keys == 0, "{case}: legacy proxy keys remain");
        transaction.rollback().await?;
        println!("outbound_proxy_migration_{case}=true");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Ok(action) = env::var("STRAVIA_STORAGE_SCHEMA_ACTION") {
        let pg_url = env::var("STRAVIA_STORAGE_PG_URL").context("STRAVIA_STORAGE_PG_URL")?;
        let schema = env::var("STRAVIA_STORAGE_PG_SCHEMA").context("STRAVIA_STORAGE_PG_SCHEMA")?;
        ensure!(
            schema
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "invalid schema: {schema}"
        );
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&pg_url)
            .await?;
        match action.as_str() {
            "create" => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                    .execute(&pool)
                    .await?;
            }
            "drop" => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                    .execute(&pool)
                    .await?;
            }
            "verify_rpm_migration" => {
                let mut connection = pool.acquire().await?;
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
                    .execute(&mut *connection)
                    .await?;
                let directory = PathBuf::from("backend/crates/stravia-core/migrations/postgres");
                let mut files = std::fs::read_dir(&directory)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()?;
                files.sort();
                // Replay only migrations before 0009 to model the pre-RPM database.
                // Later migrations depend on the schema produced by 0009.
                for file in files.iter().take_while(|path| {
                    !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("0009")
                }) {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(file)?))
                        .execute(&mut *connection)
                        .await?;
                }
                sqlx::query("INSERT INTO api_keys (id, token, name, concurrency_limit) VALUES ('old', 'old-token', 'Old key', 5)")
                    .execute(&mut *connection).await?;
                sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(
                    directory.join("0009_rpm_admission.sql"),
                )?))
                .execute(&mut *connection)
                .await?;
                let rpm: Option<i32> =
                    sqlx::query_scalar("SELECT rpm_limit FROM api_keys WHERE id = 'old'")
                        .fetch_one(&mut *connection)
                        .await?;
                ensure!(rpm.is_none(), "old concurrency must not become RPM");
                let old_columns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = $1 AND table_name = 'api_keys' AND column_name = 'concurrency_limit'")
                    .bind(&schema).fetch_one(&mut *connection).await?;
                ensure!(old_columns == 0, "old concurrency column remains");
                println!("rpm_migration_clears_old_limit=true");
                for file in files.iter().filter(|path| {
                    let name = path.file_name().unwrap().to_string_lossy();
                    name.as_ref() >= "0010" && name.as_ref() < "0013"
                }) {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(file)?))
                        .execute(&mut *connection)
                        .await?;
                }
                let config = serde_json::json!({
                    "preferred_wait_ms": 987, "total_wait_ms": 1234, "queue_capacity": 7,
                    "pools": [
                        {"id": "limited", "name": "Limited", "rpm_limit": 9},
                        {"id": "unlimited", "name": "Unlimited", "rpm_limit": null}
                    ],
                    "destinations": [
                        {"provider_id": "p", "model": "a", "rpm_limit": null, "rpm_pool_id": "limited"},
                        {"provider_id": "q", "model": "b", "rpm_limit": null, "rpm_pool_id": "limited"},
                        {"provider_id": "p", "model": "c", "rpm_limit": null, "rpm_pool_id": "unlimited"},
                        {"provider_id": "p", "model": "d", "rpm_limit": 3, "rpm_pool_id": null},
                        {"provider_id": "p", "model": "e", "rpm_limit": 5}
                    ]
                });
                sqlx::query("INSERT INTO settings (name, value) VALUES ('rpm_admission', $1)")
                    .bind(config.to_string())
                    .execute(&mut *connection)
                    .await?;
                sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(
                    directory.join("0014_remove_shared_rpm_pools.sql"),
                )?))
                .execute(&mut *connection)
                .await?;
                let value: String =
                    sqlx::query_scalar("SELECT value FROM settings WHERE name = 'rpm_admission'")
                        .fetch_one(&mut *connection)
                        .await?;
                let expected = serde_json::json!({
                    "preferred_wait_ms": 987, "total_wait_ms": 1234, "queue_capacity": 7,
                    "destinations": [
                        {"provider_id": "p", "model": "a", "rpm_limit": null},
                        {"provider_id": "q", "model": "b", "rpm_limit": null},
                        {"provider_id": "p", "model": "c", "rpm_limit": null},
                        {"provider_id": "p", "model": "d", "rpm_limit": 3},
                        {"provider_id": "p", "model": "e", "rpm_limit": 5}
                    ]
                });
                ensure!(
                    serde_json::from_str::<serde_json::Value>(&value)? == expected,
                    "shared RPM removal changed destination limits or other settings"
                );
                println!("rpm_migration_preserves_destination_limits=true");
            }
            "verify_outbound_proxy_migration" => {
                verify_outbound_proxy_migration(&pool, &schema).await?;
            }
            "inspect_observation" => {
                let tables: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name IN ('interaction_observations', 'inference_run_observations', 'model_turn_observations', 'target_attempt_observations', 'observation_events', 'rejected_request_observations')",
                )
                .bind(&schema)
                .fetch_one(&pool)
                .await?;
                // Debug trace manifests live in <data_dir>/diagnostics/
                // observation-debug/<trace_id>/manifest.json; the table and
                // the unused event-expiry index must not come back.
                let removed: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name IN ('debug_trace_manifests')",
                )
                .bind(&schema)
                .fetch_one(&pool)
                .await?;
                let legacy: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1 AND table_name = 'request_logs'",
                )
                .bind(&schema)
                .fetch_one(&pool)
                .await?;
                let indexes: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = $1 AND indexname IN ('interaction_observations_window_idx', 'model_turns_analytics_idx', 'target_attempts_analytics_idx')",
                )
                .bind(&schema)
                .fetch_one(&pool)
                .await?;
                let removed_indexes: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_indexes WHERE schemaname = $1 AND indexname IN ('observation_events_expiry_idx')",
                )
                .bind(&schema)
                .fetch_one(&pool)
                .await?;
                println!("observation_tables={tables}");
                println!("removed_tables={removed}");
                println!("legacy_tables={legacy}");
                println!("observation_indexes={indexes}");
                println!("removed_indexes={removed_indexes}");
            }
            other => anyhow::bail!("unknown schema action: {other}"),
        }
        pool.close().await;
        return Ok(());
    }

    let backend = env::var("STRAVIA_STORAGE_BACKEND").context("STRAVIA_STORAGE_BACKEND")?;
    let upstream = env::var("STRAVIA_STORAGE_UPSTREAM").context("STRAVIA_STORAGE_UPSTREAM")?;
    let data_dir =
        PathBuf::from(env::var("STRAVIA_STORAGE_DATA_DIR").context("STRAVIA_STORAGE_DATA_DIR")?);
    let server_port: u16 = env::var("STRAVIA_STORAGE_SERVER_PORT")
        .context("STRAVIA_STORAGE_SERVER_PORT")?
        .parse()
        .context("invalid STRAVIA_STORAGE_SERVER_PORT")?;

    let mut config = GatewayConfig {
        data_dir,
        ..Default::default()
    };

    match backend.as_str() {
        "sqlite" => {
            config.storage.backend = StorageBackendKind::Sqlite;
        }
        "postgres" => {
            let pg_url = env::var("STRAVIA_STORAGE_PG_URL").context("STRAVIA_STORAGE_PG_URL")?;
            config.cache.redis_url = Some(
                env::var("STRAVIA_TEST_REDIS_URL")
                    .context("PostgreSQL storage harness requires STRAVIA_TEST_REDIS_URL")?,
            );
            let schema =
                env::var("STRAVIA_STORAGE_PG_SCHEMA").context("STRAVIA_STORAGE_PG_SCHEMA")?;
            ensure!(
                schema
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "invalid schema: {schema}"
            );
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&pg_url)
                .await?;
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA IF NOT EXISTS {schema}"
            )))
            .execute(&pool)
            .await?;
            pool.close().await;

            let url_with_schema = if pg_url.contains('?') {
                format!("{pg_url}&options=-csearch_path%3D{schema}")
            } else {
                format!("{pg_url}?options=-csearch_path%3D{schema}")
            };
            config.storage.backend = StorageBackendKind::Postgres;
            config.storage.postgres = SqlStorageConfig {
                url: Some(url_with_schema),
                ..Default::default()
            };
        }
        other => anyhow::bail!("unknown backend: {other}"),
    }

    let gw = Gateway::new(config).await?;
    let admin = gw.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: Some(format!("{backend}-e2e-provider")),
            source: ProviderSourceInput::Custom {
                vendor: "custom".to_string(),
                channel: "default".to_string(),
                protocol: Some("openai-compatible".to_string()),
                base_url: format!("{upstream}/v1"),
                models_source: None,
                static_models: None,
            },
            credential: ProviderCredentialInput::ApiKey {
                value: "dummy".to_string(),
            },
            vendor_options: Default::default(),
            use_proxy: false,
        })
        .await?;
    admin
        .create_manual_provider_model(
            &provider.id,
            "gpt-4o-mini",
            CreateManualProviderModel {
                template_id: None,
                metadata: serde_json::json!({
                    "id": "gpt-4o-mini",
                    "name": "GPT-4o mini",
                }),
            },
        )
        .await?;

    let route = admin
        .create_model(CreateRoute {
            model_id: format!("{backend}-model"),
            display_name: Some(format!("{backend} Model")),
            balance: None,
            targets: vec![CreateTarget {
                provider_id: provider.id.clone(),
                model: "gpt-4o-mini".to_string(),
                enabled: true,
                priority: None,
                first_token_timeout_ms: None,
                target_retry_budget: None,
                target_cooldown_ms: None,
                thinking_level_map: Vec::new(),
            }],
            default_thinking_level: None,
        })
        .await?;

    let api_key = admin
        .create_api_key(CreateApiKey {
            key: None,
            name: format!("{backend}-e2e-key"),
            rpm_limit: Some(10),
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_media_understanding: false,
            inject_web_search: false,
            inject_media_generation: false,
            expires_at: None,
            model_ids: vec![route.id.clone().into()],
        })
        .await?;

    ensure!(
        !api_key.inject_media_generation,
        "new keys must not auto-inject media generation"
    );
    admin
        .update_api_key(
            &api_key.id,
            serde_json::from_value(serde_json::json!({
                "inject_media_generation": true
            }))?,
        )
        .await?;
    let reread_key = admin.get_api_key(&api_key.id).await?;
    ensure!(
        reread_key.inject_media_generation,
        "media generation injection selection persists independently"
    );
    ensure!(
        reread_key.rpm_limit == Some(10),
        "RPM survives omitted updates"
    );
    admin
        .update_api_key(
            &api_key.id,
            serde_json::from_value(serde_json::json!({
                "rpm_limit": null
            }))?,
        )
        .await?;
    ensure!(
        admin.get_api_key(&api_key.id).await?.rpm_limit.is_none(),
        "explicit null clears RPM"
    );
    for invalid in [0, -1] {
        ensure!(
            admin
                .update_api_key(
                    &api_key.id,
                    serde_json::from_value(serde_json::json!({
                        "rpm_limit": invalid
                    }))?
                )
                .await
                .is_err(),
            "nonpositive RPM rejected"
        );
    }
    admin
        .update_api_key(
            &api_key.id,
            serde_json::from_value(serde_json::json!({
                "rpm_limit": 1
            }))?,
        )
        .await?;
    ensure!(
        admin.get_api_key(&api_key.id).await?.rpm_limit == Some(1),
        "positive RPM persists"
    );

    ensure!(admin.list_providers().await?.len() == 1, "provider count");
    let routes = admin.list_models().await?;
    ensure!(routes.len() == 1, "Route count");
    ensure!(
        routes[0].model_id == format!("{backend}-model"),
        "Route Model ID"
    );
    ensure!(
        routes[0].display_name.as_deref() == Some(format!("{backend} Model").as_str()),
        "Route display name"
    );
    ensure!(routes[0].targets.len() == 1, "Route aggregate Target count");
    ensure!(
        routes[0].targets[0].provider_id().as_str() == provider.id,
        "Route aggregate Provider"
    );
    let updated = admin
        .update_model(
            &route.model_id,
            UpdateRoute {
                display_name: Some(Some(format!("{backend} Renamed Model"))),
                ..Default::default()
            },
        )
        .await?;
    ensure!(
        updated.id == route.id,
        "display-name update preserves Route identity"
    );
    ensure!(
        updated.model_id == route.model_id,
        "display-name update preserves Model ID"
    );
    ensure!(
        updated.display_name.as_deref() == Some(format!("{backend} Renamed Model").as_str()),
        "updated Route display name"
    );
    let failed_put = gw
        .storage
        .routes()
        .put(PutRoute {
            id: Some(route.id.clone()),
            model_id: route.model_id.clone(),
            display_name: updated.display_name.clone(),
            selection_strategy: "latency_preference".to_string(),
            is_enabled: false,
            default_thinking_level: None,
            targets: Some(vec![CreateTarget {
                provider_id: "missing-provider".to_string(),
                model: "missing-model".to_string(),
                enabled: true,
                priority: Some(1),
                first_token_timeout_ms: Some(60_000),
                target_retry_budget: Some(5),
                target_cooldown_ms: Some(120_000),
                thinking_level_map: vec![],
            }]),
        })
        .await;
    ensure!(failed_put.is_err(), "invalid Route aggregate put must fail");
    let preserved = gw
        .storage
        .routes()
        .get(&route.model_id)
        .await?
        .context("preserved Route")?;
    ensure!(
        preserved.balance == route.balance,
        "failed put preserves Route strategy"
    );
    ensure!(
        preserved.is_enabled == route.is_enabled,
        "failed put preserves Route state"
    );
    ensure!(
        preserved.targets.len() == 1,
        "failed put preserves Target count"
    );
    ensure!(
        preserved.targets[0].provider_id().as_str() == provider.id,
        "failed put preserves Target"
    );
    ensure!(admin.list_api_keys().await?.len() == 1, "api key count");

    let admin_auth = AdminAuth::new(gw.storage.clone());
    admin_auth.ensure_native_admin().await?;
    let native_session = admin_auth.login_native().await?;
    let cors_origins = standalone_local_origins(server_port);
    let app = build_http_app(
        gw,
        HttpAppConfig {
            admin_auth,
            admin_mode: AdminMode::Desktop,
            admin_entry: Default::default(),
            desktop_cors_origins: cors_origins.clone(),
            proxy_cors_origins: cors_origins,
            serve_embedded_webui: false,
        },
    );
    let server = start_http_server(format!("127.0.0.1:{server_port}"), app).await?;

    let client = reqwest::Client::new();
    let admin_status = client
        .get(format!("http://127.0.0.1:{server_port}/api/v1/status"))
        .bearer_auth(&native_session.access_token)
        .send()
        .await?;
    ensure!(
        admin_status.status() == StatusCode::OK,
        "native admin bearer should 200"
    );
    let url = format!("http://127.0.0.1:{server_port}/v1/chat/completions");
    let payload = serde_json::json!({"model": format!("{backend}-model"), "messages": [{"role":"user","content":"hi"}]});

    let no_key = client.post(&url).json(&payload).send().await?;
    ensure!(
        no_key.status() == StatusCode::UNAUTHORIZED,
        "missing key should 401"
    );

    let ok = client
        .post(&url)
        .bearer_auth(&api_key.token)
        .json(&payload)
        .send()
        .await?;
    ensure!(ok.status() == StatusCode::OK, "valid key should 200");
    let body: serde_json::Value = ok.json().await?;
    ensure!(
        body["choices"][0]["message"]["content"].as_str() == Some("ok"),
        "content mismatch"
    );
    let rejected = client
        .post(&url)
        .bearer_auth(&api_key.token)
        .json(&payload)
        .send()
        .await?;
    ensure!(
        rejected.status() == StatusCode::TOO_MANY_REQUESTS,
        "same backend API key enforces RPM"
    );
    let retry_after = rejected
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    ensure!(
        retry_after.is_some_and(|seconds| seconds > 0 && seconds <= 60),
        "RPM rejection includes conservative Retry-After"
    );
    admin
        .update_api_key(
            &api_key.id,
            serde_json::from_value(serde_json::json!({
                "rpm_limit": null
            }))?,
        )
        .await?;

    let mut observation_roots = 0i64;
    let mut stats_requests = 0i64;
    for _ in 0..20 {
        let forest = client
            .get(format!(
                "http://127.0.0.1:{server_port}/api/v1/observations/interactions?limit=10"
            ))
            .bearer_auth(&native_session.access_token)
            .send()
            .await?;
        ensure!(
            forest.status() == StatusCode::OK,
            "Observation forest should 200"
        );
        let forest: serde_json::Value = forest.json().await?;
        observation_roots = forest["data"]["root_total"].as_i64().unwrap_or(0);
        let stats = admin.get_stats_overview(None).await?;
        stats_requests = stats.total_requests;
        if observation_roots >= 1 && stats_requests >= 1 {
            println!("backend={backend}");
            println!("observation_roots={observation_roots}");
            println!("stats_total_requests={stats_requests}");
            println!("proxy_status_ok=200");
            println!("proxy_status_no_key=401");
            admin.delete_provider(&provider.id).await?;
            ensure!(
                admin.list_models().await?.is_empty(),
                "Provider delete removes empty Route"
            );
            server.shutdown().await?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    server.shutdown().await?;
    anyhow::bail!("observation/stat timeout: roots={observation_roots} requests={stats_requests}");
}
