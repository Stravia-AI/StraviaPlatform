use std::borrow::Cow;
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

pub(crate) const TRACE_SCHEMA_VERSION: u32 = 2;
// 重组单条 wire 消息的内存缓冲上限；不是落盘容量配额，超限只影响该条消息。
const WIRE_MESSAGE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const SEGMENT_LIMIT_BYTES: u64 = 8 * 1024 * 1024;
const WRITER_QUEUE_CAPACITY: usize = 1024;
const MANAGED_DIRECTORY: &str = "observation-debug";
const WRITER_OVERFLOW: &str = "writer_overflow";
const STORAGE_ERROR: &str = "storage_error";
const CREDENTIAL_REDACTION_UNSUPPORTED: &str = "credential_redaction_unsupported";
const COMMAND_CODE_PROTOCOL: &str = "command-code/generate/v1";
const INCOMPLETE_STRUCTURED_WIRE_OMITTED: &str = "incomplete_structured_wire_omitted";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TraceRecord {
    // 仅用于内存中的响应重组；进程内资源身份不进入诊断文件。
    #[serde(skip)]
    pub capture_id: Option<u64>,
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
        if self.transport.as_deref() == Some("websocket")
            && matches!(self.message_type.as_deref(), Some("ping" | "pong"))
        {
            // 心跳载荷可以是任意字节，既不是媒体，也不能绕过凭据保护原样落盘。
            self.payload_encoding = "json".into();
            self.payload = serde_json::json!({
                "original_wire_bytes": false,
                "content_capture": "omitted",
                "reason": "control_frame_payload_omitted"
            });
        } else if self.payload_encoding == "base64" || binary_message || wrapped_base64 {
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
                kinds.extend(
                    super::redaction::externalize_capture(&mut decoded_payload, true).into_kinds(),
                );
                kinds.extend(redact_value(&mut decoded_payload).into_kinds());
                let Value::String(redacted_text) = decoded_payload else {
                    return Err(CREDENTIAL_REDACTION_UNSUPPORTED);
                };
                if kinds.contains(&RedactionKind::MediaExternalized)
                    || kinds.contains(&RedactionKind::MediaUnrecoverable)
                {
                    self.payload_encoding = "json".into();
                    self.payload = Value::String(redacted_text);
                } else {
                    self.payload = Value::String(
                        base64::engine::general_purpose::STANDARD.encode(redacted_text.as_bytes()),
                    );
                }
            } else {
                self.payload_encoding = "json".into();
                self.payload = serde_json::json!({
                    "media_externalized": true,
                    "original_wire_bytes": false,
                    "content_capture": "unrecoverable",
                    "reason": "opaque_binary_not_normalized"
                });
                kinds.insert(RedactionKind::MediaUnrecoverable);
            }
        } else {
            if protect {
                protected.value(&mut self.payload);
            }
            if self.direction.is_some()
                || matches!(
                    self.stage.as_deref(),
                    Some(
                        "artifact_normalized_request"
                            | "decoded_request"
                            | "restored_request"
                            | "effective_model_request"
                            | "canonical_terminal_response"
                            | "canonical_content"
                            | "response_after_hook"
                            | "client_projection_content"
                    )
                )
            {
                kinds.extend(
                    super::redaction::externalize_capture(
                        &mut self.payload,
                        self.direction.is_some(),
                    )
                    .into_kinds(),
                );
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
    // Segment snapshots are byte prefixes, so queued records must be sequence-ordered.
    queued_sequence: parking_lot::Mutex<i64>,
    wire_pending: parking_lot::Mutex<std::collections::HashMap<String, (Vec<u8>, TraceRecord)>>,
    ndjson_pending: parking_lot::Mutex<std::collections::HashMap<String, (Vec<u8>, TraceRecord)>>,
    json_http_captures: parking_lot::Mutex<HashSet<u64>>,
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
    Record {
        trace_id: Arc<str>,
        state: Arc<TraceState>,
        bytes: Vec<u8>,
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
                actual_retained: AtomicU64::new(0),
                available: false,
            }),
        }
    }

    pub(crate) fn create(&self) -> TraceHandle {
        let trace_id = stravia_runtime_contract::identifier::new_id();
        let state = Arc::new(TraceState {
            queued_sequence: parking_lot::Mutex::new(0),
            wire_pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
            ndjson_pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
            json_http_captures: parking_lot::Mutex::new(HashSet::new()),
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
            .tx
            .send(WriterCommand::ClearAll { response })
            .await
            .map_err(|_| writer_unavailable())?;
        receive.await.map_err(|_| writer_unavailable())?
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

fn is_command_code_ndjson(record: &TraceRecord) -> bool {
    record.protocol.as_deref() == Some(COMMAND_CODE_PROTOCOL)
        && record.direction.as_deref() == Some("upstream_response")
        && matches!(
            record.message_type.as_deref(),
            Some("body_chunk" | "sse_chunk")
        )
}

fn declares_json_body(headers: &Value) -> bool {
    headers
        .get("content-type")
        .and_then(Value::as_str)
        .is_some_and(|content_type| {
            let media_type = content_type.split(';').next().unwrap_or_default().trim();
            let Some((_, subtype)) = media_type.split_once('/') else {
                return false;
            };
            subtype.eq_ignore_ascii_case("json")
                || subtype
                    .rsplit_once('+')
                    .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("json"))
        })
}

fn captured_payload_bytes(
    payload: &Value,
    base64_encoded: bool,
) -> Result<Cow<'_, [u8]>, &'static str> {
    if !base64_encoded && let Some(text) = payload.as_str() {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }
    let Some(encoded) = payload.as_str().or_else(|| {
        payload
            .as_object()
            .filter(|object| object.get("encoding").and_then(Value::as_str) == Some("base64"))
            .and_then(|object| object.get("data"))
            .and_then(Value::as_str)
    }) else {
        return Err(INCOMPLETE_STRUCTURED_WIRE_OMITTED);
    };
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map(Cow::Owned)
        .map_err(|_| INCOMPLETE_STRUCTURED_WIRE_OMITTED)
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
        if record.protocol.as_deref() == Some(COMMAND_CODE_PROTOCOL)
            && record.direction.as_deref() == Some("upstream_response")
            && record.message_type.as_deref() == Some("response_headers")
            && let Some(capture_id) = record.capture_id
        {
            // 重定向共用响应资源；只有最终响应的格式决定其响应体如何重组。
            let mut json_captures = self.state.json_http_captures.lock();
            if declares_json_body(&record.headers) {
                json_captures.insert(capture_id);
            } else {
                json_captures.remove(&capture_id);
            }
        }
        if is_command_code_ndjson(&record)
            && !record.capture_id.is_some_and(|capture_id| {
                self.state.json_http_captures.lock().contains(&capture_id)
            })
        {
            let payload = std::mem::take(&mut record.payload);
            return match captured_payload_bytes(&payload, record.payload_encoding == "base64") {
                Ok(bytes) => self.record_command_code_ndjson(&record, &bytes),
                Err(reason) => {
                    self.mark_partial(reason, false);
                    TraceWriteOutcome::Partial(reason)
                }
            };
        }
        if matches!(
            record.message_type.as_deref(),
            Some("body_chunk" | "sse_chunk")
        ) {
            let payload = std::mem::take(&mut record.payload);
            let bytes = match captured_payload_bytes(&payload, record.payload_encoding == "base64")
            {
                Ok(bytes) => bytes,
                Err(reason) => {
                    self.mark_partial(reason, false);
                    return TraceWriteOutcome::Partial(reason);
                }
            };
            let key = format!(
                "{}:{}:{}:{}",
                record.capture_id.unwrap_or(0),
                record.direction.as_deref().unwrap_or_default(),
                record.attempt_id.as_deref().unwrap_or_default(),
                record.message_type.as_deref().unwrap_or_default()
            );
            let mut pending = self.state.wire_pending.lock();
            let (buffer, _) = pending
                .entry(key.clone())
                .or_insert_with(|| (Vec::new(), record.clone()));
            if buffer.len().saturating_add(bytes.len()) as u64 > WIRE_MESSAGE_LIMIT_BYTES {
                pending.remove(&key);
                self.mark_partial("structured_wire_capture_limit", false);
                return TraceWriteOutcome::Partial("structured_wire_capture_limit");
            }
            if buffer.is_empty() {
                *buffer = bytes.into_owned();
            } else {
                buffer.extend_from_slice(&bytes);
            }
            let complete = serde_json::from_slice::<serde::de::IgnoredAny>(buffer).is_ok()
                || ((buffer.starts_with(b"data:")
                    || buffer.starts_with(b"event:")
                    || buffer.starts_with(b":"))
                    && (buffer.ends_with(b"\n\n") || buffer.ends_with(b"\r\n\r\n")));
            if !complete {
                return TraceWriteOutcome::Queued;
            }
            let bytes = pending.remove(&key).expect("complete wire message").0;
            let Ok(text) = String::from_utf8(bytes) else {
                self.mark_partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED, false);
                return TraceWriteOutcome::Partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED);
            };
            record.payload = Value::String(text);
            record.payload_encoding.clear();
            record.payload_encoding.push_str("json");
            record.representation = "reassembled_application_message".into();
        }
        self.queue_record(record)
    }

    fn record_command_code_ndjson(&self, record: &TraceRecord, chunk: &[u8]) -> TraceWriteOutcome {
        let key = format!(
            "{}:{}:{}:{}:{}",
            record.capture_id.unwrap_or(0),
            record.protocol.as_deref().unwrap_or_default(),
            record.direction.as_deref().unwrap_or_default(),
            record.attempt_id.as_deref().unwrap_or_default(),
            record.message_type.as_deref().unwrap_or_default()
        );
        let mut chunk_template = record.clone();
        chunk_template.payload_encoding.clear();
        chunk_template.payload_encoding.push_str("json");
        let mut outcome = TraceWriteOutcome::Queued;
        let mut limit_exceeded = false;
        let mut pending = self.state.ndjson_pending.lock();
        let (buffer, template) = pending
            .entry(key.clone())
            .or_insert_with(|| (Vec::new(), chunk_template.clone()));
        for fragment in chunk.split_inclusive(|byte| *byte == b'\n') {
            if buffer.len().saturating_add(fragment.len()) as u64 > WIRE_MESSAGE_LIMIT_BYTES {
                limit_exceeded = true;
                break;
            }
            buffer.extend_from_slice(fragment);
            if fragment.last() != Some(&b'\n') {
                continue;
            }
            let mut line = std::mem::take(buffer);
            while line
                .last()
                .is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
            {
                line.pop();
            }
            let mut framed = template.clone();
            *template = chunk_template.clone();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Ok(text) = String::from_utf8(line) else {
                self.mark_partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED, false);
                outcome = TraceWriteOutcome::Partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED);
                continue;
            };
            if serde_json::from_str::<serde::de::IgnoredAny>(&text).is_err() {
                self.mark_partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED, false);
                outcome = TraceWriteOutcome::Partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED);
                continue;
            }
            framed.payload = Value::String(text);
            framed.representation = "reassembled_application_message".into();
            if let partial @ TraceWriteOutcome::Partial(_) = self.queue_record(framed) {
                outcome = partial;
            }
        }
        if limit_exceeded || buffer.is_empty() {
            pending.remove(&key);
        }
        drop(pending);
        if limit_exceeded {
            self.mark_partial("structured_wire_capture_limit", false);
            if outcome == TraceWriteOutcome::Queued {
                outcome = TraceWriteOutcome::Partial("structured_wire_capture_limit");
            }
        }
        outcome
    }

    fn queue_record(&self, mut record: TraceRecord) -> TraceWriteOutcome {
        if let Err(reason) = record.redact_before_queue(&self.state.protected) {
            self.mark_partial(reason, false);
            return TraceWriteOutcome::Partial(reason);
        }
        let externalized = record.redactions.iter().any(|kind| {
            matches!(
                kind,
                RedactionKind::MediaExternalized | RedactionKind::MediaUnrecoverable
            )
        });
        if externalized {
            record.representation = "artifact_externalized".into();
            if record
                .redactions
                .contains(&RedactionKind::MediaUnrecoverable)
            {
                self.mark_partial("media_unrecoverable", false);
            }
        }
        let mut queued_sequence = self.state.queued_sequence.lock();
        record.sequence = record.sequence.max(*queued_sequence);
        *queued_sequence = record.sequence;
        let mut bytes = match serde_json::to_vec(&record) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.mark_partial(STORAGE_ERROR, true);
                return TraceWriteOutcome::Partial(STORAGE_ERROR);
            }
        };
        bytes.push(b'\n');

        let command = WriterCommand::Record {
            trace_id: Arc::clone(&self.trace_id),
            state: Arc::clone(&self.state),
            bytes,
        };
        if self.manager.inner.tx.try_send(command).is_err() {
            self.mark_partial(WRITER_OVERFLOW, false);
            return TraceWriteOutcome::Partial(WRITER_OVERFLOW);
        }
        TraceWriteOutcome::Queued
    }

    pub(crate) async fn finish(&self) -> TraceManifest {
        if !self.state.finished.swap(true, Ordering::AcqRel) {
            let ndjson_pending = std::mem::take(&mut *self.state.ndjson_pending.lock());
            for (_, (bytes, mut record)) in ndjson_pending {
                if bytes.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let complete = serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_ok();
                match String::from_utf8(bytes) {
                    Ok(text) if complete => {
                        record.payload = Value::String(text);
                        record.representation = "reassembled_application_message".into();
                        let _ = self.queue_record(record);
                    }
                    _ => self.mark_partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED, false),
                }
            }
            let pending = std::mem::take(&mut *self.state.wire_pending.lock());
            for (_, (bytes, mut record)) in pending {
                match String::from_utf8(bytes) {
                    Ok(text) => {
                        let trimmed = text.trim_start();
                        if trimmed.starts_with(['{', '[', '"', ':'])
                            || trimmed.starts_with("data:")
                            || trimmed.starts_with("event:")
                        {
                            self.mark_partial(INCOMPLETE_STRUCTURED_WIRE_OMITTED, false);
                            continue;
                        }
                        // Plain-text HTTP errors are complete at EOF, not at a JSON/SSE boundary.
                        // Delay their redaction until now so split upload grants remain secret.
                        record.payload = Value::String(text);
                        record.payload_encoding.clear();
                        record.payload_encoding.push_str("json");
                    }
                    Err(error) => {
                        record.payload = Value::String(
                            base64::engine::general_purpose::STANDARD.encode(error.into_bytes()),
                        );
                        record.payload_encoding.clear();
                        record.payload_encoding.push_str("base64");
                    }
                }
                record.representation = "reassembled_application_message".into();
                let _ = self.queue_record(record);
            }
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

    /// Drain queued records without closing capture or copying trace segments.
    pub(crate) async fn flush(&self) -> io::Result<()> {
        let (response, receive) = oneshot::channel();
        self.manager
            .inner
            .tx
            .send(WriterCommand::Flush {
                trace_id: Arc::clone(&self.trace_id),
                response,
            })
            .await
            .map_err(|_| writer_unavailable())?;
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
                        state.stopped.store(true, Ordering::Release);
                        state.reasons.insert(STORAGE_ERROR.to_owned());
                        writers.remove(trace_id.as_ref());
                    }
                }
            }
            WriterCommand::Flush { trace_id, response } => {
                let result = match writers.get_mut(trace_id.as_ref()) {
                    Some(writer) => writer.flush().await,
                    None => Ok(()),
                };
                let _ = response.send(result);
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
            WriterCommand::ClearAll { response } => {
                for (_, mut writer) in writers.drain() {
                    // 先关闭句柄再删目录，Windows 上打开的文件无法删除。
                    if let Err(error) = writer.shutdown().await {
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
                    if let Err(error) = writer.flush().await {
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
}

struct WriteFailure {
    written: u64,
}

impl ActiveWriter {
    async fn write_record(&mut self, bytes: &[u8]) -> Result<u64, WriteFailure> {
        let mut encoded = self
            .encoder
            .encode(bytes, self.segment_bytes)
            .map_err(|_| WriteFailure { written: 0 })?;
        if self.segment_bytes > 0
            && self.segment_bytes.saturating_add(encoded.len() as u64) > SEGMENT_LIMIT_BYTES
        {
            if self.flush().await.is_err() || self.rotate().await.is_err() {
                return Err(WriteFailure { written: 0 });
            }
            encoded = self
                .encoder
                .encode(bytes, 0)
                .map_err(|_| WriteFailure { written: 0 })?;
        }
        let bytes = encoded.as_slice();
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
        self.encoder = super::trace_storage::Encoder::default();
        self.file = open_segment(&self.directory, self.segment).await?;
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.file.shutdown().await
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
        encoder: super::trace_storage::Encoder::default(),
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
    async fn structural_trace_preserves_content_scope_and_snapshot_cutoffs() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let manager = TraceManager::new(root.path().to_owned())?;
        let trace = manager.create();
        let payload = serde_json::json!({"text":"原文与空白\n ".repeat(10_000)});
        for sequence in 1..=3 {
            let mut record = binary_record(String::new());
            record.sequence = sequence;
            record.recorded_at = sequence * 10;
            record.payload = payload.clone();
            record.payload_encoding = "json".into();
            record.direction = None;
            record.transport = None;
            record.message_type = None;
            record.stage = Some("canonical_content".into());
            record.layer = "content".into();
            trace.record(record);
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
        assert_eq!(records[0]["sequence"], 1);
        assert_eq!(records[1]["sequence"], 2);
        assert_eq!(records[1]["recorded_at"], 20);
        assert_eq!(records[0]["payload"], payload);
        assert_eq!(records[1]["payload"], payload);
        let manifest = trace.finish().await;
        assert_eq!(manifest.event_count, 3);
        assert!(manifest.bytes_written < serde_json::to_vec(&payload)?.len() as u64 * 2);
        Ok(())
    }

    fn binary_record(payload: String) -> TraceRecord {
        TraceRecord {
            capture_id: None,
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
    fn websocket_control_payloads_are_omitted_without_media_gaps() {
        for message_type in ["ping", "pong"] {
            for bytes in [b"\xff\x00\x81".as_slice(), b"api_key=never-persist-this"] {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                let mut record = binary_record(encoded.clone());
                record.message_type = Some(message_type.to_owned());
                record.payload_encoding = "json".to_owned();
                record.payload = serde_json::json!({"encoding": "base64", "data": encoded});
                record
                    .redact_before_queue(&super::super::redaction::ProtectedSecrets::default())
                    .expect("control frame capture");
                assert_eq!(record.payload["content_capture"], "omitted");
                assert_eq!(record.payload["reason"], "control_frame_payload_omitted");
                assert!(!record.payload.to_string().contains("never-persist-this"));
                assert!(!record.payload.to_string().contains(&encoded));
                assert!(
                    !record
                        .redactions
                        .contains(&RedactionKind::MediaUnrecoverable)
                );
            }
        }
    }

    #[test]
    fn opaque_binary_is_omitted_while_utf8_structured_bytes_are_redacted() {
        let opaque = base64::engine::general_purpose::STANDARD.encode([0xff, 0x00, 0x81]);
        let mut opaque_record = binary_record(opaque.clone());
        assert!(
            opaque_record
                .redact_before_queue(&super::super::redaction::ProtectedSecrets::default())
                .is_ok()
        );
        assert!(!opaque_record.payload.to_string().contains(&opaque));
        assert_eq!(opaque_record.payload["content_capture"], "unrecoverable");

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
}
