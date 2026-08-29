use std::sync::Arc;

use sqlx::sqlite::SqlitePoolOptions;

use super::*;
use crate::protocol::ir::{AiItem, AiRequest, MessageContent};

async fn sqlite_store() -> Arc<dyn HistoryMarkerStore> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    Arc::new(SqlHistoryMarkerStore::sqlite(pool))
}

fn principal(id: &str) -> Principal {
    Principal::new(id)
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "web_search".into(),
        arguments: r#"{"query":"rust"}"#.into(),
    }
}

async fn assert_store_contract(store: Arc<dyn HistoryMarkerStore>) {
    let owner = principal("owner");
    let stranger = principal("stranger");
    let marker = store
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web-search".into(),
                call: call("call-1"),
                activity: "Searching the web".into(),
                execution_limit: Duration::from_secs(30),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .expect("platform marker");

    assert!(
        store
            .resolve(&stranger, &marker.reference)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .claim_execution(
                &owner,
                &marker.reference,
                "worker-a",
                Duration::from_secs(5)
            )
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    assert_eq!(
        store
            .claim_execution(
                &owner,
                &marker.reference,
                "worker-b",
                Duration::from_secs(5)
            )
            .await
            .unwrap(),
        ClaimOutcome::Busy
    );

    let segment = HiddenHistorySegment::Platform {
        call: call("call-1"),
        result: ContentBlock::ToolResult {
            tool_use_id: "call-1".into(),
            content: serde_json::json!({"answer": "stable"}),
            is_error: Some(false),
            cache_control: None,
        },
    };
    store
        .finish_execution(
            &owner,
            &marker.reference,
            "worker-a",
            PlatformExecutionState::Completed,
            segment,
        )
        .await
        .expect("terminal result");
    store
        .finish_execution(
            &owner,
            &marker.reference,
            "worker-a",
            PlatformExecutionState::Completed,
            HiddenHistorySegment::Platform {
                call: call("call-1"),
                result: ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: serde_json::json!({"answer": "stable"}),
                    is_error: Some(false),
                    cache_control: None,
                },
            },
        )
        .await
        .expect("identical terminal write is idempotent");
    let resolved = store
        .wait_terminal(&owner, &marker.reference)
        .await
        .unwrap()
        .expect("resolved marker");
    assert_eq!(
        resolved.execution_state,
        Some(PlatformExecutionState::Completed)
    );
    assert!(matches!(
        resolved.segment,
        Some(HiddenHistorySegment::Platform { .. })
    ));
}

#[tokio::test]
async fn sqlite_history_marker_store_contract() {
    assert_store_contract(sqlite_store().await).await;
}

#[tokio::test]
async fn postgres_history_marker_store_contract_when_configured() {
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
    let schema = format!("stravia_marker_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create isolated PostgreSQL schema");
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().expect("PostgreSQL connection options");
    let options = options.options([("search_path", schema.as_str())]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("isolated PostgreSQL pool");
    crate::migrations::migrate_postgres(&pool)
        .await
        .expect("PostgreSQL migrations");
    assert_store_contract(Arc::new(SqlHistoryMarkerStore::postgres(pool.clone()))).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop isolated PostgreSQL schema");
}

#[tokio::test]
async fn sqlite_claim_is_atomic_across_store_instances() {
    let data_dir = tempfile::tempdir().expect("temporary data directory");
    let pool = crate::db::init_pool(data_dir.path())
        .await
        .expect("SQLite pool");
    crate::migrations::migrate_sqlite(&pool)
        .await
        .expect("SQLite migrations");
    let first: Arc<dyn HistoryMarkerStore> = Arc::new(SqlHistoryMarkerStore::sqlite(pool.clone()));
    let second: Arc<dyn HistoryMarkerStore> = Arc::new(SqlHistoryMarkerStore::sqlite(pool));
    let owner = principal("owner");
    let marker = first
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web-search".into(),
                call: call("call-atomic"),
                activity: "Searching the web".into(),
                execution_limit: Duration::from_secs(30),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();

    let (left, right) = tokio::join!(
        first.claim_execution(
            &owner,
            &marker.reference,
            "worker-a",
            Duration::from_secs(5)
        ),
        second.claim_execution(
            &owner,
            &marker.reference,
            "worker-b",
            Duration::from_secs(5)
        )
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ClaimOutcome::Claimed)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ClaimOutcome::Busy)
            .count(),
        1
    );
}

#[tokio::test]
async fn sqlite_deadline_and_lost_lease_become_distinct_terminal_errors() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let deadline = store
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web-search".into(),
                call: call("deadline"),
                activity: "Searching the web".into(),
                execution_limit: Duration::from_millis(1),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .resolve(&owner, &deadline.reference)
            .await
            .unwrap()
            .unwrap()
            .execution_deadline_unix_ms
            .is_some()
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    let deadline = store
        .resolve(&owner, &deadline.reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        deadline.execution_state,
        Some(PlatformExecutionState::Failed)
    );

    let interrupted = store
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web-search".into(),
                call: call("interrupted"),
                activity: "Searching the web".into(),
                execution_limit: Duration::from_secs(10),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_execution(
                &owner,
                &interrupted.reference,
                "worker",
                Duration::from_millis(1),
            )
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    let interrupted = store
        .resolve(&owner, &interrupted.reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        interrupted.execution_state,
        Some(PlatformExecutionState::Interrupted)
    );
    assert_eq!(
        store
            .claim_execution(
                &owner,
                &interrupted.marker.reference,
                "replacement",
                Duration::from_secs(1),
            )
            .await
            .unwrap(),
        ClaimOutcome::Terminal
    );
    assert!(matches!(
        store
            .finish_execution(
                &owner,
                &interrupted.marker.reference,
                "worker",
                PlatformExecutionState::Completed,
                HiddenHistorySegment::Platform {
                    call: call("interrupted"),
                    result: ContentBlock::ToolResult {
                        tool_use_id: "interrupted".into(),
                        content: serde_json::json!("late"),
                        is_error: Some(false),
                        cache_control: None,
                    },
                },
            )
            .await,
        Err(HistoryMarkerError::TerminalConflict)
    ));
}

#[tokio::test]
async fn sqlite_completed_marker_remains_immutable_after_its_execution_deadline() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let marker = store
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web-search".into(),
                call: call("completed"),
                activity: "Searching the web".into(),
                execution_limit: Duration::from_millis(50),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .claim_execution(&owner, &marker.reference, "worker", Duration::from_secs(1))
        .await
        .unwrap();
    let segment = HiddenHistorySegment::Platform {
        call: call("completed"),
        result: ContentBlock::ToolResult {
            tool_use_id: "completed".into(),
            content: serde_json::json!("done"),
            is_error: Some(false),
            cache_control: None,
        },
    };
    store
        .finish_execution(
            &owner,
            &marker.reference,
            "worker",
            PlatformExecutionState::Completed,
            segment.clone(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(75)).await;
    let resolved = store
        .resolve(&owner, &marker.reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resolved.execution_state,
        Some(PlatformExecutionState::Completed)
    );
    assert_eq!(
        serde_json::to_value(resolved.segment).unwrap(),
        serde_json::to_value(Some(segment)).unwrap()
    );
}

#[tokio::test]
async fn sqlite_thinking_markers_are_immutable_and_publish_extends_retention() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let block = ContentBlock::RedactedThinking {
        data: "opaque".into(),
    };
    let marker = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: block.clone(),
                activity: "Preserving protected reasoning".into(),
                pending_retention: Duration::from_millis(50),
            },
        )
        .await
        .unwrap();
    store
        .publish(
            &owner,
            std::slice::from_ref(&marker.reference),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    store.cleanup_expired().await.unwrap();
    let resolved = store
        .resolve(&owner, &marker.reference)
        .await
        .unwrap()
        .unwrap();
    assert!(resolved.published);
    let Some(HiddenHistorySegment::Thinking {
        block: restored_block,
    }) = resolved.segment
    else {
        panic!("Thinking marker should restore one Thinking segment");
    };
    assert_eq!(
        serde_json::to_value(restored_block).unwrap(),
        serde_json::to_value(block).unwrap()
    );
}

#[tokio::test]
async fn resolver_restores_trailing_marker_before_its_public_item_and_preserves_edits() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let marker = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "authoritative".into(),
                    signature: Some("opaque-signature".into()),
                },
                activity: "Preserving protected reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .publish(
            &owner,
            std::slice::from_ref(&marker.reference),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let rendered = render_history_marker(&marker);
    let mut request = AiRequest::new(
        "model",
        vec![
            AiItem::output_text(format!("client edit\n{rendered}")),
            AiItem::output_text(rendered),
            AiItem::function_call(call("public-call")),
        ],
    );

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary.restored_thinking_segments, 1);
    assert_eq!(request.items.len(), 3);
    assert!(matches!(
        &request.items[0].content,
        MessageContent::Blocks(blocks)
            if matches!(
                blocks.as_slice(),
                [ContentBlock::Thinking {
                    thinking,
                    signature: Some(signature),
                }] if thinking == "authoritative" && signature == "opaque-signature"
            )
    ));
    assert_eq!(request.items[1].output_text_ref(), Some("client edit"));
    assert_eq!(
        request.items[2]
            .function_call_ref()
            .map(|call| call.id.as_str()),
        Some("public-call")
    );
}

#[tokio::test]
async fn resolver_collapses_legacy_marker_item_duplicate_without_losing_public_call() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let marker = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "authoritative".into(),
                    signature: Some("opaque-signature".into()),
                },
                activity: "Preserving protected reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .publish(
            &owner,
            std::slice::from_ref(&marker.reference),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let mut marked = AiItem::output_text(format!("{}public", render_history_marker(&marker)));
    marked.tool_calls = Some(vec![call("public-call")]);
    let mut duplicate = AiItem::output_text("public");
    duplicate.tool_calls = Some(vec![call("public-call")]);
    let mut request = AiRequest::new(
        "model",
        vec![
            marked,
            duplicate,
            AiItem::function_call_output("public-call", serde_json::Value::String("result".into())),
        ],
    );

    resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(
        request
            .items
            .iter()
            .flat_map(|item| item.tool_calls.iter().flatten())
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["public-call"]
    );
    assert_eq!(
        request
            .items
            .iter()
            .filter_map(|item| item.tool_call_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["public-call"]
    );
}

#[tokio::test]
async fn resolver_removes_unknown_and_unauthorized_private_markers() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let stranger = principal("stranger");
    let marker = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::RedactedThinking {
                    data: "private".into(),
                },
                activity: "Preserving protected reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .publish(
            &owner,
            std::slice::from_ref(&marker.reference),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let unknown = HistoryMarker {
        reference: "hm_ffffffffffffffffffff".into(),
        kind: HistoryMarkerKind::Thinking,
        activity: "Preserving protected reasoning".into(),
    };
    let mut request = AiRequest::new(
        "model",
        vec![AiItem::output_text(format!(
            "{}\n{}",
            render_history_marker(&marker),
            render_history_marker(&unknown)
        ))],
    );

    let summary = resolve_request_markers(store.as_ref(), &stranger, &mut request)
        .await
        .unwrap();

    assert_eq!(summary, MarkerResolution::default());
    assert!(request.items.is_empty());
}
