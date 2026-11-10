use super::*;

const JOURNAL_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JournalScope {
    pub(crate) vendor_id: String,
    pub(crate) provider_id: String,
    pub(crate) data_epoch: i64,
    pub(crate) provider_fence: i64,
    pub(crate) principal: String,
    pub(crate) admission: String,
    pub(crate) lease: String,
    pub(crate) key: String,
    pub(crate) candidate: Option<crate::generation_chain::HistoryBoundaryCandidate>,
}

pub(super) struct MemoryBoundaryFact {
    pub(super) principal: String,
    pub(super) vendor_id: String,
    pub(super) provider_id: String,
    pub(super) provider_fence: i64,
    pub(super) owner_admission_id: String,
    pub(super) candidate: crate::generation_chain::HistoryBoundaryCandidate,
    pub(super) previous: Option<String>,
}

pub(super) struct MemoryJournalNode {
    pub(super) principal: String,
    pub(super) vendor_id: String,
    pub(super) provider_id: String,
    pub(super) data_epoch: i64,
    pub(super) previous: Option<String>,
    pub(super) text: String,
    pub(super) logical_size: u64,
}

enum JournalOperation<'a> {
    Load,
    Append(String, Option<&'a str>),
    Read(&'a str),
    Exchange(u64, Option<&'a [u8]>, Option<&'a str>),
}
enum JournalResult {
    Snapshot(AtomicStateSnapshot),
    Node(String),
    Document(String, Option<String>),
    Exchanged(bool),
}

/// Collect nodes unreachable from current cells, checkpoints, or live operation pins.
/// Caller holds the provider read lock and the memory data write lock.
pub(super) fn sweep_memory_atomic_journal(
    data: &mut MemoryPluginData,
    providers: &[Provider],
) -> u64 {
    let now = chrono::Utc::now().timestamp_millis();
    data.journal_pins
        .retain(|(principal, admission, lease, id), _| {
            let live = data
                .atomic_leases
                .get(&(principal.clone(), admission.clone(), lease.clone()))
                .is_some_and(|expiry| *expiry > now);
            let valid = data.journal_nodes.get(id).is_some_and(|node| {
                node.principal == *principal
                    && providers.iter().any(|p| {
                        p.id == node.provider_id
                            && p.vendor.as_deref() == Some(node.vendor_id.as_str())
                    })
                    && data
                        .installed
                        .get(&node.vendor_id)
                        .or_else(|| data.installed.get("base"))
                        .is_some_and(|plugin| plugin.data_epoch == node.data_epoch)
            });
            live && valid
        });
    let mut pending: Vec<String> = data
        .atomic_cells
        .values()
        .filter_map(|cell| cell.journal_head.clone())
        .collect();
    for checkpoint in data.atomic_checkpoints.values() {
        pending.extend(
            checkpoint
                .cells
                .iter()
                .filter_map(|(_, cell)| cell.journal_head.clone()),
        );
    }
    pending.extend(data.journal_pins.keys().map(|(_, _, _, id)| id.clone()));
    let mut reachable = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if reachable.insert(id.clone())
            && let Some(previous) = data
                .journal_nodes
                .get(&id)
                .and_then(|node| node.previous.as_ref())
        {
            pending.push(previous.clone());
        }
    }
    let dead: Vec<String> = data
        .journal_nodes
        .keys()
        .filter(|id| !reachable.contains(*id))
        .take(256)
        .cloned()
        .collect();
    let count = dead.len() as u64;
    for id in dead {
        data.journal_nodes.remove(&id);
    }
    count
}

/// Collect at most one bounded batch of facts not reachable from a lineage or checkpoint.
pub(super) fn sweep_memory_boundary_facts(data: &mut MemoryPluginData) -> u64 {
    let mut pending: Vec<String> = data
        .atomic_lineage_boundary_heads
        .values()
        .cloned()
        .collect();
    pending.extend(
        data.atomic_checkpoints
            .values()
            .filter_map(|checkpoint| checkpoint.boundary_head.clone()),
    );
    let mut reachable = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if reachable.insert(id.clone())
            && let Some(previous) = data
                .boundary_facts
                .get(&id)
                .and_then(|fact| fact.previous.as_ref())
        {
            pending.push(previous.clone());
        }
    }
    let dead: Vec<String> = data
        .boundary_facts
        .keys()
        .filter(|id| !reachable.contains(*id))
        .take(256)
        .cloned()
        .collect();
    let count = dead.len() as u64;
    for id in dead {
        if let Some(fact) = data.boundary_facts.remove(&id) {
            let key = (
                fact.principal,
                fact.vendor_id,
                fact.provider_id,
                fact.provider_fence,
                fact.owner_admission_id,
                fact.candidate.id,
            );
            if let Some(ids) = data.boundary_fact_candidates.get_mut(&key) {
                ids.remove(&id);
                if ids.is_empty() {
                    data.boundary_fact_candidates.remove(&key);
                }
            }
        }
    }
    count
}

impl PluginStore {
    pub(crate) async fn journal_load(
        &self,
        scope: &JournalScope,
    ) -> Result<AtomicStateSnapshot, PluginStorageError> {
        match self
            .journal_operation(scope, JournalOperation::Load)
            .await?
        {
            JournalResult::Snapshot(v) => Ok(v),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn journal_append(
        &self,
        scope: &JournalScope,
        text: String,
        previous: Option<&str>,
    ) -> Result<String, PluginStorageError> {
        match self
            .journal_operation(scope, JournalOperation::Append(text, previous))
            .await?
        {
            JournalResult::Node(v) => Ok(v),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn journal_read(
        &self,
        scope: &JournalScope,
        node_id: &str,
    ) -> Result<(String, Option<String>), PluginStorageError> {
        match self
            .journal_operation(scope, JournalOperation::Read(node_id))
            .await?
        {
            JournalResult::Document(text, previous) => Ok((text, previous)),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn journal_compare_exchange(
        &self,
        scope: &JournalScope,
        expected: u64,
        payload: Option<&[u8]>,
        head: Option<&str>,
    ) -> Result<bool, PluginStorageError> {
        match self
            .journal_operation(scope, JournalOperation::Exchange(expected, payload, head))
            .await?
        {
            JournalResult::Exchanged(v) => Ok(v),
            _ => unreachable!(),
        }
    }

    async fn journal_operation(
        &self,
        s: &JournalScope,
        op: JournalOperation<'_>,
    ) -> Result<JournalResult, PluginStorageError> {
        if s.key.len() > 128 {
            return Err(PluginStorageError::Limit);
        }
        if s.principal.is_empty() || s.admission.is_empty() || s.lease.is_empty() {
            return Err(PluginStorageError::StaleOperation);
        }
        match &op {
            JournalOperation::Append(text, _) if text.len() as u64 > JOURNAL_LIMIT => {
                return Err(PluginStorageError::Limit);
            }
            JournalOperation::Exchange(_, Some(payload), _)
                if payload.len() > MAX_PAYLOAD_BYTES =>
            {
                return Err(PluginStorageError::Limit);
            }
            _ => {}
        }
        macro_rules! sql {
            ($pool:expr, $begin:literal, $lock:literal, $owner:literal, $content:ident, $pg:expr, $revision:literal) => {{
                let mut connection = $pool.acquire_write().await.map_err(storage_error)?;
                let mut tx = connection.begin_with($begin).await.map_err(storage_error)?;
                if $pg {
                    sqlx::query("SELECT pg_advisory_xact_lock_shared(7342179085017261)").execute(&mut *tx).await.map_err(storage_error)?;
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('lineage:' || $1),hashtext($2))").bind(&s.principal).bind(&s.admission).execute(&mut *tx).await.map_err(storage_error)?;
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('provider:' || $1),hashtext($2))").bind(&s.vendor_id).bind(&s.provider_id).execute(&mut *tx).await.map_err(storage_error)?;
                }
                sqlx::query($lock).execute(&mut *tx).await.map_err(storage_error)?;
                let epoch: Option<i64> = sqlx::query_scalar($owner).bind(&s.provider_id).bind(&s.vendor_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                let lease: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_leases WHERE principal=$1 AND admission_id=$2 AND lease_id=$3 AND expires_at_ms>$4")
                    .bind(&s.principal).bind(&s.admission).bind(&s.lease).bind(chrono::Utc::now().timestamp_millis()).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                if epoch != Some(s.data_epoch) || lease.is_none() { return Err(PluginStorageError::StaleOperation); }
                let fence: Option<i64> = sqlx::query_scalar("SELECT generation FROM vendor_atomic_provider_fences WHERE provider_id=$1")
                    .bind(&s.provider_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                if fence != Some(s.provider_fence) { return Err(PluginStorageError::StaleOperation); }
                // Pin membership, not possession of an identifier, grants access to a node.
                macro_rules! authorized {
                    ($id:expr) => {{
                        let node: Option<(i64, Option<String>, i64)> = sqlx::query_as("SELECT n.logical_size,n.previous_id,n.content_id FROM vendor_atomic_journal_nodes n JOIN vendor_atomic_journal_pins p ON p.node_id=n.node_id WHERE p.principal=$1 AND p.admission_id=$2 AND p.lease_id=$3 AND n.node_id=$4 AND n.principal=$1 AND n.vendor_id=$5 AND n.provider_id=$6 AND n.data_epoch=$7")
                            .bind(&s.principal).bind(&s.admission).bind(&s.lease).bind($id).bind(&s.vendor_id).bind(&s.provider_id).bind(s.data_epoch).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                        node.ok_or(PluginStorageError::StaleOperation)?
                    }};
                }
                macro_rules! pin {
                    ($id:expr, $bytes:expr) => {{
                        sqlx::query("INSERT INTO vendor_atomic_journal_pins(principal,admission_id,lease_id,node_id,staged_bytes) VALUES($1,$2,$3,$4,$5) ON CONFLICT(principal,admission_id,lease_id,node_id) DO NOTHING")
                            .bind(&s.principal).bind(&s.admission).bind(&s.lease).bind($id).bind($bytes).execute(&mut *tx).await.map_err(storage_error)?;
                    }};
                }
                let result = match op {
                    JournalOperation::Load => {
                        let row: Option<(i64, Option<Vec<u8>>, Option<String>)> = sqlx::query_as("SELECT revision,payload,journal_head FROM vendor_atomic_cells WHERE vendor_id=$1 AND provider_id=$2 AND principal=$3 AND admission_id=$4 AND cell_key=$5")
                            .bind(&s.vendor_id).bind(&s.provider_id).bind(&s.principal).bind(&s.admission).bind(&s.key).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                        let snapshot = match row { Some((revision,payload,journal_head)) => AtomicStateSnapshot { revision: u64::try_from(revision).map_err(|_| PluginStorageError::Storage)?, payload, journal_head }, None => AtomicStateSnapshot::default() };
                        if let Some(id) = &snapshot.journal_head {
                            let valid: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_journal_nodes WHERE node_id=$1 AND principal=$2 AND vendor_id=$3 AND provider_id=$4 AND data_epoch=$5")
                                .bind(id).bind(&s.principal).bind(&s.vendor_id).bind(&s.provider_id).bind(s.data_epoch).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                            if valid.is_none() { return Err(PluginStorageError::Storage); }
                            pin!(id, 0_i64);
                        }
                        JournalResult::Snapshot(snapshot)
                    }
                    JournalOperation::Append(text, previous) => {
                        let previous_size = if let Some(id) = previous { let (size, _, _) = authorized!(id); u64::try_from(size).map_err(|_| PluginStorageError::Storage)? } else { 0 };
                        let size = text.len() as u64;
                        let logical = previous_size.checked_add(size).filter(|v| *v <= JOURNAL_LIMIT).ok_or(PluginStorageError::Limit)?;
                        let staged: i64 = sqlx::query_scalar("SELECT CAST(COALESCE(SUM(staged_bytes),0) AS BIGINT) FROM vendor_atomic_journal_pins WHERE principal=$1 AND admission_id=$2 AND lease_id=$3")
                            .bind(&s.principal).bind(&s.admission).bind(&s.lease).fetch_one(&mut *tx).await.map_err(storage_error)?;
                        let staged_size = size.max(1);
                        if u64::try_from(staged).map_err(|_| PluginStorageError::Storage)?.checked_add(staged_size).filter(|v| *v <= JOURNAL_LIMIT).is_none() { return Err(PluginStorageError::Limit); }
                        let content_scope = crate::content_store::$content::resolve_scope(&mut tx, &s.principal).await.map_err(|_| PluginStorageError::Storage)?;
                        let mut contents = [crate::content_store::Content::text(text)];
                        let ids = crate::content_store::$content::intern(&mut tx, content_scope, &mut contents).await.map_err(|_| PluginStorageError::Storage)?;
                        let id = uuid::Uuid::new_v4().to_string();
                        sqlx::query("INSERT INTO vendor_atomic_journal_nodes(node_id,principal,vendor_id,provider_id,data_epoch,previous_id,content_id,logical_size,document_size) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                            .bind(&id).bind(&s.principal).bind(&s.vendor_id).bind(&s.provider_id).bind(s.data_epoch).bind(previous).bind(ids[0]).bind(logical as i64).bind(size as i64).execute(&mut *tx).await.map_err(storage_error)?;
                        pin!(&id, staged_size as i64);
                        JournalResult::Node(id)
                    }
                    JournalOperation::Read(id) => {
                        let (_, previous, content_id) = authorized!(id);
                        let content_scope = crate::content_store::$content::scope_id(&mut tx, &s.principal).await.map_err(|_| PluginStorageError::Storage)?.ok_or(PluginStorageError::Storage)?;
                        let mut bodies = crate::content_store::$content::read(&mut tx, content_scope, &[content_id]).await.map_err(|_| PluginStorageError::Storage)?;
                        let body = bodies.pop().ok_or(PluginStorageError::Storage)?;
                        body.text().map_err(|_| PluginStorageError::Storage)?;
                        let text = String::from_utf8(body.bytes).map_err(|_| PluginStorageError::Storage)?;
                        if let Some(prev) = &previous {
                            let valid: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_journal_nodes WHERE node_id=$1 AND principal=$2 AND vendor_id=$3 AND provider_id=$4 AND data_epoch=$5")
                                .bind(prev).bind(&s.principal).bind(&s.vendor_id).bind(&s.provider_id).bind(s.data_epoch).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                            if valid.is_none() { return Err(PluginStorageError::Storage); }
                            pin!(prev, 0_i64);
                        }
                        JournalResult::Document(text, previous)
                    }
                    JournalOperation::Exchange(expected, payload, head) => {
                        let head_size = if let Some(id) = head { let (size, _, _) = authorized!(id); u64::try_from(size).map_err(|_| PluginStorageError::Storage)? } else { 0 };
                        let current: Option<(i64, Option<String>)> = sqlx::query_as("SELECT revision,journal_head FROM vendor_atomic_cells WHERE vendor_id=$1 AND provider_id=$2 AND principal=$3 AND admission_id=$4 AND cell_key=$5")
                            .bind(&s.vendor_id).bind(&s.provider_id).bind(&s.principal).bind(&s.admission).bind(&s.key).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                        let current_revision = current.as_ref().map_or(0, |(revision, _)| *revision);
                        if current_revision < 0 { return Err(PluginStorageError::Storage); }
                        if current_revision as u64 != expected { JournalResult::Exchanged(false) } else {
                            if current.is_none() {
                                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vendor_atomic_cells WHERE vendor_id=$1 AND provider_id=$2 AND principal=$3 AND admission_id=$4").bind(&s.vendor_id).bind(&s.provider_id).bind(&s.principal).bind(&s.admission).fetch_one(&mut *tx).await.map_err(storage_error)?;
                                if count >= 16 { return Err(PluginStorageError::Limit); }
                            }
                            let used: i64 = sqlx::query_scalar("SELECT CAST(COALESCE(SUM(n.logical_size),0) AS BIGINT) FROM vendor_atomic_cells c JOIN vendor_atomic_journal_nodes n ON n.node_id=c.journal_head WHERE c.principal=$1 AND c.admission_id=$2 AND NOT (c.vendor_id=$3 AND c.provider_id=$4 AND c.cell_key=$5)")
                                .bind(&s.principal).bind(&s.admission).bind(&s.vendor_id).bind(&s.provider_id).bind(&s.key).fetch_one(&mut *tx).await.map_err(storage_error)?;
                            if u64::try_from(used).map_err(|_| PluginStorageError::Storage)?.checked_add(head_size).filter(|v| *v <= JOURNAL_LIMIT).is_none() { return Err(PluginStorageError::Limit); }
                            let revision: i64 = sqlx::query_scalar($revision).fetch_one(&mut *tx).await.map_err(storage_error)?;
                            sqlx::query("INSERT INTO vendor_atomic_cells(vendor_id,provider_id,principal,admission_id,cell_key,revision,payload,journal_head) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(vendor_id,provider_id,principal,admission_id,cell_key) DO UPDATE SET revision=excluded.revision,payload=excluded.payload,journal_head=excluded.journal_head")
                                .bind(&s.vendor_id).bind(&s.provider_id).bind(&s.principal).bind(&s.admission).bind(&s.key).bind(revision).bind(payload).bind(head).execute(&mut *tx).await.map_err(storage_error)?;
                            if current.as_ref().and_then(|(_, old)| old.as_deref()) != head {
                                if let Some(candidate) = &s.candidate {
                                    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT boundary_head FROM vendor_atomic_lineages WHERE principal=$1 AND admission_id=$2")
                                        .bind(&s.principal).bind(&s.admission).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                                    let previous = row.ok_or(PluginStorageError::MissingLineage)?.0;
                                    let boundary_id = candidate.id.to_string();
                                    let existing: Option<i64> = sqlx::query_scalar("SELECT CAST(1 AS BIGINT) FROM vendor_atomic_boundary_facts WHERE principal=$1 AND vendor_id=$2 AND provider_id=$3 AND provider_fence=$4 AND owner_admission_id=$5 AND boundary_id=$6 LIMIT 1")
                                        .bind(&s.principal).bind(&s.vendor_id).bind(&s.provider_id).bind(s.provider_fence).bind(&s.admission).bind(&boundary_id).fetch_optional(&mut *tx).await.map_err(storage_error)?;
                                    if existing.is_none() {
                                        let item_gap = i64::try_from(candidate.source_item_gap).map_err(|_| PluginStorageError::Limit)?;
                                        let unit_gap = i64::try_from(candidate.source_unit_gap).map_err(|_| PluginStorageError::Limit)?;
                                        let fact_id = uuid::Uuid::new_v4().to_string();
                                        sqlx::query("INSERT INTO vendor_atomic_boundary_facts(node_id,principal,vendor_id,provider_id,provider_fence,owner_admission_id,previous_id,boundary_id,window_id,source_admission_id,source_item_gap,source_unit_gap,prefix_hash,controls_hash) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
                                            .bind(&fact_id).bind(&s.principal).bind(&s.vendor_id).bind(&s.provider_id).bind(s.provider_fence).bind(&s.admission).bind(previous.as_deref()).bind(&boundary_id).bind(candidate.window_id.to_string()).bind(candidate.source_admission_id.to_string()).bind(item_gap).bind(unit_gap).bind(candidate.source_prefix_hash.as_slice()).bind(candidate.source_controls_hash.as_slice()).execute(&mut *tx).await.map_err(storage_error)?;
                                        sqlx::query("UPDATE vendor_atomic_lineages SET boundary_head=$1 WHERE principal=$2 AND admission_id=$3").bind(&fact_id).bind(&s.principal).bind(&s.admission).execute(&mut *tx).await.map_err(storage_error)?;
                                    }
                                }
                            }
                            JournalResult::Exchanged(true)
                        }
                    }
                };
                tx.commit().await.map_err(storage_error)?;
                connection.finish().await.map_err(storage_error)?;
                Ok(result)
            }};
        }
        match &self.backend {
            PluginStoreBackend::Sqlite(pool) => sql!(
                pool,
                "BEGIN IMMEDIATE",
                "UPDATE vendor_atomic_revision SET revision=revision WHERE id=1",
                "SELECT v.data_epoch FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') WHERE p.id=$1 AND p.vendor=$2",
                sqlite,
                false,
                "UPDATE vendor_atomic_revision SET revision=revision+1 WHERE id=1 RETURNING revision"
            ),
            PluginStoreBackend::Postgres(pool) => sql!(
                pool,
                "BEGIN",
                "SELECT 1",
                "SELECT v.data_epoch FROM providers p JOIN vendor_plugins v ON v.vendor_id=COALESCE((SELECT vendor_id FROM vendor_plugins WHERE vendor_id=p.vendor),'base') WHERE p.id=$1 AND p.vendor=$2 FOR SHARE OF p,v",
                postgres,
                true,
                "SELECT nextval('vendor_atomic_revision_seq')"
            ),
            PluginStoreBackend::Memory(store) => {
                let providers = store.providers.read().await;
                let mut data = store.data.write().await;
                let owner = providers
                    .iter()
                    .find(|p| p.id == s.provider_id)
                    .and_then(|p| p.vendor.as_deref())
                    == Some(s.vendor_id.as_str());
                let epoch = data
                    .installed
                    .get(&s.vendor_id)
                    .or_else(|| data.installed.get("base"))
                    .is_some_and(|v| v.data_epoch == s.data_epoch);
                if !owner
                    || !epoch
                    || data.atomic_provider_fences.get(&s.provider_id) != Some(&s.provider_fence)
                    || !data
                        .atomic_leases
                        .get(&(s.principal.clone(), s.admission.clone(), s.lease.clone()))
                        .is_some_and(|expiry| *expiry > chrono::Utc::now().timestamp_millis())
                {
                    return Err(PluginStorageError::StaleOperation);
                }
                validate_memory_atomic_mirror(store, &s.principal, &s.admission, &s.lease).await?;
                let cell = (
                    s.vendor_id.clone(),
                    s.provider_id.clone(),
                    s.principal.clone(),
                    s.admission.clone(),
                    s.key.clone(),
                );
                macro_rules! pin_key {
                    ($id:expr) => {
                        (
                            s.principal.clone(),
                            s.admission.clone(),
                            s.lease.clone(),
                            $id.to_owned(),
                        )
                    };
                }
                macro_rules! node {
                    ($id:expr) => {{
                        let node = data
                            .journal_nodes
                            .get($id)
                            .ok_or(PluginStorageError::StaleOperation)?;
                        if node.principal != s.principal
                            || node.vendor_id != s.vendor_id
                            || node.provider_id != s.provider_id
                            || node.data_epoch != s.data_epoch
                        {
                            return Err(PluginStorageError::StaleOperation);
                        }
                        node
                    }};
                }
                macro_rules! authorized {
                    ($id:expr) => {{
                        if !data.journal_pins.contains_key(&pin_key!($id)) {
                            return Err(PluginStorageError::StaleOperation);
                        }
                        node!($id)
                    }};
                }
                match op {
                    JournalOperation::Load => {
                        let snapshot = data.atomic_cells.get(&cell).cloned().unwrap_or_default();
                        if let Some(id) = &snapshot.journal_head {
                            node!(id);
                            data.journal_pins.entry(pin_key!(id)).or_insert(0);
                        }
                        Ok(JournalResult::Snapshot(snapshot))
                    }
                    JournalOperation::Append(text, previous) => {
                        let previous_size = if let Some(id) = previous {
                            authorized!(id).logical_size
                        } else {
                            0
                        };
                        let size = text.len() as u64;
                        let logical_size = previous_size
                            .checked_add(size)
                            .filter(|v| *v <= JOURNAL_LIMIT)
                            .ok_or(PluginStorageError::Limit)?;
                        let staged = data
                            .journal_pins
                            .iter()
                            .filter(|((p, a, l, _), _)| {
                                p == &s.principal && a == &s.admission && l == &s.lease
                            })
                            .try_fold(0_u64, |n, (_, v)| n.checked_add(*v))
                            .ok_or(PluginStorageError::Limit)?;
                        if staged
                            .checked_add(size.max(1))
                            .filter(|v| *v <= JOURNAL_LIMIT)
                            .is_none()
                        {
                            return Err(PluginStorageError::Limit);
                        }
                        let id = uuid::Uuid::new_v4().to_string();
                        data.journal_nodes.insert(
                            id.clone(),
                            MemoryJournalNode {
                                principal: s.principal.clone(),
                                vendor_id: s.vendor_id.clone(),
                                provider_id: s.provider_id.clone(),
                                data_epoch: s.data_epoch,
                                previous: previous.map(str::to_owned),
                                text,
                                logical_size,
                            },
                        );
                        data.journal_pins.insert(pin_key!(&id), size.max(1));
                        Ok(JournalResult::Node(id))
                    }
                    JournalOperation::Read(id) => {
                        let node = authorized!(id);
                        let text = node.text.clone();
                        let previous = node.previous.clone();
                        if let Some(prev) = &previous {
                            node!(prev);
                            data.journal_pins.entry(pin_key!(prev)).or_insert(0);
                        }
                        Ok(JournalResult::Document(text, previous))
                    }
                    JournalOperation::Exchange(expected, payload, head) => {
                        let head_size = if let Some(id) = head {
                            authorized!(id).logical_size
                        } else {
                            0
                        };
                        let current = data.atomic_cells.get(&cell).cloned().unwrap_or_default();
                        if current.revision != expected {
                            return Ok(JournalResult::Exchanged(false));
                        }
                        if current.revision == 0
                            && data
                                .atomic_cells
                                .keys()
                                .filter(|(v, p, pr, a, _)| {
                                    v == &s.vendor_id
                                        && p == &s.provider_id
                                        && pr == &s.principal
                                        && a == &s.admission
                                })
                                .count()
                                >= 16
                        {
                            return Err(PluginStorageError::Limit);
                        }
                        let mut used = head_size;
                        for ((v, p, pr, a, k), snapshot) in &data.atomic_cells {
                            if pr == &s.principal
                                && a == &s.admission
                                && !(v == &s.vendor_id && p == &s.provider_id && k == &s.key)
                                && let Some(id) = &snapshot.journal_head
                            {
                                let node = data
                                    .journal_nodes
                                    .get(id)
                                    .ok_or(PluginStorageError::Storage)?;
                                used = used
                                    .checked_add(node.logical_size)
                                    .ok_or(PluginStorageError::Limit)?;
                            }
                        }
                        if used > JOURNAL_LIMIT {
                            return Err(PluginStorageError::Limit);
                        }
                        let mut new_fact = None;
                        if current.journal_head.as_deref() != head
                            && let Some(candidate) = &s.candidate
                        {
                            i64::try_from(candidate.source_item_gap)
                                .map_err(|_| PluginStorageError::Limit)?;
                            i64::try_from(candidate.source_unit_gap)
                                .map_err(|_| PluginStorageError::Limit)?;
                            let lineage = (s.principal.clone(), s.admission.clone());
                            if !data.atomic_lineages.contains_key(&lineage) {
                                return Err(PluginStorageError::MissingLineage);
                            }
                            let previous =
                                data.atomic_lineage_boundary_heads.get(&lineage).cloned();
                            let index_key = (
                                s.principal.clone(),
                                s.vendor_id.clone(),
                                s.provider_id.clone(),
                                s.provider_fence,
                                s.admission.clone(),
                                candidate.id,
                            );
                            let found = data.boundary_fact_candidates.contains_key(&index_key);
                            if !found {
                                let id = uuid::Uuid::new_v4().to_string();
                                let fact = MemoryBoundaryFact {
                                    principal: s.principal.clone(),
                                    vendor_id: s.vendor_id.clone(),
                                    provider_id: s.provider_id.clone(),
                                    provider_fence: s.provider_fence,
                                    owner_admission_id: s.admission.clone(),
                                    candidate: candidate.clone(),
                                    previous,
                                };
                                new_fact = Some((lineage, index_key, id, fact));
                            }
                        }
                        data.atomic_revision = data
                            .atomic_revision
                            .checked_add(1)
                            .ok_or(PluginStorageError::Storage)?;
                        let revision = data.atomic_revision as u64;
                        data.atomic_cells.insert(
                            cell,
                            AtomicStateSnapshot {
                                revision,
                                payload: payload.map(<[u8]>::to_vec),
                                journal_head: head.map(str::to_owned),
                            },
                        );
                        data.atomic_lineage_cells
                            .entry((s.principal.clone(), s.admission.clone()))
                            .or_default()
                            .insert((s.vendor_id.clone(), s.provider_id.clone(), s.key.clone()));
                        if let Some((lineage, index_key, id, fact)) = new_fact {
                            data.boundary_facts.insert(id.clone(), fact);
                            data.boundary_fact_candidates
                                .entry(index_key)
                                .or_default()
                                .insert(id.clone());
                            data.atomic_lineage_boundary_heads.insert(lineage, id);
                        }
                        Ok(JournalResult::Exchanged(true))
                    }
                }
            }
        }
    }
}
