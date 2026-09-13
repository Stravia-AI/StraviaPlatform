mod bundle;
mod codec;
mod grouping;
mod live;
mod query;
pub(crate) mod redaction;
mod retention;
pub(crate) mod scope;
mod store;
mod tail;
mod trace;
mod types;
mod writer;

pub use types::*;

use sqlx::{PgPool, SqlitePool};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use bundle::{BundleRunSnapshot, BundleService, BundleSnapshot};
use store::ObservationStore;
use trace::{
    RUN_LIMIT_BYTES, TOTAL_LIMIT_BYTES, TRACE_SCHEMA_VERSION, TraceHandle, TraceManager,
    TraceRecord,
};
use writer::WriterCommand;

#[derive(Clone)]
pub(crate) struct InteractionObservation {
    inner: Arc<Inner>,
}
struct Inner {
    store: ObservationStore,
    writer: mpsc::Sender<WriterCommand>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    updates: broadcast::Sender<ObservationUpdate>,
    live_content: Arc<live::LiveState>,
    trace_sequence: Arc<AtomicI64>,
    debug: AtomicBool,
    retention_days: Arc<AtomicU32>,
    traces: TraceManager,
    bundles: BundleService,
    active_traces: Arc<Mutex<HashMap<String, TraceHandle>>>,
    partial_trace_count: Arc<AtomicU64>,
    unpersisted_gaps: Arc<Mutex<UnpersistedGaps>>,
    ephemeral_root: Option<PathBuf>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct UnpersistedGaps {
    generation: u64,
    runs: HashMap<String, (i64, u64)>,
}

impl UnpersistedGaps {
    fn record(&mut self, run_id: &str, occurred_at: i64) {
        self.generation += 1;
        self.runs
            .insert(run_id.to_owned(), (occurred_at, self.generation));
    }

    fn visible(&self, now: i64, retention_days: u32) -> bool {
        self.runs
            .values()
            .any(|(at, _)| writer::expires(*at, retention_days) > now)
    }

    fn expire(&mut self, now: i64, retention_days: u32) {
        self.runs
            .retain(|_, (at, _)| writer::expires(*at, retention_days) > now);
    }

    fn clear_removed(&mut self, covered: &HashMap<String, (i64, u64)>, removed: &[String]) {
        for run_id in removed {
            if self.runs.get(run_id) == covered.get(run_id) {
                self.runs.remove(run_id);
            }
        }
    }
}

impl InteractionObservation {
    pub(crate) async fn new(
        sqlite: Option<SqlitePool>,
        postgres: Option<PgPool>,
        data_dir: PathBuf,
        retention_days: u32,
        persistent: bool,
    ) -> Self {
        let store = ObservationStore::new(sqlite, postgres)
            .expect("Gateway provides exactly one observation SQL backend");
        if store.recover_after_restart().await.is_err() {
            tracing::warn!("observation restart recovery unavailable");
        }
        let ephemeral_root = (!persistent).then(|| {
            std::env::temp_dir().join(format!("stravia-observation-{}", uuid::Uuid::new_v4()))
        });
        let trace_data_dir = ephemeral_root.clone().unwrap_or(data_dir);
        let traces = TraceManager::new(trace_data_dir).unwrap_or_else(|_| {
            tracing::warn!("observation trace storage unavailable");
            TraceManager::degraded()
        });
        match store.manifest_ids().await {
            Ok((retained, tombstoned)) => match traces.reconcile(retained, tombstoned).await {
                Ok(report) => {
                    if store
                        .delete_manifests(&report.removed_tombstones)
                        .await
                        .is_err()
                    {
                        tracing::warn!("trace tombstone reconciliation persistence unavailable");
                    }
                }
                Err(_) => tracing::warn!("trace file reconciliation unavailable"),
            },
            // 数据库不可读不等于没有保留记录，不能据此删除诊断文件。
            Err(_) => tracing::warn!("trace manifest reconciliation unavailable"),
        }
        let trace_sequence = Arc::new(AtomicI64::new(match store.max_sequence().await {
            Ok(sequence) => sequence,
            Err(_) => {
                tracing::warn!("observation sequence recovery unavailable");
                0
            }
        }));
        let (updates, _) = broadcast::channel(2048);
        let live_content = Arc::new(live::LiveState::default());
        let retention_days = Arc::new(AtomicU32::new(retention_days));
        let (_, partial_count) = store.debug_manifest_counts().await.unwrap_or((0, 0));
        let active_traces = Arc::new(Mutex::new(HashMap::new()));
        let partial_trace_count = Arc::new(AtomicU64::new(partial_count));
        let unpersisted_gaps = Arc::new(Mutex::new(UnpersistedGaps::default()));
        let (writer, task) = writer::spawn(
            store.clone(),
            Arc::clone(&retention_days),
            updates.clone(),
            Arc::clone(&trace_sequence),
            traces.clone(),
            Arc::clone(&active_traces),
            Arc::clone(&partial_trace_count),
            Arc::clone(&unpersisted_gaps),
            Arc::clone(&live_content),
        );
        Self {
            inner: Arc::new(Inner {
                store,
                writer,
                writer_task: Mutex::new(Some(task)),
                updates,
                live_content,
                trace_sequence,
                debug: AtomicBool::new(false),
                retention_days,
                traces,
                bundles: BundleService::default(),
                active_traces,
                partial_trace_count,
                unpersisted_gaps,
                ephemeral_root,
                stopped: AtomicBool::new(false),
            }),
        }
    }
    pub(crate) fn observe_ingress(&self, mut start: IngressStart) -> IngressObserver {
        let received_at = writer::now();
        redaction::redact_ingress(&mut start);
        let debug = self.inner.debug.load(Ordering::Acquire);
        // 保留一个控制槽，最终状态与 Trace 关闭不能被普通事件挤出队列。
        let finalization = self.inner.writer.clone().try_reserve_owned().ok();
        let trace = (debug && finalization.is_some()).then(|| self.inner.traces.create());
        let websocket = start.method == "WEBSOCKET";
        IngressObserver {
            observation: self.clone(),
            received_at,
            start: Some(start),
            debug_enabled: debug,
            trace,
            finalization,
            rejection_id: None,
            websocket,
        }
    }
    pub(crate) async fn query_forest(&self, q: ForestQuery) -> anyhow::Result<ForestPage> {
        self.inner.store.query_forest(q).await
    }
    pub(crate) async fn get_interaction_summary(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionSnapshot>> {
        self.inner.store.get_interaction_summary(id, filters).await
    }
    pub(crate) async fn get_interaction(
        &self,
        id: &str,
        filters: ForestQuery,
    ) -> anyhow::Result<Option<InteractionDetail>> {
        self.inner.store.get_interaction(id, filters).await
    }
    pub(crate) async fn get_interaction_events(
        &self,
        id: &str,
        query: InteractionEventsQuery,
    ) -> anyhow::Result<Option<InteractionEventsPage>> {
        self.inner.store.get_interaction_events(id, query).await
    }
    pub(crate) async fn query_rejections(
        &self,
        q: RejectionQuery,
    ) -> anyhow::Result<RejectionPage> {
        self.inner.store.query_rejections(q).await
    }
    pub(crate) async fn get_rejection(&self, id: &str) -> anyhow::Result<Option<RejectionDetail>> {
        self.inner.store.get_rejection(id).await
    }
    pub(crate) fn debug_state(&self) -> DebugState {
        let active_partial = self
            .inner
            .active_traces
            .lock()
            .expect("trace registry")
            .values()
            .filter(|trace| trace.manifest().status == "partial")
            .count() as u64;
        DebugState {
            enabled: self.inner.debug.load(Ordering::Acquire),
            run_limit_bytes: RUN_LIMIT_BYTES,
            total_limit_bytes: TOTAL_LIMIT_BYTES,
            retained_bytes: self.inner.traces.retained_bytes(),
            partial_trace_count: self
                .inner
                .partial_trace_count
                .load(Ordering::Acquire)
                .saturating_add(active_partial),
            retention_days: self.inner.retention_days.load(Ordering::Relaxed),
        }
    }
    pub(crate) fn set_debug_enabled(&self, enabled: bool) -> DebugState {
        self.inner.debug.store(enabled, Ordering::Release);
        self.debug_state()
    }
    pub(crate) async fn set_retention_days(&self, days: u32) -> anyhow::Result<DebugState> {
        self.inner.store.update_retention(days).await?;
        self.inner.retention_days.store(days, Ordering::Release);
        let _ = self.inner.writer.send(WriterCommand::ClearTail).await;
        self.sweep().await?;
        Ok(self.debug_state())
    }
    pub(crate) async fn clear_history(&self) -> anyhow::Result<ClearHistoryResult> {
        let covered = self
            .inner
            .unpersisted_gaps
            .lock()
            .expect("observation gaps")
            .runs
            .clone();
        let _ = self.inner.writer.send(WriterCommand::ClearTail).await;
        self.flush().await?;
        let mut known_runs = Vec::new();
        for run_id in covered.keys() {
            if self.inner.store.contains_gap_run(run_id).await? {
                known_runs.push(run_id.clone());
            }
        }
        let result = self.inner.store.mark_clear_tombstones().await?;
        let (_, tomb) = self.inner.store.manifest_ids().await?;
        let mut deleted = Vec::new();
        for id in &tomb {
            match self.inner.traces.delete(id).await {
                Ok(()) => deleted.push(id.clone()),
                Err(error) => tracing::warn!(trace_id=%id,%error,"trace clear failed"),
            }
        }
        self.inner.store.delete_manifests(&deleted).await?;
        self.purge_history(None).await?;
        let mut removed = Vec::new();
        for run_id in known_runs {
            if !self.inner.store.contains_gap_run(&run_id).await? {
                removed.push(run_id);
            }
        }
        // Missing admission ownership is not proof that cleanup covered the loss.
        self.inner
            .unpersisted_gaps
            .lock()
            .expect("observation gaps")
            .clear_removed(&covered, &removed);
        let (_, partial) = self
            .inner
            .store
            .debug_manifest_counts()
            .await
            .unwrap_or((0, 0));
        self.inner
            .partial_trace_count
            .store(partial, Ordering::Release);
        Ok(result)
    }
    pub(crate) async fn sweep_retention(&self) -> anyhow::Result<()> {
        self.sweep().await
    }
    async fn sweep(&self) -> anyhow::Result<()> {
        self.flush().await?;
        let now = chrono::Utc::now().timestamp_millis();
        let ids = self.inner.store.mark_expired_tombstones(now).await?;
        let mut deleted = Vec::new();
        for id in ids {
            if self.inner.traces.delete(&id).await.is_ok() {
                deleted.push(id)
            }
        }
        self.inner.store.delete_manifests(&deleted).await?;
        self.purge_history(Some(now)).await?;
        self.inner
            .unpersisted_gaps
            .lock()
            .expect("observation gaps")
            .expire(now, self.inner.retention_days.load(Ordering::Acquire));
        let (_, partial) = self
            .inner
            .store
            .debug_manifest_counts()
            .await
            .unwrap_or((0, 0));
        self.inner
            .partial_trace_count
            .store(partial, Ordering::Release);
        Ok(())
    }
    pub(crate) fn subscribe(&self, after: i64) -> ObservationStream {
        let store = self.inner.store.clone();
        let live_content = Arc::clone(&self.inner.live_content);
        let mut live = self.inner.updates.subscribe();
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let min = store.min_sequence().await.ok().flatten();
            let max = store.max_sequence().await.unwrap_or(0);
            if after > max
                || (after > 0 && min.map_or(after < max, |min| after < min.saturating_sub(1)))
            {
                let _ = tx
                    .send(ObservationUpdate::ResetRequired {
                        snapshot_sequence: max,
                    })
                    .await;
                return;
            }
            let mut last = after;
            if replay_to(&store, &tx, &mut last, max).await.is_err() {
                let _ = tx
                    .send(ObservationUpdate::ResetRequired {
                        snapshot_sequence: max,
                    })
                    .await;
                return;
            }
            if tx
                .send(ObservationUpdate::LiveSnapshot {
                    blocks: live_content.snapshot(),
                })
                .await
                .is_err()
            {
                return;
            }
            loop {
                match live.recv().await {
                    Ok(ObservationUpdate::Event(event)) => {
                        if event.sequence <= last {
                            continue;
                        }
                        if event.sequence > last.saturating_add(1) {
                            if replay_to(&store, &tx, &mut last, event.sequence)
                                .await
                                .is_err()
                                || last < event.sequence
                            {
                                let _ = tx
                                    .send(ObservationUpdate::ResetRequired {
                                        snapshot_sequence: store
                                            .max_sequence()
                                            .await
                                            .unwrap_or(last),
                                    })
                                    .await;
                                return;
                            }
                            continue;
                        }
                        last = event.sequence;
                        if tx.send(ObservationUpdate::Event(event)).await.is_err() {
                            return;
                        }
                    }
                    Ok(update) => {
                        if tx.send(update).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = store.max_sequence().await.unwrap_or(max);
                        let _ = tx
                            .send(ObservationUpdate::ResetRequired {
                                snapshot_sequence: snapshot,
                            })
                            .await;
                        return;
                    }
                    Err(_) => return,
                }
            }
        });
        Box::pin(ReceiverStream::new(rx))
    }
    pub(crate) async fn issue_bundle_ticket(
        &self,
        request: BundleRequest,
    ) -> anyhow::Result<DownloadTicket> {
        if matches!(request.kind, BundleResourceKind::Interaction) {
            let (done, receive) = oneshot::channel();
            self.inner
                .writer
                .send(WriterCommand::FlushInteraction {
                    interaction_id: request.resource_id.clone(),
                    done,
                })
                .await
                .map_err(|_| anyhow::anyhow!("observation writer unavailable"))?;
            receive
                .await
                .map_err(|_| anyhow::anyhow!("observation writer unavailable"))?;
        }
        let max = self.inner.store.max_sequence().await?;
        let through = request.through_sequence.unwrap_or(max).min(max);
        let exported_at = chrono::Utc::now().timestamp_millis();
        let snapshot = match request.kind {
            BundleResourceKind::Interaction => {
                let detail = self
                    .inner
                    .store
                    .get_interaction_for_bundle(&request.resource_id, through)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("interaction not found"))?;
                let mut snapshot_events: Vec<ObservationEvent> = detail
                    .runs
                    .iter()
                    .flat_map(|run| run.events.iter())
                    .filter(|event| event.sequence <= through)
                    .cloned()
                    .collect();
                snapshot_events.sort_unstable_by_key(|event| event.sequence);
                let admitted: std::collections::HashSet<String> = snapshot_events
                    .iter()
                    .filter(|event| event.kind == "run_admitted")
                    .filter_map(|event| event.run_id.clone())
                    .collect();
                if admitted.is_empty() {
                    anyhow::bail!("bundle snapshot unavailable");
                }
                let projected_status = project_bundle_status(&snapshot_events);
                let summary =
                    project_bundle_summary(&detail, &snapshot_events, through, &projected_status);
                let mut runs = Vec::with_capacity(admitted.len());
                for run in detail.runs.iter().filter(|run| admitted.contains(&run.id)) {
                    let active = {
                        self.inner
                            .active_traces
                            .lock()
                            .expect("trace registry")
                            .get(&run.id)
                            .cloned()
                    };
                    let (status, bytes, reasons, trace) = if let Some(handle) = active {
                        let manifest = handle.manifest();
                        let snap = handle.snapshot(through).await.ok();
                        (
                            manifest.status,
                            manifest.bytes_written,
                            manifest.reasons,
                            snap,
                        )
                    } else if let Some(manifest) = &run.trace {
                        let snap = self
                            .inner
                            .traces
                            .snapshot(&manifest.trace_id, through)
                            .await
                            .ok();
                        (
                            manifest.status.clone(),
                            manifest.bytes_written,
                            manifest.reasons.clone(),
                            snap,
                        )
                    } else {
                        ("none".to_owned(), 0, Vec::new(), None)
                    };
                    runs.push(BundleRunSnapshot {
                        run_id: run.id.clone(),
                        debug_enabled: run.debug_enabled,
                        trace_status: status,
                        bytes_written: bytes,
                        reasons,
                        trace,
                    });
                }
                BundleSnapshot {
                    kind: BundleResourceKind::Interaction,
                    resource_id: request.resource_id,
                    exported_at,
                    through_sequence: through,
                    resource_status: projected_status,
                    summary,
                    runs,
                }
            }
            BundleResourceKind::RejectedRequest => {
                let detail = self
                    .inner
                    .store
                    .get_rejection(&request.resource_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("rejected request not found"))?;
                let snapshot_events: Vec<_> = detail
                    .events
                    .iter()
                    .filter(|event| event.sequence <= through)
                    .cloned()
                    .collect();
                if snapshot_events.is_empty() {
                    anyhow::bail!("bundle snapshot unavailable");
                }
                let summary = serde_json::json!({"schema_version":1,"rejection_id":request.resource_id.clone(),"through_event_sequence":through,"events":snapshot_events});
                let mut runs = Vec::new();
                if let Some(manifest) = &detail.trace {
                    runs.push(BundleRunSnapshot {
                        run_id: detail.rejection.id.clone(),
                        debug_enabled: detail.rejection.debug_enabled,
                        trace_status: manifest.status.clone(),
                        bytes_written: manifest.bytes_written,
                        reasons: manifest.reasons.clone(),
                        trace: self
                            .inner
                            .traces
                            .snapshot(&manifest.trace_id, through)
                            .await
                            .ok(),
                    });
                } else {
                    runs.push(BundleRunSnapshot {
                        run_id: detail.rejection.id.clone(),
                        debug_enabled: detail.rejection.debug_enabled,
                        trace_status: "none".into(),
                        bytes_written: 0,
                        reasons: if detail.rejection.debug_enabled {
                            vec!["trace_missing".into()]
                        } else {
                            Vec::new()
                        },
                        trace: None,
                    });
                }
                BundleSnapshot {
                    kind: BundleResourceKind::RejectedRequest,
                    resource_id: request.resource_id,
                    exported_at,
                    through_sequence: through,
                    resource_status: "rejected".into(),
                    summary,
                    runs,
                }
            }
        };
        let mut snapshot = snapshot;
        let mut references = std::collections::BTreeSet::new();
        for run in &snapshot.runs {
            if let Some(trace) = &run.trace {
                for value in load_trace_values(trace.clone()).await? {
                    collect_captured_artifacts(&value, &mut references);
                }
            }
        }
        if !references.is_empty() {
            let mut media = Vec::with_capacity(references.len());
            for reference in references {
                let id = reference.trim_start_matches("https://stravia/artifact/");
                let available = self.inner.store.artifact_available(id, exported_at).await?;
                media.push(serde_json::json!({
                    "artifact_reference": reference,
                    "media_externalized": true,
                    "content_capture": if available { "reference_only" } else { "unrecoverable" },
                    "reason": if available { "media_not_embedded_in_bundle" } else { "artifact_expired_or_unavailable" },
                    "checked_at": exported_at
                }));
            }
            snapshot.summary["externalized_media"] = serde_json::Value::Array(media);
        }
        Ok(self.inner.bundles.issue(snapshot))
    }
    pub(crate) async fn consume_bundle_ticket(&self, ticket: &str) -> anyhow::Result<BundleStream> {
        Ok(self
            .inner
            .bundles
            .consume(ticket)
            .map_err(anyhow::Error::new)?
            .stream)
    }
    async fn purge_history(&self, expired_before: Option<i64>) -> anyhow::Result<()> {
        let (done, receiver) = oneshot::channel();
        self.inner
            .writer
            .send(WriterCommand::Purge {
                expired_before,
                done,
            })
            .await
            .map_err(|_| anyhow::anyhow!("observation writer unavailable"))?;
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("observation writer unavailable"))?
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.inner
            .writer
            .send(WriterCommand::Barrier(sender))
            .await
            .map_err(|_| anyhow::anyhow!("observation writer unavailable"))?;
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("observation writer unavailable"))?;
        Ok(())
    }
    pub(crate) fn stop_background(&self) {
        if !self.inner.stopped.swap(true, Ordering::AcqRel) {
            let (tx, _) = oneshot::channel();
            let _ = self.inner.writer.try_send(WriterCommand::Shutdown(tx));
        }
    }
    pub(crate) async fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        let task = { self.inner.writer_task.lock().expect("writer lock").take() };
        if let Some(task) = task {
            let (tx, rx) = oneshot::channel();
            if self
                .inner
                .writer
                .send(WriterCommand::Shutdown(tx))
                .await
                .is_ok()
            {
                let _ = rx.await;
            }
            let _ = task.await;
        }
        self.inner.traces.shutdown().await;
        if let Some(root) = &self.inner.ephemeral_root {
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }
}

async fn replay_to(
    store: &ObservationStore,
    sender: &mpsc::Sender<ObservationUpdate>,
    last: &mut i64,
    through: i64,
) -> anyhow::Result<()> {
    while *last < through {
        let before = *last;
        for event in store.replay(before).await? {
            if event.sequence > through {
                break;
            }
            *last = event.sequence;
            sender
                .send(ObservationUpdate::Event(event))
                .await
                .map_err(|_| anyhow::anyhow!("observation subscriber closed"))?;
        }
        if *last == before {
            break;
        }
    }
    Ok(())
}

pub(crate) struct IngressObserver {
    received_at: i64,
    observation: InteractionObservation,
    start: Option<IngressStart>,
    debug_enabled: bool,
    trace: Option<TraceHandle>,
    finalization: Option<mpsc::OwnedPermit<WriterCommand>>,
    rejection_id: Option<String>,
    websocket: bool,
}

#[derive(Clone)]
pub(crate) struct IngressCapture {
    trace: TraceHandle,
}

impl IngressCapture {
    pub(crate) const MAX_BODY_BYTES: usize = RUN_LIMIT_BYTES as usize;

    pub(crate) fn record(&self, event: RunEvent) {
        record_trace(&self.trace, None, None, event);
    }

    pub(crate) fn mark_partial(&self, reason: &'static str) {
        self.trace.mark_partial(reason, false);
    }
}

impl IngressObserver {
    pub(crate) fn is_websocket(&self) -> bool {
        self.websocket
    }
    pub(crate) fn capture(&self) -> Option<IngressCapture> {
        self.debug_enabled
            .then(|| self.trace.clone())
            .flatten()
            .map(|trace| IngressCapture { trace })
    }
    pub(crate) fn reject_pending(&mut self, outcome: RejectedOutcome) {
        self.reject_inner(outcome);
    }
    pub(crate) fn record_debug(&self, event: impl FnOnce() -> RunEvent) {
        if self.debug_enabled {
            self.record(event());
        }
    }
    pub(crate) fn record(&self, event: RunEvent) {
        if let Some(trace) = &self.trace {
            if matches!(event, RunEvent::ObservationGap { .. }) {
                trace.mark_partial("observation_gap", false);
                return;
            }
            record_trace(trace, None, None, event)
        }
    }
    pub(crate) fn admit(mut self, mut start: RunStart) -> RunObserver {
        start.ingress_received_at = self.received_at;
        let ingress = self.start.take();
        let debug_enabled = self.observation.inner.debug.load(Ordering::Acquire);
        let discarded_trace = if !debug_enabled {
            self.trace.take()
        } else {
            None
        };
        if debug_enabled && self.trace.is_none() && self.finalization.is_some() {
            let trace = self.observation.inner.traces.create();
            trace.mark_partial("debug_enabled_after_ingress", false);
            self.trace = Some(trace);
        }
        if let Some(trace) = &self.trace {
            self.observation
                .inner
                .active_traces
                .lock()
                .expect("trace registry")
                .insert(start.id.clone(), trace.clone());
        }
        let protected = self.trace.as_ref().map_or_else(
            redaction::ProtectedSecrets::default,
            TraceHandle::protected_secrets,
        );
        let inner = Arc::new(RunObserverInner {
            observation: self.observation.clone(),
            run_id: start.id.clone(),
            principal: start.principal.clone(),
            debug_enabled,
            trace: self.trace.take(),
            terminal: AtomicBool::new(false),
            gap: AtomicBool::new(false),
            finalization: Mutex::new(self.finalization.take()),
            pending_finish: Mutex::new(None),
            pending_input: Mutex::new(None),
            pending_tool_results: Mutex::new(Vec::new()),
            thinking_redaction: Mutex::new(HashMap::new()),
            visible_redaction: Mutex::new(redaction::VisibleTextRedactor::with_protected(
                protected.clone(),
            )),
            protected,
        });
        if self
            .observation
            .inner
            .writer
            .try_send(WriterCommand::Admit {
                start,
                debug_enabled,
                trace: inner.trace.clone(),
                discarded_trace,
            })
            .is_err()
        {
            inner.gap.store(true, Ordering::Release);
            self.observation
                .inner
                .unpersisted_gaps
                .lock()
                .expect("observation gaps")
                .record(&inner.run_id, writer::now());
        }
        drop(ingress);
        RunObserver { inner }
    }
    pub(crate) fn reject(mut self, outcome: RejectedOutcome) {
        self.reject_inner(outcome);
    }
    fn reject_inner(&mut self, mut outcome: RejectedOutcome) {
        redaction::redact_rejected_outcome(&mut outcome);
        if let Some(start) = self.start.take() {
            if self
                .observation
                .inner
                .writer
                .try_send(WriterCommand::Reject {
                    ingress: start.clone(),
                    outcome,
                    debug_enabled: self.debug_enabled,
                })
                .is_err()
            {
                tracing::warn!(rejection_id=%start.id,"rejection observation queue full")
            };
            self.rejection_id = Some(start.id);
        }
    }
}
impl Drop for IngressObserver {
    fn drop(&mut self) {
        if self.start.is_some() {
            self.reject_inner(RejectedOutcome {
                stage: "ingress".into(),
                code: "request_aborted".into(),
                status_code: 499,
            });
        }
        if let Some(permit) = self.finalization.take() {
            permit.send(WriterCommand::Finalize {
                run_id: None,
                rejection_id: self.rejection_id.take(),
                trace: self.trace.take(),
                pending_finish: None,
                gap: false,
            });
        }
    }
}

#[derive(Clone)]
pub(crate) struct RunObserver {
    inner: Arc<RunObserverInner>,
}
struct RunObserverInner {
    observation: InteractionObservation,
    run_id: String,
    principal: String,
    debug_enabled: bool,
    trace: Option<TraceHandle>,
    terminal: AtomicBool,
    gap: AtomicBool,
    finalization: Mutex<Option<mpsc::OwnedPermit<WriterCommand>>>,
    pending_finish: Mutex<Option<RunOutcome>>,
    // Canonical user text remains memory-only until Model Turn protection succeeds.
    pending_input: Mutex<Option<String>>,
    pending_tool_results: Mutex<Vec<RunEvent>>,
    thinking_redaction: Mutex<HashMap<(String, String), redaction::VisibleTextRedactor>>,
    visible_redaction: Mutex<redaction::VisibleTextRedactor>,
    protected: redaction::ProtectedSecrets,
}
impl RunObserver {
    /// Admission only: use the received canonical window, never effective model history.
    pub(crate) fn capture_input_preview(
        &self,
        input: &[stravia_runtime_contract::protocol::ir::AiItem],
    ) {
        *self
            .inner
            .pending_input
            .lock()
            .expect("input preview state") = redaction::user_input_text(input);
    }

    /// 客户端返回先留在内存，和输入预览共用凭据映射完成后的发布边界。
    pub(crate) fn capture_client_tool_results(
        &self,
        input: &[stravia_runtime_contract::protocol::ir::AiItem],
    ) {
        use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent, Role};
        let mut pending = self
            .inner
            .pending_tool_results
            .lock()
            .expect("tool result state");
        for item in input {
            let before = pending.len();
            if let MessageContent::Blocks(blocks) = &item.content {
                for block in blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = block
                    {
                        pending.push(RunEvent::ClientToolResult {
                            tool_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: is_error.unwrap_or(false),
                        });
                    }
                }
            }
            if pending.len() == before
                && item.role == Role::Tool
                && let Some(tool_id) = &item.tool_call_id
            {
                pending.push(RunEvent::ClientToolResult {
                    tool_id: tool_id.clone(),
                    content: serde_json::to_value(&item.content).expect("canonical tool content"),
                    is_error: false,
                });
            }
        }
    }

    /// Publish once, only after all active and newly discovered mappings are registered.
    pub(crate) fn publish_input_preview(&self) {
        let tool_results = std::mem::take(
            &mut *self
                .inner
                .pending_tool_results
                .lock()
                .expect("tool result state"),
        );
        self.send_tool_results(tool_results);
        let Some(text) = self
            .inner
            .pending_input
            .lock()
            .expect("input preview state")
            .take()
        else {
            return;
        };
        let preview = redaction::input_preview(text, &self.inner.protected);
        if self
            .inner
            .observation
            .inner
            .writer
            .try_send(WriterCommand::InputPreview {
                run_id: self.inner.run_id.clone(),
                preview,
            })
            .is_err()
        {
            self.inner.gap.store(true, Ordering::Release);
            self.inner
                .observation
                .inner
                .unpersisted_gaps
                .lock()
                .expect("observation gaps")
                .record(&self.inner.run_id, writer::now());
        }
    }

    /// Diagnostic-only: pass the received normalized window, never materialized history.
    pub(crate) fn observe_client_input(
        &self,
        input: &[stravia_runtime_contract::protocol::ir::AiItem],
    ) {
        self.send_tail(tail::Window::capture(input), false);
    }
    /// Call only after successful delivery and Generation commit, with client-visible output.
    pub(crate) fn observe_client_completion(
        &self,
        input: &[stravia_runtime_contract::protocol::ir::AiItem],
        output: &[stravia_runtime_contract::protocol::ir::AiItem],
    ) {
        let window = tail::Window::capture(input).and_then(|mut window| {
            window
                .append(tail::Window::capture(output)?)
                .then_some(window)
        });
        self.send_tail(window, true);
    }
    fn send_tail(&self, window: Option<tail::Window>, completed: bool) {
        if self
            .inner
            .observation
            .inner
            .writer
            .try_send(WriterCommand::Tail {
                run_id: self.inner.run_id.clone(),
                principal: self.inner.principal.clone(),
                window,
                completed,
            })
            .is_err()
        {
            self.inner.gap.store(true, Ordering::Release);
        }
    }
    pub(crate) fn protect_secrets<'a>(&self, secrets: impl IntoIterator<Item = &'a str>) {
        self.inner.protected.register(secrets);
    }
    pub(crate) fn debug_enabled(&self) -> bool {
        self.inner.debug_enabled
    }
    pub(crate) fn record_debug(&self, event: impl FnOnce() -> RunEvent) {
        if self.inner.debug_enabled {
            self.record(event());
        }
    }
    pub(crate) fn record(&self, event: RunEvent) {
        if !self.inner.debug_enabled
            && matches!(event, RunEvent::Checkpoint { .. } | RunEvent::Wire { .. })
        {
            return;
        }
        if let RunEvent::ModelThinkingDelta {
            model_turn_id,
            attempt_id,
            text,
        } = event
        {
            let ready = self
                .inner
                .thinking_redaction
                .lock()
                .expect("thinking redaction state")
                .entry((model_turn_id.clone(), attempt_id.clone()))
                .or_insert_with(|| {
                    redaction::VisibleTextRedactor::with_protected(self.inner.protected.clone())
                })
                .push(text);
            if let Some(text) = ready {
                self.send_event(RunEvent::ModelThinkingDelta {
                    model_turn_id,
                    attempt_id,
                    text,
                });
            }
            return;
        }
        if let RunEvent::ModelThinkingFinished {
            model_turn_id,
            attempt_id,
        } = &event
        {
            self.flush_thinking(model_turn_id, attempt_id);
        }
        if let RunEvent::TargetAttemptFinished {
            model_turn_id,
            attempt_id,
            ..
        } = &event
        {
            if self.flush_thinking(model_turn_id, attempt_id) {
                self.send_event(RunEvent::ModelThinkingFinished {
                    model_turn_id: model_turn_id.clone(),
                    attempt_id: attempt_id.clone(),
                });
            }
        }
        if let RunEvent::ClientVisibleContentDelta { text } = event {
            let ready = self
                .inner
                .visible_redaction
                .lock()
                .expect("visible redaction state")
                .push(text);
            if let Some(text) = ready {
                self.send_event(RunEvent::ClientVisibleContentDelta { text });
            }
            return;
        }
        if matches!(
            event,
            RunEvent::ClientOutputCommitted | RunEvent::DeliveryFinished { .. }
        ) {
            self.flush_visible();
        }
        self.send_event(event);
    }
    fn flush_thinking(&self, model_turn_id: &str, attempt_id: &str) -> bool {
        let state = self
            .inner
            .thinking_redaction
            .lock()
            .expect("thinking redaction state")
            .remove(&(model_turn_id.to_owned(), attempt_id.to_owned()));
        let Some(mut state) = state else { return false };
        if let Some(text) = state.finish() {
            self.send_event(RunEvent::ModelThinkingDelta {
                model_turn_id: model_turn_id.to_owned(),
                attempt_id: attempt_id.to_owned(),
                text,
            });
        }
        true
    }
    fn finish_thinking(&self) {
        let pending = std::mem::take(
            &mut *self
                .inner
                .thinking_redaction
                .lock()
                .expect("thinking redaction state"),
        );
        for ((model_turn_id, attempt_id), mut state) in pending {
            if let Some(text) = state.finish() {
                self.send_event(RunEvent::ModelThinkingDelta {
                    model_turn_id: model_turn_id.clone(),
                    attempt_id: attempt_id.clone(),
                    text,
                });
            }
            self.send_event(RunEvent::ModelThinkingFinished {
                model_turn_id,
                attempt_id,
            });
        }
    }
    fn flush_visible(&self) {
        let ready = self
            .inner
            .visible_redaction
            .lock()
            .expect("visible redaction state")
            .finish();
        if let Some(text) = ready {
            self.send_event(RunEvent::ClientVisibleContentDelta { text });
        }
    }
    fn send_tool_results(&self, mut events: Vec<RunEvent>) {
        if events.is_empty() {
            return;
        }
        for event in &mut events {
            self.inner.protected.event(event);
            redaction::redact_run_event(event);
        }
        if self
            .inner
            .observation
            .inner
            .writer
            .try_send(WriterCommand::ClientToolResults {
                run_id: self.inner.run_id.clone(),
                events,
            })
            .is_err()
        {
            self.inner.gap.store(true, Ordering::Release);
            self.inner
                .observation
                .inner
                .unpersisted_gaps
                .lock()
                .expect("observation gaps")
                .record(&self.inner.run_id, writer::now());
        }
    }
    fn send_event(&self, mut event: RunEvent) {
        if matches!(event, RunEvent::ClientToolResult { .. }) {
            self.send_tool_results(vec![event]);
            return;
        }
        self.inner.protected.event(&mut event);
        redaction::redact_run_event(&mut event);
        if matches!(event, RunEvent::ObservationGap { .. }) {
            if let Some(trace) = &self.inner.trace {
                trace.mark_partial("observation_gap", false);
            }
        }
        if matches!(event, RunEvent::Checkpoint { .. } | RunEvent::Wire { .. }) {
            if let Some(trace) = &self.inner.trace {
                // Trace owns its queue. The next durable observation boundary includes this
                // record; already published snapshot cutoffs cannot include future capture.
                let sequence = self
                    .inner
                    .observation
                    .inner
                    .trace_sequence
                    .load(Ordering::Acquire)
                    .saturating_add(1);
                record_trace_at(trace, Some(&self.inner.run_id), None, event, sequence);
            }
            return;
        }
        if self.inner.gap.swap(false, Ordering::AcqRel) {
            let _ = self
                .inner
                .observation
                .inner
                .writer
                .try_send(WriterCommand::Event {
                    run_id: self.inner.run_id.clone(),
                    event: RunEvent::ObservationGap {
                        reason: "writer_overflow".into(),
                    },
                });
        }
        if self
            .inner
            .observation
            .inner
            .writer
            .try_send(WriterCommand::Event {
                run_id: self.inner.run_id.clone(),
                event,
            })
            .is_err()
        {
            self.inner.gap.store(true, Ordering::Release);
            let observation = &self.inner.observation.inner;
            observation
                .unpersisted_gaps
                .lock()
                .expect("observation gaps")
                .record(&self.inner.run_id, writer::now());
        }
    }
    pub(crate) fn finish(&self, mut outcome: RunOutcome) {
        self.flush_visible();
        self.finish_thinking();
        self.inner.protected.text(&mut outcome.status);
        if let Some(reason) = &mut outcome.terminal_reason {
            self.inner.protected.text(reason);
        }
        redaction::redact_run_outcome(&mut outcome);
        if !self.inner.terminal.swap(true, Ordering::AcqRel) {
            if let Err(error) =
                self.inner
                    .observation
                    .inner
                    .writer
                    .try_send(WriterCommand::Finish {
                        run_id: self.inner.run_id.clone(),
                        outcome,
                    })
            {
                if let WriterCommand::Finish { outcome, .. } = error.into_inner() {
                    *self.inner.pending_finish.lock().expect("terminal state") = Some(outcome);
                    self.inner.gap.store(true, Ordering::Release);
                    self.inner
                        .observation
                        .inner
                        .unpersisted_gaps
                        .lock()
                        .expect("observation gaps")
                        .record(&self.inner.run_id, writer::now());
                }
            }
        }
    }
}
impl Drop for RunObserverInner {
    fn drop(&mut self) {
        for ((model_turn_id, attempt_id), mut state) in std::mem::take(
            self.thinking_redaction
                .get_mut()
                .expect("thinking redaction state"),
        ) {
            let delta = state.finish().map(|text| RunEvent::ModelThinkingDelta {
                model_turn_id: model_turn_id.clone(),
                attempt_id: attempt_id.clone(),
                text,
            });
            for event in [
                delta,
                Some(RunEvent::ModelThinkingFinished {
                    model_turn_id,
                    attempt_id,
                }),
            ]
            .into_iter()
            .flatten()
            {
                if self
                    .observation
                    .inner
                    .writer
                    .try_send(WriterCommand::Event {
                        run_id: self.run_id.clone(),
                        event,
                    })
                    .is_err()
                {
                    *self.gap.get_mut() = true;
                }
            }
        }
        if let Some(text) = self
            .visible_redaction
            .get_mut()
            .expect("visible redaction state")
            .finish()
        {
            if self
                .observation
                .inner
                .writer
                .try_send(WriterCommand::Event {
                    run_id: self.run_id.clone(),
                    event: RunEvent::ClientVisibleContentDelta { text },
                })
                .is_err()
            {
                *self.gap.get_mut() = true;
            }
        }
        let mut pending_finish = self
            .pending_finish
            .get_mut()
            .expect("terminal state")
            .take();
        if !*self.terminal.get_mut() {
            pending_finish = Some(RunOutcome {
                delivery_completed_at: None,
                status: "interrupted".into(),
                terminal_reason: Some("observer_dropped".into()),
                generation_node_id: None,
                generation_root_id: None,
            });
        }
        let command = WriterCommand::Finalize {
            run_id: Some(self.run_id.clone()),
            rejection_id: None,
            trace: self.trace.take(),
            pending_finish,
            gap: *self.gap.get_mut(),
        };
        if let Some(permit) = self
            .finalization
            .get_mut()
            .expect("finalization permit")
            .take()
        {
            permit.send(command);
        } else if self.observation.inner.writer.try_send(command).is_err() {
            tracing::warn!(run_id=%self.run_id, "observation finalization unavailable");
        }
    }
}
fn project_bundle_status(events: &[ObservationEvent]) -> String {
    let mut runs: HashMap<&str, &str> = HashMap::new();
    let mut parents = std::collections::HashSet::new();
    let mut active = std::collections::HashSet::new();
    for event in events {
        let Some(run) = event.run_id.as_deref() else {
            continue;
        };
        match event.kind.as_str() {
            "run_admitted" => {
                runs.insert(run, "running");
                if let Some(parent) = event
                    .payload
                    .get("parent_run_id")
                    .and_then(serde_json::Value::as_str)
                {
                    parents.insert(parent);
                }
            }
            "client_tool_handoff" => {
                runs.insert(run, "waiting_client");
            }
            "run_finished" | "run_state_changed" | "process_restarted" => {
                let status = event
                    .payload
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("interrupted");
                runs.insert(run, status);
                if event.kind == "process_restarted" {
                    active.retain(|(owner, _, _)| *owner != run);
                }
            }
            "model_turn_started" | "target_attempt_started" | "platform_tool_started" => {
                let (kind, field) = match event.kind.as_str() {
                    "model_turn_started" => ("turn", "model_turn_id"),
                    "target_attempt_started" => ("attempt", "attempt_id"),
                    _ => ("tool", "tool_id"),
                };
                if let Some(id) = event.payload.get(field).and_then(serde_json::Value::as_str) {
                    active.insert((run, kind, id));
                }
            }
            "model_turn_finished" | "target_attempt_finished" | "platform_tool_finished" => {
                let (kind, field) = match event.kind.as_str() {
                    "model_turn_finished" => ("turn", "model_turn_id"),
                    "target_attempt_finished" => ("attempt", "attempt_id"),
                    _ => ("tool", "tool_id"),
                };
                if let Some(id) = event.payload.get(field).and_then(serde_json::Value::as_str) {
                    active.remove(&(run, kind, id));
                }
            }
            _ => {}
        }
    }
    if !active.is_empty() {
        return "running".into();
    }
    grouping::rollup_status(
        runs.into_iter()
            .map(|(run, status)| (status, !parents.contains(run))),
    )
    .into()
}

fn project_bundle_summary(
    detail: &InteractionDetail,
    events: &[ObservationEvent],
    through: i64,
    status: &str,
) -> serde_json::Value {
    let mut attempts = HashMap::<&str, ConfirmedUsage>::new();
    let mut visible_tail = String::new();
    let mut observation_gap = false;
    for event in events {
        match event.kind.as_str() {
            "target_attempt_started" => {
                if let Some(id) = event
                    .payload
                    .get("attempt_id")
                    .and_then(serde_json::Value::as_str)
                {
                    attempts.entry(id).or_default();
                }
            }
            "usage_confirmed" => {
                if let Some(id) = event
                    .payload
                    .get("attempt_id")
                    .and_then(serde_json::Value::as_str)
                {
                    match serde_json::from_value(
                        event.payload.get("usage").cloned().unwrap_or_default(),
                    ) {
                        Ok(usage) => {
                            attempts.insert(id, usage);
                        }
                        Err(_) => {
                            attempts.insert(id, ConfirmedUsage::default());
                            observation_gap = true;
                        }
                    }
                }
            }
            "client_visible_content_delta" => {
                if let Some(text) = event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                {
                    visible_tail.push_str(text);
                    if let Some((offset, _)) = visible_tail.char_indices().rev().nth(4095) {
                        visible_tail.drain(..offset);
                    }
                }
            }
            "observation_gap" => observation_gap = true,
            _ => {}
        }
    }
    let mut usage = ConfirmedUsage::default();
    if !attempts.is_empty() {
        usage = ConfirmedUsage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            reasoning_tokens: Some(0),
        };
        for attempt in attempts.values() {
            grouping::add_usage(&mut usage.input_tokens, attempt.input_tokens);
            grouping::add_usage(&mut usage.output_tokens, attempt.output_tokens);
            grouping::add_usage(&mut usage.cache_read_tokens, attempt.cache_read_tokens);
            grouping::add_usage(&mut usage.cache_write_tokens, attempt.cache_write_tokens);
            grouping::add_usage(&mut usage.reasoning_tokens, attempt.reasoning_tokens);
        }
    }
    let admission = events.iter().find(|event| event.kind == "run_admitted");
    let run_ids: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "run_admitted")
        .filter_map(|event| event.run_id.as_deref())
        .collect();
    serde_json::json!({
        "schema_version": 1, "interaction_id": detail.interaction.id,
        "root_id": detail.interaction.root_id,
        "parent_interaction_id": admission.and_then(|event| event.payload.get("parent_interaction_id")),
        "first_route_id": admission.and_then(|event| event.payload.get("route_id")),
        "first_model_display_name": admission.and_then(|event| event.payload.get("model_display_name")),
        "started_at": admission.map(|event| event.occurred_at),
        "last_active_at": events.last().map(|event| event.occurred_at),
        "through_event_sequence": through, "status": status, "usage": usage,
        "visible_tail": visible_tail, "observation_gap": observation_gap, "run_ids": run_ids,
        "events": events,
    })
}
fn collect_captured_artifacts(
    value: &serde_json::Value,
    references: &mut std::collections::BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(object) => {
            if object
                .get("media_externalized")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                if let Some(reference) = object
                    .get("artifact_reference")
                    .and_then(serde_json::Value::as_str)
                {
                    references.insert(reference.to_owned());
                }
            }
            for value in object.values() {
                collect_captured_artifacts(value, references);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_captured_artifacts(value, references);
            }
        }
        serde_json::Value::String(text) => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                if !value.is_string() {
                    collect_captured_artifacts(&value, references);
                }
            }
        }
        _ => {}
    }
}

async fn load_trace_values(
    snapshot: trace::TraceSnapshot,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut values = Vec::new();
    for segment in snapshot.segments {
        let mut bytes = tokio::fs::read(segment.path).await?;
        bytes.truncate(usize::try_from(segment.bytes).unwrap_or(usize::MAX));
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            values.push(serde_json::from_slice(line)?);
        }
    }
    Ok(values)
}
fn record_trace(trace: &TraceHandle, run: Option<&str>, rejection: Option<&str>, event: RunEvent) {
    record_trace_at(trace, run, rejection, event, 0)
}
pub(super) fn record_trace_at(
    trace: &TraceHandle,
    run: Option<&str>,
    rejection: Option<&str>,
    event: RunEvent,
    sequence: i64,
) {
    let (
        stage,
        direction,
        transport,
        protocol,
        message_type,
        status_code,
        url,
        headers,
        payload,
        model_turn_id,
        attempt_id,
    ) = match event {
        RunEvent::Checkpoint {
            stage,
            payload,
            model_turn_id,
            attempt_id,
        } => (
            Some(stage),
            None,
            None,
            None,
            None,
            None,
            None,
            serde_json::Value::Null,
            payload,
            model_turn_id,
            attempt_id,
        ),
        RunEvent::Wire {
            direction,
            transport,
            protocol,
            message_type,
            status_code,
            url,
            headers,
            payload,
            model_turn_id,
            attempt_id,
        } => (
            None,
            Some(direction),
            Some(transport),
            Some(protocol),
            Some(message_type),
            status_code,
            url,
            headers,
            payload,
            model_turn_id,
            attempt_id,
        ),
        _ => return,
    };
    let (payload_encoding, payload) = match payload {
        serde_json::Value::Object(mut object)
            if object.get("encoding").and_then(serde_json::Value::as_str) == Some("base64") =>
        {
            (
                "base64".to_owned(),
                object.remove("data").unwrap_or(serde_json::Value::Null),
            )
        }
        other => ("json".to_owned(), other),
    };
    let _ = trace.record(TraceRecord {
        schema_version: TRACE_SCHEMA_VERSION,
        sequence,
        recorded_at: chrono::Utc::now().timestamp_millis(),
        interaction_id: None,
        run_id: run.map(str::to_owned),
        rejection_id: rejection.map(str::to_owned),
        model_turn_id,
        attempt_id,
        layer: if direction.is_some() {
            "wire".into()
        } else {
            "canonical".into()
        },
        direction,
        stage,
        transport,
        protocol,
        message_type,
        representation: "json".into(),
        status: None,
        status_code,
        url,
        headers,
        payload_encoding,
        payload,
        error: None,
        redactions: Vec::new(),
    });
}

#[cfg(test)]
mod snapshot_tests {
    use serde_json::Value;

    use super::*;

    #[tokio::test]
    async fn committed_details_remain_readable_without_a_writer() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        crate::migrations::migrate_sqlite(&pool).await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_owned(),
            1,
            true,
        )
        .await;
        let observer = observation
            .observe_ingress(IngressStart {
                id: "read-only-ingress".into(),
                method: "POST".into(),
                path: "/responses".into(),
                protocol: "responses".into(),
            })
            .admit(RunStart {
                id: "read-only-run".into(),
                principal: "test".into(),
                api_key_id: None,
                api_key_name: None,
                generation_root_id: None,
                generation_parent_id: None,
                has_new_user: true,
                has_matching_pending_tool_result: false,
                ingress_received_at: 0,
                canonical_fingerprint: "read-only".into(),
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            });
        observer.record(RunEvent::ClientVisibleContentDelta {
            text: "saved answer".into(),
        });
        drop(observer);
        observation.shutdown().await;
        sqlx::query("PRAGMA query_only=ON").execute(&pool).await?;
        let id: String = sqlx::query_scalar(
            "SELECT interaction_id FROM inference_run_observations WHERE id='read-only-run'",
        )
        .fetch_one(&pool)
        .await?;
        let before = observation.inner.store.max_sequence().await?;
        let detail = observation
            .get_interaction(&id, ForestQuery::default())
            .await?
            .expect("committed interaction");
        assert_eq!(detail.interaction.visible_tail, "saved answer");
        let page = observation
            .get_interaction_events(
                &id,
                InteractionEventsQuery {
                    after_sequence: Some(0),
                    ..Default::default()
                },
            )
            .await?
            .expect("committed events");
        assert!(
            page.runs
                .iter()
                .flat_map(|run| &run.events)
                .any(|event| event.kind == "client_visible_content_delta"
                    && event.payload["text"] == "saved answer")
        );
        assert_eq!(observation.inner.store.max_sequence().await?, before);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn trace_only_capture_flushes_at_durable_cutoffs_without_replay_gaps()
    -> anyhow::Result<()> {
        use tokio_stream::StreamExt;
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool).await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_owned(),
            1,
            true,
        )
        .await;
        for enabled in [true, false] {
            observation.set_debug_enabled(enabled);
            let run_id = format!("trace-{enabled}");
            let observer = observation
                .observe_ingress(IngressStart {
                    id: format!("ingress-{enabled}"),
                    method: "POST".into(),
                    path: "/responses".into(),
                    protocol: "responses".into(),
                })
                .admit(RunStart {
                    id: run_id.clone(),
                    principal: "test".into(),
                    api_key_id: None,
                    api_key_name: None,
                    generation_root_id: None,
                    generation_parent_id: None,
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    ingress_received_at: 0,
                    canonical_fingerprint: run_id.clone(),
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                });
            observation.flush().await?;
            let admitted = observation.inner.store.max_sequence().await?;
            let checkpoint = |stage: &str| RunEvent::Checkpoint {
                stage: stage.into(),
                model_turn_id: None,
                attempt_id: None,
                payload: serde_json::json!({"stage":stage}),
            };
            observer.record(checkpoint("before-cutoff"));
            observation.flush().await?;
            let cutoff = observation.inner.store.max_sequence().await?;
            observer.record(checkpoint("after-cutoff"));
            let trace = observer.inner.trace.clone();
            if let Some(trace) = &trace {
                let old = load_trace_values(trace.snapshot(cutoff).await?).await?;
                assert_eq!(
                    old.iter()
                        .filter_map(|record| record["stage"].as_str())
                        .collect::<Vec<_>>(),
                    ["before-cutoff"]
                );
                assert!(
                    load_trace_values(trace.snapshot(admitted).await?)
                        .await?
                        .is_empty()
                );
            } else {
                assert!(!enabled);
                assert_eq!(cutoff, admitted);
            }
            drop(observer);
            observation.flush().await?;
            let terminal = observation.inner.store.max_sequence().await?;
            if let Some(trace) = &trace {
                let final_records = load_trace_values(trace.snapshot(terminal).await?).await?;
                assert_eq!(
                    final_records
                        .iter()
                        .filter_map(|record| record["stage"].as_str())
                        .collect::<Vec<_>>(),
                    ["before-cutoff", "after-cutoff"]
                );
                assert_eq!(trace.manifest().status, "complete");
            }
            let rows: Vec<(i64, String)> = sqlx::query_as(
                "SELECT sequence,kind FROM observation_events WHERE sequence>? ORDER BY sequence",
            )
            .bind(admitted)
            .fetch_all(&pool)
            .await?;
            assert!(
                !rows
                    .iter()
                    .any(|(_, kind)| matches!(kind.as_str(), "wire" | "checkpoint"))
            );
            assert_eq!(
                rows.iter()
                    .map(|(sequence, _)| *sequence)
                    .collect::<Vec<_>>(),
                ((admitted + 1)..=terminal).collect::<Vec<_>>()
            );
            let mut replay = observation.subscribe(admitted);
            for (sequence, kind) in rows {
                let update = tokio::time::timeout(std::time::Duration::from_secs(2), replay.next())
                    .await?
                    .expect("persisted replay event");
                let ObservationUpdate::Event(event) = update else {
                    panic!("trace traffic must not cause a replay reset")
                };
                assert_eq!((event.sequence, event.kind), (sequence, kind));
            }
        }
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn input_preview_is_nullable_and_owned_by_initial_run() -> anyhow::Result<()> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0034_interaction_observation.sql"
        ))
        .execute(&pool)
        .await?;
        let at = writer::now();
        sqlx::query("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,expires_at) VALUES ('historical','test','historical','initial','route','running',?,?,?)")
            .bind(at).bind(at).bind(at + 86_400_000).execute(&pool).await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0040_interaction_input_preview.sql"
        ))
        .execute(&pool)
        .await?;
        let preview: Option<String> = sqlx::query_scalar(
            "SELECT input_preview FROM interaction_observations WHERE id='historical'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(preview, None);
        sqlx::query("INSERT INTO inference_run_observations(id,interaction_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,expires_at) VALUES ('initial','historical','openai','route','running',0,?,?,?)")
            .bind(at).bind(at).bind(at + 86_400_000).execute(&pool).await?;
        let store = store::ObservationStore::Sqlite(pool.clone());
        assert!(
            store
                .persist_input_preview("historical", "child", "tool-secret", at, at + 86_400_000)
                .await?
                .is_none()
        );
        store
            .persist_input_preview("historical", "initial", "first input", at, at + 86_400_000)
            .await?;
        assert!(
            store
                .persist_input_preview("historical", "initial", "later round", at, at + 86_400_000)
                .await?
                .is_none()
        );
        assert!(
            store
                .persist_input_preview("historical", "child", "tool-secret", at, at + 86_400_000)
                .await?
                .is_none()
        );
        let preview: Option<String> = sqlx::query_scalar(
            "SELECT input_preview FROM interaction_observations WHERE id='historical'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(preview.as_deref(), Some("first input"));
        let payload: String = sqlx::query_scalar(
            "SELECT payload FROM observation_events WHERE kind='input_preview_recorded'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(!payload.contains("first input"));
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn discoveries_group_by_latest_discovery_and_survive_restart() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("observations.sqlite"))
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0034_interaction_observation.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0040_interaction_input_preview.sql"
        ))
        .execute(&pool)
        .await?;
        let now = chrono::Utc::now().timestamp_millis();
        for (id, status, last_active, gap) in [
            ("first", "failed", now + 100, false),
            ("second", "interrupted", now + 200, false),
            ("missing", "completed", now, true),
        ] {
            sqlx::query("INSERT INTO interaction_observations(id,principal,api_key_name,root_id,root_run_id,first_route_id,status,started_at,last_active_at,observation_gap,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)")
                .bind(id).bind("api-key:test").bind("测试 API Key").bind(id).bind(id)
                .bind("test-route").bind(status).bind(now - 100).bind(last_active)
                .bind(gap).bind(now + 86_400_000).execute(&pool).await?;
        }
        let discovery = |rules: &[&str], sources: &[&str]| {
            serde_json::json!({
                "kind": "credential_mappings_created",
                "discoveries": [{"rule_ids": rules, "source_types": sources}]
            })
            .to_string()
        };
        for (sequence, interaction, time, payload) in [
            (
                1,
                Some("first"),
                now - 30,
                discovery(&["rule-a"], &["user_message"]),
            ),
            (
                2,
                Some("second"),
                now - 20,
                discovery(&["rule-b"], &["tool_result"]),
            ),
            (
                3,
                Some("first"),
                now - 10,
                discovery(&["rule-a", "rule-c"], &["tool_result"]),
            ),
            (
                4,
                None,
                now,
                discovery(&["internal-rule"], &["system_history"]),
            ),
        ] {
            sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,kind,payload,expires_at) VALUES (?,?,?,'credential_mappings_created',?,?)")
                .bind(sequence).bind(time).bind(interaction).bind(payload)
                .bind(now + 86_400_000).execute(&pool).await?;
        }
        pool.close().await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = store::ObservationStore::Sqlite(pool.clone());
        let first = store
            .credential_discoveries(CredentialDiscoveryQuery {
                cursor: None,
                limit: Some(1),
            })
            .await?;
        assert!(first.observation_gap);
        assert_eq!(first.items.len(), 1);
        let item = &first.items[0];
        assert_eq!(item.interaction_id, "first");
        assert_eq!(item.discovered_at, now - 10);
        assert_eq!(item.new_credential_count, 2);
        assert_eq!(item.rule_ids, ["rule-a", "rule-c"]);
        assert_eq!(item.source_types, ["tool_result", "user_message"]);
        assert_eq!(item.status, "failed");
        assert!(!item.observation_gap);
        let second = store
            .credential_discoveries(CredentialDiscoveryQuery {
                cursor: first.next_cursor,
                limit: Some(1),
            })
            .await?;
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].interaction_id, "second");
        assert_eq!(second.items[0].status, "interrupted");
        assert!(second.next_cursor.is_none());
        assert!(
            store
                .credential_discoveries(CredentialDiscoveryQuery {
                    cursor: Some("invalid".into()),
                    limit: None,
                })
                .await
                .is_err()
        );
        sqlx::query("UPDATE observation_events SET expires_at=0")
            .execute(&pool)
            .await?;
        let empty = store
            .credential_discoveries(CredentialDiscoveryQuery::default())
            .await?;
        assert!(empty.items.is_empty());
        assert!(empty.observation_gap);
        sqlx::query("UPDATE interaction_observations SET expires_at=0")
            .execute(&pool)
            .await?;
        let expired = store
            .credential_discoveries(CredentialDiscoveryQuery::default())
            .await?;
        assert!(expired.items.is_empty());
        assert!(!expired.observation_gap);
        pool.close().await;
        assert!(
            store
                .credential_discoveries(CredentialDiscoveryQuery::default())
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_parent_grouping_uses_delivery_and_ingress_across_restart() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0034_interaction_observation.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0040_interaction_input_preview.sql"
        ))
        .execute(&pool)
        .await?;
        let mut observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            1,
            false,
        )
        .await;
        let delivered_at = writer::now() - 100_000;
        for restarted in [false, true] {
            for (case, delay, tools, parent, principal, new_user, merged, reason) in [
                (
                    "boundary",
                    2000,
                    false,
                    true,
                    "owner",
                    true,
                    true,
                    "rapid_exact_continuation",
                ),
                ("late", 2001, false, true, "owner", true, false, "new_user"),
                (
                    "tool",
                    86_400_000,
                    true,
                    true,
                    "owner",
                    true,
                    true,
                    "pending_tool_result",
                ),
                (
                    "human",
                    1,
                    false,
                    true,
                    "owner",
                    true,
                    true,
                    "rapid_exact_continuation",
                ),
                (
                    "unmatched",
                    1,
                    true,
                    false,
                    "owner",
                    true,
                    false,
                    "unmatched_parent",
                ),
                (
                    "other-principal",
                    1,
                    true,
                    true,
                    "other",
                    true,
                    false,
                    "unmatched_parent",
                ),
                ("before", -1, false, true, "owner", true, false, "new_user"),
                (
                    "ordinary",
                    86_400_000,
                    false,
                    true,
                    "owner",
                    false,
                    true,
                    "exact_continuation",
                ),
            ] {
                let id = format!("{restarted}-{case}");
                let make_run = |observation: &InteractionObservation,
                                id: String,
                                received_at,
                                parent,
                                principal: &str,
                                tools,
                                new_user| {
                    let mut ingress = observation.observe_ingress(IngressStart {
                        id: id.clone(),
                        method: "POST".into(),
                        path: "/responses".into(),
                        protocol: "responses".into(),
                    });
                    ingress.received_at = received_at;
                    ingress.admit(RunStart {
                        id: id.clone(),
                        principal: principal.into(),
                        api_key_id: None,
                        api_key_name: None,
                        generation_root_id: None,
                        generation_parent_id: parent,
                        has_new_user: new_user,
                        has_matching_pending_tool_result: tools,
                        ingress_received_at: 0,
                        canonical_fingerprint: id,
                        route_id: "route".into(),
                        model_display_name: None,
                        ingress_protocol: "responses".into(),
                    })
                };
                let parent_id = format!("parent-{id}");
                let node_id = format!("node-{id}");
                let first = make_run(
                    &observation,
                    parent_id.clone(),
                    delivered_at - 1,
                    None,
                    "owner",
                    false,
                    true,
                );
                first.finish(RunOutcome {
                    status: if tools { "waiting_client" } else { "completed" }.into(),
                    terminal_reason: None,
                    generation_node_id: Some(node_id.clone()),
                    generation_root_id: Some(node_id.clone()),
                    delivery_completed_at: Some(delivered_at),
                });
                observation.flush().await?;
                // Later observation work must not move the delivery timestamp.
                first.record(RunEvent::ObservationGap {
                    reason: "late observation".into(),
                });
                observation.flush().await?;
                if restarted {
                    drop(first);
                    observation.shutdown().await;
                    observation = InteractionObservation::new(
                        Some(pool.clone()),
                        None,
                        directory.path().to_path_buf(),
                        1,
                        false,
                    )
                    .await;
                }
                let child = make_run(
                    &observation,
                    id.clone(),
                    delivered_at + delay,
                    Some(if parent {
                        node_id
                    } else {
                        "different-node".into()
                    }),
                    principal,
                    tools,
                    new_user,
                );
                observation.flush().await?;
                let parent_row: (String, bool) = sqlx::query_as("SELECT interaction_id,user_interrupted FROM inference_run_observations WHERE id=?")
                    .bind(&parent_id).fetch_one(&pool).await?;
                let child_interaction: String = sqlx::query_scalar(
                    "SELECT interaction_id FROM inference_run_observations WHERE id=?",
                )
                .bind(&id)
                .fetch_one(&pool)
                .await?;
                assert_eq!(child_interaction == parent_row.0, merged, "{id}");
                assert!(
                    !parent_row.1,
                    "grouping must not interrupt its exact parent: {id}"
                );
                let status: String =
                    sqlx::query_scalar("SELECT status FROM interaction_observations WHERE id=?")
                        .bind(&child_interaction)
                        .fetch_one(&pool)
                        .await?;
                assert_eq!(status, "running");
                let recorded_reason: String = sqlx::query_scalar("SELECT json_extract(payload,'$.grouping_reason') FROM observation_events WHERE run_id=? AND kind='run_admitted'")
                    .bind(&id).fetch_one(&pool).await?;
                assert_eq!(recorded_reason, reason, "{id}");
                drop(child);
                observation.flush().await?;
            }
        }
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[test]
    fn undurable_gaps_follow_current_retention_and_clear_generations() {
        let day = 86_400_000;
        let mut gaps = UnpersistedGaps::default();
        gaps.record("completed", day);
        assert!(!gaps.visible(3 * day, 1));
        assert!(gaps.visible(3 * day, 7));
        assert!(!gaps.visible(3 * day, 0));

        let covered = gaps.runs.clone();
        // Even a loss in the same millisecond must survive the in-flight clear.
        gaps.record("completed", day);
        gaps.clear_removed(&covered, &["completed".into()]);
        assert!(gaps.visible(day, 1));
        let covered = gaps.runs.clone();
        gaps.clear_removed(&covered, &["completed".into()]);
        assert!(!gaps.visible(day, 1));

        let covered = gaps.runs.clone();
        gaps.record("new-admission", day);
        gaps.clear_removed(&covered, &[]);
        assert!(gaps.visible(day, 1));
    }

    #[tokio::test]
    async fn clearing_gaps_preserves_active_discoveries_and_unknown_admissions()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0034_interaction_observation.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0040_interaction_input_preview.sql"
        ))
        .execute(&pool)
        .await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            1,
            true,
        )
        .await;
        let make_run = |id: &str| {
            observation
                .observe_ingress(IngressStart {
                    id: format!("ingress-{id}"),
                    method: "POST".into(),
                    path: "/responses".into(),
                    protocol: "responses".into(),
                })
                .admit(RunStart {
                    id: id.into(),
                    principal: "api-key:test".into(),
                    api_key_id: None,
                    api_key_name: Some("test".into()),
                    generation_root_id: None,
                    generation_parent_id: None,
                    has_new_user: true,
                    has_matching_pending_tool_result: false,
                    ingress_received_at: 0,
                    canonical_fingerprint: id.into(),
                    route_id: "test-route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                })
        };
        let active = make_run("active");
        let completed = make_run("completed");
        active.record(RunEvent::CredentialMappingsCreated {
            discoveries: vec![CredentialDiscovery {
                rule_ids: vec!["rule".into()],
                source_types: vec!["user_message".into()],
            }],
        });
        completed.finish(RunOutcome {
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        observation.flush().await?;
        {
            let mut gaps = observation
                .inner
                .unpersisted_gaps
                .lock()
                .expect("observation gaps");
            gaps.record("completed", writer::now());
        }
        observation.clear_history().await?;
        let page = observation
            .credential_discoveries(CredentialDiscoveryQuery::default())
            .await?;
        assert!(!page.observation_gap);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].status, "running");
        assert_eq!(page.items[0].new_credential_count, 1);
        let active_interaction = page.items[0].interaction_id.clone();
        observation
            .inner
            .unpersisted_gaps
            .lock()
            .expect("observation gaps")
            .record("active", writer::now());
        observation.clear_history().await?;
        let page = observation
            .credential_discoveries(CredentialDiscoveryQuery::default())
            .await?;
        assert!(page.observation_gap);
        assert_eq!(page.items[0].interaction_id, active_interaction);
        assert_eq!(page.items[0].new_credential_count, 1);

        active.finish(RunOutcome {
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        observation.clear_history().await?;
        assert!(
            !observation
                .credential_discoveries(CredentialDiscoveryQuery::default())
                .await?
                .observation_gap
        );
        let mut clearing = Box::pin(observation.clear_history());
        // The single-threaded runtime cannot consume the writer barrier in this poll.
        assert!(futures::poll!(clearing.as_mut()).is_pending());
        observation
            .inner
            .unpersisted_gaps
            .lock()
            .expect("observation gaps")
            .record("lost-admission", writer::now());
        clearing.await?;
        assert!(
            observation
                .credential_discoveries(CredentialDiscoveryQuery::default())
                .await?
                .observation_gap
        );
        observation.clear_history().await?;
        assert!(
            observation
                .credential_discoveries(CredentialDiscoveryQuery::default())
                .await?
                .observation_gap
        );
        observation.set_retention_days(0).await?;
        assert!(
            !observation
                .credential_discoveries(CredentialDiscoveryQuery::default())
                .await?
                .observation_gap
        );
        drop(active);
        drop(completed);
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_tool_payloads_wait_for_protection_and_stay_out_of_visible_output()
    -> anyhow::Result<()> {
        use stravia_runtime_contract::protocol::ir::AiItem;
        let directory = tempfile::tempdir()?;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0034_interaction_observation.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/sqlite/0040_interaction_input_preview.sql"
        ))
        .execute(&pool)
        .await?;
        let observation = InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.path().to_path_buf(),
            1,
            true,
        )
        .await;
        let observer = observation
            .observe_ingress(IngressStart {
                id: "ingress".into(),
                method: "POST".into(),
                path: "/responses".into(),
                protocol: "responses".into(),
            })
            .admit(RunStart {
                id: "run".into(),
                principal: "api-key:test".into(),
                api_key_id: None,
                api_key_name: None,
                generation_root_id: None,
                generation_parent_id: None,
                has_new_user: false,
                has_matching_pending_tool_result: false,
                ingress_received_at: 0,
                canonical_fingerprint: "tool-payload-test".into(),
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            });
        assert!(!observer.debug_enabled());
        observer.capture_client_tool_results(&[
            AiItem::output_text("not a received tool result"),
            AiItem::function_call_output("plain", serde_json::json!("restored-tool-secret")),
            AiItem::function_call_output(
                "structured",
                serde_json::json!({
                    "api_key": "CLIENT_TOOL_SECRET", "result": "business result",
                }),
            ),
        ]);
        observation.flush().await?;
        let unpublished: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM observation_events WHERE kind='client_tool_result'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(unpublished, 0);
        observer.protect_secrets(["restored-tool-secret"]);
        // 工具续跑没有用户预览也必须发布，重复保护边界不得重复记录。
        observer.publish_input_preview();
        observer.publish_input_preview();
        observer.record(RunEvent::ClientToolHandoff {
            tool_id: "client".into(),
            name: "local_probe".into(),
            input: Some(Value::Null),
        });
        observer.record(RunEvent::PlatformToolStarted {
            model_turn_id: "turn".into(),
            tool_id: "platform".into(),
            name: "probe".into(),
            input: Some(serde_json::json!({"api_key": "PLATFORM_INPUT_SECRET", "query": "retain"})),
        });
        observer.record(RunEvent::PlatformToolFinished {
            model_turn_id: "turn".into(),
            tool_id: "platform".into(),
            status: "failed".into(),
            duration_ms: 1,
            content: Some(serde_json::json!({
                "error": "restored-tool-secret", "access_token": "PLATFORM_RESULT_SECRET",
            })),
        });
        observer.record(RunEvent::ModelThinkingDelta {
            model_turn_id: "turn".into(),
            attempt_id: "attempt".into(),
            text: "private reasoning".into(),
        });
        observer.record(RunEvent::ClientVisibleContentDelta {
            text: "public answer".into(),
        });
        drop(observer);
        observation.flush().await?;
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, payload FROM observation_events ORDER BY sequence")
                .fetch_all(&pool)
                .await?;
        let events: Vec<(String, Value)> = rows
            .into_iter()
            .map(|(kind, payload)| (kind, serde_json::from_str(&payload).expect("event payload")))
            .collect();
        let results: Vec<_> = events
            .iter()
            .filter(|event| event.0 == "client_tool_result")
            .map(|event| &event.1)
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["content"], "***");
        assert_eq!(
            results[1]["content"],
            serde_json::json!({"api_key":"***","result":"business result"})
        );
        let handoff = &events
            .iter()
            .find(|event| event.0 == "client_tool_handoff")
            .unwrap()
            .1;
        assert_eq!(handoff.get("input"), Some(&Value::Null));
        let started = &events
            .iter()
            .find(|event| event.0 == "platform_tool_started")
            .unwrap()
            .1;
        assert_eq!(
            started["input"],
            serde_json::json!({"api_key":"***","query":"retain"})
        );
        let finished = &events
            .iter()
            .find(|event| event.0 == "platform_tool_finished")
            .unwrap()
            .1;
        assert_eq!(finished["status"], "failed");
        assert_eq!(
            finished["content"],
            serde_json::json!({"error":"***","access_token":"***"})
        );
        let thinking: String = events
            .iter()
            .filter(|event| event.0 == "model_thinking_delta")
            .filter_map(|event| event.1["text"].as_str())
            .collect();
        assert_eq!(thinking, "private reasoning");
        assert!(
            events
                .iter()
                .any(|event| event.0 == "model_thinking_finished")
        );
        let visible: String =
            sqlx::query_scalar("SELECT visible_tail FROM interaction_observations")
                .fetch_one(&pool)
                .await?;
        assert_eq!(visible, "public answer");
        let stored = serde_json::to_string(&events)?;
        for excluded in [
            "restored-tool-secret",
            "CLIENT_TOOL_SECRET",
            "PLATFORM_INPUT_SECRET",
            "PLATFORM_RESULT_SECRET",
            "not a received tool result",
        ] {
            assert!(!stored.contains(excluded));
        }
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[test]
    fn snapshot_status_keeps_detached_work_active_and_consumes_parent_handoff() {
        let records = [
            ("parent", "run_admitted", serde_json::json!({})),
            (
                "parent",
                "platform_tool_started",
                serde_json::json!({"tool_id":"background"}),
            ),
            (
                "parent",
                "client_tool_handoff",
                serde_json::json!({"tool_id":"client"}),
            ),
            (
                "parent",
                "run_finished",
                serde_json::json!({"status":"waiting_client"}),
            ),
            (
                "child",
                "run_admitted",
                serde_json::json!({"parent_run_id":"parent"}),
            ),
            (
                "child",
                "run_finished",
                serde_json::json!({"status":"completed"}),
            ),
            (
                "parent",
                "platform_tool_finished",
                serde_json::json!({"tool_id":"background","status":"completed"}),
            ),
        ];
        let events: Vec<_> = records
            .into_iter()
            .enumerate()
            .map(|(index, (run, kind, payload))| ObservationEvent {
                sequence: index as i64 + 1,
                occurred_at: index as i64,
                interaction_id: Some("interaction".into()),
                run_id: Some(run.into()),
                rejection_id: None,
                kind: kind.into(),
                payload,
            })
            .collect();
        assert_eq!(project_bundle_status(&events[..6]), "running");
        assert_eq!(project_bundle_status(&events), "completed");
    }
}
