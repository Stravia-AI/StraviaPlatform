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
async fn resolver_restores_trailing_marker_at_its_exact_position_and_preserves_edits() {
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
    assert_eq!(request.items[0].output_text_ref(), Some("client edit\n"));
    assert!(matches!(
        &request.items[1].content,
        MessageContent::Blocks(blocks)
            if matches!(
                blocks.as_slice(),
                [ContentBlock::Thinking {
                    thinking,
                    signature: Some(signature),
                }] if thinking == "authoritative" && signature == "opaque-signature"
            )
    ));
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
    assert_eq!(request.items.len(), 1);
    assert_eq!(request.items[0].output_text_ref(), Some("\n"));
}

#[tokio::test]
async fn resolver_restores_projected_text_and_platform_segment_at_exact_marker_position() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let marker = store
        .create_platform(
            &owner,
            PlatformMarkerInput {
                tool_id: "web-search".into(),
                call: call("call-ordered"),
                activity: "Searching the web".into(),
                execution_limit: Duration::from_secs(30),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .claim_execution(&owner, &marker.reference, "worker", Duration::from_secs(30))
        .await
        .unwrap();
    store
        .finish_execution(
            &owner,
            &marker.reference,
            "worker",
            PlatformExecutionState::Completed,
            HiddenHistorySegment::Platform {
                call: call("call-ordered"),
                result: ContentBlock::ToolResult {
                    tool_use_id: "call-ordered".into(),
                    content: serde_json::json!({"answer": "stable"}),
                    is_error: Some(false),
                    cache_control: None,
                },
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

    let projected = format!(
        "R1{}{}R2",
        render_text_projection_span(&marker.reference, 0, "C1"),
        render_history_marker(&marker)
    );
    let mut request = AiRequest::new(
        "model",
        vec![AiItem::thinking(projected, None), AiItem::output_text("C2")],
    );

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary.restored_platform_segments, 1);
    assert_eq!(request.items.len(), 6);
    assert!(matches!(
        request.items[0].thinking_ref(),
        Some(("R1", None))
    ));
    assert_eq!(request.items[1].output_text_ref(), Some("C1"));
    assert_eq!(
        request.items[2]
            .function_call_ref()
            .map(|call| call.id.as_str()),
        Some("call-ordered")
    );
    assert_eq!(
        request.items[3].tool_call_id.as_deref(),
        Some("call-ordered")
    );
    assert!(matches!(
        request.items[4].thinking_ref(),
        Some(("R2", None))
    ));
    assert_eq!(request.items[5].output_text_ref(), Some("C2"));
}

#[tokio::test]
async fn resolver_replaces_protected_preview_with_authoritative_thinking() {
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
    let projected = format!(
        "{}{}",
        render_preview_projection_span(&marker.reference, 0, "visible preview"),
        render_history_marker(&marker)
    );
    let mut request = AiRequest::new("model", vec![AiItem::thinking(projected, None)]);

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary.restored_thinking_segments, 1);
    assert_eq!(request.items.len(), 1);
    assert!(matches!(
        request.items[0].thinking_ref(),
        Some(("authoritative", Some("opaque-signature")))
    ));
}

#[tokio::test]
async fn resolver_treats_content_preview_as_display_only_while_marker_remains() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let marker = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "authoritative R2".into(),
                    signature: None,
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
    let edited_preview =
        render_preview_projection_span(&marker.reference, 0, "\n> client-edited preview\n");
    let mut request = AiRequest::new(
        "model",
        vec![
            AiItem::output_text("C1"),
            AiItem::output_text(format!(
                "{edited_preview}{}",
                render_history_marker(&marker)
            )),
            AiItem::output_text("C2"),
        ],
    );

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary.restored_thinking_segments, 1);
    assert_eq!(request.items.len(), 3);
    assert_eq!(request.items[0].output_text_ref(), Some("C1"));
    assert!(matches!(
        request.items[1].thinking_ref(),
        Some(("authoritative R2", None))
    ));
    assert_eq!(request.items[2].output_text_ref(), Some("C2"));
}

#[tokio::test]
async fn reserved_thinking_reference_is_in_memory_until_atomic_creation() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let reserved = reserve_thinking_marker();

    assert!(
        store
            .resolve(&owner, &reserved.reference)
            .await
            .unwrap()
            .is_none()
    );
    let marker = store
        .create_reserved_thinking(
            &owner,
            &reserved,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "authoritative".into(),
                    signature: None,
                },
                activity: reserved.activity.clone(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();

    assert_eq!(marker.reference, reserved.reference);
    let stored = store
        .resolve(&owner, &marker.reference)
        .await
        .unwrap()
        .expect("complete reserved Thinking Marker");
    assert!(matches!(
        stored.segment,
        Some(HiddenHistorySegment::Thinking {
            block: ContentBlock::Thinking {
                thinking,
                signature: None,
            }
        }) if thinking == "authoritative"
    ));
}

#[tokio::test]
async fn resolver_keeps_content_preview_as_text_after_marker_deletion() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let reference = "hm_0123456789abcdefghij";
    let edited_preview = render_preview_projection_span(reference, 0, "\n> retained client text\n");
    let mut request = AiRequest::new("model", vec![AiItem::output_text(edited_preview)]);

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary, MarkerResolution::default());
    assert_eq!(
        request.items[0].output_text_ref(),
        Some("\n> retained client text\n")
    );
}

#[tokio::test]
async fn resolver_replaces_reasoning_previews_and_restores_redacted_blocks() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let encrypted = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Reasoning {
                    summary: vec!["authoritative summary".into()],
                    content: vec!["authoritative content".into()],
                    encrypted_content: Some("opaque-encrypted-content".into()),
                },
                activity: "Preserving encrypted reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    let redacted = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::RedactedThinking {
                    data: "opaque-redacted-content".into(),
                },
                activity: "Preserving redacted reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .publish(
            &owner,
            &[encrypted.reference.clone(), redacted.reference.clone()],
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let preview = AiItem::reasoning(
        vec![render_preview_projection_span(
            &encrypted.reference,
            0,
            "visible summary",
        )],
        vec![render_preview_projection_span(
            &encrypted.reference,
            1,
            "visible content",
        )],
        None,
    );
    let markers = AiItem::thinking(
        format!(
            "{}{}",
            render_history_marker(&encrypted),
            render_history_marker(&redacted)
        ),
        None,
    );
    let mut request = AiRequest::new("model", vec![preview, markers]);

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary.restored_thinking_segments, 2);
    assert_eq!(request.items.len(), 2);
    assert!(matches!(
        request.items[0].reasoning_ref(),
        Some((summary, content, Some("opaque-encrypted-content")))
            if summary == ["authoritative summary"] && content == ["authoritative content"]
    ));
    assert!(matches!(
        &request.items[1].content,
        MessageContent::Blocks(blocks)
            if matches!(
                blocks.as_slice(),
                [ContentBlock::RedactedThinking { data }]
                    if data == "opaque-redacted-content"
            )
    ));
}

#[tokio::test]
async fn resolver_strips_mismatched_delimiters_without_retyping_visible_bytes() {
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
    let projected = format!(
        "{prefix}{reference}:text:0:start -->edited{prefix}{reference}:text:1:end -->{}",
        render_history_marker(&marker),
        prefix = PROJECTION_DELIMITER_PREFIX,
        reference = marker.reference,
    );
    let mut request = AiRequest::new("model", vec![AiItem::thinking(projected, None)]);

    resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(request.items.len(), 2);
    assert!(matches!(
        request.items[0].thinking_ref(),
        Some(("edited", None))
    ));
    assert!(matches!(
        request.items[1].thinking_ref(),
        Some(("authoritative", Some("opaque-signature")))
    ));
}

#[tokio::test]
async fn resolver_treats_marker_removal_as_an_explicit_projection_edit() {
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
    let mut request = AiRequest::new(
        "model",
        vec![AiItem::thinking(
            render_text_projection_span(&marker.reference, 0, "client edit"),
            None,
        )],
    );

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary, MarkerResolution::default());
    assert_eq!(request.items.len(), 1);
    assert!(matches!(
        request.items[0].thinking_ref(),
        Some(("client edit", None))
    ));
}

#[tokio::test]
async fn resolver_restores_multiple_markers_at_block_boundaries_once_in_order() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let first = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "first authoritative".into(),
                    signature: Some("first-signature".into()),
                },
                activity: "Preserving first reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    let second = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "second authoritative".into(),
                    signature: Some("second-signature".into()),
                },
                activity: "Preserving second reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    store
        .publish(
            &owner,
            &[first.reference.clone(), second.reference.clone()],
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let projected = format!(
        "{}middle{}{}",
        render_history_marker(&first),
        render_history_marker(&second),
        render_history_marker(&first)
    );
    let mut request = AiRequest::new("model", vec![AiItem::thinking(projected, None)]);

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary.restored_thinking_segments, 2);
    assert_eq!(request.items.len(), 3);
    assert!(matches!(
        request.items[0].thinking_ref(),
        Some(("first authoritative", Some("first-signature")))
    ));
    assert!(matches!(
        request.items[1].thinking_ref(),
        Some(("middle", None))
    ));
    assert!(matches!(
        request.items[2].thinking_ref(),
        Some(("second authoritative", Some("second-signature")))
    ));
}

#[tokio::test]
async fn resolver_strips_unpublished_and_expired_markers_without_losing_visible_text() {
    let store = sqlite_store().await;
    let owner = principal("owner");
    let unpublished = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "unpublished".into(),
                    signature: Some("opaque".into()),
                },
                activity: "Unpublished reasoning".into(),
                pending_retention: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
    let expired = store
        .create_thinking(
            &owner,
            ThinkingMarkerInput {
                block: ContentBlock::Thinking {
                    thinking: "expired".into(),
                    signature: Some("opaque".into()),
                },
                activity: "Expired reasoning".into(),
                pending_retention: Duration::ZERO,
            },
        )
        .await
        .unwrap();
    let projected = format!(
        "{}visible{}",
        render_history_marker(&unpublished),
        render_history_marker(&expired)
    );
    let mut request = AiRequest::new("model", vec![AiItem::thinking(projected, None)]);

    let summary = resolve_request_markers(store.as_ref(), &owner, &mut request)
        .await
        .unwrap();

    assert_eq!(summary, MarkerResolution::default());
    assert_eq!(request.items.len(), 1);
    assert!(matches!(
        request.items[0].thinking_ref(),
        Some(("visible", None))
    ));
}
