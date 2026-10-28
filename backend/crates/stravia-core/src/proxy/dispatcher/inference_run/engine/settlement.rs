//! Settlement owns delivery preparation and the fixed tail an Inference Run
//! owes after bytes are confirmed Sent.
//!
//! Delivery adapters still own the synchronous Sent receipt. Buffered delivery
//! binds its tail to body EOF; streaming keeps Marker executions separate from
//! final-response completion. No settlement operation sends bytes or detaches
//! work from its existing body/producer task.

#[cfg(test)]
use super::super::RunTerminalContext;
use axum::response::Response;

use super::delivery::after_body_delivery;
use super::ledger::RunLedger;
use super::projection::{ClientProjectionSession, ProjectedDeltaBatch, ProjectionDelivery};
use crate::interaction_observation::RunEvent;
use crate::interaction_observation::RunObserver;

/// Runtime-only envelope for a Generation Chain write and the Vendor result
/// whose bytes it will publish. The fence deliberately never enters a
/// persistent request/response DTO.
pub(super) struct PendingGenerationChainWrite {
    pub(super) write: crate::generation_chain::GenerationChainWrite,
    pub(super) vendor_publications: Vec<crate::plugin::VendorPublicationFence>,
}

use super::super::{GuardedVendorPublications, VendorWriteGuards};

/// Execution work stays paired with the Hook run that owns its delivered result.
#[derive(Default)]
pub(super) struct MarkerExecutions {
    background: Vec<crate::PreparedMarkerExecution>,
    started: Vec<crate::StartedMarkerDelivery>,
    run: Option<crate::hook::InferenceRun>,
}

impl MarkerExecutions {
    pub(super) fn take(
        background: Vec<crate::PreparedMarkerExecution>,
        started: Vec<crate::StartedMarkerDelivery>,
        run: &mut Option<crate::hook::InferenceRun>,
    ) -> Self {
        // Empty work must leave the run available to its caller's Hook cleanup.
        let run = if background.is_empty() && started.is_empty() {
            None
        } else {
            run.take()
        };
        Self {
            background,
            started,
            run,
        }
    }

    /// Consume only work whose Markers were delivered. This does not confirm a
    /// complete response, commit Generation history, or perform Tool Execution
    /// Handoff; those facts retain their existing owners.
    pub(super) async fn finish(
        self,
        gateway: &crate::Gateway,
        session: &mut ClientProjectionSession,
        ledger: &RunLedger,
        observer: &RunObserver,
    ) -> SettlementOutcome {
        settle(
            gateway,
            session,
            ledger,
            observer,
            Settlement {
                executions: self,
                ..Default::default()
            },
        )
        .await
    }
}

/// An owned, single-use response tail. Its pending write and prepared Vendor
/// guards cannot be separated by a delivery caller.
pub(super) struct CompletionSettlement {
    settlement: Settlement,
}

impl CompletionSettlement {
    pub(super) fn buffered(
        staged_delivery: ProjectedDeltaBatch,
        pending_generation_chain: Option<PendingGenerationChainWrite>,
        executions: MarkerExecutions,
    ) -> Self {
        Self {
            settlement: Settlement {
                staged_delivery: Some(staged_delivery),
                pending_generation_chain,
                executions,
                ..Default::default()
            },
        }
    }

    /// Streaming Marker work is deliberately absent: a final receipt must not
    /// release executions skipped by the earlier Marker-delivery gate.
    pub(super) fn streaming(pending_generation_chain: Option<PendingGenerationChainWrite>) -> Self {
        Self {
            settlement: Settlement {
                pending_generation_chain,
                ..Default::default()
            },
        }
    }

    /// Acquire the existing revocation fence before exposing terminal output.
    /// Failure preserves the pending publications for the existing settlement
    /// check; it neither approves publication nor rejects the response early.
    pub(super) async fn prepare(&mut self, ledger: &RunLedger, observer: &RunObserver) {
        let pending = self.settlement.pending_generation_chain.as_mut();
        let publications = pending
            .as_ref()
            .map(|pending| pending.vendor_publications.as_slice())
            .unwrap_or_default();
        self.settlement.prepared_vendor_guards =
            match crate::model_turn::vendor_write_fences(publications).await {
                Ok(guards) => {
                    let guards = std::sync::Arc::new(GuardedVendorPublications {
                        _guards: guards,
                        confirmed_sent: std::sync::atomic::AtomicBool::new(false),
                        publications: pending
                            .map(|pending| std::mem::take(&mut pending.vendor_publications))
                            .unwrap_or_default(),
                    });
                    ledger
                        .terminal
                        .prepare_client_completion(observer.clone(), guards.clone());
                    Some(guards)
                }
                Err(_) => None,
            };
    }

    /// Prepare and bind the tail before returning the buffered response.
    /// The existing body wrapper polls it inline only after successful EOF;
    /// dropping the body never spawns a replacement task.
    pub(super) async fn bind_buffered(
        mut self,
        response: Response,
        gateway: crate::Gateway,
        mut session: ClientProjectionSession,
        ledger: RunLedger,
        observer: RunObserver,
    ) -> Response {
        self.prepare(&ledger, &observer).await;
        after_body_delivery(response, async move {
            let delivered_at = chrono::Utc::now().timestamp_millis();
            ledger
                .terminal
                .publish_prepared_client_completion(delivered_at);
            self.finish_sent(&gateway, &mut session, &ledger, &observer, delivered_at)
                .await;
        })
    }

    /// Consume the tail using the adapter's actual receipt timestamp. Live
    /// adapters have already published Sent synchronously before this await.
    pub(super) async fn finish_sent(
        mut self,
        gateway: &crate::Gateway,
        session: &mut ClientProjectionSession,
        ledger: &RunLedger,
        observer: &RunObserver,
        delivered_at: i64,
    ) -> SettlementOutcome {
        self.settlement.delivery_completed_at = Some(delivered_at);
        settle(gateway, session, ledger, observer, self.settlement).await
    }
}

/// Private work representation shared by the two response shapes and the
/// stream's earlier Marker phase. Callers cannot mix their optional fields.
///
/// Marker publication, execution results and Generation persistence remain
/// independent steps, not a transaction.
#[derive(Default)]
struct Settlement {
    /// Marker batch staged for the Sent response, published after observation.
    staged_delivery: Option<ProjectedDeltaBatch>,
    executions: MarkerExecutions,
    /// The staged Generation Chain node awaiting its fenced commit persist.
    pending_generation_chain: Option<PendingGenerationChainWrite>,
    /// The terminal-delivery timestamp reported by the adapter.
    delivery_completed_at: Option<i64>,
    /// Revocation guards acquired before the transport exposed terminal output.
    prepared_vendor_guards: Option<VendorWriteGuards>,
}

/// A settlement step that failed. Later steps still ran; the outcome
/// enumerates what did not complete so callers and tests can inspect it.
#[derive(Debug)]
pub(super) enum SettlementFailure {
    /// Delivered Markers could not be published.
    MarkerPublish(crate::history_marker::HistoryMarkerError),
    /// The current Platform delivery result could not be persisted.
    MarkerResult(crate::history_marker::HistoryMarkerError),
    /// Confirmed executions had no live Inference Run to finish under.
    ExecutionsWithoutRun { count: usize },
    /// The Vendor generation became stale before its final history write.
    VendorPublication(anyhow::Error),
    /// The pending Generation Chain node failed to persist.
    GenerationCommit(crate::generation_chain::PersistError),
}

impl std::fmt::Display for SettlementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MarkerPublish(error) => write!(f, "settlement_marker_publish: {error}"),
            Self::MarkerResult(error) => write!(f, "settlement_marker_result: {error}"),
            Self::ExecutionsWithoutRun { count } => {
                write!(f, "settlement_executions_without_run: {count}")
            }
            Self::VendorPublication(error) => {
                write!(f, "settlement_vendor_publication: {error}")
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
/// 1. publish the real delivered client projection and adapter timestamp;
/// 2. publish the staged Marker batch and record the executions it released;
/// 3. start background Platform executions and spawn the already-started ones;
/// 4. persist the pending Generation Chain node, then flag it committed.
///
/// A step's failure is recorded as an observation gap and enumerated in the
/// outcome; later independent steps still run. A stale Vendor fence is the one
/// exception: no result-derived publication step may start after revocation.
async fn settle(
    gateway: &crate::Gateway,
    session: &mut ClientProjectionSession,
    ledger: &RunLedger,
    observer: &RunObserver,
    settlement: Settlement,
) -> SettlementOutcome {
    let Settlement {
        mut staged_delivery,
        executions:
            MarkerExecutions {
                background: mut background_executions,
                started: mut started_executions,
                run,
            },
        mut pending_generation_chain,
        delivery_completed_at,
        prepared_vendor_guards,
    } = settlement;
    let mut outcome = SettlementOutcome {
        published_platform_executions: Vec::new(),
        failures: Vec::new(),
    };

    let guards = match prepared_vendor_guards {
        Some(guards) => guards.ensure_for_settlement().map(|()| guards),
        None => {
            let publications = pending_generation_chain
                .as_ref()
                .map(|pending| pending.vendor_publications.as_slice())
                .unwrap_or_default();
            crate::model_turn::vendor_write_fences(publications)
                .await
                .map(|guards| {
                    std::sync::Arc::new(GuardedVendorPublications {
                        _guards: guards,
                        confirmed_sent: std::sync::atomic::AtomicBool::new(false),
                        publications: pending_generation_chain
                            .as_mut()
                            .map(|pending| std::mem::take(&mut pending.vendor_publications))
                            .unwrap_or_default(),
                    })
                })
        }
    };
    let _vendor_guards = match guards {
        Ok(guards) => Some(guards),
        Err(error) => {
            tracing::error!("refused stale Vendor settlement write: {error}");
            let failure = SettlementFailure::VendorPublication(error);
            observer.record(RunEvent::ObservationGap {
                reason: failure.to_string(),
            });
            outcome.failures.push(failure);
            // 已送达的字节无法收回，但旧代结果不得再发布 Marker、启动依赖任务、
            // 写 Generation Chain 或登记观测到的历史响应。
            staged_delivery = None;
            background_executions.clear();
            started_executions.clear();
            pending_generation_chain = None;
            None
        }
    };

    // This is observation-only publication: it cannot wait for Marker side
    // effects or Generation persistence. The existing Sent receipt, not a
    // pre-delivery ClientToolHandoff, proves the client received this window.
    if let Some(delivered_at) = delivery_completed_at {
        ledger.terminal.set_delivery_completed_at(delivered_at);
        if _vendor_guards.is_some() {
            ledger
                .terminal
                .publish_client_completion(observer, delivered_at);
        }
    }

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
            Some(mut run) => {
                if let Err(error) = run.finish_marker_deliveries(started_executions).await {
                    let failure = SettlementFailure::MarkerResult(error);
                    observer.record(RunEvent::ObservationGap {
                        reason: failure.to_string(),
                    });
                    outcome.failures.push(failure);
                }
            }
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

    if let Some(mut pending) = pending_generation_chain {
        match pending.write.persist_holding_fence().await {
            Ok(()) => {
                if let Some(fence) = pending.write.take_commit_fence() {
                    observer.hold_generation_commit_fence(fence);
                }
                ledger
                    .terminal
                    .generation_committed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
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

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::history_marker::{HistoryMarker, HistoryMarkerKind};
    use stravia_runtime_contract::protocol::ids::OPEN_RESPONSES_2026_04_24;
    use stravia_runtime_contract::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use stravia_runtime_contract::protocol::ir::{AiItem, AiRequest, AiResponse};

    async fn fixture() -> (
        tempfile::TempDir,
        ClientProjectionSession,
        RunLedger,
        RunObserver,
        crate::Gateway,
        crate::interaction_observation::InteractionObservation,
    ) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("SQLite pool");
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .expect("SQLite migrations");
        let directory = tempfile::tempdir().expect("temp dir");
        let gateway = crate::Gateway::from_storage(
            crate::config::GatewayConfig {
                data_dir: directory.path().to_path_buf(),
                ..Default::default()
            },
            std::sync::Arc::new(crate::storage::MemoryStorage::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
        )
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
                    client_request: Arc::new(AiRequest::new("model", Vec::new())),
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
                Arc::new(AiRequest::new("model", Vec::new())),
                gateway.compaction.clone(),
                principal,
                crate::model_turn::CompactionPublications::default(),
            ),
            crate::model_turn::CompactionPublications::default(),
        );
        (directory, session, ledger, observer, gateway, observation)
    }

    fn staged_platform_batch(
        session: &mut ClientProjectionSession,
        reference: &str,
    ) -> ProjectedDeltaBatch {
        session.begin_model_leg(
            stravia_protocol_codec::transform::ThinkingCarrierFacts {
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

    /// Delivery timestamps seed observation retention and tail attribution.
    /// They must be real receipt times; a fixed past instant expires the tail
    /// before a continuation can be attributed to it.
    fn sent_at() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    const VENDOR: &str = "settlement-vendor";

    async fn vendor_settlement(
        gateway: &crate::Gateway,
        session: &mut ClientProjectionSession,
        ledger: &RunLedger,
        tracker: &crate::plugin::VendorOperationTracker,
        caller: stravia_runtime_contract::CancellationToken,
    ) -> (
        CompletionSettlement,
        AiResponse,
        String,
        crate::plugin::VendorPublicationFence,
        String,
    ) {
        let operation = tracker.begin(VENDOR).expect("Vendor operation");
        let publication =
            operation.publication_fence(caller, stravia_runtime_contract::Deadline::never());
        // A finished operation leaves a real generation publication fence alive.
        drop(operation);
        let marker = gateway
            .history_markers
            .create_platform(
                &ledger.terminal.principal,
                crate::history_marker::PlatformMarkerInput {
                    tool_id: "web_search".into(),
                    call: stravia_runtime_contract::protocol::ir::ToolCall {
                        id: "platform-call".into(),
                        name: "web_search".into(),
                        arguments: "{}".into(),
                    },
                    activity: "Searching".into(),
                    execution_limit: std::time::Duration::from_secs(60),
                    pending_retention: std::time::Duration::from_secs(3600),
                },
            )
            .await
            .expect("Platform marker");
        let batch = staged_platform_batch(session, &marker.reference);
        let mut response = AiResponse::new("response", "model");
        response.items = vec![AiItem::function_call(
            stravia_runtime_contract::protocol::ir::ToolCall {
                id: "client-call".into(),
                name: "inspect".into(),
                arguments: "{}".into(),
            },
        )];
        let mut request = AiRequest::new("model", Vec::new());
        request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        let mut write = gateway
            .generation_chains
            .begin(ledger.terminal.principal.clone(), request)
            .await
            .expect("begin Generation Chain");
        let generation_id = write.id().to_owned();
        assert!(write.stage(
            &mut response,
            &crate::generation_chain::GenerationSource::Hook {
                protocol: OPEN_RESPONSES_2026_04_24,
            },
            None,
        ));
        ledger
            .terminal
            .stage_client_output(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &response);
        let pending = PendingGenerationChainWrite {
            write,
            vendor_publications: vec![publication.clone()],
        };
        (
            CompletionSettlement::buffered(batch, Some(pending), MarkerExecutions::default()),
            response,
            generation_id,
            publication,
            marker.reference,
        )
    }

    async fn assert_generation_stored(
        gateway: &crate::Gateway,
        ledger: &RunLedger,
        generation_id: &str,
        stored: bool,
    ) {
        let mut request = AiRequest::new("model", Vec::new());
        request.meta.source_protocol = Some(OPEN_RESPONSES_2026_04_24);
        request.parameters.continuation = Some(
            stravia_runtime_contract::protocol::ir::Continuation::PreviousResponse(
                generation_id.to_owned(),
            ),
        );
        let continuation = gateway
            .generation_chains
            .begin(ledger.terminal.principal.clone(), request)
            .await;
        if stored {
            let continuation = continuation.expect("delivered generation persisted");
            assert_eq!(continuation.parent_id(), Some(generation_id));
        } else {
            assert!(matches!(
                continuation,
                Err(crate::generation_chain::BeginError::PreviousResponseNotFound)
            ));
        }
    }

    async fn assert_delivered_history(
        observation: &crate::interaction_observation::InteractionObservation,
        response: AiResponse,
        delivered: bool,
    ) {
        use crate::interaction_observation::{AdmissionFacts, IngressStart, RunStart};
        let mut items = response.items;
        items.push(AiItem::function_call_output(
            "client-call",
            serde_json::json!("result"),
        ));
        let _continuation = observation
            .observe_ingress(IngressStart {
                id: "settle-continuation".into(),
                method: "POST".into(),
                path: "/v1/chat/completions".into(),
                protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
            })
            .admit(
                RunStart {
                    id: "settle-continuation".into(),
                    principal: "owner".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "local-route".into(),
                    model_display_name: None,
                    ingress_protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1.to_string(),
                },
                AdmissionFacts {
                    client_request: Arc::new(AiRequest::new("model", items)),
                    has_new_user: false,
                    has_matching_pending_tool_result: true,
                    generation_root_id: None,
                    generation_parent_id: None,
                },
            );
        observation.flush().await.expect("flush delivered history");
        let forest = observation
            .query_forest(Default::default())
            .await
            .expect("observation forest");
        let mut found = false;
        for interaction in forest.roots.iter().flat_map(|root| &root.interactions) {
            let detail = observation
                .get_interaction(&interaction.id, Default::default())
                .await
                .expect("interaction query")
                .expect("interaction detail");
            if let Some(run) = detail
                .runs
                .iter()
                .find(|run| run.id == "settle-continuation")
            {
                assert_eq!(
                    run.parent_run_id.as_deref(),
                    delivered.then_some("settle-run")
                );
                found = true;
            }
        }
        assert!(found, "continuation observation persisted");
    }

    #[tokio::test]
    async fn buffered_settlement_waits_for_body_eof_before_publishing_history() {
        use futures::StreamExt;

        let (_directory, mut session, ledger, observer, gateway, observation) = fixture().await;
        let tracker = crate::plugin::VendorOperationTracker::default();
        let (settlement, response, generation_id, _, marker) = vendor_settlement(
            &gateway,
            &mut session,
            &ledger,
            &tracker,
            stravia_runtime_contract::CancellationToken::new(),
        )
        .await;
        let bound = settlement
            .bind_buffered(
                Response::new(axum::body::Body::from("delivered payload")),
                gateway.clone(),
                session,
                ledger.clone(),
                observer.clone(),
            )
            .await;
        let mut body = bound.into_body().into_data_stream();
        assert_eq!(
            body.next().await.expect("payload").expect("body bytes"),
            "delivered payload"
        );
        // Reading the last data chunk is not the buffered EOF receipt.
        assert_eq!(ledger.terminal.delivery_completed_at(), None);
        assert!(ledger.published_executions().is_empty());
        assert!(body.next().await.is_none());
        assert!(ledger.terminal.delivery_completed_at().is_some());
        assert_eq!(ledger.published_executions(), vec![marker]);
        // The enclosing Run normally finishes observation after body EOF,
        // releasing the commit fence before a continuation can read history.
        observer.finish(crate::interaction_observation::RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: ledger.terminal.delivery_completed_at(),
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: Some(generation_id.clone()),
            generation_root_id: Some(generation_id.clone()),
        });
        assert_generation_stored(&gateway, &ledger, &generation_id, true).await;
        assert_delivered_history(&observation, response, true).await;
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_buffered_body_before_eof_discards_all_publications() {
        use futures::StreamExt;

        for read_payload in [false, true] {
            let (_directory, mut session, ledger, observer, gateway, observation) = fixture().await;
            let tracker = crate::plugin::VendorOperationTracker::default();
            let (settlement, response, generation_id, _, _) = vendor_settlement(
                &gateway,
                &mut session,
                &ledger,
                &tracker,
                stravia_runtime_contract::CancellationToken::new(),
            )
            .await;
            let bound = settlement
                .bind_buffered(
                    Response::new(axum::body::Body::from("unconfirmed payload")),
                    gateway.clone(),
                    session,
                    ledger.clone(),
                    observer.clone(),
                )
                .await;
            let mut body = bound.into_body().into_data_stream();
            if read_payload {
                assert_eq!(
                    body.next().await.expect("payload").expect("body bytes"),
                    "unconfirmed payload"
                );
            }
            drop(body);
            assert_eq!(ledger.terminal.delivery_completed_at(), None);
            assert!(ledger.published_executions().is_empty());
            assert_generation_stored(&gateway, &ledger, &generation_id, false).await;
            assert_delivered_history(&observation, response, false).await;
            observation.shutdown().await;
        }
    }

    #[tokio::test]
    async fn failed_preparation_keeps_vendor_publications_fenced_at_body_eof() {
        let (_directory, mut session, ledger, observer, gateway, observation) = fixture().await;
        let tracker = crate::plugin::VendorOperationTracker::default();
        let caller = stravia_runtime_contract::CancellationToken::new();
        let (settlement, response, generation_id, _, _) =
            vendor_settlement(&gateway, &mut session, &ledger, &tracker, caller.clone()).await;
        caller.cancel();
        let bound = settlement
            .bind_buffered(
                Response::new(axum::body::Body::from("already rendered")),
                gateway.clone(),
                session,
                ledger.clone(),
                observer.clone(),
            )
            .await;
        let bytes = axum::body::to_bytes(bound.into_body(), usize::MAX)
            .await
            .expect("existing response delivery remains unchanged");
        assert_eq!(bytes, "already rendered");
        assert!(ledger.published_executions().is_empty());
        assert_generation_stored(&gateway, &ledger, &generation_id, false).await;
        assert_delivered_history(&observation, response, false).await;
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn confirmed_sent_settlement_survives_caller_cancellation() {
        let (_directory, mut session, ledger, observer, gateway, observation) = fixture().await;
        let tracker = crate::plugin::VendorOperationTracker::default();
        let caller = stravia_runtime_contract::CancellationToken::new();
        let sent_at = sent_at();
        let (mut settlement, response, generation_id, publication, marker) =
            vendor_settlement(&gateway, &mut session, &ledger, &tracker, caller.clone()).await;
        settlement.prepare(&ledger, &observer).await;
        ledger.terminal.publish_prepared_client_completion(sent_at);
        assert_eq!(ledger.terminal.delivery_completed_at(), Some(sent_at));
        caller.cancel();
        assert!(publication.ensure_current().is_err());
        assert!(publication.ensure_generation_current().is_ok());
        let outcome = settlement
            .finish_sent(&gateway, &mut session, &ledger, &observer, sent_at)
            .await;
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.published_platform_executions, vec![marker]);
        assert!(session.client_output_committed());
        assert!(
            ledger
                .terminal
                .generation_committed
                .load(std::sync::atomic::Ordering::Acquire)
        );
        observer.finish(crate::interaction_observation::RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: Some(sent_at),
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: Some(generation_id.clone()),
            generation_root_id: Some(generation_id.clone()),
        });
        assert_generation_stored(&gateway, &ledger, &generation_id, true).await;
        assert_delivered_history(&observation, response, true).await;
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn confirmed_sent_settlement_rejects_vendor_revocation_while_guards_are_held() {
        let (_directory, mut session, ledger, observer, gateway, observation) = fixture().await;
        let tracker = crate::plugin::VendorOperationTracker::default();
        let sent_at = sent_at();
        let (mut settlement, response, generation_id, publication, _) = vendor_settlement(
            &gateway,
            &mut session,
            &ledger,
            &tracker,
            stravia_runtime_contract::CancellationToken::new(),
        )
        .await;
        settlement.prepare(&ledger, &observer).await;
        ledger.terminal.publish_prepared_client_completion(sent_at);
        assert_eq!(ledger.terminal.delivery_completed_at(), Some(sent_at));
        // Poll the actual drain once. It advances the epoch and cancels the
        // publication token synchronously, then blocks on our prepared read locks.
        let drain = tracker.cancel_and_drain_for_test(VENDOR);
        tokio::pin!(drain);
        assert!(matches!(
            futures::poll!(drain.as_mut()),
            std::task::Poll::Pending
        ));
        assert!(publication.ensure_generation_current().is_err());
        let outcome = settlement
            .finish_sent(&gateway, &mut session, &ledger, &observer, sent_at)
            .await;
        assert!(matches!(
            outcome.failures.as_slice(),
            [SettlementFailure::VendorPublication(_)]
        ));
        assert!(outcome.published_platform_executions.is_empty());
        assert!(ledger.published_executions().is_empty());
        assert!(!session.client_output_committed());
        assert!(
            !ledger
                .terminal
                .generation_committed
                .load(std::sync::atomic::Ordering::Acquire)
        );
        drain
            .await
            .expect("revocation drains after settlement releases read guards");
        assert_generation_stored(&gateway, &ledger, &generation_id, false).await;
        assert_delivered_history(&observation, response, true).await;
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn prepared_guards_without_sent_do_not_bypass_caller_cancellation() {
        let (_directory, mut session, ledger, observer, gateway, observation) = fixture().await;
        let tracker = crate::plugin::VendorOperationTracker::default();
        let caller = stravia_runtime_contract::CancellationToken::new();
        let sent_at = sent_at();
        let (mut settlement, response, generation_id, _, _) =
            vendor_settlement(&gateway, &mut session, &ledger, &tracker, caller.clone()).await;
        settlement.prepare(&ledger, &observer).await;
        caller.cancel();
        // The real synchronous callback must not issue a Sent certificate when
        // strict validation fails, even though read guards were acquired earlier.
        ledger.terminal.publish_prepared_client_completion(sent_at);
        assert_eq!(ledger.terminal.delivery_completed_at(), None);
        let outcome = settlement
            .finish_sent(&gateway, &mut session, &ledger, &observer, sent_at)
            .await;
        assert!(matches!(
            outcome.failures.as_slice(),
            [SettlementFailure::VendorPublication(_)]
        ));
        assert!(outcome.published_platform_executions.is_empty());
        assert!(ledger.published_executions().is_empty());
        assert!(!session.client_output_committed());
        assert!(
            !ledger
                .terminal
                .generation_committed
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_generation_stored(&gateway, &ledger, &generation_id, false).await;
        assert_delivered_history(&observation, response, false).await;
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn confirmed_response_publishes_markers_with_delivery_timestamp() {
        let (_directory, mut session, ledger, observer, gateway, _observation) = fixture().await;
        let sent_at = sent_at();
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
        ledger
            .terminal
            .stage_client_output(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, &response);
        let mut settlement =
            CompletionSettlement::buffered(batch, None, MarkerExecutions::default());
        settlement.prepare(&ledger, &observer).await;
        ledger.terminal.publish_prepared_client_completion(sent_at);
        let outcome = settlement
            .finish_sent(&gateway, &mut session, &ledger, &observer, sent_at)
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
        assert_eq!(ledger.terminal.delivery_completed_at(), Some(sent_at));
    }

    #[tokio::test]
    async fn settle_enumerates_failures_and_still_runs_later_steps() {
        let (_directory, mut session, ledger, observer, gateway, _observation) = fixture().await;
        let sent_at = sent_at();
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

        ledger.terminal.stage_client_output(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &AiResponse::new("response-1", "model"),
        );
        let mut settlement = CompletionSettlement::buffered(
            batch,
            Some(PendingGenerationChainWrite {
                write: pending,
                vendor_publications: Vec::new(),
            }),
            MarkerExecutions::default(),
        );
        settlement.prepare(&ledger, &observer).await;
        ledger.terminal.publish_prepared_client_completion(sent_at);
        let outcome = settlement
            .finish_sent(&gateway, &mut session, &ledger, &observer, sent_at)
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
        assert_eq!(ledger.terminal.delivery_completed_at(), Some(sent_at));
        assert!(
            !ledger
                .terminal
                .generation_committed
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
}
