//! 历史回看的读取契约：写入走 `RunObserver` 公共路径，读取走 `Recall::read`。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use stravia_credential_protection::marker;
use stravia_credential_protection::store::{
    DetectedSecret, InternedMappings, Mapping, MappingStore,
};
use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, MessageContent, Role, ToolCall};
use stravia_runtime_contract::redaction::RedactionError;
use stravia_web_access_contract::read_path::{ReadInput, ReadTarget, parse_read_input};

use super::*;
use crate::interaction_observation::{
    AdmissionFacts, IngressStart, RunEvent, RunObserver, RunOutcome, RunStart,
    canonical_item_block_id,
};

const OWNER: &str = "history-owner";
const OTHER: &str = "history-other";
const SECRET: &str = "PlainCredentialValue42";
const ANSWER_LINES: usize = 300;
/// 新对话的 Generation 根就是首个节点自身，后续子节点沿用同一根，Observation 据此归入同一链路。
const ROOT_NODE: &str = "node-a";

fn reference() -> String {
    format!(
        "{}{}{}",
        marker::PREFIX,
        "q".repeat(marker::IDENTIFIER_LEN),
        marker::SUFFIX
    )
}

/// 只为 OWNER 提供一条映射；历史回看只读取映射，不创建或还原。
struct OwnerMappings;

#[async_trait]
impl MappingStore for OwnerMappings {
    async fn active(&self, principal: &Principal) -> Result<Vec<Mapping>, RedactionError> {
        Ok(if principal.api_key_id() == OWNER {
            vec![Mapping {
                reference: reference(),
                secret: SECRET.to_owned(),
                rule_ids: Default::default(),
            }]
        } else {
            Vec::new()
        })
    }

    async fn intern(
        &self,
        _principal: &Principal,
        _secrets: &[DetectedSecret],
    ) -> Result<InternedMappings, RedactionError> {
        unreachable!("history recall never creates mappings")
    }

    async fn publish(
        &self,
        _principal: &Principal,
        _references: &[String],
    ) -> Result<(), RedactionError> {
        unreachable!("history recall never publishes mappings")
    }

    async fn sweep(
        &self,
        _now: i64,
        _retention: std::time::Duration,
    ) -> Result<u64, RedactionError> {
        unreachable!("history recall never sweeps mappings")
    }

    async fn plaintext(&self, _reference: &str) -> Result<Option<String>, RedactionError> {
        unreachable!("history recall never restores plaintext")
    }
}

struct Fixture {
    observation: InteractionObservation,
    pool: sqlx::SqlitePool,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
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
            pool,
            _directory: directory,
        })
    }

    async fn recall(&self, principal: &str) -> Recall<'_> {
        let secrets = KnownSecrets::load(&OwnerMappings, &Principal::new(principal))
            .await
            .expect("mappings");
        Recall {
            observation: &self.observation,
            secrets,
            retention_days: 7,
            active_since: 0,
        }
    }

    async fn close(self) {
        self.observation.shutdown().await;
        self.pool.close().await;
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn user(text: &str) -> AiItem {
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
    run: &str,
    principal: &str,
    items: Vec<AiItem>,
    generation: Option<(&str, Option<&str>)>,
) -> RunObserver {
    let observer = observation
        .observe_ingress(IngressStart {
            id: run.into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        })
        .admit(
            RunStart {
                id: run.into(),
                principal: principal.into(),
                api_key_id: Some(principal.into()),
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: Some("recall-model".into()),
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: Arc::new(AiRequest::new("model", items.clone())),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: generation.map(|(root, _)| root.into()),
                generation_parent_id: generation.and_then(|(_, parent)| parent).map(str::to_owned),
            },
        );
    observer.capture_user_input(&items);
    observer.capture_client_tool_results(&items);
    observer.publish_user_input();
    observer
}

fn visible(id: &str, text: &str, complete: bool) -> RunEvent {
    RunEvent::ClientVisibleContent {
        text: text.into(),
        parts: vec![json!({"type": "text", "text": text})],
        block_id: canonical_item_block_id(id, 0),
        item: json!({"id": id, "role": "assistant", "content": text}),
        complete,
    }
}

fn finish(run: &RunObserver, status: &str, delivered_at: i64, node: Option<&str>) {
    run.finish(RunOutcome {
        client_output_committed: status == "completed" || status == "waiting_client",
        delivery: None,
        delivery_completed_at: Some(delivered_at),
        status: status.into(),
        terminal_reason: None,
        generation_node_id: node.map(str::to_owned),
        generation_root_id: node.map(|_| ROOT_NODE.into()),
    });
}

fn answer() -> String {
    (1..=ANSWER_LINES)
        .map(|line| format!("answer line {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一条 OWNER 链（根 A，两个从 A 分叉的子 Interaction B、C），一个 OWNER 的失败无输出 Interaction，
/// 以及一个 OTHER 的 Interaction。
async fn seed(fixture: &Fixture) -> anyhow::Result<()> {
    let observation = &fixture.observation;
    let first = vec![user(&format!("Deploy using {SECRET} please"))];
    let a = admit(
        observation,
        "run-a",
        OWNER,
        first.clone(),
        Some((ROOT_NODE, None)),
    );
    a.record(RunEvent::PlatformToolStarted {
        model_turn_id: "turn-a".into(),
        tool_id: "platform-1".into(),
        name: "exec_command".into(),
        input: Some(json!({"cmd": format!("deploy --token {SECRET}")})),
    });
    let output = (1..=30)
        .map(|line| format!("deploy output {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    a.record(RunEvent::PlatformToolFinished {
        model_turn_id: "turn-a".into(),
        tool_id: "platform-1".into(),
        status: "completed".into(),
        duration_ms: 5,
        content: Some(json!(output)),
    });
    a.record(visible("draft", "interrupted draft", false));
    a.record(visible("answer", &answer(), true));
    a.record(RunEvent::ClientToolHandoff {
        tool_id: "call-a".into(),
        name: "lookup".into(),
        input: Some(json!({"query": "status"})),
    });
    // 父分支的交付早于子请求超过合并窗口，新的用户输入才会分叉为子 Interaction。
    finish(&a, "waiting_client", now() - 60_000, Some(ROOT_NODE));
    observation.flush().await?;

    for (run, question) in [("run-b", "B question"), ("run-c", "C question")] {
        let items = vec![
            first[0].clone(),
            AiItem::function_call(ToolCall {
                id: "call-a".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            }),
            AiItem::function_call_output("call-a", json!("ok")),
            user(question),
        ];
        let child = admit(
            observation,
            run,
            OWNER,
            items,
            Some((ROOT_NODE, Some(ROOT_NODE))),
        );
        child.record(visible(run, &format!("{question} answered"), true));
        finish(&child, "completed", now(), None);
        observation.flush().await?;
    }

    let failed = admit(
        observation,
        "run-failed",
        OWNER,
        vec![user("never answered")],
        None,
    );
    finish(&failed, "failed", now(), None);
    let other = admit(
        observation,
        "run-other",
        OTHER,
        vec![user("other question")],
        None,
    );
    other.record(visible("other", "other answer", true));
    finish(&other, "completed", now(), None);
    observation.flush().await?;
    Ok(())
}

async fn read(
    recall: &Recall<'_>,
    principal: &str,
    path: &str,
) -> Result<Value, PlatformToolError> {
    let input = ReadInput {
        path: path.to_owned(),
        question: None,
        attachments: None,
        allowed_domains: None,
        wait: None,
    };
    let ReadTarget::History(history) = parse_read_input(&input).expect("history path") else {
        panic!("{path} is not a history path");
    };
    recall.read(principal, &history).await
}

fn content(page: &Value) -> &str {
    page["content"].as_str().expect("page content")
}

/// 按页读完一个文件，返回 (行号, 行) 列表；每页的 `returned_ranges` 必须与内容逐行对应。
async fn read_all(recall: &Recall<'_>, path: &str) -> Vec<(u64, String)> {
    let mut lines = Vec::new();
    let mut next = Some(path.to_owned());
    while let Some(path) = next.take() {
        let page = read(recall, OWNER, &path).await.expect("page");
        let mut text = content(&page).lines();
        for range in page["returned_ranges"].as_array().expect("ranges") {
            let start = range["start_line"].as_u64().unwrap();
            let end = range["end_line"].as_u64().unwrap();
            for number in start..=end {
                lines.push((number, text.next().expect("line for range").to_owned()));
            }
        }
        assert_eq!(text.next(), None, "content beyond returned ranges");
        assert_eq!(page["has_more"], json!(page.get("next_path").is_some()));
        next = page["next_path"].as_str().map(str::to_owned);
    }
    lines
}

#[tokio::test]
async fn history_parent_expiration_preserves_child_line_count() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    seed(&fixture).await?;
    let chains = fixture
        .observation
        .recall_chains(OWNER, 0, None, 10)
        .await?;
    let root = &chains.chains[0].root_id;
    let members = fixture
        .observation
        .recall_members(OWNER, root, 0, None, 10)
        .await?
        .unwrap();
    let parent = &members.members[0].id;
    let child = &members.members[1].id;
    let path = format!("stravia://history/{root}/{child}");
    let recall = fixture.recall(OWNER).await;
    let before = read_all(&recall, &path).await;
    assert!(
        before
            .iter()
            .any(|(_, line)| line == &format!("- parent: stravia://history/{root}/{parent}"))
    );
    // 单独推进父节点的活动时间，使用真实的保留期清理路径删除父节点。
    sqlx::query("UPDATE interaction_observations SET last_active_at = ? WHERE id = ?")
        .bind(now() - 120_000)
        .bind(parent)
        .execute(&fixture.pool)
        .await?;
    fixture
        .observation
        .sweep_history(now(), std::time::Duration::from_secs(60))
        .await?;
    let after = read_all(&recall, &path).await;
    assert_eq!(before.len(), after.len());
    assert!(after.iter().any(|(_, line)| line == "- parent: none"));
    assert_eq!(
        before.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        after.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_retention_sweep_and_clear_remove_readable_entries() -> anyhow::Result<()> {
    for clear in [false, true] {
        let fixture = Fixture::new().await?;
        let run = admit(
            &fixture.observation,
            "cleanup-run",
            OWNER,
            vec![user("cleanup question")],
            None,
        );
        run.record(visible("answer", "cleanup answer", true));
        finish(&run, "completed", now(), None);
        fixture.observation.flush().await?;
        let chains = fixture
            .observation
            .recall_chains(OWNER, 0, None, 10)
            .await?;
        let root = &chains.chains[0].root_id;
        let members = fixture
            .observation
            .recall_members(OWNER, root, 0, None, 10)
            .await?
            .unwrap();
        let chain = format!("stravia://history/{root}/");
        let transcript = format!("{chain}{}", members.members[0].id);
        let recall = fixture.recall(OWNER).await;
        assert!(content(&read(&recall, OWNER, "stravia://history/").await?).contains(&chain));
        assert!(read(&recall, OWNER, &transcript).await.is_ok());
        if clear {
            assert_eq!(
                fixture
                    .observation
                    .clear_history()
                    .await?
                    .deleted_interactions,
                1
            );
        } else {
            assert_eq!(
                fixture
                    .observation
                    .sweep_history(now() + 120_000, std::time::Duration::from_secs(60))
                    .await?,
                1
            );
        }
        let listing = read(&recall, OWNER, "stravia://history/").await?;
        assert!(!content(&listing).contains(&chain));
        for path in [&chain, &transcript] {
            assert_eq!(
                read(&recall, OWNER, path).await.unwrap_err().message,
                UNAVAILABLE
            );
        }
        drop(recall);
        fixture.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn history_renders_unmatched_tool_results_gaps_and_failures() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let run = admit(
        &fixture.observation,
        "diagnostic-run",
        OWNER,
        vec![user("question")],
        None,
    );
    run.record(visible("answer", "partial answer", true));
    run.record(RunEvent::ClientToolResult {
        tool_id: "unknown-call".into(),
        content: json!("orphan result"),
        is_error: true,
    });
    run.record(RunEvent::ObservationGap {
        reason: "missing stream segment".into(),
    });
    run.record(RunEvent::RequestFailed {
        error: crate::interaction_observation::FailureDiagnostic {
            source: Some("platform".into()),
            code: Some("EXAMPLE_ERROR".into()),
            message: Some("Example failure".into()),
            status_code: Some(500),
            upstream_code: None,
        },
    });
    run.finish(RunOutcome {
        client_output_committed: true,
        delivery: None,
        delivery_completed_at: Some(now()),
        status: "failed".into(),
        terminal_reason: None,
        generation_node_id: None,
        generation_root_id: None,
    });
    fixture.observation.flush().await?;
    let chains = fixture
        .observation
        .recall_chains(OWNER, 0, None, 10)
        .await?;
    let root = &chains.chains[0].root_id;
    let members = fixture
        .observation
        .recall_members(OWNER, root, 0, None, 10)
        .await?
        .unwrap();
    let path = format!("stravia://history/{root}/{}", members.members[0].id);
    let recall = fixture.recall(OWNER).await;
    let page = read(&recall, OWNER, &path).await?;
    let text = content(&page);
    assert!(
        text.contains("### tool result (unmatched call) · error\norphan result\n"),
        "{text}"
    );
    assert!(!text.contains("(full:"), "{text}");
    assert!(
        text.contains("> observation gap: missing stream segment"),
        "{text}"
    );
    assert!(
        text.contains("> run failed: Example failure (EXAMPLE_ERROR)"),
        "{text}"
    );
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_transcript_prefix_and_line_numbers_survive_appended_events() -> anyhow::Result<()>
{
    let fixture = Fixture::new().await?;
    let run = admit(
        &fixture.observation,
        "append-run",
        OWNER,
        vec![user("question")],
        None,
    );
    run.record(visible("first", "first answer", true));
    run.record(RunEvent::PlatformToolStarted {
        model_turn_id: "turn".into(),
        tool_id: "tool".into(),
        name: "lookup".into(),
        input: Some(json!({"query": "one"})),
    });
    fixture.observation.flush().await?;
    let chains = fixture
        .observation
        .recall_chains(OWNER, 0, None, 10)
        .await?;
    let root = &chains.chains[0].root_id;
    let members = fixture
        .observation
        .recall_members(OWNER, root, 0, None, 10)
        .await?
        .unwrap();
    let path = format!("stravia://history/{root}/{}", members.members[0].id);
    let recall = fixture.recall(OWNER).await;
    let before = read(&recall, OWNER, &path).await?;
    let before_lines = read_all(&recall, &path).await;
    run.record(RunEvent::PlatformToolFinished {
        model_turn_id: "turn".into(),
        tool_id: "tool".into(),
        status: "completed".into(),
        duration_ms: 1,
        content: Some(json!("tool output")),
    });
    run.record(visible("second", "second answer", true));
    fixture.observation.flush().await?;
    let after = read(&recall, OWNER, &path).await?;
    let after_lines = read_all(&recall, &path).await;
    assert!(content(&after).starts_with(content(&before)));
    assert_eq!(&after_lines[..before_lines.len()], before_lines.as_slice());
    assert!(content(&after).contains("tool output"));
    assert!(content(&after).contains("second answer"));
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_reads_owner_chains_transcripts_and_tool_files() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    seed(&fixture).await?;
    let recall = fixture.recall(OWNER).await;

    let root = read(&recall, OWNER, "stravia://history/")
        .await
        .expect("root");
    let listing = content(&root);
    assert!(
        listing.starts_with("# stravia://history/ · 1 chain · "),
        "{listing}"
    );
    assert!(
        !listing.contains("never answered"),
        "失败无输出的 Interaction 不列出: {listing}"
    );
    assert!(
        !listing.contains("other question"),
        "其他 Principal 不可见: {listing}"
    );
    assert!(
        !listing.contains(SECRET) && listing.contains(&reference()),
        "{listing}"
    );
    assert_eq!(root["has_more"], json!(false));

    let chains = fixture
        .observation
        .recall_chains(OWNER, 0, None, 10)
        .await?;
    let root_id = chains.chains[0].root_id.clone();
    let members = fixture
        .observation
        .recall_members(OWNER, &root_id, 0, None, 10)
        .await?
        .expect("chain");
    let ids: Vec<&str> = members
        .members
        .iter()
        .map(|member| member.id.as_str())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    let (a, b, c) = (ids[0], ids[1], ids[2]);

    let chain = read(&recall, OWNER, &format!("stravia://history/{root_id}/"))
        .await
        .expect("chain");
    let chain_lines: Vec<&str> = content(&chain).lines().collect();
    assert!(chain_lines[0].contains("3 interactions"), "{chain_lines:?}");
    assert!(
        !chain_lines[2].contains("branch from"),
        "B 紧跟父 A: {chain_lines:?}"
    );
    assert!(
        chain_lines[3].ends_with(&format!(" · branch from {a}")),
        "C 的父不是上一行: {chain_lines:?}"
    );

    let transcript = format!("stravia://history/{root_id}/{a}");
    let lines = read_all(&recall, &transcript).await;
    let numbers: Vec<u64> = lines.iter().map(|(number, _)| *number).collect();
    assert_eq!(
        numbers,
        (1..=lines.len() as u64).collect::<Vec<_>>(),
        "行号连续且不重复"
    );
    let text: Vec<&str> = lines.iter().map(|(_, line)| line.as_str()).collect();
    let joined = text.join("\n");
    assert_eq!(text[0], format!("# Interaction {a}"));
    assert!(!joined.contains(SECRET), "凭据在转录中替换为标记");
    assert!(joined.contains(&format!("Deploy using {} please", reference())));
    assert!(
        !joined.contains("interrupted draft"),
        "未完成的可见块不渲染"
    );
    assert!(joined.contains("### tool call 1 · exec_command (platform)"));
    assert!(joined.contains("### tool result 1 · completed"));
    assert!(joined.contains("deploy output 20") && !joined.contains("deploy output 21"));
    assert!(joined.contains(&format!("… (truncated)\n(full: {transcript}/tools/1)")));
    assert!(joined.contains("### tool call 2 · lookup (client)"));
    assert!(text.contains(&format!("answer line {ANSWER_LINES}").as_str()));

    // 选择直接取到与分页一致的行；`:-1` 是最后一行。
    let total = lines.len();
    let last = read(&recall, OWNER, &format!("{transcript}:-1"))
        .await
        .expect("last line");
    assert_eq!(content(&last), format!("{}\n", text[total - 1]));
    assert_eq!(
        last["returned_ranges"],
        json!([{"start_line": total, "end_line": total}])
    );
    let middle = read(&recall, OWNER, &format!("{transcript}:150+2"))
        .await
        .expect("range");
    assert_eq!(content(&middle), format!("{}\n{}\n", text[149], text[150]));
    let error = read(&recall, OWNER, &format!("{transcript}:{}", total + 1))
        .await
        .unwrap_err();
    assert_eq!(error.message, "Line selection is outside the history file");

    let tool = format!("{transcript}/tools/1");
    let tool_lines = read_all(&recall, &tool).await;
    let tool_text = tool_lines
        .iter()
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_text.starts_with("# Tool call 1 · exec_command · platform"),
        "{tool_text}"
    );
    assert!(tool_text.contains(&format!("\"cmd\": \"deploy --token {}\"", reference())));
    assert!(tool_text.contains("## result 1 · completed · 5 ms\ndeploy output 1\n"));
    assert!(tool_text.ends_with("deploy output 30"), "{tool_text}");
    assert!(!tool_text.contains(SECRET));

    for path in [
        format!("stravia://history/{root_id}/{b}"),
        format!("stravia://history/{root_id}/{c}"),
    ] {
        let child = read(&recall, OWNER, &path).await.expect("child transcript");
        assert!(
            content(&child).contains(&format!("- parent: stravia://history/{root_id}/{a}")),
            "{}",
            content(&child)
        );
    }

    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_hides_foreign_missing_and_mismatched_entries_uniformly() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    seed(&fixture).await?;
    let owner = fixture.recall(OWNER).await;
    let other = fixture.recall(OTHER).await;
    let chains = fixture
        .observation
        .recall_chains(OWNER, 0, None, 10)
        .await?;
    let root_id = chains.chains[0].root_id.clone();
    let members = fixture
        .observation
        .recall_members(OWNER, &root_id, 0, None, 10)
        .await?
        .expect("chain");
    let a = members.members[0].id.clone();
    let b = members.members[1].id.clone();
    let transcript = format!("stravia://history/{root_id}/{a}");

    // 其他 Principal、不存在的 id、错配的 root 与不存在的工具编号返回同一个错误。
    for (recall, principal, path) in [
        (&other, OTHER, format!("stravia://history/{root_id}/")),
        (&other, OTHER, transcript.clone()),
        (&other, OTHER, format!("{transcript}/tools/1")),
        (&owner, OWNER, "stravia://history/missing-root/".to_owned()),
        (
            &owner,
            OWNER,
            format!("stravia://history/{root_id}/missing"),
        ),
        (&owner, OWNER, format!("stravia://history/{b}/{a}")),
        (&owner, OWNER, format!("{transcript}/tools/9")),
    ] {
        let error = read(recall, principal, &path).await.unwrap_err();
        assert_eq!(error.message, UNAVAILABLE, "{principal} {path}");
    }
    let foreign_root = read(&other, OTHER, "stravia://history/")
        .await
        .expect("root");
    assert!(content(&foreign_root).contains("other question"));
    assert!(!content(&foreign_root).contains("Deploy using"));

    // 游标绑定签发它的路径；篡改或换路径都会被拒绝。
    let first = read(&owner, OWNER, &transcript).await.expect("first page");
    let next = first["next_path"].as_str().expect("long transcript pages");
    let cursor = next.rsplit_once(":cursor=").unwrap().1;
    for path in [
        format!("stravia://history/{root_id}/{b}:cursor={cursor}"),
        format!("{transcript}/tools/1:cursor={cursor}"),
        format!("stravia://history/{root_id}/:cursor={cursor}"),
        format!("{transcript}:cursor=AAAA"),
    ] {
        let error = read(&owner, OWNER, &path).await.unwrap_err();
        assert_eq!(error.message, "Invalid history cursor", "{path}");
    }

    fixture.close().await;
    Ok(())
}

#[test]
fn line_selections_merge_and_resolve_last_against_total() {
    use stravia_web_access_contract::read_path::LineSelection;
    let selections = [
        LineSelection::Inclusive { start: 5, end: 8 },
        LineSelection::Count { start: 1, count: 3 },
        LineSelection::Inclusive { start: 4, end: 4 },
        LineSelection::Last(2),
    ];
    assert_eq!(
        page::resolve(Some(&selections), Some(20)).unwrap(),
        vec![[1, 8], [19, 20]]
    );
    assert!(page::needs_total(Some(&selections)));
    assert!(page::resolve(Some(&[LineSelection::Last(1)]), Some(0)).is_err());
    assert!(page::resolve(Some(&[LineSelection::Inclusive { start: 3, end: 2 }]), None).is_err());
}

#[test]
fn oversized_line_splits_at_char_boundary_and_resumes_mid_line() {
    use crate::mcp::read::text::PAGE_BYTES;
    // 多字节字符让切点不可能正好落在 PAGE_BYTES 上。
    let line = format!("a{}", "界".repeat(PAGE_BYTES));
    let mut pieces = String::new();
    let mut position = Position::start(vec![[1, 1]]);
    let mut pages = 0;
    loop {
        let mut current = Page::new(position);
        let (_, from) = current.resume_at();
        current.offer(1, &line, from).unwrap();
        let (content, ranges, next) = current.finish();
        assert_eq!(ranges, json!([{"start_line": 1, "end_line": 1}]));
        assert!(content.len() <= PAGE_BYTES + 1);
        pieces.push_str(content.trim_end_matches('\n'));
        pages += 1;
        match next {
            Some(next) => position = next,
            None => break,
        }
    }
    assert_eq!(pieces, line);
    assert!(pages > 2);
}
