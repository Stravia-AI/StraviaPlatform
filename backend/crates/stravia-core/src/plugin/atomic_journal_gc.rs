//! Bounded collection of immutable journal nodes and their shared content.
//!
//! Root publication and collection use the same atomic-state lock. A lease pin
//! is a root only while its operation and provider epoch remain valid.

use super::*;

const EXPIRED_PINS: &str = "DELETE FROM vendor_atomic_journal_pins WHERE (principal, admission_id, lease_id, node_id) IN (SELECT p.principal,p.admission_id,p.lease_id,p.node_id FROM vendor_atomic_journal_pins p JOIN vendor_atomic_journal_nodes n ON n.node_id=p.node_id WHERE NOT EXISTS (SELECT 1 FROM vendor_atomic_leases l WHERE l.principal=p.principal AND l.admission_id=p.admission_id AND l.lease_id=p.lease_id AND l.expires_at_ms>$1) OR NOT EXISTS (SELECT 1 FROM providers v JOIN vendor_plugins plugin ON plugin.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=v.vendor),'base') WHERE v.id=n.provider_id AND v.vendor=n.vendor_id AND plugin.data_epoch=n.data_epoch) LIMIT 256)";

const COLLECT: &str = "WITH RECURSIVE live(node_id) AS (SELECT journal_head FROM vendor_atomic_cells WHERE journal_head IS NOT NULL UNION SELECT journal_head FROM vendor_atomic_checkpoint_cells WHERE journal_head IS NOT NULL UNION SELECT p.node_id FROM vendor_atomic_journal_pins p JOIN vendor_atomic_leases l ON l.principal=p.principal AND l.admission_id=p.admission_id AND l.lease_id=p.lease_id JOIN vendor_atomic_journal_nodes n ON n.node_id=p.node_id JOIN providers v ON v.id=n.provider_id AND v.vendor=n.vendor_id JOIN vendor_plugins plugin ON plugin.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=v.vendor),'base') AND plugin.data_epoch=n.data_epoch WHERE l.expires_at_ms>$1 UNION SELECT n.previous_id FROM vendor_atomic_journal_nodes n JOIN live ON live.node_id=n.node_id WHERE n.previous_id IS NOT NULL) DELETE FROM vendor_atomic_journal_nodes WHERE node_id IN (SELECT n.node_id FROM vendor_atomic_journal_nodes n WHERE NOT EXISTS (SELECT 1 FROM live WHERE live.node_id=n.node_id) LIMIT 256) RETURNING content_id";

const COLLECT_FACTS: &str = "WITH RECURSIVE live(node_id) AS (SELECT boundary_head FROM vendor_atomic_lineages WHERE boundary_head IS NOT NULL UNION SELECT boundary_head FROM vendor_atomic_checkpoints WHERE boundary_head IS NOT NULL UNION SELECT n.previous_id FROM vendor_atomic_boundary_facts n JOIN live ON live.node_id=n.node_id WHERE n.previous_id IS NOT NULL) DELETE FROM vendor_atomic_boundary_facts WHERE node_id IN (SELECT n.node_id FROM vendor_atomic_boundary_facts n WHERE NOT EXISTS (SELECT 1 FROM live WHERE live.node_id=n.node_id) LIMIT 256)";

impl PluginStore {
    /// Remove one bounded batch; subsequent maintenance passes collect the rest.
    pub(crate) async fn sweep_atomic_journal(&self) -> anyhow::Result<u64> {
        macro_rules! sweep {
            ($pool:expr, $begin:literal, $lock:literal, $content:ident) => {{
                let mut connection = $pool.acquire_write().await?;
                let mut tx = connection.begin_with($begin).await?;
                sqlx::query($lock).execute(&mut *tx).await?;
                let now = chrono::Utc::now().timestamp_millis();
                sqlx::query(EXPIRED_PINS)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                let bodies: Vec<i64> = sqlx::query_scalar(COLLECT)
                    .bind(now)
                    .fetch_all(&mut *tx)
                    .await?;
                let facts = sqlx::query(COLLECT_FACTS)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                crate::content_store::$content::enqueue_unreferenced(&mut *tx, &bodies).await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(bodies.len() as u64 + facts)
            }};
        }
        match &self.backend {
            PluginStoreBackend::Sqlite(pool) => sweep!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                sqlite
            ),
            PluginStoreBackend::Postgres(pool) => sweep!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock(7342179085017261)",
                postgres
            ),
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let mut data = store.data.write().await;
                Ok(
                    atomic_journal::sweep_memory_atomic_journal(&mut data, &providers)
                        + atomic_journal::sweep_memory_boundary_facts(&mut data),
                )
            }
        }
    }
}
