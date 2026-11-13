use std::sync::Arc;

use futures::StreamExt;
use stravia_credential_protection::policy::RuleScopeSource;
use stravia_credential_protection::store::{DetectedSecret, MappingStore, SqlMappingStore};
use stravia_credential_protection::{CustomRuleInput, SqlCustomRuleStore};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::model_turn::CanonicalEvent;
use stravia_runtime_contract::protocol::ir::{AiRequest, AiStreamDelta};
use stravia_runtime_contract::redaction::RedactionTrace;

use super::ReversibleRedaction;
use crate::history_retention::HistoryRetention;
use crate::runtime_cache::RuntimeCache;

async fn protect(redaction: &ReversibleRedaction, owner: &Principal) -> String {
    let mut request = AiRequest::new("expiry-model", Vec::new());
    request.instructions = Some("stravia".into());
    let mut request = Arc::new(request);
    redaction.protect(owner, &mut request, None).await.unwrap();
    request.instructions.clone().unwrap()
}

#[tokio::test]
async fn expiry_stops_cached_replacement_but_preserves_restoration_until_redetection() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let store = Arc::new(SqlMappingStore::sqlite(pool.clone()));
    let owner = Principal::new("expiry-owner");
    // 无归因存量误收录的裸词不会再次被内置规则检出。
    let mapping = store
        .intern(&owner, &[DetectedSecret::from("stravia")])
        .await
        .unwrap()
        .mappings
        .remove(0);
    store
        .publish(&owner, &[mapping.reference.clone()])
        .await
        .unwrap();
    let stale = chrono::Utc::now().timestamp_millis() - 2 * 86_400_000;
    let mut tx = pool.begin_write().await.unwrap();
    sqlx::query(
        "UPDATE reversible_redaction_mappings SET last_detected_at = ? WHERE reference = ?",
    )
    .bind(stale)
    .bind(&mapping.reference)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let storage: crate::storage::DynStorage = Arc::new(crate::storage::MemoryStorage::new(
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ));
    storage
        .settings()
        .set(stravia_credential_protection::SETTING_KEY, "true")
        .await
        .unwrap();
    let retention = HistoryRetention::new(7);
    let custom = Arc::new(SqlCustomRuleStore::sqlite(pool.clone()));
    let redaction = ReversibleRedaction::new(
        storage,
        retention.clone(),
        store.clone(),
        custom.clone(),
        RuntimeCache::tinyufo(1024 * 1024),
    );
    assert!(
        protect(&redaction, &owner)
            .await
            .contains(&mapping.reference)
    );

    // 修改统一保留期立即作用于已缓存快照，不等待缓存 TTL 或后台扫描。
    retention.set_days(1);
    assert_eq!(protect(&redaction, &owner).await, "stravia");
    let mappings = redaction.mappings.active(&owner).await.unwrap();
    assert!(!redaction.rule_scope().await.unwrap().allows(&mappings[0]));
    let input = futures::stream::iter([Ok(CanonicalEvent::Delta(AiStreamDelta::TextDelta(
        mapping.reference.clone(),
    )))]);
    let mut output =
        redaction.restore_stream(Box::pin(input), mappings, RedactionTrace::default(), None);
    let mut restored = String::new();
    while let Some(event) = output.next().await {
        if let CanonicalEvent::Delta(AiStreamDelta::TextDelta(text)) = event.unwrap() {
            restored.push_str(&text);
        }
    }
    assert_eq!(restored, "stravia");
    assert_eq!(
        store.active(&owner).await.unwrap()[0].last_detected_at,
        stale
    );

    let rule = custom
        .create(
            serde_json::from_value::<CustomRuleInput>(serde_json::json!({
                "name": "Redetection", "spec": {"mode": "simple", "text": "stravia"}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        protect(&redaction, &owner)
            .await
            .contains(&mapping.reference)
    );
    custom.delete(&rule.id).await.unwrap();
    // 检出续期必须让旧缓存失效，否则撤下检测规则后又会把它当作过期裸词发送。
    assert!(
        protect(&redaction, &owner)
            .await
            .contains(&mapping.reference)
    );
    let renewed = redaction.mappings.active(&owner).await.unwrap();
    assert!(renewed[0].last_detected_at > stale);
    assert!(redaction.rule_scope().await.unwrap().allows(&renewed[0]));

    redaction
        .mappings
        .sweep(
            chrono::Utc::now().timestamp_millis() + 3 * 86_400_000,
            retention.window(),
        )
        .await
        .unwrap();
    assert_eq!(store.plaintext(&mapping.reference).await.unwrap(), None);
    pool.close().await;
}
