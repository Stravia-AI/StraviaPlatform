//! 读取侧公开接口测试：写入走 `RunObserver` 公共路径，读取走 Core 查询 API。
//!
//! 同一组场景分别跑 SQLite 与（显式配置时）PostgreSQL，保证两个后端的读取契约一致。

use std::sync::Arc;

use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role};

use super::*;
use crate::content_store::Content;

/// 超过任何旧截断上限（4096）的输入/输出文本。
fn long_text(tag: &str) -> String {
    format!("{tag}:{}", "x".repeat(6000))
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

fn admit(
    observation: &InteractionObservation,
    id: &str,
    principal: &str,
    route: &str,
    items: Vec<AiItem>,
    parent_generation: Option<&str>,
) -> RunObserver {
    observation
        .observe_ingress(IngressStart {
            id: id.into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: id.into(),
                principal: principal.into(),
                api_key_id: None,
                api_key_name: None,
                route_id: route.into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new("model", items)),
                has_new_user: parent_generation.is_none(),
                has_matching_pending_tool_result: false,
                generation_root_id: parent_generation.map(|_| "root-generation".into()),
                generation_parent_id: parent_generation.map(str::to_owned),
            },
        )
}

fn finish(run: &RunObserver, status: &str, generation: Option<&str>) {
    run.finish(RunOutcome {
        client_output_committed: status == "completed" || status == "waiting_client",
        delivery: None,
        delivery_completed_at: Some(writer::now()),
        status: status.into(),
        terminal_reason: None,
        generation_node_id: generation.map(str::to_owned),
        generation_root_id: generation.map(|_| "root-generation".into()),
    });
}

fn visible(id: &str, text: &str) -> RunEvent {
    RunEvent::ClientVisibleContent {
        text: text.into(),
        parts: vec![json!({"type": "text", "text": text})],
        block_id: id.into(),
        item: json!({"id": id, "role": "assistant", "content": text}),
        complete: true,
    }
}

/// 一个完整的模型轮次：首个尝试与轮次首尾合并，用量随尝试终态落库。
fn record_turn(run: &RunObserver, turn: &str, attempt: &str, answer: &str) {
    run.record(RunEvent::ModelTurnStarted {
        retention_detached: false,
        model_turn_id: turn.into(),
        route_id: "route".into(),
        model_display_name: None,
        estimated_input_tokens: None,
    });
    run.record(RunEvent::TargetAttemptStarted {
        model_turn_id: turn.into(),
        attempt_id: attempt.into(),
        target_id: "target".into(),
        provider_id: "provider".into(),
        provider_name: "Provider".into(),
        upstream_model: "model".into(),
        protocol: "responses".into(),
        upstream_url: "http://127.0.0.1".into(),
        cost_basis: None,
    });
    run.record(visible(&format!("{attempt}-block"), answer));
    run.record(RunEvent::TargetAttemptFinished {
        model_turn_id: turn.into(),
        attempt_id: attempt.into(),
        status: "completed".into(),
        status_code: Some(200),
        error_code: None,
        error: None,
        duration_ms: 100,
        first_token_ms: Some(10),
        usage: Some(ConfirmedUsage {
            input_tokens: Some(11),
            output_tokens: Some(7),
            ..Default::default()
        }),
        amounts: None,
    });
    run.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: turn.into(),
        status: "completed".into(),
    });
}

fn summary_by_route<'a>(forest: &'a ForestPage, route: &str) -> &'a InteractionSummary {
    forest
        .roots
        .iter()
        .flat_map(|root| &root.interactions)
        .find(|interaction| interaction.first_route_id == route)
        .unwrap_or_else(|| panic!("interaction for route {route}"))
}

fn all_events(page: &InteractionEventsPage) -> Vec<&ObservationEvent> {
    page.runs.iter().flat_map(|run| &run.events).collect()
}

/// 为第二个主体登记一条不被任何 Observation 引用的正文，模拟只被 Turn / Marker 引用的内容。
async fn intern_unreferenced(
    observation: &InteractionObservation,
    principal: &str,
    text: &str,
) -> anyhow::Result<i64> {
    let mut contents = [Content::text(text)];
    let ids = match &observation.inner.store {
        store::ObservationStore::Sqlite(pool, _) => {
            let mut tx = pool.begin().await?;
            let ids = content_refs::intern_sqlite(&mut tx, principal, &mut contents).await?;
            tx.commit().await?;
            ids
        }
        store::ObservationStore::Postgres(pool, _) => {
            let mut tx = pool.begin().await?;
            let ids = content_refs::intern_postgres(&mut tx, principal, &mut contents).await?;
            tx.commit().await?;
            ids
        }
    };
    Ok(ids[0])
}

struct Seeded {
    input: String,
    output: String,
    other_output: String,
}

/// 两个主体各一条 Interaction；A 带超过旧上限的输入与输出。
async fn seed_text(observation: &InteractionObservation) -> anyhow::Result<Seeded> {
    let input = long_text("input");
    let output = long_text("output");
    let other_output = long_text("beta-output");

    let run = admit(
        observation,
        "read-a",
        "reader-a",
        "route-a",
        vec![user_item(&input)],
        None,
    );
    run.capture_input_preview(&[user_item(&input)]);
    run.publish_input_preview();
    record_turn(&run, "turn-a", "attempt-a", &output);
    finish(&run, "completed", None);

    let other = admit(
        observation,
        "read-b",
        "reader-b",
        "route-b",
        vec![user_item("beta question")],
        None,
    );
    other.capture_input_preview(&[user_item("beta question")]);
    other.publish_input_preview();
    record_turn(&other, "turn-b", "attempt-b", &other_output);
    finish(&other, "completed", None);

    observation.flush().await?;
    Ok(Seeded {
        input,
        output,
        other_output,
    })
}

async fn summaries_carry_refs_and_raw_len(
    observation: &InteractionObservation,
    seeded: &Seeded,
) -> anyhow::Result<()> {
    let forest = observation.query_forest(ForestQuery::default()).await?;
    let a = summary_by_route(&forest, "route-a");
    assert_eq!(a.input.refs.len(), 1);
    assert_eq!(a.input.raw_len, seeded.input.len() as i64);
    assert_eq!(a.input.refs[0].raw_len, seeded.input.len() as i64);
    assert_eq!(a.output.refs.len(), 1);
    assert_eq!(a.output.raw_len, seeded.output.len() as i64);

    let b = summary_by_route(&forest, "route-b");
    assert_eq!(b.input.raw_len, "beta question".len() as i64);
    assert_eq!(b.output.raw_len, seeded.other_output.len() as i64);
    assert_ne!(a.input.refs[0].content_id, b.input.refs[0].content_id);

    // 列表只有引用与长度，不得夹带正文。
    let wire = serde_json::to_string(&forest)?;
    assert!(
        !wire.contains(&seeded.input),
        "list must not embed input text"
    );
    assert!(
        !wire.contains(&seeded.output),
        "list must not embed output text"
    );
    assert!(wire.contains("\"raw_len\""));
    for banned in ["input_preview", "visible_tail", "truncated"] {
        assert!(!wire.contains(banned), "removed field {banned} leaked");
    }
    Ok(())
}

async fn detail_returns_full_text(
    observation: &InteractionObservation,
    seeded: &Seeded,
) -> anyhow::Result<()> {
    let forest = observation.query_forest(ForestQuery::default()).await?;
    let id = summary_by_route(&forest, "route-a").id.clone();
    let detail = observation
        .get_interaction(&id, ForestQuery::default())
        .await?
        .expect("interaction detail");
    assert!(seeded.input.len() > 4096);
    assert_eq!(detail.input_text, seeded.input);
    assert_eq!(detail.output_text, seeded.output);
    Ok(())
}

async fn events_use_merged_turn_projection(
    observation: &InteractionObservation,
    seeded: &Seeded,
) -> anyhow::Result<()> {
    let forest = observation.query_forest(ForestQuery::default()).await?;
    let id = summary_by_route(&forest, "route-a").id.clone();
    let page = observation
        .get_interaction_events(
            &id,
            InteractionEventsQuery {
                after_sequence: Some(0),
                ..Default::default()
            },
        )
        .await?
        .expect("events page");
    let events = all_events(&page);
    let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();

    let input = events
        .iter()
        .find(|event| event.kind == "user_input_recorded")
        .expect("user_input_recorded");
    assert_eq!(input.payload["text"], seeded.input);

    // 首个尝试并入轮次首尾：不再有独立的尝试开始/结束行。
    let started = events
        .iter()
        .find(|event| event.kind == "model_turn_started")
        .expect("model_turn_started");
    assert_eq!(started.payload["attempt"]["attempt_id"], "attempt-a");
    assert!(!kinds.contains(&"target_attempt_started"), "{kinds:?}");
    assert!(!kinds.contains(&"target_attempt_finished"), "{kinds:?}");
    let finished = events
        .iter()
        .find(|event| event.kind == "model_turn_finished")
        .expect("model_turn_finished");
    assert_eq!(finished.payload["attempt"]["attempt_id"], "attempt-a");
    assert_eq!(finished.payload["attempt"]["status"], "completed");
    assert_eq!(finished.payload["usage"]["input_tokens"], 11);

    let text = events
        .iter()
        .find(|event| event.kind == "client_visible_content")
        .expect("client_visible_content");
    assert_eq!(text.payload["text"], seeded.output);

    // 无载荷 kind 的字段来自 Run 列。
    let admitted = events
        .iter()
        .find(|event| event.kind == "run_admitted")
        .expect("run_admitted");
    assert_eq!(admitted.payload["has_new_user"], true);
    assert_eq!(admitted.payload["grouping_reason"], "new_root");
    Ok(())
}

async fn contents_are_visible_only_through_observation_refs(
    observation: &InteractionObservation,
    seeded: &Seeded,
) -> anyhow::Result<()> {
    let forest = observation.query_forest(ForestQuery::default()).await?;
    let a = summary_by_route(&forest, "route-a");
    let input_id = a.input.refs[0].content_id;
    let output_id = a.output.refs[0].content_id;

    // 其他主体 scope 里没有任何 Observation 引用的正文（等价于只被 Turn / Marker 引用）。
    let orphan = intern_unreferenced(observation, "turn-only-principal", "turn only body").await?;
    let missing = 9_000_000_000_i64;

    let contents = observation
        .observation_contents(&[output_id, orphan, input_id, missing, output_id])
        .await?;
    let ids: Vec<i64> = contents.iter().map(|content| content.content_id).collect();
    assert_eq!(
        ids,
        vec![output_id, input_id],
        "未引用/未知/重复 id 必须被忽略并保持请求顺序"
    );
    assert_eq!(contents[0].body, json!(seeded.output));
    assert_eq!(contents[0].raw_len, seeded.output.len() as i64);
    assert_eq!(contents[0].format, "text");
    assert_eq!(contents[1].body, json!(seeded.input));

    let owners = observation
        .content_interactions(&[input_id, output_id, orphan])
        .await?;
    assert_eq!(owners[&input_id], vec![a.id.clone()]);
    assert_eq!(owners[&output_id], vec![a.id.clone()]);
    assert!(!owners.contains_key(&orphan));

    let none = observation.observation_contents(&[orphan, missing]).await?;
    assert!(none.is_empty());
    Ok(())
}

/// 客户端批量工具返回：按顺序给出 `results[]`；文本与 JSON 正文都还原成调用方看到的 JSON 值。
async fn client_tool_results_are_batched(
    observation: &InteractionObservation,
) -> anyhow::Result<()> {
    let run = admit(
        observation,
        "tool-run",
        "reader-tools",
        "route-tools",
        vec![user_item("run tools")],
        None,
    );
    run.record(RunEvent::ClientToolResult {
        tool_id: "a".into(),
        content: json!("first result"),
        is_error: false,
    });
    run.record(RunEvent::ClientToolResult {
        tool_id: "b".into(),
        content: json!({"code": 7, "reason": "tool failed"}),
        is_error: true,
    });
    finish(&run, "completed", None);
    observation.flush().await?;

    let forest = observation.query_forest(ForestQuery::default()).await?;
    let id = summary_by_route(&forest, "route-tools").id.clone();
    let page = observation
        .get_interaction_events(
            &id,
            InteractionEventsQuery {
                after_sequence: Some(0),
                ..Default::default()
            },
        )
        .await?
        .expect("events page");
    let results: Vec<Value> = all_events(&page)
        .into_iter()
        .filter(|event| event.kind == "client_tool_result")
        .flat_map(|event| {
            event.payload["results"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        results,
        vec![
            json!({"tool_id": "a", "content": "first result", "is_error": false}),
            json!({"tool_id": "b", "content": {"code": 7, "reason": "tool failed"}, "is_error": true}),
        ]
    );

    // JSON 单元与文本单元都能经内容端点按 id 取回，格式标签区分二者。
    let result_ids: Vec<i64> = {
        let wanted = observation_tool_result_content_ids(observation, &id).await?;
        assert_eq!(wanted.len(), 2);
        wanted
    };
    let bodies = observation.observation_contents(&result_ids).await?;
    assert_eq!(bodies.len(), 2);
    let formats: Vec<&str> = bodies.iter().map(|body| body.format.as_str()).collect();
    assert!(
        formats.contains(&"text") && formats.contains(&"json"),
        "{formats:?}"
    );
    Ok(())
}

async fn observation_tool_result_content_ids(
    observation: &InteractionObservation,
    interaction_id: &str,
) -> anyhow::Result<Vec<i64>> {
    let ids = match &observation.inner.store {
        store::ObservationStore::Sqlite(pool, _) => sqlx::query_scalar(
            "SELECT tr.content_id FROM observation_tool_results tr JOIN observation_events e ON e.sequence=tr.sequence WHERE e.interaction_id=? ORDER BY tr.sequence,tr.ordinal",
        )
        .bind(interaction_id)
        .fetch_all(pool)
        .await?,
        store::ObservationStore::Postgres(pool, _) => sqlx::query_scalar(
            "SELECT tr.content_id FROM observation_tool_results tr JOIN observation_events e ON e.sequence=tr.sequence WHERE e.interaction_id=$1 ORDER BY tr.sequence,tr.ordinal",
        )
        .bind(interaction_id)
        .fetch_all(pool)
        .await?,
    };
    Ok(ids)
}

async fn run_read_contract(observation: &InteractionObservation) -> anyhow::Result<()> {
    let seeded = seed_text(observation).await?;
    summaries_carry_refs_and_raw_len(observation, &seeded).await?;
    detail_returns_full_text(observation, &seeded).await?;
    events_use_merged_turn_projection(observation, &seeded).await?;
    contents_are_visible_only_through_observation_refs(observation, &seeded).await?;
    client_tool_results_are_batched(observation).await
}

#[tokio::test]
async fn sqlite_read_contract() -> anyhow::Result<()> {
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
    let result = run_read_contract(&observation).await;
    observation.shutdown().await;
    pool.close().await;
    result
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_read_contract() -> anyhow::Result<()> {
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
        .expect("set STRAVIA_TEST_POSTGRES_URL to run PostgreSQL read contracts");
    let directory = tempfile::tempdir()?;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("stravia_read_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    let result = async {
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
        let result = run_read_contract(&observation).await;
        observation.shutdown().await;
        result
    }
    .await;
    pool.close().await;
    let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await;
    admin.close().await;
    cleanup?;
    result
}
