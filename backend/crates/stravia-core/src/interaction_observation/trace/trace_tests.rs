use super::*;

use base64::Engine;

fn command_code_chunk(sequence: i64, payload: Value) -> TraceRecord {
    TraceRecord {
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
async fn command_code_ndjson_reassembles_base64_utf8_slices_before_redaction() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let wire = "{\"type\":\"text-delta\",\"access_token\":\"split-secret\",\"text\":\"雪\"}\n"
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

    for (sequence, fragment) in [
        &wire[..credential_split],
        &wire[credential_split..utf8_split],
        &wire[utf8_split..],
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            handle.record(command_code_chunk(
                sequence as i64 + 1,
                bytes_value(fragment)
            )),
            TraceWriteOutcome::Queued
        );
    }

    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "complete");
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 1);
    let persisted = records[0].payload.as_str().expect("NDJSON text payload");
    assert!(!persisted.contains("split-secret"));
    assert!(!persisted.contains("split-"));
    let payload: Value = serde_json::from_str(persisted).expect("redacted NDJSON record");
    assert_eq!(payload["text"], "雪");
    assert_ne!(payload["access_token"], "split-secret");

    manager.shutdown().await;
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
