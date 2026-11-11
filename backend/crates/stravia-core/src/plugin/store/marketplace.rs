use std::collections::{BTreeMap, HashMap};

use super::{PluginStorageError, PluginStore, PluginStoreBackend};
use crate::plugin::marketplace::{CachedCatalog, MarketError, PluginSourceSummary};
use anyhow::{Context, Result};

#[derive(Default)]
pub(super) struct MemoryMarketplace {
    pub(super) sources: BTreeMap<String, PluginSourceSummary>,
    catalogs: HashMap<String, Vec<CachedCatalog>>,
    incomplete: HashMap<String, bool>,
    pub(super) seen: HashMap<(String, String, String), (String, bool)>,
    next_generation: i64,
    generations: HashMap<String, i64>,
}

fn ensure_current(source: &PluginSourceSummary, revision: i64) -> Result<()> {
    anyhow::ensure!(
        source.trust_revision == revision,
        PluginStorageError::Changed
    );
    Ok(())
}

impl PluginStore {
    pub async fn list_sources(&self) -> Result<Vec<PluginSourceSummary>> {
        macro_rules! load {
            ($pool:expr) => {{
                let rows: Vec<String> =
                    sqlx::query_scalar("SELECT source_json FROM vendor_plugin_sources ORDER BY id")
                        .fetch_all($pool)
                        .await?;
                rows.into_iter()
                    .map(|row| {
                        serde_json::from_str(&row).context("invalid persisted plugin source")
                    })
                    .collect()
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => Ok(store
                .data
                .read()
                .await
                .marketplace
                .sources
                .values()
                .cloned()
                .collect()),
            PluginStoreBackend::Sqlite(pool) => load!(pool),
            PluginStoreBackend::Postgres(pool) => load!(pool),
        }
    }

    pub async fn get_source(&self, id: &str) -> Result<Option<PluginSourceSummary>> {
        macro_rules! load {
            ($pool:expr) => {{
                let row: Option<String> =
                    sqlx::query_scalar("SELECT source_json FROM vendor_plugin_sources WHERE id=$1")
                        .bind(id)
                        .fetch_optional($pool)
                        .await?;
                row.map(|s| serde_json::from_str(&s).context("invalid persisted plugin source"))
                    .transpose()
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                Ok(store.data.read().await.marketplace.sources.get(id).cloned())
            }
            PluginStoreBackend::Sqlite(pool) => load!(pool),
            PluginStoreBackend::Postgres(pool) => load!(pool),
        }
    }

    pub async fn put_source(
        &self,
        source: &PluginSourceSummary,
        expected: Option<&PluginSourceSummary>,
    ) -> Result<()> {
        anyhow::ensure!(
            expected.is_none_or(|old| old.id == source.id),
            "source identity mismatch"
        );
        if let Some(old) = expected
            && old
                .error
                .as_ref()
                .is_some_and(|error| error.code == "key_changed")
        {
            anyhow::ensure!(
                source.trust_revision > old.trust_revision
                    || source
                        .error
                        .as_ref()
                        .is_some_and(|error| error.code == "key_changed"),
                "key change requires explicit retrust"
            );
        }
        let invalidate = expected.is_some_and(|old| {
            old.trust_revision != source.trust_revision
                || old.public_key != source.public_key
                || old.fingerprint != source.fingerprint
        });
        let mut stored = source.clone();
        if invalidate {
            stored.succeeded_at = None;
        }
        let json = serde_json::to_string(&stored)?;
        let expected_json = expected.map(serde_json::to_string).transpose()?;
        macro_rules! put { ($pool:expr, $begin:literal) => {{
            let mut connection = $pool.acquire_write().await?;
            let mut tx = connection.begin_with($begin).await?;
            let result = if let Some(old) = &expected_json {
                sqlx::query("UPDATE vendor_plugin_sources SET source_json=$2, repository_id=$3, repository=$4, fingerprint=$5, trust_revision=$6, enabled=$7 WHERE id=$1 AND source_json=$8")
                    .bind(&source.id).bind(&json).bind(source.repository_id.to_string()).bind(&source.repository).bind(&source.fingerprint).bind(source.trust_revision).bind(source.enabled).bind(old).execute(&mut *tx).await?
            } else {
                sqlx::query("INSERT INTO vendor_plugin_sources (id,source_json,repository_id,repository,fingerprint,trust_revision,enabled) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(id) DO NOTHING")
                    .bind(&source.id).bind(&json).bind(source.repository_id.to_string()).bind(&source.repository).bind(&source.fingerprint).bind(source.trust_revision).bind(source.enabled).execute(&mut *tx).await?
            };
            anyhow::ensure!(result.rows_affected() == 1, PluginStorageError::Changed);
            if invalidate {
                sqlx::query("DELETE FROM vendor_plugin_catalogs WHERE source_id=$1").bind(&source.id).execute(&mut *tx).await?;
                sqlx::query("UPDATE vendor_plugin_sources SET incomplete=$2 WHERE id=$1").bind(&source.id).bind(false).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            connection.finish().await?;
            Ok(())
        }} }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let current = data
                    .marketplace
                    .sources
                    .get(&source.id)
                    .map(serde_json::to_string)
                    .transpose()?;
                anyhow::ensure!(current == expected_json, PluginStorageError::Changed);
                anyhow::ensure!(
                    !data
                        .marketplace
                        .sources
                        .values()
                        .any(|other| other.id != source.id
                            && other.repository_id == source.repository_id),
                    "repository already exists"
                );
                if invalidate {
                    data.marketplace.catalogs.remove(&source.id);
                    data.marketplace.incomplete.remove(&source.id);
                }
                data.marketplace.sources.insert(source.id.clone(), stored);
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => put!(pool, "BEGIN IMMEDIATE"),
            PluginStoreBackend::Postgres(pool) => put!(pool, "BEGIN"),
        }
    }

    pub async fn delete_source(&self, id: &str, expected: &PluginSourceSummary) -> Result<()> {
        anyhow::ensure!(
            id == expected.id && !expected.official,
            "cannot delete plugin source"
        );
        let json = serde_json::to_string(expected)?;
        macro_rules! delete {
            ($pool:expr, $begin:literal) => {{
                let mut connection = $pool.acquire_write().await?;
                let mut tx = connection.begin_with($begin).await?;
                let result =
                    sqlx::query("DELETE FROM vendor_plugin_sources WHERE id=$1 AND source_json=$2")
                        .bind(id)
                        .bind(&json)
                        .execute(&mut *tx)
                        .await?;
                anyhow::ensure!(result.rows_affected() == 1, PluginStorageError::Changed);
                tx.commit().await?;
                connection.finish().await?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let current = data
                    .marketplace
                    .sources
                    .get(id)
                    .map(serde_json::to_string)
                    .transpose()?;
                anyhow::ensure!(
                    current.as_deref() == Some(json.as_str()),
                    PluginStorageError::Changed
                );
                data.marketplace.sources.remove(id);
                data.marketplace.catalogs.remove(id);
                data.marketplace.incomplete.remove(id);
                data.marketplace.generations.remove(id);
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => delete!(pool, "BEGIN IMMEDIATE"),
            PluginStoreBackend::Postgres(pool) => delete!(pool, "BEGIN"),
        }
    }

    pub async fn list_catalogs(&self, source_id: &str) -> Result<Vec<CachedCatalog>> {
        macro_rules! load { ($pool:expr) => {{
            let rows: Vec<String> = sqlx::query_scalar("SELECT catalog_json FROM vendor_plugin_catalogs WHERE source_id=$1 ORDER BY release_id").bind(source_id).fetch_all($pool).await?;
            rows.into_iter().map(|row| serde_json::from_str(&row).context("invalid persisted catalog")).collect()
        }} }
        match &self.backend {
            PluginStoreBackend::Memory(store) => Ok(store
                .data
                .read()
                .await
                .marketplace
                .catalogs
                .get(source_id)
                .cloned()
                .unwrap_or_default()),
            PluginStoreBackend::Sqlite(pool) => load!(pool),
            PluginStoreBackend::Postgres(pool) => load!(pool),
        }
    }

    pub async fn begin_source_check(&self, expected: &PluginSourceSummary) -> Result<i64> {
        fn check(source: &PluginSourceSummary, expected: &PluginSourceSummary) -> Result<()> {
            ensure_current(source, expected.trust_revision)?;
            anyhow::ensure!(
                source.enabled == expected.enabled
                    && source.include_prerelease == expected.include_prerelease,
                PluginStorageError::Changed
            );
            Ok(())
        }
        macro_rules! begin { ($pool:expr, $begin:literal, $lock:literal) => {{
            let mut connection = $pool.acquire_write().await?;
            let mut tx = connection.begin_with($begin).await?;
            let row: Option<String> = sqlx::query_scalar($lock).bind(&expected.id).fetch_optional(&mut *tx).await?;
            let source: PluginSourceSummary = serde_json::from_str(&row.ok_or(PluginStorageError::Changed)?)?;
            check(&source, expected)?;
            let generation: i64 = sqlx::query_scalar("UPDATE vendor_plugin_check_sequence SET generation=generation+1 WHERE id=1 RETURNING generation").fetch_one(&mut *tx).await?;
            sqlx::query("UPDATE vendor_plugin_sources SET check_generation=$2 WHERE id=$1").bind(&expected.id).bind(generation).execute(&mut *tx).await?;
            tx.commit().await?; connection.finish().await?; Ok(generation)
        }} }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let market = &mut data.marketplace;
                check(
                    market
                        .sources
                        .get(&expected.id)
                        .ok_or(PluginStorageError::Changed)?,
                    expected,
                )?;
                market.next_generation = market
                    .next_generation
                    .checked_add(1)
                    .context("refresh generation exhausted")?;
                let generation = market.next_generation;
                market.generations.insert(expected.id.clone(), generation);
                Ok(generation)
            }
            PluginStoreBackend::Sqlite(pool) => begin!(
                pool,
                "BEGIN IMMEDIATE",
                "SELECT source_json, check_generation FROM vendor_plugin_sources WHERE id=$1"
            ),
            PluginStoreBackend::Postgres(pool) => begin!(
                pool,
                "BEGIN",
                "SELECT source_json, check_generation FROM vendor_plugin_sources WHERE id=$1 FOR UPDATE"
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn replace_catalogs(
        &self,
        source_id: &str,
        expected: &PluginSourceSummary,
        generation: i64,
        checked_at: i64,
        catalogs: &[CachedCatalog],
        incomplete: bool,
        mirrored: bool,
    ) -> Result<()> {
        anyhow::ensure!(expected.id == source_id, "source identity mismatch");
        // Keep at most ten pages of 100 releases, each bounded like the fetcher.
        anyhow::ensure!(
            catalogs.len() <= 1000
                && catalogs.iter().all(|c| c.source_id == source_id
                    && c.catalog.len() <= 256 * 1024
                    && c.signature.len() <= 4096),
            "catalog cache limit or source mismatch"
        );
        let encoded: Vec<_> = catalogs
            .iter()
            .map(|c| Ok((c.release_id.to_string(), serde_json::to_string(c)?)))
            .collect::<Result<_>>()?;
        macro_rules! replace { ($pool:expr, $begin:literal, $lock:literal) => {{
            let mut connection = $pool.acquire_write().await?;
            let mut tx = connection.begin_with($begin).await?;
            let row: Option<(String, i64)> = sqlx::query_as($lock).bind(source_id).fetch_optional(&mut *tx).await?;
            let (json, current_generation) = row.ok_or(PluginStorageError::Changed)?;
            anyhow::ensure!(generation == current_generation, PluginStorageError::Changed);
            let mut source: PluginSourceSummary = serde_json::from_str(&json)?;
            ensure_current(&source, expected.trust_revision)?;
            anyhow::ensure!(source.enabled == expected.enabled && source.include_prerelease == expected.include_prerelease, PluginStorageError::Changed);
            source.checked_at = Some(checked_at); source.succeeded_at = Some(checked_at); source.mirrored = Some(mirrored); if !source.error.as_ref().is_some_and(|error| error.code == "key_changed") { source.error = None; }
            sqlx::query("UPDATE vendor_plugin_sources SET source_json=$2, incomplete=$3 WHERE id=$1").bind(source_id).bind(serde_json::to_string(&source)?).bind(incomplete).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM vendor_plugin_catalogs WHERE source_id=$1").bind(source_id).execute(&mut *tx).await?;
            for (id, json) in &encoded { sqlx::query("INSERT INTO vendor_plugin_catalogs (source_id,release_id,catalog_json) VALUES ($1,$2,$3)").bind(source_id).bind(id).bind(json).execute(&mut *tx).await?; }
            tx.commit().await?; connection.finish().await?; Ok(())
        }} }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let market = &mut data.marketplace;
                anyhow::ensure!(
                    market.generations.get(source_id) == Some(&generation),
                    PluginStorageError::Changed
                );
                let source = market
                    .sources
                    .get_mut(source_id)
                    .ok_or(PluginStorageError::Changed)?;
                ensure_current(source, expected.trust_revision)?;
                anyhow::ensure!(
                    source.enabled == expected.enabled
                        && source.include_prerelease == expected.include_prerelease,
                    PluginStorageError::Changed
                );
                source.checked_at = Some(checked_at);
                source.succeeded_at = Some(checked_at);
                source.mirrored = Some(mirrored);
                if !source
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "key_changed")
                {
                    source.error = None;
                }
                market
                    .catalogs
                    .insert(source_id.to_owned(), catalogs.to_vec());
                market.incomplete.insert(source_id.to_owned(), incomplete);
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => replace!(
                pool,
                "BEGIN IMMEDIATE",
                "SELECT source_json, check_generation FROM vendor_plugin_sources WHERE id=$1"
            ),
            PluginStoreBackend::Postgres(pool) => replace!(
                pool,
                "BEGIN",
                "SELECT source_json, check_generation FROM vendor_plugin_sources WHERE id=$1 FOR UPDATE"
            ),
        }
    }

    pub async fn record_source_check(
        &self,
        source_id: &str,
        expected_trust_revision: i64,
        generation: i64,
        checked_at: i64,
        mirrored: Option<bool>,
        error: Option<&MarketError>,
    ) -> Result<()> {
        fn update(
            source: &mut PluginSourceSummary,
            checked_at: i64,
            mirrored: Option<bool>,
            error: Option<&MarketError>,
        ) {
            source.checked_at = Some(checked_at);
            if let Some(mirrored) = mirrored {
                source.mirrored = Some(mirrored);
            }
            if error.is_none() {
                source.succeeded_at = Some(checked_at);
            }
            if !source
                .error
                .as_ref()
                .is_some_and(|error| error.code == "key_changed")
            {
                source.error = error.cloned();
            }
        }
        macro_rules! record {
            ($pool:expr, $begin:literal, $lock:literal) => {{
                let mut connection = $pool.acquire_write().await?;
                let mut tx = connection.begin_with($begin).await?;
                let row: Option<(String, i64)> = sqlx::query_as($lock)
                    .bind(source_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                let (json, current_generation) = row.ok_or(PluginStorageError::Changed)?;
                anyhow::ensure!(
                    generation == current_generation
                        || error.is_some_and(|error| error.code == "key_changed"),
                    PluginStorageError::Changed
                );
                let mut source: PluginSourceSummary = serde_json::from_str(&json)?;
                ensure_current(&source, expected_trust_revision)?;
                update(&mut source, checked_at, mirrored, error);
                sqlx::query("UPDATE vendor_plugin_sources SET source_json=$2 WHERE id=$1")
                    .bind(source_id)
                    .bind(serde_json::to_string(&source)?)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                anyhow::ensure!(
                    data.marketplace.generations.get(source_id) == Some(&generation)
                        || error.is_some_and(|error| error.code == "key_changed"),
                    PluginStorageError::Changed
                );
                let source = data
                    .marketplace
                    .sources
                    .get_mut(source_id)
                    .ok_or(PluginStorageError::Changed)?;
                ensure_current(source, expected_trust_revision)?;
                update(source, checked_at, mirrored, error);
                Ok(())
            }
            PluginStoreBackend::Sqlite(pool) => record!(
                pool,
                "BEGIN IMMEDIATE",
                "SELECT source_json, check_generation FROM vendor_plugin_sources WHERE id=$1"
            ),
            PluginStoreBackend::Postgres(pool) => record!(
                pool,
                "BEGIN",
                "SELECT source_json, check_generation FROM vendor_plugin_sources WHERE id=$1 FOR UPDATE"
            ),
        }
    }

    pub async fn mark_source_key_changed(
        &self,
        source_id: &str,
        expected_revision: i64,
        old_fingerprint: &str,
        new_fingerprint: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            old_fingerprint != new_fingerprint,
            "signing key did not change"
        );
        fn mark(
            source: &mut PluginSourceSummary,
            revision: i64,
            old: &str,
            new: &str,
        ) -> Result<()> {
            ensure_current(source, revision)?;
            anyhow::ensure!(source.fingerprint == old, PluginStorageError::Changed);
            source.error = Some(MarketError::new(
                "key_changed",
                format!("Publisher key changed: {old} -> {new}"),
            ));
            Ok(())
        }
        macro_rules! mark_sql {
            ($pool:expr, $begin:literal, $lock:literal) => {{
                let mut connection = $pool.acquire_write().await?;
                let mut tx = connection.begin_with($begin).await?;
                let json: Option<String> = sqlx::query_scalar($lock)
                    .bind(source_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                let mut source: PluginSourceSummary =
                    serde_json::from_str(&json.ok_or(PluginStorageError::Changed)?)?;
                mark(
                    &mut source,
                    expected_revision,
                    old_fingerprint,
                    new_fingerprint,
                )?;
                sqlx::query("UPDATE vendor_plugin_sources SET source_json=$2 WHERE id=$1")
                    .bind(source_id)
                    .bind(serde_json::to_string(&source)?)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                connection.finish().await?;
                Ok(())
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                let source = data
                    .marketplace
                    .sources
                    .get_mut(source_id)
                    .ok_or(PluginStorageError::Changed)?;
                mark(source, expected_revision, old_fingerprint, new_fingerprint)
            }
            PluginStoreBackend::Sqlite(pool) => mark_sql!(
                pool,
                "BEGIN IMMEDIATE",
                "SELECT source_json FROM vendor_plugin_sources WHERE id=$1"
            ),
            PluginStoreBackend::Postgres(pool) => mark_sql!(
                pool,
                "BEGIN",
                "SELECT source_json FROM vendor_plugin_sources WHERE id=$1 FOR UPDATE"
            ),
        }
    }

    pub async fn record_seen_digest(
        &self,
        source_id: &str,
        vendor_id: &str,
        version: &str,
        digest: &str,
    ) -> Result<bool> {
        macro_rules! record { ($pool:expr, $begin:literal, $lock:literal) => {{
            let mut connection = $pool.acquire_write().await?;
            let mut tx = connection.begin_with($begin).await?;
            let source: Option<String> = sqlx::query_scalar($lock).bind(source_id).fetch_optional(&mut *tx).await?;
            source.ok_or(PluginStorageError::Changed)?;
            let conflict: bool = sqlx::query_scalar("INSERT INTO vendor_plugin_seen_digests (source_id,vendor_id,version,digest) VALUES ($1,$2,$3,$4) ON CONFLICT(source_id,vendor_id,version) DO UPDATE SET conflict = vendor_plugin_seen_digests.conflict OR vendor_plugin_seen_digests.digest <> excluded.digest RETURNING conflict").bind(source_id).bind(vendor_id).bind(version).bind(digest).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            connection.finish().await?;
            Ok(conflict)
        }} }
        match &self.backend {
            PluginStoreBackend::Memory(store) => {
                let mut data = store.data.write().await;
                anyhow::ensure!(
                    data.marketplace.sources.contains_key(source_id),
                    "plugin source not found"
                );
                let existing = data
                    .marketplace
                    .seen
                    .entry((
                        source_id.to_owned(),
                        vendor_id.to_owned(),
                        version.to_owned(),
                    ))
                    .or_insert_with(|| (digest.to_owned(), false));
                existing.1 |= existing.0 != digest;
                Ok(existing.1)
            }
            PluginStoreBackend::Sqlite(pool) => record!(
                pool,
                "BEGIN IMMEDIATE",
                "SELECT id FROM vendor_plugin_sources WHERE id=$1"
            ),
            PluginStoreBackend::Postgres(pool) => record!(
                pool,
                "BEGIN",
                "SELECT id FROM vendor_plugin_sources WHERE id=$1 FOR UPDATE"
            ),
        }
    }

    pub async fn list_seen_digests(
        &self,
        source_id: &str,
    ) -> Result<HashMap<(String, String), (String, bool)>> {
        macro_rules! load { ($pool:expr) => {{
            let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
                "SELECT vendor_id, version, digest, conflict FROM vendor_plugin_seen_digests WHERE source_id=$1"
            ).bind(source_id).fetch_all($pool).await?;
            Ok(rows.into_iter().map(|(vendor, version, digest, conflict)| {
                ((vendor, version), (digest, conflict))
            }).collect())
        }} }
        match &self.backend {
            PluginStoreBackend::Memory(store) => Ok(store
                .data
                .read()
                .await
                .marketplace
                .seen
                .iter()
                .filter(|((source, _, _), _)| source == source_id)
                .map(|((_, vendor, version), value)| {
                    ((vendor.clone(), version.clone()), value.clone())
                })
                .collect()),
            PluginStoreBackend::Sqlite(pool) => load!(pool),
            PluginStoreBackend::Postgres(pool) => load!(pool),
        }
    }

    pub async fn source_incomplete(&self, source_id: &str) -> Result<bool> {
        macro_rules! load {
            ($pool:expr) => {{
                Ok(sqlx::query_scalar::<_, bool>(
                    "SELECT incomplete FROM vendor_plugin_sources WHERE id=$1",
                )
                .bind(source_id)
                .fetch_optional($pool)
                .await?
                .unwrap_or(false))
            }};
        }
        match &self.backend {
            PluginStoreBackend::Memory(store) => Ok(*store
                .data
                .read()
                .await
                .marketplace
                .incomplete
                .get(source_id)
                .unwrap_or(&false)),
            PluginStoreBackend::Sqlite(pool) => load!(pool),
            PluginStoreBackend::Postgres(pool) => load!(pool),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::store::InstalledPlugin;
    use crate::plugin::types::PluginInstallOrigin;
    use bytes::Bytes;

    fn source() -> PluginSourceSummary {
        PluginSourceSummary {
            id: "source".into(),
            repository_id: 42,
            repository: "owner/repo".into(),
            url: "https://github.com/owner/repo".into(),
            official: false,
            public_key: "key".into(),
            fingerprint: "fingerprint".into(),
            trust_revision: 1,
            enabled: true,
            include_prerelease: false,
            mirrored: None,
            checked_at: None,
            succeeded_at: None,
            error: None,
        }
    }

    async fn cache_and_ledger_regressions(store: &PluginStore) -> Result<()> {
        let old = source();
        store.put_source(&old, None).await?;
        let mut other = old.clone();
        other.id = "other".into();
        other.repository_id = 43;
        store.put_source(&other, None).await?;
        store
            .record_seen_digest("source", "vendor", "1", "first")
            .await?;
        store
            .record_seen_digest("source", "vendor", "1", "second")
            .await?;
        store
            .record_seen_digest("other", "vendor", "1", "other")
            .await?;
        let ledger = store.list_seen_digests("source").await?;
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.get(&("vendor".into(), "1".into())),
            Some(&("first".into(), true))
        );
        assert!(store.list_seen_digests("missing").await?.is_empty());
        let generation = store.begin_source_check(&old).await?;
        let catalog = CachedCatalog {
            source_id: old.id.clone(),
            release_id: 1,
            release_tag: "v1".into(),
            release_url: "url".into(),
            prerelease: false,
            catalog: vec![1],
            signature: "sig".into(),
        };
        store
            .replace_catalogs("source", &old, generation, 10, &[catalog], true, false)
            .await?;
        let current = store.get_source("source").await?.unwrap();
        let mut rotated = current.clone();
        rotated.trust_revision += 1;
        rotated.public_key = "new key".into();
        rotated.fingerprint = "new fingerprint".into();
        let stale = store.put_source(&rotated, Some(&old)).await.unwrap_err();
        assert!(matches!(
            stale.downcast_ref::<PluginStorageError>(),
            Some(PluginStorageError::Changed)
        ));
        assert_eq!(store.list_catalogs("source").await?.len(), 1);
        store.put_source(&rotated, Some(&current)).await?;
        assert!(store.list_catalogs("source").await?.is_empty());
        assert!(!store.source_incomplete("source").await?);
        assert_eq!(
            store.get_source("source").await?.unwrap().succeeded_at,
            None
        );
        assert_eq!(store.list_seen_digests("source").await?, ledger);
        let stale = store
            .replace_catalogs("source", &old, generation, 11, &[], false, false)
            .await
            .unwrap_err();
        assert!(matches!(
            stale.downcast_ref::<PluginStorageError>(),
            Some(PluginStorageError::Changed)
        ));
        let stale = store.begin_source_check(&old).await.unwrap_err();
        assert!(matches!(
            stale.downcast_ref::<PluginStorageError>(),
            Some(PluginStorageError::Changed)
        ));
        let next = store.begin_source_check(&rotated).await?;
        assert!(next > generation);
        let stale = store
            .replace_catalogs("source", &rotated, generation, 12, &[], false, false)
            .await
            .unwrap_err();
        assert!(matches!(
            stale.downcast_ref::<PluginStorageError>(),
            Some(PluginStorageError::Changed)
        ));
        store.delete_source("other", &other).await?;
        let missing = store.begin_source_check(&other).await.unwrap_err();
        assert!(matches!(
            missing.downcast_ref::<PluginStorageError>(),
            Some(PluginStorageError::Changed)
        ));
        Ok(())
    }

    async fn transactional_trust_regressions(store: PluginStore) -> Result<()> {
        use crate::plugin::store::PluginStorageError;
        let original = source();
        store.put_source(&original, None).await?;
        let origin = PluginInstallOrigin {
            source_id: original.id.clone(),
            repository_id: original.repository_id,
            repository: original.repository.clone(),
            public_key_fingerprint: original.fingerprint.clone(),
            trust_revision: 1,
            release_id: 1,
            release_tag: "v1".into(),
            release_url: "https://example.com".into(),
            catalog_sha256: "catalog".into(),
            artifact_sha256: "artifact".into(),
        };
        let mut plugin = InstalledPlugin {
            vendor_id: "vendor".into(),
            version: "1.0.0".into(),
            source: "marketplace".into(),
            descriptor: "{}".into(),
            component: Bytes::new(),
            digest: "first".into(),
            revision: 1,
            data_epoch: 0,
            installed_at: 1,
            origin: Some(serde_json::to_string(&origin)?),
        };
        store
            .record_seen_digest("source", "vendor", "1.0.0", "first")
            .await?;
        let mut renamed = original.clone();
        renamed.repository = "new-owner/new-name".into();
        store.put_source(&renamed, Some(&original)).await?;
        store
            .record_seen_digest("source", "vendor", "1.0.0", "second")
            .await?;
        assert!(matches!(
            store.install(&plugin, None, &[]).await,
            Err(PluginStorageError::VersionConflict)
        ));
        plugin.version = "2.0.0".into();
        store
            .record_seen_digest("source", "vendor", "2.0.0", "first")
            .await?;
        let stale_generation = store.begin_source_check(&renamed).await?;
        let generation = store.begin_source_check(&renamed).await?;
        store
            .record_source_check(
                "source",
                1,
                stale_generation,
                2,
                None,
                Some(&MarketError::new("key_changed", "observed")),
            )
            .await?;
        assert!(matches!(
            store.install(&plugin, None, &[]).await,
            Err(PluginStorageError::KeyChanged)
        ));
        store
            .mark_source_key_changed("source", 1, "fingerprint", "new")
            .await?;
        store
            .record_source_check(
                "source",
                1,
                generation,
                3,
                None,
                Some(&MarketError::new("network", "offline")),
            )
            .await?;
        store
            .replace_catalogs("source", &renamed, generation, 4, &[], false, false)
            .await?;
        assert!(matches!(
            store.install(&plugin, None, &[]).await,
            Err(PluginStorageError::KeyChanged)
        ));
        let blocked = store.get_source("source").await?.unwrap();
        assert_eq!(
            blocked.error.as_ref().map(|e| e.code.as_str()),
            Some("key_changed")
        );
        let mut unsafe_clear = blocked.clone();
        unsafe_clear.error = None;
        assert!(
            store
                .put_source(&unsafe_clear, Some(&blocked))
                .await
                .is_err()
        );
        let mut retrusted = unsafe_clear;
        retrusted.trust_revision = 2;
        store.put_source(&retrusted, Some(&blocked)).await?;
        assert!(
            store
                .mark_source_key_changed("source", 1, "fingerprint", "new")
                .await
                .is_err()
        );
        assert!(
            store
                .record_source_check(
                    "source",
                    1,
                    stale_generation,
                    5,
                    None,
                    Some(&MarketError::new("key_changed", "stale"))
                )
                .await
                .is_err()
        );
        let mut new_origin = origin;
        new_origin.trust_revision = 2;
        plugin.origin = Some(serde_json::to_string(&new_origin)?);
        store.install(&plugin, None, &[]).await?;
        Ok(())
    }

    #[tokio::test]
    async fn memory_cache_and_ledger_regressions() -> Result<()> {
        use std::sync::Arc;
        use tokio::sync::RwLock;
        let store = PluginStore::memory(
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(crate::storage::memory::MemoryOAuthCredentialStore {
                credentials: RwLock::new(HashMap::new()),
            }),
            Arc::new(std::sync::atomic::AtomicI64::new(0)),
        );
        cache_and_ledger_regressions(&store).await
    }

    #[tokio::test]
    async fn sqlite_cache_and_ledger_regressions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        cache_and_ledger_regressions(&PluginStore::sqlite(pool)).await
    }

    #[tokio::test]
    async fn memory_transactional_trust_regressions() -> Result<()> {
        use std::sync::Arc;
        use tokio::sync::RwLock;
        let store = PluginStore::memory(
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(crate::storage::memory::MemoryOAuthCredentialStore {
                credentials: RwLock::new(HashMap::new()),
            }),
            Arc::new(std::sync::atomic::AtomicI64::new(0)),
        );
        transactional_trust_regressions(store).await
    }

    #[tokio::test]
    async fn sqlite_transactional_trust_regressions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        transactional_trust_regressions(PluginStore::sqlite(pool)).await
    }

    #[tokio::test]
    async fn sqlite_source_cas_conflicts_and_install_rollback() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = crate::db::init_pool(directory.path()).await?;
        crate::migrations::migrate_sqlite(&pool, None).await?;
        let store = PluginStore::sqlite(pool);
        let old = source();
        store.put_source(&old, None).await?;
        assert!(
            !store
                .record_seen_digest("source", "other", "1.0.0", "first")
                .await?
        );
        assert!(
            store
                .record_seen_digest("source", "other", "1.0.0", "second")
                .await?
        );
        assert!(
            store
                .record_seen_digest("source", "other", "1.0.0", "first")
                .await?
        );
        store
            .record_seen_digest("source", "vendor", "1.0.0", "first")
            .await?;
        let origin = PluginInstallOrigin {
            source_id: old.id.clone(),
            repository_id: old.repository_id,
            repository: old.repository.clone(),
            public_key_fingerprint: old.fingerprint.clone(),
            trust_revision: 1,
            release_id: 1,
            release_tag: "v1".into(),
            release_url: "https://example.com".into(),
            catalog_sha256: "catalog".into(),
            artifact_sha256: "artifact".into(),
        };
        let plugin = InstalledPlugin {
            vendor_id: "vendor".into(),
            version: "1.0.0".into(),
            source: "marketplace".into(),
            descriptor: "{}".into(),
            component: Bytes::new(),
            digest: "first".into(),
            revision: 1,
            data_epoch: 0,
            installed_at: 1,
            origin: Some(serde_json::to_string(&origin)?),
        };
        store.install(&plugin, None, &[]).await?;
        let stale_generation = store.begin_source_check(&old).await?;
        let mut rotated = old.clone();
        rotated.trust_revision = 2;
        rotated.fingerprint = "new".into();
        store.put_source(&rotated, Some(&old)).await?;
        assert!(store.put_source(&old, Some(&old)).await.is_err());
        assert!(
            store
                .replace_catalogs("source", &old, stale_generation, 10, &[], false, false)
                .await
                .is_err()
        );
        assert!(
            store
                .record_source_check("source", 1, stale_generation, 10, None, None)
                .await
                .is_err()
        );
        let mut replacement = plugin.clone();
        replacement.version = "2.0.0".into();
        replacement.revision = 2;
        assert!(store.install(&replacement, Some(1), &[]).await.is_err());
        let installed = store.list().await?;
        assert_eq!(installed[0].version, "1.0.0");
        assert_eq!(installed[0].origin, plugin.origin);
        let first = store.begin_source_check(&rotated).await?;
        let second = store.begin_source_check(&rotated).await?;
        assert!(second > first);
        store
            .record_source_check("source", 2, second, 20, None, None)
            .await?;
        assert!(
            store
                .record_source_check("source", 2, first, 20, None, None)
                .await
                .is_err()
        );
        Ok(())
    }
}
