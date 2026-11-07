use super::*;

impl PluginStore {
    /// Reclaim only unreferenced conversation-scoped state. SQLite serializes
    /// writers; PostgreSQL uses the exclusive atomic-state GC gate.
    pub(crate) async fn sweep_conversation_atomic_state(
        &self,
        cutoff_ms: i64,
        admissions: &dyn crate::storage::PromptAdmissionStore,
    ) -> anyhow::Result<u64> {
        self.collect_conversation_atomic_state(cutoff_ms, admissions)
            .await
    }

    pub(crate) async fn clear_conversation_atomic_state(
        &self,
        admissions: &dyn crate::storage::PromptAdmissionStore,
    ) -> anyhow::Result<u64> {
        macro_rules! clear_sql {
            ($pool:expr, $begin:literal, $lock:literal) => {{
                let mut connection = $pool.acquire_write().await?;
                let mut tx = connection.begin_with($begin).await?;
                sqlx::query($lock).execute(&mut *tx).await?;
                let changed =
                    sqlx::query("UPDATE prompt_admission_epoch SET epoch=epoch+1 WHERE id=1")
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                anyhow::ensure!(changed == 1, "prompt admission epoch unavailable");
                sqlx::query("DELETE FROM prompt_admissions")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM vendor_atomic_leases")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                connection.finish().await?;
            }};
        }
        match &self.backend {
            PluginStoreBackend::Sqlite(pool) => clear_sql!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1"
            ),
            PluginStoreBackend::Postgres(pool) => clear_sql!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock(7342179085017261)"
            ),
            PluginStoreBackend::Memory(store) => {
                let lock = admissions
                    .memory_history_lock()
                    .ok_or_else(|| anyhow::anyhow!("memory admission history lock unavailable"))?;
                let _guard = lock.write().await;
                admissions.clear().await?;
                let mut data = store.data.write().await;
                data.atomic_leases.clear();
                let pool = store
                    .history_pool
                    .read()
                    .map_err(|_| anyhow::anyhow!("atomic history pool lock poisoned"))?
                    .clone();
                if let Some(pool) = pool {
                    let mut connection = pool.acquire_write().await?;
                    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                    let changed =
                        sqlx::query("UPDATE prompt_admission_epoch SET epoch=epoch+1 WHERE id=1")
                            .execute(&mut *tx)
                            .await?
                            .rows_affected();
                    anyhow::ensure!(changed == 1, "prompt admission epoch unavailable");
                    sqlx::query("DELETE FROM vendor_atomic_leases")
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    connection.finish().await?;
                }
            }
        }
        self.collect_conversation_atomic_state(i64::MAX, admissions)
            .await
    }

    async fn collect_conversation_atomic_state(
        &self,
        cutoff_ms: i64,
        admissions: &dyn crate::storage::PromptAdmissionStore,
    ) -> anyhow::Result<u64> {
        let now = chrono::Utc::now().timestamp_millis();
        macro_rules! checkpoint_predicate {
            () => { "created_at_ms < $1 AND NOT EXISTS (SELECT 1 FROM prompt_admissions a WHERE a.principal=vendor_atomic_checkpoints.principal AND a.id=vendor_atomic_checkpoints.admission_id) AND NOT EXISTS (SELECT 1 FROM vendor_atomic_leases l WHERE l.principal=vendor_atomic_checkpoints.principal AND l.admission_id=vendor_atomic_checkpoints.admission_id AND l.expires_at_ms > $2) AND NOT EXISTS (SELECT 1 FROM turn_node_atomic_checkpoints r WHERE r.principal=vendor_atomic_checkpoints.principal AND r.checkpoint_id=vendor_atomic_checkpoints.checkpoint_id) AND NOT EXISTS (SELECT 1 FROM native_compaction_atomic_checkpoints r WHERE r.principal=vendor_atomic_checkpoints.principal AND r.checkpoint_id=vendor_atomic_checkpoints.checkpoint_id)" };
        }
        macro_rules! sweep {
            ($pool:expr, $begin:literal, $lock:literal, $postgres:expr) => {{
                let mut connection = $pool.acquire_write().await?;
                let mut tx = connection.begin_with($begin).await?;
                sqlx::query($lock)
                    .execute(&mut *tx).await?;
                let mut deleted = 0u64;
                if $postgres {
                    let candidates: Vec<(String, String)> = sqlx::query_as("SELECT principal, checkpoint_id FROM vendor_atomic_checkpoints WHERE created_at_ms < $1 FOR UPDATE SKIP LOCKED")
                        .bind(cutoff_ms).fetch_all(&mut *tx).await?;
                    for (principal, checkpoint) in candidates {
                        deleted += sqlx::query(concat!("DELETE FROM vendor_atomic_checkpoints WHERE ", checkpoint_predicate!(), " AND principal=$3 AND checkpoint_id=$4")).bind(cutoff_ms).bind(now).bind(principal).bind(checkpoint).execute(&mut *tx).await?.rows_affected();
                    }
                    let leases: Vec<(String, String, String)> = sqlx::query_as("SELECT principal, admission_id, lease_id FROM vendor_atomic_leases WHERE expires_at_ms <= $1 FOR UPDATE SKIP LOCKED")
                        .bind(now).fetch_all(&mut *tx).await?;
                    for (principal, admission, lease) in leases {
                        sqlx::query("DELETE FROM vendor_atomic_leases WHERE principal=$1 AND admission_id=$2 AND lease_id=$3 AND expires_at_ms <= $4")
                            .bind(principal).bind(admission).bind(lease).bind(now).execute(&mut *tx).await?;
                    }
                } else {
                    deleted = sqlx::query(concat!("DELETE FROM vendor_atomic_checkpoints WHERE ", checkpoint_predicate!())).bind(cutoff_ms).bind(now).execute(&mut *tx).await?.rows_affected();
                    sqlx::query("DELETE FROM vendor_atomic_leases WHERE expires_at_ms <= $1").bind(now).execute(&mut *tx).await?;
                }
                deleted = deleted.saturating_add(sqlx::query("DELETE FROM vendor_atomic_cells WHERE principal <> '' AND EXISTS (SELECT 1 FROM vendor_atomic_lineages g WHERE g.principal=vendor_atomic_cells.principal AND g.admission_id=vendor_atomic_cells.admission_id AND g.created_at_ms < $2) AND NOT EXISTS (SELECT 1 FROM prompt_admissions a WHERE a.principal=vendor_atomic_cells.principal AND a.id=vendor_atomic_cells.admission_id) AND NOT EXISTS (SELECT 1 FROM vendor_atomic_leases l WHERE l.principal=vendor_atomic_cells.principal AND l.admission_id=vendor_atomic_cells.admission_id AND l.expires_at_ms > $1)")
                    .bind(now).bind(cutoff_ms).execute(&mut *tx).await?.rows_affected());
                deleted = deleted.saturating_add(sqlx::query("DELETE FROM vendor_atomic_lineages WHERE created_at_ms < $2 AND NOT EXISTS (SELECT 1 FROM prompt_admissions a WHERE a.principal=vendor_atomic_lineages.principal AND a.id=vendor_atomic_lineages.admission_id) AND NOT EXISTS (SELECT 1 FROM vendor_atomic_leases l WHERE l.principal=vendor_atomic_lineages.principal AND l.admission_id=vendor_atomic_lineages.admission_id AND l.expires_at_ms > $1)")
                    .bind(now).bind(cutoff_ms).execute(&mut *tx).await?.rows_affected());
                tx.commit().await?;
                connection.finish().await?;
                Ok(deleted)
            }};
        }
        match &self.backend {
            PluginStoreBackend::Sqlite(pool) => sweep!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                false
            ),
            PluginStoreBackend::Postgres(pool) => sweep!(
                pool,
                "BEGIN",
                "SELECT pg_advisory_xact_lock(7342179085017261)",
                true
            ),
            PluginStoreBackend::Memory(store) => {
                // Serialize memory publication with collection. SQL turn and compaction
                // attachments take the revision lock, but never the memory lock.
                let mut data = store.data.write().await;
                let pool = store
                    .history_pool
                    .read()
                    .map_err(|_| anyhow::anyhow!("atomic history pool lock poisoned"))?
                    .clone();
                let mut live: BTreeSet<_> = data
                    .atomic_leases
                    .iter()
                    .filter(|(_, expires)| **expires > now)
                    .map(|((principal, admission, _), _)| (principal.clone(), admission.clone()))
                    .collect();
                for (principal, admission_id) in data.atomic_lineages.keys() {
                    if !live.contains(&(principal.clone(), admission_id.clone()))
                        && admissions.get(principal, admission_id).await?.is_some()
                    {
                        live.insert((principal.clone(), admission_id.clone()));
                    }
                }
                let mut dead: BTreeSet<_> = data
                    .atomic_lineages
                    .iter()
                    .filter(|(key, created)| **created < cutoff_ms && !live.contains(*key))
                    .map(|(key, _)| key.clone())
                    .collect();
                let candidates: Vec<_> = data
                    .atomic_checkpoints
                    .iter()
                    .filter(|((principal, _), checkpoint)| {
                        checkpoint.created_at_ms < cutoff_ms
                            && !live.contains(&(principal.clone(), checkpoint.admission_id.clone()))
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                let mut removed = Vec::new();
                if let Some(pool) = pool {
                    let mut connection = pool.acquire_write().await?;
                    let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
                    sqlx::query("UPDATE vendor_atomic_revision SET revision=revision WHERE id=1")
                        .execute(&mut *tx)
                        .await?;
                    // A cancelled or failed mirror may have committed an SQL lease
                    // after memory fencing failed. It remains publication authority
                    // until its actual expiry and must protect the snapshot too.
                    let sql_live: BTreeSet<(String, String)> = sqlx::query_as("SELECT DISTINCT principal, admission_id FROM vendor_atomic_leases WHERE expires_at_ms > $1")
                        .bind(now).fetch_all(&mut *tx).await?.into_iter().collect();
                    dead.retain(|key| !sql_live.contains(key));
                    for (principal, checkpoint) in candidates {
                        if data
                            .atomic_checkpoints
                            .get(&(principal.clone(), checkpoint.clone()))
                            .is_some_and(|snapshot| {
                                sql_live
                                    .contains(&(principal.clone(), snapshot.admission_id.clone()))
                            })
                        {
                            continue;
                        }
                        let referenced: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turn_node_atomic_checkpoints WHERE principal=$1 AND checkpoint_id=$2 UNION ALL SELECT 1 FROM native_compaction_atomic_checkpoints WHERE principal=$1 AND checkpoint_id=$2)")
                            .bind(&principal).bind(&checkpoint).fetch_one(&mut *tx).await?;
                        if referenced == 0 {
                            sqlx::query("DELETE FROM vendor_atomic_checkpoints WHERE principal=$1 AND checkpoint_id=$2")
                                .bind(&principal).bind(&checkpoint).execute(&mut *tx).await?;
                            removed.push((principal, checkpoint));
                        }
                    }
                    for (principal, admission) in &dead {
                        sqlx::query("DELETE FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2")
                            .bind(principal).bind(admission).execute(&mut *tx).await?;
                    }
                    sqlx::query("DELETE FROM vendor_atomic_leases WHERE expires_at_ms <= $1")
                        .bind(now)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    connection.finish().await?;
                } else {
                    removed = candidates;
                }
                let mut deleted = 0u64;
                for key in removed {
                    deleted += u64::from(data.atomic_checkpoints.remove(&key).is_some());
                }
                data.atomic_leases.retain(|_, expires| *expires > now);
                for (principal, admission) in dead {
                    if let Some(cells) = data
                        .atomic_lineage_cells
                        .remove(&(principal.clone(), admission.clone()))
                    {
                        for (vendor, provider, key) in cells {
                            deleted += u64::from(
                                data.atomic_cells
                                    .remove(&(
                                        vendor,
                                        provider,
                                        principal.clone(),
                                        admission.clone(),
                                        key,
                                    ))
                                    .is_some(),
                            );
                        }
                    }
                    data.atomic_lineage_boundary_heads
                        .remove(&(principal.clone(), admission.clone()));
                    data.atomic_lineage_epochs
                        .remove(&(principal.clone(), admission.clone()));
                    deleted += u64::from(
                        data.atomic_lineages
                            .remove(&(principal, admission))
                            .is_some(),
                    );
                }
                Ok(deleted)
            }
        }
    }
}
