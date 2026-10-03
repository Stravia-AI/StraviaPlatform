use super::types::TraceManifest;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileTraceManifest {
    pub schema_version: u32,
    pub trace_id: String,
    pub enabled: bool,
    pub status: String,
    pub bytes_written: u64,
    pub event_count: u64,
    pub reasons: Vec<String>,
    pub run_id: Option<String>,
    pub rejection_id: Option<String>,
    pub relative_directory: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub expires_at: i64,
    pub tombstoned: bool,
}
impl FileTraceManifest {
    pub(crate) fn trace(&self) -> TraceManifest {
        TraceManifest {
            trace_id: self.trace_id.clone(),
            enabled: self.enabled,
            status: self.status.clone(),
            bytes_written: self.bytes_written,
            event_count: self.event_count,
            reasons: self.reasons.clone(),
        }
    }
}
#[derive(Default)]
struct Entries {
    traces: HashMap<String, FileTraceManifest>,
    runs: HashMap<String, String>,
    rejections: HashMap<String, String>,
}
impl Entries {
    fn insert(&mut self, entry: FileTraceManifest) {
        if let Some(id) = &entry.run_id {
            self.runs.insert(id.clone(), entry.trace_id.clone());
        }
        if let Some(id) = &entry.rejection_id {
            self.rejections.insert(id.clone(), entry.trace_id.clone());
        }
        self.traces.insert(entry.trace_id.clone(), entry);
    }
    fn remove(&mut self, id: &str) {
        if let Some(entry) = self.traces.remove(id) {
            if let Some(owner) = entry.run_id {
                self.runs.remove(&owner);
            }
            if let Some(owner) = entry.rejection_id {
                self.rejections.remove(&owner);
            }
        }
    }
}
pub(crate) struct DebugTraceIndex {
    root: PathBuf,
    available: bool,
    entries: RwLock<Entries>,
    mutations: tokio::sync::Mutex<()>,
}
impl DebugTraceIndex {
    /// Synchronous startup scan; the caller runs this under spawn_blocking.
    pub(crate) fn load(diagnostics_root: &Path) -> anyhow::Result<Self> {
        let root = diagnostics_root.join("observation-debug");
        std::fs::create_dir_all(&root)?;
        anyhow::ensure!(
            !std::fs::symlink_metadata(&root)?.file_type().is_symlink(),
            "trace root is a symlink"
        );
        let root = root.canonicalize()?;
        let mut entries = Entries::default();
        for child in std::fs::read_dir(&root)? {
            // A broken child must not disable captures for the entire managed root.
            let result = (|| -> anyhow::Result<Option<FileTraceManifest>> {
                let child = child?;
                let name = child.file_name().to_string_lossy().into_owned();
                let kind = child.file_type()?;
                anyhow::ensure!(!kind.is_symlink(), "unsafe trace directory");
                if name.starts_with(".deleting-") {
                    if kind.is_dir() {
                        std::fs::remove_dir_all(child.path())?;
                    }
                    return Ok(None);
                }
                if !kind.is_dir() || !stravia_runtime_contract::identifier::valid_id(&name) {
                    return Ok(None);
                }
                let path = child.path().join("manifest.json");
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::remove_dir_all(child.path())?;
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                };
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "unsafe trace manifest"
                );
                let entry: FileTraceManifest = serde_json::from_slice(&std::fs::read(path)?)?;
                anyhow::ensure!(
                    entry.trace_id == name && entry.relative_directory == name,
                    "trace manifest directory mismatch"
                );
                Ok(Some(entry))
            })();
            match result {
                Ok(Some(entry)) => entries.insert(entry),
                Ok(None) => {}
                // Do not log paths, manifest contents, or parser errors (which can contain data).
                Err(_) => tracing::warn!("Skipping an invalid or unreadable debug trace manifest"),
            }
        }
        Ok(Self {
            root,
            available: true,
            entries: RwLock::new(entries),
            mutations: tokio::sync::Mutex::new(()),
        })
    }
    pub(crate) fn empty(diagnostics_root: &Path) -> Self {
        Self {
            root: diagnostics_root.join("observation-debug"),
            available: false,
            entries: RwLock::new(Entries::default()),
            mutations: tokio::sync::Mutex::new(()),
        }
    }
    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<FileTraceManifest> {
        self.entries
            .read()
            .traces
            .get(id)
            .filter(|m| !m.tombstoned)
            .cloned()
    }
    pub(crate) fn list(&self) -> Vec<FileTraceManifest> {
        self.entries.read().traces.values().cloned().collect()
    }
    pub(crate) fn for_run(&self, id: &str) -> Option<TraceManifest> {
        let entries = self.entries.read();
        entries
            .runs
            .get(id)
            .and_then(|id| entries.traces.get(id))
            .filter(|m| !m.tombstoned)
            .map(FileTraceManifest::trace)
    }
    pub(crate) fn run_complete(&self, id: &str) -> bool {
        let entries = self.entries.read();
        entries
            .runs
            .get(id)
            .and_then(|id| entries.traces.get(id))
            .is_some_and(|manifest| !manifest.tombstoned && manifest.status == "complete")
    }
    pub(crate) fn for_rejection(&self, id: &str) -> Option<TraceManifest> {
        let entries = self.entries.read();
        entries
            .rejections
            .get(id)
            .and_then(|id| entries.traces.get(id))
            .filter(|m| !m.tombstoned)
            .map(FileTraceManifest::trace)
    }
    async fn write_entry(&self, entry: &FileTraceManifest) -> anyhow::Result<()> {
        anyhow::ensure!(self.available, "trace manifest index unavailable");
        anyhow::ensure!(
            stravia_runtime_contract::identifier::valid_id(&entry.trace_id),
            "invalid trace id"
        );
        let directory = self.root.join(&entry.trace_id);
        let indexed = self.entries.read().traces.contains_key(&entry.trace_id);
        let bytes = serde_json::to_vec(entry)?;
        // 校验、落盘和关闭共用一个 blocking 任务，避免逐操作调度及执行器上的句柄关闭。
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            std::fs::create_dir_all(&directory)?;
            anyhow::ensure!(
                !std::fs::symlink_metadata(&directory)?
                    .file_type()
                    .is_symlink(),
                "trace directory is a symlink"
            );
            anyhow::ensure!(
                std::fs::canonicalize(&directory)?.parent() == directory.parent(),
                "trace directory escaped root"
            );
            let target = directory.join("manifest.json");
            match std::fs::symlink_metadata(&target) {
                Ok(metadata) => {
                    anyhow::ensure!(
                        metadata.is_file() && !metadata.file_type().is_symlink(),
                        "trace manifest is not a regular file"
                    );
                    anyhow::ensure!(indexed, "unindexed trace manifest cannot be overwritten");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let temporary = directory.join(format!(
                ".manifest-{}.tmp",
                stravia_runtime_contract::identifier::new_id()
            ));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            use std::io::Write;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            // std rename atomically replaces the destination, including on Windows.
            std::fs::rename(&temporary, target)?;
            Ok(())
        })
        .await??;
        self.entries.write().insert(entry.clone());
        Ok(())
    }
    /// Mutations of already-indexed manifests (tombstone, retention, recovery)
    /// always advance the in-memory index so queries, counts and later erasure
    /// stay consistent; a failed durable rewrite only fails the call when a
    /// divergent manifest file actually remains on disk.
    async fn rewrite_entry(&self, entry: &FileTraceManifest) -> anyhow::Result<()> {
        let indexed = self.entries.read().traces.contains_key(&entry.trace_id);
        match self.write_entry(entry).await {
            Ok(()) => Ok(()),
            Err(error) => {
                // Any durable manifest path that still exists — file, symlink or
                // otherwise — was left divergent from the index and stays an
                // error; only a write that left nothing behind degrades cleanly.
                let divergent = tokio::fs::symlink_metadata(
                    self.root.join(&entry.trace_id).join("manifest.json"),
                )
                .await
                .is_ok();
                if indexed || !divergent {
                    self.entries.write().insert(entry.clone());
                }
                if divergent {
                    Err(error)
                } else {
                    tracing::warn!(trace_id = %entry.trace_id, "trace manifest rewrite could not be saved");
                    Ok(())
                }
            }
        }
    }
    pub(crate) async fn persist(&self, entry: FileTraceManifest) -> anyhow::Result<()> {
        let _guard = self.mutations.lock().await;
        self.rewrite_entry(&entry).await
    }
    pub(crate) async fn update(&self, entry: FileTraceManifest) -> anyhow::Result<()> {
        self.persist(entry).await
    }
    pub(crate) async fn save_manifest(
        &self,
        run_id: Option<&str>,
        rejection_id: Option<&str>,
        m: &TraceManifest,
        created: i64,
        expires: i64,
        completed: bool,
    ) -> anyhow::Result<()> {
        let _guard = self.mutations.lock().await;
        // Clear retires capture ownership, including later maintenance/finalization.
        if m.reasons
            .iter()
            .any(|reason| reason == "debug_data_cleared")
        {
            return Ok(());
        }
        let prior = self.entries.read().traces.get(&m.trace_id).cloned();
        let entry = FileTraceManifest {
            schema_version: super::trace::TRACE_SCHEMA_VERSION,
            trace_id: m.trace_id.clone(),
            enabled: m.enabled,
            status: m.status.clone(),
            bytes_written: m.bytes_written,
            event_count: m.event_count,
            reasons: m.reasons.clone(),
            run_id: run_id.map(str::to_owned),
            rejection_id: rejection_id.map(str::to_owned),
            relative_directory: m.trace_id.clone(),
            created_at: prior.as_ref().map_or(created, |m| m.created_at),
            completed_at: prior
                .as_ref()
                .and_then(|m| m.completed_at)
                .or_else(|| completed.then_some(created)),
            expires_at: prior
                .as_ref()
                .filter(|m| m.completed_at.is_some())
                .map_or(expires, |m| m.expires_at),
            tombstoned: false,
        };
        match self.write_entry(&entry).await {
            Ok(()) => Ok(()),
            Err(error) => {
                // A terminal manifest is the last write for a trace: keep it in
                // the index so queries and counts still see the finalized
                // capture even though the durable write failed. In-flight saves
                // are advisory and retried by later maintenance flushes, so a
                // failed intermediate write stays out of the index.
                let conflicting = tokio::fs::symlink_metadata(
                    self.root.join(&entry.trace_id).join("manifest.json"),
                )
                .await
                .is_ok();
                if completed && (prior.is_some() || !conflicting) {
                    self.entries.write().insert(entry);
                }
                Err(error)
            }
        }
    }
    pub(crate) async fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _guard = self.mutations.lock().await;
        anyhow::ensure!(
            stravia_runtime_contract::identifier::valid_id(id),
            "invalid trace id"
        );
        let source = self.root.join(id);
        let deleting = self.root.join(format!(
            ".deleting-{}",
            stravia_runtime_contract::identifier::new_id()
        ));
        match tokio::fs::rename(&source, &deleting).await {
            Ok(()) => tokio::fs::remove_dir_all(&deleting).await?,
            // The trace directory may never have been created (a manifest
            // retained only in memory) or the managed root itself may be
            // unavailable; only a confirmed missing path permits index erasure.
            Err(error) => match tokio::fs::symlink_metadata(&source).await {
                Err(missing)
                    if matches!(
                        missing.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                _ => return Err(error.into()),
            },
        }
        self.entries.write().remove(id);
        Ok(())
    }
    pub(crate) async fn manifest_ids(&self) -> anyhow::Result<(HashSet<String>, HashSet<String>)> {
        let mut retained = HashSet::new();
        let mut tombstones = HashSet::new();
        for entry in self.list() {
            if entry.tombstoned {
                tombstones.insert(entry.trace_id);
            } else {
                retained.insert(entry.trace_id);
            }
        }
        Ok((retained, tombstones))
    }
    pub(crate) async fn delete_manifests(&self, ids: &[String]) -> anyhow::Result<()> {
        for id in ids {
            self.remove(id).await?;
        }
        Ok(())
    }
    pub(crate) async fn mark_all_debug_tombstones(&self) -> anyhow::Result<Vec<String>> {
        let _guard = self.mutations.lock().await;
        let mut ids = Vec::new();
        for mut entry in self.list() {
            entry.tombstoned = true;
            ids.push(entry.trace_id.clone());
            // Tombstoning only fences the in-memory entry before the caller
            // deletes it; the durable rewrite is best-effort because the
            // directory removal is the authoritative erasure and reports its
            // own failures.
            let _ = self.rewrite_entry(&entry).await;
        }
        Ok(ids)
    }
    pub(crate) async fn update_retention(&self, days: u32) -> anyhow::Result<()> {
        let _guard = self.mutations.lock().await;
        let ttl = i64::from(days).saturating_mul(86_400_000);
        let mut first_error = None;
        for mut entry in self.list() {
            entry.expires_at = entry
                .completed_at
                .unwrap_or(entry.created_at)
                .saturating_add(ttl);
            if let Err(error) = self.rewrite_entry(&entry).await {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    pub(crate) async fn recover(&self, now: i64) -> anyhow::Result<()> {
        anyhow::ensure!(self.available, "trace manifest index unavailable");
        for entry in self.list().into_iter().filter(|m| m.tombstoned) {
            self.remove(&entry.trace_id).await?;
        }
        let _guard = self.mutations.lock().await;
        let mut first_error = None;
        for mut entry in self.list() {
            if matches!(entry.status.as_str(), "running" | "writing") {
                entry.status = "partial".into();
                if !entry.reasons.iter().any(|r| r == "process_interrupted") {
                    entry.reasons.push("process_interrupted".into());
                }
                entry.completed_at.get_or_insert(now);
                if let Err(error) = self.rewrite_entry(&entry).await {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    pub(crate) async fn debug_manifest_counts(&self) -> anyhow::Result<(u64, u64)> {
        let entries = self.entries.read();
        let mut bytes = 0u64;
        let mut partial = 0u64;
        for entry in entries.traces.values().filter(|m| !m.tombstoned) {
            bytes = bytes.saturating_add(entry.bytes_written);
            if entry.status == "partial" && entry.completed_at.is_some() {
                partial += 1;
            }
        }
        Ok((bytes, partial))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str) -> FileTraceManifest {
        FileTraceManifest {
            schema_version: super::super::trace::TRACE_SCHEMA_VERSION,
            trace_id: id.into(),
            enabled: true,
            status: "complete".into(),
            bytes_written: 42,
            event_count: 1,
            reasons: vec![],
            run_id: Some(format!("run-{id}")),
            rejection_id: None,
            relative_directory: id.into(),
            created_at: 100,
            completed_at: Some(1_000),
            expires_at: 2_000,
            tombstoned: false,
        }
    }

    async fn expire(index: &std::sync::Arc<DebugTraceIndex>, now: i64) -> Vec<String> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::migrations::migrate_sqlite(&pool, None)
            .await
            .unwrap();
        sqlx::query("INSERT INTO interaction_observations(id,principal,root_id,root_run_id,first_route_id,status,started_at,last_active_at,expires_at) VALUES ('completed-owner','test','completed-owner','initial','route','failed',100,1000,2000)")
            .execute(&pool).await.unwrap();
        for entry in index.list() {
            if let Some(run_id) = entry.run_id {
                sqlx::query("INSERT OR IGNORE INTO inference_run_observations(id,interaction_id,parent_run_id,ingress_protocol,route_id,status,debug_enabled,started_at,last_active_at,expires_at) VALUES (?, 'completed-owner',NULL,'openai','route','failed',0,100,1000,2000)")
                    .bind(run_id).execute(&pool).await.unwrap();
            }
        }
        let store = super::super::store::ObservationStore::Sqlite(
            pool,
            std::sync::Arc::clone(index),
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        );
        store.mark_expired_tombstones(now).await.unwrap()
    }

    fn put(root: &Path, id: &str, bytes: &[u8]) {
        let directory = root.join("observation-debug").join(id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("manifest.json"), bytes).unwrap();
    }

    #[tokio::test]
    async fn invalid_child_preserves_other_queries_new_captures_and_expiry() {
        for bad in [
            b"not json".to_vec(),
            serde_json::to_vec(&manifest("dddddddddddddddddddddddddddd")).unwrap(),
        ] {
            let root = tempfile::tempdir().unwrap();
            put(root.path(), "cccccccccccccccccccccccccccc", &bad);
            put(
                root.path(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &serde_json::to_vec(&manifest("aaaaaaaaaaaaaaaaaaaaaaaaaaaa")).unwrap(),
            );
            let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
            assert_eq!(
                index
                    .for_run("run-aaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .unwrap()
                    .bytes_written,
                42
            );
            assert!(index.for_run("run-dddddddddddddddddddddddddddd").is_none());
            index
                .persist(manifest("bbbbbbbbbbbbbbbbbbbbbbbbbbbb"))
                .await
                .unwrap();
            assert_eq!(
                index
                    .for_run("run-bbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                    .unwrap()
                    .trace_id,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            );
            assert!(
                index
                    .persist(manifest("cccccccccccccccccccccccccccc"))
                    .await
                    .is_err()
            );
            assert_eq!(
                std::fs::read(
                    root.path()
                        .join("observation-debug/cccccccccccccccccccccccccccc/manifest.json")
                )
                .unwrap(),
                bad
            );
            assert!(expire(&index, 1_999).await.is_empty());
            let expired = expire(&index, 2_000).await;
            assert_eq!(
                expired.into_iter().collect::<HashSet<_>>(),
                HashSet::from([
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
                ])
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_manifest_is_not_followed_or_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside.json");
        let bytes = serde_json::to_vec(&manifest("eeeeeeeeeeeeeeeeeeeeeeeeeeee")).unwrap();
        std::fs::write(&outside, &bytes).unwrap();
        let directory = root
            .path()
            .join("observation-debug/eeeeeeeeeeeeeeeeeeeeeeeeeeee");
        std::fs::create_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(&outside, directory.join("manifest.json")).unwrap();
        put(
            root.path(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &serde_json::to_vec(&manifest("aaaaaaaaaaaaaaaaaaaaaaaaaaaa")).unwrap(),
        );
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        assert!(index.get("eeeeeeeeeeeeeeeeeeeeeeeeeeee").is_none());
        assert_eq!(
            index
                .for_run("run-aaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap()
                .bytes_written,
            42
        );
        assert!(
            index
                .persist(manifest("eeeeeeeeeeeeeeeeeeeeeeeeeeee"))
                .await
                .is_err()
        );
        index
            .persist(manifest("bbbbbbbbbbbbbbbbbbbbbbbbbbbb"))
            .await
            .unwrap();
        assert_eq!(std::fs::read(outside).unwrap(), bytes);
    }

    #[tokio::test]
    async fn terminal_updates_preserve_completion_and_expiry_across_restart() {
        let root = tempfile::tempdir().unwrap();
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        let original = manifest("ffffffffffffffffffffffffffff");
        index.persist(original.clone()).await.unwrap();
        let mut trace = original.trace();
        trace.status = "partial".into();
        trace.reasons.push("observation_storage_error".into());
        index
            .save_manifest(
                original.run_id.as_deref(),
                None,
                &trace,
                1_500,
                2_500,
                false,
            )
            .await
            .unwrap();
        index
            .save_manifest(original.run_id.as_deref(), None, &trace, 1_800, 2_800, true)
            .await
            .unwrap();
        drop(index);
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        let entry = index.get(&original.trace_id).unwrap();
        assert_eq!(entry.status, "partial");
        assert_eq!(entry.reasons, ["observation_storage_error"]);
        assert_eq!(entry.completed_at, Some(1_000));
        assert_eq!(entry.expires_at, 2_000);
        assert_eq!(index.debug_manifest_counts().await.unwrap().1, 1);
        assert!(expire(&index, 1_999).await.is_empty());
        assert_eq!(expire(&index, 2_000).await, [original.trace_id]);
    }

    #[tokio::test]
    async fn restart_preserves_long_run_deadline_and_existing_completion() {
        let root = tempfile::tempdir().unwrap();
        let mut entry = manifest("ffffffffffffffffffffffffffff");
        entry.status = "writing".into();
        put(
            root.path(),
            "ffffffffffffffffffffffffffff",
            &serde_json::to_vec(&entry).unwrap(),
        );
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        index.recover(1_900).await.unwrap();
        let recovered = index.get("ffffffffffffffffffffffffffff").unwrap();
        assert_eq!(recovered.completed_at, Some(1_000));
        assert_eq!(recovered.expires_at, 2_000);
        drop(index);
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        index.recover(2_000).await.unwrap();
        assert_eq!(
            expire(&index, 2_000).await,
            ["ffffffffffffffffffffffffffff"]
        );
    }

    #[test]
    fn unavailable_root_remains_an_error() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("observation-debug"), b"not a directory").unwrap();
        assert!(DebugTraceIndex::load(root.path()).is_err());
    }

    #[tokio::test]
    async fn configured_retention_uses_completion_without_adding_run_duration() {
        let root = tempfile::tempdir().unwrap();
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        index
            .persist(manifest("ffffffffffffffffffffffffffff"))
            .await
            .unwrap();
        index.update_retention(1).await.unwrap();
        let deadline = 1_000 + 86_400_000;
        assert_eq!(
            index
                .get("ffffffffffffffffffffffffffff")
                .unwrap()
                .expires_at,
            deadline
        );
        assert!(expire(&index, deadline - 1).await.is_empty());
        assert_eq!(
            expire(&index, deadline).await,
            ["ffffffffffffffffffffffffffff"]
        );
    }

    #[tokio::test]
    async fn missing_completion_recovery_does_not_extend_expiry() {
        let root = tempfile::tempdir().unwrap();
        let mut entry = manifest("gggggggggggggggggggggggggggg");
        entry.status = "running".into();
        entry.completed_at = None;
        put(
            root.path(),
            "gggggggggggggggggggggggggggg",
            &serde_json::to_vec(&entry).unwrap(),
        );
        let index = std::sync::Arc::new(DebugTraceIndex::load(root.path()).unwrap());
        index.recover(3_000).await.unwrap();
        assert_eq!(
            index
                .get("gggggggggggggggggggggggggggg")
                .unwrap()
                .expires_at,
            2_000
        );
        assert_eq!(
            expire(&index, 3_000).await,
            ["gggggggggggggggggggggggggggg"]
        );
    }

    #[tokio::test]
    async fn failed_terminal_write_retains_manifest_for_queries_and_erase() {
        let root = tempfile::tempdir().unwrap();
        // An unavailable index still retains terminal manifests in memory.
        let index = DebugTraceIndex::empty(root.path());
        let terminal = TraceManifest {
            trace_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            enabled: true,
            status: "partial".into(),
            bytes_written: 0,
            event_count: 0,
            reasons: vec!["storage_error".into()],
        };
        assert!(
            index
                .save_manifest(Some("run-a"), None, &terminal, 100, 2_000, true)
                .await
                .is_err()
        );
        assert_eq!(
            index.for_run("run-a").map(|m| m.status).as_deref(),
            Some("partial")
        );
        assert_eq!(index.debug_manifest_counts().await.unwrap().1, 1);
        // Failed in-flight saves stay invisible: later flushes retry them.
        let running = TraceManifest {
            trace_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            enabled: true,
            status: "running".into(),
            bytes_written: 0,
            event_count: 0,
            reasons: Vec::new(),
        };
        assert!(
            index
                .save_manifest(Some("run-b"), None, &running, 100, 2_000, false)
                .await
                .is_err()
        );
        assert!(index.for_run("run-b").is_none());
        // Retained state still tombstones and erases without any durable file.
        let (_, tombstones) = index.manifest_ids().await.unwrap();
        assert!(tombstones.is_empty());
        index
            .mark_all_debug_tombstones()
            .await
            .expect("memory tombstones");
        index
            .delete_manifests(&["aaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()])
            .await
            .expect("memory erase");
        assert!(index.for_run("run-a").is_none());
        assert_eq!(index.debug_manifest_counts().await.unwrap(), (0, 0));
    }
}
