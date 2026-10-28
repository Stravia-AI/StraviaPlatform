use crate::interaction_observation::*;
use futures::StreamExt;
use serde_json::Value;
use std::io::Cursor;

async fn fixture() -> anyhow::Result<(tempfile::TempDir, sqlx::SqlitePool, InteractionObservation)>
{
    let directory = tempfile::tempdir()?;
    let pool = crate::test_support::migrated_sqlite_pool().await?;
    let observation = InteractionObservation::new(
        Some(pool.clone()),
        None,
        directory.path().to_path_buf(),
        1,
        true,
        crate::generation_chain::test_chain().await,
    )
    .await;
    Ok((directory, pool, observation))
}

fn admit(observation: &InteractionObservation, id: &str, parent: Option<&str>) -> RunObserver {
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
                principal: "bundle-owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "route".into(),
                model_display_name: None,
                ingress_protocol: "responses".into(),
            },
            AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                )
                .into(),
                has_new_user: parent.is_none(),
                has_matching_pending_tool_result: false,
                generation_root_id: parent.map(|_| "root-generation".into()),
                generation_parent_id: parent.map(str::to_owned),
            },
        )
}

fn finish(run: &RunObserver, status: &str, generation: Option<&str>) {
    run.finish(RunOutcome {
        client_output_committed: status == "completed" || status == "waiting_client",
        delivery: None,
        delivery_completed_at: Some(super::super::writer::now()),
        status: status.into(),
        terminal_reason: None,
        generation_node_id: generation.map(str::to_owned),
        generation_root_id: generation.map(|_| "root-generation".into()),
    });
}

async fn ticket(observation: &InteractionObservation) -> anyhow::Result<DownloadTicket> {
    observation.flush().await?;
    let forest = observation.query_forest(ForestQuery::default()).await?;
    let interaction = &forest.roots.first().expect("fixture root").interactions[0];
    observation
        .issue_bundle_ticket(BundleRequest {
            kind: BundleResourceKind::Interaction,
            resource_id: interaction.id.clone(),
            through_sequence: Some(forest.snapshot_sequence),
        })
        .await
}

async fn download(
    observation: &InteractionObservation,
    ticket: &DownloadTicket,
) -> anyhow::Result<(Value, Value)> {
    let token = ticket
        .download_url
        .rsplit('/')
        .next()
        .expect("ticket token");
    let mut stream = observation.consume_bundle_ticket(token).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let manifest = serde_json::from_reader(archive.by_name("manifest.json")?)?;
    let summary = serde_json::from_reader(archive.by_name("interaction.json")?)?;
    Ok((manifest, summary))
}

#[tokio::test]
async fn platform_tool_remote_bytes_and_header_redaction_survive_bundle_export()
-> anyhow::Result<()> {
    use axum::{body::Bytes, response::IntoResponse};
    use base64::Engine;
    use std::io::Read;

    let (_directory, pool, observation) = fixture().await?;
    observation.set_debug_enabled(true);
    let run = admit(&observation, "platform-http", None);
    let request_body = br#"{"Authorization":"body-value-is-not-a-header"}"#;
    let response_body = [0, 255, 128, 13, 10];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/remote-tool", listener.local_addr()?);
    let app = axum::Router::new().route(
        "/remote-tool",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: Bytes| async move {
                assert_eq!(headers["authorization"], "Bearer request-header-secret");
                assert_eq!(body.as_ref(), request_body);
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [("authorization", "response-header-secret")],
                    Bytes::copy_from_slice(&response_body),
                )
                    .into_response()
            },
        ),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let web = stravia_web_access::LocalWeb::new(stravia_web_access::OutboundProxyMode::Direct)?;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(&url)
        .header("Authorization", "Bearer request-header-secret")
        .body(request_body.to_vec())?;
    let (response, body) =
        scope::scope(Some(run.clone()), web.http_client().fetch(request.into())).await?;
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(body, response_body);
    finish(&run, "completed", Some("platform-generation"));
    let ticket = ticket(&observation).await?;
    let token = ticket.download_url.rsplit('/').next().unwrap();
    let mut stream = observation.consume_bundle_ticket(token).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut records = Vec::<Value>::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.name()?.ends_with("events.jsonl") {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        assert!(!text.contains("request-header-secret"));
        assert!(!text.contains("response-header-secret"));
        for line in text.lines() {
            records.push(serde_json::from_str(line)?);
        }
    }
    let outgoing = records
        .iter()
        .filter(|record| record["direction"] == "upstream_request")
        .filter_map(|record| record["payload"].as_str())
        .collect::<String>();
    assert_eq!(outgoing.as_bytes(), request_body);
    let incoming = records
        .iter()
        .filter(|record| {
            record["direction"] == "upstream_response" && record["message_type"] == "body_chunk"
        })
        .flat_map(|record| {
            assert_eq!(record["payload_encoding"], "base64");
            base64::engine::general_purpose::STANDARD
                .decode(record["payload"].as_str().unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(incoming, response_body);
    assert!(records.iter().any(|record| {
        record["status_code"] == 503 && record["headers"]["authorization"] == "***"
    }));
    assert!(
        records
            .iter()
            .all(|record| record["run_id"] == "platform-http")
    );
    server.abort();
    observation.shutdown().await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn bundle_usage_revision_preserves_reported_fields_and_ticket_watermark() -> anyhow::Result<()>
{
    let (_directory, pool, observation) = fixture().await?;
    let run = admit(&observation, "usage-run", None);
    run.record(RunEvent::ModelTurnStarted {
        retention_detached: false,
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
        upstream_url: "http://127.0.0.1".into(),
        cost_basis: None,
    });
    run.record(RunEvent::UsageConfirmed {
        model_turn_id: "turn".into(),
        attempt_id: "attempt".into(),
        usage: ConfirmedUsage {
            input_tokens: Some(20),
            output_tokens: Some(7),
            cache_read_tokens: Some(4),
            cache_write_tokens: Some(0),
            ..Default::default()
        },
    });
    run.record(RunEvent::TargetAttemptFinished {
        model_turn_id: "turn".into(),
        attempt_id: "attempt".into(),
        status: "completed".into(),
        status_code: Some(200),
        error_code: None,
        error: None,
        duration_ms: 100,
        first_token_ms: Some(10),
        usage: None,
        amounts: None,
    });
    run.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: "turn".into(),
        status: "completed".into(),
    });
    finish(&run, "completed", None);
    let old = ticket(&observation).await?;
    run.record(RunEvent::UsageConfirmed {
        model_turn_id: "turn".into(),
        attempt_id: "attempt".into(),
        usage: ConfirmedUsage {
            input_tokens: Some(30),
            cache_read_tokens: Some(6),
            ..Default::default()
        },
    });
    let new = ticket(&observation).await?;
    assert!(new.through_sequence > old.through_sequence);
    for (issued, input, raw_input) in [(&old, 16, 20), (&new, 24, 30)] {
        let (manifest, summary) = download(&observation, issued).await?;
        assert_eq!(manifest["through_event_sequence"], issued.through_sequence);
        assert_eq!(summary["status"], "completed");
        assert_eq!(summary["usage"]["input_tokens"], input);
        assert_eq!(summary["usage"]["output_tokens"], 7);
        assert_eq!(summary["usage"]["cache_write_tokens"], 0);
        assert_eq!(summary["usage"]["reasoning_tokens"], Value::Null);
        let coverage = &summary["usage"]["coverage"];
        assert_eq!(coverage["attempt_count"], 1);
        assert_eq!(coverage["missing_input_tokens"], 0);
        assert_eq!(coverage["missing_output_tokens"], 0);
        assert_eq!(coverage["missing_cache_read_tokens"], 0);
        assert_eq!(coverage["missing_cache_write_tokens"], 0);
        assert_eq!(coverage["missing_reasoning_tokens"], 1);
        let events = summary["events"].as_array().expect("exported events");
        assert!(
            events
                .iter()
                .all(|event| event["sequence"].as_i64().unwrap() <= issued.through_sequence)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == "target_attempt_finished")
                .count(),
            if issued.through_sequence == old.through_sequence {
                1
            } else {
                2
            },
        );
        let terminal = events
            .iter()
            .rev()
            .find(|event| event["kind"] == "target_attempt_finished")
            .expect("attempt terminal");
        assert_eq!(terminal["payload"]["usage"]["input_tokens"], raw_input);
        assert_eq!(
            terminal["payload"]["usage"]["output_tokens"],
            if issued.through_sequence == old.through_sequence {
                Value::from(7)
            } else {
                Value::Null
            },
        );
    }
    drop(run);
    observation.shutdown().await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn bundle_net_input_clamps_attempts_and_preserves_unknown_operands() -> anyhow::Result<()> {
    let (_directory, pool, observation) = fixture().await?;
    let run = admit(&observation, "net-usage-run", None);
    run.record(RunEvent::ModelTurnStarted {
        retention_detached: false,
        model_turn_id: "net-turn".into(),
        route_id: "route".into(),
        model_display_name: None,
        estimated_input_tokens: None,
    });
    for (attempt, input, cache_read) in [
        ("reported", Some(12), Some(5)),
        ("overcached", Some(3), Some(9)),
        ("unknown-cache", Some(8), None),
        ("unknown-input", None, Some(0)),
    ] {
        run.record(RunEvent::TargetAttemptStarted {
            model_turn_id: "net-turn".into(),
            attempt_id: attempt.into(),
            target_id: "target".into(),
            provider_id: "provider".into(),
            provider_name: "Provider".into(),
            upstream_model: "model".into(),
            protocol: "responses".into(),
            upstream_url: "http://127.0.0.1".into(),
            cost_basis: None,
        });
        run.record(RunEvent::TargetAttemptFinished {
            model_turn_id: "net-turn".into(),
            attempt_id: attempt.into(),
            status: "completed".into(),
            status_code: Some(200),
            error_code: None,
            error: None,
            duration_ms: 100,
            first_token_ms: None,
            usage: Some(ConfirmedUsage {
                input_tokens: input,
                cache_read_tokens: cache_read,
                cache_write_tokens: Some(100),
                ..Default::default()
            }),
            amounts: None,
        });
    }
    run.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: "net-turn".into(),
        status: "completed".into(),
    });
    finish(&run, "completed", None);
    let issued = ticket(&observation).await?;
    let (_, summary) = download(&observation, &issued).await?;
    assert_eq!(summary["usage"]["input_tokens"], 7);
    assert_eq!(summary["usage"]["coverage"]["attempt_count"], 4);
    assert_eq!(summary["usage"]["coverage"]["missing_input_tokens"], 2);
    let raw_reported = summary["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| {
            event["kind"] == "target_attempt_finished"
                && event["payload"]["attempt_id"] == "reported"
        })
        .unwrap();
    assert_eq!(raw_reported["payload"]["usage"]["input_tokens"], 12);
    assert_eq!(raw_reported["payload"]["usage"]["cache_read_tokens"], 5);
    drop(run);
    observation.shutdown().await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn bundle_partial_public_tool_returns_keep_waiting_until_error_result_arrives()
-> anyhow::Result<()> {
    for sibling_status in ["completed", "failed"] {
        let (_directory, pool, observation) = fixture().await?;
        let parent = admit(&observation, "parent", None);
        parent.record(RunEvent::ClientToolHandoff {
            tool_id: "root-call".into(),
            name: "probe".into(),
            input: None,
        });
        finish(&parent, "waiting_client", Some("root-generation"));
        observation.flush().await?;
        let waiting = admit(&observation, "waiting", Some("root-generation"));
        for id in ["a", "b"] {
            waiting.record(RunEvent::ClientToolHandoff {
                tool_id: id.into(),
                name: "probe".into(),
                input: None,
            });
        }
        finish(&waiting, "waiting_client", Some("waiting-generation"));
        observation.flush().await?;
        let sibling = admit(&observation, "sibling", Some("root-generation"));
        sibling.record(RunEvent::ClientToolResult {
            tool_id: "a".into(),
            content: serde_json::json!("first result"),
            is_error: false,
        });
        finish(&sibling, sibling_status, Some("sibling-generation"));
        let partial = ticket(&observation).await?;
        sibling.record(RunEvent::ClientToolResult {
            tool_id: "b".into(),
            content: serde_json::json!("tool failed"),
            is_error: true,
        });
        let returned = ticket(&observation).await?;
        let (_, old) = download(&observation, &partial).await?;
        let (_, new) = download(&observation, &returned).await?;
        assert_eq!(old["status"], "waiting_client");
        assert_eq!(
            new["status"],
            if sibling_status == "completed" {
                "completed"
            } else {
                "interrupted"
            }
        );
        assert!(
            !old["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "client_tool_result"
                    && event["payload"]["tool_id"] == "b")
        );
        assert!(
            new["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "client_tool_result"
                    && event["payload"]["tool_id"] == "b"
                    && event["payload"]["is_error"] == true)
        );
        drop((parent, waiting, sibling));
        observation.shutdown().await;
        pool.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn bundle_detached_activity_outlives_child_delivery_at_ticket_watermark() -> anyhow::Result<()>
{
    let (_directory, pool, observation) = fixture().await?;
    let parent = admit(&observation, "parent", None);
    parent.record(RunEvent::ModelTurnStarted {
        retention_detached: false,
        model_turn_id: "turn".into(),
        route_id: "route".into(),
        model_display_name: None,
        estimated_input_tokens: None,
    });
    parent.record(RunEvent::PlatformToolStarted {
        model_turn_id: "turn".into(),
        tool_id: "background".into(),
        name: "probe".into(),
        input: None,
    });
    parent.record(RunEvent::ClientToolHandoff {
        tool_id: "client".into(),
        name: "probe".into(),
        input: None,
    });
    parent.record(RunEvent::ModelTurnFinished {
        retention_detached: false,
        model_turn_id: "turn".into(),
        status: "completed".into(),
    });
    let background = parent.clone();
    finish(&parent, "waiting_client", Some("root-generation"));
    drop(parent);
    observation.flush().await?;
    let child = admit(&observation, "child", Some("root-generation"));
    finish(&child, "completed", Some("child-generation"));
    let active = ticket(&observation).await?;
    background.record(RunEvent::PlatformToolFinished {
        model_turn_id: "turn".into(),
        tool_id: "background".into(),
        status: "completed".into(),
        duration_ms: 1,
        content: None,
    });
    let finished = ticket(&observation).await?;
    let (old_manifest, old) = download(&observation, &active).await?;
    let (new_manifest, new) = download(&observation, &finished).await?;
    assert_eq!(old_manifest["status"], "running");
    assert_eq!(old["status"], "running");
    assert_eq!(new_manifest["status"], "completed");
    assert_eq!(new["status"], "completed");
    assert!(
        !old["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "platform_tool_finished")
    );
    assert!(
        new["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "platform_tool_finished"
                && event["payload"]["tool_id"] == "background")
    );
    drop((background, child));
    observation.shutdown().await;
    pool.close().await;
    Ok(())
}
