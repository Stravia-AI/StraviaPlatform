use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::redaction::{RedactionKind, redact_error, redact_headers, redact_url, redact_value};
use super::types::TraceManifest;

pub(crate) const TRACE_SCHEMA_VERSION: u32 = 1;
pub(crate) const RUN_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const TOTAL_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SEGMENT_LIMIT_BYTES: u64 = 8 * 1024 * 1024;
const WRITER_QUEUE_CAPACITY: usize = 1024;
const MANAGED_DIRECTORY: &str = "observation-debug";
const RUN_SIZE_LIMIT: &str = "run_size_limit";
const GLOBAL_SIZE_LIMIT: &str = "global_size_limit";
const WRITER_OVERFLOW: &str = "writer_overflow";
const STORAGE_ERROR: &str = "storage_error";
const CREDENTIAL_REDACTION_UNSUPPORTED: &str = "credential_redaction_unsupported";

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
    fn redact_before_queue(
        &mut self,
        protected: &super::redaction::ProtectedSecrets,
    ) -> Result<(), &'static str> {
        self.schema_version = TRACE_SCHEMA_VERSION;
        let mut kinds = BTreeSet::new();
        let protect = self.direction.as_deref() != Some("client_to_platform");
        if protect {
            protected.value(&mut self.headers);
        }
        kinds.extend(redact_headers(&mut self.headers).into_kinds());
        let wrapped_base64 = self
            .payload
            .get("encoding")
            .and_then(Value::as_str)
            .is_some_and(|encoding| encoding.eq_ignore_ascii_case("base64"))
            && self.payload.get("data").is_some_and(Value::is_string);
        if wrapped_base64 {
            let Some(data) = self
                .payload
                .as_object_mut()
                .and_then(|object| object.remove("data"))
            else {
                return Err(CREDENTIAL_REDACTION_UNSUPPORTED);
            };
            self.payload = data;
        }
        let binary_message = self
            .message_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("binary"));
        if self.payload_encoding == "base64" || binary_message || wrapped_base64 {
            self.payload_encoding = "base64".to_owned();
            let Some(encoded) = self.payload.as_str() else {
                return Err(CREDENTIAL_REDACTION_UNSUPPORTED);
            };
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| CREDENTIAL_REDACTION_UNSUPPORTED)?;
            if let Ok(text) = String::from_utf8(decoded) {
                let mut decoded_payload = Value::String(text);
                if protect {
                    protected.value(&mut decoded_payload);
                }
                kinds.extend(redact_value(&mut decoded_payload).into_kinds());
                let Value::String(redacted_text) = decoded_payload else {
                    return Err(CREDENTIAL_REDACTION_UNSUPPORTED);
                };
                self.payload = Value::String(
                    base64::engine::general_purpose::STANDARD.encode(redacted_text.as_bytes()),
                );
            }
        } else {
            if protect {
                protected.value(&mut self.payload);
            }
            kinds.extend(redact_value(&mut self.payload).into_kinds());
        }
        if let Some(url) = &mut self.url {
            if protect {
                protected.text(url);
            }
            let (redacted, report) = redact_url(url);
            *url = redacted;
            kinds.extend(report.into_kinds());
        }
        if let Some(error) = &mut self.error {
            protected.text(error);
            let (redacted, report) = redact_error(error);
            *error = redacted;
            kinds.extend(report.into_kinds());
        }
        self.redactions = kinds.into_iter().collect();
        Ok(())
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
    retained_and_reserved: AtomicU64,
    actual_retained: AtomicU64,
    available: bool,
}

#[derive(Clone)]
pub(crate) struct TraceHandle {
    trace_id: Arc<str>,
    state: Arc<TraceState>,
    manager: TraceManager,
}

struct TraceState {
    protected: super::redaction::ProtectedSecrets,
    bytes_written: AtomicU64,
    retained_and_reserved: AtomicU64,
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
    Record {
        trace_id: Arc<str>,
        state: Arc<TraceState>,
        bytes: Vec<u8>,
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
    Shutdown {
        response: oneshot::Sender<()>,
    },
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
            retained_and_reserved: AtomicU64::new(retained),
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
                retained_and_reserved: AtomicU64::new(0),
                actual_retained: AtomicU64::new(0),
                available: false,
            }),
        }
    }

    pub(crate) fn create(&self) -> TraceHandle {
        let trace_id = uuid::Uuid::new_v4().simple().to_string();
        let state = Arc::new(TraceState {
            protected: super::redaction::ProtectedSecrets::default(),
            bytes_written: AtomicU64::new(0),
            retained_and_reserved: AtomicU64::new(0),
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
            .tx
            .try_send(WriterCommand::Create { trace_id, state })
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
            .tx
            .send(WriterCommand::Snapshot {
                trace_id: trace_id.to_owned(),
                through_sequence,
                response,
            })
            .await
            .map_err(|_| writer_unavailable())?;
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
        subtract_saturating(&self.inner.retained_and_reserved, removed);
        subtract_saturating(&self.inner.actual_retained, removed);
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        if !self.inner.available {
            return;
        }
        let (response, receive) = oneshot::channel();
        if self
            .inner
            .tx
            .send(WriterCommand::Shutdown { response })
            .await
            .is_ok()
        {
            let _ = receive.await;
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
            .retained_and_reserved
            .store(retained, Ordering::Release);
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
    pub(crate) fn record(&self, mut record: TraceRecord) -> TraceWriteOutcome {
        if self.state.stopped.load(Ordering::Acquire) || self.state.finished.load(Ordering::Acquire)
        {
            return TraceWriteOutcome::Partial(STORAGE_ERROR);
        }
        if let Err(reason) = record.redact_before_queue(&self.state.protected) {
            self.mark_partial(reason, false);
            return TraceWriteOutcome::Partial(reason);
        }
        let mut bytes = match serde_json::to_vec(&record) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.mark_partial(STORAGE_ERROR, true);
                return TraceWriteOutcome::Partial(STORAGE_ERROR);
            }
        };
        bytes.push(b'\n');
        let byte_count = bytes.len() as u64;

        if !reserve(
            &self.state.retained_and_reserved,
            byte_count,
            RUN_LIMIT_BYTES,
        ) {
            self.mark_partial(RUN_SIZE_LIMIT, true);
            return TraceWriteOutcome::Partial(RUN_SIZE_LIMIT);
        }
        if !reserve(
            &self.manager.inner.retained_and_reserved,
            byte_count,
            TOTAL_LIMIT_BYTES,
        ) {
            subtract_saturating(&self.state.retained_and_reserved, byte_count);
            self.mark_partial(GLOBAL_SIZE_LIMIT, true);
            return TraceWriteOutcome::Partial(GLOBAL_SIZE_LIMIT);
        }

        let command = WriterCommand::Record {
            trace_id: Arc::clone(&self.trace_id),
            state: Arc::clone(&self.state),
            bytes,
        };
        if self.manager.inner.tx.try_send(command).is_err() {
            subtract_saturating(&self.state.retained_and_reserved, byte_count);
            subtract_saturating(&self.manager.inner.retained_and_reserved, byte_count);
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
                .tx
                .send(WriterCommand::Finish {
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
                match create_writer(&inner.root, &trace_id).await {
                    Ok(writer) => {
                        writers.insert(trace_id, writer);
                    }
                    Err(_) => {
                        state.stopped.store(true, Ordering::Release);
                        state.reasons.insert(STORAGE_ERROR.to_owned());
                    }
                }
            }
            WriterCommand::Record {
                trace_id,
                state,
                bytes,
            } => {
                let reserved = bytes.len() as u64;
                let result = match writers.get_mut(trace_id.as_ref()) {
                    Some(writer) => writer.write_record(&bytes).await,
                    None => Err(WriteFailure { written: 0 }),
                };
                match result {
                    Ok(written) => {
                        state.bytes_written.fetch_add(written, Ordering::AcqRel);
                        inner.actual_retained.fetch_add(written, Ordering::AcqRel);
                        state.event_count.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(failure) => {
                        let written = failure.written;
                        state.bytes_written.fetch_add(written, Ordering::AcqRel);
                        inner.actual_retained.fetch_add(written, Ordering::AcqRel);
                        subtract_saturating(&state.retained_and_reserved, reserved - written);
                        subtract_saturating(&inner.retained_and_reserved, reserved - written);
                        state.stopped.store(true, Ordering::Release);
                        state.reasons.insert(STORAGE_ERROR.to_owned());
                        writers.remove(trace_id.as_ref());
                    }
                }
            }
            WriterCommand::Snapshot {
                trace_id,
                through_sequence,
                response,
            } => {
                let flush = match writers.get_mut(&trace_id) {
                    Some(writer) => writer.flush().await,
                    None => Ok(()),
                };
                if let Err(error) = flush {
                    let _ = response.send(Err(error));
                } else {
                    let root = inner.root.clone();
                    tokio::task::spawn_blocking(move || {
                        let result =
                            snapshot_directory(&root, root.join(&trace_id), through_sequence);
                        let _ = response.send(result);
                    });
                }
            }
            WriterCommand::Finish {
                trace_id,
                state,
                response,
            } => {
                let result = if let Some(mut writer) = writers.remove(trace_id.as_ref()) {
                    writer.flush().await
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
            WriterCommand::Shutdown { response } => {
                for writer in writers.values_mut() {
                    let _ = writer.flush().await;
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
}

struct WriteFailure {
    written: u64,
}

impl ActiveWriter {
    async fn write_record(&mut self, bytes: &[u8]) -> Result<u64, WriteFailure> {
        if self.segment_bytes > 0
            && self.segment_bytes.saturating_add(bytes.len() as u64) > SEGMENT_LIMIT_BYTES
        {
            if self.flush().await.is_err() || self.rotate().await.is_err() {
                return Err(WriteFailure { written: 0 });
            }
        }
        use tokio::io::AsyncWriteExt;
        let mut written = 0usize;
        while written < bytes.len() {
            match self.file.write(&bytes[written..]).await {
                Ok(0) | Err(_) => {
                    return Err(WriteFailure {
                        written: written as u64,
                    });
                }
                Ok(count) => written += count,
            }
        }
        self.segment_bytes += written as u64;
        Ok(written as u64)
    }

    async fn rotate(&mut self) -> io::Result<()> {
        self.segment += 1;
        self.segment_bytes = 0;
        self.file = open_segment(&self.directory, self.segment).await?;
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await
    }
}

async fn create_writer(root: &Path, trace_id: &str) -> io::Result<ActiveWriter> {
    validate_trace_id(trace_id)?;
    let directory = root.join(trace_id);
    tokio::fs::create_dir(&directory).await?;
    let file = open_segment(&directory, 1).await?;
    Ok(ActiveWriter {
        directory,
        segment: 1,
        segment_bytes: 0,
        file,
    })
}

async fn open_segment(directory: &Path, segment: u32) -> io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(directory.join(format!("segment-{segment:06}.jsonl")))
        .await
}

fn snapshot_directory(
    root: &Path,
    directory: PathBuf,
    through_sequence: i64,
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
        let file = fs::File::open(&path)?;
        let mut reader = io::BufReader::new(file);
        let mut bytes = 0u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
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
    if trace_id.len() == 32
        && trace_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !trace_id.contains(['/', '\\'])
    {
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

fn reserve(counter: &AtomicU64, amount: u64, limit: u64) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount).filter(|next| *next <= limit) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
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
mod tests {
    use super::*;

    fn binary_record(payload: String) -> TraceRecord {
        TraceRecord {
            schema_version: 0,
            sequence: 1,
            recorded_at: 0,
            interaction_id: None,
            run_id: Some("run".to_owned()),
            rejection_id: None,
            model_turn_id: None,
            attempt_id: None,
            layer: "wire".to_owned(),
            direction: Some("upstream_to_platform".to_owned()),
            stage: None,
            transport: Some("websocket".to_owned()),
            protocol: Some("openai_responses".to_owned()),
            message_type: Some("binary".to_owned()),
            representation: "wire".to_owned(),
            status: None,
            status_code: None,
            url: None,
            headers: Value::Null,
            payload_encoding: "base64".to_owned(),
            payload: Value::String(payload),
            error: None,
            redactions: Vec::new(),
        }
    }

    #[test]
    fn opaque_binary_is_preserved_while_utf8_structured_bytes_are_redacted() {
        let opaque = base64::engine::general_purpose::STANDARD.encode([0xff, 0x00, 0x81]);
        let mut opaque_record = binary_record(opaque.clone());
        assert!(
            opaque_record
                .redact_before_queue(&super::super::redaction::ProtectedSecrets::default())
                .is_ok()
        );
        assert_eq!(opaque_record.payload, Value::String(opaque));

        let sentinel = "never-persist-this";
        let encoded_json = base64::engine::general_purpose::STANDARD
            .encode(format!(r#"{{"api_key":"{sentinel}","content":"keep"}}"#));
        let mut structured_record = binary_record(encoded_json);
        assert!(
            structured_record
                .redact_before_queue(&super::super::redaction::ProtectedSecrets::default())
                .is_ok()
        );
        let redacted = structured_record.payload.as_str().expect("base64 payload");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(redacted)
            .expect("valid base64");
        let text = String::from_utf8(decoded).expect("UTF-8 JSON");
        assert!(!text.contains(sentinel));
        assert!(text.contains("keep"));
    }

    #[test]
    fn capacity_reservation_accepts_exact_limits_and_rejects_one_more_byte() {
        let run = AtomicU64::new(RUN_LIMIT_BYTES - 1);
        assert!(reserve(&run, 1, RUN_LIMIT_BYTES));
        assert_eq!(run.load(Ordering::Relaxed), RUN_LIMIT_BYTES);
        assert!(!reserve(&run, 1, RUN_LIMIT_BYTES));

        let retained = AtomicU64::new(TOTAL_LIMIT_BYTES - 1);
        assert!(reserve(&retained, 1, TOTAL_LIMIT_BYTES));
        assert_eq!(retained.load(Ordering::Relaxed), TOTAL_LIMIT_BYTES);
        assert!(!reserve(&retained, 1, TOTAL_LIMIT_BYTES));
    }
}
