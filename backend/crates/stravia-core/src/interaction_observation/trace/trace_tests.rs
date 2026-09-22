use super::*;

use crate::interaction_observation::{RunEvent, record_trace_at};
use base64::Engine;

fn command_code_chunk(sequence: i64, payload: Value) -> TraceRecord {
    TraceRecord {
        capture_id: None,
        schema_version: 0,
        sequence,
        recorded_at: sequence,
        interaction_id: Some("interaction".to_owned()),
        run_id: Some("run".to_owned()),
        rejection_id: None,
        model_turn_id: Some("turn".to_owned()),
        attempt_id: Some("attempt".to_owned()),
        layer: "wire".to_owned(),
        direction: Some("upstream_response".to_owned()),
        stage: None,
        transport: Some("sse".to_owned()),
        protocol: Some(COMMAND_CODE_PROTOCOL.to_owned()),
        message_type: Some("sse_chunk".to_owned()),
        representation: "wire".to_owned(),
        status: None,
        status_code: Some(200),
        url: None,
        headers: Value::Null,
        payload_encoding: "json".to_owned(),
        payload,
        error: None,
        redactions: Vec::new(),
    }
}

fn bytes_value(bytes: &[u8]) -> Value {
    std::str::from_utf8(bytes)
        .map(|text| Value::String(text.to_owned()))
        .unwrap_or_else(|_| {
            serde_json::json!({
                "encoding": "base64",
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
}

async fn persisted_records(handle: &TraceHandle) -> Vec<TraceRecord> {
    let snapshot = handle.snapshot(i64::MAX).await.expect("trace snapshot");
    let mut records = Vec::new();
    for segment in snapshot.segments {
        super::super::trace_storage::visit(&segment.path, segment.bytes, |record| {
            records.push(serde_json::from_value(record).expect("persisted trace record"));
            Ok(())
        })
        .expect("read persisted trace segment");
    }
    records
}

#[tokio::test]
async fn interleaved_http_captures_preserve_independent_json_and_ndjson_messages() {
    for protocol in [COMMAND_CODE_PROTOCOL, "custom-json"] {
        let directory = tempfile::tempdir().expect("trace directory");
        let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
        let handle = manager.create();
        for (sequence, (capture_id, fragment)) in [
            (1, "{\"text\":\"fir"),
            (2, "{\"text\":\"sec"),
            (1, "st\"}\n"),
            (2, "ond\"}\n"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut record =
                command_code_chunk(sequence as i64 + 1, bytes_value(fragment.as_bytes()));
            record.capture_id = Some(capture_id);
            record.protocol = Some(protocol.to_owned());
            record.transport = Some("http".to_owned());
            record.message_type = Some("body_chunk".to_owned());
            record.url = Some("https://vendor.test/same-endpoint".to_owned());
            handle.record(record);
        }
        assert_eq!(handle.finish().await.status, "complete");
        let payloads: Vec<Value> = persisted_records(&handle)
            .await
            .iter()
            .map(|record| {
                serde_json::from_str(record.payload.as_str().expect("text payload"))
                    .expect("JSON message")
            })
            .collect();
        assert_eq!(
            payloads,
            [
                serde_json::json!({"text": "first"}),
                serde_json::json!({"text": "second"})
            ]
        );
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn response_format_is_scoped_and_final_redirect_headers_win() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    for (sequence, (capture_id, content_type, status, payload)) in [
        (1, Some("application/json"), 200, None),
        (1, None, 200, Some("{\n  \"text\": \"first\"\n")),
        (2, Some("application/json"), 302, None),
        (2, Some("application/x-ndjson"), 200, None),
        (
            2,
            None,
            200,
            Some("{\"text\":\"second\"}\n{\"text\":\"third\"}\n"),
        ),
        (1, None, 200, Some("}\n")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut record = command_code_chunk(
            sequence as i64 + 1,
            payload.map_or(Value::Null, |text| bytes_value(text.as_bytes())),
        );
        record.capture_id = Some(capture_id);
        record.transport = Some("http".to_owned());
        record.message_type = Some(
            if content_type.is_some() {
                "response_headers"
            } else {
                "body_chunk"
            }
            .to_owned(),
        );
        record.status_code = Some(status);
        record.headers = content_type.map_or(
            Value::Null,
            |value| serde_json::json!({"content-type": value}),
        );
        record.url = Some("https://vendor.test/same-endpoint".to_owned());
        handle.record(record);
    }
    assert_eq!(handle.finish().await.status, "complete");
    let payloads: Vec<Value> = persisted_records(&handle)
        .await
        .iter()
        .filter(|record| record.representation == "reassembled_application_message")
        .map(|record| {
            serde_json::from_str(record.payload.as_str().expect("text payload"))
                .expect("JSON message")
        })
        .collect();
    assert_eq!(
        payloads,
        [
            serde_json::json!({"text": "second"}),
            serde_json::json!({"text": "third"}),
            serde_json::json!({"text": "first"}),
        ]
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn command_code_ndjson_persists_every_record_and_complete_eof_finish() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();

    assert_eq!(
        handle.record(command_code_chunk(
            1,
            Value::String(
                "{\"type\":\"start\"}\n{\"type\":\"text-delta\",\"text\":\"hello\"}\n{\"type\":\"fin"
                    .to_owned(),
            ),
        )),
        TraceWriteOutcome::Queued
    );
    assert_eq!(
        handle.record(command_code_chunk(
            2,
            Value::String("ish\",\"finishReason\":\"stop\"}".to_owned()),
        )),
        TraceWriteOutcome::Queued
    );

    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "complete");
    assert!(manifest.reasons.is_empty());

    let records = persisted_records(&handle).await;
    let payloads: Vec<&str> = records
        .iter()
        .map(|record| record.payload.as_str().expect("NDJSON text payload"))
        .collect();
    assert_eq!(
        payloads,
        [
            "{\"type\":\"start\"}",
            "{\"type\":\"text-delta\",\"text\":\"hello\"}",
            "{\"type\":\"finish\",\"finishReason\":\"stop\"}",
        ]
    );
    assert!(
        records
            .iter()
            .all(|record| record.representation == "reassembled_application_message")
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn http_wire_reassembles_base64_utf8_slices_before_redaction() {
    let wire = "{\"type\":\"text-delta\",\"text\":\"雪\",\"access_token\":\"split-secret\"}\n"
        .as_bytes()
        .to_vec();
    let snow = "雪".as_bytes();
    let credential_split = wire
        .windows(b"split-secret".len())
        .position(|window| window == b"split-secret")
        .expect("credential value")
        + b"split-".len();
    let utf8_split = wire
        .windows(snow.len())
        .position(|window| window == snow)
        .expect("UTF-8 value")
        + 1;

    for content_type in ["application/x-ndjson", "application/json"] {
        let directory = tempfile::tempdir().expect("trace directory");
        let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
        let handle = manager.create();
        record_trace_at(
            &handle,
            Some("run"),
            None,
            RunEvent::Wire {
                capture_id: Some(1),
                direction: "upstream_response".to_owned(),
                transport: "http".to_owned(),
                protocol: COMMAND_CODE_PROTOCOL.to_owned(),
                message_type: "response_headers".to_owned(),
                model_turn_id: Some("turn".to_owned()),
                attempt_id: Some("attempt".to_owned()),
                status_code: Some(200),
                url: None,
                headers: serde_json::json!({"content-type": content_type}),
                payload: Value::Null,
            },
            0,
        );
        for (sequence, fragment) in [
            &wire[..utf8_split],
            &wire[utf8_split..credential_split],
            &wire[credential_split..],
        ]
        .into_iter()
        .enumerate()
        {
            record_trace_at(
                &handle,
                Some("run"),
                None,
                RunEvent::Wire {
                    capture_id: Some(1),
                    direction: "upstream_response".to_owned(),
                    transport: "http".to_owned(),
                    protocol: COMMAND_CODE_PROTOCOL.to_owned(),
                    message_type: "body_chunk".to_owned(),
                    model_turn_id: Some("turn".to_owned()),
                    attempt_id: Some("attempt".to_owned()),
                    status_code: Some(200),
                    url: None,
                    headers: Value::Null,
                    payload: bytes_value(fragment),
                },
                sequence as i64 + 1,
            );
        }

        let manifest = handle.finish().await;
        assert_eq!(manifest.status, "complete", "{content_type}");
        let mut records = persisted_records(&handle).await;
        records.retain(|record| record.message_type.as_deref() == Some("body_chunk"));
        assert_eq!(records.len(), 1);
        let persisted = records[0]
            .payload
            .as_str()
            .expect("decoded JSON text payload");
        assert!(!persisted.contains("split-secret"));
        assert!(!persisted.contains("split-"));
        let payload: Value = serde_json::from_str(persisted).expect("redacted JSON record");
        assert_eq!(payload["text"], "雪");
        assert_ne!(payload["access_token"], "split-secret");

        manager.shutdown().await;
    }
}

#[tokio::test]
async fn command_code_ndjson_marks_incomplete_tail_partial_without_losing_prior_records() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let sentinel = "never-persist-this";

    assert_eq!(
        handle.record(command_code_chunk(
            1,
            Value::String(format!(
                "{{\"type\":\"start\"}}\n{{\"type\":\"text-delta\",\"access_token\":\"{sentinel}"
            )),
        )),
        TraceWriteOutcome::Queued
    );

    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "partial");
    assert_eq!(
        manifest.reasons,
        ["incomplete_structured_wire_omitted".to_owned()]
    );

    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload, "{\"type\":\"start\"}");
    let persisted = serde_json::to_string(&records).expect("serialize persisted records");
    assert!(!persisted.contains(sentinel));

    manager.shutdown().await;
}

#[tokio::test]
async fn command_code_ndjson_omits_malformed_terminated_record_without_leaking_credentials() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let sentinel = "malformed-secret";

    assert_eq!(
        handle.record(command_code_chunk(
            1,
            Value::String(format!(
                "{{\"type\":\"start\"}}\n{{\"access_token\":\"{sentinel}\"\n"
            )),
        )),
        TraceWriteOutcome::Partial("incomplete_structured_wire_omitted")
    );

    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "partial");
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 1);
    let persisted = serde_json::to_string(&records).expect("serialize persisted records");
    assert!(!persisted.contains(sentinel));

    manager.shutdown().await;
}

#[tokio::test]
async fn non_command_code_chunks_keep_existing_structured_message_capture() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let mut chunk = command_code_chunk(
        1,
        Value::String("{\"type\":\"one\"}\n{\"type\":\"two\"}\n".to_owned()),
    );
    chunk.protocol = Some("open-responses/responses/2026-04-24".to_owned());

    assert_eq!(handle.record(chunk), TraceWriteOutcome::Queued);
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "partial");
    assert!(persisted_records(&handle).await.is_empty());

    manager.shutdown().await;
}
