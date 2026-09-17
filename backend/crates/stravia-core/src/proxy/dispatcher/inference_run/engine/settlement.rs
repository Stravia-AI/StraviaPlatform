//! Settlement: the fixed tail sequence an Inference Run owes after bytes are
//! confirmed Sent.
//!
//! Settlement owns "what happens after delivery is confirmed" and nothing else
//! — it never sends bytes. The steps run in a fixed order and each step's
//! failure is recorded as an observation gap on the run's terminal context
//! rather than silently skipping the rest of the tail.

#[cfg(test)]
use super::super::RunTerminalContext;
use super::ledger::RunLedger;
use super::projection::{ClientProjectionSession, ProjectedDeltaBatch, ProjectionDelivery};
use crate::interaction_observation::RunEvent;
use crate::interaction_observation::RunObserver;
use stravia_runtime_contract::protocol::ids::ProtocolId;
use stravia_runtime_contract::protocol::ir::AiResponse;

/// The outstanding work a run still owes after delivery is confirmed Sent.
///
/// Every field is optional because settlement is shared by paths that owe
/// different steps: a Hook-produced buffered response has no Platform
/// executions, the live tail has no staged batch left, and so on.
#[derive(Default)]
pub(super) struct Settlement {
    /// Marker batch staged for the Sent response, published as the first step.
    pub staged_delivery: Option<ProjectedDeltaBatch>,
    /// Platform executions whose jobs start once their Markers are published.
    pub background_executions: Vec<crate::HistoryMarkerExecutionJob>,
    /// Platform executions already started, spawned once delivery is confirmed.
    pub started_executions: Vec<crate::StartedHistoryMarkerExecution>,
    /// The live Hook session executions finish under. Required whenever
    /// `started_executions` is non-empty; its absence is itself a failure.
    pub run: Option<crate::hook::InferenceRun>,
    /// The staged Generation Chain node awaiting its commit persist.
    pub pending_generation_chain: Option<crate::generation_chain::GenerationChainWrite>,
    /// The terminal-delivery timestamp reported by the adapter.
    pub delivery_completed_at: Option<i64>,
    /// The response the client received, staged as observed client output.
    pub delivered_response: Option<AiResponse>,
}

/// A settlement step that failed. Later steps still ran; the outcome
/// enumerates what did not complete so callers and tests can inspect it.
#[derive(Debug)]
pub(super) enum SettlementFailure {
    /// Delivered Markers could not be published.
    MarkerPublish(crate::history_marker::HistoryMarkerError),
    /// Confirmed executions had no live Inference Run to finish under.
    ExecutionsWithoutRun { count: usize },
    /// The pending Generation Chain node failed to persist.
    GenerationCommit(crate::generation_chain::PersistError),
}

impl std::fmt::Display for SettlementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MarkerPublish(error) => write!(f, "settlement_marker_publish: {error}"),
            Self::ExecutionsWithoutRun { count } => {
                write!(f, "settlement_executions_without_run: {count}")
            }
            Self::GenerationCommit(error) => write!(f, "settlement_generation_commit: {error}"),
        }
    }
}

#[derive(Debug)]
pub(super) struct SettlementOutcome {
    /// Platform executions whose Markers this settlement published.
    pub(super) published_platform_executions: Vec<String>,
    /// Steps that failed, in the order they were attempted.
    pub(super) failures: Vec<SettlementFailure>,
}

/// Report a confirmed-delivered batch to the projection session and record the
/// Platform executions it published into the ledger. Shared by every delivery
/// path so the publish → ledger record pair cannot drift apart.
pub(super) async fn report_projected_delivery(
    session: &mut ClientProjectionSession,
    ledger: &RunLedger,
    batch: ProjectedDeltaBatch,
    outcome: ProjectionDelivery,
) -> Result<Vec<String>, crate::history_marker::HistoryMarkerError> {
    let published = session.report_delivery(batch, outcome).await?;
    ledger.record_published_executions(published.iter().cloned());
    Ok(published)
}

/// Run the post-Sent tail in its fixed order:
///
/// 1. publish the staged Marker batch and record the executions it released;
/// 2. start background Platform executions and spawn the already-started ones;
/// 3. persist the pending Generation Chain node, then flag it committed;
/// 4. stage the delivered response as the run's observed client output.
///
/// A step's failure is recorded as an observation gap and enumerated in the
/// outcome; the remaining steps still run to completion.
pub(super) async fn settle(
    gateway: &crate::Gateway,
    session: &mut ClientProjectionSession,
    ledger: &RunLedger,
    observer: &RunObserver,
    ingress: ProtocolId,
    settlement: Settlement,
) -> SettlementOutcome {
    let Settlement {
        staged_delivery,
        background_executions,
        mut started_executions,
        run,
        pending_generation_chain,
        delivery_completed_at,
        delivered_response,
    } = settlement;
    let mut outcome = SettlementOutcome {
        published_platform_executions: Vec::new(),
        failures: Vec::new(),
    };

    if let Some(batch) = staged_delivery {
        match report_projected_delivery(session, ledger, batch, ProjectionDelivery::Sent).await {
            Ok(references) => outcome.published_platform_executions = references,
            Err(error) => {
                tracing::error!("failed to publish delivered history markers: {error}");
                let failure = SettlementFailure::MarkerPublish(error);
                observer.record(RunEvent::ObservationGap {
                    reason: failure.to_string(),
                });
                outcome.failures.push(failure);
            }
        }
    }

    if !background_executions.is_empty() {
        started_executions.extend(gateway.start_history_marker_executions(
            ledger.terminal.principal.clone(),
            background_executions,
        ));
    }
    if !started_executions.is_empty() {
        match run {
            Some(run) => gateway.spawn_started_history_marker_executions(started_executions, run),
            None => {
                let count = started_executions.len();
                tracing::error!(
                    "delivered history markers have {count} Platform executions but no Inference Run"
                );
                let failure = SettlementFailure::ExecutionsWithoutRun { count };
                observer.record(RunEvent::ObservationGap {
                    reason: failure.to_string(),
                });
                outcome.failures.push(failure);
            }
        }
    }

    if let Some(mut write) = pending_generation_chain {
        match write.persist().await {
            Ok(()) => ledger
                .terminal
                .generation_committed
                .store(true, std::sync::atomic::Ordering::Release),
            Err(error) => {
                tracing::error!("failed to commit delivered Generation Chain node: {error}");
                let failure = SettlementFailure::GenerationCommit(error);
                observer.record(RunEvent::ObservationGap {
                    reason: failure.to_string(),
                });
                outcome.failures.push(failure);
            }
        }
    }

    if let Some(delivered_at) = delivery_completed_at {
        ledger.terminal.set_delivery_completed_at(delivered_at);
    }
    if let Some(response) = delivered_response {
        ledger.terminal.stage_client_output(ingress, &response);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_marker::{HistoryMarker, HistoryMarkerKind};
    use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use stravia_runtime_contract::protocol::ir::AiItem;
    use stravia_runtime_contract::protocol::ir::AiRequest;

    async fn fixture() -> (
        ClientProjectionSession,
        RunLedger,
        RunObserver,
        crate::Gateway,
    ) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool)
            .await
            .expect("SQLite migrations");
        let directory = tempfile::tempdir().expect("temp dir");
        let gateway = crate::Gateway::new(crate::config::GatewayConfig {
            data_dir: directory.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("Gateway");
        let observation = crate::interaction_observation::InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            7,
            true,
            gateway.generation_chains.clone(),
        )
        .await;
        let observer = observation
            .observe_ingress(crate::interaction_observation::IngressStart {
                id: "settle-run".into(),
                method: "POST".into(),
                path: "/v1/chat/completions".into(),
                protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            })
            .admit(
                crate::interaction_observation::RunStart {
                    id: "settle-run".into(),
                    principal: "owner".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "local-route".into(),
                    model_display_name: None,
                    ingress_protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
                },
                crate::interaction_observation::AdmissionFacts {
                    client_request: AiRequest::new("model", Vec::new()),
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            );
        let principal = stravia_runtime_contract::Principal::new("owner");
        let session = ClientProjectionSession::new(
            gateway.history_markers.clone(),
            principal.clone(),
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        );
        let ledger = RunLedger::new(
            RunTerminalContext::new(
                None,
                None,
                Vec::new(),
                gateway.compaction.clone(),
                principal,
                crate::model_turn::CompactionPublications::default(),
            ),
            crate::model_turn::CompactionPublications::default(),
        );
        (session, ledger, observer, gateway)
    }

    fn staged_platform_batch(
        session: &mut ClientProjectionSession,
        reference: &str,
    ) -> ProjectedDeltaBatch {
        session.begin_model_leg(
            crate::protocol::transform::ThinkingCarrierFacts {
                indexed: false,
                may_be_protected: false,
                stream_unprotected_summaries: false,
            },
            Vec::new(),
            None,
        );
        session.project_platform_marker(&HistoryMarker {
            reference: reference.into(),
            kind: HistoryMarkerKind::Platform,
            activity: "Running a Platform Tool".into(),
        })
    }

    #[tokio::test]
    async fn settle_publishes_staged_markers_then_commits_and_stages() {
        let (mut session, ledger, observer, gateway) = fixture().await;
        let principal = ledger.terminal.principal.clone();
        let marker = gateway
            .history_markers
            .create_platform(
                &principal,
                crate::history_marker::PlatformMarkerInput {
                    tool_id: "web_search".into(),
                    call: stravia_runtime_contract::protocol::ir::ToolCall {
                        id: "call-1".into(),
                        name: "web_search".into(),
                        arguments: "{}".into(),
                    },
                    activity: "Searching".into(),
                    execution_limit: std::time::Duration::from_secs(60),
                    pending_retention: std::time::Duration::from_secs(3600),
                },
            )
            .await
            .expect("create Platform marker");
        let batch = staged_platform_batch(&mut session, &marker.reference);

        let mut response = AiResponse::new("response-1", "model");
        response.items = vec![AiItem::output_text("answer")];
        let outcome = settle(
            &gateway,
            &mut session,
            &ledger,
            &observer,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Settlement {
                staged_delivery: Some(batch),
                delivery_completed_at: Some(1_700_000_000_000),
                delivered_response: Some(response),
                ..Default::default()
            },
        )
        .await;

        assert!(
            outcome.failures.is_empty(),
            "settlement failures: {:?}",
            outcome.failures
        );
        assert_eq!(
            outcome.published_platform_executions,
            vec![marker.reference.clone()]
        );
        assert!(session.client_output_committed());
        assert_eq!(
            ledger.published_executions(),
            vec![marker.reference.clone()]
        );
        assert_eq!(
            ledger.terminal.delivery_completed_at(),
            Some(1_700_000_000_000)
        );
    }

    #[tokio::test]
    async fn settle_enumerates_failures_and_still_runs_later_steps() {
        let (mut session, ledger, observer, gateway) = fixture().await;
        // A Marker that was never persisted makes publication fail.
        let batch = staged_platform_batch(&mut session, "abcdefghijklmnopqrstuvwxyzab");
        // A write that was never staged makes the commit persist fail.
        let pending = gateway
            .generation_chains
            .begin(
                ledger.terminal.principal.clone(),
                AiRequest::new("model", Vec::new()),
            )
            .await
            .expect("begin Generation Chain write");

        let outcome = settle(
            &gateway,
            &mut session,
            &ledger,
            &observer,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            Settlement {
                staged_delivery: Some(batch),
                pending_generation_chain: Some(pending),
                delivery_completed_at: Some(1_700_000_000_000),
                delivered_response: Some(AiResponse::new("response-1", "model")),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(outcome.failures.len(), 2);
        assert!(matches!(
            outcome.failures[0],
            SettlementFailure::MarkerPublish(_)
        ));
        assert!(matches!(
            outcome.failures[1],
            SettlementFailure::GenerationCommit(_)
        ));
        // The tail still ran to completion despite both failures.
        assert_eq!(
            ledger.terminal.delivery_completed_at(),
            Some(1_700_000_000_000)
        );
        assert!(
            !ledger
                .terminal
                .generation_committed
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
}
