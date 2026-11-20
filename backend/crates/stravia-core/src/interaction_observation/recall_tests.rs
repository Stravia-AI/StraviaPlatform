//! Recall contracts shared by SQLite and PostgreSQL.

use std::sync::Arc;

use serde_json::json;
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};

use super::*;

struct Fixture {
    observation: InteractionObservation,
    cleanup: Cleanup,
    _directory: tempfile::TempDir,
}

enum Cleanup {
    Sqlite(sqlx::SqlitePool),
    Postgres {
        pool: sqlx::PgPool,
        admin: sqlx::PgPool,
        schema: String,
    },
}

impl Fixture {
    async fn sqlite() -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            crate::history_retention::HistoryRetention::new(1),
            true,
            crate::generation_chain::test_chain().await,
        )
        .await;
        Ok(Self {
            observation,
            cleanup: Cleanup::Sqlite(pool),
            _directory: directory,
        })
    }

    async fn postgres() -> anyhow::Result<Self> {
        let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
            .expect("set STRAVIA_TEST_POSTGRES_URL to run PostgreSQL recall contracts");
        let directory = tempfile::tempdir()?;
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_recall_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        crate::migrations::migrate_postgres(&pool, None).await?;
        let observation = InteractionObservation::new(
            None,
            Some(pool.clone()),
            directory.path().to_path_buf(),
            crate::history_retention::HistoryRetention::new(1),
            true,
            crate::generation_chain::test_chain().await,
        )
        .await;
        Ok(Self {
            observation,
            cleanup: Cleanup::Postgres {
                pool,
                admin,
                schema,
            },
            _directory: directory,
        })
    }

    async fn execute(&self, sql: &'static str) -> anyhow::Result<()> {
        match &self.cleanup {
            Cleanup::Sqlite(pool) => {
                let mut tx = pool.begin_write().await?;
                sqlx::raw_sql(sql).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            Cleanup::Postgres { pool, .. } => {
                sqlx::raw_sql(sql).execute(pool).await?;
            }
        }
        Ok(())
    }

    async fn close(self) -> anyhow::Result<()> {
        self.observation.shutdown().await;
        match self.cleanup {
            Cleanup::Sqlite(pool) => pool.close().await,
            Cleanup::Postgres {
                pool,
                admin,
                schema,
            } => {
                pool.close().await;
                let result =
                    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                        .execute(&admin)
                        .await;
                admin.close().await;
                result?;
            }
        }
        Ok(())
    }
}

// Fixed timestamps, parent links and terminal states make ordering and cutoff
// boundaries independent of the wall clock and attribution heuristics.
async fn listing_contract(fixture: &Fixture) -> anyhow::Result<()> {
    fixture.execute("INSERT INTO interaction_observations
        (id,principal,root_id,root_run_id,parent_interaction_id,first_route_id,first_model_display_name,status,started_at,last_active_at) VALUES
        ('a','owner','chain-a','a',NULL,'route','head-model','completed',100,120),
        ('bad','owner','chain-a','bad','a','route',NULL,'interrupted',101,121),
        ('b','owner','chain-a','b','bad','route','child-model','completed',102,200),
        ('c','owner','chain-a','c','a','route',NULL,'interrupted',103,200),
        ('other','owner','chain-other','other',NULL,'route',NULL,'completed',104,200),
        ('cross','owner','chain-a','cross','other','route',NULL,'completed',105,150),
        ('old','owner','chain-old','old',NULL,'route',NULL,'completed',90,100),
        ('mcp','owner','chain-mcp','mcp',NULL,'route',NULL,'completed',106,300),
        ('foreign','stranger','chain-foreign','foreign',NULL,'route',NULL,'completed',107,400);
        INSERT INTO inference_run_observations
        (id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,finished_at,last_event_sequence,client_output_committed,terminal_reason) VALUES
        ('a','a','responses','route','completed',false,100,120,120,0,true,NULL),
        ('bad','bad','responses','route','failed',false,101,121,121,0,false,'upstream_error'),
        ('b','b','responses','route','completed',false,102,200,200,0,true,NULL),
        ('c','c','responses','route','failed',false,103,200,200,0,true,'upstream_error'),
        ('other','other','responses','route','failed',false,104,200,200,0,false,'cancelled'),
        ('cross','cross','responses','route','completed',false,105,150,150,0,true,NULL),
        ('old','old','responses','route','completed',false,90,99,99,0,true,NULL),
        ('mcp','mcp','mcp','route','completed',false,106,300,300,0,true,NULL),
        ('foreign','foreign','responses','route','completed',false,107,400,400,0,true,NULL);").await?;
    // Interaction status is a run rollup and is never `failed`; a zero-output cancellation stays listed.
    fixture
        .execute("UPDATE interaction_observations SET status='interrupted' WHERE id='other'")
        .await?;
    let o = &fixture.observation;
    let owner = o.recall("owner", 100);
    // A failed run alone does not exclude an interaction that is still running.
    fixture
        .execute("UPDATE interaction_observations SET status='running' WHERE id='bad'")
        .await?;
    assert!(owner.interaction("chain-a", "bad").await?.is_some());
    fixture
        .execute("UPDATE interaction_observations SET status='interrupted' WHERE id='bad'")
        .await?;
    // chain-a (b, c) and chain-other all last ran at 200: chains page by their
    // representative member's (last_active_at, id), and b represents chain-a.
    let page = owner.chains(None, 1).await?;
    assert_eq!(page.total, 2);
    assert!(page.has_more);
    let chain = &page.chains[0];
    assert_eq!(
        (
            &*chain.root_id,
            &*chain.head_id,
            &*chain.latest_id,
            chain.interactions,
            chain.last_active_at
        ),
        ("chain-a", "a", "b", 4, 200)
    );
    assert_eq!(chain.status, "completed");
    assert_eq!(chain.model.as_deref(), Some("head-model"));
    let cursor = (chain.last_active_at, chain.latest_id.clone());
    // c sorts after the cursor but must not list chain-a a second time.
    let page2 = owner.chains(Some(&cursor), 1).await?;
    assert_eq!(page2.total, 2);
    assert!(!page2.has_more);
    assert_eq!(
        page2
            .chains
            .iter()
            .map(|chain| chain.root_id.as_str())
            .collect::<Vec<_>>(),
        ["chain-other"]
    );
    assert_eq!(page2.chains[0].status, "interrupted"); // a cancellation is not a failed request.

    let members = owner.members("chain-a", None, 2).await?.unwrap();
    assert_eq!(
        (members.total, members.started_at, members.has_more),
        (4, 100, true)
    );
    assert_eq!(
        members
            .members
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(members.members[1].parent_id.as_deref(), Some("bad"));
    assert!(!members.members[1].parent_listed);
    let cursor = (members.members[1].started_at, members.members[1].id.clone());
    let rest = owner.members("chain-a", Some(&cursor), 2).await?.unwrap();
    assert_eq!(rest.total, 4);
    assert!(!rest.has_more);
    assert_eq!(
        rest.members
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["c", "cross"]
    );
    assert!(rest.members[0].parent_listed);
    assert_eq!(
        (&*rest.members[0].status, &*rest.members[1].status),
        ("failed", "completed")
    );
    assert!(!rest.members[1].parent_listed); // parent belongs to another chain
    assert!(owner.interaction("chain-a", "bad").await?.is_none());
    assert!(owner.interaction("chain-mcp", "mcp").await?.is_none());
    assert!(owner.interaction("chain-old", "old").await?.is_none());
    assert!(owner.members("chain-old", None, 10).await?.is_none());
    let b = owner.interaction("chain-a", "b").await?.unwrap();
    assert_eq!(b.parent_id(), Some("bad"));
    assert_eq!(b.protocol(), "responses");
    assert!(b.runs().contains_key("b"));
    assert!(owner.interaction("chain-other", "b").await?.is_none());
    let nobody = o.recall("nobody", 0);
    assert!(nobody.chains(None, 10).await?.chains.is_empty());
    let stranger = o.recall("stranger", 0);
    assert!(stranger.members("chain-a", None, 10).await?.is_none());
    assert!(stranger.interaction("chain-a", "a").await?.is_none());
    assert!(
        stranger
            .previews(&["a".into(), "b".into()])
            .await?
            .is_empty()
    );
    Ok(())
}

fn user_item(text: &str) -> AiItem {
    AiItem {
        role: Role::User,
        content: MessageContent::Text(Arc::new(text.into())),
        tool_calls: None,
        tool_call_id: None,
        meta: None,
    }
}

async fn events_contract(fixture: &Fixture) -> anyhow::Result<()> {
    let o = &fixture.observation;
    let text = format!("question {}", "x".repeat(5000));
    let input = user_item(&text);
    let run = o
        .observe_ingress(IngressStart {
            id: "recall-run".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: "recall-run".into(),
                principal: "event-owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: Some("event-model".into()),
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new("model", vec![input.clone()])),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    run.capture_user_input(&[input]);
    run.publish_user_input();
    run.record(RunEvent::ClientToolHandoff {
        tool_id: "client".into(),
        name: "lookup".into(),
        input: Some(json!({"query":"abc"})),
    });
    run.record(RunEvent::PlatformToolStarted {
        model_turn_id: "turn".into(),
        tool_id: "platform".into(),
        name: "search".into(),
        input: Some(json!({"page":1})),
    });
    run.record(RunEvent::PlatformToolFinished {
        model_turn_id: "turn".into(),
        tool_id: "platform".into(),
        status: "completed".into(),
        duration_ms: 5,
        content: Some(json!({"found":true})),
    });
    run.record(RunEvent::ClientToolResult {
        tool_id: "client".into(),
        content: json!("result text"),
        is_error: false,
    });
    run.record(RunEvent::ClientToolResult {
        tool_id: "unrelated".into(),
        content: json!("other result"),
        is_error: true,
    });
    run.record(RunEvent::ObservationGap {
        reason: "test gap".into(),
    });
    run.record(RunEvent::ClientVisibleContent {
        text: "answer".into(),
        parts: vec![json!({"type":"text","text":"answer"})],
        block_id: super::types::canonical_item_block_id("answer", 0),
        item: json!({"id":"answer","role":"assistant","content":"answer"}),
        complete: true,
    });
    run.finish(RunOutcome {
        client_output_committed: true,
        delivery: None,
        delivery_completed_at: Some(writer::now()),
        status: "completed".into(),
        terminal_reason: None,
        generation_node_id: None,
        generation_root_id: None,
    });
    let failed_input = user_item("failed zero-output secret");
    let failed_run = o
        .observe_ingress(IngressStart {
            id: "recall-failed-run".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: "recall-failed-run".into(),
                principal: "event-owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: Some("event-model".into()),
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new("model", vec![failed_input.clone()])),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    failed_run.capture_user_input(&[failed_input]);
    failed_run.publish_user_input();
    failed_run.finish(RunOutcome {
        client_output_committed: false,
        delivery: None,
        delivery_completed_at: Some(writer::now()),
        status: "failed".into(),
        terminal_reason: Some("upstream_error".into()),
        generation_node_id: None,
        generation_root_id: None,
    });

    let mcp_input = user_item("mcp tool call secret");
    let mcp_run = o
        .observe_ingress(IngressStart {
            id: "recall-mcp-run".into(),
            method: "POST".into(),
            path: "/mcp".into(),
            protocol: "mcp".into(),
        })
        .admit(
            RunStart {
                id: "recall-mcp-run".into(),
                principal: "event-owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "mcp".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new("model", vec![mcp_input.clone()])),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    mcp_run.capture_user_input(&[mcp_input]);
    mcp_run.publish_user_input();
    mcp_run.finish(RunOutcome {
        client_output_committed: true,
        delivery: None,
        delivery_completed_at: Some(writer::now()),
        status: "completed".into(),
        terminal_reason: None,
        generation_node_id: None,
        generation_root_id: None,
    });

    o.flush().await?;
    let event_owner = o.recall("event-owner", 0);
    let chains = event_owner.chains(None, 10).await?;
    assert_eq!(chains.total, 1);
    let chain = &chains.chains[0];
    let id = &chain.head_id;
    let interaction = event_owner.interaction(&chain.root_id, id).await?.unwrap();
    assert_eq!(
        interaction.runs()["recall-run"].model.as_deref(),
        Some("event-model")
    );
    let failed_id = failed_run.interaction_id().borrow().clone().unwrap();
    let mcp_id = mcp_run.interaction_id().borrow().clone().unwrap();
    let previews = event_owner
        .previews(&[id.clone(), failed_id.clone(), mcp_id.clone()])
        .await?;
    assert_eq!(previews.len(), 1);
    assert_eq!(previews.get(id), Some(&text));
    assert!(
        o.recall("stranger", 0)
            .previews(std::slice::from_ref(id))
            .await?
            .is_empty()
    );
    assert!(
        o.recall("event-owner", i64::MAX)
            .previews(std::slice::from_ref(id))
            .await?
            .is_empty()
    );
    let events = interaction.events(0, 100).await?;
    assert!(events.windows(2).all(|w| w[0].sequence < w[1].sequence));
    assert!(events.iter().all(|e| matches!(
        e.kind.as_str(),
        "user_input_recorded"
            | "client_visible_content"
            | "client_tool_handoff"
            | "platform_tool_started"
            | "client_tool_result"
            | "platform_tool_finished"
            | "observation_gap"
            | "request_failed"
    )));
    assert!(
        events
            .iter()
            .any(|e| e.kind == "user_input_recorded" && e.payload["text"] == text)
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "client_visible_content" && e.payload["text"] == "answer")
    );
    let first = interaction.events(0, 1).await?;
    assert_eq!(first.len(), 1);
    let remaining = interaction.events(first[0].sequence, 100).await?;
    assert_eq!(
        remaining.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        events[1..].iter().map(|e| e.sequence).collect::<Vec<_>>()
    );
    let second = events
        .iter()
        .find(|e| e.kind == "platform_tool_started")
        .unwrap();
    assert_eq!(
        interaction.tool_ids(second.sequence).await?,
        [Some("client".to_owned())]
    );
    assert_eq!(
        interaction.tool_ids(i64::MAX).await?,
        [Some("client".to_owned()), Some("platform".to_owned())]
    );
    assert!(interaction.tool(0).await?.is_none());
    let client = interaction.tool(1).await?.unwrap();
    assert_eq!(client.call.kind, "client_tool_handoff");
    assert_eq!(client.call.payload["name"], "lookup");
    assert_eq!(client.call.payload["input"], json!({"query":"abc"}));
    assert_eq!(client.results.len(), 1);
    assert!(
        client.results[0].payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["tool_id"] == "client"
                && r["content"] == "result text"
                && r["is_error"] == false)
    );
    let platform = interaction.tool(2).await?.unwrap();
    assert_eq!(platform.call.kind, "platform_tool_started");
    assert_eq!(platform.results.len(), 1);
    assert_eq!(platform.results[0].kind, "platform_tool_finished");
    assert_eq!(
        platform.results[0].payload["content"],
        json!({"found":true})
    );
    assert!(interaction.tool(3).await?.is_none());

    let fallback = o
        .observe_ingress(IngressStart {
            id: "fallback-run".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: "fallback-run".into(),
                principal: "fallback-owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new("model", vec![])),
                has_new_user: false,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    fallback.record(RunEvent::ClientVisibleContent {
        text: "fallback answer".into(),
        parts: vec![json!({"type":"text","text":"fallback answer"})],
        block_id: super::types::canonical_item_block_id("fallback", 0),
        item: json!({"id":"fallback","role":"assistant","content":"fallback answer"}),
        complete: true,
    });
    fallback.finish(RunOutcome {
        client_output_committed: true,
        delivery: None,
        delivery_completed_at: Some(writer::now()),
        status: "completed".into(),
        terminal_reason: None,
        generation_node_id: None,
        generation_root_id: None,
    });
    o.flush().await?;
    let fallback_owner = o.recall("fallback-owner", 0);
    let fallback_chains = fallback_owner.chains(None, 10).await?;
    let fallback_id = &fallback_chains.chains[0].head_id;
    let previews = fallback_owner
        .previews(&[fallback_id.clone(), "missing".into()])
        .await?;
    assert_eq!(previews.len(), 1);
    assert_eq!(
        previews.get(fallback_id).map(String::as_str),
        Some("fallback answer")
    );
    Ok(())
}

async fn contract(fixture: &Fixture) -> anyhow::Result<()> {
    listing_contract(fixture).await?;
    events_contract(fixture).await
}

#[tokio::test]
async fn sqlite_recall_queries_only_use_keyed_lookups() -> anyhow::Result<()> {
    // Without ANALYZE statistics SQLite once answered `status='failed'` through
    // inference_runs_status_idx, scanning every failed run once per listed interaction.
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let preview_ids = ["a".to_owned(), "b".to_owned()];
    let previews_sql =
        super::recall::build_previews_query::<sqlx::Sqlite>("owner", 100, &preview_ids)
            .sql()
            .as_str()
            .to_owned();
    for (is_previews, sql) in [
        (false, super::recall::chain_count_sql()),
        (false, super::recall::chain_page_sql()),
        (false, super::recall::member_count_sql()),
        (false, super::recall::member_page_sql()),
        (false, super::recall::interaction_sql()),
        (true, previews_sql),
    ] {
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(&pool)
                .await?;
        for (_, _, _, detail) in &plan {
            let table_scan = detail.strip_prefix("SCAN ").is_some_and(|name| {
                !["page", "chains", "d", "req", "m", "ranked"].contains(&name)
                    && !name.ends_with("CONSTANT ROW")
                    && !name.ends_with("CONSTANT ROWS")
                    && !plan
                        .iter()
                        .any(|(_, _, _, step)| step == &format!("CO-ROUTINE {name}"))
            });
            assert!(
                !table_scan && !detail.contains("inference_runs_status_idx"),
                "recall query must reach rows through keyed lookups: {plan:?}\n{sql}"
            );
            if is_previews {
                assert!(
                    !detail.contains("interaction_observations_principal_activity_idx"),
                    "previews query must look up requested ids by primary key, not scan principal activity: {plan:?}\n{sql}"
                );
            }
        }
    }
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_recall_contract() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    let result = contract(&fixture).await;
    fixture.close().await?;
    result
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_recall_contract() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    let result = contract(&fixture).await;
    fixture.close().await?;
    result
}
