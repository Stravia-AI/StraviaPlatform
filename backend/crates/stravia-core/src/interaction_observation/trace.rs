use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::redaction::{REDACTED, RedactionKind};
use super::types::TraceManifest;

pub(crate) const TRACE_SCHEMA_VERSION: u32 = 2;
const SEGMENT_LIMIT_BYTES: u64 = 8 * 1024 * 1024;
const WRITE_BATCH_BYTES: usize = 64 * 1024;
const WRITER_QUEUE_CAPACITY: usize = 1024;
const MANAGED_DIRECTORY: &str = "observation-debug";
const WRITER_OVERFLOW: &str = "writer_overflow";
const STORAGE_ERROR: &str = "storage_error";
const WIRE_CAPTURE_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const WIRE_CAPTURE_LIMIT: &str = "wire_capture_limit";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TraceRecord {
    pub schema_version: u32,
    pub sequence: i64,
    pub recorded_at: i64,
    pub interaction_id: Option<String>,
    pub run_id: Option<String>,
    pub rejection_id: Option<String>,
    pub model_turn_id: Option<String>,
    pub attempt_id: Option<String>,
    pub layer: String,
    pub direction: Option<String>,
    pub stage: Option<String>,
    pub transport: Option<String>,
    pub protocol: Option<String>,
    pub message_type: Option<String>,
    pub representation: String,
    pub status: Option<String>,
    pub status_code: Option<u16>,
    pub url: Option<String>,
    pub headers: Value,
    pub payload_encoding: String,
    pub payload: Value,
    pub error: Option<String>,
    pub redactions: Vec<RedactionKind>,
}

impl TraceRecord {
    fn redact_before_queue(&mut self) {
        self.schema_version = TRACE_SCHEMA_VERSION;
        let mut kinds: BTreeSet<_> = self.redactions.drain(..).collect();
        if redact_authorization_headers(&mut self.headers) {
            kinds.insert(RedactionKind::CredentialHeader);
        }
        if self.transport.as_deref() == Some("websocket")
            && matches!(self.message_type.as_deref(), Some("ping" | "pong"))
        {
            // Ping/Pong are intentionally metadata-only. Their application payload is
            // not a complete wire capture and must not be presented as one.
            self.payload_encoding = "json".into();
            self.payload = serde_json::json!({
                "original_wire_bytes": false,
                "content_capture": "omitted",
                "reason": "control_frame_payload_omitted"
            });
        }
        self.redactions = kinds.into_iter().collect();
    }
}

pub(super) fn redact_authorization_headers(headers: &mut Value) -> bool {
    let Value::Object(headers) = headers else {
        return false;
    };
    let mut redacted = false;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        redact_header_value(value);
        redacted = true;
    }
    redacted
}

fn redact_header_value(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                redact_header_value(value);
            }
        }
        _ => *value = Value::String(REDACTED.to_owned()),
    }
}

fn wire_value_size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => 16,
        Value::String(text) => text.len(),
        Value::Array(values) => values
            .iter()
            .map(wire_value_size)
            .fold(0, usize::saturating_add),
        Value::Object(values) => values.iter().fold(0, |total, (key, value)| {
            total
                .saturating_add(key.len())
                .saturating_add(wire_value_size(value))
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceWriteOutcome {
    Queued,
    Partial(&'static str),
}

#[derive(Debug, Clone)]
pub(crate) struct TraceSnapshot {
    pub segments: Vec<TraceSegmentSnapshot>,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct TraceSegmentSnapshot {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReconcileReport {
    pub removed_tombstones: Vec<String>,
    pub removed_orphans: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct TraceManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    root: PathBuf,
    tx: mpsc::Sender<WriterCommand>,
    producer: parking_lot::Mutex<RecordProducer>,
    actual_retained: AtomicU64,
    available: bool,
}

#[derive(Default)]
struct RecordProducer {
    // 固定先锁生产端、再锁批次；writer 只持批次锁，避免与生产端互相等待。
    appendable: Option<Arc<parking_lot::Mutex<QueuedRecordBatch>>>,
}

struct QueuedRecordBatch {
    trace_id: Arc<str>,
    state: Arc<TraceState>,
    records: Vec<Vec<u8>>,
    bytes: usize,
    sealed: bool,
}

impl QueuedRecordBatch {
    fn new(trace_id: Arc<str>, state: Arc<TraceState>, bytes: Vec<u8>) -> Self {
        let byte_count = bytes.len();
        Self {
            trace_id,
            state,
            records: vec![bytes],
            bytes: byte_count,
            sealed: false,
        }
    }

    fn can_append(&self, trace_id: &str, bytes: usize) -> bool {
        !self.sealed
            && self.trace_id.as_ref() == trace_id
            && self.bytes.saturating_add(bytes) <= WRITE_BATCH_BYTES
    }

    fn seal(&mut self) {
        self.sealed = true;
    }
}

#[derive(Clone)]
pub(crate) struct TraceHandle {
    trace_id: Arc<str>,
    state: Arc<TraceState>,
    manager: TraceManager,
}

struct TraceState {
    // Segment snapshots are byte prefixes, so queued records must be sequence-ordered.
    queued_sequence: parking_lot::Mutex<i64>,
    protected: super::redaction::ProtectedSecrets,
    bytes_written: AtomicU64,
    event_count: AtomicU64,
    stopped: AtomicBool,
    finished: AtomicBool,
    reasons: dashmap::DashSet<String>,
}

enum WriterCommand {
    Create {
        trace_id: String,
        state: Arc<TraceState>,
    },
    // The producer may append redacted records until the writer receives and seals the batch.
    RecordBatch {
        batch: Arc<parking_lot::Mutex<QueuedRecordBatch>>,
    },
    Flush {
        trace_id: Arc<str>,
        response: oneshot::Sender<io::Result<()>>,
    },
    Snapshot {
        trace_id: String,
        through_sequence: i64,
        response: oneshot::Sender<io::Result<TraceSnapshot>>,
    },
    Finish {
        trace_id: Arc<str>,
        state: Arc<TraceState>,
        response: oneshot::Sender<io::Result<()>>,
    },
    ClearAll {
        response: oneshot::Sender<io::Result<()>>,
    },
    Shutdown {
        response: oneshot::Sender<()>,
    },
}

impl ManagerInner {
    fn queue_record_batch(
        &self,
        trace_id: &Arc<str>,
        state: &Arc<TraceState>,
        bytes: Vec<u8>,
    ) -> Result<(), ()> {
        let mut producer = self.producer.lock();
        if let Some(batch) = producer.appendable.as_ref() {
            let mut batch = batch.lock();
            if batch.can_append(trace_id, bytes.len()) {
                batch.bytes = batch.bytes.saturating_add(bytes.len());
                batch.records.push(bytes);
                return Ok(());
            }
            batch.seal();
        }
        producer.appendable = None;

        let batch = Arc::new(parking_lot::Mutex::new(QueuedRecordBatch::new(
            Arc::clone(trace_id),
            Arc::clone(state),
            bytes,
        )));
        self.tx
            .try_send(WriterCommand::RecordBatch {
                batch: Arc::clone(&batch),
            })
            .map_err(|_| ())?;
        producer.appendable = Some(batch);
        Ok(())
    }

    fn seal_record_batch(producer: &mut RecordProducer) {
        if let Some(batch) = producer.appendable.take() {
            batch.lock().seal();
        }
    }

    fn try_send_barrier(&self, command: WriterCommand) -> Result<(), ()> {
        let mut producer = self.producer.lock();
        Self::seal_record_batch(&mut producer);
        self.tx.try_send(command).map_err(|_| ())
    }

    async fn send_barrier(&self, command: WriterCommand) -> io::Result<()> {
        // 等待队列许可时不持生产端锁；取得许可后同步封口并发送，
        // 确保没有记录插入末批封口与屏障入队之间。
        let permit = self.tx.reserve().await.map_err(|_| writer_unavailable())?;
        let mut producer = self.producer.lock();
        Self::seal_record_batch(&mut producer);
        permit.send(command);
        Ok(())
    }
}

impl TraceManager {
    pub(crate) fn new(data_dir: PathBuf) -> io::Result<Self> {
        let requested_root = data_dir.join(MANAGED_DIRECTORY);
        fs::create_dir_all(&requested_root)?;
        let root = requested_root.canonicalize()?;
        let retained = managed_size(&root)?;
        let (tx, rx) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let inner = Arc::new(ManagerInner {
            root,
            tx,
            producer: parking_lot::Mutex::new(RecordProducer::default()),
            actual_retained: AtomicU64::new(retained),
            available: true,
        });
        tokio::spawn(writer_loop(Arc::clone(&inner), rx));
        Ok(Self { inner })
    }

    pub(crate) fn degraded() -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        Self {
            inner: Arc::new(ManagerInner {
                root: PathBuf::new(),
                tx,
                producer: parking_lot::Mutex::new(RecordProducer::default()),
                actual_retained: AtomicU64::new(0),
                available: false,
            }),
        }
    }

    pub(crate) fn create(&self) -> TraceHandle {
        let trace_id = stravia_runtime_contract::identifier::new_id();
        let state = Arc::new(TraceState {
            queued_sequence: parking_lot::Mutex::new(0),
            protected: super::redaction::ProtectedSecrets::default(),
            bytes_written: AtomicU64::new(0),
            event_count: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            reasons: dashmap::DashSet::new(),
        });
        let handle = TraceHandle {
            trace_id: Arc::from(trace_id.clone()),
            state: Arc::clone(&state),
            manager: self.clone(),
        };
        if !self.inner.available {
            handle.mark_partial(STORAGE_ERROR, true);
        } else if self
            .inner
            .try_send_barrier(WriterCommand::Create { trace_id, state })
            .is_err()
        {
            handle.mark_partial(WRITER_OVERFLOW, true);
        }
        handle
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        self.inner.actual_retained.load(Ordering::Acquire)
    }

    pub(crate) async fn snapshot(
        &self,
        trace_id: &str,
        through_sequence: i64,
    ) -> io::Result<TraceSnapshot> {
        if !self.inner.available {
            return Err(writer_unavailable());
        }
        validate_trace_id(trace_id)?;
        let (response, receive) = oneshot::channel();
        self.inner
            .send_barrier(WriterCommand::Snapshot {
                trace_id: trace_id.to_owned(),
                through_sequence,
                response,
            })
            .await?;
        receive.await.map_err(|_| writer_unavailable())?
    }

    pub(crate) async fn delete(&self, trace_id: &str) -> io::Result<()> {
        if !self.inner.available {
            return Err(writer_unavailable());
        }
        let directory = self.checked_trace_directory(trace_id)?;
        let root = self.inner.root.clone();
        let removed =
            tokio::task::spawn_blocking(move || remove_managed_directory(&root, &directory))
                .await
                .map_err(|error| {
                    io::Error::other(format!("trace deletion task failed: {error}"))
                })??;
        subtract_saturating(&self.inner.actual_retained, removed);
        Ok(())
    }

    /// Close every active writer and delete all managed trace directories.
    /// Runs inside the writer loop so queued records are ordered deterministically:
    /// earlier records are written then removed, later records fail as unavailable.
    pub(crate) async fn delete_all(&self) -> io::Result<()> {
        if !self.inner.available {
            return Err(writer_unavailable());
        }
        let (response, receive) = oneshot::channel();
        self.inner
            .send_barrier(WriterCommand::ClearAll { response })
            .await?;
        receive.await.map_err(|_| writer_unavailable())?
    }

    pub(crate) async fn shutdown(&self) {
        if !self.inner.available {
            return;
        }
        let (response, receive) = oneshot::channel();
        if self
            .inner
            .send_barrier(WriterCommand::Shutdown { response })
            .await
            .is_ok()
            && let Err(error) = receive.await
        {
            tracing::debug!(%error, "trace writer dropped shutdown acknowledgement");
        }
    }

    pub(crate) async fn reconcile(
        &self,
        retained_trace_ids: HashSet<String>,
        tombstoned_trace_ids: HashSet<String>,
    ) -> io::Result<ReconcileReport> {
        if !self.inner.available {
            return Ok(ReconcileReport::default());
        }
        for id in retained_trace_ids.iter().chain(tombstoned_trace_ids.iter()) {
            validate_trace_id(id)?;
        }
        let root = self.inner.root.clone();
        let result = tokio::task::spawn_blocking(move || {
            reconcile_directories(&root, &retained_trace_ids, &tombstoned_trace_ids)
        })
        .await
        .map_err(|error| {
            io::Error::other(format!("trace reconciliation task failed: {error}"))
        })??;
        let retained = managed_size(&self.inner.root)?;
        self.inner
            .actual_retained
            .store(retained, Ordering::Release);
        Ok(result)
    }

    fn checked_trace_directory(&self, trace_id: &str) -> io::Result<PathBuf> {
        validate_trace_id(trace_id)?;
        Ok(self.inner.root.join(trace_id))
    }
}

impl TraceHandle {
    pub(crate) fn protected_secrets(&self) -> super::redaction::ProtectedSecrets {
        self.state.protected.clone()
    }
    pub(crate) fn record(&self, record: TraceRecord) -> TraceWriteOutcome {
        if self.state.stopped.load(Ordering::Acquire) || self.state.finished.load(Ordering::Acquire)
        {
            return TraceWriteOutcome::Partial(STORAGE_ERROR);
        }
        // Wire capture is deliberately transport-boundary based: queue the observed
        // header/body/message record as-is without JSON, SSE, NDJSON, or Connect parsing.
        self.queue_record(record)
    }

    fn encode_record(
        &self,
        mut record: TraceRecord,
        queued_sequence: &mut i64,
    ) -> Result<Vec<u8>, &'static str> {
        record.sequence = record.sequence.max(*queued_sequence);
        *queued_sequence = record.sequence;
        let mut bytes = serde_json::to_vec(&record).map_err(|_| {
            self.mark_partial(STORAGE_ERROR, true);
            STORAGE_ERROR
        })?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn queue_record(&self, mut record: TraceRecord) -> TraceWriteOutcome {
        record.redact_before_queue();
        if wire_value_size(&record.payload) > WIRE_CAPTURE_LIMIT_BYTES {
            self.mark_partial(WIRE_CAPTURE_LIMIT, false);
            return TraceWriteOutcome::Partial(WIRE_CAPTURE_LIMIT);
        }
        let mut queued_sequence = self.state.queued_sequence.lock();
        let bytes = match self.encode_record(record, &mut queued_sequence) {
            Ok(bytes) => bytes,
            Err(reason) => return TraceWriteOutcome::Partial(reason),
        };
        self.queue_encoded_record(bytes)
    }

    fn queue_encoded_record(&self, bytes: Vec<u8>) -> TraceWriteOutcome {
        if self
            .manager
            .inner
            .queue_record_batch(&self.trace_id, &self.state, bytes)
            .is_err()
        {
            self.mark_partial(WRITER_OVERFLOW, false);
            return TraceWriteOutcome::Partial(WRITER_OVERFLOW);
        }
        TraceWriteOutcome::Queued
    }

    pub(crate) async fn finish(&self) -> TraceManifest {
        if !self.state.finished.swap(true, Ordering::AcqRel) {
            let (response, receive) = oneshot::channel();
            let sent = self
                .manager
                .inner
                .send_barrier(WriterCommand::Finish {
                    trace_id: Arc::clone(&self.trace_id),
                    state: Arc::clone(&self.state),
                    response,
                })
                .await;
            if sent.is_err() || !matches!(receive.await, Ok(Ok(()))) {
                self.mark_partial(STORAGE_ERROR, true);
            }
        }
        self.manifest()
    }

    /// Drain queued records without closing capture or copying trace segments.
    pub(crate) async fn flush(&self) -> io::Result<()> {
        let (response, receive) = oneshot::channel();
        self.manager
            .inner
            .send_barrier(WriterCommand::Flush {
                trace_id: Arc::clone(&self.trace_id),
                response,
            })
            .await?;
        receive.await.map_err(|_| writer_unavailable())?
    }

    pub(crate) async fn snapshot(&self, through_sequence: i64) -> io::Result<TraceSnapshot> {
        self.manager
            .snapshot(&self.trace_id, through_sequence)
            .await
    }

    pub(crate) fn mark_observation_gap(&self) {
        self.mark_partial(STORAGE_ERROR, false);
    }

    pub(crate) fn manifest(&self) -> TraceManifest {
        let mut reasons: Vec<String> = self
            .state
            .reasons
            .iter()
            .map(|reason| reason.key().clone())
            .collect();
        reasons.sort();
        TraceManifest {
            trace_id: self.trace_id.to_string(),
            enabled: true,
            status: if reasons.is_empty() && self.state.finished.load(Ordering::Acquire) {
                "complete"
            } else if reasons.is_empty() {
                "running"
            } else {
                "partial"
            }
            .to_owned(),
            bytes_written: self.state.bytes_written.load(Ordering::Acquire),
            event_count: self.state.event_count.load(Ordering::Acquire),
            reasons,
        }
    }

    pub(super) fn mark_partial(&self, reason: &'static str, stop: bool) {
        if stop {
            self.state.stopped.store(true, Ordering::Release);
        }
        self.state.reasons.insert(reason.to_owned());
    }
}

async fn writer_loop(inner: Arc<ManagerInner>, mut rx: mpsc::Receiver<WriterCommand>) {
    let mut writers = std::collections::HashMap::<String, ActiveWriter>::new();
    while let Some(command) = rx.recv().await {
        match command {
            WriterCommand::Create { trace_id, state } => {
                match create_writer(&inner.root, &trace_id, Arc::clone(&state)).await {
                    Ok(writer) => {
                        writers.insert(trace_id, writer);
                    }
                    Err(_) => {
                        state.stopped.store(true, Ordering::Release);
                        state.reasons.insert(STORAGE_ERROR.to_owned());
                    }
                }
            }
            WriterCommand::RecordBatch { batch } => {
                let (trace_id, state, records) = {
                    let mut batch = batch.lock();
                    batch.seal();
                    (
                        Arc::clone(&batch.trace_id),
                        Arc::clone(&batch.state),
                        std::mem::take(&mut batch.records),
                    )
                };
                let result = if let Some(writer) = writers.get_mut(trace_id.as_ref()) {
                    let mut result = Ok(());
                    for bytes in records {
                        if writer
                            .write_record(&bytes, &inner.actual_retained)
                            .await
                            .is_err()
                        {
                            result = Err(WriteFailure);
                            break;
                        }
                    }
                    result
                } else {
                    Err(WriteFailure)
                };
                if result.is_err() {
                    state.stopped.store(true, Ordering::Release);
                    state.reasons.insert(STORAGE_ERROR.to_owned());
                    writers.remove(trace_id.as_ref());
                }
            }
            WriterCommand::Flush { trace_id, response } => {
                let result = match writers.get_mut(trace_id.as_ref()) {
                    Some(writer) => writer.flush(&inner.actual_retained).await,
                    None => Ok(()),
                };
                if result.is_err() {
                    writers.remove(trace_id.as_ref());
                }
                let _ = response.send(result);
            }
            WriterCommand::Snapshot {
                trace_id,
                through_sequence,
                response,
            } => {
                let flushed = match writers.get_mut(&trace_id) {
                    Some(writer) => match writer.flush(&inner.actual_retained).await {
                        Ok(()) => Ok(Some(SnapshotWatermark {
                            last_segment: writer.segment,
                            last_segment_bytes: writer.segment_bytes,
                        })),
                        Err(error) => Err(error),
                    },
                    None => Ok(None),
                };
                match flushed {
                    Err(error) => {
                        writers.remove(&trace_id);
                        let _ = response.send(Err(error));
                    }
                    Ok(watermark) => {
                        let root = inner.root.clone();
                        tokio::task::spawn_blocking(move || {
                            let result = snapshot_directory(
                                &root,
                                root.join(&trace_id),
                                through_sequence,
                                watermark,
                            );
                            let _ = response.send(result);
                        });
                    }
                }
            }
            WriterCommand::Finish {
                trace_id,
                state,
                response,
            } => {
                let result = if let Some(mut writer) = writers.remove(trace_id.as_ref()) {
                    writer.flush(&inner.actual_retained).await
                } else if state.stopped.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "trace is not active",
                    ))
                };
                if result.is_err() {
                    state.stopped.store(true, Ordering::Release);
                    state.reasons.insert(STORAGE_ERROR.to_owned());
                }
                let _ = response.send(result);
            }
            WriterCommand::ClearAll { response } => {
                for (_, mut writer) in writers.drain() {
                    // 先关闭句柄再删目录，Windows 上打开的文件无法删除。
                    if let Err(error) = writer.shutdown(&inner.actual_retained).await {
                        tracing::debug!(%error, "trace writer close failed during clear");
                    }
                }
                let root = inner.root.clone();
                let result = tokio::task::spawn_blocking(move || clear_managed_directories(&root))
                    .await
                    .map_err(|error| io::Error::other(format!("trace clear task failed: {error}")))
                    .and_then(|result| result);
                if result.is_ok() {
                    inner.actual_retained.store(0, Ordering::Release);
                }
                let _ = response.send(result.map(|_| ()));
            }
            WriterCommand::Shutdown { response } => {
                for writer in writers.values_mut() {
                    if let Err(error) = writer.flush(&inner.actual_retained).await {
                        tracing::debug!(%error, "trace writer flush failed during shutdown");
                    }
                }
                let _ = response.send(());
                break;
            }
        }
    }
}

struct ActiveWriter {
    directory: PathBuf,
    segment: u32,
    segment_bytes: u64,
    file: tokio::fs::File,
    encoder: super::trace_storage::Encoder,
    state: Arc<TraceState>,
    buffered: Vec<u8>,
    record_ends: Vec<usize>,
}

struct WriteFailure;

impl ActiveWriter {
    async fn write_record(
        &mut self,
        bytes: &[u8],
        retained: &AtomicU64,
    ) -> Result<(), WriteFailure> {
        let mut encoded = self
            .encoder
            .encode(bytes, self.segment_bytes)
            .map_err(|_| WriteFailure)?;
        if self.segment_bytes > 0
            && self.segment_bytes.saturating_add(encoded.len() as u64) > SEGMENT_LIMIT_BYTES
        {
            if self.flush(retained).await.is_err() || self.rotate().await.is_err() {
                return Err(WriteFailure);
            }
            encoded = self.encoder.encode(bytes, 0).map_err(|_| WriteFailure)?;
        }
        if !self.buffered.is_empty()
            && self.buffered.len().saturating_add(encoded.len()) > WRITE_BATCH_BYTES
            && self.flush(retained).await.is_err()
        {
            return Err(WriteFailure);
        }
        self.buffered.extend_from_slice(&encoded);
        self.record_ends.push(self.buffered.len());
        self.segment_bytes += encoded.len() as u64;
        if self.buffered.len() >= WRITE_BATCH_BYTES && self.flush(retained).await.is_err() {
            return Err(WriteFailure);
        }
        Ok(())
    }

    async fn rotate(&mut self) -> io::Result<()> {
        self.segment += 1;
        self.segment_bytes = 0;
        self.encoder = super::trace_storage::Encoder::default();
        self.file = open_segment(&self.directory, self.segment).await?;
        Ok(())
    }

    async fn flush(&mut self, retained: &AtomicU64) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let result = async {
            self.file.write_all(&self.buffered).await?;
            self.file.flush().await
        }
        .await;
        match result {
            Ok(()) => self.publish(self.buffered.len(), retained),
            Err(_) => {
                // Tokio File 的 write 可先接收内存数据；只有 flush 成功，
                // 或失败后可确认的文件长度，才是 manifest 可公布的落盘水位。
                let start = self
                    .segment_bytes
                    .saturating_sub(self.buffered.len() as u64);
                match self.file.metadata().await {
                    Ok(metadata) => {
                        let written = metadata
                            .len()
                            .saturating_sub(start)
                            .min(self.buffered.len() as u64)
                            as usize;
                        let complete = self
                            .record_ends
                            .iter()
                            .copied()
                            .take_while(|end| *end <= written)
                            .last()
                            .unwrap_or(0);
                        match self
                            .file
                            .set_len(start.saturating_add(complete as u64))
                            .await
                        {
                            Ok(()) => self.publish(complete, retained),
                            Err(error) => {
                                tracing::debug!(%error, "partial trace record truncation failed")
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "trace size unavailable after write failure");
                        if let Err(error) = self.file.set_len(start).await {
                            tracing::debug!(%error, "uncertain trace write rollback failed");
                        }
                    }
                }
                self.state.stopped.store(true, Ordering::Release);
                self.state.reasons.insert(STORAGE_ERROR.to_owned());
            }
        }
        self.clear_buffer();
        result
    }

    fn publish(&self, written: usize, retained: &AtomicU64) {
        if written == 0 {
            return;
        }
        let events = self.record_ends.partition_point(|end| *end <= written) as u64;
        self.state
            .bytes_written
            .fetch_add(written as u64, Ordering::AcqRel);
        self.state.event_count.fetch_add(events, Ordering::AcqRel);
        retained.fetch_add(written as u64, Ordering::AcqRel);
    }

    fn clear_buffer(&mut self) {
        self.buffered.clear();
        self.record_ends.clear();
        if self.buffered.capacity() > WRITE_BATCH_BYTES {
            self.buffered = Vec::with_capacity(WRITE_BATCH_BYTES);
        }
    }

    async fn shutdown(&mut self, retained: &AtomicU64) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.flush(retained).await?;
        self.file.shutdown().await
    }
}

async fn create_writer(
    root: &Path,
    trace_id: &str,
    state: Arc<TraceState>,
) -> io::Result<ActiveWriter> {
    validate_trace_id(trace_id)?;
    let directory = root.join(trace_id);
    tokio::fs::create_dir(&directory).await?;
    let file = open_segment(&directory, 1).await?;
    Ok(ActiveWriter {
        directory,
        segment: 1,
        segment_bytes: 0,
        file,
        encoder: super::trace_storage::Encoder::default(),
        state,
        buffered: Vec::with_capacity(WRITE_BATCH_BYTES),
        record_ends: Vec::new(),
    })
}

async fn open_segment(directory: &Path, segment: u32) -> io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(directory.join(format!("segment-{segment:06}.jsonl")))
        .await
}

#[derive(Clone, Copy)]
struct SnapshotWatermark {
    last_segment: u32,
    last_segment_bytes: u64,
}

fn snapshot_directory(
    root: &Path,
    directory: PathBuf,
    through_sequence: i64,
    watermark: Option<SnapshotWatermark>,
) -> io::Result<TraceSnapshot> {
    if !directory.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "trace is unavailable",
        ));
    }
    let canonical = directory.canonicalize()?;
    if canonical.parent() != Some(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace path escaped managed root",
        ));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&canonical)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && is_segment_name(&entry.file_name().to_string_lossy()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    let mut segments = Vec::new();
    let mut total = 0u64;
    let mut reached_cutoff = false;
    for path in paths {
        let segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(segment_number)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid trace segment"))?;
        let byte_limit = match watermark {
            Some(watermark) if segment > watermark.last_segment => break,
            Some(watermark) if segment == watermark.last_segment => watermark.last_segment_bytes,
            _ => u64::MAX,
        };
        let file = fs::File::open(&path)?;
        let mut reader = io::BufReader::new(file);
        let mut bytes = 0u64;
        let mut line = Vec::new();
        loop {
            if bytes >= byte_limit {
                break;
            }
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            if bytes.saturating_add(read as u64) > byte_limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "snapshot watermark splits a trace record",
                ));
            }
            let sequence = serde_json::from_slice::<Value>(&line)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "trace record is invalid"))?
                .get("sequence")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "trace sequence is missing")
                })?;
            if sequence > through_sequence {
                reached_cutoff = true;
                break;
            }
            bytes = bytes.saturating_add(read as u64);
        }
        if bytes > 0 {
            total = total.saturating_add(bytes);
            segments.push(TraceSegmentSnapshot { path, bytes });
        }
        if reached_cutoff {
            break;
        }
    }
    Ok(TraceSnapshot {
        segments,
        bytes: total,
    })
}

fn reconcile_directories(
    root: &Path,
    retained: &HashSet<String>,
    tombstoned: &HashSet<String>,
) -> io::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let mut ordered_tombstones: Vec<&String> = tombstoned.iter().collect();
    ordered_tombstones.sort();
    for id in ordered_tombstones {
        remove_managed_directory(root, &root.join(id))?;
        // A missing directory is already a successfully completed tombstone.
        report.removed_tombstones.push((*id).clone());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_trace_id(&id).is_err() {
            continue;
        }
        if !retained.contains(&id) {
            remove_managed_directory(root, &entry.path())?;
            report.removed_orphans.push(id);
        }
    }
    Ok(report)
}

fn remove_managed_directory(root: &Path, directory: &Path) -> io::Result<u64> {
    if !directory.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() {
        fs::remove_file(directory)?;
        return Ok(0);
    }
    if metadata.is_file() {
        let bytes = metadata.len();
        fs::remove_file(directory)?;
        return Ok(bytes);
    }
    let canonical = directory.canonicalize()?;
    if canonical.parent() != Some(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace path escaped managed root",
        ));
    }
    let bytes = directory_size(&canonical)?;
    fs::remove_dir_all(canonical)?;
    Ok(bytes)
}

fn clear_managed_directories(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if validate_trace_id(&entry.file_name().to_string_lossy()).is_ok() {
            remove_managed_directory(root, &entry.path())?;
        }
    }
    Ok(())
}

fn managed_size(root: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && validate_trace_id(&entry.file_name().to_string_lossy()).is_ok()
        {
            total = total.saturating_add(directory_size(&entry.path())?);
        }
    }
    Ok(total)
}

fn directory_size(directory: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && is_segment_name(&entry.file_name().to_string_lossy()) {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn validate_trace_id(trace_id: &str) -> io::Result<()> {
    if stravia_runtime_contract::identifier::valid_id(trace_id) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid trace identity",
        ))
    }
}

fn is_segment_name(name: &str) -> bool {
    name.len() == "segment-000001.jsonl".len()
        && name.starts_with("segment-")
        && name.ends_with(".jsonl")
        && name[8..14].bytes().all(|byte| byte.is_ascii_digit())
}

fn segment_number(name: &str) -> Option<u32> {
    is_segment_name(name)
        .then(|| name[8..14].parse().ok())
        .flatten()
}

pub(crate) fn optimize_trace_directory(root: &Path) -> io::Result<Vec<(String, u64)>> {
    let mut reports = Vec::new();
    if !root.exists() {
        return Ok(reports);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_trace_id(&id).is_err() {
            continue;
        }
        if !entry.file_type()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trace directory is not a directory",
            ));
        }
        let mut bytes = 0;
        for segment in fs::read_dir(entry.path())? {
            let segment = segment?;
            if !is_segment_name(&segment.file_name().to_string_lossy()) {
                continue;
            }
            if !segment.file_type()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "trace segment is not a file",
                ));
            }
            bytes += super::trace_storage::optimize_segment(&segment.path())?;
        }
        reports.push((id, bytes));
    }
    Ok(reports)
}

fn subtract_saturating(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(amount))
    });
}

fn writer_unavailable() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "trace writer unavailable")
}

#[cfg(test)]
#[path = "trace/trace_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wire_snapshot_preserves_sequence_and_payload_boundaries() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let manager = TraceManager::new(root.path().to_owned())?;
        let trace = manager.create();
        for sequence in 1..=3 {
            let mut record = TraceRecord {
                schema_version: 0,
                sequence,
                recorded_at: sequence * 10,
                interaction_id: None,
                run_id: Some("run".to_owned()),
                rejection_id: None,
                model_turn_id: None,
                attempt_id: None,
                layer: "wire".into(),
                direction: Some("upstream_to_platform".into()),
                stage: None,
                transport: Some("http".into()),
                protocol: Some("openai".into()),
                message_type: Some("body_chunk".into()),
                representation: "wire".into(),
                status: None,
                status_code: Some(200),
                url: None,
                headers: Value::Null,
                payload_encoding: "json".into(),
                payload: Value::String(format!("chunk-{sequence}")),
                error: None,
                redactions: Vec::new(),
            };
            trace.record(record.clone());
            record.sequence = sequence;
        }
        trace.flush().await?;
        let snapshot = trace.snapshot(2).await?;
        let mut records = Vec::new();
        for segment in &snapshot.segments {
            super::super::trace_storage::visit(&segment.path, segment.bytes, |record| {
                records.push(record);
                Ok(())
            })?;
        }
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["payload"], "chunk-1");
        assert_eq!(records[1]["payload"], "chunk-2");
        assert_eq!(trace.finish().await.event_count, 3);
        manager.shutdown().await;
        Ok(())
    }

    #[test]
    fn websocket_control_payloads_are_metadata_only() {
        for message_type in ["ping", "pong"] {
            let mut record = TraceRecord {
                schema_version: 0,
                sequence: 1,
                recorded_at: 0,
                interaction_id: None,
                run_id: Some("run".into()),
                rejection_id: None,
                model_turn_id: None,
                attempt_id: None,
                layer: "wire".into(),
                direction: Some("upstream_to_platform".into()),
                stage: None,
                transport: Some("websocket".into()),
                protocol: Some("openai".into()),
                message_type: Some(message_type.into()),
                representation: "wire".into(),
                status: None,
                status_code: Some(101),
                url: None,
                headers: Value::Null,
                payload_encoding: "base64".into(),
                payload: Value::String("control-payload".into()),
                error: None,
                redactions: Vec::new(),
            };
            record.redact_before_queue();
            assert_eq!(record.payload["content_capture"], "omitted");
            assert_eq!(record.payload["reason"], "control_frame_payload_omitted");
            assert!(!record.payload.to_string().contains("control-payload"));
        }
    }
}
