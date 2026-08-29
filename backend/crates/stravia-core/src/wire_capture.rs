use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

const CAPTURE_VERSION: u32 = 3;

#[derive(Clone)]
pub(crate) struct WireCapture {
    inner: Arc<WireCaptureInner>,
}

struct WireCaptureInner {
    directory: PathBuf,
    sequence: AtomicU64,
    paths: std::sync::Mutex<CapturePaths>,
    write_lock: std::sync::Mutex<()>,
}

#[derive(Default)]
struct CapturePaths {
    requests: HashMap<String, PathBuf>,
    chains: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapturePeer {
    Client,
    Upstream,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapturePhase {
    Request,
    Response,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureTransport {
    Http,
    Sse,
    WebSocket,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureRepresentation {
    Wire,
    Normalized,
}

fn legacy_capture_representation() -> CaptureRepresentation {
    CaptureRepresentation::Normalized
}

#[derive(Debug, Serialize, Deserialize)]
struct CaptureEvent {
    version: u32,
    sequence: u64,
    recorded_at: String,
    capture_id: String,
    peer: CapturePeer,
    phase: CapturePhase,
    transport: CaptureTransport,
    protocol: String,
    #[serde(default = "legacy_capture_representation")]
    representation: CaptureRepresentation,
    status: Option<u16>,
    headers: Option<String>,
    body: String,
}

impl WireCapture {
    pub(crate) fn new(directory: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "failed to create wire capture directory {}",
                directory.display()
            )
        })?;
        Ok(Self {
            inner: Arc::new(WireCaptureInner {
                directory,
                sequence: AtomicU64::new(0),
                paths: std::sync::Mutex::new(CapturePaths::default()),
                write_lock: std::sync::Mutex::new(()),
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn path_for(&self, capture_id: &str) -> PathBuf {
        self.path_for_time(capture_id, chrono::Utc::now())
    }

    fn path_for_time(
        &self,
        capture_id: &str,
        recorded_at: chrono::DateTime<chrono::Utc>,
    ) -> PathBuf {
        let mut paths = self
            .inner
            .paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        paths
            .requests
            .entry(capture_id.to_owned())
            .or_insert_with(|| {
                self.inner
                    .directory
                    .join(capture_filename(capture_id, recorded_at))
            })
            .clone()
    }

    pub(crate) fn bind_chain(&self, capture_id: &str, root_chain_id: &str) {
        let Ok(_guard) = self.inner.write_lock.lock() else {
            return;
        };
        let mut paths = self
            .inner
            .paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source = paths
            .requests
            .entry(capture_id.to_owned())
            .or_insert_with(|| {
                self.inner
                    .directory
                    .join(capture_filename(capture_id, chrono::Utc::now()))
            })
            .clone();
        let target = paths
            .chains
            .entry(root_chain_id.to_owned())
            .or_insert_with(|| {
                existing_chain_path(&self.inner.directory, root_chain_id)
                    .unwrap_or_else(|| chain_path_from_request(&source, root_chain_id))
            })
            .clone();
        if source == target {
            return;
        }

        let moved = if !source.exists() {
            true
        } else if target.exists() {
            append_capture_file(&source, &target)
        } else {
            fs::rename(&source, &target).is_ok()
        };
        if moved {
            paths.requests.insert(capture_id.to_owned(), target);
        } else {
            tracing::warn!(
                source = %source.display(),
                target = %target.display(),
                "failed to bind wire capture to generation chain"
            );
        }
    }

    pub(crate) fn record_client_request(
        &self,
        capture_id: &str,
        protocol: &str,
        envelope: &crate::protocol::ir::RawEnvelope,
    ) {
        let body = envelope
            .body
            .as_ref()
            .and_then(|body| serde_json::to_vec(body).ok())
            .unwrap_or_default();
        self.record(
            capture_id,
            CapturePeer::Client,
            CapturePhase::Request,
            CaptureTransport::Http,
            protocol,
            None,
            crate::proxy::observability::header_map_to_redacted_json(&envelope.headers),
            &body,
        );
    }

    pub(crate) fn wrap_client_response(
        &self,
        capture_id: String,
        protocol: String,
        response: axum::response::Response,
    ) -> axum::response::Response {
        let (parts, body) = response.into_parts();
        self.record(
            &capture_id,
            CapturePeer::Client,
            CapturePhase::Response,
            CaptureTransport::Http,
            &protocol,
            Some(parts.status.as_u16()),
            crate::proxy::observability::headers_to_json(&parts.headers),
            &[],
        );
        let capture = self.clone();
        let status = parts.status.as_u16();
        let stream = body.into_data_stream().map(move |chunk| {
            if let Ok(bytes) = &chunk {
                capture.record(
                    &capture_id,
                    CapturePeer::Client,
                    CapturePhase::Response,
                    CaptureTransport::Http,
                    &protocol,
                    Some(status),
                    None,
                    bytes,
                );
            }
            chunk
        });
        axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &self,
        capture_id: &str,
        peer: CapturePeer,
        phase: CapturePhase,
        transport: CaptureTransport,
        protocol: impl Into<String>,
        status: Option<u16>,
        headers: Option<String>,
        body: &[u8],
    ) {
        self.record_with_representation(
            capture_id,
            peer,
            phase,
            transport,
            protocol,
            CaptureRepresentation::Wire,
            status,
            headers,
            body,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_normalized(
        &self,
        capture_id: &str,
        peer: CapturePeer,
        phase: CapturePhase,
        transport: CaptureTransport,
        protocol: impl Into<String>,
        status: Option<u16>,
        headers: Option<String>,
        body: &[u8],
    ) {
        self.record_with_representation(
            capture_id,
            peer,
            phase,
            transport,
            protocol,
            CaptureRepresentation::Normalized,
            status,
            headers,
            body,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_with_representation(
        &self,
        capture_id: &str,
        peer: CapturePeer,
        phase: CapturePhase,
        transport: CaptureTransport,
        protocol: impl Into<String>,
        representation: CaptureRepresentation,
        status: Option<u16>,
        headers: Option<String>,
        body: &[u8],
    ) {
        let recorded_at = chrono::Utc::now();
        let event = CaptureEvent {
            version: CAPTURE_VERSION,
            sequence: self.inner.sequence.fetch_add(1, Ordering::Relaxed),
            recorded_at: recorded_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            capture_id: capture_id.to_owned(),
            peer,
            phase,
            transport,
            protocol: protocol.into(),
            representation,
            status,
            headers,
            body: String::from_utf8_lossy(body).into_owned(),
        };
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        let Ok(_guard) = self.inner.write_lock.lock() else {
            return;
        };
        let path = self.path_for_time(capture_id, recorded_at);
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = writeln!(file, "{line}") {
                    tracing::warn!(path = %path.display(), error = %error, "failed to write wire capture");
                }
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "failed to open wire capture");
            }
        }
    }
}

fn capture_filename(capture_id: &str, recorded_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{}__{capture_id}.jsonl",
        recorded_at.format("%Y-%m-%dT%H-%M-%S%.3fZ")
    )
}

fn chain_path_from_request(request_path: &Path, root_chain_id: &str) -> PathBuf {
    let timestamp = request_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once("__"))
        .map(|(timestamp, _)| timestamp.to_owned())
        .unwrap_or_else(|| {
            chrono::Utc::now()
                .format("%Y-%m-%dT%H-%M-%S%.3fZ")
                .to_string()
        });
    request_path.with_file_name(format!("{timestamp}__chain-{root_chain_id}.jsonl"))
}

fn existing_chain_path(directory: &Path, root_chain_id: &str) -> Option<PathBuf> {
    let suffix = format!("__chain-{root_chain_id}.jsonl");
    fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
        .min()
}

fn append_capture_file(source: &Path, target: &Path) -> bool {
    let Ok(bytes) = fs::read(source) else {
        return false;
    };
    let Ok(mut target_file) = OpenOptions::new().append(true).open(target) else {
        return false;
    };
    if target_file.write_all(&bytes).is_err() {
        return false;
    }
    if let Err(error) = fs::remove_file(source) {
        tracing::warn!(
            source = %source.display(),
            error = %error,
            "failed to remove merged wire capture file"
        );
    }
    true
}

#[cfg(test)]
pub(crate) fn replay_upstream_responses(
    path: &Path,
) -> anyhow::Result<Vec<crate::protocol::ir::AiStreamDelta>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open wire capture {}", path.display()))?;
    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let event: CaptureEvent = serde_json::from_str(&line)
            .with_context(|| format!("invalid wire capture event in {}", path.display()))?;
        anyhow::ensure!(
            matches!(event.version, 2 | CAPTURE_VERSION),
            "unsupported wire capture version {}",
            event.version
        );
        if event.peer == CapturePeer::Upstream
            && event.phase == CapturePhase::Response
            && !(event.transport == CaptureTransport::WebSocket
                && event.representation == CaptureRepresentation::Wire)
        {
            events.push(event);
        }
    }
    events.sort_by_key(|event| event.sequence);
    let protocol = events
        .first()
        .map(|event| event.protocol.as_str())
        .context("wire capture contains no upstream response events")?;
    let endpoint = crate::protocol::ProviderProtocols::parse_protocol_key(protocol)
        .with_context(|| format!("unknown captured protocol {protocol}"))?;
    let mut decoder =
        crate::protocol::transform::ProtocolTransform::global().decode_stream(endpoint)?;
    let mut deltas = Vec::new();
    for event in events {
        deltas.extend(decoder.decode_chunk(event.body.as_bytes())?);
    }
    deltas.extend(decoder.finish()?);
    Ok(deltas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::codec::open_responses::formatter::response_resource_snapshot;
    use crate::protocol::ids::OPEN_RESPONSES_2026_04_24;
    use chrono::TimeZone;

    #[test]
    fn capture_filename_contains_sortable_utc_time_and_request_id() {
        let recorded_at = chrono::Utc
            .with_ymd_and_hms(2026, 8, 24, 1, 35, 58)
            .single()
            .expect("UTC timestamp")
            + chrono::Duration::milliseconds(232);

        assert_eq!(
            capture_filename("req-3e1a456a-795d-43d2-b4a1-3358eda67812", recorded_at),
            "2026-08-24T01-35-58.232Z__req-3e1a456a-795d-43d2-b4a1-3358eda67812.jsonl"
        );
    }

    #[test]
    fn generation_chain_requests_share_the_root_capture_file() {
        let directory = tempfile::tempdir().expect("capture directory");
        let capture = WireCapture::new(directory.path().to_path_buf()).expect("wire capture");

        capture.record(
            "req-first",
            CapturePeer::Client,
            CapturePhase::Request,
            CaptureTransport::Http,
            OPEN_RESPONSES_2026_04_24.to_string(),
            None,
            None,
            b"first request",
        );
        capture.bind_chain("req-first", "resp_root");
        capture.record(
            "req-first",
            CapturePeer::Client,
            CapturePhase::Response,
            CaptureTransport::Http,
            OPEN_RESPONSES_2026_04_24.to_string(),
            Some(200),
            None,
            b"first response",
        );
        capture.record(
            "req-second",
            CapturePeer::Client,
            CapturePhase::Request,
            CaptureTransport::Http,
            OPEN_RESPONSES_2026_04_24.to_string(),
            None,
            None,
            b"second request",
        );
        capture.bind_chain("req-second", "resp_root");
        capture.record(
            "req-second",
            CapturePeer::Client,
            CapturePhase::Response,
            CaptureTransport::Http,
            OPEN_RESPONSES_2026_04_24.to_string(),
            Some(200),
            None,
            b"second response",
        );

        let paths = fs::read_dir(directory.path())
            .expect("capture directory")
            .map(|entry| entry.expect("capture entry").path())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 1);
        assert!(
            paths[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("__chain-resp_root.jsonl"))
        );
        let events = fs::read_to_string(&paths[0])
            .expect("chain capture")
            .lines()
            .map(|line| serde_json::from_str::<CaptureEvent>(line).expect("capture event"))
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .map(|event| event.capture_id.as_str())
                .collect::<Vec<_>>(),
            vec!["req-first", "req-first", "req-second", "req-second"]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.body.as_str())
                .collect::<Vec<_>>(),
            vec![
                "first request",
                "first response",
                "second request",
                "second response"
            ]
        );
    }

    #[test]
    fn captured_upstream_stream_replays_through_the_real_decoder() {
        let directory = tempfile::tempdir().expect("capture directory");
        let capture = WireCapture::new(directory.path().to_path_buf()).expect("wire capture");
        let capture_id = "req-replay";
        let created = serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response_resource_snapshot(
                "resp_1",
                "gpt-5.6-sol",
                "in_progress",
                Vec::new(),
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
            )
        });
        let added = serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "content": [],
                "internal_chat_message_metadata_passthrough": {"source": "codex"}
            }
        });
        let done = serde_json::json!({
            "type": "response.output_item.done",
            "sequence_number": 2,
            "output_index": 0,
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "content": [],
                "internal_chat_message_metadata_passthrough": {"source": "codex"}
            }
        });
        let completed = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 3,
            "response": response_resource_snapshot(
                "resp_1",
                "gpt-5.6-sol",
                "completed",
                Vec::new(),
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::json!({
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "total_tokens": 2,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens_details": {"reasoning_tokens": 1}
                }),
            )
        });
        for event in [created, added, done, completed] {
            let event_type = event["type"].as_str().expect("event type");
            let wire = event.to_string();
            let frame = format!("event: {event_type}\ndata: {event}\n\n");
            capture.record(
                capture_id,
                CapturePeer::Upstream,
                CapturePhase::Response,
                CaptureTransport::WebSocket,
                OPEN_RESPONSES_2026_04_24.to_string(),
                Some(200),
                None,
                wire.as_bytes(),
            );
            capture.record_normalized(
                capture_id,
                CapturePeer::Upstream,
                CapturePhase::Response,
                CaptureTransport::WebSocket,
                OPEN_RESPONSES_2026_04_24.to_string(),
                Some(200),
                None,
                frame.as_bytes(),
            );
        }
        capture.record_normalized(
            capture_id,
            CapturePeer::Upstream,
            CapturePhase::Response,
            CaptureTransport::WebSocket,
            OPEN_RESPONSES_2026_04_24.to_string(),
            Some(200),
            None,
            b"data: [DONE]\n\n",
        );

        let capture_path = capture.path_for(capture_id);
        assert!(
            capture_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("__req-replay.jsonl"))
        );
        let captured_events = fs::read_to_string(&capture_path)
            .expect("captured events")
            .lines()
            .map(|line| serde_json::from_str::<CaptureEvent>(line).expect("capture event"))
            .collect::<Vec<_>>();
        assert_eq!(
            captured_events
                .iter()
                .filter(|event| event.representation == CaptureRepresentation::Wire)
                .count(),
            4
        );
        assert_eq!(
            captured_events
                .iter()
                .filter(|event| event.representation == CaptureRepresentation::Normalized)
                .count(),
            5
        );
        let deltas =
            replay_upstream_responses(&capture_path).expect("timestamped captured stream replay");
        assert!(
            deltas
                .iter()
                .any(|delta| matches!(delta, crate::protocol::ir::AiStreamDelta::ItemDone { .. }))
        );

        let legacy_path = directory.path().join("req-replay.jsonl");
        let legacy = captured_events
            .iter()
            .filter(|event| event.representation == CaptureRepresentation::Normalized)
            .map(|event| {
                let mut value = serde_json::to_value(event).expect("legacy capture value");
                value["version"] = serde_json::Value::from(2);
                value
                    .as_object_mut()
                    .expect("capture object")
                    .remove("representation");
                serde_json::to_string(&value).expect("legacy capture line")
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&legacy_path, format!("{legacy}\n")).expect("legacy capture fixture");
        replay_upstream_responses(&legacy_path)
            .expect("legacy capture filename remains replayable");
    }

    #[test]
    #[ignore = "set STRAVIA_WIRE_REPLAY_FILE to replay a diagnostic capture"]
    fn replay_wire_capture_from_environment() {
        let path = std::env::var_os("STRAVIA_WIRE_REPLAY_FILE")
            .map(PathBuf::from)
            .expect("STRAVIA_WIRE_REPLAY_FILE");
        replay_upstream_responses(&path).expect("captured upstream response must decode");
    }
}
