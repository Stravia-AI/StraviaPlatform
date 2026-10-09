mod attribution;
mod bundle;
mod codec;
mod grouping;
mod live;
mod manifest_index;
mod query;
mod redaction;
mod retention;
pub(crate) mod scope;
mod store;
mod tail;
mod trace;
mod trace_storage;
mod types;
pub(crate) mod upgrade;
mod writer;

pub(crate) use attribution::AdmissionFacts;
pub(crate) use query::failed::{FAILED_SELECT, REQUEST_SELECT};
pub(crate) use redaction::{ProtectedSecrets, redact_text, redact_url, redact_value};
pub(crate) use trace::optimize_trace_directory;
pub use types::*;

use parking_lot::Mutex;
use sqlx::{PgPool, SqlitePool};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
};
use stravia_protocol_codec::accumulator::CanonicalPartIndex;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use bundle::BundleExport;
use store::ObservationStore;
use trace::{TRACE_SCHEMA_VERSION, TraceHandle, TraceManager, TraceRecord};
use writer::WriterCommand;

#[derive(Clone)]
pub(crate) struct InteractionObservation {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub(crate) struct ClientConnectionObservation {
    observation: InteractionObservation,
    waiting: Arc<Mutex<Option<Vec<String>>>>,
}

impl ClientConnectionObservation {
    pub(crate) fn new(observation: InteractionObservation) -> Self {
        Self {
            observation,
            waiting: Arc::new(Mutex::new(Some(Vec::new()))),
        }
    }

    pub(crate) fn waiting(&self, observer: &RunObserver) {
        let mut waiting = self.waiting.lock();
        if let Some(runs) = waiting.as_mut() {
            runs.push(observer.inner.run_id.clone());
        } else {
            self.disconnect(vec![observer.inner.run_id.clone()]);
        }
    }

    pub(crate) fn close(&self) {
        if let Some(runs) = self.waiting.lock().take() {
            self.disconnect(runs);
        }
    }

    fn disconnect(&self, runs: Vec<String>) {
        if runs.is_empty() {
            return;
        }
        let writer = self.observation.inner.writer.clone();
        let command = WriterCommand::ClientDisconnected { runs };
        if let Err(error) = writer.try_send(command) {
            // 断线不阻塞传输清理；队列繁忙时仍按写者顺序提交状态更新。
            tokio::spawn(async move {
                if writer.send(error.into_inner()).await.is_err() {
                    tracing::warn!("client disconnect observation unavailable");
                }
            });
        }
    }
}

/// HTTP 客户端没有连接关闭信号；等待工具回传的叶 Run 闲置超过该窗口按
/// client_wait_expired 转 disconnected。窗口必须覆盖合法的长时客户端工具执行。
const WAITING_CLIENT_IDLE_MS: i64 = 24 * 60 * 60 * 1000;

struct Inner {
    store: ObservationStore,
    writer: mpsc::Sender<WriterCommand>,
    text_slots: Arc<tokio::sync::Semaphore>,
    writer_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    updates: broadcast::Sender<ObservationUpdate>,
    live_content: Arc<live::LiveState>,
    trace_sequence: Arc<AtomicI64>,
    debug: Arc<AtomicBool>,
    metrics_upkeep: Mutex<Option<tokio::task::JoinHandle<()>>>,
    retention_days: Arc<AtomicU32>,
    traces: TraceManager,
    bundles: BundleExport,
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
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.startup",
        skip_all
    )]
    pub(crate) async fn new(
        sqlite: Option<SqlitePool>,
        postgres: Option<PgPool>,
        data_dir: PathBuf,
        retention_days: u32,
        persistent: bool,
        generation_chains: crate::generation_chain::GenerationChain,
        sqlite_write_gate: Option<Arc<tokio::sync::Mutex<()>>>,
    ) -> Self {
        let ephemeral_root = (!persistent).then(|| {
            data_dir.join(format!(
                ".ephemeral-{}",
                stravia_runtime_contract::identifier::new_id()
            ))
        });
        let trace_data_dir = ephemeral_root.clone().unwrap_or(data_dir);
        let index_root = trace_data_dir.clone();
        let debug_trace_index = Arc::new(
            match tokio::task::spawn_blocking(move || {
                manifest_index::DebugTraceIndex::load(&index_root)
            })
            .await
            {
                Ok(Ok(index)) => index,
                _ => {
                    tracing::warn!("observation file manifests unavailable");
                    manifest_index::DebugTraceIndex::empty(&trace_data_dir)
                }
            },
        );
        let store = ObservationStore::new(
            sqlite,
            postgres,
            Arc::clone(&debug_trace_index),
            sqlite_write_gate,
        )
        .expect("Gateway provides exactly one observation SQL backend");
        if store.recover_after_restart().await.is_err() {
            tracing::warn!("observation restart recovery unavailable");
        }
        if let Err(error) = debug_trace_index
            .recover(chrono::Utc::now().timestamp_millis())
            .await
        {
            tracing::warn!(%error, "trace manifest restart recovery unavailable");
        }
        let traces = TraceManager::new(trace_data_dir).unwrap_or_else(|_| {
            tracing::warn!("observation trace storage unavailable");
            TraceManager::degraded()
        });
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
        let (writer, task) = writer::spawn(writer::WriterDeps {
            store: store.clone(),
            debug_trace_index: Arc::clone(&debug_trace_index),
            retention_days: Arc::clone(&retention_days),
            updates: updates.clone(),
            trace_sequence: Arc::clone(&trace_sequence),
            traces: traces.clone(),
            active_traces: Arc::clone(&active_traces),
            partial_trace_count: Arc::clone(&partial_trace_count),
            unpersisted_gaps: Arc::clone(&unpersisted_gaps),
            live: Arc::clone(&live_content),
            generation_chains,
        });
        let debug = Arc::new(AtomicBool::new(false));
        crate::performance::bind_debug(&debug);
        let bundles = BundleExport::new(
            store.clone(),
            writer.clone(),
            traces.clone(),
            Arc::clone(&active_traces),
        );
        Self {
            inner: Arc::new(Inner {
                store,
                writer,
                text_slots: Arc::new(tokio::sync::Semaphore::new(writer::QUEUE_CAPACITY / 2)),
                writer_task: Mutex::new(Some(task)),
                updates,
                live_content,
                trace_sequence,
                debug,
                metrics_upkeep: Mutex::new(crate::performance::spawn_upkeep()),
                retention_days,
                traces,
                bundles,
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
            metadata: RequestMetadata::default(),
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
    pub(crate) async fn query_root_changes(
        &self,
        query: RootChangesQuery,
    ) -> anyhow::Result<RootChangesPage> {
        self.inner.store.query_root_changes(query).await
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
    pub(crate) fn debug_enabled(&self) -> bool {
        self.inner.debug.load(Ordering::Acquire)
    }

    pub(crate) fn writer_queue_depth(&self) -> usize {
        writer::QUEUE_CAPACITY.saturating_sub(self.inner.writer.capacity())
    }

    pub(crate) fn debug_state(&self) -> DebugState {
        let active_partial = self
            .inner
            .active_traces
            .lock()
            .values()
            .filter(|trace| trace.manifest().status == "partial")
            .count() as u64;
        DebugState {
            enabled: self.inner.debug.load(Ordering::Acquire),
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
        crate::performance::set_debug(&self.inner.debug, enabled);
        self.debug_state()
    }
    /// 删除全部已落盘 Debug Trace 与 manifest，不动请求记录；活动 Trace 标记 partial 后停止。
    /// 在 observation writer 中串行执行，退役的 capture 不再发布 manifest；清除后新建 capture 不受影响。
    pub(crate) async fn clear_debug(&self) -> anyhow::Result<DebugState> {
        let (response, receive) = tokio::sync::oneshot::channel();
        self.inner
            .writer
            .send(WriterCommand::ClearDebug { response })
            .await
            .map_err(|_| anyhow::anyhow!("observation writer unavailable during debug clear"))?;
        receive
            .await
            .map_err(|_| anyhow::anyhow!("observation writer stopped during debug clear"))??;
        Ok(self.debug_state())
    }
    pub(crate) async fn set_retention_days(&self, days: u32) -> anyhow::Result<DebugState> {
        self.inner.store.update_retention(days).await?;
        self.inner.retention_days.store(days, Ordering::Release);
        if let Err(error) = self.inner.writer.send(WriterCommand::ClearTail).await {
            tracing::debug!(%error, "observation writer unavailable during retention update");
        }
        self.sweep().await?;
        Ok(self.debug_state())
    }
    pub(crate) async fn clear_history(&self) -> anyhow::Result<ClearHistoryResult> {
        let covered = self.inner.unpersisted_gaps.lock().runs.clone();
        if let Err(error) = self.inner.writer.send(WriterCommand::ClearTail).await {
            tracing::debug!(%error, "observation writer unavailable during history clear");
        }
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
    #[tracing::instrument(
        target = "stravia::perf",
        name = "observation.maintenance.sweep",
        skip_all
    )]
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
        for event in self
            .inner
            .store
            .expire_idle_waiting_client(now, WAITING_CLIENT_IDLE_MS)
            .await?
        {
            self.inner
                .trace_sequence
                .fetch_max(event.sequence, Ordering::AcqRel);
            let _ = self.inner.updates.send(ObservationUpdate::Event(event));
        }
        self.inner
            .unpersisted_gaps
            .lock()
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
        let mut live = self.inner.updates.subscribe();
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let max = match store.max_sequence().await {
                Ok(max) => max,
                Err(error) => {
                    tracing::warn!(cause=%writer::redacted_persist_cause(&error), "observation subscription sequence unavailable");
                    let _ = tx
                        .send(ObservationUpdate::ResetRequired {
                            snapshot_sequence: 0,
                        })
                        .await;
                    return;
                }
            };
            let min = match store.min_sequence().await {
                Ok(min) => min,
                Err(error) => {
                    tracing::warn!(cause=%writer::redacted_persist_cause(&error), "observation subscription cursor unavailable");
                    let _ = tx
                        .send(ObservationUpdate::ResetRequired {
                            snapshot_sequence: max,
                        })
                        .await;
                    return;
                }
            };
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
            let mut roots = HashMap::new();
            if let Err(error) = replay_to(&store, &tx, &mut last, max, &mut roots).await {
                if tx.is_closed() {
                    return;
                }
                tracing::warn!(cause=%writer::redacted_persist_cause(&error), "observation subscription replay unavailable");
                let _ = tx
                    .send(ObservationUpdate::ResetRequired {
                        snapshot_sequence: max,
                    })
                    .await;
                return;
            }
            loop {
                match live.recv().await {
                    Ok(ObservationUpdate::Event(event)) => {
                        if event.sequence <= last {
                            continue;
                        }
                        if event.sequence > last.saturating_add(1) {
                            let replay =
                                replay_to(&store, &tx, &mut last, event.sequence, &mut roots).await;
                            if let Err(error) = &replay {
                                if tx.is_closed() {
                                    return;
                                }
                                tracing::warn!(cause=%writer::redacted_persist_cause(error), "observation subscription recovery unavailable");
                            }
                            if replay.is_err() || last < event.sequence {
                                let _ = tx
                                    .send(ObservationUpdate::ResetRequired {
                                        snapshot_sequence: subscription_sequence(&store, last)
                                            .await,
                                    })
                                    .await;
                                return;
                            }
                            continue;
                        }
                        last = event.sequence;
                        if let Err(error) = send_change(&store, &tx, event, &mut roots).await {
                            if tx.is_closed() {
                                return;
                            }
                            tracing::warn!(cause=%writer::redacted_persist_cause(&error), "observation change lookup unavailable");
                            let _ = tx
                                .send(ObservationUpdate::ResetRequired {
                                    snapshot_sequence: subscription_sequence(&store, last).await,
                                })
                                .await;
                            return;
                        }
                    }
                    Ok(ObservationUpdate::ResetRequired { snapshot_sequence }) => {
                        if tx
                            .send(ObservationUpdate::ResetRequired { snapshot_sequence })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(
                        ObservationUpdate::Change(_)
                        | ObservationUpdate::LiveContent(_)
                        | ObservationUpdate::LiveSnapshot { .. }
                        | ObservationUpdate::LiveGap { .. }
                        | ObservationUpdate::LiveFinished { .. },
                    ) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = subscription_sequence(&store, max).await;
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
    pub(crate) fn subscribe_live(&self, interaction_id: String) -> ObservationStream {
        self.inner.live_content.subscribe(interaction_id)
    }
    pub(crate) async fn issue_bundle_ticket(
        &self,
        request: BundleRequest,
    ) -> anyhow::Result<DownloadTicket> {
        self.inner.bundles.issue(request).await
    }
    pub(crate) async fn consume_bundle_ticket(&self, ticket: &str) -> anyhow::Result<BundleStream> {
        self.inner
            .bundles
            .consume(ticket)
            .map_err(anyhow::Error::new)
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

    pub(crate) async fn flush(&self) -> anyhow::Result<()> {
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
        crate::performance::set_debug(&self.inner.debug, false);
        if let Some(task) = self.inner.metrics_upkeep.lock().take() {
            task.abort();
        }
        if !self.inner.stopped.swap(true, Ordering::AcqRel) {
            let (tx, _) = oneshot::channel();
            let _ = self.inner.writer.try_send(WriterCommand::Shutdown(tx));
        }
    }
    pub(crate) async fn shutdown(&self) {
        crate::performance::set_debug(&self.inner.debug, false);
        if let Some(task) = self.inner.metrics_upkeep.lock().take() {
            task.abort();
        }
        self.inner.stopped.store(true, Ordering::Release);
        let task = { self.inner.writer_task.lock().take() };
        if let Some(task) = task {
            let (tx, rx) = oneshot::channel();
            if self
                .inner
                .writer
                .send(WriterCommand::Shutdown(tx))
                .await
                .is_ok()
                && let Err(error) = rx.await
            {
                tracing::debug!(%error, "observation writer dropped shutdown acknowledgement");
            }
            if let Err(error) = task.await {
                tracing::debug!(%error, "observation writer task failed during shutdown");
            }
        }
        self.inner.traces.shutdown().await;
        if let Some(root) = &self.inner.ephemeral_root
            && let Err(error) = tokio::fs::remove_dir_all(root).await
        {
            tracing::debug!(%error, root = %root.display(), "failed to remove ephemeral observation root");
        }
    }
}

async fn subscription_sequence(store: &ObservationStore, fallback: i64) -> i64 {
    match store.max_sequence().await {
        Ok(sequence) => sequence,
        Err(error) => {
            tracing::warn!(cause=%writer::redacted_persist_cause(&error), "observation reset sequence unavailable");
            fallback
        }
    }
}

async fn replay_to(
    store: &ObservationStore,
    sender: &mpsc::Sender<ObservationUpdate>,
    last: &mut i64,
    through: i64,
    roots: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    while *last < through {
        let before = *last;
        for event in store.replay(before).await? {
            if event.sequence > through {
                break;
            }
            *last = event.sequence;
            send_change(store, sender, event, roots).await?;
        }
        if *last == before {
            break;
        }
    }
    Ok(())
}

async fn send_change(
    store: &ObservationStore,
    sender: &mpsc::Sender<ObservationUpdate>,
    event: ObservationEvent,
    roots: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let root_id = match event.interaction_id.as_deref() {
        Some(id) => {
            if roots.len() >= 2048 {
                roots.clear();
            }
            if let Some(root) = roots.get(id) {
                Some(root.clone())
            } else {
                let root = store.observation_root_id(id).await?;
                if let Some(root) = &root {
                    roots.insert(id.to_owned(), root.clone());
                }
                root
            }
        }
        None => None,
    };
    sender
        .send(ObservationUpdate::Change(ObservationChange::from_event(
            event, root_id,
        )))
        .await
        .map_err(|_| anyhow::anyhow!("observation subscriber closed"))
}

#[derive(Default)]
pub(super) struct RequestMetadata {
    model: Option<String>,
    api_key_id: Option<String>,
    api_key_name: Option<String>,
}

pub(crate) struct IngressObserver {
    received_at: i64,
    metadata: RequestMetadata,
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

pub(crate) fn wire_bytes_value(bytes: &[u8]) -> serde_json::Value {
    std::str::from_utf8(bytes)
        .map(|text| serde_json::Value::String(text.to_owned()))
        .unwrap_or_else(|_| {
            serde_json::json!({
                "encoding": "base64",
                "data": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    bytes,
                ),
            })
        })
}

pub(crate) fn wire_headers_value(headers: &axum::http::HeaderMap) -> serde_json::Value {
    let mut encoded = serde_json::Map::new();
    for (name, value) in headers {
        let value = value
            .to_str()
            .map(|value| serde_json::Value::String(value.to_owned()))
            .unwrap_or_else(|_| wire_bytes_value(value.as_bytes()));
        match encoded.entry(name.as_str().to_owned()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(value);
            }
            serde_json::map::Entry::Occupied(mut entry) => match entry.get_mut() {
                serde_json::Value::Array(values) => values.push(value),
                existing => {
                    let first = std::mem::replace(existing, serde_json::Value::Null);
                    *existing = serde_json::Value::Array(vec![first, value]);
                }
            },
        }
    }
    serde_json::Value::Object(encoded)
}

pub(crate) fn wire_headers_value_for_async(headers: &axum::http::HeaderMap) -> serde_json::Value {
    let mut encoded = wire_headers_value(headers);
    trace::redact_authorization_headers(&mut encoded);
    encoded
}

impl IngressCapture {
    // 单个请求体的 Wire 捕获总量上限；不是 Trace 落盘容量配额。
    pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

    pub(crate) fn record(&self, event: RunEvent) {
        record_trace(&self.trace, None, None, event);
    }

    pub(crate) fn mark_partial(&self, reason: &'static str) {
        self.trace.mark_partial(reason, false);
    }
}

impl IngressObserver {
    pub(crate) fn set_model(&mut self, model: &str) {
        self.metadata.model = Some(redaction::redact_text(model));
    }

    pub(crate) fn set_authenticated_source(&mut self, key_id: &str, key_name: &str) {
        self.metadata.api_key_id = Some(key_id.to_owned());
        self.metadata.api_key_name = Some(redaction::redact_text(key_name));
    }

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
    pub(crate) fn record_debug_at(&self, recorded_at: i64, event: impl FnOnce() -> RunEvent) {
        if self.debug_enabled
            && let Some(trace) = &self.trace
        {
            record_trace_observed_at(trace, None, None, event(), 0, recorded_at);
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
    /// One admission carries everything Run Attribution needs; there is no
    /// separate input-registration call to get out of order.
    pub(crate) fn admit(mut self, start: RunStart, facts: AdmissionFacts) -> RunObserver {
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
            event_boundary: Mutex::new(()),
            gap: AtomicBool::new(false),
            gap_reported: AtomicBool::new(false),
            finalization: Mutex::new(self.finalization.take()),
            completion: Mutex::new(
                self.observation
                    .inner
                    .writer
                    .clone()
                    .try_reserve_owned()
                    .ok(),
            ),
            pending_finish: Mutex::new(None),
            queued_text: Mutex::new(None),
            failure: Mutex::new(None),
            generation_commit_fences: Mutex::new(Vec::new()),
            pending_input: Mutex::new(None),
            pending_tool_results: Mutex::new(Vec::new()),
            thinking_redaction: Mutex::new(std::collections::BTreeMap::new()),
            visible_redaction: Mutex::new(std::collections::BTreeMap::new()),
            protected,
        });
        // 没有成功收据与最终清理的保留槽，就不创建可被后续 User 误判的运行投影。
        let can_admit = inner.completion.lock().is_some() && inner.finalization.lock().is_some();
        let admitted = if can_admit {
            let facts = facts.prepare(&start.id);
            self.observation
                .inner
                .writer
                .try_send(WriterCommand::Admit(Box::new(writer::AdmitPayload {
                    start,
                    facts,
                    received_at: self.received_at,
                    metadata: std::mem::take(&mut self.metadata),
                    debug_enabled,
                    trace: inner.trace.clone(),
                    discarded_trace,
                })))
                .is_ok()
        } else {
            false
        };
        if !admitted {
            inner.gap.store(true, Ordering::Release);
            self.observation
                .inner
                .unpersisted_gaps
                .lock()
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
                    metadata: std::mem::take(&mut self.metadata),
                    debug_enabled: self.debug_enabled,
                    started_at: self.received_at,
                    duration_ms: writer::now().saturating_sub(self.received_at),
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
                failure: None,
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

/// A committed asynchronous publication may outlive the request future that
/// started it. Holding the shared observer keeps finalization behind that publication.
pub(crate) struct RunPublicationGuard {
    inner: Arc<RunObserverInner>,
}

struct RunObserverInner {
    observation: InteractionObservation,
    run_id: String,
    principal: String,
    debug_enabled: bool,
    trace: Option<TraceHandle>,
    terminal: AtomicBool,
    event_boundary: Mutex<()>,
    gap: AtomicBool,
    gap_reported: AtomicBool,
    finalization: Mutex<Option<mpsc::OwnedPermit<WriterCommand>>>,
    completion: Mutex<Option<mpsc::OwnedPermit<WriterCommand>>>,
    pending_finish: Mutex<Option<(RunOutcome, i64)>>,
    queued_text: Mutex<Option<Arc<Mutex<Option<RunEvent>>>>>,
    failure: Mutex<Option<FailureDiagnostic>>,
    generation_commit_fences: Mutex<Vec<crate::generation_chain::GenerationCommitFence>>,
    // Canonical user text remains memory-only until Model Turn protection succeeds.
    pending_input: Mutex<Option<Arc<stravia_runtime_contract::protocol::ir::AiRequest>>>,
    pending_tool_results: Mutex<Vec<RunEvent>>,
    thinking_redaction: Mutex<
        std::collections::BTreeMap<
            (String, String, usize, CanonicalPartIndex),
            redaction::VisibleTextRedactor,
        >,
    >,
    visible_redaction: Mutex<
        std::collections::BTreeMap<(usize, CanonicalPartIndex), redaction::VisibleTextRedactor>,
    >,
    protected: redaction::ProtectedSecrets,
}
impl RunObserver {
    pub(crate) fn hold_generation_commit_fence(
        &self,
        fence: crate::generation_chain::GenerationCommitFence,
    ) {
        let mut fences = self.inner.generation_commit_fences.lock();
        if self.inner.terminal.load(Ordering::Acquire) {
            fence.resolve();
            return;
        }
        fences.push(fence);
    }

    /// Admission only: use the received canonical window, never effective model history.
    pub(crate) fn capture_input_preview(
        &self,
        input: Arc<stravia_runtime_contract::protocol::ir::AiRequest>,
    ) {
        *self.inner.pending_input.lock() = Some(input);
    }

    /// 客户端返回先留在内存，和输入预览共用凭据映射完成后的发布边界。
    pub(crate) fn capture_client_tool_results(
        &self,
        input: &[stravia_runtime_contract::protocol::ir::AiItem],
    ) {
        use stravia_runtime_contract::protocol::ir::{ContentBlock, MessageContent, Role};
        let mut pending = self.inner.pending_tool_results.lock();
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
                            tool_id: tool_use_id.to_string(),
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
                    tool_id: tool_id.to_string(),
                    content: serde_json::to_value(&item.content).expect("canonical tool content"),
                    is_error: false,
                });
            }
        }
    }

    /// Publish once, only after all active and newly discovered mappings are registered.
    pub(crate) fn publish_input_preview(&self) {
        let tool_results = std::mem::take(&mut *self.inner.pending_tool_results.lock());
        self.send_tool_results(tool_results);
        let Some(input) = self.inner.pending_input.lock().take() else {
            return;
        };
        let Some(text) = redaction::user_input_text(&input.items) else {
            return;
        };
        let preview = redaction::input_preview(text, &self.inner.protected);
        self.inner.queued_text.lock().take();
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
                .record(&self.inner.run_id, writer::now());
        }
    }

    /// 仅在完整客户端交付成功后调用；归属证据不等待 Generation 提交。
    pub(crate) fn observe_client_completion(
        &self,
        input: &[stravia_runtime_contract::protocol::ir::AiItem],
        output: &[stravia_runtime_contract::protocol::ir::AiItem],
        delivered_at: i64,
        waiting_client: bool,
    ) {
        let _boundary = self.inner.event_boundary.lock();
        if self.inner.terminal.load(Ordering::Acquire) || self.inner.failure.lock().is_some() {
            return;
        }
        // 收据使用准入时的保留槽；不能让队列压力改变 Sent/Admit 的顺序。
        let Some(permit) = self.inner.completion.lock().take() else {
            return;
        };
        self.inner.queued_text.lock().take();
        let window = tail::Window::capture(input).and_then(|mut window| {
            window
                .append(tail::Window::capture(output)?)
                .then_some(window)
        });
        let writer = permit.send(WriterCommand::ClientCompletion {
            run_id: self.inner.run_id.clone(),
            principal: self.inner.principal.clone(),
            window,
            delivered_at,
            waiting_client,
        });
        if writer.is_closed() {
            self.inner.gap.store(true, Ordering::Release);
            self.inner
                .observation
                .inner
                .unpersisted_gaps
                .lock()
                .record(&self.inner.run_id, writer::now());
        }
    }
    pub(crate) fn protect_secrets<'a>(&self, secrets: impl IntoIterator<Item = &'a str>) {
        self.inner.protected.register(secrets);
    }
    pub(crate) fn protected_secrets(&self) -> ProtectedSecrets {
        self.inner.protected.clone()
    }
    pub(crate) fn publication_guard(&self) -> Option<RunPublicationGuard> {
        let _boundary = self.inner.event_boundary.lock();
        (!self.inner.terminal.load(Ordering::Acquire)).then(|| RunPublicationGuard {
            inner: self.inner.clone(),
        })
    }
    pub(crate) fn record_failure(&self, error: FailureDiagnostic) {
        self.record(RunEvent::RequestFailed { error });
    }

    pub(crate) fn record_response_failure(&self, error: FailureDiagnostic) {
        // 最终 Target 或流处理边界比 HTTP 错误转换保留更准确的上游事实。
        if self.inner.failure.lock().is_none() {
            self.record_failure(error);
        }
    }
    pub(crate) fn record_debug(&self, event: impl FnOnce() -> RunEvent) {
        if self.inner.debug_enabled {
            self.record(event());
        }
    }
    pub(crate) fn run_id(&self) -> &str {
        &self.inner.run_id
    }
    pub(crate) fn record(&self, event: RunEvent) {
        if !self.inner.debug_enabled && matches!(event, RunEvent::Wire { .. }) {
            return;
        }
        if let RunEvent::ModelThinkingDelta {
            model_turn_id,
            attempt_id,
            item_ordinal,
            part_index,
            text,
        } = event
        {
            let ready = self
                .inner
                .thinking_redaction
                .lock()
                .entry((
                    model_turn_id.clone(),
                    attempt_id.clone(),
                    item_ordinal,
                    part_index,
                ))
                .or_insert_with(|| {
                    redaction::VisibleTextRedactor::with_protected(self.inner.protected.clone())
                })
                .push(text);
            if let Some(text) = ready {
                self.send_event(RunEvent::ModelThinkingDelta {
                    model_turn_id,
                    attempt_id,
                    item_ordinal,
                    part_index,
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
            && self.flush_thinking(model_turn_id, attempt_id)
        {
            self.send_event(RunEvent::ModelThinkingFinished {
                model_turn_id: model_turn_id.clone(),
                attempt_id: attempt_id.clone(),
            });
        }
        if let RunEvent::ClientVisibleContentDelta {
            text,
            item_ordinal,
            part_index,
        } = event
        {
            let ready = self
                .inner
                .visible_redaction
                .lock()
                .entry((item_ordinal, part_index))
                .or_insert_with(|| {
                    redaction::VisibleTextRedactor::with_protected(self.inner.protected.clone())
                })
                .push(text);
            if let Some(text) = ready {
                self.send_event(RunEvent::ClientVisibleContentDelta {
                    text,
                    item_ordinal,
                    part_index,
                });
            }
            return;
        }
        if let RunEvent::ModelThinking {
            model_turn_id,
            attempt_id,
            ..
        } = &event
        {
            self.flush_thinking(model_turn_id, attempt_id);
        }
        if matches!(event, RunEvent::ClientVisibleContent { .. }) {
            self.flush_visible();
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
        let mut states = self.inner.thinking_redaction.lock();
        let keys: Vec<_> = states
            .keys()
            .filter(|(turn, attempt, _, _)| turn == model_turn_id && attempt == attempt_id)
            .cloned()
            .collect();
        let had_pending = !keys.is_empty();
        let pending: Vec<_> = keys
            .into_iter()
            .map(|key| {
                let state = states.remove(&key).unwrap();
                (key, state)
            })
            .collect();
        drop(states);
        for ((_, _, item_ordinal, part_index), mut state) in pending {
            if let Some(text) = state.finish() {
                self.send_event(RunEvent::ModelThinkingDelta {
                    model_turn_id: model_turn_id.to_owned(),
                    attempt_id: attempt_id.to_owned(),
                    item_ordinal,
                    part_index,
                    text,
                });
            }
        }
        had_pending
    }
    fn finish_thinking(&self) {
        let pending = std::mem::take(&mut *self.inner.thinking_redaction.lock());
        for ((model_turn_id, attempt_id, item_ordinal, part_index), mut state) in pending {
            if let Some(text) = state.finish() {
                self.send_event(RunEvent::ModelThinkingDelta {
                    model_turn_id: model_turn_id.clone(),
                    attempt_id: attempt_id.clone(),
                    item_ordinal,
                    part_index,
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
        let pending = std::mem::take(&mut *self.inner.visible_redaction.lock());
        for ((item_ordinal, part_index), mut state) in pending {
            if let Some(text) = state.finish() {
                self.send_event(RunEvent::ClientVisibleContentDelta {
                    text,
                    item_ordinal,
                    part_index,
                });
            }
        }
    }
    fn send_tool_results(&self, mut events: Vec<RunEvent>) {
        if events.is_empty() {
            return;
        }
        self.inner.queued_text.lock().take();
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
                .record(&self.inner.run_id, writer::now());
        }
    }
    fn send_event(&self, mut event: RunEvent) {
        // Keep concurrently produced activity ordered with Finish without rejecting
        // meaningful background events that arrive after client delivery.
        let _boundary = self.inner.event_boundary.lock();
        if matches!(event, RunEvent::ClientToolResult { .. }) {
            self.send_tool_results(vec![event]);
            return;
        }
        if matches!(event, RunEvent::Wire { .. }) {
            if let Some(trace) = &self.inner.trace {
                // Wire Debug applies its Authorization-only policy inside the Trace queue.
                // Ordinary Observation redaction must not rewrite raw wire content first.
                // Wire records carry the durable event frontier observed at capture
                // time, not a synthetic next sequence: a ticket whose
                // through-sequence equals that frontier includes the bytes, while
                // later captures stay excluded by the fixed snapshot watermark.
                let sequence = self
                    .inner
                    .observation
                    .inner
                    .trace_sequence
                    .load(Ordering::Acquire);
                record_trace_at(trace, Some(&self.inner.run_id), None, event, sequence);
            }
            return;
        }
        self.inner.protected.event(&mut event);
        redaction::redact_run_event(&mut event);
        if let RunEvent::RequestFailed { error } = &event {
            *self.inner.failure.lock() = Some(error.clone());
        }
        if matches!(event, RunEvent::ObservationGap { .. })
            && let Some(trace) = &self.inner.trace
        {
            trace.mark_partial("observation_gap", false);
        }
        if self.inner.gap.load(Ordering::Acquire)
            && !self.inner.gap_reported.swap(true, Ordering::AcqRel)
        {
            self.inner.queued_text.lock().take();
            if self
                .inner
                .observation
                .inner
                .writer
                .try_send(WriterCommand::Event {
                    run_id: self.inner.run_id.clone(),
                    event: RunEvent::ObservationGap {
                        reason: "writer_overflow".into(),
                    },
                })
                .is_err()
            {
                self.inner.gap_reported.store(false, Ordering::Release);
            }
        }
        // 在入队前合并同一 Run 的相邻文本；入队后才合并会让细粒度 delta
        // 先占满命令队列。写者取走后不可再追加，非文本事件保留顺序边界。
        let mut queued = self.inner.queued_text.lock();
        let text_len = writer::text_mut(&mut event).map(|text| text.len());
        if let (Some(len), Some(pending)) = (text_len, queued.as_ref()) {
            let mut pending = pending.lock();
            if let Some(previous) = pending.as_mut()
                && writer::same_scope(previous, &event)
                && writer::text_mut(previous).is_some_and(|text| {
                    text.len().saturating_add(len) <= writer::LIVE_COALESCE_BYTES
                })
            {
                let text = writer::text_mut(&mut event).expect("text delta");
                writer::text_mut(previous)
                    .expect("same text scope")
                    .push_str(text);
                return;
            }
        }
        let command = if text_len.is_some() {
            // 文本最多占队列一半；生命周期事件仍共用 FIFO，但不会被正文洪峰挤满。
            let Ok(slot) = self
                .inner
                .observation
                .inner
                .text_slots
                .clone()
                .try_acquire_owned()
            else {
                *queued = None;
                self.inner.gap.store(true, Ordering::Release);
                self.inner
                    .observation
                    .inner
                    .unpersisted_gaps
                    .lock()
                    .record(&self.inner.run_id, writer::now());
                return;
            };
            let pending = Arc::new(Mutex::new(Some(event)));
            *queued = Some(Arc::clone(&pending));
            WriterCommand::Text {
                run_id: self.inner.run_id.clone(),
                event: pending,
                _slot: slot,
            }
        } else {
            *queued = None;
            WriterCommand::Event {
                run_id: self.inner.run_id.clone(),
                event,
            }
        };
        if self
            .inner
            .observation
            .inner
            .writer
            .try_send(command)
            .is_err()
        {
            *queued = None;
            self.inner.gap.store(true, Ordering::Release);
            let observation = &self.inner.observation.inner;
            observation
                .unpersisted_gaps
                .lock()
                .record(&self.inner.run_id, writer::now());
        }
    }
    pub(crate) fn finish(&self, mut outcome: RunOutcome) {
        let finished_at = writer::now();
        if let Some(error) = self.inner.failure.lock().as_ref() {
            outcome.status = "failed".into();
            outcome.terminal_reason.clone_from(&error.code);
        }
        self.flush_visible();
        self.finish_thinking();
        self.inner.queued_text.lock().take();
        self.inner.protected.text(&mut outcome.status);
        if let Some(delivery) = &mut outcome.delivery {
            self.inner.protected.text(&mut delivery.status);
            if let Some(reason) = &mut delivery.reason {
                self.inner.protected.text(reason);
            }
        }
        if let Some(reason) = &mut outcome.terminal_reason {
            self.inner.protected.text(reason);
        }
        redaction::redact_run_outcome(&mut outcome);
        let _boundary = self.inner.event_boundary.lock();
        let mut generation_commit_fences = self.inner.generation_commit_fences.lock();
        if !self.inner.terminal.swap(true, Ordering::AcqRel) {
            let command = WriterCommand::Finish {
                run_id: self.inner.run_id.clone(),
                outcome,
                finished_at,
            };
            let result = if let Some(permit) = self.inner.completion.lock().take() {
                permit.send(command);
                Ok(())
            } else {
                self.inner.observation.inner.writer.try_send(command)
            };
            if let Err(error) = result {
                if let WriterCommand::Finish {
                    outcome,
                    finished_at,
                    ..
                } = error.into_inner()
                {
                    *self.inner.pending_finish.lock() = Some((outcome, finished_at));
                }
                self.inner.gap.store(true, Ordering::Release);
                self.inner
                    .observation
                    .inner
                    .unpersisted_gaps
                    .lock()
                    .record(&self.inner.run_id, writer::now());
            }
            // Preserve Finish/Admit FIFO ordering without waiting for observation
            // persistence. A full or closed writer only records a diagnostic gap.
            for fence in generation_commit_fences.drain(..) {
                fence.resolve();
            }
        }
    }
}

impl RunPublicationGuard {
    pub(crate) fn protect_secrets<'a>(&self, secrets: impl IntoIterator<Item = &'a str>) {
        self.inner.protected.register(secrets);
    }

    pub(crate) fn credential_mappings_created(&self, discoveries: Vec<CredentialDiscovery>) {
        let _boundary = self.inner.event_boundary.lock();
        let mut event = RunEvent::CredentialMappingsCreated { discoveries };
        self.inner.protected.event(&mut event);
        redaction::redact_run_event(&mut event);
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
            self.inner
                .observation
                .inner
                .unpersisted_gaps
                .lock()
                .record(&self.inner.run_id, writer::now());
        }
    }
}

impl Drop for RunObserverInner {
    fn drop(&mut self) {
        for ((model_turn_id, attempt_id, item_ordinal, part_index), mut state) in
            std::mem::take(self.thinking_redaction.get_mut())
        {
            let delta = state.finish().map(|text| RunEvent::ModelThinkingDelta {
                model_turn_id: model_turn_id.clone(),
                attempt_id: attempt_id.clone(),
                item_ordinal,
                part_index,
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
        for ((item_ordinal, part_index), mut state) in
            std::mem::take(self.visible_redaction.get_mut())
        {
            if let Some(text) = state.finish()
                && self
                    .observation
                    .inner
                    .writer
                    .try_send(WriterCommand::Event {
                        run_id: self.run_id.clone(),
                        event: RunEvent::ClientVisibleContentDelta {
                            text,
                            item_ordinal,
                            part_index,
                        },
                    })
                    .is_err()
            {
                *self.gap.get_mut() = true;
            }
        }
        let mut pending_finish = self.pending_finish.get_mut().take();
        if !*self.terminal.get_mut() {
            pending_finish = Some((
                RunOutcome {
                    client_output_committed: false,
                    delivery: None,
                    delivery_completed_at: None,
                    status: "interrupted".into(),
                    terminal_reason: Some("observer_dropped".into()),
                    generation_node_id: None,
                    generation_root_id: None,
                },
                writer::now(),
            ));
        }
        let command = WriterCommand::Finalize {
            run_id: Some(self.run_id.clone()),
            rejection_id: None,
            trace: self.trace.take(),
            pending_finish,
            gap: *self.gap.get_mut(),
        };
        if let Some(permit) = self.finalization.get_mut().take() {
            permit.send(command);
        } else if self.observation.inner.writer.try_send(command).is_err() {
            tracing::warn!(run_id=%self.run_id, "observation finalization unavailable");
        }
    }
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
    record_trace_observed_at(
        trace,
        run,
        rejection,
        event,
        sequence,
        chrono::Utc::now().timestamp_millis(),
    );
}

fn record_trace_observed_at(
    trace: &TraceHandle,
    run: Option<&str>,
    rejection: Option<&str>,
    event: RunEvent,
    sequence: i64,
    recorded_at: i64,
) {
    let RunEvent::Wire {
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
    } = event
    else {
        return;
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
        recorded_at,
        interaction_id: None,
        run_id: run.map(str::to_owned),
        rejection_id: rejection.map(str::to_owned),
        model_turn_id,
        attempt_id,
        layer: "wire".into(),
        direction: Some(direction),
        stage: None,
        transport: Some(transport),
        protocol: Some(protocol),
        message_type: Some(message_type),
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
    use std::time::Duration;

    use serde_json::Value;

    fn visible_item(id: &str, text: &str) -> RunEvent {
        RunEvent::ClientVisibleContent {
            text: text.into(),
            parts: vec![serde_json::json!({"type":"text","text":text})],
            block_id: id.into(),
            item: serde_json::json!({"id":id,"role":"assistant","content":text}),
            complete: true,
        }
    }

    fn stored_payload(bytes: &[u8]) -> anyhow::Result<Value> {
        Ok(serde_json::from_slice(&crate::storage_codec::decode(
            bytes,
        )?)?)
    }

    async fn load_trace_values(snapshot: trace::TraceSnapshot) -> anyhow::Result<Vec<Value>> {
        tokio::task::spawn_blocking(move || {
            let mut values = Vec::new();
            for segment in snapshot.segments {
                trace_storage::visit(&segment.path, segment.bytes, |record| {
                    values.push(record);
                    Ok(())
                })?;
            }
            Ok::<_, std::io::Error>(values)
        })
        .await?
        .map_err(Into::into)
    }

    use super::*;

    #[test]
    fn async_wire_headers_mask_only_authorization() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer boundary-secret"),
        );
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("session=boundary-secret"),
        );

        let encoded = wire_headers_value_for_async(&headers);
        assert_eq!(encoded["authorization"], "***");
        assert_eq!(encoded["cookie"], "session=boundary-secret");
    }

    #[test]
    fn wire_headers_preserve_duplicate_and_non_utf8_values() {
        assert_eq!(
            wire_bytes_value(b"\xff\0"),
            serde_json::json!({"encoding": "base64", "data": "/wA="}),
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.append(
            "x-opaque",
            axum::http::HeaderValue::from_bytes(b"\xff").expect("opaque header"),
        );
        headers.append("x-opaque", axum::http::HeaderValue::from_static("plain"));

        assert_eq!(
            wire_headers_value(&headers)["x-opaque"],
            serde_json::json!([
                {"encoding": "base64", "data": "/w=="},
                "plain",
            ])
        );
    }

    /// Every test admission crosses the same one-call boundary as production.
    fn facts(items: Vec<stravia_runtime_contract::protocol::ir::AiItem>) -> AdmissionFacts {
        AdmissionFacts {
            client_request: Arc::new(stravia_runtime_contract::protocol::ir::AiRequest::new(
                "model", items,
            )),
            has_new_user: true,
            has_matching_pending_tool_result: false,
            generation_root_id: None,
            generation_parent_id: None,
        }
    }

    type RunRow = (String, String, Option<String>, String, Option<String>);

    async fn test_observation(
        pool: &sqlx::SqlitePool,
        directory: &std::path::Path,
        persistent: bool,
    ) -> InteractionObservation {
        InteractionObservation::new(
            Some(pool.clone()),
            None,
            directory.to_path_buf(),
            1,
            persistent,
            crate::generation_chain::test_chain().await,
            Some(Arc::new(tokio::sync::Mutex::new(()))),
        )
        .await
    }

    fn test_run(
        observation: &InteractionObservation,
        id: &str,
        facts: AdmissionFacts,
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
                    principal: "owner".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                facts,
            )
    }

    #[tokio::test]
    async fn delivered_final_receipt_settles_before_finish_without_tail_window()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let run = test_run(&observation, "receipt-no-window", facts(Vec::new()));
        let delivered_at = writer::now();
        run.observe_client_completion(&[], &[], delivered_at, false);
        run.observe_client_completion(&[], &[], delivered_at + 1, false);
        observation.flush().await?;
        let row: (String, Option<i64>, Option<i64>, bool) = sqlx::query_as(
            "SELECT status,delivery_completed_at,finished_at,user_interrupted FROM inference_run_observations WHERE id='receipt-no-window'",
        ).fetch_one(&pool).await?;
        assert_eq!(
            row,
            (
                "completed".into(),
                Some(delivered_at),
                Some(delivered_at),
                false
            )
        );
        run.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: Some(delivered_at),
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        observation.flush().await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE run_id='receipt-no-window' AND kind='run_state_changed'",
        ).fetch_one(&pool).await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn final_failure_cannot_be_successfulized_by_receipt_or_finish() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let run = test_run(&observation, "failed-receipt", facts(Vec::new()));
        run.record_failure(FailureDiagnostic::platform(
            "terminal_fault",
            "The stream failed.",
            502,
        ));
        run.observe_client_completion(&[], &[], writer::now(), false);
        run.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        observation.flush().await?;
        let row: (String, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT status,delivery_completed_at,terminal_reason FROM inference_run_observations WHERE id='failed-receipt'",
        ).fetch_one(&pool).await?;
        assert_eq!(row, ("failed".into(), None, Some("terminal_fault".into())));
        Ok(())
    }

    #[tokio::test]
    async fn reserved_receipt_survives_full_queue_and_finish_uses_finalization()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let run = test_run(&observation, "full-queue-receipt", facts(Vec::new()));
        // 当前线程在下一次 await 前不消费队列，固定 FIFO 饱和而不依赖时钟。
        for _ in 0..writer::QUEUE_CAPACITY {
            let (done, _receiver) = oneshot::channel();
            if observation
                .inner
                .writer
                .try_send(WriterCommand::Barrier(done))
                .is_err()
            {
                break;
            }
        }
        assert_eq!(observation.inner.writer.capacity(), 0);
        run.observe_client_completion(&[], &[], writer::now(), false);
        run.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        assert!(run.inner.pending_finish.lock().is_some());
        let rejected_projection = test_run(&observation, "no-receipt-slot", facts(Vec::new()));
        assert!(rejected_projection.inner.gap.load(Ordering::Acquire));
        drop(rejected_projection);
        drop(run);
        observation.flush().await?;
        let row: (String, Option<i64>) = sqlx::query_as(
            "SELECT status,finished_at FROM inference_run_observations WHERE id='full-queue-receipt'",
        ).fetch_one(&pool).await?;
        assert_eq!(row.0, "completed");
        assert!(row.1.is_some());
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE run_id='full-queue-receipt' AND kind='run_finished'",
        ).fetch_one(&pool).await?;
        assert_eq!(count, 1);
        let missing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM inference_run_observations WHERE id='no-receipt-slot'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(missing, 0);
        assert!(
            observation
                .inner
                .unpersisted_gaps
                .lock()
                .runs
                .contains_key("no-receipt-slot")
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivered_tool_result_resumes_before_source_generation_commit() -> anyhow::Result<()> {
        use stravia_runtime_contract::protocol::ir::{AiItem, ToolCall};

        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let source = test_run(&observation, "early-tool-source", facts(Vec::new()));
        source.record(RunEvent::ClientToolHandoff {
            tool_id: "early-call".into(),
            name: "probe".into(),
            input: None,
        });
        let call = AiItem::function_call(ToolCall {
            id: "early-call".into(),
            name: "probe".into(),
            arguments: "{}".into(),
        });
        let delivered_at = writer::now();
        source.observe_client_completion(&[], std::slice::from_ref(&call), delivered_at, true);
        observation.flush().await?;

        let items = vec![
            call,
            AiItem::function_call_output("early-call", serde_json::json!("result")),
        ];
        let child = test_run(&observation, "early-tool-child", facts(items.clone()));
        child.capture_client_tool_results(&items);
        child.publish_input_preview();
        child.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        // 客户端已经回传；源 Run 的 Generation 提交和终态可以稍后才完成。
        source.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: Some(delivered_at),
            status: "waiting_client".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        observation.flush().await?;

        let forest = observation.query_forest(Default::default()).await?;
        let interactions: Vec<_> = forest
            .roots
            .iter()
            .flat_map(|root| &root.interactions)
            .collect();
        assert_eq!(interactions.len(), 1, "delivered handoff must not split");
        let detail = observation
            .get_interaction(&interactions[0].id, Default::default())
            .await?
            .expect("interaction");
        let source_run = detail
            .runs
            .iter()
            .find(|run| run.id == source.run_id())
            .unwrap();
        let child_run = detail
            .runs
            .iter()
            .find(|run| run.id == child.run_id())
            .unwrap();
        assert_eq!(child_run.parent_run_id.as_deref(), Some(source.run_id()));
        assert_eq!(source_run.status, "superseded");
        assert_eq!(detail.interaction.status, "completed");
        let finished: Vec<u8> = sqlx::query_scalar(
            "SELECT payload FROM observation_events WHERE run_id=? AND kind='run_finished'",
        )
        .bind(source.run_id())
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored_payload(&finished)?["status"], "superseded");
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn received_input_replay_deduplicates_results_without_erasing_forks() -> anyhow::Result<()>
    {
        use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role, ToolCall};

        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let user = |text: &str| AiItem {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let question = user("run the probe");
        let call = AiItem::function_call(ToolCall {
            id: "history-call".into(),
            name: "probe".into(),
            arguments: "{}".into(),
        });
        let delivered_at = writer::now();
        let finish = |run: &RunObserver, status: &str| {
            run.finish(RunOutcome {
                client_output_committed: true,
                delivery: None,
                delivery_completed_at: Some(writer::now()),
                status: status.into(),
                terminal_reason: None,
                generation_node_id: Some(format!("node-{}", run.run_id())),
                generation_root_id: Some("node-history-source".into()),
            });
        };
        let source = test_run(
            &observation,
            "history-source",
            facts(vec![question.clone()]),
        );
        source.record(RunEvent::ClientToolHandoff {
            tool_id: "history-call".into(),
            name: "probe".into(),
            input: None,
        });
        source.observe_client_completion(
            std::slice::from_ref(&question),
            std::slice::from_ref(&call),
            delivered_at,
            true,
        );
        finish(&source, "waiting_client");
        observation.flush().await?;

        let original = vec![
            question,
            call,
            AiItem::function_call_output("history-call", serde_json::json!("first result")),
        ];
        let continuation_facts = |items: Vec<AiItem>, new_user| {
            let mut admission = facts(items);
            admission.has_new_user = new_user;
            admission.has_matching_pending_tool_result = true;
            admission.generation_parent_id = Some("node-history-source".into());
            admission.generation_root_id = Some("node-history-source".into());
            admission
        };
        let first = test_run(
            &observation,
            "history-received",
            continuation_facts(original.clone(), false),
        );
        first.capture_client_tool_results(&original);
        first.publish_input_preview();
        first.observe_client_completion(
            &original,
            &[AiItem::output_text(
                "the client does not replay this answer",
            )],
            writer::now(),
            false,
        );
        finish(&first, "completed");
        observation.flush().await?;

        let mut replay = original.clone();
        replay.push(user("continue the work"));
        let mut ingress = observation.observe_ingress(IngressStart {
            id: "history-replay".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        });
        // 固定跨过 rapid continuation 窗口，不靠 sleep 决定分组结果。
        ingress.received_at = delivered_at + 5_000;
        let replay_run = ingress.admit(
            RunStart {
                id: "history-replay".into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            },
            continuation_facts(replay.clone(), true),
        );
        replay_run.capture_client_tool_results(&replay);
        replay_run.publish_input_preview();
        finish(&replay_run, "completed");
        observation.flush().await?;

        let admitted: Vec<u8> = sqlx::query_scalar(
            "SELECT payload FROM observation_events WHERE run_id=? AND kind='run_admitted'",
        )
        .bind(replay_run.run_id())
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored_payload(&admitted)?["grouping_reason"], "new_user");
        let replay_results: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE run_id=? AND kind='client_tool_result'",
        )
        .bind(replay_run.run_id())
        .fetch_one(&pool)
        .await?;
        assert_eq!(replay_results, 0, "history is not another tool receipt");

        let mut changed_result = original.clone();
        changed_result[2] =
            AiItem::function_call_output("history-call", serde_json::json!("branch result"));
        let mut changed_user = original.clone();
        changed_user[0] = user("a different branch");
        for (id, input, expected) in [
            ("history-sibling", original, "first result"),
            ("history-different-result", changed_result, "branch result"),
            ("history-different-user", changed_user, "first result"),
        ] {
            let branch = test_run(&observation, id, continuation_facts(input.clone(), false));
            branch.capture_client_tool_results(&input);
            branch.publish_input_preview();
            finish(&branch, "completed");
            observation.flush().await?;
            let result: Vec<u8> = sqlx::query_scalar(
                "SELECT payload FROM observation_events WHERE run_id=? AND kind='client_tool_result'",
            )
            .bind(branch.run_id())
            .fetch_one(&pool)
            .await?;
            assert_eq!(stored_payload(&result)?["content"], expected);
        }
        let results: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE kind='client_tool_result' AND tool_id='history-call'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(results, 4, "each independent branch keeps its own receipt");
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn fixed_publish_deadline_coalesces_revisions_without_stale_scope_snapshot()
    -> anyhow::Result<()> {
        use futures::{FutureExt, StreamExt};

        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let run = test_run(&observation, "publish-budget", facts(Vec::new()));
        observation.flush().await?;
        let interaction: String = sqlx::query_scalar(
            "SELECT interaction_id FROM inference_run_observations WHERE id='publish-budget'",
        )
        .fetch_one(&pool)
        .await?;
        let mut selected = observation.subscribe_live(interaction.clone());
        assert!(matches!(
            selected.next().await,
            Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty()
        ));
        let delta = |text: &str| RunEvent::ClientVisibleContentDelta {
            item_ordinal: 0,
            part_index: (false, 0),
            text: text.into(),
        };
        run.record(delta("seed "));
        observation.flush().await?;
        assert!(matches!(
            selected.next().await,
            Some(ObservationUpdate::LiveContent(block)) if block.text == "seed "
        ));

        // Admission/database work finishes before controlling the publication clock.
        tokio::time::pause();
        tokio::time::advance(Duration::from_millis(100)).await;
        observation.flush().await?;
        let started = tokio::time::Instant::now();
        run.record(delta("middle "));
        observation.flush().await?;
        assert!(selected.next().now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(40)).await;
        run.record(delta("newest "));
        observation.flush().await?;
        let mut switched = observation.subscribe_live(interaction);
        assert!(matches!(
            switched.next().await,
            Some(ObservationUpdate::LiveSnapshot { blocks })
                if blocks[0].text == "seed middle newest "
        ));
        assert!(selected.next().now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(59)).await;
        observation.flush().await?;
        assert!(selected.next().now_or_never().is_none());
        tokio::time::advance(Duration::from_millis(1)).await;
        observation.flush().await?;
        assert!(matches!(
            selected.next().now_or_never(),
            Some(Some(ObservationUpdate::LiveContent(block)))
                if block.text == "seed middle newest "
        ));
        let wait = started.elapsed();
        assert_eq!(wait, Duration::from_millis(100));
        println!("backend_publish_wait_ms={}", wait.as_millis());
        tokio::time::resume();
        drop(run);
        observation.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn final_observer_closes_missing_activity_without_stopping_live_background()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let run = test_run(&observation, "missing-finish", facts(Vec::new()));
        run.record(RunEvent::ModelTurnStarted {
            model_turn_id: "turn".into(),
            route_id: "route".into(),
            model_display_name: None,
            estimated_input_tokens: None,
        });
        run.record(RunEvent::TargetAttemptStarted {
            model_turn_id: "turn".into(),
            attempt_id: "attempt".into(),
            target_id: "target".into(),
            provider_id: "provider".into(),
            provider_name: "Provider".into(),
            upstream_model: "model".into(),
            protocol: "responses".into(),
            upstream_url: "http://localhost".into(),
        });
        run.record(RunEvent::PlatformToolStarted {
            model_turn_id: "turn".into(),
            tool_id: "background".into(),
            name: "tool".into(),
            input: None,
        });
        run.record(RunEvent::UsageConfirmed {
            model_turn_id: "turn".into(),
            attempt_id: "attempt".into(),
            usage: ConfirmedUsage {
                input_tokens: Some(7),
                cache_read_tokens: Some(0),
                ..Default::default()
            },
        });
        observation.flush().await?;
        let early_usage: Option<i64> = sqlx::query_scalar(
            "SELECT input_tokens FROM target_attempt_observations WHERE id='attempt'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(early_usage, Some(7));
        let interaction_id: String = sqlx::query_scalar(
            "SELECT interaction_id FROM inference_run_observations WHERE id='missing-finish'",
        )
        .fetch_one(&pool)
        .await?;
        let early_detail = observation
            .get_interaction(&interaction_id, ForestQuery::default())
            .await?
            .expect("active interaction");
        let early_run = early_detail
            .runs
            .iter()
            .find(|run| run.id == "missing-finish")
            .expect("active run");
        assert_eq!(early_run.usage.input_tokens, Some(7));
        let early_usage_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE kind='usage_confirmed'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(early_usage_rows, 0);
        let background = run.clone();
        run.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
            delivery_completed_at: Some(writer::now()),
        });
        drop(run);
        observation.flush().await?;
        let active: (String, i64) = sqlx::query_as("SELECT i.status,r.background_active FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id='missing-finish'").fetch_one(&pool).await?;
        assert_eq!(
            active,
            ("running".into(), 2),
            "delivery cannot end live background work"
        );
        background.record(RunEvent::PlatformToolFinished {
            model_turn_id: "turn".into(),
            tool_id: "background".into(),
            status: "completed".into(),
            duration_ms: 1,
            content: None,
        });
        drop(background);
        observation.flush().await?;
        let final_state: (String, String, i64, bool, String, String, Option<i64>) = sqlx::query_as("SELECT i.status,r.status,r.background_active,i.observation_gap,m.status,a.status,a.input_tokens FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id JOIN model_turn_observations m ON m.run_id=r.id JOIN target_attempt_observations a ON a.run_id=r.id WHERE r.id='missing-finish'").fetch_one(&pool).await?;
        assert_eq!(
            final_state,
            (
                "completed".into(),
                "completed".into(),
                0,
                true,
                "interrupted".into(),
                "interrupted".into(),
                Some(7)
            )
        );
        let standalone_usage: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE kind='usage_confirmed'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(standalone_usage, 0);
        let terminal_attempt: Vec<u8> = sqlx::query_scalar("SELECT payload FROM observation_events WHERE kind='target_attempt_finished' AND run_id='missing-finish' ORDER BY sequence DESC LIMIT 1")
            .fetch_one(&pool).await?;
        assert_eq!(
            stored_payload(&terminal_attempt)?["usage"]["input_tokens"],
            7
        );
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn early_usage_updates_queries_without_advancing_event_sequence() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let run = test_run(&observation, "early-usage", facts(Vec::new()));
        run.record(RunEvent::ModelTurnStarted {
            model_turn_id: "early-turn".into(),
            route_id: "route".into(),
            model_display_name: None,
            estimated_input_tokens: None,
        });
        run.record(RunEvent::TargetAttemptStarted {
            model_turn_id: "early-turn".into(),
            attempt_id: "early-attempt".into(),
            target_id: "target".into(),
            provider_id: "provider".into(),
            provider_name: "Provider".into(),
            upstream_model: "model".into(),
            protocol: "responses".into(),
            upstream_url: "http://localhost".into(),
        });
        observation.flush().await?;
        let before_usage = observation.inner.store.max_sequence().await?;
        run.record(RunEvent::UsageConfirmed {
            model_turn_id: "early-turn".into(),
            attempt_id: "early-attempt".into(),
            usage: ConfirmedUsage {
                input_tokens: Some(7),
                cache_read_tokens: Some(0),
                ..Default::default()
            },
        });
        observation.flush().await?;
        assert_eq!(observation.inner.store.max_sequence().await?, before_usage);
        let usage: (Option<i64>, bool) = sqlx::query_as(
            "SELECT input_tokens,usage_recorded FROM target_attempt_observations WHERE id='early-attempt'",
        ).fetch_one(&pool).await?;
        assert_eq!(usage, (Some(7), true));
        let interaction_id: String = sqlx::query_scalar(
            "SELECT interaction_id FROM inference_run_observations WHERE id='early-usage'",
        )
        .fetch_one(&pool)
        .await?;
        let detail = observation
            .get_interaction(&interaction_id, ForestQuery::default())
            .await?
            .expect("active interaction");
        let queried_run = detail
            .runs
            .iter()
            .find(|run| run.id == "early-usage")
            .expect("active run");
        assert_eq!(queried_run.usage.input_tokens, Some(7));
        let obsolete_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE kind='usage_confirmed'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(obsolete_rows, 0);
        drop(run);
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn unmergeable_text_overflow_preserves_lifecycle_and_parent_mapping() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let parent = test_run(&observation, "protected-parent", facts(Vec::new()));
        parent.record(RunEvent::ModelTurnStarted {
            model_turn_id: "turn".into(),
            route_id: "route".into(),
            model_display_name: None,
            estimated_input_tokens: None,
        });
        for index in 0..4096 {
            parent.send_event(RunEvent::ModelThinkingDelta {
                model_turn_id: "turn".into(),
                attempt_id: (index % 2).to_string(),
                item_ordinal: 0,
                part_index: (false, 0),
                text: "x".into(),
            });
        }
        parent.record(RunEvent::ModelTurnFinished {
            model_turn_id: "turn".into(),
            status: "completed".into(),
        });
        parent.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: Some("generation".into()),
            generation_root_id: Some("generation".into()),
            delivery_completed_at: Some(writer::now()),
        });
        let mut continuation = facts(Vec::new());
        continuation.has_new_user = false;
        continuation.generation_parent_id = Some("generation".into());
        continuation.generation_root_id = Some("generation".into());
        let child = test_run(&observation, "protected-child", continuation);
        child.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: Some("generation".into()),
            delivery_completed_at: Some(writer::now()),
        });
        drop(parent);
        drop(child);
        observation.flush().await?;
        let child_parent: Option<String> = sqlx::query_scalar(
            "SELECT parent_run_id FROM inference_run_observations WHERE id='protected-child'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(child_parent.as_deref(), Some("protected-parent"));
        let model_status: String =
            sqlx::query_scalar("SELECT status FROM model_turn_observations WHERE id='turn'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            model_status, "completed",
            "finish facts must survive text overflow"
        );
        let gap: bool =
            sqlx::query_scalar("SELECT observation_gap FROM interaction_observations LIMIT 1")
                .fetch_one(&pool)
                .await?;
        assert!(gap, "discarded text must remain explicit");
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_generation_parent_is_an_explicit_observation_gap() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let mut continuation = facts(Vec::new());
        continuation.has_new_user = false;
        continuation.generation_parent_id = Some("unobserved-generation".into());
        let run = test_run(&observation, "orphan-continuation", continuation);
        observation.flush().await?;
        let state: (Option<String>, bool) = sqlx::query_as("SELECT r.parent_run_id,i.observation_gap FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id='orphan-continuation'").fetch_one(&pool).await?;
        assert_eq!(
            state,
            (None, true),
            "missing evidence must not fabricate a parent or appear complete"
        );
        drop(run);
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn text_burst_preserves_admission_finish_and_continuation() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let admit = |id: &str, facts| {
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
                        principal: "owner".into(),
                        api_key_id: None,
                        api_key_name: None,
                        route_id: "route".into(),
                        model_display_name: None,
                        ingress_protocol: "responses".into(),
                    },
                    facts,
                )
        };
        let parent = admit("burst-parent", facts(Vec::new()));
        // 同一调度片内的细粒度流输出不能耗尽生命周期消息的队列容量。
        for index in 0..16384 {
            if index == 8192 {
                parent.send_event(RunEvent::ClientOutputCommitted);
            }
            parent.send_event(RunEvent::ClientVisibleContentDelta {
                text: "文".into(),
                item_ordinal: 0,
                part_index: (false, 0),
            });
        }
        parent.record(visible_item("burst-item", &"文".repeat(16384)));
        parent.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: Some("burst-generation".into()),
            generation_root_id: Some("burst-generation".into()),
            delivery_completed_at: Some(writer::now()),
        });
        let mut continuation = facts(Vec::new());
        continuation.has_new_user = false;
        continuation.generation_parent_id = Some("burst-generation".into());
        continuation.generation_root_id = Some("burst-generation".into());
        let child = admit("burst-child", continuation);
        child.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: Some("child-generation".into()),
            generation_root_id: Some("burst-generation".into()),
            delivery_completed_at: Some(writer::now()),
        });
        drop(parent);
        drop(child);
        observation.flush().await?;
        let rows: Vec<RunRow> = sqlx::query_as("SELECT id,interaction_id,parent_run_id,status,generation_node_id FROM inference_run_observations ORDER BY id")
                .fetch_all(&pool).await?;
        assert_eq!(rows.len(), 2, "text deltas must not displace admission");
        assert_eq!(
            rows[0].1, rows[1].1,
            "continuation must retain its interaction"
        );
        assert_eq!(rows[0].2.as_deref(), Some("burst-parent"));
        assert_eq!(rows[1].3, "completed");
        assert_eq!(rows[1].4.as_deref(), Some("burst-generation"));
        let payload: Vec<u8> = sqlx::query_scalar("SELECT payload FROM observation_events WHERE run_id='burst-parent' AND kind='client_visible_content'")
            .fetch_one(&pool).await?;
        let payload = stored_payload(&payload)?;
        assert_eq!(payload["text"], "文".repeat(16384));
        assert_eq!(payload["block_id"], "burst-item");
        assert_eq!(payload["complete"], true);
        let volatile_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observation_events WHERE run_id='burst-parent' AND kind IN ('client_visible_content_delta','client_output_committed')")
            .fetch_one(&pool).await?;
        assert_eq!(volatile_rows, 0);
        observation.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn connection_close_and_tool_handoff_commute() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        for close_first in [false, true] {
            let connection = ClientConnectionObservation::new(observation.clone());
            let mut observers = Vec::new();
            for status in ["waiting_client", "completed"] {
                let id = format!("{close_first}-{status}");
                let observer = observation
                    .observe_ingress(IngressStart {
                        id: id.clone(),
                        method: "WEBSOCKET".into(),
                        path: "/v1/responses".into(),
                        protocol: "responses".into(),
                    })
                    .admit(
                        RunStart {
                            id: id.clone(),
                            principal: "owner".into(),
                            api_key_id: None,
                            api_key_name: None,
                            route_id: "route".into(),
                            model_display_name: None,
                            ingress_protocol: "responses".into(),
                        },
                        facts(Vec::new()),
                    );
                if close_first {
                    connection.close();
                }
                observer.finish(RunOutcome {
                    client_output_committed: false,
                    delivery: None,
                    status: status.into(),
                    terminal_reason: None,
                    generation_node_id: Some(format!("node-{id}")),
                    generation_root_id: Some(format!("node-{id}")),
                    delivery_completed_at: Some(writer::now()),
                });
                // 持久层仍须保护已完成响应，不能把连接关闭当成交付失败。
                connection.waiting(&observer);
                observers.push(observer);
            }
            connection.close();
            connection.close();
            observation.flush().await?;
            let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT status,terminal_reason,generation_node_id FROM inference_run_observations WHERE id LIKE ? ORDER BY id",
            ).bind(format!("{close_first}-%")).fetch_all(&pool).await?;
            assert_eq!(
                rows,
                vec![
                    (
                        "completed".into(),
                        None,
                        Some(format!("node-{close_first}-completed"))
                    ),
                    (
                        "disconnected".into(),
                        Some("client_disconnected".into()),
                        Some(format!("node-{close_first}-waiting_client"))
                    ),
                ]
            );
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM observation_events WHERE run_id=? AND kind='run_state_changed'",
            ).bind(format!("{close_first}-waiting_client")).fetch_one(&pool).await?;
            assert_eq!(count, 1);
            drop(observers);
        }
        observation.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn committed_details_remain_readable_without_a_writer() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let observer = observation
            .observe_ingress(IngressStart {
                id: "read-only-ingress".into(),
                method: "POST".into(),
                path: "/responses".into(),
                protocol: "responses".into(),
            })
            .admit(
                RunStart {
                    id: "read-only-run".into(),
                    principal: "test".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                facts(Vec::new()),
            );
        observer.record(visible_item("saved-answer", "saved answer"));
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
                .any(|event| event.kind == "client_visible_content"
                    && event.payload["text"] == "saved answer")
        );
        assert_eq!(observation.inner.store.max_sequence().await?, before);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn clear_debug_active_trace_does_not_resurrect() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        observation.set_debug_enabled(true);
        let active = test_run(&observation, "cleared-active", facts(Vec::new()));
        observation.flush().await?;
        let old = active
            .inner
            .trace
            .as_ref()
            .expect("debug trace")
            .manifest()
            .trace_id;
        let old_directory = directory.path().join("observation-debug").join(&old);
        assert!(old_directory.join("manifest.json").is_file());
        observation.clear_debug().await?;
        observation.flush().await?;
        observation.sweep_retention().await?;
        assert!(!old_directory.exists());
        assert!(
            observation
                .inner
                .store
                .debug_trace_index()
                .get(&old)
                .is_none()
        );
        drop(active);
        observation.flush().await?;
        observation.sweep_retention().await?;
        assert!(!old_directory.exists());
        assert!(
            observation
                .inner
                .store
                .debug_trace_index()
                .get(&old)
                .is_none()
        );
        let fresh = test_run(&observation, "fresh-after-clear", facts(Vec::new()));
        observation.flush().await?;
        let new = fresh
            .inner
            .trace
            .as_ref()
            .expect("new debug trace")
            .manifest()
            .trace_id;
        drop(fresh);
        observation.flush().await?;
        assert!(
            directory
                .path()
                .join("observation-debug")
                .join(&new)
                .join("manifest.json")
                .is_file()
        );
        assert!(
            observation
                .inner
                .store
                .debug_trace_index()
                .get(&new)
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn clear_debug_retires_unadmitted_trace_without_active_writer() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        observation.set_debug_enabled(true);
        let ingress = observation.observe_ingress(IngressStart {
            id: "unadmitted".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "responses".into(),
        });
        let trace = ingress.trace.as_ref().expect("ingress capture").clone();
        // Finish removes ActiveWriter while the ingress handle still owns its state,
        // reproducing the ownership shape of failed Create/recording without filesystem timing.
        let old = trace.finish().await.trace_id;
        trace.mark_partial("storage_error", true);
        assert!(observation.inner.active_traces.lock().is_empty());
        observation.clear_debug().await?;
        drop(ingress);
        observation.flush().await?;
        observation.sweep_retention().await?;
        assert!(
            trace
                .manifest()
                .reasons
                .iter()
                .any(|reason| reason == "debug_data_cleared")
        );
        assert!(
            !directory
                .path()
                .join("observation-debug")
                .join(&old)
                .exists()
        );
        assert!(
            observation
                .inner
                .store
                .debug_trace_index()
                .get(&old)
                .is_none()
        );
        assert_eq!(
            observation.inner.store.debug_manifest_counts().await?,
            (0, 0)
        );
        Ok(())
    }

    #[tokio::test]
    async fn wire_capture_is_eligible_at_the_durable_sequence_it_was_observed_at()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        observation.set_debug_enabled(true);
        let observer = test_run(&observation, "wire-frontier", facts(Vec::new()));
        observation.flush().await?;
        let trace = observer.inner.trace.clone().expect("debug trace");
        // No ordinary event lands after the capture: a ticket at the durable
        // frontier must still include wire bytes observed while the log stood
        // at that sequence.
        let frontier = observation.inner.store.max_sequence().await?;
        assert!(frontier >= 1);
        observer.record(RunEvent::Wire {
            direction: "upstream_response".into(),
            transport: "http".into(),
            protocol: "responses".into(),
            message_type: "body_chunk".into(),
            model_turn_id: None,
            attempt_id: None,
            status_code: Some(200),
            url: None,
            headers: Value::Null,
            payload: serde_json::json!({"marker": "incomplete-prefix"}),
        });
        observation.flush().await?;
        let at_frontier = load_trace_values(trace.snapshot(frontier).await?).await?;
        assert_eq!(
            at_frontier
                .iter()
                .filter_map(|record| record["payload"]["marker"].as_str())
                .collect::<Vec<_>>(),
            ["incomplete-prefix"]
        );
        // The same record stays outside snapshots bounded before the frontier.
        let before_frontier = load_trace_values(trace.snapshot(frontier - 1).await?).await?;
        assert!(before_frontier.is_empty());
        drop(observer);
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn blocked_trace_root_retains_terminal_manifest_state() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        observation.set_debug_enabled(true);
        // Displace the managed root after startup so every manifest write fails.
        let root = directory.path().join("observation-debug");
        let displaced = directory.path().join("observation-debug-displaced");
        std::fs::rename(&root, &displaced)?;
        std::fs::write(&root, b"regular file blocks managed trace directories")?;

        let active = test_run(&observation, "blocked-run", facts(Vec::new()));
        let trace = active.inner.trace.clone().expect("debug trace");
        observation.flush().await?;
        // The trace channel barrier orders the failed Create before the assert.
        trace.flush().await?;
        // While the capture is active the live handle already counts as partial
        // and no manifest answers run-level queries yet.
        assert!(observation.debug_state().partial_trace_count >= 1);
        let index = observation.inner.store.debug_trace_index();
        assert!(index.for_run("blocked-run").is_none());

        active.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
            delivery_completed_at: Some(writer::now()),
        });
        drop(active);
        observation.flush().await?;
        // The finalized manifest is retained in the in-memory index even though
        // the durable write failed, so queries and counts still see it.
        assert!(observation.debug_state().partial_trace_count >= 1);
        assert_eq!(
            index.for_run("blocked-run").map(|m| m.status).as_deref(),
            Some("partial")
        );
        let (_, partial) = observation.inner.store.debug_manifest_counts().await?;
        assert!(partial >= 1);
        observation.sweep_retention().await?;
        assert!(observation.debug_state().partial_trace_count >= 1);
        // Clear and retention erase the retained state without durable files.
        observation.clear_debug().await?;
        assert_eq!(observation.debug_state().partial_trace_count, 0);
        assert!(index.for_run("blocked-run").is_none());
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn trace_capture_flushes_to_files_without_database_debug_events() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        for enabled in [true, false] {
            observation.set_debug_enabled(enabled);
            let observer = test_run(&observation, &format!("trace-{enabled}"), facts(Vec::new()));
            observation.flush().await?;
            let trace = observer.inner.trace.clone();
            for marker in ["before-flush", "after-flush"] {
                observer.record(RunEvent::Wire {
                    direction: "platform_to_client".into(),
                    transport: "http".into(),
                    protocol: "responses".into(),
                    message_type: "body_chunk".into(),
                    model_turn_id: None,
                    attempt_id: None,
                    status_code: Some(200),
                    url: None,
                    headers: Value::Null,
                    payload: serde_json::json!({"marker":marker}),
                });
                observation.flush().await?;
            }
            observer.finish(RunOutcome {
                client_output_committed: false,
                delivery: None,
                status: "completed".into(),
                terminal_reason: None,
                generation_node_id: None,
                generation_root_id: None,
                delivery_completed_at: Some(writer::now()),
            });
            drop(observer);
            observation.flush().await?;
            if let Some(trace) = trace {
                let records = load_trace_values(trace.snapshot(i64::MAX).await?).await?;
                let markers: Vec<_> = records
                    .iter()
                    .filter_map(|record| record["payload"]["marker"].as_str())
                    .collect();
                assert_eq!(markers, ["before-flush", "after-flush"]);
                let manifest_path = directory
                    .path()
                    .join("observation-debug")
                    .join(&trace.manifest().trace_id)
                    .join("manifest.json");
                let manifest: Value =
                    serde_json::from_slice(&tokio::fs::read(manifest_path).await?)?;
                assert_eq!(manifest["status"], "complete");
                assert_eq!(manifest["run_id"], format!("trace-{enabled}"));
            } else {
                assert!(!enabled);
            }
        }
        let debug_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observation_events WHERE kind IN ('wire','content','target_selected','trace_manifest_updated')").fetch_one(&pool).await?;
        assert_eq!(debug_rows, 0);
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn historical_user_preview_requires_received_prefix_evidence() -> anyhow::Result<()> {
        use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role, ToolCall};

        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let user = AiItem {
            role: Role::User,
            content: MessageContent::Text("repeat preview-secret".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let publish = |id: &str, input: &[AiItem]| {
            let run = test_run(&observation, id, facts(input.to_vec()));
            run.capture_input_preview(Arc::new(
                stravia_runtime_contract::protocol::ir::AiRequest::new("model", input.to_vec()),
            ));
            run.capture_client_tool_results(input);
            run.protect_secrets(["preview-secret"]);
            run.publish_input_preview();
            run.publish_input_preview();
            run
        };
        let deliver = |run: &RunObserver, input: &[AiItem], id: &str| {
            let call = AiItem::function_call(ToolCall {
                id: id.into(),
                name: "probe".into(),
                arguments: "{}".into(),
            });
            run.record(RunEvent::ClientToolHandoff {
                tool_id: id.into(),
                name: "probe".into(),
                input: None,
            });
            run.observe_client_completion(input, std::slice::from_ref(&call), writer::now(), true);
            run.finish(RunOutcome {
                client_output_committed: true,
                delivery: None,
                delivery_completed_at: Some(writer::now()),
                status: "waiting_client".into(),
                terminal_reason: None,
                generation_node_id: None,
                generation_root_id: None,
            });
            call
        };
        let mut input = vec![user.clone()];
        let initial = publish("preview-initial", &input);
        let call = deliver(&initial, &input, "preview-call-1");
        observation.flush().await?;
        input.extend([
            call,
            AiItem::function_call_output("preview-call-1", serde_json::json!("ok")),
        ]);

        // This is a separate fork, despite returning the exact pending tool ID.
        let mut fork = input.clone();
        fork[0].content = MessageContent::Text("changed prefix preview-secret".into());
        let branch = publish("preview-fork", &fork);
        branch.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        let continuation = publish("preview-continuation", &input);
        let call = deliver(&continuation, &input, "preview-call-2");
        observation.flush().await?;
        input.extend([
            call,
            AiItem::function_call_output("preview-call-2", serde_json::json!("ok")),
        ]);
        // Identical text is a new input when its canonical item follows history.
        input.push(user);
        let followup = publish("preview-followup", &input);
        let call = deliver(&followup, &input, "preview-call-3");
        observation.flush().await?;
        input.extend([
            call,
            AiItem::function_call_output("preview-call-3", serde_json::json!("ok")),
        ]);
        let continuation = publish("preview-followup-continuation", &input);
        deliver(&continuation, &input, "preview-call-4");
        observation.flush().await?;

        // Restart loses process-local input proofs. No Generation node exists
        // to rematerialize them, so the same input must conservatively survive.
        observation.shutdown().await;
        let observation = test_observation(&pool, directory.path(), false).await;
        let unknown = test_run(&observation, "preview-unavailable", facts(input.clone()));
        unknown.capture_input_preview(Arc::new(
            stravia_runtime_contract::protocol::ir::AiRequest::new("model", input.clone()),
        ));
        unknown.protect_secrets(["preview-secret"]);
        unknown.publish_input_preview();
        observation.flush().await?;
        let previews: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT run_id,payload FROM observation_events WHERE kind='input_preview_recorded' ORDER BY sequence",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            previews
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            [
                "preview-initial",
                "preview-fork",
                "preview-followup",
                "preview-unavailable"
            ]
        );
        for (_, payload) in previews {
            assert!(
                !stored_payload(&payload)?["text"]
                    .as_str()
                    .unwrap()
                    .contains("preview-secret")
            );
        }
        let forest = observation.query_forest(Default::default()).await?;
        let mut queried_previews = Vec::new();
        for interaction in forest.roots.iter().flat_map(|root| &root.interactions) {
            let detail = observation
                .get_interaction(&interaction.id, Default::default())
                .await?
                .expect("persisted interaction");
            assert!(
                detail
                    .runs
                    .iter()
                    .all(|run| run.generation_parent_id.is_none())
            );
            for run in &detail.runs {
                for event in &run.events {
                    if event.kind == "input_preview_recorded" {
                        queried_previews.push(run.id.clone());
                        assert!(!event.payload.to_string().contains("preview-secret"));
                    }
                }
            }
        }
        queried_previews.sort();
        assert_eq!(
            queried_previews,
            [
                "preview-followup",
                "preview-fork",
                "preview-initial",
                "preview-unavailable"
            ]
        );
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn verified_parent_historical_user_preview_is_not_republished() -> anyhow::Result<()> {
        use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role, ToolCall};

        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), false).await;
        let question = AiItem {
            role: Role::User,
            content: MessageContent::Text("question".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        };
        let parent = test_run(
            &observation,
            "preview-verified-parent",
            facts(vec![question.clone()]),
        );
        parent.capture_input_preview(Arc::new(
            stravia_runtime_contract::protocol::ir::AiRequest::new("model", vec![question.clone()]),
        ));
        parent.publish_input_preview();
        parent.finish(RunOutcome {
            client_output_committed: true,
            delivery: None,
            delivery_completed_at: Some(writer::now()),
            status: "waiting_client".into(),
            terminal_reason: None,
            generation_node_id: Some("preview-verified-node".into()),
            generation_root_id: Some("preview-verified-node".into()),
        });
        observation.flush().await?;
        let mut input = vec![
            question.clone(),
            AiItem::function_call(ToolCall {
                id: "verified-call".into(),
                name: "probe".into(),
                arguments: "{}".into(),
            }),
            AiItem::function_call_output("verified-call", serde_json::json!("ok")),
        ];
        for (id, new_user) in [
            ("preview-verified-history", false),
            ("preview-verified-new-user", true),
        ] {
            if new_user {
                input.push(question.clone());
            }
            let mut admission = facts(input.clone());
            admission.generation_parent_id = Some("preview-verified-node".into());
            let child = test_run(&observation, id, admission);
            child.capture_input_preview(Arc::new(
                stravia_runtime_contract::protocol::ir::AiRequest::new("model", input.clone()),
            ));
            child.publish_input_preview();
        }
        observation.flush().await?;
        let previews: Vec<String> = sqlx::query_scalar(
            "SELECT run_id FROM observation_events WHERE kind='input_preview_recorded' ORDER BY sequence",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            previews,
            ["preview-verified-parent", "preview-verified-new-user"]
        );
        let payload: Vec<u8> = sqlx::query_scalar(
            "SELECT payload FROM observation_events WHERE kind='run_admitted' AND run_id='preview-verified-history'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored_payload(&payload)?["has_new_user"], false);
        observation.shutdown().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn input_preview_is_root_owned_and_recorded_once_per_run() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let at = writer::now();
        sqlx::query("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,expires_at) VALUES ('historical','test','historical','initial','route','running',?,?,?)")
            .bind(at).bind(at).bind(at + 86_400_000).execute(&pool).await?;
        let preview: Option<String> = sqlx::query_scalar(
            "SELECT input_preview FROM interaction_observations WHERE id='historical'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(preview, None);
        sqlx::query("INSERT INTO inference_run_observations(id,interaction_id,parent_run_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,expires_at) VALUES ('initial','historical',NULL,'openai','route','running',0,?,?,?),('child','historical','initial','openai','route','running',0,?,?,?)")
            .bind(at).bind(at).bind(at + 86_400_000)
            .bind(at + 1).bind(at + 1).bind(at + 86_400_000).execute(&pool).await?;
        let store = store::ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(manifest_index::DebugTraceIndex::load(directory.path())?),
            Arc::new(tokio::sync::Mutex::new(())),
        );
        assert!(
            store
                .persist_input_preview("historical", "missing", "not owned", at, at + 86_400_000)
                .await?
                .is_none()
        );
        let initial = store
            .persist_input_preview("historical", "initial", "first input", at, at + 86_400_000)
            .await?
            .expect("initial run input event");
        assert!(
            store
                .persist_input_preview(
                    "historical",
                    "initial",
                    "ignored duplicate",
                    at,
                    at + 86_400_000
                )
                .await?
                .is_none()
        );
        let child = store
            .persist_input_preview(
                "historical",
                "child",
                "follow-up input",
                at + 1,
                at + 86_400_000,
            )
            .await?
            .expect("child run input event");
        assert!(initial.sequence < child.sequence);
        assert!(
            store
                .persist_input_preview(
                    "historical",
                    "child",
                    "ignored duplicate",
                    at + 1,
                    at + 86_400_000
                )
                .await?
                .is_none()
        );
        let preview: Option<String> = sqlx::query_scalar(
            "SELECT input_preview FROM interaction_observations WHERE id='historical'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(preview.as_deref(), Some("first input"));
        let events: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT run_id,payload FROM observation_events WHERE kind='input_preview_recorded' ORDER BY sequence",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            events
                .iter()
                .map(|(run_id, _)| run_id.as_str())
                .collect::<Vec<_>>(),
            ["initial", "child"]
        );
        assert_eq!(
            events
                .iter()
                .map(|(_, payload)| stored_payload(payload))
                .collect::<Result<Vec<_>, _>>()?,
            [
                serde_json::json!({"kind": "input_preview_recorded", "text": "first input"}),
                serde_json::json!({"kind": "input_preview_recorded", "text": "follow-up input"}),
            ]
        );
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
        crate::migrations::migrate_sqlite(&pool, None).await?;
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
                .bind(sequence).bind(time).bind(interaction).bind(crate::storage_codec::encode(payload.as_bytes())?)
                .bind(now + 86_400_000).execute(&pool).await?;
        }
        pool.close().await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = store::ObservationStore::Sqlite(
            pool.clone(),
            Arc::new(manifest_index::DebugTraceIndex::load(directory.path())?),
            Arc::new(tokio::sync::Mutex::new(())),
        );
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
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let mut observation = test_observation(&pool, directory.path(), false).await;
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
                    ingress.admit(
                        RunStart {
                            id: id.clone(),
                            principal: principal.into(),
                            api_key_id: None,
                            api_key_name: None,
                            route_id: "route".into(),
                            model_display_name: None,
                            ingress_protocol: "responses".into(),
                        },
                        AdmissionFacts {
                            has_new_user: new_user,
                            has_matching_pending_tool_result: tools,
                            generation_parent_id: parent,
                            ..facts(Vec::new())
                        },
                    )
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
                    client_output_committed: false,
                    delivery: None,
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
                    observation = test_observation(&pool, directory.path(), false).await;
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
                let admitted_payload: Vec<u8> = sqlx::query_scalar(
                    "SELECT payload FROM observation_events WHERE run_id=? AND kind='run_admitted'",
                )
                .bind(&id)
                .fetch_one(&pool)
                .await?;
                assert_eq!(
                    stored_payload(&admitted_payload)?["grouping_reason"],
                    reason,
                    "{id}"
                );
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
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let make_run = |id: &str| {
            observation
                .observe_ingress(IngressStart {
                    id: format!("ingress-{id}"),
                    method: "POST".into(),
                    path: "/responses".into(),
                    protocol: "responses".into(),
                })
                .admit(
                    RunStart {
                        id: id.into(),
                        principal: "api-key:test".into(),
                        api_key_id: None,
                        api_key_name: Some("test".into()),
                        route_id: "test-route".into(),
                        model_display_name: None,
                        ingress_protocol: "responses".into(),
                    },
                    facts(Vec::new()),
                )
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
            client_output_committed: false,
            delivery: None,
            delivery_completed_at: None,
            status: "completed".into(),
            terminal_reason: None,
            generation_node_id: None,
            generation_root_id: None,
        });
        observation.flush().await?;
        {
            let mut gaps = observation.inner.unpersisted_gaps.lock();
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
            .record("active", writer::now());
        observation.clear_history().await?;
        let page = observation
            .credential_discoveries(CredentialDiscoveryQuery::default())
            .await?;
        assert!(page.observation_gap);
        assert_eq!(page.items[0].interaction_id, active_interaction);
        assert_eq!(page.items[0].new_credential_count, 1);

        active.finish(RunOutcome {
            client_output_committed: false,
            delivery: None,
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
        let pool = crate::test_support::migrated_sqlite_pool().await?;
        let observation = test_observation(&pool, directory.path(), true).await;
        let observer = observation
            .observe_ingress(IngressStart {
                id: "ingress".into(),
                method: "POST".into(),
                path: "/responses".into(),
                protocol: "responses".into(),
            })
            .admit(
                RunStart {
                    id: "run".into(),
                    principal: "api-key:test".into(),
                    api_key_id: None,
                    api_key_name: None,
                    route_id: "route".into(),
                    model_display_name: None,
                    ingress_protocol: "responses".into(),
                },
                AdmissionFacts {
                    has_new_user: false,
                    ..facts(Vec::new())
                },
            );
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
        observer.record(RunEvent::ModelThinking {
            model_turn_id: "turn".into(),
            attempt_id: "attempt".into(),
            text: "private reasoning".into(),
            parts: vec![serde_json::json!({"type":"thinking","text":"private reasoning"})],
            block_id: "private-reasoning".into(),
            item: serde_json::json!({"id":"private-reasoning","content":"private reasoning"}),
            complete: true,
        });
        observer.record(visible_item("public-answer", "public answer"));
        drop(observer);
        observation.flush().await?;
        let rows: Vec<(String, Vec<u8>)> =
            sqlx::query_as("SELECT kind, payload FROM observation_events ORDER BY sequence")
                .fetch_all(&pool)
                .await?;
        let events: Vec<(String, Value)> = rows
            .into_iter()
            .map(|(kind, payload)| (kind, stored_payload(&payload).expect("event payload")))
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
            .filter(|event| event.0 == "model_thinking")
            .filter_map(|event| event.1["text"].as_str())
            .collect();
        assert_eq!(thinking, "private reasoning");
        assert!(events.iter().all(|event| !matches!(
            event.0.as_str(),
            "model_thinking_delta" | "model_thinking_finished" | "client_visible_content_delta"
        )));
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
}
