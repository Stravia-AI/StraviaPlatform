use super::*;

use serde_json::Value;

fn wire_record(sequence: i64, message_type: &str, payload: impl Into<WirePayload>) -> TraceRecord {
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
        transport: Some("http".to_owned()),
        protocol: Some("openai-compatible".to_owned()),
        message_type: Some(message_type.to_owned()),
        representation: "wire".to_owned(),
        status: None,
        status_code: Some(200),
        url: None,
        headers: Value::Null,
        payload_encoding: "json".to_owned(),
        payload: payload.into(),
        error: None,
        redactions: Vec::new(),
    }
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
async fn actual_wire_capture_limit_is_partial() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    // Exercise the production cap without changing process-wide environment.
    let handle = manager.create();
    let oversized = bytes::Bytes::from(vec![b'x'; WIRE_CAPTURE_LIMIT_BYTES + 1]);
    assert_eq!(
        handle.record(wire_record(1, "body", WirePayload::Bytes(oversized))),
        TraceWriteOutcome::Partial(WIRE_CAPTURE_LIMIT)
    );
    assert_eq!(
        handle.record(wire_record(2, "body", Value::String("survives".into()))),
        TraceWriteOutcome::Queued
    );
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "partial");
    assert_eq!(manifest.reasons, [WIRE_CAPTURE_LIMIT]);
    assert_eq!(persisted_records(&handle).await.len(), 1);
    manager.shutdown().await;
}

#[test]
fn shared_wire_payload_matches_legacy_encoding_and_capture_budget() {
    for bytes in [
        &b""[..],
        &b"{\"not\":\"parsed\"}\n\0\\\""[..],
        "雪😀".as_bytes(),
        &b"\xff"[..],
        &b"\xff\0"[..],
        &b"\xff\0\x80"[..],
        &b"\xff\0\x80\x81"[..],
    ] {
        let legacy = match std::str::from_utf8(bytes) {
            Ok(text) => Value::String(text.to_owned()),
            Err(_) => serde_json::json!({
                "encoding": "base64",
                "data": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD, bytes,
                ),
            }),
        };
        let shared = WirePayload::Bytes(bytes::Bytes::copy_from_slice(bytes));
        let encoded = serde_json::to_vec(&shared).unwrap();
        assert_eq!(encoded, serde_json::to_vec(&legacy).unwrap());
        assert_eq!(wire_payload_size(&shared), wire_value_size(&legacy));
        let restored: WirePayload = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), legacy);
        if let Value::Object(object) = legacy {
            let normalized = WirePayload::Base64(bytes::Bytes::copy_from_slice(bytes));
            assert_eq!(
                serde_json::to_vec(&normalized).unwrap(),
                serde_json::to_vec(&object["data"]).unwrap(),
            );
            assert_eq!(
                wire_payload_size(&normalized),
                wire_value_size(&object["data"])
            );
        }
    }
}

#[tokio::test]
async fn shared_wire_bytes_preserve_chunks_order_and_snapshot_frontier() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let chunk = bytes::Bytes::from_static(b"first\xff\0last");
    for (sequence, range) in [(1, 0..5), (2, 5..7), (3, 7..11)] {
        assert_eq!(
            handle.record(wire_record(
                sequence,
                "body_chunk",
                WirePayload::Bytes(chunk.slice(range))
            )),
            TraceWriteOutcome::Queued,
        );
    }
    drop(chunk);
    let snapshot = handle.snapshot(2).await.expect("snapshot");
    let mut visible = Vec::new();
    for segment in snapshot.segments {
        super::super::trace_storage::visit(&segment.path, segment.bytes, |record| {
            visible.push(record);
            Ok(())
        })
        .expect("snapshot records");
    }
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0]["payload"], "first");
    assert_eq!(
        visible[1]["payload"],
        serde_json::json!({"data": "/wA=", "encoding": "base64"})
    );
    handle.finish().await;
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 3);
    assert_eq!(serde_json::to_value(&records[2].payload).unwrap(), "last");
    manager.shutdown().await;
}

#[tokio::test]
async fn manifest_directory_created_before_trace_writer_preserves_wire_capture() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    // 当前线程在首次 await 前不会处理 Create，固定复现 manifest 写者先创建目录的顺序。
    std::fs::create_dir(
        directory
            .path()
            .join("observation-debug")
            .join(handle.manifest().trace_id),
    )
    .expect("manifest writer directory");
    assert_eq!(
        handle.record(wire_record(
            1,
            "sse_chunk",
            Value::String("data: captured\n\n".into())
        )),
        TraceWriteOutcome::Queued
    );
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "complete");
    let records = persisted_records(&handle).await;
    assert_eq!(records[0].sequence, 1);
    assert_eq!(
        serde_json::to_value(&records[0].payload).unwrap(),
        "data: captured\n\n"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn wire_chunks_are_exported_immediately_without_protocol_reassembly() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();

    assert_eq!(
        handle.record(wire_record(1, "sse_chunk", Value::String("data: {".into()))),
        TraceWriteOutcome::Queued
    );
    let before_finish = handle.snapshot(i64::MAX).await.expect("running snapshot");
    let mut running = Vec::new();
    for segment in before_finish.segments {
        super::super::trace_storage::visit(&segment.path, segment.bytes, |record| {
            running.push(record);
            Ok(())
        })
        .expect("read running snapshot");
    }
    assert_eq!(running.len(), 1);
    assert_eq!(running[0]["payload"], "data: {");

    assert_eq!(
        handle.record(wire_record(
            2,
            "sse_chunk",
            Value::String("not-json\n\n".into())
        )),
        TraceWriteOutcome::Queued
    );
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "complete");
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 2);
    assert_eq!(
        serde_json::to_value(&records[0].payload).unwrap(),
        "data: {"
    );
    assert_eq!(
        serde_json::to_value(&records[1].payload).unwrap(),
        "not-json\n\n"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn debug_redacts_only_authorization_header_values() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let sentinel = "same-synthetic-marker";
    let mut record = wire_record(1, "request_head", Value::String(sentinel.into()));
    record.direction = Some("client_to_platform".into());
    record.url = Some(format!("https://example.test/v1?token={sentinel}"));
    record.headers = serde_json::json!({
        "aUtHoRiZaTiOn": [sentinel, sentinel],
        "x-api-key": sentinel,
        "Cookie": sentinel
    });

    assert_eq!(handle.record(record), TraceWriteOutcome::Queued);
    assert_eq!(handle.finish().await.status, "complete");
    let records = persisted_records(&handle).await;
    let record = &records[0];
    assert_eq!(
        record.headers["aUtHoRiZaTiOn"],
        serde_json::json!(["***", "***"])
    );
    assert_eq!(record.headers["x-api-key"], sentinel);
    assert_eq!(record.headers["Cookie"], sentinel);
    assert_eq!(
        record.url.as_deref(),
        Some(format!("https://example.test/v1?token={sentinel}").as_str())
    );
    assert_eq!(serde_json::to_value(&record.payload).unwrap(), sentinel);
    assert_eq!(record.redactions, [RedactionKind::CredentialHeader]);
    manager.shutdown().await;
}

#[tokio::test]
async fn ordered_wire_headers_preserve_bytes_and_redact_every_authorization() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let mut record = wire_record(1, "request_head", Value::Null);
    record.headers = serde_json::json!([
        {"name": "X-Test", "bytes": [255, 0, 65]},
        {"name": "Authorization", "bytes": [115, 101, 99, 114, 101, 116]},
        {"name": "Cookie", "bytes": [1, 2]},
        {"name": "aUtHoRiZaTiOn", "bytes": []},
        {"name": "X-Test", "bytes": [66]}
    ]);

    assert_eq!(handle.record(record), TraceWriteOutcome::Queued);
    assert_eq!(handle.finish().await.status, "complete");
    let records = persisted_records(&handle).await;
    assert_eq!(records[0].schema_version, 3);
    assert_eq!(
        records[0].headers,
        serde_json::json!([
            {"name": "X-Test", "bytes": [255, 0, 65]},
            {"name": "Authorization", "bytes": [42, 42, 42]},
            {"name": "Cookie", "bytes": [1, 2]},
            {"name": "aUtHoRiZaTiOn", "bytes": [42, 42, 42]},
            {"name": "X-Test", "bytes": [66]}
        ])
    );
    assert_eq!(records[0].redactions, [RedactionKind::CredentialHeader]);
    manager.shutdown().await;
}

#[tokio::test]
async fn raw_media_non_utf8_and_websocket_messages_preserve_observed_bytes() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let media = r#"{"image":"data:image/png;base64,AAECA/8="}"#;
    assert_eq!(
        handle.record(wire_record(1, "body_chunk", Value::String(media.into()))),
        TraceWriteOutcome::Queued
    );

    let mut non_utf8 = wire_record(2, "body_chunk", Value::String("/wAB".into()));
    non_utf8.payload_encoding = "base64".into();
    assert_eq!(handle.record(non_utf8), TraceWriteOutcome::Queued);

    let mut websocket_text = wire_record(3, "text", Value::String("ws-雪".into()));
    websocket_text.transport = Some("websocket".into());
    assert_eq!(handle.record(websocket_text), TraceWriteOutcome::Queued);

    let mut websocket_binary = wire_record(4, "binary", Value::String("AP+A".into()));
    websocket_binary.transport = Some("websocket".into());
    websocket_binary.payload_encoding = "base64".into();
    assert_eq!(handle.record(websocket_binary), TraceWriteOutcome::Queued);

    assert_eq!(handle.finish().await.status, "complete");
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 4);
    assert_eq!(serde_json::to_value(&records[0].payload).unwrap(), media);
    assert_eq!(records[1].payload_encoding, "base64");
    assert_eq!(serde_json::to_value(&records[1].payload).unwrap(), "/wAB");
    assert_eq!(records[2].transport.as_deref(), Some("websocket"));
    assert_eq!(serde_json::to_value(&records[2].payload).unwrap(), "ws-雪");
    assert_eq!(records[3].message_type.as_deref(), Some("binary"));
    assert_eq!(records[3].payload_encoding, "base64");
    assert_eq!(serde_json::to_value(&records[3].payload).unwrap(), "AP+A");
    manager.shutdown().await;
}

#[tokio::test]
async fn malformed_complete_wire_is_complete_capture() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let malformed = "{ definitely not json";
    let record = wire_record(1, "body_chunk", Value::String(malformed.into()));
    assert_eq!(handle.record(record), TraceWriteOutcome::Queued);
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "complete");
    assert!(manifest.reasons.is_empty());
    let records = persisted_records(&handle).await;
    assert_eq!(
        serde_json::to_value(&records[0].payload).unwrap(),
        malformed
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn websocket_ping_pong_are_metadata_only() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    for (sequence, kind) in [(1, "ping"), (2, "pong")] {
        let mut record = wire_record(sequence, kind, Value::String("control-payload".into()));
        record.transport = Some("websocket".into());
        assert_eq!(handle.record(record), TraceWriteOutcome::Queued);
    }
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "complete");
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 2);
    for record in records {
        let payload = serde_json::to_value(record.payload).unwrap();
        assert_eq!(payload["content_capture"], "omitted");
        assert_eq!(payload["reason"], "control_frame_payload_omitted");
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn oversized_wire_record_marks_partial_without_queueing_payload() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let record = wire_record(
        1,
        "body_chunk",
        Value::String("x".repeat(WIRE_CAPTURE_LIMIT_BYTES + 1)),
    );
    assert_eq!(
        handle.record(record),
        TraceWriteOutcome::Partial(WIRE_CAPTURE_LIMIT)
    );
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "partial");
    assert_eq!(manifest.reasons, [WIRE_CAPTURE_LIMIT.to_owned()]);
    assert!(persisted_records(&handle).await.is_empty());
    manager.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_barrier_seals_the_batch_before_later_records() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    assert_eq!(
        handle.record(wire_record(7, "body_chunk", Value::String("before".into()))),
        TraceWriteOutcome::Queued
    );

    let snapshot = handle.snapshot(7);
    tokio::pin!(snapshot);
    tokio::select! {
        biased;
        result = &mut snapshot => panic!("snapshot completed before its queued writer work: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    assert_eq!(
        handle.record(wire_record(7, "body_chunk", Value::String("after".into()))),
        TraceWriteOutcome::Queued
    );

    let snapshot = snapshot.await.expect("fixed-cut snapshot");
    let mut records = Vec::new();
    for segment in snapshot.segments {
        super::super::trace_storage::visit(&segment.path, segment.bytes, |record| {
            records.push(serde_json::from_value::<TraceRecord>(record).expect("trace record"));
            Ok(())
        })
        .expect("read fixed-cut snapshot");
    }
    assert_eq!(records.len(), 1);
    assert_eq!(serde_json::to_value(&records[0].payload).unwrap(), "before");
    assert_eq!(handle.finish().await.status, "complete");
    assert_eq!(persisted_records(&handle).await.len(), 2);
    manager.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn full_queue_of_oversized_batches_still_reports_partial() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    let payload = "x".repeat(WRITE_BATCH_BYTES);

    for index in 0..WRITER_QUEUE_CAPACITY - 1 {
        assert_eq!(
            handle.record(wire_record(
                index as i64 + 1,
                "body_chunk",
                Value::String(payload.clone()),
            )),
            TraceWriteOutcome::Queued
        );
    }
    assert_eq!(
        handle.record(wire_record(
            WRITER_QUEUE_CAPACITY as i64,
            "body_chunk",
            Value::String(payload),
        )),
        TraceWriteOutcome::Partial(WRITER_OVERFLOW)
    );
    assert_eq!(handle.manifest().status, "partial");
    assert_eq!(handle.manifest().reasons, [WRITER_OVERFLOW.to_owned()]);
    manager.shutdown().await;
}

#[tokio::test]
async fn flushed_manifest_matches_readable_trace() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    assert_eq!(
        handle.record(wire_record(
            1,
            "sse_chunk",
            Value::String("data: start\n".into()),
        )),
        TraceWriteOutcome::Queued
    );
    handle.flush().await.expect("flush buffered trace record");
    let after_flush = handle.manifest();
    let snapshot = handle.snapshot(i64::MAX).await.expect("flushed snapshot");
    assert_eq!(
        after_flush.bytes_written,
        snapshot
            .segments
            .iter()
            .map(|segment| segment.bytes)
            .sum::<u64>()
    );
    assert_eq!(after_flush.event_count, 1);
    assert_eq!(persisted_records(&handle).await.len(), 1);
    handle.finish().await;
    manager.shutdown().await;
}

#[tokio::test]
async fn stopped_capture_preserves_previously_queued_records() {
    let directory = tempfile::tempdir().expect("trace directory");
    let manager = TraceManager::new(directory.path().to_owned()).expect("trace manager");
    let handle = manager.create();
    assert_eq!(
        handle.record(wire_record(
            1,
            "sse_chunk",
            Value::String("data: start\n".into())
        )),
        TraceWriteOutcome::Queued
    );
    handle.mark_partial(STORAGE_ERROR, true);
    let manifest = handle.finish().await;
    assert_eq!(manifest.status, "partial");
    let records = persisted_records(&handle).await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        serde_json::to_value(&records[0].payload).unwrap(),
        "data: start\n"
    );
    manager.shutdown().await;
}
