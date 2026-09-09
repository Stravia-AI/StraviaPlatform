use super::*;
use crate::compaction::{Compaction, CompactionRegistration, CompactionTarget};
use crate::interaction_observation::{CompactionMode, InteractionObservation, RunStart};
use crate::model_turn::{CompactionPublication, CompactionReceipt};
use std::sync::{Arc, atomic::AtomicBool};

#[tokio::test]
async fn delivered_native_state_survives_later_failure_but_unexposed_states_expire() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::migrations::migrate_sqlite(&pool).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let observation = InteractionObservation::new(
        Some(pool.clone()),
        None,
        directory.path().to_path_buf(),
        7,
        true,
    )
    .await;
    let observer = observation
        .observe_ingress(IngressStart {
            id: "native-delivery".into(),
            method: "POST".into(),
            path: "/v1/responses".into(),
            protocol: "open-responses/responses/2026-04-24".into(),
        })
        .admit(RunStart {
            id: "native-delivery".into(),
            principal: "owner".into(),
            api_key_id: None,
            api_key_name: None,
            generation_root_id: None,
            generation_parent_id: None,
            has_new_user: true,
            canonical_fingerprint: "native-delivery".into(),
            route_id: "local-route".into(),
            model_display_name: None,
            ingress_protocol: "open-responses/responses/2026-04-24".into(),
        });
    let compaction = Compaction::sqlite(pool.clone());
    let principal = stravia_runtime_contract::Principal::new("owner");
    let publications = crate::model_turn::CompactionPublications::default();
    let mut states = Vec::new();
    for name in ["delivered", "incomplete", "altered", "unexposed"] {
        let wire = serde_json::json!({
            "type": "compaction", "id": name, "encrypted_content": format!("state-{name}"),
            "provider_metadata": {"revision": 2},
        });
        let item = crate::protocol::codec::open_responses::decoder::decode_input_item(&wire)
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
        publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CompactionPublication {
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
    let terminal = RunTerminalContext {
        generation_node_id: None,
        generation_root_id: None,
        generation_committed: Arc::new(AtomicBool::new(false)),
        waiting_client: false,
        visible_text: Vec::new(),
        client_input: Arc::new(Vec::new()),
        client_output: Vec::new(),
        compaction: compaction.clone(),
        principal: principal.clone(),
        compaction_records: publications,
    };
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
    observation
        .get_interaction("absent", Default::default())
        .await
        .unwrap();
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
