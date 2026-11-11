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

async fn search(recall: &Recall<'_>, principal: &str, path: &str, question: &str) -> Value {
    let input = ReadInput {
        path: path.to_owned(),
        question: Some(question.to_owned()),
        attachments: None,
        allowed_domains: None,
        wait: None,
    };
    let ReadTarget::History(history) = parse_read_input(&input).expect("search path") else {
        panic!("not a history path");
    };
    recall.read(principal, &history).await.expect("search")
}

fn search_hits(page: &Value) -> Vec<(&str, Vec<&str>)> {
    let mut hits: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in content(page).lines() {
        if let Some(id) = line.strip_prefix("## ") {
            hits.push((id, Vec::new()));
        } else if let Some(line) = line.strip_prefix("- stravia://history/") {
            let path = line.split_whitespace().next().expect("hit path");
            hits.last_mut().expect("hit interaction").1.push(path);
        }
    }
    hits
}

async fn index_all_sqlite(fixture: &Fixture) -> anyhow::Result<()> {
    for principal in [OWNER, OTHER] {
        let chains = fixture
            .observation
            .recall_chains(principal, 0, None, 100)
            .await?;
        for chain in chains.chains {
            let members = fixture
                .observation
                .recall_members(principal, &chain.root_id, 0, None, 100)
                .await?
                .unwrap();
            for member in members.members {
                while index::index_sqlite(&fixture.pool, &member.id).await? {}
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn history_search_orders_scopes_and_reads_hits() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    seed(&fixture).await?;
    index_all_sqlite(&fixture).await?;
    let recall = fixture.recall(OWNER).await;
    let page = search(&recall, OWNER, "stravia://history/", "question answered").await;
    assert_eq!(page["representation"], "markdown");
    let hits = search_hits(&page);
    assert_eq!(hits.len(), 2, "cross-message AND: {}", content(&page));
    for (id, paths) in &hits {
        assert!(!paths.is_empty());
        assert!(paths.len() <= 3);
        for path in paths {
            let full_path = format!("stravia://history/{path}");
            assert!(full_path.contains(id));
            let excerpt = read(&recall, OWNER, &full_path)
                .await
                .expect("returned hit must be readable");
            let text = content(&excerpt).to_lowercase();
            assert!(
                text.contains("question") || text.contains("answered"),
                "{text}"
            );
        }
    }
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
    let child = &members.members[1].id;
    let scoped = search(
        &recall,
        OWNER,
        &format!("stravia://history/{root}/{child}"),
        "question answered",
    )
    .await;
    assert_eq!(
        search_hits(&scoped)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![child.as_str()]
    );
    let foreign = search(&recall, OTHER, "stravia://history/", "question answered").await;
    assert!(search_hits(&foreign).is_empty());
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_search_ranks_roles_and_intersects_distinct_messages() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let mut ids = Vec::new();
    for (run, input, output) in [
        ("rank-user", "zircon", "ordinary response"),
        ("rank-answer", "ordinary input", "zircon"),
        ("rank-cross", "zircon", "cobalt"),
    ] {
        let observer = admit(&fixture.observation, run, OWNER, vec![user(input)], None);
        observer.record(visible(run, output, true));
        finish(&observer, "completed", now(), None);
        fixture.observation.flush().await?;
        let chains = fixture
            .observation
            .recall_chains(OWNER, 0, None, 20)
            .await?;
        let chain = chains
            .chains
            .iter()
            .find(|chain| {
                !ids.iter()
                    .any(|(id, _): &(String, String)| id == &chain.head_id)
            })
            .unwrap();
        ids.push((chain.head_id.clone(), chain.root_id.clone()));
    }
    index_all_sqlite(&fixture).await?;
    let recall = fixture.recall(OWNER).await;
    let page = search(&recall, OWNER, "stravia://history/", "zircon").await;
    let found: Vec<_> = search_hits(&page).iter().map(|(id, _)| *id).collect();
    assert_eq!(found.len(), 3);
    assert_eq!(found[2], ids[1].0, "user matches precede assistant matches");
    let cross = search(&recall, OWNER, "stravia://history/", "zircon cobalt").await;
    assert_eq!(
        search_hits(&cross)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![ids[2].0.as_str()]
    );
    let scoped = search(
        &recall,
        OWNER,
        &format!("stravia://history/{}/", ids[0].1),
        "zircon",
    )
    .await;
    assert_eq!(
        search_hits(&scoped)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![ids[0].0.as_str()]
    );
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_search_finds_overlap_tool_arguments_and_redacts_snippets() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let mut words = vec!["filler"; 255];
    words.extend(["overlapalpha", "overlapbeta"]);
    let observer = admit(
        &fixture.observation,
        "segments",
        OWNER,
        vec![user(&words.join(" "))],
        None,
    );
    observer.record(RunEvent::PlatformToolStarted {
        model_turn_id: "turn".into(),
        tool_id: "tool".into(),
        name: "executor".into(),
        input: Some(json!({"command": "needletool --deploy"})),
    });
    observer.record(visible("answer", &format!("publicmarker {SECRET}"), true));
    finish(&observer, "completed", now(), None);
    fixture.observation.flush().await?;
    index_all_sqlite(&fixture).await?;
    let recall = fixture.recall(OWNER).await;
    let phrase = search(
        &recall,
        OWNER,
        "stravia://history/",
        "\"overlapalpha overlapbeta\"",
    )
    .await;
    let hits = search_hits(&phrase);
    assert_eq!(hits.len(), 1, "{}", content(&phrase));
    assert!(!hits[0].1.is_empty());
    for path in &hits[0].1 {
        let page = read(&recall, OWNER, &format!("stravia://history/{path}")).await?;
        assert!(content(&page).contains("overlapalpha overlapbeta"));
    }
    let tool = search(&recall, OWNER, "stravia://history/", "needletool").await;
    let hits = search_hits(&tool);
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].1.is_empty());
    for path in &hits[0].1 {
        assert!(
            path.contains("/tools/"),
            "argument hits point to tool files: {path}"
        );
        let page = read(&recall, OWNER, &format!("stravia://history/{path}")).await?;
        assert!(content(&page).contains("needletool"));
    }
    let redacted = search(&recall, OWNER, "stravia://history/", "publicmarker").await;
    assert_eq!(search_hits(&redacted).len(), 1);
    assert!(content(&redacted).contains("«publicmarker»"));
    assert!(content(&redacted).contains(&reference()));
    assert!(!content(&redacted).contains(SECRET));
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_search_hit_line_ranges_survive_appended_events() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let observer = admit(
        &fixture.observation,
        "ongoing",
        OWNER,
        vec![user("stableanchor")],
        None,
    );
    observer.record(visible("first", "first response", true));
    fixture.observation.flush().await?;
    index_all_sqlite(&fixture).await?;
    let recall = fixture.recall(OWNER).await;
    let page = search(&recall, OWNER, "stravia://history/", "stableanchor").await;
    let hits = search_hits(&page);
    assert_eq!(hits.len(), 1);
    let path = format!("stravia://history/{}", hits[0].1.first().expect("hit path"));
    let before = read(&recall, OWNER, &path).await?;
    assert!(content(&before).contains("stableanchor"));
    observer.record(visible("second", "later appended text", true));
    fixture.observation.flush().await?;
    index_all_sqlite(&fixture).await?;
    let after = read(&recall, OWNER, &path).await?;
    assert_eq!(content(&before), content(&after));
    finish(&observer, "completed", now(), None);
    drop(recall);
    fixture.close().await;
    Ok(())
}

enum SearchDatabase<'a> {
    Sqlite(&'a sqlx::SqlitePool),
    Postgres(&'a sqlx::PgPool),
}

async fn shared_search_corpus(
    observation: &InteractionObservation,
    db: SearchDatabase<'_>,
) -> anyhow::Result<Vec<Vec<(usize, Vec<String>)>>> {
    let mut ids = Vec::new();
    let mut roots = Vec::new();
    let mut overlap = vec!["filler"; 10];
    overlap.extend(["middlealpha", "middlebeta"]);
    overlap.extend(vec!["filler"; 243]);
    overlap.extend(["overlapalpha", "overlapbeta"]);
    let overlap = overlap.join(" ");
    for (run, input, output) in [
        ("corpus-a", "迁移失败 retention.rs", "resolved cobalt"),
        ("corpus-b", "ordinary input", "迁移失败 retention.rs cobalt"),
        ("corpus-c", overlap.as_str(), "ordinary"),
    ] {
        let observer = admit(observation, run, OWNER, vec![user(input)], None);
        observer.record(visible(run, output, true));
        finish(&observer, "completed", now(), None);
        observation.flush().await?;
        let chains = observation.recall_chains(OWNER, 0, None, 10).await?;
        let chain = chains
            .chains
            .iter()
            .find(|chain| !ids.contains(&chain.head_id))
            .unwrap();
        ids.push(chain.head_id.clone());
        roots.push(chain.root_id.clone());
    }
    let other = admit(
        observation,
        "corpus-other",
        OTHER,
        vec![user("迁移失败 retention.rs cobalt")],
        None,
    );
    other.record(visible("foreign", "foreign", true));
    finish(&other, "completed", now(), None);
    observation.flush().await?;
    let foreign = observation.recall_chains(OTHER, 0, None, 10).await?.chains[0]
        .head_id
        .clone();
    for id in ids.iter().chain(std::iter::once(&foreign)) {
        while match db {
            SearchDatabase::Sqlite(pool) => index::index_sqlite(pool, id).await?,
            SearchDatabase::Postgres(pool) => index::index_postgres(pool, id).await?,
        } {}
    }
    let secrets = KnownSecrets::load(&OwnerMappings, &Principal::new(OWNER))
        .await
        .expect("mappings");
    let recall = Recall {
        observation,
        secrets,
        retention_days: 7,
        active_since: 0,
    };
    let mut results = Vec::new();
    let root_path = "stravia://history/".to_owned();
    let chain_path = format!("stravia://history/{}/", roots[0]);
    let transcript_path = format!("stravia://history/{}/{}", roots[0], ids[0]);
    let cases: [(&str, &str, &[usize]); 8] = [
        (&root_path, "迁移失败", &[0, 1]),
        (&root_path, "retention.rs", &[0, 1]),
        // corpus-a 的两个词项位于不同消息中。
        (&root_path, "迁移失败 cobalt", &[0, 1]),
        (&root_path, "\"retention rs\"", &[0, 1]),
        (&root_path, "\"middlealpha middlebeta\"", &[2]),
        (&root_path, "\"overlapalpha overlapbeta\"", &[2]),
        (&chain_path, "迁移失败", &[0]),
        (&transcript_path, "迁移失败 cobalt", &[0]),
    ];
    for (path, query, expected) in cases {
        let page = search(&recall, OWNER, path, query).await;
        let hits = search_hits(&page);
        assert_eq!(hits.len(), expected.len(), "{query}: {}", content(&page));
        for ((id, paths), expected_index) in hits.iter().zip(expected) {
            assert_eq!(*id, ids[*expected_index]);
            assert!(!paths.is_empty());
        }
        let snippets: Vec<String> = content(&page)
            .lines()
            .filter_map(|line| line.strip_prefix("- stravia://history/"))
            .map(|line| {
                let (_, snippet) = line.split_once(" — ").expect("hit snippet");
                assert!(
                    snippet.chars().count() <= 160,
                    "snippet exceeds 160 characters"
                );
                snippet.to_owned()
            })
            .collect();
        let mut rows = Vec::new();
        for (id, paths) in hits {
            let mut excerpts = Vec::new();
            for path in paths {
                let result = read(&recall, OWNER, &format!("stravia://history/{path}")).await?;
                if query == "\"middlealpha middlebeta\"" {
                    assert!(content(&result).contains("middlealpha middlebeta"));
                }
                if query == "\"overlapalpha overlapbeta\"" {
                    assert!(content(&result).contains("overlapalpha overlapbeta"));
                }
                excerpts.push(content(&result).to_owned());
            }
            rows.push((ids.iter().position(|known| known == id).unwrap(), excerpts));
        }
        rows.push((usize::MAX, snippets));
        results.push(rows);
    }
    let other_secrets = KnownSecrets::load(&OwnerMappings, &Principal::new(OTHER))
        .await
        .expect("mappings");
    let other_recall = Recall {
        observation,
        secrets: other_secrets,
        retention_days: 7,
        active_since: 0,
    };
    let other_page = search(&other_recall, OTHER, &root_path, "迁移失败 cobalt").await;
    let other_hits = search_hits(&other_page);
    assert_eq!(other_hits.len(), 1);
    assert_eq!(other_hits[0].0, foreign);
    let absent = search(
        &other_recall,
        OTHER,
        &root_path,
        "\"overlapalpha overlapbeta\"",
    )
    .await;
    assert!(search_hits(&absent).is_empty());
    Ok(results)
}

#[tokio::test]
async fn history_search_sqlite_shared_corpus() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    shared_search_corpus(&fixture.observation, SearchDatabase::Sqlite(&fixture.pool)).await?;
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_search_paginates_without_duplicate_interactions() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    for n in 0..12 {
        let run = format!("page-{n}");
        let observer = admit(
            &fixture.observation,
            &run,
            OWNER,
            vec![user("paginationneedle")],
            None,
        );
        observer.record(visible(&run, "response", true));
        finish(&observer, "completed", now(), None);
        fixture.observation.flush().await?;
    }
    index_all_sqlite(&fixture).await?;
    // 所有命中的角色权重与时间相同，只以 Interaction id 排序并翻页。
    sqlx::query("UPDATE history_search_segments SET occurred_at = ?")
        .bind(now())
        .execute(&fixture.pool)
        .await?;
    let mut expected: Vec<_> = fixture
        .observation
        .recall_chains(OWNER, 0, None, 100)
        .await?
        .chains
        .into_iter()
        .map(|chain| chain.head_id)
        .collect();
    expected.sort();
    assert_eq!(expected.len(), 12);
    let recall = fixture.recall(OWNER).await;
    let first = search(&recall, OWNER, "stravia://history/", "paginationneedle").await;
    let first_hits = search_hits(&first);
    assert_eq!(first_hits.len(), 10);
    let next = first["next_path"].as_str().expect("search cursor");
    let second = read(&recall, OWNER, next).await?;
    let second_hits = search_hits(&second);
    assert_eq!(second_hits.len(), 2);
    let ids: Vec<_> = first_hits
        .iter()
        .chain(&second_hits)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(ids, expected.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(second["next_path"].is_null());
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_search_hit_beyond_long_line_page_cut_remains_readable() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let text = format!("{}distantneedle end", "padding ".repeat(5000));
    let observer = admit(
        &fixture.observation,
        "long-hit",
        OWNER,
        vec![user(&text)],
        None,
    );
    observer.record(visible("long-hit-answer", "response", true));
    finish(&observer, "completed", now(), None);
    fixture.observation.flush().await?;
    index_all_sqlite(&fixture).await?;

    let recall = fixture.recall(OWNER).await;
    let page = search(&recall, OWNER, "stravia://history/", "distantneedle").await;
    assert!(
        content(&page).contains("«distantneedle»"),
        "{}",
        content(&page)
    );
    let hits = search_hits(&page);
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].1.is_empty());
    let hit_path = format!("stravia://history/{}", hits[0].1[0]);
    let first = read(&recall, OWNER, &hit_path).await?;
    assert!(!content(&first).contains("distantneedle"));
    let next = first["next_path"].as_str().expect("long line continuation");
    let continuation = read(&recall, OWNER, next).await?;
    assert!(content(&continuation).contains("distantneedle"));
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test]
async fn history_search_foreign_segments_cannot_starve_owner() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let owner = admit(
        &fixture.observation,
        "flood-owner",
        OWNER,
        vec![user("x")],
        None,
    );
    owner.record(visible("owner-response", "owner response", true));
    finish(&owner, "completed", now(), None);
    fixture.observation.flush().await?;
    let owner_id = fixture
        .observation
        .recall_chains(OWNER, 0, None, 10)
        .await?
        .chains[0]
        .head_id
        .clone();

    // 先索引超过候选上限的其他 Principal 命中，确保它们无法挤掉当前 Principal。
    let foreign_text = "x ".repeat(241 * 2002 + 15);
    let foreign = admit(
        &fixture.observation,
        "flood-foreign",
        OTHER,
        vec![user(&foreign_text)],
        None,
    );
    foreign.record(visible("foreign-response", "foreign response", true));
    finish(&foreign, "completed", now(), None);
    fixture.observation.flush().await?;
    let foreign_id = fixture
        .observation
        .recall_chains(OTHER, 0, None, 10)
        .await?
        .chains[0]
        .head_id
        .clone();
    while index::index_sqlite(&fixture.pool, &foreign_id).await? {}
    while index::index_sqlite(&fixture.pool, &owner_id).await? {}
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM history_search_segments WHERE principal = ?")
            .bind(OTHER)
            .fetch_one(&fixture.pool)
            .await?;
    assert!(
        count > 2000,
        "fixture must exceed the candidate cap: {count}"
    );
    let recall = fixture.recall(OWNER).await;
    let page = search(&recall, OWNER, "stravia://history/", "x").await;
    let hits = search_hits(&page);
    assert_eq!(hits.len(), 1, "{}", content(&page));
    assert_eq!(hits[0].0, owner_id);
    drop(recall);
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
async fn history_search_matches_postgres_shared_corpus() -> anyhow::Result<()> {
    let sqlite = Fixture::new().await?;
    let expected =
        shared_search_corpus(&sqlite.observation, SearchDatabase::Sqlite(&sqlite.pool)).await?;
    sqlite.close().await;
    let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
        .or_else(|_| std::env::var("DB_URL"))
        .or_else(|_| std::env::var("DATABASE_URL"))?;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let schema = format!("history_search_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    let scenario_pool = pool.clone();
    let result = tokio::spawn(async move {
        crate::migrations::migrate_postgres(&scenario_pool, None).await?;
        let directory = tempfile::tempdir()?;
        let observation = InteractionObservation::new(
            None,
            Some(scenario_pool.clone()),
            directory.path().to_path_buf(),
            crate::history_retention::HistoryRetention::new(1),
            true,
            crate::generation_chain::test_chain().await,
        )
        .await;
        let actual =
            shared_search_corpus(&observation, SearchDatabase::Postgres(&scenario_pool)).await;
        observation.shutdown().await;
        let actual = actual?;
        assert_eq!(actual, expected);
        anyhow::Ok(())
    })
    .await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result??;
    Ok(())
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
