use super::*;

#[tokio::test]
async fn delivered_model_legs_keep_distinct_ordinals_after_late_item_ids() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let observation = InteractionObservation::new(
        Some(pool.clone()),
        None,
        directory.path().to_path_buf(),
        7,
        true,
        crate::generation_chain::test_chain().await,
        Some(std::sync::Arc::new(tokio::sync::Mutex::new(()))),
    )
    .await;
    let observer = observation
        .observe_ingress(IngressStart {
            id: "native-delivery".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "open-responses/responses/2026-04-24".into(),
        })
        .admit(
            RunStart {
                id: "native-delivery".into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "local-route".into(),
                model_display_name: None,
                ingress_protocol: "open-responses/responses/2026-04-24".into(),
            },
            AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                ),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    let terminal = RunTerminalContext::new(
        None,
        None,
        Vec::new(),
        Compaction::sqlite(pool.clone()),
        stravia_runtime_contract::Principal::new("owner"),
        crate::model_turn::CompactionPublications::default(),
    );
    use stravia_runtime_contract::protocol::ir::{AiItem, AiStreamDelta};
    for (leg, text) in ["first delivered", "second delivered"]
        .into_iter()
        .enumerate()
    {
        terminal.observe_visible_leg(leg);
        terminal.observe_visible_leg(leg);
        terminal.observe_visible_deltas(&observer, &[AiStreamDelta::TextDelta(text.into())]);
        terminal.observe_visible_deltas(
            &observer,
            &[AiStreamDelta::ItemDone {
                index: 0,
                item: AiItem::output_text(text).with_graph_metadata(
                    Some(format!("late-{text}")),
                    Some(stravia_runtime_contract::protocol::ir::AiItemStatus::Completed),
                    stravia_runtime_contract::protocol::ir::AiItemProvenance::Provider,
                    stravia_runtime_contract::protocol::ir::AiItemAudience::Client,
                ),
            }],
        );
    }
    terminal.finish_visible_items(&observer, false);
    observation.flush().await.unwrap();
    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT payload FROM observation_events WHERE kind = 'client_visible_content' ORDER BY sequence").fetch_all(&pool).await.unwrap();
    let payloads: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|bytes| {
            serde_json::from_slice(&crate::storage_codec::decode(&bytes).unwrap()).unwrap()
        })
        .collect();
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0]["text"], "first delivered");
    assert_eq!(payloads[1]["text"], "second delivered");
    assert_eq!(payloads[0]["block_id"], "native-delivery:item:0");
    assert_eq!(payloads[1]["block_id"], "native-delivery:item:1");
    assert_eq!(payloads[0]["complete"], false);
    assert_eq!(payloads[1]["complete"], false);
}

#[tokio::test]
async fn websocket_sent_partial_excludes_unsent_staged_output_and_preserves_item_identity() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let observation = InteractionObservation::new(
        Some(pool.clone()),
        None,
        directory.path().to_path_buf(),
        7,
        true,
        crate::generation_chain::test_chain().await,
        Some(std::sync::Arc::new(tokio::sync::Mutex::new(()))),
    )
    .await;
    let observer = observation
        .observe_ingress(IngressStart {
            id: "native-delivery".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "open-responses/responses/2026-04-24".into(),
        })
        .admit(
            RunStart {
                id: "native-delivery".into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "local-route".into(),
                model_display_name: None,
                ingress_protocol: "open-responses/responses/2026-04-24".into(),
            },
            AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                ),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    let terminal = RunTerminalContext::new(
        None,
        None,
        Vec::new(),
        Compaction::sqlite(pool.clone()),
        stravia_runtime_contract::Principal::new("owner"),
        crate::model_turn::CompactionPublications::default(),
    );
    let mut delivery = WebSocketRunDelivery {
        root: tracing::Span::none(),
        delivery_span: tracing::Span::none(),
        observer: observer.clone(),
        connection: None,
        terminal: terminal.clone(),
        stream_completion: None,
        delivery_completed_at: None,
        committed: false,
        finished: false,
        decoder:
            stravia_protocol_codec::codec::open_responses::parser::ResponsesStreamParser::default(),
    };
    let mut created =
        stravia_protocol_codec::codec::open_responses::formatter::ResponsesResponseFormatter
            .format_response(&stravia_runtime_contract::protocol::ir::AiResponse::new(
                "response-delivery",
                "model",
            ));
    created["status"] = "in_progress".into();
    delivery
        .sent_text(&serde_json::json!({"type":"response.created", "response":created}).to_string());
    for value in [
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"first","role":"assistant","status":"in_progress","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","output_index":0,"item_id":"first","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","output_index":0,"item_id":"first","content_index":0,"delta":"delivered partial"}),
        serde_json::json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"second","role":"assistant","status":"in_progress","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","output_index":1,"item_id":"second","content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","output_index":1,"item_id":"second","content_index":0,"delta":"second delivered"}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"first","role":"assistant","status":"completed","content":[{"type":"output_text","text":"delivered partial","annotations":[]}]}}),
    ] {
        delivery.sent_text(&value.to_string());
    }
    assert!(terminal.shared().visible_stream_started);
    terminal.shared().canonical_output = Some(vec![stravia_protocol_codec::codec::open_responses::decoder::decode_input_item(&serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"unsent staged suffix"}]})).unwrap().unwrap()]);
    delivery.finish("cancelled", Some("client_disconnected".into()));
    observation.flush().await.unwrap();
    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT payload FROM observation_events WHERE kind = 'client_visible_content' ORDER BY sequence").fetch_all(&pool).await.unwrap();
    let payloads: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|bytes| {
            serde_json::from_slice(&crate::storage_codec::decode(&bytes).unwrap()).unwrap()
        })
        .collect();
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0]["text"], "delivered partial");
    assert_eq!(payloads[1]["text"], "second delivered");
    assert_eq!(payloads[0]["block_id"], "native-delivery:item:0");
    assert_eq!(payloads[1]["block_id"], "native-delivery:item:1");
    assert_eq!(payloads[0]["complete"], false);
    assert_eq!(payloads[1]["complete"], false);
}

use crate::compaction::{Compaction, CompactionRegistration, CompactionTarget};
use crate::interaction_observation::{
    AdmissionFacts, CompactionMode, InteractionObservation, RunStart,
};
use crate::model_turn::{CompactionPublication, CompactionReceipt};

#[tokio::test]
async fn delivered_native_state_survives_later_failure_but_unexposed_states_expire() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool, None)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let observation = InteractionObservation::new(
        Some(pool.clone()),
        None,
        directory.path().to_path_buf(),
        7,
        true,
        crate::generation_chain::test_chain().await,
        Some(std::sync::Arc::new(tokio::sync::Mutex::new(()))),
    )
    .await;
    let observer = observation
        .observe_ingress(IngressStart {
            id: "native-delivery".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "open-responses/responses/2026-04-24".into(),
        })
        .admit(
            RunStart {
                id: "native-delivery".into(),
                principal: "owner".into(),
                api_key_id: None,
                api_key_name: None,
                route_id: "local-route".into(),
                model_display_name: None,
                ingress_protocol: "open-responses/responses/2026-04-24".into(),
            },
            AdmissionFacts {
                client_request: stravia_runtime_contract::protocol::ir::AiRequest::new(
                    "model",
                    Vec::new(),
                ),
                has_new_user: true,
                has_matching_pending_tool_result: false,
                generation_root_id: None,
                generation_parent_id: None,
            },
        );
    let compaction = Compaction::sqlite(pool.clone());
    let principal = stravia_runtime_contract::Principal::new("owner");
    let publications = crate::model_turn::CompactionPublications::default();
    let mut states = Vec::new();
    for name in ["delivered", "incomplete", "altered", "unexposed"] {
        let wire = serde_json::json!({
            "type": "compaction", "id": name, "encrypted_content": format!("state-{name}"),
            "provider_metadata": {"revision": 2},
        });
        let item = stravia_protocol_codec::codec::open_responses::decoder::decode_input_item(&wire)
            .unwrap()
            .unwrap();
        let record = compaction
            .register(
                &principal,
                CompactionRegistration {
                    source_generation_id: None,
                    source_record_ids: Vec::new(),
                    operation_id: name.into(),
                    target: CompactionTarget {
                        target_key: "local-target".into(),
                        namespace: "local-account".into(),
                        model: "local-model".into(),
                        protocol: "open-responses/responses/2026-04-24".into(),
                    },
                    window: vec![item.clone()],
                    state_items: vec![item.clone()],
                },
            )
            .await
            .unwrap();
        publications.lock().push(CompactionPublication {
            record_id: record.id,
            operation_id: name.into(),
            model_turn_id: "turn".into(),
            mode: CompactionMode::Inline,
            source_generation_id: None,
            state: item.clone(),
            receipt: CompactionReceipt::Pending,
        });
        states.push(item);
    }
    let terminal = RunTerminalContext::new(
        None,
        None,
        Vec::new(),
        compaction.clone(),
        principal.clone(),
        publications,
    );
    let native = |index: usize| {
        stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(&states[index])
            .unwrap()
    };
    // This is the shared receipt interface used only after an HTTP complete
    // frame is polled or a WebSocket text write has been acknowledged.
    let complete = serde_json::json!({"type": "response.output_item.done", "item": native(0)});
    let receipts = terminal.receive_native_items(&native_delivery_items(&complete), false);
    terminal
        .confirm_native_receipts(&observer, receipts)
        .unwrap()
        .await
        .unwrap();
    let mut partial = native(1);
    partial["encrypted_content"] = "partial-ciphertext".into();
    let incomplete = serde_json::json!({"type": "response.output_item.added", "item": partial});
    let mut altered = native(2);
    altered["provider_metadata"]["revision"] = 3.into();
    let altered = serde_json::json!({"type": "response.output_item.done", "item": altered});
    for event in [incomplete, altered] {
        let receipts = terminal.receive_native_items(&native_delivery_items(&event), false);
        if let Some(confirmation) = terminal.confirm_native_receipts(&observer, receipts) {
            confirmation.await.unwrap();
        }
    }
    terminal.finish_http_delivery(
        &observer,
        200,
        "delivery_failed",
        Some("upstream_eof".into()),
        None,
    );
    // Advance beyond pending retention, without sleeping or touching the record
    // through resolve (which would itself renew it).
    sqlx::query("UPDATE native_compactions SET expires_at = expires_at - $1")
        .bind(2_i64 * 60 * 60 * 1000)
        .execute(&pool)
        .await
        .unwrap();
    compaction.cleanup_expired().await.unwrap();
    let resumed = compaction
        .resolve(&principal, &states[..1])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.operation_id, "delivered");
    for state in &states[1..] {
        assert!(
            compaction
                .resolve(&principal, std::slice::from_ref(state))
                .await
                .unwrap()
                .is_none()
        );
    }
    observation.flush().await.unwrap();
    let forest = observation.query_forest(Default::default()).await.unwrap();
    let interaction = &forest.roots[0].interactions[0];
    let detail = observation
        .get_interaction(&interaction.id, Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.runs[0].status, "failed");
    assert!(detail.runs[0].generation_node_id.is_none());
}
