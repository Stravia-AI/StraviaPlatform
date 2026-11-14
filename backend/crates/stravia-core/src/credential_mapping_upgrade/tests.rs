use futures::FutureExt;
use serde_json::json;
use sqlx::migrate::Migrator;

use crate::migrations::{
    migrate_postgres, migrate_sqlite, run_postgres_migrator, run_sqlite_migrator,
};

macro_rules! cleanup_fixture {
    ($name:ident, $pool:ty, $migrate:ident, $setup:literal, $trigger:literal, $drop:literal) => {
        async fn $name(pool: &$pool) -> anyhow::Result<()> {
            let now = chrono::Utc::now().timestamp_millis();
            sqlx::query("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at) VALUES('cleanup-interaction','alice','root','run','route','completed',$1,$1)")
                .bind(now).execute(pool).await?;
            sqlx::query("INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at) VALUES('run','cleanup-interaction','openai','route','completed',FALSE,$1,$1)")
                .bind(now).execute(pool).await?;
            sqlx::query("INSERT INTO credential_custom_rules(id,name,description,enabled,spec,created_at,updated_at) VALUES('custom.present','Fixture','',FALSE,$1,$2,$2)")
                .bind(json!({"mode":"simple","text":"IsolatedFixture9!"}).to_string())
                .bind(now).execute(pool).await?;
            sqlx::query("INSERT INTO settings(name,value) VALUES('reversible_redaction_disabled_rules',$1)")
                .bind(json!(["kingfisher.old", "removed-builtin", "generic-api-key"]).to_string())
                .execute(pool).await?;
            for principal in ["api-key:alice", "api-key:bob"] {
                let scope: i64 = sqlx::query_scalar("INSERT INTO content_scopes(principal) VALUES($1) RETURNING id")
                    .bind(principal).fetch_one(pool).await?;
                sqlx::query("INSERT INTO turn_nodes(id,scope_id,kind,payload_version,created_at) VALUES($1,$2,'response',6,$3)")
                    .bind(principal).bind(scope).bind(now).execute(pool).await?;
            }
            for (reference, principal, ids, pending) in [
                ("legacy-empty", "api-key:alice", json!([]), false),
                ("legacy-retired", "api-key:alice", json!(["kingfisher.old", "removed-builtin"]), false),
                ("legacy-custom", "api-key:bob", json!(["custom.deleted"]), false),
                ("legacy-pending", "api-key:bob", json!([]), true),
                ("current", "api-key:alice", json!(["generic-credential-uri"]), false),
                ("disabled", "api-key:alice", json!(["generic-api-key"]), false),
                ("custom", "api-key:bob", json!(["custom.present"]), false),
                ("mixed", "api-key:bob", json!(["kingfisher.old", "generic-api-key"]), false),
            ] {
                sqlx::query("INSERT INTO reversible_redaction_mappings(reference,principal,secret,published_at,pending_until,created_at,last_detected_at,rule_ids) VALUES($1,$2,$3,$4,$5,$6,$6,$7)")
                    .bind(reference).bind(principal).bind(format!("Fixture9!{reference}"))
                    .bind((!pending).then_some(now)).bind(pending.then_some(now + 3_600_000))
                    .bind(now).bind(ids.to_string()).execute(pool).await?;
                sqlx::query("INSERT INTO turn_node_redaction_mappings(node_pk,reference) SELECT node_pk,$1 FROM turn_nodes WHERE id=$2")
                    .bind(reference).bind(principal).execute(pool).await?;
                sqlx::query("INSERT INTO credential_mapping_reuses(marker,interaction_id,source_type,last_reused_at) VALUES($1,'cleanup-interaction','user_message',$2)")
                    .bind(reference).bind(now).execute(pool).await?;
            }
            // 覆盖真实历史扇出量级，删除不能因仍有大量引用而跳过。
            sqlx::query("WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<50000) INSERT INTO turn_nodes(id,scope_id,kind,payload_version,created_at) SELECT 'fanout-' || CAST(value AS TEXT),scope.id,'response',6,$1 FROM n CROSS JOIN content_scopes scope WHERE scope.principal='api-key:alice'")
                .bind(now).execute(pool).await?;
            sqlx::query("INSERT INTO turn_node_redaction_mappings(node_pk,reference) SELECT node_pk,'legacy-empty' FROM turn_nodes WHERE id LIKE 'fanout-%'")
                .execute(pool).await?;
            for reference in ["legacy-empty", "current"] {
                sqlx::query("INSERT INTO redaction_mapping_gc(reference) VALUES($1)")
                    .bind(reference).execute(pool).await?;
            }
            // 跨过元数据批次边界，且最近检出的旧映射也必须清理。
            sqlx::query("WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<1025) INSERT INTO reversible_redaction_mappings(reference,principal,secret,published_at,created_at,last_detected_at,rule_ids) SELECT 'batch-' || CAST(value AS TEXT),'api-key:alice','Fixture9!batch',$1,$1,$1,'[]' FROM n")
                .bind(now).execute(pool).await?;
            let retired = json!({"marker":"legacy-retired","rule_ids":["kingfisher.old"],"locators":[{"rule_id":"kingfisher.old","source_type":"system"}],"source_types":["system"]});
            let mixed = json!({"marker":"mixed","rule_ids":["kingfisher.old","generic-api-key"],"locators":[{"rule_id":"kingfisher.old","source_type":"system"},{"rule_id":"generic-api-key","source_type":"user_message"}],"source_types":["system","user_message"]});
            for (sequence, kind, discoveries) in [
                (10_i64, "credential_mappings_created", json!([retired.clone()])),
                (20, "credential_mappings_reused", json!([retired, mixed])),
                (30, "credential_mappings_created", json!([{"marker":"legacy-empty","rule_ids":[],"locators":[]}]))
            ] {
                let attrs = crate::storage_codec::encode(serde_json::to_vec(&json!({"discoveries":discoveries}))?.into())?;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,run_id,kind,attrs) VALUES($1,$2,'cleanup-interaction','run',$3,$4)")
                    .bind(sequence).bind(now).bind(kind).bind(attrs).execute(pool).await?;
            }
            sqlx::query($setup).execute(pool).await?;
            sqlx::query($trigger).execute(pool).await?;
            assert!($migrate(pool, None).await.is_err());
            let finalized: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=55 AND success)")
                .fetch_one(pool).await?;
            assert!(!finalized);
            // SQL 迁移删除失败时，解除引用与复用登记也必须整体回滚。
            let referenced: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_node_redaction_mappings WHERE reference LIKE 'legacy-%'")
                .fetch_one(pool).await?;
            assert_eq!(referenced, 50_004);
            let reused: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credential_mapping_reuses WHERE marker LIKE 'legacy-%'")
                .fetch_one(pool).await?;
            assert_eq!(reused, 4);
            sqlx::query($drop).execute(pool).await?;
            let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = progress.clone();
            let started = std::time::Instant::now();
            crate::startup_progress::observe_startup(move |event| observed.lock().unwrap().push(event), $migrate(pool, None)).await?;
            eprintln!("{} legacy cleanup: 1,033 mappings, 50,008 references, 3 events; resumed upgrade {:?}", stringify!($pool), started.elapsed());
            {
                let events = progress.lock().unwrap();
                let cleanup: Vec<_> = events.iter().filter(|event| event.phase == "credential_cleanup").collect();
                assert!(cleanup.first().is_some_and(|event| event.total.is_none()));
                assert!(cleanup.last().is_some_and(|event| event.total == Some(event.completed)));
                assert!(cleanup.windows(2).all(|pair| pair[0].completed <= pair[1].completed));
            }
            let mappings: Vec<(String, String)> = sqlx::query_as("SELECT reference,rule_ids FROM reversible_redaction_mappings ORDER BY reference")
                .fetch_all(pool).await?;
            assert_eq!(mappings.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(), ["current", "custom", "disabled", "mixed"]);
            let mixed_ids: serde_json::Value = serde_json::from_str(&mappings.iter().find(|row| row.0 == "mixed").unwrap().1)?;
            assert_eq!(mixed_ids, json!(["generic-api-key"]));
            let references: Vec<String> = sqlx::query_scalar("SELECT reference FROM turn_node_redaction_mappings ORDER BY reference")
                .fetch_all(pool).await?;
            assert_eq!(references, ["current", "custom", "disabled", "mixed"]);
            let reuses: Vec<String> = sqlx::query_scalar("SELECT marker FROM credential_mapping_reuses ORDER BY marker")
                .fetch_all(pool).await?;
            assert_eq!(reuses, references);
            let queued: Vec<String> = sqlx::query_scalar("SELECT reference FROM redaction_mapping_gc ORDER BY reference")
                .fetch_all(pool).await?;
            assert_eq!(queued, ["current"]);
            let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as("SELECT sequence,attrs FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused') ORDER BY sequence")
                .fetch_all(pool).await?;
            assert_eq!(rows.iter().map(|row| row.0).collect::<Vec<_>>(), [20]);
            let payload: serde_json::Value = serde_json::from_slice(&crate::storage_codec::decode(&rows[0].1)?)?;
            assert_eq!(payload, json!({"discoveries":[{"marker":"mixed","rule_ids":["generic-api-key"],"locators":[{"rule_id":"generic-api-key","source_type":"user_message"}],"source_types":["user_message"]}]}));
            let disabled: String = sqlx::query_scalar("SELECT value FROM settings WHERE name='reversible_redaction_disabled_rules'")
                .fetch_one(pool).await?;
            assert_eq!(serde_json::from_str::<serde_json::Value>(&disabled)?, json!(["generic-api-key"]));
            let custom_enabled: bool = sqlx::query_scalar("SELECT enabled FROM credential_custom_rules WHERE id='custom.present'")
                .fetch_one(pool).await?;
            assert!(!custom_enabled);
            let history: Vec<(i64, Vec<u8>)> = sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(pool).await?;
            assert!(history.iter().any(|row| row.0 == 55));
            let restart = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = restart.clone();
            crate::startup_progress::observe_startup(move |event| observed.lock().unwrap().push(event), $migrate(pool, None)).await?;
            assert!(!restart.lock().unwrap().iter().any(|event| event.phase == "credential_cleanup"));
            let after_history: Vec<(i64, Vec<u8>)> = sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(pool).await?;
            assert_eq!(history, after_history);
            let after: Vec<(String, String)> = sqlx::query_as("SELECT reference,rule_ids FROM reversible_redaction_mappings ORDER BY reference")
                .fetch_all(pool).await?;
            assert_eq!(mappings, after);
            Ok(())
        }
    };
}

cleanup_fixture!(
    sqlite_cleanup,
    sqlx::SqlitePool,
    migrate_sqlite,
    "SELECT 1",
    "CREATE TRIGGER fail_legacy_delete BEFORE DELETE ON reversible_redaction_mappings WHEN OLD.reference='legacy-empty' BEGIN SELECT RAISE(ABORT,'interrupted legacy purge'); END",
    "DROP TRIGGER fail_legacy_delete"
);
cleanup_fixture!(
    postgres_cleanup,
    sqlx::PgPool,
    migrate_postgres,
    "CREATE FUNCTION fail_legacy_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.reference='legacy-empty' THEN RAISE EXCEPTION 'interrupted legacy purge'; END IF; RETURN OLD; END $$",
    "CREATE TRIGGER fail_legacy_delete BEFORE DELETE ON reversible_redaction_mappings FOR EACH ROW EXECUTE FUNCTION fail_legacy_delete()",
    "DROP TRIGGER fail_legacy_delete ON reversible_redaction_mappings"
);

#[tokio::test]
async fn sqlite_legacy_cleanup_deletes_referenced_mappings_and_resumes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(directory.path().join("upgrade.db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let all = sqlx::migrate!("./migrations/sqlite");
    let prefix = Migrator::with_migrations(
        all.iter()
            .filter(|migration| migration.version <= 54)
            .cloned()
            .collect(),
    );
    run_sqlite_migrator(&pool, &prefix, None).await?;
    let result = sqlite_cleanup(&pool).await;
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await?;
    assert!(violations.is_empty());
    pool.close().await;
    result
}

#[tokio::test]
async fn postgres_legacy_cleanup_deletes_referenced_mappings_and_resumes_when_configured()
-> anyhow::Result<()> {
    let Ok(url) = std::env::var("STRAVIA_TEST_POSTGRES_URL") else {
        return Ok(());
    };
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let control_pool = sqlx::PgPool::connect(&url).await?;
    let schema = format!("legacy_cleanup_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&control_pool)
        .await?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await;
    let result = match pool {
        Ok(pool) => {
            let result = std::panic::AssertUnwindSafe(async {
                let all = sqlx::migrate!("./migrations/postgres");
                let prefix = Migrator::with_migrations(
                    all.iter()
                        .filter(|migration| migration.version <= 54)
                        .cloned()
                        .collect(),
                );
                run_postgres_migrator(&pool, &prefix, None).await?;
                postgres_cleanup(&pool).await
            })
            .catch_unwind()
            .await;
            pool.close().await;
            result
        }
        Err(error) => Ok(Err(error.into())),
    };
    let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&control_pool)
        .await;
    control_pool.close().await;
    match result {
        Ok(result) => result?,
        Err(panic) => {
            cleanup?;
            std::panic::resume_unwind(panic);
        }
    }
    cleanup?;
    Ok(())
}
