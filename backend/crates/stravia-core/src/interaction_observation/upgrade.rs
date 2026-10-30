//! Startup-only, resumable conversion of the observation v1 storage.
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use sqlx::Row;
use std::{io::Write, path::Path};

fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
}

// Always join every started worker, including on failure: no detached file writes.
async fn blocking_chunks<T: Send + 'static, U: Send + 'static>(
    items: Vec<T>,
    work: fn(T) -> anyhow::Result<U>,
) -> anyhow::Result<Vec<U>> {
    let chunk_size = items.len().div_ceil(worker_count()).max(1);
    let mut items = items.into_iter();
    let mut tasks = Vec::new();
    loop {
        let chunk: Vec<_> = items.by_ref().take(chunk_size).collect();
        if chunk.is_empty() {
            break;
        }
        tasks.push(tokio::task::spawn_blocking(move || {
            chunk
                .into_iter()
                .map(work)
                .collect::<anyhow::Result<Vec<_>>>()
        }));
    }
    let mut output = Vec::new();
    let mut failure = None;
    for task in tasks {
        match task.await {
            Ok(Ok(mut values)) => output.append(&mut values),
            Ok(Err(error)) => {
                failure.get_or_insert(error);
            }
            Err(error) => {
                failure.get_or_insert(error.into());
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(output)
}

fn decode_legacy(current: String) -> anyhow::Result<(String, Value)> {
    let value = super::codec::decode_payload(serde_json::from_str(&current)?)?;
    Ok((current, value))
}

async fn encode_event(value: Value) -> anyhow::Result<(Value, Vec<u8>)> {
    tokio::task::spawn_blocking(move || {
        let bytes = crate::storage_codec::encode(serde_json::to_vec(&value)?.into())?;
        Ok((value, bytes))
    })
    .await?
}

async fn decode_event(bytes: Vec<u8>) -> anyhow::Result<Value> {
    tokio::task::spawn_blocking(move || {
        Ok(serde_json::from_slice(&crate::storage_codec::decode(
            &bytes,
        )?)?)
    })
    .await?
}

async fn legacy_json(value: Value) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || Ok(serde_json::to_string(&value)?)).await?
}

async fn export_manifests(root: &Path, manifests: Vec<Value>) -> anyhow::Result<()> {
    let root = root.to_owned();
    blocking_chunks(
        manifests
            .into_iter()
            .map(|manifest| (root.clone(), manifest))
            .collect(),
        |(root, manifest)| export_manifest(&root, manifest),
    )
    .await?;
    Ok(())
}

fn legacy_committed(value: &Value, through: i64) -> bool {
    value["client_output_committed_sequence"]
        .as_i64()
        .is_some_and(|sequence| sequence <= through)
}

fn export_manifest(root: &Path, manifest: Value) -> anyhow::Result<()> {
    let id = manifest["trace_id"]
        .as_str()
        .context("missing legacy trace id")?;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            && Path::new(id).components().count() == 1,
        "invalid legacy trace id"
    );
    let debug_root = root.join("observation-debug");
    if debug_root.exists() {
        ensure!(
            !std::fs::symlink_metadata(&debug_root)?
                .file_type()
                .is_symlink(),
            "debug root is a symlink"
        );
    }
    let directory = debug_root.join(id);
    if manifest["tombstoned"].as_bool() == Some(true) {
        return Ok(());
    }
    std::fs::create_dir_all(&directory)?;
    ensure!(
        !std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink(),
        "legacy trace directory is a symlink"
    );
    ensure!(
        directory.canonicalize()?.starts_with(root.canonicalize()?),
        "legacy trace directory escapes diagnostics root"
    );
    let target = directory.join("manifest.json");
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "legacy trace manifest is not a regular file"
            );
            let existing: Value = serde_json::from_slice(&std::fs::read(&target)?)
                .context("invalid resumed legacy trace manifest")?;
            ensure!(
                existing == manifest,
                "resumed legacy trace manifest differs from authoritative database facts"
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary.write_all(&serde_json::to_vec(&manifest)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(directory.join("manifest.json"))?;
    Ok(())
}

pub(crate) async fn export_debug_manifests_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    root: Option<&Path>,
) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
    )
    .bind("debug_trace_manifests")
    .fetch_one(&mut *connection)
    .await?;
    if !exists {
        return Ok(());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM debug_trace_manifests")
        .fetch_one(&mut *connection)
        .await?;
    let total = u64::try_from(count)?;
    crate::startup_progress::report(
        "debug_manifests",
        "Exporting diagnostic manifests",
        0,
        Some(total),
    );
    if count == 0 {
        return Ok(());
    }
    let root =
        root.context("legacy debug manifests require a diagnostics directory before migration")?;
    let mut exported = 0usize;
    let mut after = String::new();
    loop {
        let rows = sqlx::query(
            "SELECT * FROM debug_trace_manifests WHERE trace_id>? ORDER BY trace_id LIMIT 200",
        )
        .bind(&after)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        let mut manifests = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("trace_id")?;
            let reason: Option<String> = row.try_get("partial_reason")?;
            manifests.push(
                json!({"schema_version":2,"trace_id":id,"enabled":true,
                "status":row.try_get::<String,_>("status")?, "bytes_written":row.try_get::<i64,_>("bytes_written")?,
                "event_count":row.try_get::<i64,_>("event_count")?, "reasons":reason.into_iter().collect::<Vec<_>>(),
                "run_id":row.try_get::<Option<String>,_>("run_id")?, "rejection_id":row.try_get::<Option<String>,_>("rejection_id")?,
                "relative_directory":id, "created_at":row.try_get::<i64,_>("created_at")?,
                "completed_at":row.try_get::<Option<i64>,_>("completed_at")?, "expires_at":row.try_get::<i64,_>("expires_at")?,
                "tombstoned":row.try_get::<bool,_>("tombstoned")?}),
            );
            after = id;
        }
        let batch_size = manifests.len();
        export_manifests(root, manifests).await?;
        exported += batch_size;
        crate::startup_progress::report(
            "debug_manifests",
            "Exporting diagnostic manifests",
            exported as u64,
            Some(total),
        );
    }
    Ok(())
}

pub(crate) async fn convert_event_storage_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
    )
    .bind("observation_events_legacy")
    .fetch_one(&mut *connection)
    .await?;
    if !exists {
        return Ok(());
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS observation_events_legacy_upgrade_run_sequence ON observation_events_legacy(run_id,sequence)")
        .execute(&mut *connection).await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observation_events_legacy")
        .fetch_one(&mut *connection)
        .await?;
    let total = u64::try_from(total)?;
    crate::startup_progress::report(
        "observation_data",
        "Converting observation events",
        0,
        Some(total),
    );
    let mut converted = 0usize;
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at FROM observation_events_legacy ORDER BY sequence LIMIT 200").fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            crate::startup_progress::report(
                "observation_data",
                "Cleaning up migrated observations",
                0,
                None,
            );
            sqlx::query("DROP TABLE observation_events_legacy")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            break;
        }
        let batch_size = rows.len();
        let payloads = rows
            .iter()
            .map(|row| row.try_get::<String, _>("payload"))
            .collect::<Result<Vec<_>, _>>()?;
        let decoded = blocking_chunks(payloads, decode_legacy).await?;
        // Only reread rows actually changed by an earlier lifecycle merge.
        let mut mutated_sequences = std::collections::HashSet::new();
        for (row, (mut current, mut value)) in rows.into_iter().zip(decoded) {
            let sequence: i64 = row.try_get("sequence")?;
            let occurred: i64 = row.try_get("occurred_at")?;
            let run: Option<String> = row.try_get("run_id")?;
            let mut kind: String = row.try_get("kind")?;
            if mutated_sequences.remove(&sequence) {
                current = sqlx::query_scalar(
                    "SELECT payload FROM observation_events_legacy WHERE sequence=?",
                )
                .bind(sequence)
                .fetch_one(&mut *tx)
                .await?;
                (current, value) = blocking_chunks(vec![current], decode_legacy)
                    .await?
                    .pop()
                    .context("missing decoded legacy event")?;
            }
            match kind.as_str() {
                "model_thinking_delta"
                | "client_visible_content_delta"
                | "model_thinking_finished" => {
                    (kind, value) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        let original = value.clone();
                        let finished = kind == "model_thinking_finished";
                        kind = if kind == "client_visible_content_delta" {
                            "client_visible_content"
                        } else {
                            "model_thinking"
                        }
                        .into();
                        value["kind"] = kind.clone().into();
                        value["legacy_text"] = original;
                        value
                            .as_object_mut()
                            .context("invalid legacy text payload")?
                            .remove("block_id");
                        value
                            .as_object_mut()
                            .context("invalid legacy text payload")?
                            .remove("item");
                        if value.get("text").is_none() {
                            value["text"] = "".into();
                        }
                        if value.get("parts").is_none() {
                            value["parts"] = json!([{"type":"text","text":value["text"]}]);
                        }
                        if value.get("complete").is_none() {
                            value["complete"] = finished.into();
                        }
                        if finished {
                            value["parts"] = json!([]);
                        }
                        Ok((kind, value))
                    })
                    .await??;
                }

                "trace_manifest_updated" => {}
                "usage_confirmed" => {
                    let original = value.clone();
                    let attempt = value["attempt_id"]
                        .as_str()
                        .context("legacy usage has no attempt id")?;
                    let projection = sqlx::query("SELECT status,status_code,error_code,duration_ms,first_token_ms FROM target_attempt_observations WHERE id=?").bind(attempt).fetch_optional(&mut *tx).await?;
                    let status = projection
                        .as_ref()
                        .map(|row| row.try_get::<String, _>("status"))
                        .transpose()?;
                    value = json!({"kind":"target_attempt_finished","model_turn_id":original["model_turn_id"],"attempt_id":original["attempt_id"],"status":status.unwrap_or_else(||"interrupted".into()),"status_code":null,"error_code":null,"duration_ms":null,"first_token_ms":null,"usage":original["usage"],"legacy_lifecycle":[{"sequence":sequence,"occurred_at":occurred,"payload":original}]});
                    if let Some(projection) = projection {
                        value["status_code"] =
                            json!(projection.try_get::<Option<i64>, _>("status_code")?);
                        value["error_code"] =
                            json!(projection.try_get::<Option<String>, _>("error_code")?);
                        value["duration_ms"] =
                            json!(projection.try_get::<Option<i64>, _>("duration_ms")?);
                        value["first_token_ms"] =
                            json!(projection.try_get::<Option<i64>, _>("first_token_ms")?);
                    }
                    kind = "target_attempt_finished".into();
                }
                "delivery_finished" => {
                    let original = value.clone();
                    value = json!({"kind":"run_finished","status":original["status"],"reason":original["reason"],"delivery_completed_at":if original["status"] == "delivered" { json!(occurred) } else { Value::Null },"delivery":{"status":original["status"],"reason":original["reason"],"completed_at":occurred},"legacy_lifecycle":[{"sequence":sequence,"occurred_at":occurred,"payload":original}]});
                    if let Some(projection) = sqlx::query("SELECT r.status,r.terminal_reason,r.generation_node_id,i.generation_root_id,r.finished_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=?").bind(&run).fetch_optional(&mut *tx).await? {
                        value["status"] = projection.try_get::<String,_>("status")?.into();
                        value["terminal_reason"] = json!(projection.try_get::<Option<String>,_>("terminal_reason")?);
                        value["generation_node_id"] = json!(projection.try_get::<Option<String>,_>("generation_node_id")?);
                        value["generation_root_id"] = json!(projection.try_get::<Option<String>,_>("generation_root_id")?);
                        value["finished_at"] = json!(projection.try_get::<Option<i64>,_>("finished_at")?);
                    } else {
                        value["terminal_reason"] = value["reason"].clone();
                        value["generation_node_id"] = Value::Null; value["generation_root_id"] = Value::Null;
                    }
                    kind = "run_finished".into();
                }
                "generation_associated" | "client_output_committed" | "process_restarted" => {
                    value = json!({"kind":"run_state_changed","status":value["status"],"reason":value["reason"],"legacy_lifecycle":[{"sequence":sequence,"occurred_at":occurred,"kind":kind,"payload":value}]});
                    kind = "run_state_changed".into();
                }
                _ => {}
            }
            // Merge removed lifecycle facts into a durable event, preserving original identity and time.
            let original_kind: String = row.try_get("kind")?;
            let obsolete = matches!(
                original_kind.as_str(),
                "usage_confirmed"
                    | "delivery_finished"
                    | "generation_associated"
                    | "client_output_committed"
            );
            let mut merged = false;
            if obsolete {
                for legacy in [false, true] {
                    let candidates = if legacy {
                        sqlx::query("SELECT sequence,kind,payload FROM observation_events_legacy WHERE run_id=? AND sequence<>? AND kind NOT IN ('trace_manifest_updated','usage_confirmed','delivery_finished','generation_associated','client_output_committed','process_restarted') AND ((?3='usage_confirmed' AND kind='target_attempt_finished') OR (?3='delivery_finished' AND kind='run_finished') OR (?3='client_output_committed' AND kind='run_admitted') OR ?3='generation_associated') ORDER BY sequence LIMIT CASE WHEN ?3='generation_associated' THEN 1 ELSE -1 END").bind(&run).bind(sequence).bind(&original_kind).fetch_all(&mut *tx).await?
                    } else {
                        sqlx::query("SELECT sequence,kind,payload FROM observation_events WHERE run_id=? AND ((?2='usage_confirmed' AND kind='target_attempt_finished') OR (?2='delivery_finished' AND kind='run_finished') OR (?2='client_output_committed' AND kind='run_admitted') OR ?2='generation_associated') ORDER BY sequence LIMIT CASE WHEN ?2='generation_associated' THEN 1 ELSE -1 END").bind(&run).bind(&original_kind).fetch_all(&mut *tx).await?
                    };
                    for candidate in candidates {
                        let candidate_kind: String = candidate.try_get("kind")?;
                        let mut target: Value = if legacy {
                            blocking_chunks(
                                vec![candidate.try_get::<String, _>("payload")?],
                                decode_legacy,
                            )
                            .await?
                            .pop()
                            .context("missing decoded lifecycle candidate")?
                            .1
                        } else {
                            decode_event(candidate.try_get::<Vec<u8>, _>("payload")?).await?
                        };
                        if original_kind == "client_output_committed"
                            && candidate_kind != "run_admitted"
                        {
                            continue;
                        }
                        if original_kind == "usage_confirmed"
                            && (candidate_kind != "target_attempt_finished"
                                || target["attempt_id"] != value["attempt_id"]
                                || target["model_turn_id"] != value["model_turn_id"]
                                || !target["usage"].is_null())
                        {
                            continue;
                        }
                        if original_kind == "delivery_finished"
                            && (candidate_kind != "run_finished" || !target["delivery"].is_null())
                        {
                            continue;
                        }
                        if original_kind == "usage_confirmed" {
                            target["usage"] = value["usage"].clone();
                        }
                        if original_kind == "delivery_finished" {
                            target["delivery"] = value["delivery"].clone();
                            if target["delivery_completed_at"].is_null() {
                                target["delivery_completed_at"] =
                                    value["delivery_completed_at"].clone();
                            }
                            sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,?) WHERE id=?").bind(target["delivery_completed_at"].as_i64()).bind(&run).execute(&mut *tx).await?;
                        }
                        if original_kind == "client_output_committed" {
                            let previous = target["client_output_committed_sequence"]
                                .as_i64()
                                .unwrap_or(sequence);
                            target["client_output_committed_sequence"] =
                                previous.min(sequence).into();
                        }
                        if target.get("legacy_lifecycle").is_none() {
                            target["legacy_lifecycle"] = json!([]);
                        }
                        target["legacy_lifecycle"].as_array_mut().context("invalid legacy lifecycle facts")?.push(json!({"sequence":sequence,"occurred_at":occurred,"kind":original_kind,"payload":serde_json::from_str::<Value>(&current)?}));
                        let id: i64 = candidate.try_get("sequence")?;
                        if legacy {
                            mutated_sequences.insert(id);
                            sqlx::query(
                                "UPDATE observation_events_legacy SET payload=? WHERE sequence=?",
                            )
                            .bind(legacy_json(target).await?)
                            .bind(id)
                            .execute(&mut *tx)
                            .await?;
                        } else {
                            sqlx::query("UPDATE observation_events SET payload=? WHERE sequence=?")
                                .bind(encode_event(target).await?.1)
                                .bind(id)
                                .execute(&mut *tx)
                                .await?;
                        }
                        merged = true;
                        break;
                    }
                    if merged {
                        break;
                    }
                }
            }
            if original_kind == "client_output_committed" {
                ensure!(merged, "legacy client commit has no admission event");
                if let Some(terminal) = sqlx::query("SELECT occurred_at,payload FROM observation_events WHERE run_id=? AND sequence<? AND kind='run_finished' ORDER BY sequence DESC LIMIT 1").bind(&run).bind(sequence).fetch_optional(&mut *tx).await? {
                    value = decode_event(terminal.try_get::<Vec<u8>,_>("payload")?).await?;
                    if value.get("finished_at").is_none() { value["finished_at"] = terminal.try_get::<i64,_>("occurred_at")?.into(); }
                    value["client_output_committed"] = true.into();
                    kind = "run_finished".into();
                    merged = false;
                }
            }
            if kind != "trace_manifest_updated" && !merged {
                if kind == "target_attempt_finished" && value.get("usage").is_none() {
                    value["usage"] = Value::Null;
                }
                if kind == "run_finished" {
                    let commits = sqlx::query("SELECT payload FROM observation_events WHERE run_id=? AND sequence<=? AND kind='run_admitted'").bind(&run).bind(sequence).fetch_all(&mut *tx).await?;
                    for commit in commits {
                        let fact = decode_event(commit.try_get::<Vec<u8>, _>("payload")?).await?;
                        if legacy_committed(&fact, sequence) {
                            value["client_output_committed"] = true.into();
                        }
                    }
                    if value.get("delivery").is_none() {
                        value["delivery"] = Value::Null;
                    }
                    if let Some(completed) = value["delivery_completed_at"].as_i64() {
                        sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,?) WHERE id=?").bind(completed).bind(&run).execute(&mut *tx).await?;
                    }
                }
                let (encoded_value, bytes) = encode_event(value).await?;
                value = encoded_value;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at,tool_id,operation_id) VALUES (?,?,?,?,?,?,?,?,?,?)")
                    .bind(sequence).bind(if matches!(original_kind.as_str(), "delivery_finished" | "client_output_committed") { value["finished_at"].as_i64().unwrap_or(occurred) } else { occurred }).bind(row.try_get::<Option<String>,_>("interaction_id")?).bind(&run)
                    .bind(row.try_get::<Option<String>,_>("rejection_id")?).bind(&kind).bind(bytes).bind(row.try_get::<i64,_>("expires_at")?)
                    .bind(value["tool_id"].as_str()).bind(value["operation_id"].as_str()).execute(&mut *tx).await?;
            }
            sqlx::query("DELETE FROM observation_events_legacy WHERE sequence=?")
                .bind(sequence)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        converted += batch_size;
        crate::startup_progress::report(
            "observation_data",
            "Converting observation events",
            converted as u64,
            Some(total),
        );
    }
    Ok(())
}

pub(crate) async fn export_debug_manifests_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    root: Option<&Path>,
) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind("debug_trace_manifests")
        .fetch_one(&mut *connection)
        .await?;
    if !exists {
        return Ok(());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM debug_trace_manifests")
        .fetch_one(&mut *connection)
        .await?;
    let total = u64::try_from(count)?;
    crate::startup_progress::report(
        "debug_manifests",
        "Exporting diagnostic manifests",
        0,
        Some(total),
    );
    if count == 0 {
        return Ok(());
    }
    let root =
        root.context("legacy debug manifests require a diagnostics directory before migration")?;
    let mut exported = 0usize;
    let mut after = String::new();
    loop {
        let rows = sqlx::query(
            "SELECT * FROM debug_trace_manifests WHERE trace_id>$1 ORDER BY trace_id LIMIT 200",
        )
        .bind(&after)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        let mut manifests = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("trace_id")?;
            let reason: Option<String> = row.try_get("partial_reason")?;
            manifests.push(
                json!({"schema_version":2,"trace_id":id,"enabled":true,
                "status":row.try_get::<String,_>("status")?, "bytes_written":row.try_get::<i64,_>("bytes_written")?,
                "event_count":row.try_get::<i64,_>("event_count")?, "reasons":reason.into_iter().collect::<Vec<_>>(),
                "run_id":row.try_get::<Option<String>,_>("run_id")?, "rejection_id":row.try_get::<Option<String>,_>("rejection_id")?,
                "relative_directory":id, "created_at":row.try_get::<i64,_>("created_at")?,
                "completed_at":row.try_get::<Option<i64>,_>("completed_at")?, "expires_at":row.try_get::<i64,_>("expires_at")?,
                "tombstoned":row.try_get::<bool,_>("tombstoned")?}),
            );
            after = id;
        }
        let batch_size = manifests.len();
        export_manifests(root, manifests).await?;
        exported += batch_size;
        crate::startup_progress::report(
            "debug_manifests",
            "Exporting diagnostic manifests",
            exported as u64,
            Some(total),
        );
    }
    Ok(())
}

pub(crate) async fn convert_event_storage_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind("observation_events_legacy")
        .fetch_one(&mut *connection)
        .await?;
    if !exists {
        return Ok(());
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS observation_events_legacy_upgrade_run_sequence ON observation_events_legacy(run_id,sequence)")
        .execute(&mut *connection).await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observation_events_legacy")
        .fetch_one(&mut *connection)
        .await?;
    let total = u64::try_from(total)?;
    crate::startup_progress::report(
        "observation_data",
        "Converting observation events",
        0,
        Some(total),
    );
    let mut converted = 0usize;
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query("SELECT sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload::text AS payload,expires_at FROM observation_events_legacy ORDER BY sequence LIMIT 200").fetch_all(&mut *tx).await?;
        if rows.is_empty() {
            crate::startup_progress::report(
                "observation_data",
                "Cleaning up migrated observations",
                0,
                None,
            );
            sqlx::query("DROP TABLE observation_events_legacy")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            break;
        }
        let batch_size = rows.len();
        let payloads = rows
            .iter()
            .map(|row| row.try_get::<String, _>("payload"))
            .collect::<Result<Vec<_>, _>>()?;
        let decoded = blocking_chunks(payloads, decode_legacy).await?;
        // Only reread rows actually changed by an earlier lifecycle merge.
        let mut mutated_sequences = std::collections::HashSet::new();
        for (row, (mut current, mut value)) in rows.into_iter().zip(decoded) {
            let sequence: i64 = row.try_get("sequence")?;
            let occurred: i64 = row.try_get("occurred_at")?;
            let run: Option<String> = row.try_get("run_id")?;
            let mut kind: String = row.try_get("kind")?;
            if mutated_sequences.remove(&sequence) {
                current = sqlx::query_scalar(
                    "SELECT payload::text FROM observation_events_legacy WHERE sequence=$1",
                )
                .bind(sequence)
                .fetch_one(&mut *tx)
                .await?;
                (current, value) = blocking_chunks(vec![current], decode_legacy)
                    .await?
                    .pop()
                    .context("missing decoded legacy event")?;
            }
            match kind.as_str() {
                "model_thinking_delta"
                | "client_visible_content_delta"
                | "model_thinking_finished" => {
                    (kind, value) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        let original = value.clone();
                        let finished = kind == "model_thinking_finished";
                        kind = if kind == "client_visible_content_delta" {
                            "client_visible_content"
                        } else {
                            "model_thinking"
                        }
                        .into();
                        value["kind"] = kind.clone().into();
                        value["legacy_text"] = original;
                        value
                            .as_object_mut()
                            .context("invalid legacy text payload")?
                            .remove("block_id");
                        value
                            .as_object_mut()
                            .context("invalid legacy text payload")?
                            .remove("item");
                        if value.get("text").is_none() {
                            value["text"] = "".into();
                        }
                        if value.get("parts").is_none() {
                            value["parts"] = json!([{"type":"text","text":value["text"]}]);
                        }
                        if value.get("complete").is_none() {
                            value["complete"] = finished.into();
                        }
                        if finished {
                            value["parts"] = json!([]);
                        }
                        Ok((kind, value))
                    })
                    .await??;
                }

                "trace_manifest_updated" => {}
                "usage_confirmed" => {
                    let original = value.clone();
                    let attempt = value["attempt_id"]
                        .as_str()
                        .context("legacy usage has no attempt id")?;
                    let projection = sqlx::query("SELECT status,status_code,error_code,duration_ms,first_token_ms FROM target_attempt_observations WHERE id=$1").bind(attempt).fetch_optional(&mut *tx).await?;
                    let status = projection
                        .as_ref()
                        .map(|row| row.try_get::<String, _>("status"))
                        .transpose()?;
                    value = json!({"kind":"target_attempt_finished","model_turn_id":original["model_turn_id"],"attempt_id":original["attempt_id"],"status":status.unwrap_or_else(||"interrupted".into()),"status_code":null,"error_code":null,"duration_ms":null,"first_token_ms":null,"usage":original["usage"],"legacy_lifecycle":[{"sequence":sequence,"occurred_at":occurred,"payload":original}]});
                    if let Some(projection) = projection {
                        value["status_code"] =
                            json!(projection.try_get::<Option<i64>, _>("status_code")?);
                        value["error_code"] =
                            json!(projection.try_get::<Option<String>, _>("error_code")?);
                        value["duration_ms"] =
                            json!(projection.try_get::<Option<i64>, _>("duration_ms")?);
                        value["first_token_ms"] =
                            json!(projection.try_get::<Option<i64>, _>("first_token_ms")?);
                    }
                    kind = "target_attempt_finished".into();
                }
                "delivery_finished" => {
                    let original = value.clone();
                    value = json!({"kind":"run_finished","status":original["status"],"reason":original["reason"],"delivery_completed_at":if original["status"] == "delivered" { json!(occurred) } else { Value::Null },"delivery":{"status":original["status"],"reason":original["reason"],"completed_at":occurred},"legacy_lifecycle":[{"sequence":sequence,"occurred_at":occurred,"payload":original}]});
                    if let Some(projection) = sqlx::query("SELECT r.status,r.terminal_reason,r.generation_node_id,i.generation_root_id,r.finished_at FROM inference_run_observations r JOIN interaction_observations i ON i.id=r.interaction_id WHERE r.id=$1").bind(&run).fetch_optional(&mut *tx).await? {
                        value["status"] = projection.try_get::<String,_>("status")?.into();
                        value["terminal_reason"] = json!(projection.try_get::<Option<String>,_>("terminal_reason")?);
                        value["generation_node_id"] = json!(projection.try_get::<Option<String>,_>("generation_node_id")?);
                        value["generation_root_id"] = json!(projection.try_get::<Option<String>,_>("generation_root_id")?);
                        value["finished_at"] = json!(projection.try_get::<Option<i64>,_>("finished_at")?);
                    } else {
                        value["terminal_reason"] = value["reason"].clone();
                        value["generation_node_id"] = Value::Null; value["generation_root_id"] = Value::Null;
                    }
                    kind = "run_finished".into();
                }
                "generation_associated" | "client_output_committed" | "process_restarted" => {
                    value = json!({"kind":"run_state_changed","status":value["status"],"reason":value["reason"],"legacy_lifecycle":[{"sequence":sequence,"occurred_at":occurred,"kind":kind,"payload":value}]});
                    kind = "run_state_changed".into();
                }
                _ => {}
            }
            // Merge removed lifecycle facts into a durable event, preserving original identity and time.
            let original_kind: String = row.try_get("kind")?;
            let obsolete = matches!(
                original_kind.as_str(),
                "usage_confirmed"
                    | "delivery_finished"
                    | "generation_associated"
                    | "client_output_committed"
            );
            let mut merged = false;
            if obsolete {
                for legacy in [false, true] {
                    let candidates = if legacy {
                        sqlx::query("SELECT sequence,kind,payload::text AS payload FROM observation_events_legacy WHERE run_id=$1 AND sequence<>$2 AND kind NOT IN ('trace_manifest_updated','usage_confirmed','delivery_finished','generation_associated','client_output_committed','process_restarted') AND (($3='usage_confirmed' AND kind='target_attempt_finished') OR ($3='delivery_finished' AND kind='run_finished') OR ($3='client_output_committed' AND kind='run_admitted') OR $3='generation_associated') ORDER BY sequence LIMIT CASE WHEN $3='generation_associated' THEN 1 ELSE NULL END").bind(&run).bind(sequence).bind(&original_kind).fetch_all(&mut *tx).await?
                    } else {
                        sqlx::query("SELECT sequence,kind,payload FROM observation_events WHERE run_id=$1 AND (($2='usage_confirmed' AND kind='target_attempt_finished') OR ($2='delivery_finished' AND kind='run_finished') OR ($2='client_output_committed' AND kind='run_admitted') OR $2='generation_associated') ORDER BY sequence LIMIT CASE WHEN $2='generation_associated' THEN 1 ELSE NULL END").bind(&run).bind(&original_kind).fetch_all(&mut *tx).await?
                    };
                    for candidate in candidates {
                        let candidate_kind: String = candidate.try_get("kind")?;
                        let mut target: Value = if legacy {
                            blocking_chunks(
                                vec![candidate.try_get::<String, _>("payload")?],
                                decode_legacy,
                            )
                            .await?
                            .pop()
                            .context("missing decoded lifecycle candidate")?
                            .1
                        } else {
                            decode_event(candidate.try_get::<Vec<u8>, _>("payload")?).await?
                        };
                        if original_kind == "client_output_committed"
                            && candidate_kind != "run_admitted"
                        {
                            continue;
                        }
                        if original_kind == "usage_confirmed"
                            && (candidate_kind != "target_attempt_finished"
                                || target["attempt_id"] != value["attempt_id"]
                                || target["model_turn_id"] != value["model_turn_id"]
                                || !target["usage"].is_null())
                        {
                            continue;
                        }
                        if original_kind == "delivery_finished"
                            && (candidate_kind != "run_finished" || !target["delivery"].is_null())
                        {
                            continue;
                        }
                        if original_kind == "usage_confirmed" {
                            target["usage"] = value["usage"].clone();
                        }
                        if original_kind == "delivery_finished" {
                            target["delivery"] = value["delivery"].clone();
                            if target["delivery_completed_at"].is_null() {
                                target["delivery_completed_at"] =
                                    value["delivery_completed_at"].clone();
                            }
                            sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,$1) WHERE id=$2").bind(target["delivery_completed_at"].as_i64()).bind(&run).execute(&mut *tx).await?;
                        }
                        if original_kind == "client_output_committed" {
                            let previous = target["client_output_committed_sequence"]
                                .as_i64()
                                .unwrap_or(sequence);
                            target["client_output_committed_sequence"] =
                                previous.min(sequence).into();
                        }
                        if target.get("legacy_lifecycle").is_none() {
                            target["legacy_lifecycle"] = json!([]);
                        }
                        target["legacy_lifecycle"].as_array_mut().context("invalid legacy lifecycle facts")?.push(json!({"sequence":sequence,"occurred_at":occurred,"kind":original_kind,"payload":serde_json::from_str::<Value>(&current)?}));
                        let id: i64 = candidate.try_get("sequence")?;
                        if legacy {
                            mutated_sequences.insert(id);
                            sqlx::query("UPDATE observation_events_legacy SET payload=$1::jsonb WHERE sequence=$2").bind(legacy_json(target).await?).bind(id).execute(&mut *tx).await?;
                        } else {
                            sqlx::query(
                                "UPDATE observation_events SET payload=$1 WHERE sequence=$2",
                            )
                            .bind(encode_event(target).await?.1)
                            .bind(id)
                            .execute(&mut *tx)
                            .await?;
                        }
                        merged = true;
                        break;
                    }
                    if merged {
                        break;
                    }
                }
            }
            if original_kind == "client_output_committed" {
                ensure!(merged, "legacy client commit has no admission event");
                if let Some(terminal) = sqlx::query("SELECT occurred_at,payload FROM observation_events WHERE run_id=$1 AND sequence<$2 AND kind='run_finished' ORDER BY sequence DESC LIMIT 1").bind(&run).bind(sequence).fetch_optional(&mut *tx).await? {
                    value = decode_event(terminal.try_get::<Vec<u8>,_>("payload")?).await?;
                    if value.get("finished_at").is_none() { value["finished_at"] = terminal.try_get::<i64,_>("occurred_at")?.into(); }
                    value["client_output_committed"] = true.into();
                    kind = "run_finished".into();
                    merged = false;
                }
            }
            if kind != "trace_manifest_updated" && !merged {
                if kind == "target_attempt_finished" && value.get("usage").is_none() {
                    value["usage"] = Value::Null;
                }
                if kind == "run_finished" {
                    let commits = sqlx::query("SELECT payload FROM observation_events WHERE run_id=$1 AND sequence<=$2 AND kind='run_admitted'").bind(&run).bind(sequence).fetch_all(&mut *tx).await?;
                    for commit in commits {
                        let fact = decode_event(commit.try_get::<Vec<u8>, _>("payload")?).await?;
                        if legacy_committed(&fact, sequence) {
                            value["client_output_committed"] = true.into();
                        }
                    }
                    if value.get("delivery").is_none() {
                        value["delivery"] = Value::Null;
                    }
                    if let Some(completed) = value["delivery_completed_at"].as_i64() {
                        sqlx::query("UPDATE inference_run_observations SET delivery_completed_at=COALESCE(delivery_completed_at,$1) WHERE id=$2").bind(completed).bind(&run).execute(&mut *tx).await?;
                    }
                }
                let (encoded_value, bytes) = encode_event(value).await?;
                value = encoded_value;
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,interaction_id,run_id,rejection_id,kind,payload,expires_at,tool_id,operation_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                    .bind(sequence).bind(if matches!(original_kind.as_str(), "delivery_finished" | "client_output_committed") { value["finished_at"].as_i64().unwrap_or(occurred) } else { occurred }).bind(row.try_get::<Option<String>,_>("interaction_id")?).bind(&run)
                    .bind(row.try_get::<Option<String>,_>("rejection_id")?).bind(&kind).bind(bytes).bind(row.try_get::<i64,_>("expires_at")?)
                    .bind(value["tool_id"].as_str()).bind(value["operation_id"].as_str()).execute(&mut *tx).await?;
            }
            sqlx::query("DELETE FROM observation_events_legacy WHERE sequence=$1")
                .bind(sequence)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        converted += batch_size;
        crate::startup_progress::report(
            "observation_data",
            "Converting observation events",
            converted as u64,
            Some(total),
        );
    }
    Ok(())
}

/// 尝试行的错误来源/消息/上游错误码只存在于事件载荷中；迁移新增列后按载荷回填。
/// 只有保留期内的失败尝试可恢复，且仅在来源仍未知时写入，重复启动保持幂等。
type AttemptErrorDetails = (String, Option<String>, Option<String>);

fn decode_attempt_finished_payload(payload: Vec<u8>) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&crate::storage_codec::decode(
        &payload,
    )?)?)
}

fn record_attempt_error(
    latest: &mut std::collections::HashMap<String, AttemptErrorDetails>,
    payload: &Value,
) {
    let Some(attempt_id) = payload["attempt_id"].as_str() else {
        return;
    };
    let error = &payload["error"];
    let Some(source) = error["source"].as_str().filter(|source| !source.is_empty()) else {
        // 终态修订可能撤销先前的错误诊断；以最后一条载荷为准。
        latest.remove(attempt_id);
        return;
    };
    latest.insert(
        attempt_id.to_owned(),
        (
            source.to_owned(),
            error["message"].as_str().map(str::to_owned),
            error["upstream_code"].as_str().map(str::to_owned),
        ),
    );
}

pub(crate) async fn backfill_attempt_error_details_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    let now = super::writer::now();
    let mut latest = std::collections::HashMap::new();
    let mut after = i64::MIN;
    loop {
        let rows = sqlx::query(
            "SELECT e.sequence,e.payload FROM target_attempt_observations a
             JOIN inference_run_observations r ON r.id=a.run_id
             JOIN observation_events e ON e.run_id=a.run_id AND e.kind='target_attempt_finished'
             WHERE a.status='failed' AND a.error_source IS NULL AND r.expires_at>?1 AND e.expires_at>?1 AND e.sequence>?2
             ORDER BY e.sequence LIMIT 500",
        )
        .bind(now)
        .bind(after)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        let mut payloads = Vec::with_capacity(rows.len());
        for row in &rows {
            after = row.try_get("sequence")?;
            payloads.push(row.try_get::<Vec<u8>, _>("payload")?);
        }
        for payload in blocking_chunks(payloads, decode_attempt_finished_payload).await? {
            record_attempt_error(&mut latest, &payload);
        }
    }
    if latest.is_empty() {
        return Ok(());
    }
    let mut tx = connection.begin().await?;
    for (attempt_id, (source, message, upstream_code)) in latest {
        sqlx::query("UPDATE target_attempt_observations SET error_source=?1,error_message=?2,upstream_error_code=?3 WHERE id=?4 AND status='failed' AND error_source IS NULL")
            .bind(source)
            .bind(message)
            .bind(upstream_code)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn backfill_attempt_error_details_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    let now = super::writer::now();
    let mut latest = std::collections::HashMap::new();
    let mut after = i64::MIN;
    loop {
        let rows = sqlx::query(
            "SELECT e.sequence,e.payload FROM target_attempt_observations a
             JOIN inference_run_observations r ON r.id=a.run_id
             JOIN observation_events e ON e.run_id=a.run_id AND e.kind='target_attempt_finished'
             WHERE a.status='failed' AND a.error_source IS NULL AND r.expires_at>$1 AND e.expires_at>$1 AND e.sequence>$2
             ORDER BY e.sequence LIMIT 500",
        )
        .bind(now)
        .bind(after)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        let mut payloads = Vec::with_capacity(rows.len());
        for row in &rows {
            after = row.try_get("sequence")?;
            payloads.push(row.try_get::<Vec<u8>, _>("payload")?);
        }
        for payload in blocking_chunks(payloads, decode_attempt_finished_payload).await? {
            record_attempt_error(&mut latest, &payload);
        }
    }
    if latest.is_empty() {
        return Ok(());
    }
    let mut tx = connection.begin().await?;
    for (attempt_id, (source, message, upstream_code)) in latest {
        sqlx::query("UPDATE target_attempt_observations SET error_source=$1,error_message=$2,upstream_error_code=$3 WHERE id=$4 AND status='failed' AND error_source IS NULL")
            .bind(source)
            .bind(message)
            .bind(upstream_code)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

const CREDENTIAL_CLEANUP_BATCH: usize = 500;

enum RetiredPayload {
    Unchanged,
    Rewritten(Vec<u8>),
    Empty,
}

fn is_kingfisher_rule(rule: &str) -> bool {
    rule.starts_with("kingfisher.")
}

/// 就地移除已下线规则的归因；返回是否改写，以及移除后是否已无规则与位置。
fn retire_kingfisher_discovery(discovery: &mut Value) -> bool {
    let Some(object) = discovery.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if let Some(Value::Array(rule_ids)) = object.get_mut("rule_ids") {
        let before = rule_ids.len();
        rule_ids.retain(|rule| !rule.as_str().is_some_and(is_kingfisher_rule));
        changed |= rule_ids.len() != before;
    }
    let locators_changed = if let Some(Value::Array(locators)) = object.get_mut("locators") {
        let before = locators.len();
        locators.retain(|locator| {
            !locator
                .get("rule_id")
                .and_then(Value::as_str)
                .is_some_and(is_kingfisher_rule)
        });
        let changed = locators.len() != before;
        if changed {
            let sources: std::collections::BTreeSet<String> = locators
                .iter()
                .filter_map(|locator| {
                    locator
                        .get("source_type")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect();
            object.insert(
                "source_types".into(),
                Value::Array(sources.into_iter().map(Value::String).collect()),
            );
        }
        changed
    } else {
        false
    };
    changed || locators_changed
}

fn retire_kingfisher_payload(value: Value) -> anyhow::Result<RetiredPayload> {
    let mut value = value;
    let Some(discoveries) = value.get_mut("discoveries").and_then(Value::as_array_mut) else {
        return Ok(RetiredPayload::Unchanged);
    };
    let mut changed = false;
    let mut kept = Vec::with_capacity(discoveries.len());
    for mut discovery in std::mem::take(discoveries) {
        let discovery_changed = retire_kingfisher_discovery(&mut discovery);
        let empty = discovery
            .get("rule_ids")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
            && discovery
                .get("locators")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
        if discovery_changed && empty {
            changed = true;
            continue;
        }
        changed |= discovery_changed;
        kept.push(discovery);
    }
    *discoveries = kept;
    if !changed {
        return Ok(RetiredPayload::Unchanged);
    }
    let empty = discoveries.is_empty();
    if empty {
        return Ok(RetiredPayload::Empty);
    }
    Ok(RetiredPayload::Rewritten(crate::storage_codec::encode(
        serde_json::to_vec(&value)?.into(),
    )?))
}

fn decode_stored_event(bytes: Vec<u8>) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&crate::storage_codec::decode(
        &bytes,
    )?)?)
}

/// 分批清理 SQLite 中的下线规则；每批一个事务，事件删除后 sequence 游标不回退。
pub(crate) async fn retire_kingfisher_discoveries_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<()> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused')",
    )
    .fetch_one(&mut *connection)
    .await?;
    let mut after = i64::MIN;
    let mut cleaned = 0i64;
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query(
            "SELECT sequence,payload FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused') AND sequence>? ORDER BY sequence LIMIT ?",
        )
        .bind(after)
        .bind(CREDENTIAL_CLEANUP_BATCH as i64)
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.commit().await?;
            break;
        }
        let mut sequences = Vec::with_capacity(rows.len());
        let mut payloads = Vec::with_capacity(rows.len());
        for row in rows {
            sequences.push(row.try_get::<i64, _>("sequence")?);
            payloads.push(row.try_get::<Vec<u8>, _>("payload")?);
        }
        after = *sequences
            .last()
            .context("credential cleanup batch is empty")?;
        let values = blocking_chunks(payloads, decode_stored_event).await?;
        let retired = blocking_chunks(values, retire_kingfisher_payload).await?;
        for (sequence, retired) in sequences.into_iter().zip(retired) {
            match retired {
                RetiredPayload::Unchanged => {}
                RetiredPayload::Rewritten(payload) => {
                    sqlx::query("UPDATE observation_events SET payload=? WHERE sequence=?")
                        .bind(payload)
                        .bind(sequence)
                        .execute(&mut *tx)
                        .await?;
                    cleaned += 1;
                }
                RetiredPayload::Empty => {
                    sqlx::query("DELETE FROM observation_events WHERE sequence=?")
                        .bind(sequence)
                        .execute(&mut *tx)
                        .await?;
                    cleaned += 1;
                }
            }
        }
        tx.commit().await?;
        crate::startup_progress::report(
            "credential_cleanup",
            "Removing retired credential rules",
            cleaned as u64,
            Some(total.max(0) as u64),
        );
    }
    Ok(())
}

/// 分批清理 PostgreSQL 中的下线规则；事务与游标语义同 SQLite。
pub(crate) async fn retire_kingfisher_discoveries_postgres(
    connection: &mut sqlx::PgConnection<'_>,
) -> anyhow::Result<()> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused')",
    )
    .fetch_one(&mut *connection)
    .await?;
    let mut after = i64::MIN;
    let mut cleaned = 0i64;
    loop {
        let mut tx = connection.begin().await?;
        let rows = sqlx::query(
            "SELECT sequence,payload FROM observation_events WHERE kind IN ('credential_mappings_created','credential_mappings_reused') AND sequence>$1 ORDER BY sequence LIMIT $2",
        )
        .bind(after)
        .bind(CREDENTIAL_CLEANUP_BATCH as i64)
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.commit().await?;
            break;
        }
        let mut sequences = Vec::with_capacity(rows.len());
        let mut payloads = Vec::with_capacity(rows.len());
        for row in rows {
            sequences.push(row.try_get::<i64, _>("sequence")?);
            payloads.push(row.try_get::<Vec<u8>, _>("payload")?);
        }
        after = *sequences
            .last()
            .context("credential cleanup batch is empty")?;
        let values = blocking_chunks(payloads, decode_stored_event).await?;
        let retired = blocking_chunks(values, retire_kingfisher_payload).await?;
        for (sequence, retired) in sequences.into_iter().zip(retired) {
            match retired {
                RetiredPayload::Unchanged => {}
                RetiredPayload::Rewritten(payload) => {
                    sqlx::query("UPDATE observation_events SET payload=$1 WHERE sequence=$2")
                        .bind(payload)
                        .bind(sequence)
                        .execute(&mut *tx)
                        .await?;
                    cleaned += 1;
                }
                RetiredPayload::Empty => {
                    sqlx::query("DELETE FROM observation_events WHERE sequence=$1")
                        .bind(sequence)
                        .execute(&mut *tx)
                        .await?;
                    cleaned += 1;
                }
            }
        }
        tx.commit().await?;
        crate::startup_progress::report(
            "credential_cleanup",
            "Removing retired credential rules",
            cleaned as u64,
            Some(total.max(0) as u64),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_snapshot_mutations_survive_failed_batch_and_resume() -> anyhow::Result<()> {
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::raw_sql("CREATE TABLE observation_events_legacy(sequence INTEGER PRIMARY KEY,occurred_at INTEGER,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT,payload TEXT,expires_at INTEGER); CREATE TABLE observation_events(sequence INTEGER PRIMARY KEY,occurred_at INTEGER,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT,payload BLOB,expires_at INTEGER,tool_id TEXT,operation_id TEXT); CREATE TABLE target_attempt_observations(id TEXT PRIMARY KEY,status TEXT,status_code INTEGER,error_code TEXT,duration_ms INTEGER,first_token_ms INTEGER); INSERT INTO target_attempt_observations VALUES ('attempt','succeeded',200,NULL,10,2)")
            .execute(&mut connection).await?;
        for sequence in 1..=200_i64 {
            sqlx::query("INSERT INTO observation_events_legacy VALUES (?,?,'i','run',NULL,'client_visible_content_delta',?,9999)")
                .bind(sequence).bind(sequence)
                .bind(json!({"kind":"client_visible_content_delta","text":sequence.to_string()}).to_string())
                .execute(&mut connection).await?;
        }
        for (sequence, kind, payload) in [
            (201_i64, "usage_confirmed", json!({"kind":"usage_confirmed","attempt_id":"attempt","model_turn_id":"turn","usage":{"input_tokens":7}}).to_string()),
            (202, "target_attempt_finished", json!({"kind":"target_attempt_finished","attempt_id":"attempt","model_turn_id":"other","usage":null}).to_string()),
            (203, "target_attempt_finished", json!({"kind":"target_attempt_finished","attempt_id":"attempt","model_turn_id":"turn","usage":null}).to_string()),
            (399, "usage_confirmed", json!({"kind":"usage_confirmed","attempt_id":"attempt","model_turn_id":"later","usage":{"input_tokens":9}}).to_string()),
            (401, "target_attempt_finished", json!({"kind":"target_attempt_finished","attempt_id":"attempt","model_turn_id":"later","usage":null}).to_string()),
            (402, "client_visible_content_delta", "{broken".into()),
        ] {
            sqlx::query("INSERT INTO observation_events_legacy VALUES (?,?,'i','run',NULL,?,?,9999)")
                .bind(sequence).bind(sequence).bind(kind).bind(payload)
                .execute(&mut connection).await?;
        }
        for sequence in (204..=400_i64).filter(|sequence| *sequence != 399) {
            sqlx::query("INSERT INTO observation_events_legacy VALUES (?,?,'i','run',NULL,'client_visible_content_delta',?,9999)")
                .bind(sequence).bind(sequence)
                .bind(json!({"kind":"client_visible_content_delta","text":sequence.to_string()}).to_string())
                .execute(&mut connection).await?;
        }
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = events.clone();
        assert!(
            crate::startup_progress::observe_startup(
                move |event| observed.lock().unwrap().push(event),
                convert_event_storage_sqlite(&mut connection),
            )
            .await
            .is_err()
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .map(|event| (event.completed, event.total))
                .collect::<Vec<_>>(),
            vec![(0, Some(402)), (200, Some(402)), (400, Some(402))]
        );
        let committed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observation_events")
            .fetch_one(&mut connection)
            .await?;
        assert_eq!(committed, 398);
        let remaining: Vec<i64> =
            sqlx::query_scalar("SELECT sequence FROM observation_events_legacy ORDER BY sequence")
                .fetch_all(&mut connection)
                .await?;
        assert_eq!(remaining, vec![401, 402]);
        sqlx::query("UPDATE observation_events_legacy SET payload=? WHERE sequence=402")
            .bind(json!({"kind":"client_visible_content_delta","text":"repaired"}).to_string())
            .execute(&mut connection)
            .await?;
        events.lock().unwrap().clear();
        let observed = events.clone();
        crate::startup_progress::observe_startup(
            move |event| observed.lock().unwrap().push(event),
            convert_event_storage_sqlite(&mut connection),
        )
        .await?;
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.total.is_some())
                .map(|event| (event.completed, event.total))
                .collect::<Vec<_>>(),
            vec![(0, Some(2)), (2, Some(2))]
        );
        convert_event_storage_sqlite(&mut connection).await?;
        let payloads: Vec<Vec<u8>> = sqlx::query_scalar("SELECT payload FROM observation_events WHERE kind='target_attempt_finished' ORDER BY sequence")
            .fetch_all(&mut connection).await?;
        let values = payloads
            .into_iter()
            .map(|bytes| {
                Ok(serde_json::from_slice::<Value>(
                    &crate::storage_codec::decode(&bytes)?,
                )?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(values[0]["usage"], Value::Null);
        assert_eq!(values[1]["usage"], json!({"input_tokens":7}));
        assert_eq!(values[1]["legacy_lifecycle"][0]["sequence"], json!(201));
        assert_eq!(
            values[1]["legacy_lifecycle"][0]["payload"]["attempt_id"],
            json!("attempt")
        );
        assert_eq!(values[2]["usage"], json!({"input_tokens":9}));
        assert_eq!(values[2]["legacy_lifecycle"][0]["sequence"], json!(399));
        Ok(())
    }

    #[tokio::test]
    async fn manifest_worker_failures_join_all_started_exports() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let manifests = vec![
            json!({"trace_id":"../invalid","tombstoned":false}),
            json!({"trace_id":"valid","tombstoned":false}),
        ];
        assert!(export_manifests(root.path(), manifests).await.is_err());
        // A single worker stops its chunk on the first error. With multiple workers,
        // the independent valid export has completed before the error is returned.
        if worker_count() > 1 {
            let target = root.path().join("observation-debug/valid/manifest.json");
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(target)?)?,
                json!({"trace_id":"valid","tombstoned":false})
            );
        }
        assert!(!root.path().join("invalid").exists());
        Ok(())
    }

    #[tokio::test]
    async fn legacy_chunks_and_delivery_facts_survive_resumed_conversion() -> anyhow::Result<()> {
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE observation_events_legacy(sequence INTEGER PRIMARY KEY,occurred_at INTEGER,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT,payload TEXT,expires_at INTEGER); CREATE TABLE observation_events(sequence INTEGER PRIMARY KEY,occurred_at INTEGER,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT,payload BLOB,expires_at INTEGER,tool_id TEXT,operation_id TEXT); CREATE TABLE inference_run_observations(id TEXT PRIMARY KEY,interaction_id TEXT,status TEXT,terminal_reason TEXT,generation_node_id TEXT,finished_at INTEGER,delivery_completed_at INTEGER); CREATE TABLE interaction_observations(id TEXT PRIMARY KEY,generation_root_id TEXT)").execute(&mut connection).await?;
        sqlx::query("INSERT INTO interaction_observations VALUES ('i',NULL)")
            .execute(&mut connection)
            .await?;
        let mut sequence = 0_i64;
        for (run, status, original_timestamp) in [
            ("delivered", "delivered", Some(91_i64)),
            ("late", "delivered", Some(91_i64)),
            ("failed", "failed", None),
            ("cancelled", "cancelled", None),
        ] {
            sqlx::query(
                "INSERT INTO inference_run_observations VALUES (?,'i',?,NULL,NULL,100,NULL)",
            )
            .bind(run)
            .bind(status)
            .execute(&mut connection)
            .await?;
            let mut facts = vec![("run_admitted", json!({"kind":"run_admitted"}))];
            for chunk in 0..205 {
                facts.push(("client_visible_content_delta", json!({"kind":"client_visible_content_delta","block_id":format!("random-{chunk}"),"text":format!("{chunk}:{}", "x".repeat(1024)),"model_turn_id":"turn","attempt_id":"attempt"})));
            }
            if run == "delivered" {
                facts.push((
                    "client_output_committed",
                    json!({"kind":"client_output_committed"}),
                ));
            }
            facts.push((
                "delivery_finished",
                json!({"kind":"delivery_finished","status":status,"reason":null}),
            ));
            facts.push(("run_finished", json!({"kind":"run_finished","status":status,"terminal_reason":null,"delivery_completed_at":original_timestamp})));
            if run == "late" {
                facts.push((
                    "client_output_committed",
                    json!({"kind":"client_output_committed"}),
                ));
            }
            for (kind, payload) in facts {
                sequence += 1;
                sqlx::query(
                    "INSERT INTO observation_events_legacy VALUES (?,?,'i',?,NULL,?,?,99999)",
                )
                .bind(sequence)
                .bind(sequence)
                .bind(run)
                .bind(kind)
                .bind(payload.to_string())
                .execute(&mut connection)
                .await?;
            }
        }
        convert_event_storage_sqlite(&mut connection).await?;
        convert_event_storage_sqlite(&mut connection).await?;
        for (run, expected) in [
            ("delivered", Some(91_i64)),
            ("late", Some(91_i64)),
            ("failed", None),
            ("cancelled", None),
        ] {
            let timestamp: Option<i64> = sqlx::query_scalar(
                "SELECT delivery_completed_at FROM inference_run_observations WHERE id=?",
            )
            .bind(run)
            .fetch_one(&mut connection)
            .await?;
            assert_eq!(timestamp, expected);
            let rows = sqlx::query(
                "SELECT kind,payload FROM observation_events WHERE run_id=? ORDER BY sequence",
            )
            .bind(run)
            .fetch_all(&mut connection)
            .await?;
            let mut text = String::new();
            let mut terminal_flags = Vec::new();
            for row in rows {
                let value: Value = serde_json::from_slice(&crate::storage_codec::decode(
                    &row.try_get::<Vec<u8>, _>("payload")?,
                )?)?;
                match row.try_get::<String, _>("kind")?.as_str() {
                    "client_visible_content" => {
                        assert!(value.get("block_id").is_none());
                        assert!(value.get("item").is_none());
                        assert!(value["legacy_text"]["block_id"].is_string());
                        text.push_str(value["text"].as_str().unwrap());
                    }
                    "run_finished" => {
                        assert_eq!(value["delivery_completed_at"], json!(expected));
                        terminal_flags
                            .push(value["client_output_committed"].as_bool().unwrap_or(false));
                    }
                    "run_state_changed" | "client_output_committed" => {
                        panic!("commit signal must not become an alias row")
                    }
                    _ => {}
                }
            }
            assert_eq!(
                terminal_flags,
                if run == "late" {
                    vec![false, true]
                } else {
                    vec![run == "delivered"]
                }
            );
            assert_eq!(
                text,
                (0..205)
                    .map(|chunk| format!("{chunk}:{}", "x".repeat(1024)))
                    .collect::<String>()
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn legacy_delivery_postgres_when_configured() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("DB_URL") else {
            return Ok(());
        };
        let mut connection = sqlx::PgConnection::connect(&url).await?;
        sqlx::raw_sql("CREATE TEMP TABLE observation_events_legacy(sequence BIGINT PRIMARY KEY,occurred_at BIGINT,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT,payload JSONB,expires_at BIGINT); CREATE TEMP TABLE observation_events(sequence BIGINT PRIMARY KEY,occurred_at BIGINT,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT,payload BYTEA,expires_at BIGINT,tool_id TEXT,operation_id TEXT); CREATE TEMP TABLE inference_run_observations(id TEXT PRIMARY KEY,interaction_id TEXT,status TEXT,terminal_reason TEXT,generation_node_id TEXT,finished_at BIGINT,delivery_completed_at BIGINT); CREATE TEMP TABLE interaction_observations(id TEXT PRIMARY KEY,generation_root_id TEXT); INSERT INTO interaction_observations VALUES ('i',NULL)").execute(&mut connection).await?;
        for (index, status) in ["delivered", "failed", "cancelled"].into_iter().enumerate() {
            sqlx::query(
                "INSERT INTO inference_run_observations VALUES ($1,'i',$1,NULL,NULL,100,NULL)",
            )
            .bind(status)
            .execute(&mut connection)
            .await?;
            let original = if status == "delivered" {
                Some(91_i64)
            } else {
                None
            };
            for (offset, kind, payload) in [
                (
                    1_i64,
                    "delivery_finished",
                    json!({"kind":"delivery_finished","status":status,"reason":null}),
                ),
                (
                    2,
                    "run_finished",
                    json!({"kind":"run_finished","status":status,"delivery_completed_at":original}),
                ),
            ] {
                sqlx::query("INSERT INTO observation_events_legacy VALUES ($1,$1,'i',$2,NULL,$3,$4::jsonb,99999)").bind(index as i64 * 10 + offset).bind(status).bind(kind).bind(payload.to_string()).execute(&mut connection).await?;
            }
        }
        convert_event_storage_postgres(&mut connection).await?;
        convert_event_storage_postgres(&mut connection).await?;
        for status in ["delivered", "failed", "cancelled"] {
            let timestamp: Option<i64> = sqlx::query_scalar(
                "SELECT delivery_completed_at FROM inference_run_observations WHERE id=$1",
            )
            .bind(status)
            .fetch_one(&mut connection)
            .await?;
            assert_eq!(
                timestamp,
                if status == "delivered" {
                    Some(91)
                } else {
                    None
                }
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn resumed_export_requires_exact_authoritative_facts() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE debug_trace_manifests (trace_id TEXT PRIMARY KEY, run_id TEXT, rejection_id TEXT, status TEXT, bytes_written INTEGER, event_count INTEGER, partial_reason TEXT, created_at INTEGER, completed_at INTEGER, expires_at INTEGER, tombstoned INTEGER)").execute(&mut connection).await?;
        sqlx::query("INSERT INTO debug_trace_manifests VALUES ('legacy','owner',NULL,'complete',123,4,NULL,10,20,30,0)").execute(&mut connection).await?;
        export_debug_manifests_sqlite(&mut connection, Some(root.path())).await?;
        let path = root.path().join("observation-debug/legacy/manifest.json");
        let original = std::fs::read(&path)?;
        export_debug_manifests_sqlite(&mut connection, Some(root.path())).await?;
        assert_eq!(std::fs::read(&path)?, original);
        let authoritative: Value = serde_json::from_slice(&original)?;
        let mut invalid = vec![b"{corrupt".to_vec()];
        for (key, value) in [
            ("trace_id", json!("other")),
            ("relative_directory", json!("other")),
            ("schema_version", json!(1)),
            ("run_id", json!("wrong-owner")),
            ("rejection_id", json!("wrong-rejection")),
            ("bytes_written", json!(124)),
            ("event_count", json!(5)),
            ("status", json!("partial")),
            ("completed_at", json!(21)),
            ("reasons", json!(["wrong-fact"])),
        ] {
            let mut altered = authoritative.clone();
            altered[key] = value;
            invalid.push(serde_json::to_vec(&altered)?);
        }
        for bytes in invalid {
            std::fs::write(&path, &bytes)?;
            assert!(
                export_debug_manifests_sqlite(&mut connection, Some(root.path()))
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path)?, bytes);
            let source: (String, i64, i64) = sqlx::query_as("SELECT run_id,bytes_written,event_count FROM debug_trace_manifests WHERE trace_id='legacy'").fetch_one(&mut connection).await?;
            assert_eq!(source, ("owner".into(), 123, 4));
        }
        std::fs::write(&path, &original)?;
        export_debug_manifests_sqlite(&mut connection, Some(root.path())).await?;
        assert_eq!(std::fs::read(&path)?, original);
        Ok(())
    }

    fn retired_kingfisher_fixture() -> [(i64, &'static str, Vec<u8>); 7] {
        let encode = |value: Value| {
            crate::storage_codec::encode(serde_json::to_vec(&value).unwrap().into()).unwrap()
        };
        [
            (
                1,
                "credential_mappings_created",
                encode(json!({
                    "kind": "credential_mappings_created",
                    "discoveries": [
                        {
                            "rule_ids": ["kingfisher.aws", "github-pat"],
                            "source_types": ["tool_result", "tool_arguments"],
                            "locators": [
                                {"rule_id": "kingfisher.aws", "source_type": "tool_result", "tool_id": "call-1"},
                                {"rule_id": "github-pat", "source_type": "tool_arguments", "tool_id": "call-1"}
                            ],
                            "marker": "m-keep"
                        },
                        {
                            "rule_ids": ["kingfisher.azure"],
                            "source_types": ["tool_result"],
                            "locators": [
                                {"rule_id": "kingfisher.azure", "source_type": "tool_result", "tool_id": "call-2"}
                            ],
                            "marker": "m-drop"
                        }
                    ]
                })),
            ),
            (
                2,
                "credential_mappings_reused",
                encode(json!({
                    "kind": "credential_mappings_reused",
                    "discoveries": [{
                        "rule_ids": ["kingfisher.gcp", "custom.rule"],
                        "source_types": ["user_message", "tool_result"],
                        "locators": [
                            {"rule_id": "kingfisher.gcp", "source_type": "user_message", "latest_input": true},
                            {"rule_id": "custom.rule", "source_type": "tool_result", "tool_id": "call-3"}
                        ],
                        "marker": "m-keep"
                    }]
                })),
            ),
            (
                3,
                "credential_mappings_created",
                encode(json!({
                    "kind": "credential_mappings_created",
                    "discoveries": [{
                        "rule_ids": ["github-pat"],
                        "source_types": ["user_message"],
                        "locators": [{"rule_id": "github-pat", "source_type": "user_message", "latest_input": true}],
                        "marker": "m-untouched"
                    }]
                })),
            ),
            (
                4,
                "credential_mappings_created",
                encode(json!({
                    "kind": "credential_mappings_created",
                    "discoveries": [{
                        "rule_ids": ["kingfisher.only"],
                        "source_types": ["system"],
                        "locators": [{"rule_id": "kingfisher.only", "source_type": "system"}],
                        "marker": "m-empty"
                    }]
                })),
            ),
            (
                5,
                "run_admitted",
                encode(json!({"kind": "run_admitted", "status": "running"})),
            ),
            (
                6,
                "credential_mappings_created",
                encode(json!({
                    "kind": "credential_mappings_created",
                    "discoveries": [{
                        "rule_ids": ["kingfisher.aws", "github-pat"],
                        "source_types": ["user_message"],
                        "marker": "m-rules-only"
                    }]
                })),
            ),
            (
                7,
                "credential_mappings_created",
                encode(json!({
                    "kind": "credential_mappings_created",
                    "discoveries": [{
                        "rule_ids": ["kingfisher.aws"],
                        "source_types": ["user_message"],
                        "marker": "m-rules-empty"
                    }]
                })),
            ),
        ]
    }

    fn assert_retired_kingfisher_cleanup(
        first: &[(i64, Vec<u8>)],
        untouched: &[u8],
    ) -> anyhow::Result<()> {
        assert_eq!(
            first
                .iter()
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3, 5, 6]
        );
        let decode = |payload: Vec<u8>| -> anyhow::Result<Value> {
            Ok(serde_json::from_slice(&crate::storage_codec::decode(
                &payload,
            )?)?)
        };
        let kept = decode(first[0].1.clone())?;
        assert_eq!(kept["discoveries"].as_array().unwrap().len(), 1);
        assert_eq!(kept["discoveries"][0]["rule_ids"], json!(["github-pat"]));
        assert_eq!(
            kept["discoveries"][0]["locators"],
            json!([{"rule_id": "github-pat", "source_type": "tool_arguments", "tool_id": "call-1"}])
        );
        assert_eq!(
            kept["discoveries"][0]["source_types"],
            json!(["tool_arguments"])
        );
        let reused = decode(first[1].1.clone())?;
        assert_eq!(reused["discoveries"][0]["rule_ids"], json!(["custom.rule"]));
        assert_eq!(
            reused["discoveries"][0]["source_types"],
            json!(["tool_result"])
        );
        assert_eq!(first[2].1, untouched);
        let admitted = decode(first[3].1.clone())?;
        assert_eq!(admitted["status"], "running");
        let rules_only = decode(first[4].1.clone())?;
        assert_eq!(
            rules_only["discoveries"][0]["rule_ids"],
            json!(["github-pat"])
        );
        assert!(!serde_json::to_string(first)?.contains("kingfisher."));
        Ok(())
    }

    #[tokio::test]
    async fn retired_kingfisher_rules_are_removed_idempotently() -> anyhow::Result<()> {
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::raw_sql(
            "CREATE TABLE observation_events(sequence INTEGER PRIMARY KEY,occurred_at INTEGER NOT NULL,interaction_id TEXT,run_id TEXT,rejection_id TEXT,kind TEXT NOT NULL,payload BLOB NOT NULL,expires_at INTEGER NOT NULL,tool_id TEXT,operation_id TEXT)",
        )
        .execute(&mut connection)
        .await?;
        for (sequence, kind, payload) in retired_kingfisher_fixture() {
            sqlx::query("INSERT INTO observation_events(sequence,occurred_at,kind,payload,expires_at) VALUES (?,?,?,?,?)")
                .bind(sequence)
                .bind(sequence)
                .bind(kind)
                .bind(payload)
                .bind(9999)
                .execute(&mut connection)
                .await?;
        }
        let untouched: Vec<u8> =
            sqlx::query_scalar("SELECT payload FROM observation_events WHERE sequence=3")
                .fetch_one(&mut connection)
                .await?;

        retire_kingfisher_discoveries_sqlite(&mut connection).await?;
        let first: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT sequence,payload FROM observation_events ORDER BY sequence")
                .fetch_all(&mut connection)
                .await?;
        assert_retired_kingfisher_cleanup(&first, &untouched)?;

        retire_kingfisher_discoveries_sqlite(&mut connection).await?;
        let second: Vec<(i64, Vec<u8>)> =
            sqlx::query_as("SELECT sequence,payload FROM observation_events ORDER BY sequence")
                .fetch_all(&mut connection)
                .await?;
        assert_eq!(second, first);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires STRAVIA_TEST_POSTGRES_URL pointing to an isolated test PostgreSQL"]
    async fn postgres_retired_kingfisher_rules_are_removed_idempotently() -> anyhow::Result<()> {
        let url = std::env::var("STRAVIA_TEST_POSTGRES_URL")
            .expect("set STRAVIA_TEST_POSTGRES_URL to run PostgreSQL cleanup contracts");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        let schema = format!("stravia_cleanup_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let options: sqlx::postgres::PgConnectOptions = url.parse()?;
        let scenario_options = options.options([("search_path", schema.as_str())]);
        let scenario = tokio::spawn(async move {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect_with(scenario_options)
                .await?;
            sqlx::raw_sql(
                "CREATE TABLE observation_events(sequence bigint PRIMARY KEY,occurred_at bigint NOT NULL,interaction_id text,run_id text,rejection_id text,kind text NOT NULL,payload bytea NOT NULL,expires_at bigint NOT NULL,tool_id text,operation_id text)",
            )
            .execute(&pool)
            .await?;
            for (sequence, kind, payload) in retired_kingfisher_fixture() {
                sqlx::query("INSERT INTO observation_events(sequence,occurred_at,kind,payload,expires_at) VALUES ($1,$2,$3,$4,$5)")
                    .bind(sequence)
                    .bind(sequence)
                    .bind(kind)
                    .bind(payload)
                    .bind(9999_i64)
                    .execute(&pool)
                    .await?;
            }
            let untouched: Vec<u8> =
                sqlx::query_scalar("SELECT payload FROM observation_events WHERE sequence=3")
                    .fetch_one(&pool)
                    .await?;

            retire_kingfisher_discoveries_postgres(&mut *pool.acquire().await?).await?;
            let first: Vec<(i64, Vec<u8>)> =
                sqlx::query_as("SELECT sequence,payload FROM observation_events ORDER BY sequence")
                    .fetch_all(&pool)
                    .await?;
            assert_retired_kingfisher_cleanup(&first, &untouched)?;

            retire_kingfisher_discoveries_postgres(&mut *pool.acquire().await?).await?;
            let second: Vec<(i64, Vec<u8>)> =
                sqlx::query_as("SELECT sequence,payload FROM observation_events ORDER BY sequence")
                    .fetch_all(&pool)
                    .await?;
            assert_eq!(second, first);
            pool.close().await;
            Ok::<(), anyhow::Error>(())
        })
        .await;

        // Keep cleanup outside the task so contract panics still drop the schema.
        let cleanup = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await;
        admin.close().await;
        cleanup?;
        match scenario {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(error.into()),
        }
    }
}
