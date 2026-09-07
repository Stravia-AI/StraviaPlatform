use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::redaction::redact_value;
use super::trace::TraceSnapshot;
use super::types::{BundleResourceKind, BundleStream, DownloadTicket};

const BUNDLE_SCHEMA_VERSION: u32 = 1;
const TICKET_TTL: Duration = Duration::from_secs(60);
const STREAM_CHANNEL_CAPACITY: usize = 8;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const TICKET_UNAVAILABLE: &str = "download ticket unavailable";

#[derive(Debug, Clone)]
pub(crate) struct BundleSnapshot {
    pub kind: BundleResourceKind,
    pub resource_id: String,
    pub exported_at: i64,
    pub through_sequence: i64,
    pub resource_status: String,
    pub summary: Value,
    pub runs: Vec<BundleRunSnapshot>,
}

#[derive(Debug, Clone)]
pub(crate) struct BundleRunSnapshot {
    pub run_id: String,
    pub debug_enabled: bool,
    pub trace_status: String,
    pub bytes_written: u64,
    pub reasons: Vec<String>,
    pub trace: Option<TraceSnapshot>,
}

pub(crate) struct ConsumedBundle {
    pub stream: BundleStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TicketUnavailable;

impl std::fmt::Display for TicketUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(TICKET_UNAVAILABLE)
    }
}

impl std::error::Error for TicketUnavailable {}

#[derive(Clone, Default)]
pub(crate) struct BundleService {
    tickets: Arc<Mutex<HashMap<String, TicketEntry>>>,
}

struct TicketEntry {
    expires: Instant,
    snapshot: BundleSnapshot,
}

impl BundleService {
    pub(crate) fn issue(&self, mut snapshot: BundleSnapshot) -> DownloadTicket {
        self.remove_expired();
        redact_value(&mut snapshot.summary);
        snapshot.exported_at = chrono::Utc::now().timestamp_millis();
        let expires_at = snapshot
            .exported_at
            .saturating_add(TICKET_TTL.as_millis() as i64);
        let through_sequence = snapshot.through_sequence;
        let mut tickets = self
            .tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        DownloadTicket {
            download_url: format!("/api/v1/observations/debug-bundles/{ticket}"),
            expires_at,
            through_sequence,
        }
    }

    pub(crate) fn consume(&self, ticket: &str) -> Result<ConsumedBundle, TicketUnavailable> {
        if ticket.len() != 64 || !ticket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(TicketUnavailable);
        }
        let entry = self
            .tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(ticket)
            .filter(|entry| Instant::now() < entry.expires)
            .ok_or(TicketUnavailable)?;
        Ok(ConsumedBundle {
            stream: stream_bundle(entry.snapshot),
        })
    }

    fn remove_expired(&self) {
        let now = Instant::now();
        self.tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, entry| entry.expires > now);
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
    // Segment bytes have already passed the sole pre-queue redaction boundary. Copying the
    // immutable byte cutoffs directly preserves JSONL fidelity and keeps export memory bounded.
    for segment in &snapshot.segments {
        let file = std::fs::File::open(&segment.path)?;
        io::copy(&mut file.take(segment.bytes), &mut *zip)?;
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
    fidelity: &'static str,
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
    categories: [&'static str; 5],
    opaque_binary: &'static str,
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
                categories: [
                    "credential_headers",
                    "url_userinfo",
                    "credential_query_values",
                    "recursive_structured_credential_fields",
                    "credential_patterns_in_errors",
                ],
                opaque_binary: "preserved_as_base64_without_structured_field_interpretation",
            },
            fidelity: "application_protocol_capture_not_packet_capture",
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

const README: &str = "Stravia Interaction Debug Bundle\n\nSchema version: 1\n\nThis archive contains application-protocol observations captured at Stravia adapter boundaries. It is not a packet capture and does not preserve TLS records, TCP packets, HTTP/2 frames, or operating-system network framing. HTTP body chunks, SSE bytes, and WebSocket messages reflect the application adapters' observed boundaries.\n\nCredential headers, URL userinfo, credential-like query values, recursive structured credential fields, and recognizable credential patterns in errors are permanently replaced with *** before persistence. Credential patterns in business text are interpreted within each application message, not by joining text across multiple messages. Opaque non-UTF-8 application payloads are preserved as base64 and are not interpreted as structured fields. Other prompts, tool arguments/results, binary content, and business content may remain sensitive.\n\nCompleteness is declared in manifest.json. complete means every applicable Debug trace for a terminal resource is present. partial includes running point-in-time snapshots, mixed Debug enablement, writer/capacity/storage gaps, and missing applicable records. none means no applicable Debug trace is available. Each run entry gives its own capture status, byte counts, and stable reasons. The export is fixed through through_event_sequence; later activity is not included.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn run(debug_enabled: bool, status: &str, captured: bool) -> BundleRunSnapshot {
        BundleRunSnapshot {
            run_id: "run".to_owned(),
            debug_enabled,
            trace_status: status.to_owned(),
            bytes_written: if captured { 1 } else { 0 },
            reasons: Vec::new(),
            trace: captured.then(|| TraceSnapshot {
                segments: Vec::new(),
                bytes: 1,
            }),
        }
    }

    fn snapshot(status: &str, runs: Vec<BundleRunSnapshot>) -> BundleSnapshot {
        BundleSnapshot {
            kind: BundleResourceKind::Interaction,
            resource_id: "interaction".to_owned(),
            exported_at: 0,
            through_sequence: 1,
            resource_status: status.to_owned(),
            summary: Value::Null,
            runs,
        }
    }

    #[test]
    fn bundle_completeness_distinguishes_final_running_mixed_and_absent_capture() {
        assert_eq!(
            bundle_completeness(&snapshot("completed", vec![run(true, "complete", true)])),
            "complete"
        );
        assert_eq!(
            bundle_completeness(&snapshot("running", vec![run(true, "running", true)])),
            "partial"
        );
        assert_eq!(
            bundle_completeness(&snapshot(
                "completed",
                vec![run(true, "complete", true), run(false, "complete", false)]
            )),
            "partial"
        );
        assert_eq!(
            bundle_completeness(&snapshot("completed", vec![run(false, "complete", false)])),
            "none"
        );
    }
}
