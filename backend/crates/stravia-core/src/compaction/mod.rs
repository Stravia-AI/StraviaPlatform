//! 原生压缩状态的持久来源。诊断投影不能充当续接事实源。

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::ProtocolExt;
use stravia_runtime_contract::protocol::ir::canonical;

mod sql;

#[cfg(test)]
mod retention_tests;

const PENDING_RETENTION: Duration = Duration::from_secs(60 * 60);
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_RESOLUTION_RECORDS: usize = 256;

pub(crate) struct NativeCompactionControls {
    pub trigger: bool,
    pub active_control: bool,
}

impl NativeCompactionControls {
    pub fn classify(request: &AiRequest) -> Self {
        let trigger = request.items.iter().any(AiItem::is_compaction_trigger);
        let control = match &request.ext {
            Some(ProtocolExt::OpenResponses(ext)) => ext.passthrough_body.get("context_management"),
            _ => None,
        };
        // Null and empty controls are inactive; an explicit trigger remains active.
        let active_control = control
            .is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty));
        Self {
            trigger,
            active_control,
        }
    }

    pub fn requested(&self) -> bool {
        self.trigger || self.active_control
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactionTarget {
    pub target_key: String,
    pub namespace: String,
    pub model: String,
    pub protocol: String,
}

pub(crate) struct CompactionRegistration {
    pub source_generation_id: Option<String>,
    pub source_record_ids: Vec<String>,
    pub operation_id: String,
    pub target: CompactionTarget,
    pub window: Vec<AiItem>,
    pub state_items: Vec<AiItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CompactionRecord {
    pub id: String,
    pub source_generation_id: Option<String>,
    pub source_record_ids: Vec<String>,
    pub operation_id: String,
    pub target: CompactionTarget,
    pub window: Vec<AiItem>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedCompaction {
    pub record_ids: Vec<String>,
    pub source_generation_id: Option<String>,
    pub operation_id: String,
    pub target: CompactionTarget,
    pub window: Vec<AiItem>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CompactionError {
    #[error("Native compaction evidence conflicts or is ambiguous.")]
    Conflict,
    #[error("Native compaction state is unavailable.")]
    Unavailable,
    #[error("Native compaction state registration failed.")]
    Storage,
    #[error("Native compaction state is invalid.")]
    Invalid,
}

impl CompactionError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Conflict => "compaction_conflict",
            Self::Unavailable => "compaction_unavailable",
            Self::Storage => "compaction_storage_failed",
            Self::Invalid => "invalid_compaction_state",
        }
    }
}

#[derive(Clone)]
pub(crate) struct Compaction {
    store: sql::Store,
}

impl Compaction {
    pub(crate) fn sqlite(pool: sqlx::SqlitePool) -> Self {
        Self {
            store: sql::Store::Sqlite(pool),
        }
    }

    pub(crate) fn postgres(pool: sqlx::PgPool) -> Self {
        Self {
            store: sql::Store::Postgres(pool),
        }
    }

    /// 原子登记完整状态后即可解析；交付确认只延长保留期，不开启解析资格。
    pub(crate) async fn register(
        &self,
        principal: &Principal,
        input: CompactionRegistration,
    ) -> Result<CompactionRecord, CompactionError> {
        if input.state_items.is_empty() || input.window.is_empty() {
            return Err(CompactionError::Invalid);
        }
        let mut states = Vec::with_capacity(input.state_items.len());
        for item in &input.state_items {
            let value = state_value(item).ok_or(CompactionError::Invalid)?;
            if !input
                .window
                .iter()
                .any(|window_item| state_value(window_item).as_ref() == Some(&value))
            {
                return Err(CompactionError::Invalid);
            }
            let fingerprint = state_fingerprint(&value)?;
            let identity = value.get("id").and_then(Value::as_str).map(str::to_owned);
            if !states
                .iter()
                .any(|state: &sql::State| state.fingerprint == fingerprint)
            {
                states.push(sql::State {
                    identity,
                    fingerprint,
                    payload: value,
                });
            }
        }
        let record = CompactionRecord {
            id: format!("compact_{}", uuid::Uuid::new_v4().simple()),
            source_generation_id: input.source_generation_id,
            source_record_ids: input.source_record_ids,
            operation_id: input.operation_id,
            target: input.target,
            window: input.window,
        };
        self.store
            .extend(
                principal,
                &record.source_record_ids,
                PENDING_RETENTION,
                false,
            )
            .await?;
        self.store
            .insert(principal, &record, &states, PENDING_RETENTION)
            .await?;
        Ok(record)
    }

    /// 精确校验 identity 与完整状态；从不按登记时间选择来源。
    pub(crate) async fn resolve(
        &self,
        principal: &Principal,
        items: &[AiItem],
    ) -> Result<Option<ResolvedCompaction>, CompactionError> {
        let mut selected: Vec<CompactionRecord> = Vec::new();
        for item in items {
            let Some(value) = state_value(item) else {
                continue;
            };
            let fingerprint = state_fingerprint(&value)?;
            let identity = value.get("id").and_then(Value::as_str);
            let matches = self.store.lookup(principal, identity, &fingerprint).await?;
            if matches.len() > MAX_RESOLUTION_RECORDS {
                return Err(CompactionError::Conflict);
            }
            let mut exact = Vec::new();
            for candidate in matches {
                if candidate.state != value {
                    return Err(CompactionError::Conflict);
                }
                let record = candidate.record;
                if !exact
                    .iter()
                    .any(|existing: &CompactionRecord| existing.id == record.id)
                {
                    exact.push(record);
                }
            }
            if exact.len() > 1 {
                // 同一状态重复产生于同一来源可以重复登记；不同来源保留歧义。
                let first = &exact[0];
                if exact[1..]
                    .iter()
                    .any(|record| !same_boundary(first, record))
                {
                    return Err(CompactionError::Conflict);
                }
            }
            selected.extend(exact);
            if selected.len() > MAX_RESOLUTION_RECORDS {
                return Err(CompactionError::Conflict);
            }
        }
        if selected.is_empty() {
            return Ok(None);
        }
        let ids: BTreeSet<String> = selected.iter().map(|record| record.id.clone()).collect();
        let mut all = selected.clone();
        let mut visited: HashSet<String> = ids.iter().cloned().collect();
        let mut index = 0;
        while index < all.len() {
            for predecessor in all[index].source_record_ids.clone() {
                if visited.insert(predecessor.clone()) {
                    if visited.len() > MAX_RESOLUTION_RECORDS {
                        return Err(CompactionError::Conflict);
                    }
                    all.push(
                        self.store
                            .get(principal, &predecessor)
                            .await?
                            .ok_or(CompactionError::Unavailable)?,
                    );
                }
            }
            index += 1;
        }
        let mut tip: Option<&CompactionRecord> = None;
        for candidate in &selected {
            let mut ancestors = HashSet::new();
            collect_ancestors(&candidate.id, &all, &mut ancestors);
            if selected
                .iter()
                .all(|other| ancestors.contains(&other.id) || same_boundary(candidate, other))
            {
                if let Some(previous) = tip {
                    if !same_boundary(previous, candidate) {
                        return Err(CompactionError::Conflict);
                    }
                } else {
                    tip = Some(candidate);
                }
            }
        }
        let tip = tip.ok_or(CompactionError::Conflict)?;
        if selected.iter().any(|record| record.target != tip.target) {
            return Err(CompactionError::Conflict);
        }
        let record_ids = ids.into_iter().collect::<Vec<_>>();
        self.store
            .extend(principal, &record_ids, RETENTION, false)
            .await?;
        Ok(Some(ResolvedCompaction {
            record_ids,
            source_generation_id: tip.source_generation_id.clone(),
            operation_id: tip.operation_id.clone(),
            target: tip.target.clone(),
            window: tip.window.clone(),
        }))
    }

    pub(crate) async fn confirm_delivery(
        &self,
        principal: &Principal,
        ids: &[String],
    ) -> Result<(), CompactionError> {
        self.store.extend(principal, ids, RETENTION, true).await
    }

    pub(crate) async fn extend_retention(
        &self,
        principal: &Principal,
        ids: &[String],
    ) -> Result<(), CompactionError> {
        self.store.extend(principal, ids, RETENTION, false).await
    }

    pub(crate) async fn cleanup_expired(&self) -> Result<(), CompactionError> {
        self.store.cleanup().await
    }
}

fn same_boundary(left: &CompactionRecord, right: &CompactionRecord) -> bool {
    (left.source_generation_id.is_some()
        || !left.source_record_ids.is_empty()
        || left.operation_id == right.operation_id)
        && left.source_generation_id == right.source_generation_id
        && left.source_record_ids == right.source_record_ids
        && left.target == right.target
        && canonical::history_items_equal(&left.window, &right.window)
}

fn collect_ancestors(id: &str, records: &[CompactionRecord], found: &mut HashSet<String>) {
    if !found.insert(id.to_owned()) {
        return;
    }
    if let Some(record) = records.iter().find(|record| record.id == id) {
        for predecessor in &record.source_record_ids {
            collect_ancestors(predecessor, records, found);
        }
    }
}

fn state_value(item: &AiItem) -> Option<Value> {
    if !item.is_compaction() {
        return None;
    }
    stravia_runtime_contract::protocol::ir::canonical::native_compaction_item(item)
}

fn state_fingerprint(value: &Value) -> Result<String, CompactionError> {
    // serde_json::Map 的稳定键顺序只消除 JSON object 表示差异，不改写数组或密文。
    let bytes = serde_json::to_vec(value).map_err(|_| CompactionError::Invalid)?;
    Ok(canonical::hash_hex(&canonical::hash_bytes(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AiItem {
        let wire =
            serde_json::json!({"type": "compaction", "encrypted_content": "local-opaque-state"});
        crate::protocol::codec::open_responses::decoder::decode_input_item(&wire)
            .expect("native state")
            .expect("item")
    }

    async fn store() -> Compaction {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::migrations::migrate_sqlite(&pool).await.unwrap();
        Compaction::sqlite(pool)
    }

    fn registration(operation: &str) -> CompactionRegistration {
        CompactionRegistration {
            source_generation_id: None,
            source_record_ids: Vec::new(),
            operation_id: operation.into(),
            target: CompactionTarget {
                target_key: "local-target".into(),
                namespace: "local-account-generation".into(),
                model: "local-model".into(),
                protocol: "open-responses/responses/2026-04-24".into(),
            },
            window: vec![state()],
            state_items: vec![state()],
        }
    }

    #[tokio::test]
    async fn canonical_state_changes_cannot_reuse_stale_wire_snapshot_evidence() {
        let store = store().await;
        let owner = Principal::new("owner");
        let wire = serde_json::json!({
            "type": "compaction", "id": "hook-state", "encrypted_content": "original"
        });
        let mut item = crate::protocol::codec::open_responses::decoder::decode_input_item(&wire)
            .unwrap()
            .unwrap();
        let mut input = registration("before-canonical-replacement");
        input.window = vec![item.clone()];
        input.state_items = vec![item.clone()];
        store.register(&owner, input).await.unwrap();
        let stravia_runtime_contract::protocol::ir::MessageContent::Blocks(blocks) =
            &mut item.content
        else {
            panic!("native state must be typed");
        };
        let stravia_runtime_contract::protocol::ir::ContentBlock::Compaction { encrypted_content } =
            &mut blocks[0]
        else {
            panic!("native compaction block");
        };
        *encrypted_content = "changed-by-canonical-hook".into();
        assert!(matches!(
            store.resolve(&owner, &[item]).await,
            Err(CompactionError::Conflict)
        ));
    }

    #[tokio::test]
    async fn independent_unknown_sources_cannot_overwrite_one_state() {
        let store = store().await;
        let owner = Principal::new("owner");
        store
            .register(&owner, registration("operation-a"))
            .await
            .unwrap();
        let pending = store.resolve(&owner, &[state()]).await.unwrap().unwrap();
        assert_eq!(pending.operation_id, "operation-a");
        assert!(
            store
                .resolve(&Principal::new("other"), &[state()])
                .await
                .unwrap()
                .is_none()
        );
        store
            .register(&owner, registration("operation-b"))
            .await
            .unwrap();
        assert!(matches!(
            store.resolve(&owner, &[state()]).await,
            Err(CompactionError::Conflict)
        ));
    }

    #[tokio::test]
    async fn pending_state_survives_reopening_before_delivery_confirmation() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::migrations::migrate_sqlite(&pool).await.unwrap();
        let owner = Principal::new("owner");
        let record = Compaction::sqlite(pool.clone())
            .register(&owner, registration("operation"))
            .await
            .unwrap();
        let reopened = Compaction::sqlite(pool);
        let resolved = reopened.resolve(&owner, &[state()]).await.unwrap().unwrap();
        assert_eq!(resolved.record_ids, vec![record.id.clone()]);
        reopened
            .confirm_delivery(&owner, &[record.id])
            .await
            .unwrap();
        assert!(
            reopened
                .resolve(&owner, &[state()])
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn registration_failure_cannot_create_a_resolvable_state() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::migrations::migrate_sqlite(&pool).await.unwrap();
        sqlx::query("CREATE TRIGGER deny_native_state BEFORE INSERT ON native_compaction_states BEGIN SELECT RAISE(FAIL, 'injected failure'); END")
            .execute(&pool).await.unwrap();
        let store = Compaction::sqlite(pool);
        let owner = Principal::new("owner");
        assert!(matches!(
            store
                .register(&owner, registration("failed-operation"))
                .await,
            Err(CompactionError::Storage)
        ));
        assert!(store.resolve(&owner, &[state()]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn immediate_replay_and_delivery_confirmation_do_not_compete_for_state() {
        let directory = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("compaction.db"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .unwrap();
        crate::migrations::migrate_sqlite(&pool).await.unwrap();
        let store = Compaction::sqlite(pool);
        let owner = Principal::new("owner");
        let record = store
            .register(&owner, registration("operation"))
            .await
            .unwrap();
        let replays = (0..16).map(|_| async {
            let input = [state()];
            let replay = store.resolve(&owner, &input);
            let delivery = store.confirm_delivery(&owner, std::slice::from_ref(&record.id));
            let (resolved, delivered) = futures::join!(replay, delivery);
            delivered.unwrap();
            assert_eq!(
                resolved.unwrap().unwrap().record_ids,
                vec![record.id.clone()]
            );
        });
        futures::future::join_all(replays).await;
    }
}
