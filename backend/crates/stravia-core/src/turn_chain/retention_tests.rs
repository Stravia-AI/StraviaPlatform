//! 统一历史保留期的接缝测试：只通过领域写入/读取 API 观察节点、依赖与回收。

use std::sync::Arc;
use std::time::Duration;

use stravia_credential_protection::store::{DetectedSecret, MappingStore, SqlMappingStore};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::ToolCall;
use stravia_runtime_contract::turn_chain::{
    TurnChainStore, TurnCommit, TurnNodeId, TurnNodeKind, TurnUnavailable,
};

use super::SqlTurnChainStore;
use crate::compaction::{Compaction, CompactionError, CompactionRegistration, CompactionTarget};
use crate::history_marker::{HistoryMarkerStore, PlatformMarkerInput, SqlHistoryMarkerStore};
use crate::history_retention::HistoryRetention;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn principal() -> Principal {
    Principal::new("retention-owner")
}

fn call() -> ToolCall {
    ToolCall {
        id: "retention-call".into(),
        name: "web_search".into(),
        arguments: "{}".into(),
    }
}

async fn commit_referencing_node(
    store: &SqlTurnChainStore,
    reference: &str,
    marker: Option<&str>,
    compaction_record_id: Option<&str>,
) -> TurnNodeId {
    let id = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: id.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: principal(),
            payload_version: 1,
            payload: serde_json::json!({"items": []}),
            marker_references: marker.into_iter().map(str::to_owned).collect(),
            redaction_references: vec![reference.to_owned()],
            compaction_record_ids: compaction_record_id
                .into_iter()
                .map(str::to_owned)
                .collect(),
            reusable_prefix: None,
        })
        .await
        .expect("commit referencing node");
    id
}

async fn commit_plain_node(store: &SqlTurnChainStore, parent: Option<&TurnNodeId>) -> TurnNodeId {
    let id = TurnNodeId::response();
    store
        .commit(TurnCommit {
            id: id.clone(),
            kind: TurnNodeKind::Response,
            parent_id: parent.cloned(),
            principal: principal(),
            payload_version: 1,
            payload: serde_json::json!({"items": []}),
            marker_references: Vec::new(),
            redaction_references: Vec::new(),
            compaction_record_ids: Vec::new(),
            reusable_prefix: None,
        })
        .await
        .expect("commit plain node");
    id
}

async fn compaction_registration() -> (
    CompactionRegistration,
    stravia_runtime_contract::protocol::ir::AiItem,
) {
    let item = stravia_protocol_codec::codec::open_responses::decoder::decode_input_item(
        &serde_json::json!({
            "type": "compaction",
            "id": "retention-compaction",
            "encrypted_content": "opaque-retention-state"
        }),
    )
    .expect("decode compaction state")
    .expect("compaction item");
    (
        CompactionRegistration {
            source_generation_id: None,
            source_record_ids: Vec::new(),
            operation_id: "retention-operation".into(),
            target: CompactionTarget {
                target_key: "local-target".into(),
                namespace: "local-account-generation".into(),
                model: "local-model".into(),
                protocol: "open-responses/responses/2026-04-24".into(),
            },
            window: vec![item.clone()],
            state_items: vec![item.clone()],
        },
        item,
    )
}

async fn run_reclaim_scenario(
    store: Arc<SqlTurnChainStore>,
    markers: Arc<dyn HistoryMarkerStore>,
    mappings: Arc<dyn MappingStore>,
    compaction: Compaction,
) {
    let owner = principal();
    let started_at = chrono::Utc::now().timestamp_millis();
    let mapping = mappings
        .intern(&owner, &[DetectedSecret::from("Q8n4Vk7sT2p9X5a3Lc6D0h1R")])
        .await
        .expect("intern mapping")
        .mappings
        .remove(0);
    mappings
        .publish(&owner, std::slice::from_ref(&mapping.reference))
        .await
        .expect("publish mapping");
    let marker = markers
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web_search".into(),
                call: call(),
                activity: "Searching".into(),
                execution_limit: Duration::from_secs(60),
            },
        )
        .await
        .expect("create marker");
    markers
        .publish(&owner, std::slice::from_ref(&marker.reference))
        .await
        .expect("publish marker");
    let (registration, state_item) = compaction_registration().await;
    let record = compaction
        .register(&owner, registration)
        .await
        .expect("register compaction");
    let id = commit_referencing_node(
        &store,
        &mapping.reference,
        Some(&marker.reference),
        Some(&record.id),
    )
    .await;

    let within_window = started_at + DAY_MS / 2;
    store
        .sweep_history(within_window)
        .await
        .expect("early sweep");
    markers
        .sweep(within_window, DAY)
        .await
        .expect("early markers");
    mappings
        .sweep(within_window, DAY)
        .await
        .expect("early mappings");
    compaction
        .sweep(within_window, DAY)
        .await
        .expect("early compaction");
    assert!(
        store
            .materialize(&owner, TurnNodeKind::Response, &id)
            .await
            .is_ok(),
        "N 内的节点必须仍可读"
    );
    assert!(
        mappings
            .plaintext(&mapping.reference)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        markers
            .resolve(&owner, &marker.reference)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        compaction
            .resolve(&owner, &[state_item.clone()])
            .await
            .unwrap()
            .is_some()
    );

    let after_window = started_at + 2 * DAY_MS;
    store
        .sweep_history(after_window)
        .await
        .expect("late turn sweep");
    markers
        .sweep(after_window, DAY)
        .await
        .expect("late markers");
    mappings
        .sweep(after_window, DAY)
        .await
        .expect("late mappings");
    compaction
        .sweep(after_window, DAY)
        .await
        .expect("late compaction");
    assert!(matches!(
        store.materialize(&owner, TurnNodeKind::Response, &id).await,
        Err(TurnUnavailable::Unavailable)
    ));
    assert_eq!(mappings.plaintext(&mapping.reference).await.unwrap(), None);
    assert!(
        markers
            .resolve(&owner, &marker.reference)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        compaction.resolve(&owner, &[state_item]).await,
        Ok(None) | Err(CompactionError::Unavailable)
    ));
}

#[tokio::test]
async fn sqlite_sweep_reclaims_expired_node_and_its_dependencies() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let store =
        Arc::new(SqlTurnChainStore::sqlite(pool.clone()).with_retention(HistoryRetention::new(1)));
    run_reclaim_scenario(
        store,
        Arc::new(SqlHistoryMarkerStore::sqlite(pool.clone())),
        Arc::new(SqlMappingStore::sqlite(pool.clone())),
        Compaction::sqlite(pool),
    )
    .await;
}

#[tokio::test]
async fn sqlite_commit_rejects_missing_trusted_dependency() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let store = SqlTurnChainStore::sqlite(pool).with_retention(HistoryRetention::new(1));
    let error = store
        .commit(TurnCommit {
            id: TurnNodeId::response(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            principal: principal(),
            payload_version: 1,
            payload: serde_json::json!({"items": []}),
            marker_references: Vec::new(),
            redaction_references: vec!["missing-mapping".into()],
            compaction_record_ids: Vec::new(),
            reusable_prefix: None,
        })
        .await
        .expect_err("missing trusted dependency");
    assert!(matches!(
        error,
        stravia_runtime_contract::turn_chain::TurnCommitError::DependencyUnavailable
    ));
}

#[tokio::test]
async fn sqlite_sweep_reclaims_a_chain_child_first() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let store = SqlTurnChainStore::sqlite(pool).with_retention(HistoryRetention::new(1));
    let owner = principal();
    let root = commit_plain_node(&store, None).await;
    let child = commit_plain_node(&store, Some(&root)).await;
    let within_window = chrono::Utc::now().timestamp_millis() + DAY_MS / 2;
    store
        .sweep_history(within_window)
        .await
        .expect("early sweep");
    assert!(
        store
            .materialize(&owner, TurnNodeKind::Response, &child)
            .await
            .is_ok()
    );
    let after_window = chrono::Utc::now().timestamp_millis() + 2 * DAY_MS;
    store.sweep_history(after_window).await.expect("late sweep");
    for id in [&child, &root] {
        assert!(matches!(
            store.materialize(&owner, TurnNodeKind::Response, id).await,
            Err(TurnUnavailable::Unavailable)
        ));
    }
}

#[tokio::test]
async fn sqlite_retention_change_uses_the_current_window() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let store = SqlTurnChainStore::sqlite(pool).with_retention(HistoryRetention::new(7));
    let owner = principal();
    let id = commit_plain_node(&store, None).await;
    let after_one_window = chrono::Utc::now().timestamp_millis() + 2 * DAY_MS;
    store
        .sweep_history(after_one_window)
        .await
        .expect("seven day sweep");
    assert!(
        store
            .materialize(&owner, TurnNodeKind::Response, &id)
            .await
            .is_ok()
    );

    store.retention.set_days(1);
    store
        .sweep_history(after_one_window)
        .await
        .expect("one day sweep");
    assert!(matches!(
        store.materialize(&owner, TurnNodeKind::Response, &id).await,
        Err(TurnUnavailable::Unavailable)
    ));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at PostgreSQL"]
async fn postgres_sweep_reclaims_expired_node_and_its_dependencies() {
    let url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("DB_URL"))
        .expect("DATABASE_URL");
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("admin pool");
    let schema = format!("stravia_retention_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create schema");
    let options: sqlx::postgres::PgConnectOptions = url.parse().expect("options");
    let options = options.options([("search_path", schema.as_str())]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("isolated pool");
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .unwrap();
    let store = Arc::new(
        SqlTurnChainStore::postgres(pool.clone()).with_retention(HistoryRetention::new(1)),
    );
    run_reclaim_scenario(
        store,
        Arc::new(SqlHistoryMarkerStore::postgres(pool.clone())),
        Arc::new(SqlMappingStore::postgres(pool.clone())),
        Compaction::postgres(pool.clone()),
    )
    .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop schema");
    admin.close().await;
}
