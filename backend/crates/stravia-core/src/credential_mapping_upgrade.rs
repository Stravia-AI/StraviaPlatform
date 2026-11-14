//! 凭据映射升级：0053 清理过期无引用映射，0055 强制移除无归因及退役规则映射。

use std::{collections::HashSet, sync::Arc};

use serde_json::Value;

use crate::history_retention::HistoryRetention;
use crate::startup_progress::report;

const BATCH_SIZE: i64 = 512;
const PHASE: &str = "credential_expiry";
const LABEL: &str = "Removing expired credential mappings";

macro_rules! backend {
    ($name:ident, $conn:ty, $lock:expr) => {
        pub(crate) async fn $name(connection: &mut $conn) -> anyhow::Result<()> {
            report(PHASE, LABEL, 0, None);
            let setting: Option<String> = sqlx::query_scalar(
                "SELECT value FROM settings WHERE name = 'log_retention_days'",
            )
            .fetch_optional(&mut *connection)
            .await?;
            let retention = HistoryRetention::from_setting(setting.as_deref());
            let cutoff = chrono::Utc::now()
                .timestamp_millis()
                .saturating_sub(retention.window_millis())
                .saturating_sub(stravia_credential_protection::store::RENEWAL_GRANULARITY.as_millis() as i64);
            let (scanned, expired, retained_by_reference): (i64, i64, i64) = sqlx::query_as(
                "SELECT COUNT(*), \
                 COUNT(CASE WHEN m.created_at <= $1 THEN 1 END), \
                 COUNT(CASE WHEN m.created_at <= $1 AND EXISTS \
                   (SELECT 1 FROM turn_node_redaction_mappings r WHERE r.reference = m.reference) \
                   THEN 1 END) FROM reversible_redaction_mappings m",
            )
            .bind(cutoff)
            .fetch_one(&mut *connection)
            .await?;
            let total = (expired - retained_by_reference) as u64;
            let mut deleted = 0u64;
            let mut cursor: Option<String> = None;
            report(PHASE, LABEL, deleted, Some(total));
            loop {
                let mut tx = connection.begin().await?;
                if let Some(lock) = $lock {
                    sqlx::query(lock).execute(&mut *tx).await?;
                }
                // 按已有主键单向遍历全部映射，包括未过期和被引用的行；旧 schema 的
                // created_at 没有可用索引，不能让每个删除批次重新扫描全表。
                let last: Option<String> = match &cursor {
                    Some(previous) => sqlx::query_scalar(
                        "SELECT MAX(reference) FROM (SELECT reference FROM reversible_redaction_mappings WHERE reference > $1 ORDER BY reference LIMIT $2) batch",
                    ).bind(previous).bind(BATCH_SIZE).fetch_one(&mut *tx).await?,
                    None => sqlx::query_scalar(
                        "SELECT MAX(reference) FROM (SELECT reference FROM reversible_redaction_mappings ORDER BY reference LIMIT $1) batch",
                    ).bind(BATCH_SIZE).fetch_one(&mut *tx).await?,
                };
                let Some(last) = last else {
                    tx.commit().await?;
                    break;
                };
                let count = match &cursor {
                    Some(previous) => sqlx::query(
                        "DELETE FROM reversible_redaction_mappings WHERE reference > $1 AND reference <= $2 \
                         AND created_at <= $3 AND NOT EXISTS \
                         (SELECT 1 FROM turn_node_redaction_mappings r \
                          WHERE r.reference = reversible_redaction_mappings.reference)",
                    ).bind(previous).bind(&last).bind(cutoff).execute(&mut *tx).await?,
                    None => sqlx::query(
                        "DELETE FROM reversible_redaction_mappings WHERE reference <= $1 \
                         AND created_at <= $2 AND NOT EXISTS \
                         (SELECT 1 FROM turn_node_redaction_mappings r \
                          WHERE r.reference = reversible_redaction_mappings.reference)",
                    ).bind(&last).bind(cutoff).execute(&mut *tx).await?,
                }.rows_affected();
                tx.commit().await?;
                cursor = Some(last);
                deleted += count;
                if count != 0 {
                    report(PHASE, LABEL, deleted, Some(total));
                }
            }
            tracing::info!(scanned, expired, retained_by_reference, deleted, "Expired credential mapping upgrade completed");
            Ok(())
        }
    };
}

backend!(run_sqlite, sqlx::SqliteConnection<'_>, None::<&str>);
// 与历史回收一样在每个删除事务先取独占锁，避免与批量发布等写入方形成锁顺序环。
backend!(
    run_postgres,
    sqlx::PgConnection<'_>,
    Some(stravia_credential_protection::store::PG_GC_LOCK)
);

const CLEANUP_PHASE: &str = "credential_cleanup";
const CLEANUP_LABEL: &str = "Removing retired credential rules";
const EVENT_BATCH_BYTES: usize = 8 * 1024 * 1024;

enum CleanedEvent {
    Unchanged,
    Rewritten(Vec<u8>),
    Empty,
}

fn clean_event(bytes: &[u8], known: &HashSet<String>) -> anyhow::Result<CleanedEvent> {
    let mut value: Value = serde_json::from_slice(&crate::storage_codec::decode(bytes)?)?;
    let Some(discoveries) = value.get_mut("discoveries").and_then(Value::as_array_mut) else {
        return Ok(CleanedEvent::Unchanged);
    };
    let mut changed = false;
    discoveries.retain_mut(|discovery| {
        let Some(object) = discovery.as_object_mut() else {
            return true;
        };
        if let Some(Value::Array(ids)) = object.get_mut("rule_ids") {
            let before = ids.len();
            ids.retain(|id| id.as_str().is_some_and(|id| known.contains(id)));
            changed |= before != ids.len();
        }
        if let Some(Value::Array(locators)) = object.get_mut("locators") {
            let before = locators.len();
            locators.retain(|locator| {
                locator
                    .get("rule_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| known.contains(id))
            });
            if before != locators.len() {
                changed = true;
                let sources: std::collections::BTreeSet<&str> = locators
                    .iter()
                    .filter_map(|locator| locator.get("source_type").and_then(Value::as_str))
                    .collect();
                let sources = sources
                    .into_iter()
                    .map(|source| Value::String(source.to_owned()))
                    .collect();
                object.insert("source_types".into(), Value::Array(sources));
            }
        }
        let empty = object
            .get("rule_ids")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
            && object
                .get("locators")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
        changed |= empty;
        !empty
    });
    if !changed {
        return Ok(CleanedEvent::Unchanged);
    }
    if discoveries.is_empty() {
        return Ok(CleanedEvent::Empty);
    }
    Ok(CleanedEvent::Rewritten(crate::storage_codec::encode(
        serde_json::to_vec(&value)?.into(),
    )?))
}

macro_rules! legacy_backend {
    ($name:ident, $conn:ty, $trailer:literal) => {
        /// 0055 前移除失去检测器的归因；SQL 随后原子删除空归因映射及其引用。
        pub(crate) async fn $name(connection: &mut $conn) -> anyhow::Result<()> {
            report(CLEANUP_PHASE, CLEANUP_LABEL, 0, None);
            let mappings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reversible_redaction_mappings")
                .fetch_one(&mut *connection).await?;
            let events: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused') AND attrs IS NOT NULL",
            ).fetch_one(&mut *connection).await?;
            let disabled: Option<String> = sqlx::query_scalar(
                "SELECT value FROM settings WHERE name='reversible_redaction_disabled_rules'",
            ).fetch_optional(&mut *connection).await?;
            let total = (mappings + events) as u64 + u64::from(disabled.is_some());
            report(CLEANUP_PHASE, CLEANUP_LABEL, 0, Some(total));
            if total == 0 {
                return Ok(());
            }
            // 停用不是下线：内置目录与所有仍存在的自定义规则都保留，不看 enabled。
            let mut known: HashSet<String> = stravia_credential_protection::rule_catalog().await?
                .rules.into_iter().map(|rule| rule.id).collect();
            let custom: Vec<String> = sqlx::query_scalar("SELECT id FROM credential_custom_rules")
                .fetch_all(&mut *connection).await?;
            known.extend(custom);
            let known = Arc::new(known);
            let mut completed = 0u64;
            let mut after = String::new();
            loop {
                let mut tx = connection.begin().await?;
                let rows: Vec<(String, String)> = sqlx::query_as(
                    "SELECT reference,rule_ids FROM reversible_redaction_mappings WHERE reference>$1 ORDER BY reference LIMIT $2",
                ).bind(&after).bind(BATCH_SIZE).fetch_all(&mut *tx).await?;
                if rows.is_empty() {
                    tx.commit().await?;
                    break;
                }
                let count = rows.len();
                for (reference, encoded) in rows {
                    let mut ids: Vec<String> = serde_json::from_str(&encoded)?;
                    let before = ids.len();
                    ids.retain(|id| known.contains(id));
                    if before != ids.len() {
                        sqlx::query("UPDATE reversible_redaction_mappings SET rule_ids=$1 WHERE reference=$2")
                            .bind(serde_json::to_string(&ids)?).bind(&reference)
                            .execute(&mut *tx).await?;
                    }
                    after = reference;
                }
                tx.commit().await?;
                completed += count as u64;
                report(CLEANUP_PHASE, CLEANUP_LABEL, completed, Some(total));
            }
            let mut after = i64::MIN;
            loop {
                let mut tx = connection.begin().await?;
                let candidates: Vec<(i64, Vec<u8>)> = sqlx::query_as(
                    concat!("SELECT sequence,", $trailer, " FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused') AND attrs IS NOT NULL AND sequence>$1 ORDER BY sequence LIMIT $2"),
                ).bind(after).bind(BATCH_SIZE).fetch_all(&mut *tx).await?;
                if candidates.is_empty() {
                    tx.commit().await?;
                    break;
                }
                let mut bytes = 0usize;
                let mut through = after;
                for (sequence, trailer) in candidates {
                    let size = crate::storage_codec::decoded_size_hint(&trailer)?;
                    // 首个大事件独占一批；先读固定尾部，避免先拉取一整批大载荷。
                    if through != after && size > EVENT_BATCH_BYTES.saturating_sub(bytes) {
                        break;
                    }
                    bytes = bytes.saturating_add(size);
                    through = sequence;
                }
                let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
                    "SELECT sequence,attrs FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused') AND attrs IS NOT NULL AND sequence>$1 AND sequence<=$2 ORDER BY sequence",
                ).bind(after).bind(through).fetch_all(&mut *tx).await?;
                let count = rows.len();
                let rules = Arc::clone(&known);
                let cleaned = tokio::task::spawn_blocking(move || {
                    rows.into_iter().map(|(sequence, payload)| {
                        clean_event(&payload, &rules).map(|event| (sequence, event))
                    }).collect::<anyhow::Result<Vec<_>>>()
                }).await??;
                for (sequence, event) in cleaned {
                    match event {
                        CleanedEvent::Unchanged => {}
                        CleanedEvent::Rewritten(payload) => {
                            sqlx::query("UPDATE observation_events SET attrs=$1 WHERE sequence=$2")
                                .bind(payload).bind(sequence).execute(&mut *tx).await?;
                        }
                        CleanedEvent::Empty => {
                            sqlx::query("DELETE FROM observation_events WHERE sequence=$1")
                                .bind(sequence).execute(&mut *tx).await?;
                        }
                    }
                }
                tx.commit().await?;
                after = through;
                completed += count as u64;
                report(CLEANUP_PHASE, CLEANUP_LABEL, completed, Some(total));
            }
            if let Some(encoded) = disabled {
                let mut ids: Vec<String> = serde_json::from_str(&encoded)?;
                let before = ids.len();
                ids.retain(|id| !id.starts_with(stravia_credential_protection::CUSTOM_RULE_ID_PREFIX) && known.contains(id));
                if before != ids.len() {
                    sqlx::query("UPDATE settings SET value=$1,updated_at=CURRENT_TIMESTAMP WHERE name='reversible_redaction_disabled_rules'")
                        .bind(serde_json::to_string(&ids)?).execute(&mut *connection).await?;
                }
                completed += 1;
                report(CLEANUP_PHASE, CLEANUP_LABEL, completed, Some(total));
            }
            Ok(())
        }
    };
}

legacy_backend!(
    retire_sqlite,
    sqlx::SqliteConnection<'_>,
    "substr(attrs,-14)"
);
legacy_backend!(
    retire_postgres,
    sqlx::PgConnection<'_>,
    "substr(attrs,octet_length(attrs)-13)"
);

#[cfg(test)]
mod tests;
