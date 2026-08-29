use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;

use crate::hook::Principal;
use crate::proxy::context::CancellationToken;
use crate::turn_chain::{
    SqlTurnChainStore, TurnChainStore, TurnCommit, TurnCommitError, TurnNode, TurnNodeId,
    TurnNodeKind, TurnUnavailable,
};

use super::{
    BackendOutput, MemoryWebSearchConfigStore, SearchBackend, SearchBackendInput, SearchCompletion,
    SearchEvidence, SearchEvidenceSet, SearchReport, SearchReportValidator, SearchSource,
    SearchTurnId, WebSearchBackendDraft, WebSearchBackendKind, WebSearchConfig, WebSearchEvent,
    WebSearchInput, WebSearchRunPolicy, WebSearchRunner,
};

#[tokio::test]
async fn report_rejects_a_source_without_verified_evidence() {
    let turn_id = SearchTurnId::new("wst_contract");
    let report = SearchReport {
        answer: "A claim [source-wst_contract-1]".into(),
        sources: vec![SearchSource {
            id: "source-wst_contract-1".into(),
            url: "https://8.8.8.8/invented".into(),
            title: Some("Invented".into()),
        }],
        limitations: vec![],
    };
    let evidence = SearchEvidenceSet::from_evidence([SearchEvidence {
        url: "https://1.1.1.1/verified".into(),
        title: Some("Verified".into()),
    }]);

    let error = SearchReportValidator
        .validate(
            &turn_id,
            SearchCompletion::Complete,
            None,
            report,
            &evidence,
        )
        .await
        .expect_err("invented URL must be rejected");

    assert_eq!(error.code, "unverified_source");
}

#[test]
fn provenance_rejects_non_public_single_label_hosts() {
    let error = super::validator::normalize_public_url("https://intranet/path")
        .expect_err("single-label host is not public");

    assert_eq!(error.code, "invalid_source_url");
}

#[tokio::test]
async fn partial_report_accepts_a_localized_limitation() {
    let turn_id = SearchTurnId::new("wst_localized");
    let report = SearchReport {
        answer: "検証済みの回答 [source-wst_localized-1]".into(),
        sources: vec![SearchSource {
            id: "source-wst_localized-1".into(),
            url: "https://8.8.8.8/search".into(),
            title: Some("検証済み".into()),
        }],
        limitations: vec!["時間内に確認できた範囲のみです。".into()],
    };
    let evidence = SearchEvidenceSet::from_evidence([SearchEvidence {
        url: "https://8.8.8.8/search".into(),
        title: Some("検証済み".into()),
    }]);

    let validated = SearchReportValidator
        .validate(
            &turn_id,
            SearchCompletion::Partial,
            Some(super::SearchPartialCause::WorkingBudgetExhausted),
            report,
            &evidence,
        )
        .await
        .expect("localized limitation is structural partial disclosure");

    assert_eq!(validated.limitations.len(), 1);
}

#[tokio::test]
async fn report_rejects_an_oversized_source_title() {
    let turn_id = SearchTurnId::new("wst_bounds");
    let report = SearchReport {
        answer: "A claim [source-wst_bounds-1]".into(),
        sources: vec![SearchSource {
            id: "source-wst_bounds-1".into(),
            url: "https://8.8.8.8/search".into(),
            title: Some("x".repeat(2 * 1024 + 1)),
        }],
        limitations: vec![],
    };
    let evidence = SearchEvidenceSet::from_evidence([SearchEvidence {
        url: "https://8.8.8.8/search".into(),
        title: None,
    }]);

    let error = SearchReportValidator
        .validate(
            &turn_id,
            SearchCompletion::Complete,
            None,
            report,
            &evidence,
        )
        .await
        .expect_err("oversized title must be rejected");

    assert_eq!(error.code, "invalid_report");
}

struct CountingBackend {
    calls: AtomicUsize,
    kind: WebSearchBackendKind,
    delay: Duration,
    inputs: Mutex<Vec<SearchBackendInput>>,
}

impl CountingBackend {
    fn local() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            kind: WebSearchBackendKind::Local,
            delay: Duration::ZERO,
            inputs: Mutex::new(Vec::new()),
        }
    }

    fn codex() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            kind: WebSearchBackendKind::Codex,
            delay: Duration::ZERO,
            inputs: Mutex::new(Vec::new()),
        }
    }

    fn delayed_codex(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::codex()
        }
    }
}

#[async_trait]
impl SearchBackend for CountingBackend {
    fn kind(&self) -> WebSearchBackendKind {
        self.kind
    }

    async fn run(&self, input: SearchBackendInput) -> Result<BackendOutput, super::WebSearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().expect("inputs").push(input.clone());
        tokio::time::sleep(self.delay).await;
        let id = format!("source-{}-1", input.turn_id);
        Ok(BackendOutput {
            completion: SearchCompletion::Complete,
            partial_cause: None,
            report: SearchReport {
                answer: format!("Verified claim [{id}]"),
                sources: vec![SearchSource {
                    id,
                    url: "https://8.8.8.8/search".into(),
                    title: Some("Verified".into()),
                }],
                limitations: vec![],
            },
            evidence: SearchEvidenceSet::from_evidence([SearchEvidence {
                url: "https://8.8.8.8/search".into(),
                title: Some("Verified".into()),
            }]),
            usage: Default::default(),
            model_turns: 1,
            tool_calls: 2,
        })
    }
}

fn enabled_local_config() -> WebSearchConfig {
    WebSearchConfig {
        revision: 7,
        enabled: true,
        backend: Some(WebSearchBackendDraft::Local {
            model_id: Some("search-model".into()),
        }),
        max_turns: 12,
        total_time_seconds: 600,
        updated_at: "2026-08-11T00:00:00Z".into(),
    }
}

#[tokio::test]
async fn runner_is_lazy_and_drop_cancels_the_request_owned_run() {
    let backend = Arc::new(CountingBackend::local());
    let runner = WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config())),
        Arc::new(crate::turn_chain::test_store().await),
        backend.clone(),
        backend.clone(),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(crate::web_search::AllowSearchRun),
    );
    let cancellation = CancellationToken::new();
    let stream = runner.run(WebSearchInput {
        principal: Principal::new("owner"),
        query: "Search a verified claim".into(),
        previous_turn_id: None,
        policy: None,
        cancellation: cancellation.clone(),
        deadline: Instant::now() + Duration::from_secs(30),
    });

    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    drop(stream);

    assert!(cancellation.is_cancelled());
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn runner_emits_one_terminal_result_and_commits_the_search_turn() {
    let backend = Arc::new(CountingBackend::local());
    let turns = Arc::new(crate::turn_chain::test_store().await);
    let runner = WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config())),
        turns.clone(),
        backend,
        Arc::new(CountingBackend::codex()),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(crate::web_search::AllowSearchRun),
    );
    let mut stream = runner.run(WebSearchInput {
        principal: Principal::new("owner"),
        query: "Search a verified claim".into(),
        previous_turn_id: None,
        policy: None,
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    });

    let mut terminal = None;
    let mut terminal_count = 0;
    while let Some(event) = stream.next().await {
        if event.is_terminal() {
            terminal_count += 1;
            terminal = Some(event);
        }
    }

    assert_eq!(terminal_count, 1);
    let WebSearchEvent::Completed(result) = terminal.expect("terminal event") else {
        panic!("expected completed Search");
    };
    let chain = turns
        .materialize(
            &Principal::new("owner"),
            crate::turn_chain::TurnNodeKind::WebSearch,
            &result.turn_id,
        )
        .await
        .expect("committed Search Turn");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].payload["query"], "Search a verified claim");
}

#[tokio::test]
async fn codex_uses_the_request_deadline_instead_of_saved_local_time_limit() {
    let mut config = enabled_local_config();
    config.backend = Some(WebSearchBackendDraft::Codex {
        provider_id: Some("codex-provider".into()),
        upstream_model: Some("gpt-5".into()),
    });
    config.total_time_seconds = 0;
    let runner = WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(config)),
        Arc::new(crate::turn_chain::test_store().await),
        Arc::new(CountingBackend::local()),
        Arc::new(CountingBackend::delayed_codex(Duration::from_millis(20))),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(crate::web_search::AllowSearchRun),
    );

    let result = completed(runner.run(WebSearchInput {
        principal: Principal::new("owner"),
        query: "Verify a claim".into(),
        previous_turn_id: None,
        policy: None,
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(1),
    }))
    .await;

    assert_eq!(result.completion, SearchCompletion::Complete);
}

async fn completed(stream: super::WebSearchEventStream) -> super::WebSearchResult {
    let events = stream.collect::<Vec<_>>().await;
    events
        .into_iter()
        .find_map(|event| match event {
            WebSearchEvent::Completed(result) => Some(result),
            _ => None,
        })
        .expect("completed Search result")
}

#[tokio::test]
async fn continuation_uses_the_exact_parent_snapshot_and_supports_sibling_branches() {
    let local = Arc::new(CountingBackend::local());
    let codex = Arc::new(CountingBackend::codex());
    let config = Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config()));
    let turns = Arc::new(crate::turn_chain::test_store().await);
    let runner = WebSearchRunner::new(
        config.clone(),
        turns.clone(),
        local.clone(),
        codex.clone(),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(crate::web_search::AllowSearchRun),
    );
    let principal = Principal::new("owner");
    let root = completed(runner.run(WebSearchInput {
        principal: principal.clone(),
        query: "Root question".into(),
        previous_turn_id: None,
        policy: Some(WebSearchRunPolicy {
            allowed_domains: vec!["EXAMPLE.COM.".into()],
            blocked_domains: vec![],
        }),
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    }))
    .await;

    config.replace(WebSearchConfig::default()).await;

    let inherited = completed(runner.run(WebSearchInput {
        principal: principal.clone(),
        query: "Inherited branch".into(),
        previous_turn_id: Some(root.turn_id.clone()),
        policy: None,
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    }))
    .await;
    let replaced = completed(runner.run(WebSearchInput {
        principal: principal.clone(),
        query: "Replacement branch".into(),
        previous_turn_id: Some(root.turn_id.clone()),
        policy: Some(WebSearchRunPolicy {
            allowed_domains: vec![],
            blocked_domains: vec!["blocked.example".into()],
        }),
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    }))
    .await;

    assert_ne!(inherited.turn_id, replaced.turn_id);
    assert_eq!(local.calls.load(Ordering::SeqCst), 3);
    assert_eq!(codex.calls.load(Ordering::SeqCst), 0);
    {
        let inputs = local.inputs.lock().expect("inputs");
        assert_eq!(inputs[1].ancestors.len(), 1);
        assert_eq!(inputs[1].definition_revision, Some(1));
        assert_eq!(
            inputs[1].local_limits.map(|limits| limits.max_turns),
            Some(12)
        );
        assert_eq!(inputs[1].policy.allowed_domains, ["example.com"]);
        assert_eq!(inputs[2].policy.allowed_domains, Vec::<String>::new());
        assert_eq!(inputs[2].policy.blocked_domains, ["blocked.example"]);
    }
    assert_eq!(
        turns
            .materialize(
                &principal,
                crate::turn_chain::TurnNodeKind::WebSearch,
                &inherited.turn_id,
            )
            .await
            .expect("inherited branch")
            .len(),
        2
    );
    assert_eq!(
        turns
            .materialize(
                &principal,
                crate::turn_chain::TurnNodeKind::WebSearch,
                &replaced.turn_id,
            )
            .await
            .expect("replacement branch")
            .len(),
        2
    );
}

#[tokio::test]
async fn continuation_is_principal_scoped_and_never_uses_an_implicit_latest_turn() {
    let backend = Arc::new(CountingBackend::local());
    let runner = WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config())),
        Arc::new(crate::turn_chain::test_store().await),
        backend.clone(),
        Arc::new(CountingBackend::codex()),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(crate::web_search::AllowSearchRun),
    );
    let root = completed(runner.run(WebSearchInput {
        principal: Principal::new("owner"),
        query: "Owner root".into(),
        previous_turn_id: None,
        policy: None,
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    }))
    .await;

    let foreign_events = runner
        .run(WebSearchInput {
            principal: Principal::new("other-owner"),
            query: "Foreign continuation".into(),
            previous_turn_id: Some(root.turn_id),
            policy: None,
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(30),
        })
        .collect::<Vec<_>>()
        .await;
    let foreign_error = foreign_events
        .into_iter()
        .find_map(|event| match event {
            WebSearchEvent::Failed(error) => Some(error),
            _ => None,
        })
        .expect("foreign continuation failure");
    assert_eq!(foreign_error.code, "turn_unavailable");

    completed(runner.run(WebSearchInput {
        principal: Principal::new("other-owner"),
        query: "Independent root".into(),
        previous_turn_id: None,
        policy: None,
        cancellation: CancellationToken::new(),
        deadline: Instant::now() + Duration::from_secs(30),
    }))
    .await;

    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    let inputs = backend.inputs.lock().expect("inputs");
    assert!(inputs[1].ancestors.is_empty());
}

struct FailingBackend;

#[async_trait]
impl SearchBackend for FailingBackend {
    fn kind(&self) -> WebSearchBackendKind {
        WebSearchBackendKind::Local
    }

    async fn run(
        &self,
        _input: SearchBackendInput,
    ) -> Result<BackendOutput, super::WebSearchError> {
        Err(super::WebSearchError::backend(
            WebSearchBackendKind::Local,
            "context_overflow",
            "Search context exceeds the configured model limit",
        ))
    }
}

#[tokio::test]
async fn backend_failure_does_not_commit_a_search_turn() {
    let turns = Arc::new(crate::turn_chain::test_store().await);
    let runner = WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config())),
        turns.clone(),
        Arc::new(FailingBackend),
        Arc::new(CountingBackend::codex()),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(crate::web_search::AllowSearchRun),
    );
    let principal = Principal::new("owner");
    let events = runner
        .run(WebSearchInput {
            principal: principal.clone(),
            query: "Overflow the complete ancestor context".into(),
            previous_turn_id: None,
            policy: None,
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(30),
        })
        .collect::<Vec<_>>()
        .await;
    let turn_id = events
        .iter()
        .find_map(|event| match event {
            WebSearchEvent::RunStarted { turn_id } => Some(turn_id.clone()),
            _ => None,
        })
        .expect("started turn");
    let error = events
        .iter()
        .find_map(|event| match event {
            WebSearchEvent::Failed(error) => Some(error),
            _ => None,
        })
        .expect("backend failure");

    assert_eq!(error.backend, Some(WebSearchBackendKind::Local));
    assert_eq!(error.code, "context_overflow");
    assert!(
        turns
            .materialize(
                &principal,
                crate::turn_chain::TurnNodeKind::WebSearch,
                &turn_id
            )
            .await
            .is_err()
    );
}

struct PendingBackend;

#[async_trait]
impl SearchBackend for PendingBackend {
    fn kind(&self) -> WebSearchBackendKind {
        WebSearchBackendKind::Local
    }

    async fn run(
        &self,
        _input: SearchBackendInput,
    ) -> Result<BackendOutput, super::WebSearchError> {
        futures::future::pending().await
    }
}

struct RevokingAuthorizer {
    calls: AtomicUsize,
}

#[async_trait]
impl super::SearchRunAuthorizer for RevokingAuthorizer {
    async fn authorize(
        &self,
        _principal: &Principal,
        _binding: &super::ResolvedWebSearchBackend,
    ) -> Result<(), super::WebSearchError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err(super::WebSearchError::new(
                "authorization_failed",
                "Web Search authorization failed",
            ))
        }
    }
}

#[tokio::test]
async fn in_progress_search_observes_authorization_revocation() {
    let runner = pending_runner(
        Arc::new(crate::turn_chain::test_store().await),
        Arc::new(RevokingAuthorizer {
            calls: AtomicUsize::new(0),
        }),
    );
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        runner
            .run(WebSearchInput {
                principal: Principal::new("owner"),
                query: "Search until revoked".into(),
                previous_turn_id: None,
                policy: None,
                cancellation: CancellationToken::new(),
                deadline: Instant::now() + Duration::from_secs(30),
            })
            .collect::<Vec<_>>(),
    )
    .await
    .expect("authorization is revalidated while the backend is running");

    assert!(matches!(
        events.last(),
        Some(WebSearchEvent::Failed(error)) if error.code == "authorization_failed"
    ));
}

struct BlockingCommitStore {
    inner: SqlTurnChainStore,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl TurnChainStore for BlockingCommitStore {
    async fn materialize(
        &self,
        principal: &Principal,
        kind: TurnNodeKind,
        id: &TurnNodeId,
    ) -> Result<Vec<TurnNode>, TurnUnavailable> {
        self.inner.materialize(principal, kind, id).await
    }

    async fn commit(&self, _commit: TurnCommit) -> Result<TurnNodeId, TurnCommitError> {
        self.entered.notify_waiters();
        futures::future::pending().await
    }

    async fn sweep_expired(&self) -> Result<u64, TurnUnavailable> {
        self.inner.sweep_expired().await
    }
}

#[tokio::test]
async fn cancellation_while_committing_does_not_create_a_search_turn() {
    let turns = Arc::new(BlockingCommitStore {
        inner: crate::turn_chain::test_store().await,
        entered: tokio::sync::Notify::new(),
    });
    let backend = Arc::new(CountingBackend::local());
    let runner = WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config())),
        turns.clone(),
        backend.clone(),
        backend,
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        Arc::new(super::AllowSearchRun),
    );
    let cancellation = CancellationToken::new();
    let entered = turns.entered.notified();
    let events = tokio::spawn(
        runner
            .run(WebSearchInput {
                principal: Principal::new("owner"),
                query: "Cancel during commit".into(),
                previous_turn_id: None,
                policy: None,
                cancellation: cancellation.clone(),
                deadline: Instant::now() + Duration::from_secs(30),
            })
            .collect::<Vec<_>>(),
    );
    entered.await;
    cancellation.cancel();
    let events = events.await.expect("Search stream task");

    assert!(matches!(
        events.last(),
        Some(WebSearchEvent::Failed(error)) if error.code == "cancelled"
    ));
    let turn_id = events
        .iter()
        .find_map(|event| match event {
            WebSearchEvent::RunStarted { turn_id } => Some(turn_id),
            _ => None,
        })
        .expect("started Search Turn");
    assert!(
        turns
            .inner
            .materialize(&Principal::new("owner"), TurnNodeKind::WebSearch, turn_id,)
            .await
            .is_err()
    );
}

fn pending_runner(
    turns: Arc<SqlTurnChainStore>,
    authorizer: Arc<dyn super::SearchRunAuthorizer>,
) -> WebSearchRunner {
    WebSearchRunner::new(
        Arc::new(MemoryWebSearchConfigStore::new(enabled_local_config())),
        turns,
        Arc::new(PendingBackend),
        Arc::new(CountingBackend::codex()),
        Arc::new(SearchReportValidator),
        Duration::from_secs(7 * 24 * 60 * 60),
        authorizer,
    )
}

#[tokio::test]
async fn explicit_cancellation_emits_one_failure_and_does_not_commit() {
    let turns = Arc::new(crate::turn_chain::test_store().await);
    let runner = pending_runner(turns.clone(), Arc::new(super::AllowSearchRun));
    let principal = Principal::new("owner");
    let cancellation = CancellationToken::new();
    let mut stream = runner.run(WebSearchInput {
        principal: principal.clone(),
        query: "Cancel this run".into(),
        previous_turn_id: None,
        policy: None,
        cancellation: cancellation.clone(),
        deadline: Instant::now() + Duration::from_secs(30),
    });
    let turn_id = loop {
        match stream.next().await.expect("run event") {
            WebSearchEvent::RunStarted { turn_id } => break turn_id,
            _ => continue,
        }
    };
    cancellation.cancel();
    let events = stream.collect::<Vec<_>>().await;
    let failures = events
        .iter()
        .filter_map(|event| match event {
            WebSearchEvent::Failed(error) => Some(error),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].code, "cancelled");
    assert!(
        turns
            .materialize(
                &principal,
                crate::turn_chain::TurnNodeKind::WebSearch,
                &turn_id
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn expired_deadline_emits_failure_without_committing() {
    let turns = Arc::new(crate::turn_chain::test_store().await);
    let runner = pending_runner(turns.clone(), Arc::new(super::AllowSearchRun));
    let principal = Principal::new("owner");
    let events = runner
        .run(WebSearchInput {
            principal: principal.clone(),
            query: "Run out of time".into(),
            previous_turn_id: None,
            policy: None,
            cancellation: CancellationToken::new(),
            deadline: Instant::now(),
        })
        .collect::<Vec<_>>()
        .await;
    let turn_id = events
        .iter()
        .find_map(|event| match event {
            WebSearchEvent::RunStarted { turn_id } => Some(turn_id),
            _ => None,
        })
        .expect("started turn");
    let error = events
        .iter()
        .find_map(|event| match event {
            WebSearchEvent::Failed(error) => Some(error),
            _ => None,
        })
        .expect("deadline failure");

    assert_eq!(error.code, "deadline_exceeded");
    assert!(
        turns
            .materialize(
                &principal,
                crate::turn_chain::TurnNodeKind::WebSearch,
                turn_id
            )
            .await
            .is_err()
    );
}
