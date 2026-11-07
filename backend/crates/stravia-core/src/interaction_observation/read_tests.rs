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
    admit_with(
        observation,
        id,
        principal,
        route,
        items,
        parent_generation,
        parent_generation.is_none(),
    )
}

fn admit_with(
    observation: &InteractionObservation,
    id: &str,
    principal: &str,
    route: &str,
    items: Vec<AiItem>,
    parent_generation: Option<&str>,
    has_new_user: bool,
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
                has_new_user,
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
        block_id: super::types::canonical_item_block_id(id, 0),
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
    run.capture_user_input(&[user_item(&input)]);
    run.publish_user_input();
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
    other.capture_user_input(&[user_item("beta question")]);
    other.publish_user_input();
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
    // cache-read 未知，管理读取边界不暴露总输入 11 作为净输入。
    assert!(finished.payload["usage"]["input_tokens"].is_null());
    assert_eq!(finished.payload["usage"]["output_tokens"], 7);

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
    // A journal-only body in the same scope as an observed interaction must not
    // become visible by guessing its ID. A body also referenced by observation
    // remains visible, even when a journal references it too.
    let private = intern_unreferenced(observation, "reader-a", "private journal body").await?;
    let insert = "INSERT INTO vendor_atomic_journal_nodes \
        (node_id, principal, vendor_id, provider_id, data_epoch, content_id, logical_size, document_size) \
        VALUES ($1, 'api-key:reader-a', 'vendor', 'provider', 0, $2, 1, 1)";
    match &observation.inner.store {
        store::ObservationStore::Sqlite(pool, _) => {
            sqlx::query(insert)
                .bind("private-test-node")
                .bind(private)
                .execute(pool)
                .await?;
            sqlx::query(insert)
                .bind("shared-test-node")
                .bind(output_id)
                .execute(pool)
                .await?;
        }
        store::ObservationStore::Postgres(pool, _) => {
            sqlx::query(insert)
                .bind("private-test-node")
                .bind(private)
                .execute(pool)
                .await?;
            sqlx::query(insert)
                .bind("shared-test-node")
                .bind(output_id)
                .execute(pool)
                .await?;
        }
    }
    let missing = 9_000_000_000_i64;

    let contents = observation
        .observation_contents(&[output_id, orphan, private, input_id, missing, output_id])
        .await?;
    let ids: Vec<i64> = contents
        .iter()
        .map(|visible| visible.content.content_id)
        .collect();
    assert_eq!(
        ids,
        vec![output_id, input_id],
        "未引用/未知/重复 id 必须被忽略并保持请求顺序"
    );
    assert_eq!(contents[0].content.body, json!(seeded.output));
    assert_eq!(contents[0].content.raw_len, seeded.output.len() as i64);
    assert_eq!(contents[0].content.format, ObservationContentFormat::Text);
    assert_eq!(contents[1].content.body, json!(seeded.input));

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

/// 内容 id 的校验归 Core：空、含非正数或超过上限都是 `InvalidContentIds`。
async fn content_ids_are_validated_by_the_query(
    observation: &InteractionObservation,
) -> anyhow::Result<()> {
    let too_many: Vec<i64> = (1..=MAX_OBSERVATION_CONTENT_IDS as i64 + 1).collect();
    for ids in [&[][..], &[1, 0], &[-3], &too_many] {
        let error = observation
            .observation_contents(ids)
            .await
            .err()
            .expect("非法 id 必须被拒绝");
        assert!(
            matches!(
                error.downcast_ref::<ObservationQueryError>(),
                Some(ObservationQueryError::InvalidContentIds)
            ),
            "{} 个 id: {error}",
            ids.len()
        );
    }
    // 上限本身可接受；这些 id 不存在，因此结果为空。
    let at_cap: Vec<i64> = (9_000_000_000..)
        .take(MAX_OBSERVATION_CONTENT_IDS)
        .collect();
    assert!(observation.observation_contents(&at_cap).await?.is_empty());
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
    let formats: Vec<ObservationContentFormat> = bodies
        .iter()
        .map(|visible| visible.content.format)
        .collect();
    assert!(
        formats.contains(&ObservationContentFormat::Text)
            && formats.contains(&ObservationContentFormat::Json),
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

fn usage(input: Option<i64>, output: Option<i64>, cache_read: Option<i64>) -> ConfirmedUsage {
    ConfirmedUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        ..Default::default()
    }
}

fn started_turn(run: &RunObserver, turn: &str, attempt: &str) {
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
}

fn finished_attempt(
    turn: &str,
    attempt: &str,
    status: &str,
    usage: Option<ConfirmedUsage>,
) -> RunEvent {
    RunEvent::TargetAttemptFinished {
        model_turn_id: turn.into(),
        attempt_id: attempt.into(),
        status: status.into(),
        status_code: Some(if status == "completed" { 200 } else { 502 }),
        error_code: None,
        error: None,
        duration_ms: 100,
        first_token_ms: None,
        usage,
        amounts: None,
    }
}

async fn run_events_of(
    observation: &InteractionObservation,
    route: &str,
) -> anyhow::Result<(InteractionSummary, Vec<ObservationEvent>)> {
    let forest = observation.query_forest(ForestQuery::default()).await?;
    let summary = summary_by_route(&forest, route).clone();
    let page = observation
        .get_interaction_events(
            &summary.id,
            InteractionEventsQuery {
                after_sequence: Some(0),
                ..Default::default()
            },
        )
        .await?
        .expect("events page");
    let events = all_events(&page).into_iter().cloned().collect();
    Ok((summary, events))
}

/// 管理读取边界对每个 attempt 的 usage 只投影一次净输入（总输入 − cache-read，cache-read
/// 未知则为 null）：独立的 attempt 结束行与并入 `model_turn_finished` 的最后一个 attempt
/// （`payload.usage` 与 `payload.attempt.usage`）口径一致，也与摘要（来自 attempt 行列）一致。
async fn merged_turn_finish_usage_uses_management_net_input(
    observation: &InteractionObservation,
) -> anyhow::Result<()> {
    // 迟到的 UsageConfirmed 补齐输入与 cache-read：合并后净输入 = 3 - 1。
    let late = admit(
        observation,
        "usage-late",
        "usage-reader",
        "route-usage-late",
        vec![user_item("late usage")],
        None,
    );
    started_turn(&late, "late-turn", "late-attempt");
    late.record(finished_attempt(
        "late-turn",
        "late-attempt",
        "completed",
        Some(usage(None, Some(2), None)),
    ));
    late.record(RunEvent::UsageConfirmed {
        model_turn_id: "late-turn".into(),
        attempt_id: "late-attempt".into(),
        usage: usage(Some(3), None, Some(1)),
    });
    late.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: "late-turn".into(),
        status: "completed".into(),
    });
    finish(&late, "completed", None);

    // cache-read 未知：总输入 3 不能当作净输入暴露。
    let unknown = admit(
        observation,
        "usage-unknown",
        "usage-reader",
        "route-usage-unknown",
        vec![user_item("unknown cache")],
        None,
    );
    started_turn(&unknown, "unknown-turn", "unknown-attempt");
    unknown.record(finished_attempt(
        "unknown-turn",
        "unknown-attempt",
        "completed",
        Some(usage(Some(3), Some(2), None)),
    ));
    unknown.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: "unknown-turn".into(),
        status: "completed".into(),
    });
    finish(&unknown, "completed", None);

    // 故障转移：被取代的 attempt 保留独立结束行，最后一个 attempt 并入 Turn 结束行。
    let failover = admit(
        observation,
        "usage-failover",
        "usage-reader",
        "route-usage-failover",
        vec![user_item("failover")],
        None,
    );
    started_turn(&failover, "failover-turn", "failover-first");
    failover.record(finished_attempt(
        "failover-turn",
        "failover-first",
        "failed",
        Some(usage(Some(20), Some(1), Some(5))),
    ));
    failover.record(RunEvent::TargetAttemptStarted {
        model_turn_id: "failover-turn".into(),
        attempt_id: "failover-second".into(),
        target_id: "target".into(),
        provider_id: "provider".into(),
        provider_name: "Provider".into(),
        upstream_model: "model".into(),
        protocol: "responses".into(),
        upstream_url: "http://127.0.0.1".into(),
        cost_basis: None,
    });
    failover.record(finished_attempt(
        "failover-turn",
        "failover-second",
        "completed",
        Some(usage(Some(100), Some(7), Some(40))),
    ));
    failover.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: "failover-turn".into(),
        status: "completed".into(),
    });
    finish(&failover, "completed", None);
    observation.flush().await?;

    let (summary, events) = run_events_of(observation, "route-usage-late").await?;
    let turn = events
        .iter()
        .find(|event| event.kind == "model_turn_finished")
        .expect("model_turn_finished");
    assert_eq!(turn.payload["usage"]["input_tokens"], 2, "{turn:?}");
    assert_eq!(turn.payload["usage"], turn.payload["attempt"]["usage"]);
    assert_eq!(turn.payload["usage"]["output_tokens"], 2);
    assert_eq!(summary.usage.input_tokens, Some(2));
    assert_eq!(summary.usage.output_tokens, Some(2));

    let (summary, events) = run_events_of(observation, "route-usage-unknown").await?;
    let turn = events
        .iter()
        .find(|event| event.kind == "model_turn_finished")
        .expect("model_turn_finished");
    assert!(turn.payload["usage"]["input_tokens"].is_null(), "{turn:?}");
    assert_eq!(turn.payload["usage"], turn.payload["attempt"]["usage"]);
    assert_eq!(turn.payload["usage"]["output_tokens"], 2);
    assert_eq!(summary.usage.input_tokens, None);
    assert_eq!(
        summary
            .usage
            .coverage
            .as_ref()
            .unwrap()
            .missing_input_tokens,
        1
    );

    let (summary, events) = run_events_of(observation, "route-usage-failover").await?;
    let separate = events
        .iter()
        .find(|event| event.kind == "target_attempt_finished")
        .expect("superseded attempt keeps its own row");
    assert_eq!(separate.payload["usage"]["input_tokens"], 15);
    let turn = events
        .iter()
        .find(|event| event.kind == "model_turn_finished")
        .expect("model_turn_finished");
    assert_eq!(turn.payload["usage"]["input_tokens"], 60);
    assert_eq!(turn.payload["usage"], turn.payload["attempt"]["usage"]);
    assert_eq!(summary.usage.input_tokens, Some(75));
    Ok(())
}

/// 编辑历史用户消息后产生的子 Interaction：它的首个 Run 带着指向父 Interaction 的
/// `parent_run_id`，但仍是该子 Interaction 自己的根 Run；详情的完整输入必须与摘要引用一致。
async fn branch_child_detail_carries_its_own_input_text(
    observation: &InteractionObservation,
) -> anyhow::Result<()> {
    use stravia_runtime_contract::protocol::ir::ToolCall;

    let first = vec![user_item("branch original question")];
    let parent = admit(
        observation,
        "branch-parent",
        "branch-reader",
        "route-branch-parent",
        first.clone(),
        None,
    );
    parent.capture_user_input(&first);
    parent.publish_user_input();
    parent.record(RunEvent::ClientToolHandoff {
        tool_id: "branch-call".into(),
        name: "probe".into(),
        input: None,
    });
    // 父分支的交付早于子请求超过合并窗口，新的用户输入才会分叉为子 Interaction。
    parent.finish(RunOutcome {
        client_output_committed: true,
        delivery: None,
        delivery_completed_at: Some(writer::now() - 60_000),
        status: "waiting_client".into(),
        terminal_reason: None,
        generation_node_id: Some("branch-node".into()),
        generation_root_id: Some("root-generation".into()),
    });
    observation.flush().await?;

    let edited = vec![
        user_item("branch original question"),
        AiItem::function_call(ToolCall {
            id: "branch-call".into(),
            name: "probe".into(),
            arguments: "{}".into(),
        }),
        AiItem::function_call_output("branch-call", json!("ok")),
        user_item("branch edited question"),
    ];
    let child = admit_with(
        observation,
        "branch-child",
        "branch-reader",
        "route-branch-child",
        edited.clone(),
        Some("branch-node"),
        true,
    );
    child.capture_user_input(&edited);
    child.capture_client_tool_results(&edited);
    child.publish_user_input();
    record_turn(&child, "branch-turn", "branch-attempt", "branch answer");
    finish(&child, "completed", None);
    observation.flush().await?;

    let forest = observation.query_forest(ForestQuery::default()).await?;
    let parent_summary = summary_by_route(&forest, "route-branch-parent");
    let summary = summary_by_route(&forest, "route-branch-child");
    assert_eq!(
        summary.parent_interaction_id.as_deref(),
        Some(parent_summary.id.as_str()),
        "新的用户输入分叉为子 Interaction"
    );
    assert_eq!(summary.input.refs.len(), 1, "{summary:?}");
    let detail = observation
        .get_interaction(&summary.id, ForestQuery::default())
        .await?
        .expect("child detail");
    assert_eq!(detail.input_text, "branch edited question");
    let contents = observation
        .observation_contents(&[summary.input.refs[0].content_id])
        .await?;
    assert_eq!(contents[0].content.body, json!(detail.input_text));
    Ok(())
}

async fn run_read_contract(observation: &InteractionObservation) -> anyhow::Result<()> {
    let seeded = seed_text(observation).await?;
    summaries_carry_refs_and_raw_len(observation, &seeded).await?;
    detail_returns_full_text(observation, &seeded).await?;
    events_use_merged_turn_projection(observation, &seeded).await?;
    content_ids_are_validated_by_the_query(observation).await?;
    contents_are_visible_only_through_observation_refs(observation, &seeded).await?;
    client_tool_results_are_batched(observation).await
}

/// 一个后端上的 Observation 夹具；`close` 回收连接与（PostgreSQL 的）隔离 schema。
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
                let cleanup =
                    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
                        .execute(&admin)
                        .await;
                admin.close().await;
                cleanup?;
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn sqlite_read_contract() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    let result = run_read_contract(&fixture.observation).await;
    fixture.close().await?;
    result
}

#[tokio::test]
async fn sqlite_merged_turn_finish_usage_uses_management_net_input() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    let result = merged_turn_finish_usage_uses_management_net_input(&fixture.observation).await;
    fixture.close().await?;
    result
}

#[tokio::test]
async fn sqlite_branch_child_detail_carries_its_own_input_text() -> anyhow::Result<()> {
    let fixture = Fixture::sqlite().await?;
    let result = branch_child_detail_carries_its_own_input_text(&fixture.observation).await;
    fixture.close().await?;
    result
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_read_contract() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    let result = run_read_contract(&fixture.observation).await;
    fixture.close().await?;
    result
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_merged_turn_finish_usage_uses_management_net_input() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    let result = merged_turn_finish_usage_uses_management_net_input(&fixture.observation).await;
    fixture.close().await?;
    result
}

#[tokio::test]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn postgres_branch_child_detail_carries_its_own_input_text() -> anyhow::Result<()> {
    let fixture = Fixture::postgres().await?;
    let result = branch_child_detail_carries_its_own_input_text(&fixture.observation).await;
    fixture.close().await?;
    result
}
