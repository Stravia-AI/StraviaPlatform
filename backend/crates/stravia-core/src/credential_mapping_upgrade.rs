//! 0053 前按创建时间清理已过期且无历史引用的凭据映射。

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
