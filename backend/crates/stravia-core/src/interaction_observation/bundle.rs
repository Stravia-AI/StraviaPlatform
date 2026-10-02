use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::redaction::redact_value;
use super::store::ObservationStore;
use super::trace::{TraceHandle, TraceManager, TraceSnapshot};
use super::types::{
    BundleRequest, BundleResourceKind, BundleStream, ConfirmedUsage, DownloadTicket,
    ObservationEvent, TraceManifest,
};
use super::writer::WriterCommand;

const BUNDLE_SCHEMA_VERSION: u32 = 1;
const TICKET_TTL: Duration = Duration::from_secs(60);
const STREAM_CHANNEL_CAPACITY: usize = 8;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const TICKET_UNAVAILABLE: &str = "download ticket unavailable";

struct BundleSnapshot {
    kind: BundleResourceKind,
    resource_id: String,
    exported_at: i64,
    through_sequence: i64,
    resource_status: String,
    summary: Value,
    runs: Vec<BundleRunSnapshot>,
}

struct BundleRunSnapshot {
    run_id: String,
    debug_enabled: bool,
    trace_status: String,
    bytes_written: u64,
    reasons: Vec<String>,
    trace: Option<TraceSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TicketUnavailable;

impl std::fmt::Display for TicketUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(TICKET_UNAVAILABLE)
    }
}

impl std::error::Error for TicketUnavailable {}

#[derive(Clone)]
pub(super) struct BundleExport {
    store: ObservationStore,
    writer: mpsc::Sender<WriterCommand>,
    traces: TraceManager,
    active_traces: Arc<Mutex<HashMap<String, TraceHandle>>>,
    tickets: Arc<Mutex<HashMap<String, TicketEntry>>>,
}

struct TicketEntry {
    expires: Instant,
    snapshot: BundleSnapshot,
}

impl BundleExport {
    pub(super) fn new(
        store: ObservationStore,
        writer: mpsc::Sender<WriterCommand>,
        traces: TraceManager,
        active_traces: Arc<Mutex<HashMap<String, TraceHandle>>>,
    ) -> Self {
        Self {
            store,
            writer,
            traces,
            active_traces,
            tickets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) async fn issue(&self, request: BundleRequest) -> anyhow::Result<DownloadTicket> {
        if matches!(request.kind, BundleResourceKind::Interaction) {
            let (done, receive) = oneshot::channel();
            self.writer
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
        // 固定水位在 writer 屏障之后；票据保存物理前缀，下载不再读取当前状态。
        let max = self.store.max_sequence().await?;
        let through = request.through_sequence.unwrap_or(max).min(max);
        let exported_at = chrono::Utc::now().timestamp_millis();
        let mut snapshot = match request.kind {
            BundleResourceKind::Interaction => {
                let records = self
                    .store
                    .bundle_interaction_records(&request.resource_id, through)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("interaction not found"))?;
                if !records
                    .events
                    .iter()
                    .any(|event| event.kind == "run_admitted")
                {
                    anyhow::bail!("bundle snapshot unavailable");
                }
                let projection = BundleProjection::replay(&records.events);
                let mut runs = Vec::with_capacity(records.runs.len());
                for run in records.runs {
                    runs.push(
                        self.capture(run.id, run.debug_enabled, run.trace, through, true)
                            .await,
                    );
                }
                let summary = projection.summary(
                    &request.resource_id,
                    &records.root_id,
                    through,
                    records.events,
                );
                BundleSnapshot {
                    kind: BundleResourceKind::Interaction,
                    resource_id: request.resource_id,
                    exported_at,
                    through_sequence: through,
                    resource_status: projection.status,
                    summary,
                    runs,
                }
            }
            BundleResourceKind::RejectedRequest => {
                let records = self
                    .store
                    .bundle_rejection_records(&request.resource_id, through)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("rejected request not found"))?;
                if records.events.is_empty() {
                    anyhow::bail!("bundle snapshot unavailable");
                }
                let capture = self
                    .capture(
                        records.id,
                        records.debug_enabled,
                        records.trace,
                        through,
                        false,
                    )
                    .await;
                let summary = serde_json::json!({
                    "schema_version": 1,
                    "rejection_id": request.resource_id,
                    "through_event_sequence": through,
                    "events": records.events,
                });
                BundleSnapshot {
                    kind: BundleResourceKind::RejectedRequest,
                    resource_id: request.resource_id,
                    exported_at,
                    through_sequence: through,
                    resource_status: "rejected".into(),
                    summary,
                    runs: vec![capture],
                }
            }
        };
        self.describe_legacy_media(&mut snapshot).await?;
        self.remove_expired();
        redact_value(&mut snapshot.summary);
        let expires_at = snapshot
            .exported_at
            .saturating_add(TICKET_TTL.as_millis() as i64);
        let through_sequence = snapshot.through_sequence;
        let mut tickets = self.tickets.lock();
        let ticket = loop {
            let mut token_bytes = [0u8; 32];
            rand::fill(&mut token_bytes);
            let candidate = hex(&token_bytes);
            if !tickets.contains_key(&candidate) {
                break candidate;
            }
        };
        tickets.insert(
            ticket.clone(),
            TicketEntry {
                expires: Instant::now() + TICKET_TTL,
                snapshot,
            },
        );
        drop(tickets);
        Ok(DownloadTicket {
            download_url: format!("/api/v1/observations/debug-bundles/{ticket}"),
            expires_at,
            through_sequence,
        })
    }

    pub(super) fn consume(&self, ticket: &str) -> Result<BundleStream, TicketUnavailable> {
        if ticket.len() != 64 || !ticket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(TicketUnavailable);
        }
        let entry = self
            .tickets
            .lock()
            .remove(ticket)
            .filter(|entry| Instant::now() < entry.expires)
            .ok_or(TicketUnavailable)?;
        Ok(stream_bundle(entry.snapshot))
    }

    async fn capture(
        &self,
        run_id: String,
        debug_enabled: bool,
        manifest: Option<TraceManifest>,
        through: i64,
        active_run: bool,
    ) -> BundleRunSnapshot {
        let active = if active_run {
            self.active_traces.lock().get(&run_id).cloned()
        } else {
            None
        };
        let (manifest, trace) = if let Some(handle) = active {
            let manifest = handle.manifest();
            let trace = handle.snapshot(through).await;
            (Some(manifest), Some(trace))
        } else if let Some(manifest) = manifest {
            let trace = self.traces.snapshot(&manifest.trace_id, through).await;
            (Some(manifest), Some(trace))
        } else {
            (None, None)
        };
        let trace = trace.and_then(|result| match result {
            Ok(snapshot) => Some(snapshot),
            Err(_) => {
                tracing::debug!("bundle trace snapshot unavailable");
                None
            }
        });
        let (trace_status, reasons) = manifest.map_or_else(
            || {
                (
                    "none".into(),
                    if !active_run && debug_enabled {
                        vec!["trace_missing".into()]
                    } else {
                        Vec::new()
                    },
                )
            },
            |manifest| (manifest.status, manifest.reasons),
        );
        BundleRunSnapshot {
            run_id,
            debug_enabled,
            trace_status,
            bytes_written: trace.as_ref().map_or(0, |snapshot| snapshot.bytes),
            reasons,
            trace,
        }
    }

    async fn describe_legacy_media(&self, snapshot: &mut BundleSnapshot) -> anyhow::Result<()> {
        let traces: Vec<_> = snapshot
            .runs
            .iter()
            .filter_map(|run| run.trace.clone())
            .collect();
        if traces.is_empty() {
            return Ok(());
        }
        let references = tokio::task::spawn_blocking(move || {
            let mut references = BTreeSet::new();
            for trace in traces {
                for segment in trace.segments {
                    super::trace_storage::visit(&segment.path, segment.bytes, |record| {
                        collect_captured_artifacts(&record, &mut references);
                        Ok(())
                    })?;
                }
            }
            Ok::<_, io::Error>(references)
        })
        .await??;
        if !references.is_empty() {
            let mut media = Vec::with_capacity(references.len());
            for reference in references {
                let id = stravia_runtime_contract::artifact::ArtifactId::from_reference(&reference)
                    .map_err(anyhow::Error::new)?;
                let available = self
                    .store
                    .artifact_available(id.as_str(), snapshot.exported_at)
                    .await?;
                media.push(serde_json::json!({
                    "artifact_reference": reference,
                    "media_externalized": true,
                    "content_capture": if available { "reference_only" } else { "unrecoverable" },
                    "reason": if available { "media_not_embedded_in_bundle" } else { "artifact_expired_or_unavailable" },
                    "checked_at": snapshot.exported_at,
                }));
            }
            snapshot.summary["externalized_media"] = Value::Array(media);
        }
        Ok(())
    }

    fn remove_expired(&self) {
        let now = Instant::now();
        self.tickets.lock().retain(|_, entry| entry.expires > now);
    }
}

struct BundleProjection {
    status: String,
    usage: ConfirmedUsage,
    visible_tail: String,
    observation_gap: bool,
}

impl BundleProjection {
    fn replay(events: &[ObservationEvent]) -> Self {
        let mut runs = HashMap::new();
        let mut parents = HashSet::new();
        let mut active = HashSet::new();
        let mut attempts = HashMap::<&str, Option<ConfirmedUsage>>::new();
        let mut visible_tail = String::new();
        let mut observation_gap = false;
        let resolved =
            super::grouping::resolved_client_tool_runs(events.iter().filter_map(|event| {
                match event.kind.as_str() {
                    "target_attempt_started" => {
                        if let Some(id) = event.payload["attempt_id"].as_str() {
                            attempts.entry(id).or_default();
                        }
                    }
                    "target_attempt_finished" => {
                        if let Some(id) = event.payload["attempt_id"].as_str()
                            && let Some(usage) =
                                event.payload.get("usage").filter(|value| !value.is_null())
                        {
                            let recorded = attempts.entry(id).or_default();
                            match ConfirmedUsage::deserialize(usage) {
                                Ok(usage) => {
                                    // 修订只更新新报告的字段；未知不能撤销已确认值，也不累加快照。
                                    let previous =
                                        recorded.get_or_insert_with(ConfirmedUsage::default);
                                    previous.input_tokens =
                                        usage.input_tokens.or(previous.input_tokens);
                                    previous.output_tokens =
                                        usage.output_tokens.or(previous.output_tokens);
                                    previous.cache_read_tokens =
                                        usage.cache_read_tokens.or(previous.cache_read_tokens);
                                    previous.cache_write_tokens =
                                        usage.cache_write_tokens.or(previous.cache_write_tokens);
                                    previous.reasoning_tokens =
                                        usage.reasoning_tokens.or(previous.reasoning_tokens);
                                }
                                Err(_) => {
                                    *recorded = Some(ConfirmedUsage::default());
                                    observation_gap = true;
                                }
                            }
                        }
                    }
                    "client_visible_content" => {
                        if let Some(text) = event.payload["text"].as_str() {
                            visible_tail.push_str(text);
                            if let Some((offset, _)) = visible_tail.char_indices().rev().nth(4095) {
                                visible_tail.drain(..offset);
                            }
                        }
                    }
                    "model_turn_started"
                        if !visible_tail.is_empty()
                            && !visible_tail.ends_with(super::store::TURN_SEPARATOR) =>
                    {
                        visible_tail.push_str(super::store::TURN_SEPARATOR);
                    }
                    "observation_gap" => observation_gap = true,
                    _ => {}
                }
                let Some(run) = event.run_id.as_deref() else {
                    return None;
                };
                match event.kind.as_str() {
                    "run_admitted" => {
                        runs.insert(run, "running");
                        if let Some(parent) = event.payload["parent_run_id"].as_str() {
                            parents.insert(parent);
                        }
                    }
                    "client_tool_handoff" => {
                        runs.insert(run, "waiting_client");
                    }
                    "run_finished" | "run_state_changed" => {
                        runs.insert(
                            run,
                            event.payload["status"].as_str().unwrap_or("interrupted"),
                        );
                        if event.kind == "run_state_changed"
                            && event.payload["reason"].as_str() == Some("process_restarted")
                        {
                            active.retain(|(owner, _, _)| *owner != run);
                        }
                    }
                    "model_turn_started" | "target_attempt_started" | "platform_tool_started" => {
                        let (kind, field) = match event.kind.as_str() {
                            "model_turn_started" => ("turn", "model_turn_id"),
                            "target_attempt_started" => ("attempt", "attempt_id"),
                            _ => ("tool", "tool_id"),
                        };
                        if let Some(id) = event.payload[field].as_str() {
                            active.insert((run, kind, id));
                        }
                    }
                    "model_turn_finished"
                    | "target_attempt_finished"
                    | "platform_tool_finished" => {
                        let (kind, field) = match event.kind.as_str() {
                            "model_turn_finished" => ("turn", "model_turn_id"),
                            "target_attempt_finished" => ("attempt", "attempt_id"),
                            _ => ("tool", "tool_id"),
                        };
                        if let Some(id) = event.payload[field].as_str() {
                            active.remove(&(run, kind, id));
                        }
                    }
                    _ => {}
                }
                let is_handoff = match event.kind.as_str() {
                    "client_tool_handoff" => true,
                    "client_tool_result" => false,
                    _ => return None,
                };
                Some(super::grouping::ClientToolEvidence {
                    sequence: event.sequence,
                    run_id: run,
                    tool_id: event.payload["tool_id"].as_str(),
                    is_handoff,
                })
            }));
        let status = if active.is_empty() {
            super::grouping::rollup_status(runs.into_iter().map(|(run, status)| {
                (
                    status,
                    !parents.contains(run)
                        && !(status == "waiting_client" && resolved.contains(run)),
                )
            }))
        } else {
            "running"
        };
        let unknown = ConfirmedUsage::default();
        Self {
            status: status.into(),
            usage: ConfirmedUsage::aggregate(
                attempts
                    .values()
                    .map(|usage| usage.as_ref().unwrap_or(&unknown)),
            ),
            visible_tail,
            observation_gap,
        }
    }

    fn summary(
        &self,
        interaction_id: &str,
        root_id: &str,
        through: i64,
        events: Vec<ObservationEvent>,
    ) -> Value {
        let admission = events.iter().find(|event| event.kind == "run_admitted");
        let run_ids: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "run_admitted")
            .filter_map(|event| event.run_id.as_deref())
            .collect();
        serde_json::json!({
            "schema_version": 1,
            "interaction_id": interaction_id,
            "root_id": root_id,
            "parent_interaction_id": admission.and_then(|event| event.payload.get("parent_interaction_id")),
            "first_route_id": admission.and_then(|event| event.payload.get("route_id")),
            "first_model_display_name": admission.and_then(|event| event.payload.get("model_display_name")),
            "started_at": admission.map(|event| event.occurred_at),
            "last_active_at": events.last().map(|event| event.occurred_at),
            "through_event_sequence": through,
            "status": self.status,
            "usage": self.usage,
            "visible_tail": self.visible_tail,
            "observation_gap": self.observation_gap,
            "run_ids": run_ids,
            "events": events,
        })
    }
}

fn collect_captured_artifacts(value: &Value, references: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if object.get("media_externalized").and_then(Value::as_bool) == Some(true)
                && let Some(reference) = object.get("artifact_reference").and_then(Value::as_str)
            {
                references.insert(reference.to_owned());
            }
            for value in object.values() {
                collect_captured_artifacts(value, references);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_captured_artifacts(value, references);
            }
        }
        Value::String(text) => {
            if let Ok(value) = serde_json::from_str::<Value>(text)
                && !value.is_string()
            {
                collect_captured_artifacts(&value, references);
            }
        }
        _ => {}
    }
}

fn stream_bundle(snapshot: BundleSnapshot) -> BundleStream {
    let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    tokio::task::spawn_blocking(move || {
        let error_sender = sender.clone();
        let writer = ChunkWriter::new(sender);
        if write_bundle(writer, snapshot).is_err() {
            // The error is deliberately generic: paths, ticket values, and payloads never enter it.
            let _ = error_sender.blocking_send(Err(io::Error::other("bundle stream failed")));
        }
    });
    Box::pin(ReceiverStream::new(receiver))
}

fn write_bundle(writer: ChunkWriter, mut snapshot: BundleSnapshot) -> io::Result<()> {
    redact_value(&mut snapshot.summary);
    let completeness = bundle_completeness(&snapshot);
    let manifest = BundleManifest::from_snapshot(&snapshot, completeness);
    let mut zip = ZipWriter::new_stream(writer);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    write_json_entry(&mut zip, "manifest.json", &manifest, options)?;
    let summary_name = match &snapshot.kind {
        BundleResourceKind::Interaction => "interaction.json",
        BundleResourceKind::RejectedRequest => "rejected-request.json",
    };
    write_json_entry(&mut zip, summary_name, &snapshot.summary, options)?;
    write_bytes_entry(&mut zip, "README.txt", README.as_bytes(), options)?;

    for (index, run) in snapshot.runs.iter().enumerate() {
        let Some(trace) = &run.trace else {
            continue;
        };
        let path = match &snapshot.kind {
            BundleResourceKind::Interaction => format!(
                "runs/{:03}-{}/events.jsonl",
                index + 1,
                safe_archive_component(&run.run_id)
            ),
            BundleResourceKind::RejectedRequest => "trace/events.jsonl".to_owned(),
        };
        zip.start_file(path, options).map_err(zip_error)?;
        write_trace(&mut zip, trace)?;
    }

    let mut writer = zip.finish().map_err(zip_error)?.into_inner();
    writer.finish()
}

fn write_json_entry<W: Write>(
    zip: &mut ZipWriter<zip::write::StreamWriter<W>>,
    name: &str,
    value: &impl Serialize,
    options: SimpleFileOptions,
) -> io::Result<()> {
    zip.start_file(name, options).map_err(zip_error)?;
    serde_json::to_writer_pretty(&mut *zip, value).map_err(io::Error::other)?;
    zip.write_all(b"\n")
}

fn write_bytes_entry<W: Write>(
    zip: &mut ZipWriter<zip::write::StreamWriter<W>>,
    name: &str,
    bytes: &[u8],
    options: SimpleFileOptions,
) -> io::Result<()> {
    zip.start_file(name, options).map_err(zip_error)?;
    zip.write_all(bytes)
}

fn write_trace<W: Write>(
    zip: &mut ZipWriter<zip::write::StreamWriter<W>>,
    snapshot: &TraceSnapshot,
) -> io::Result<()> {
    // 存储引用只属于磁盘表示；导出恢复完整记录，wire 字符串不重新解释。
    for segment in &snapshot.segments {
        super::trace_storage::visit(&segment.path, segment.bytes, |record| {
            serde_json::to_writer(&mut *zip, &record).map_err(io::Error::other)?;
            zip.write_all(b"\n")
        })?;
    }
    Ok(())
}

#[derive(Serialize)]
struct BundleManifest<'a> {
    schema_version: u32,
    kind: &'static str,
    resource_id: &'a str,
    exported_at: i64,
    through_event_sequence: i64,
    status: &'a str,
    completeness: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    runs: Vec<CaptureManifest<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rejected_request: Option<CaptureManifest<'a>>,
    redaction: RedactionDeclaration,
    capture_policy: &'static str,
    historical_records: &'static str,
    fidelity: &'static str,
    media_content_policy: &'static str,
}

#[derive(Serialize)]
struct CaptureManifest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rejection_id: Option<&'a str>,
    debug_enabled: bool,
    capture_status: &'a str,
    bytes_written: u64,
    snapshot_bytes: u64,
    reasons: &'a [String],
}

#[derive(Serialize)]
struct RedactionDeclaration {
    permanent: bool,
    replacement: &'static str,
    categories: [&'static str; 1],
    binary_payloads: &'static str,
}

impl<'a> BundleManifest<'a> {
    fn from_snapshot(snapshot: &'a BundleSnapshot, completeness: &'static str) -> Self {
        let interaction = matches!(&snapshot.kind, BundleResourceKind::Interaction);
        let capture = |run: &'a BundleRunSnapshot| CaptureManifest {
            run_id: interaction.then_some(run.run_id.as_str()),
            rejection_id: (!interaction).then_some(run.run_id.as_str()),
            debug_enabled: run.debug_enabled,
            capture_status: capture_status(run),
            bytes_written: run.bytes_written,
            snapshot_bytes: run.trace.as_ref().map_or(0, |trace| trace.bytes),
            reasons: &run.reasons,
        };
        Self {
            schema_version: BUNDLE_SCHEMA_VERSION,
            kind: match &snapshot.kind {
                BundleResourceKind::Interaction => "interaction",
                BundleResourceKind::RejectedRequest => "rejected_request",
            },
            resource_id: &snapshot.resource_id,
            exported_at: snapshot.exported_at,
            through_event_sequence: snapshot.through_sequence,
            status: &snapshot.resource_status,
            completeness,
            runs: if interaction {
                snapshot.runs.iter().map(capture).collect()
            } else {
                Vec::new()
            },
            rejected_request: if interaction {
                None
            } else {
                snapshot.runs.first().map(capture)
            },
            redaction: RedactionDeclaration {
                permanent: true,
                replacement: "***",
                categories: ["http_authorization_header_values"],
                binary_payloads: "captured_as_observed_bytes_without_decoding_or_externalization",
            },
            capture_policy: "current_capture_authorization_header_values_only",
            historical_records: "per_record_metadata_authoritative_legacy_records_exported_unchanged",
            fidelity: "application_protocol_capture_not_packet_capture",
            media_content_policy: "current_capture_raw_wire_bytes_embedded_without_artifact_externalization",
        }
    }
}

fn capture_status(run: &BundleRunSnapshot) -> &str {
    if !run.debug_enabled {
        "not_enabled"
    } else if run.trace.as_ref().is_none_or(|trace| trace.bytes == 0) {
        "missing"
    } else if run.trace_status == "complete" && run.reasons.is_empty() {
        "complete"
    } else if run.trace_status == "running" && run.reasons.is_empty() {
        "captured"
    } else {
        "partial"
    }
}

fn bundle_completeness(snapshot: &BundleSnapshot) -> &'static str {
    let statuses: Vec<&str> = snapshot.runs.iter().map(capture_status).collect();
    if statuses.is_empty()
        || statuses
            .iter()
            .all(|status| *status == "not_enabled" || *status == "missing")
    {
        return "none";
    }
    let terminal = !matches!(
        snapshot.resource_status.as_str(),
        "running" | "waiting_client"
    );
    if terminal && statuses.iter().all(|status| *status == "complete") {
        "complete"
    } else {
        "partial"
    }
}

fn safe_archive_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn zip_error(error: zip::result::ZipError) -> io::Error {
    io::Error::other(error)
}

struct ChunkWriter {
    sender: mpsc::Sender<Result<Bytes, io::Error>>,
    buffer: Vec<u8>,
}

impl ChunkWriter {
    fn new(sender: mpsc::Sender<Result<Bytes, io::Error>>) -> Self {
        Self {
            sender,
            buffer: Vec::with_capacity(STREAM_CHUNK_BYTES),
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = Bytes::from(std::mem::replace(
            &mut self.buffer,
            Vec::with_capacity(STREAM_CHUNK_BYTES),
        ));
        self.sender
            .blocking_send(Ok(bytes))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "bundle receiver closed"))
    }

    fn finish(&mut self) -> io::Result<()> {
        self.emit()
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let original = bytes.len();
        while !bytes.is_empty() {
            let available = STREAM_CHUNK_BYTES - self.buffer.len();
            let count = available.min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.buffer.len() == STREAM_CHUNK_BYTES {
                self.emit()?;
            }
        }
        Ok(original)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit()
    }
}

const README: &str = "Stravia Interaction Debug Bundle\n\nSchema version: 1\n\nThis archive contains application-protocol observations captured at Stravia transport boundaries. It is not a packet capture and does not preserve TLS records, TCP packets, HTTP/2 frames, or operating-system network framing. HTTP headers, body chunks, SSE bytes, WebSocket handshake metadata, and application messages reflect observed application boundaries.\n\nOnly HTTP Authorization header values (case-insensitive header name) are permanently replaced with *** before any queue or storage. Other headers, URL and query values, prompts, tool content, credentials in other locations, and media remain as captured. For current captures, binary and media payloads remain raw wire bytes; they are not decoded, scanned, reassembled, or externalized as Artifact references. Ping and Pong payloads are metadata-only. Legacy records are exported unchanged: their per-record redaction and media metadata remain authoritative, and prior redaction or externalization cannot be reversed. Treat this bundle as sensitive diagnostic data that may contain credentials.\n\nCompleteness is declared in manifest.json. complete means every applicable Debug trace for a terminal resource is present. partial includes running point-in-time snapshots, mixed Debug enablement, writer/capacity/storage gaps, and missing applicable records. none means no applicable Debug trace is available. Each run entry gives its own capture status, byte counts, and stable reasons. The export is fixed through through_event_sequence; later activity is not included.\n";

#[cfg(test)]
mod tests;
