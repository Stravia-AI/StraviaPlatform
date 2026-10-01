use std::sync::Arc;

use serde_json::json;
use stravia_credential_protection::{CustomRuleError, CustomRuleInput, SqlCustomRuleStore};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiRequest;

use crate::config::GatewayConfig;
use crate::gateway::Gateway;

fn input(value: serde_json::Value) -> CustomRuleInput {
    serde_json::from_value(value).unwrap()
}

fn simple(name: &str, text: &str) -> CustomRuleInput {
    input(json!({ "name": name, "spec": { "mode": "simple", "text": text } }))
}

fn pattern(regex: &str, group: usize) -> CustomRuleInput {
    input(json!({
        "name": "Pattern",
        "spec": { "mode": "pattern", "regex": regex, "secret_group": group, "keywords": ["acme"] },
    }))
}

fn invalid(result: Result<impl Sized, CustomRuleError>) -> (&'static str, &'static str) {
    match result {
        Err(CustomRuleError::Invalid { field, reason }) => (field, reason),
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("input was accepted"),
    }
}

async fn assert_store_contract(store: &SqlCustomRuleStore) {
    assert!(store.list().await.unwrap().is_empty());

    assert_eq!(
        invalid(store.create(simple("  ", "abc")).await),
        ("name", "required")
    );
    assert_eq!(
        invalid(store.create(simple("n", "  \n")).await),
        ("text", "required")
    );
    assert_eq!(
        invalid(store.create(pattern("(unclosed", 0)).await),
        ("regex", "invalid")
    );
    assert_eq!(
        invalid(store.create(pattern("acme-(\\d+)", 2)).await),
        ("secret_group", "out_of_range")
    );
    assert_eq!(
        invalid(
            store
                .create(input(json!({
                    "name": "n",
                    "spec": { "mode": "pattern", "regex": "x", "min_entropy": 9.0 },
                })))
                .await
        ),
        ("min_entropy", "out_of_range")
    );
    assert!(
        store.list().await.unwrap().is_empty(),
        "rejected input is never stored"
    );

    let created = store
        .create(simple("  Staging  ", " A B C "))
        .await
        .unwrap();
    assert!(
        created
            .id
            .starts_with(stravia_credential_protection::CUSTOM_RULE_ID_PREFIX)
    );
    assert_eq!(created.name, "Staging");
    let listed = store.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        serde_json::to_value(&listed[0].spec).unwrap(),
        json!({ "mode": "simple", "text": " A B C " }),
        "matched text is stored as typed"
    );

    let updated = store
        .update(&created.id, input(json!({
            "name": "Staging",
            "enabled": false,
            "spec": { "mode": "pattern", "regex": "acme-(\\d+)", "secret_group": 1, "keywords": [" acme ", "", "acme"] },
        })))
        .await
        .unwrap();
    assert!(!updated.enabled);
    assert_eq!(updated.created_at, created.created_at);
    assert_eq!(
        serde_json::to_value(&updated.spec).unwrap()["keywords"],
        json!(["acme"]),
        "keywords are trimmed and deduplicated"
    );
    assert!(matches!(
        store.update("custom.missing", simple("n", "x")).await,
        Err(CustomRuleError::NotFound)
    ));

    store.delete(&created.id).await.unwrap();
    assert!(matches!(
        store.delete(&created.id).await,
        Err(CustomRuleError::NotFound)
    ));
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn sqlite_custom_rule_store_contract() {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::db::init_pool(directory.path()).await.unwrap();
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .unwrap();
    assert_store_contract(&SqlCustomRuleStore::sqlite(pool.clone())).await;
    pool.close().await;
}

#[tokio::test]
async fn postgres_custom_rule_store_contract_when_configured() {
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
    let schema = format!("stravia_custom_rule_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated PostgreSQL schema");
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("isolated PostgreSQL pool");
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .unwrap();
    assert_store_contract(&SqlCustomRuleStore::postgres(pool.clone())).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop isolated PostgreSQL schema");
    admin.close().await;
}

#[tokio::test]
async fn enabled_custom_rules_protect_requests_and_show_up_in_tests() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::from_storage(
        GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(crate::storage::MemoryStorage::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )),
    )
    .await
    .unwrap();
    let admin = gateway.admin();
    admin
        .set_setting(stravia_credential_protection::SETTING_KEY, "true")
        .await
        .unwrap();
    let literal = admin
        .create_credential_custom_rule(simple("Literal", "A B C"))
        .await
        .unwrap();
    let dormant = admin
        .create_credential_custom_rule(input(json!({
            "name": "Dormant",
            "enabled": false,
            "spec": { "mode": "simple", "text": "never-sent" },
        })))
        .await
        .unwrap();
    let pattern = admin
        .create_credential_custom_rule(pattern("acme-(\\d{4})-[a-z]+", 1))
        .await
        .unwrap();

    let text = "pass A B C, acme-1234-xyz, never-sent, a b c";
    let matches = admin.test_credential_protection(text.into()).await.unwrap();
    let hit = |id: &str| {
        matches
            .iter()
            .filter(|m| m.rule_id == id)
            .collect::<Vec<_>>()
    };
    let literal_hits = hit(&literal.id);
    assert_eq!(literal_hits.len(), 1, "matching is case-sensitive");
    assert_eq!((literal_hits[0].start, literal_hits[0].end), (5, 10));
    let pattern_hits = hit(&pattern.id);
    assert_eq!(pattern_hits.len(), 1);
    assert_eq!(
        (pattern_hits[0].start, pattern_hits[0].end),
        (17, 21),
        "only the selected capture group is the credential"
    );
    assert!(hit(&dormant.id).is_empty(), "disabled rules do not apply");

    let mut request = AiRequest::new("model", Vec::new());
    request.instructions = Some(text.into());
    let principal = Principal::new("custom-rule-owner");
    gateway
        .redaction
        .protect(&principal, &mut request, None)
        .await
        .unwrap();
    let protected = request.instructions.unwrap();
    assert!(!protected.contains("A B C"));
    assert!(!protected.contains("1234"));
    assert!(protected.contains("never-sent") && protected.contains("a b c"));
    assert!(
        protected.contains("acme-<!--sr:"),
        "text outside the group is kept"
    );

    admin
        .update_credential_custom_rule(
            &literal.id,
            input(json!({
                "name": "Literal",
                "enabled": false,
                "spec": { "mode": "simple", "text": "A B C" },
            })),
        )
        .await
        .unwrap();
    let after = admin.test_credential_protection(text.into()).await.unwrap();
    assert!(
        after.iter().all(|m| m.rule_id != literal.id),
        "a disabled rule stops matching immediately"
    );
}
