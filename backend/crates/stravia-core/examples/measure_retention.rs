//! Isolated SQLite retention microbenchmark. Run the same source in release mode
//! at both revisions; seed, publication, and validity reads are outside timing.
use std::hint::black_box;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use stravia_core::history_marker::{
    ClaimOutcome, HiddenHistorySegment, HistoryMarkerStore, PlatformExecutionState,
    PlatformMarkerInput, SqlHistoryMarkerStore,
};
use stravia_credential_protection::store::{DetectedSecret, MappingStore, SqlMappingStore};
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::{ContentBlock, ToolCall};

const DAY: u64 = 24 * 60 * 60;

#[derive(Clone, Copy)]
enum Workload {
    Marker,
    Mapping,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Marker => "history_marker_extend_retention",
            Self::Mapping => "credential_mapping_extend_retention",
        }
    }

    // Read-only observations, not a replacement for either production writer.
    fn observation_sql(self) -> &'static str {
        match self {
            Self::Marker => {
                "SELECT COUNT(*), MIN(expires_at), MAX(expires_at), SUM(CASE WHEN published_at IS NOT NULL AND execution_state = 'completed' AND segment_payload IS NOT NULL THEN 1 ELSE 0 END) FROM history_markers WHERE principal = ?"
            }
            Self::Mapping => {
                "SELECT COUNT(*), MIN(expires_at), MAX(expires_at), SUM(CASE WHEN published_at IS NOT NULL THEN 1 ELSE 0 END) FROM reversible_redaction_mappings WHERE principal = ?"
            }
        }
    }
}

async fn deadlines(
    pool: &SqlitePool,
    workload: Workload,
    principal: &Principal,
    count: usize,
) -> anyhow::Result<(i64, i64)> {
    let (actual, min, max, published): (i64, Option<i64>, Option<i64>, Option<i64>) =
        sqlx::query_as(workload.observation_sql())
            .bind(principal.continuation_key())
            .fetch_one(pool)
            .await?;
    ensure!(
        actual == count as i64 && published == Some(count as i64),
        "seed/publication/terminal-state count mismatch"
    );
    Ok((
        min.context("missing minimum expiry")?,
        max.context("missing maximum expiry")?,
    ))
}

async fn measure(
    pool: &SqlitePool,
    workload: Workload,
    count: usize,
    warmup: usize,
    rounds: usize,
) -> anyhow::Result<Value> {
    let principal = Principal::new(format!("synthetic-retention-{}-{count}", workload.name()));
    let markers = SqlHistoryMarkerStore::sqlite(pool.clone());
    let mappings = SqlMappingStore::sqlite(pool.clone());
    let mut references = Vec::with_capacity(count);
    match workload {
        Workload::Mapping => {
            let secrets = (0..count)
                .map(|index| {
                    format!("synthetic-only-secret-{count}-{index}")
                        .as_str()
                        .into()
                })
                .collect::<Vec<DetectedSecret>>();
            let interned = mappings.intern(&principal, &secrets).await?;
            ensure!(
                interned.created.len() == count,
                "mapping seed was not fresh"
            );
            references.extend(
                interned
                    .mappings
                    .into_iter()
                    .map(|mapping| mapping.reference),
            );
            mappings
                .publish(&principal, &references, Duration::from_secs(7 * DAY))
                .await?;
        }
        Workload::Marker => {
            for index in 0..count {
                let call = ToolCall {
                    id: format!("synthetic-call-{index}").into(),
                    name: "synthetic_tool".into(),
                    arguments: "{}".into(),
                };
                let marker = markers
                    .create_platform(
                        &principal,
                        PlatformMarkerInput {
                            tool_id: "synthetic-tool".into(),
                            call: call.clone(),
                            activity: "Synthetic retention benchmark".into(),
                            execution_limit: Duration::from_secs(60),
                            pending_retention: Duration::from_secs(3600),
                        },
                    )
                    .await?;
                ensure!(
                    markers
                        .claim_execution(
                            &principal,
                            &marker.reference,
                            "synthetic-owner",
                            Duration::from_secs(60)
                        )
                        .await?
                        == ClaimOutcome::Claimed,
                    "marker execution was not claimed"
                );
                markers.finish_execution(&principal, &marker.reference, "synthetic-owner",
                    PlatformExecutionState::Completed,
                    HiddenHistorySegment::Platform {
                        call,
                        result: ContentBlock::ToolResult {
                            content_kind: Some(stravia_runtime_contract::protocol::ir::ToolResultContentKind::Json),
                            tool_use_id: format!("synthetic-call-{index}").into(),
                            content: json!({"synthetic": true}),
                            is_error: Some(false),
                            cache_control: None,
                        },
                    }).await?;
                references.push(marker.reference);
            }
            markers
                .publish(&principal, &references, Duration::from_secs(7 * DAY))
                .await?;
            for reference in &references {
                let resolved = markers
                    .resolve(&principal, reference)
                    .await?
                    .context("seeded marker could not be resolved")?;
                ensure!(
                    resolved.published
                        && resolved.segment.is_some()
                        && resolved.execution_state == Some(PlatformExecutionState::Completed),
                    "seeded marker is not published and terminal"
                );
            }
        }
    }
    ensure!(references.len() == count, "reference count mismatch");
    let (seed_min, seed_max) = deadlines(pool, workload, &principal, count).await?;
    let mut previous_min = seed_min;
    let mut samples = Vec::with_capacity(rounds);
    for iteration in 0..(warmup + rounds) {
        // Increase by one second each iteration: even same-millisecond rounds
        // must perform a real expiry extension, not just a no-op retain.
        let retention = Duration::from_secs(8 * DAY + iteration as u64 + 1);
        let started_unix_ms = chrono::Utc::now().timestamp_millis();
        let started = Instant::now();
        match workload {
            Workload::Marker => {
                markers
                    .extend_retention(&principal, black_box(&references), retention)
                    .await?
            }
            Workload::Mapping => {
                mappings
                    .extend_retention(&principal, black_box(&references), retention)
                    .await?
            }
        }
        let elapsed_us = black_box(started.elapsed().as_secs_f64() * 1_000_000.0);
        let ended_unix_ms = chrono::Utc::now().timestamp_millis();
        let (min, max) = deadlines(pool, workload, &principal, count).await?;
        let retention_ms = i64::try_from(retention.as_millis())?;
        ensure!(
            min > previous_min
                && min >= started_unix_ms + retention_ms
                && max <= ended_unix_ms + retention_ms,
            "retention did not extend all published references to the requested deadline"
        );
        black_box((min, max));
        previous_min = min;
        if iteration >= warmup {
            samples.push(elapsed_us);
        }
    }
    let mut ordered = samples.clone();
    ordered.sort_by(f64::total_cmp);
    let percentile = |percent: usize| ordered[(ordered.len() * percent).div_ceil(100) - 1];
    Ok(json!({
        "workload": workload.name(), "references": count,
        "raw_samples_us": samples, "p50_us": percentile(50), "p95_us": percentile(95),
        "counts": {"seeded": count, "published": count, "validated_each_iteration": count,
            "warmup": warmup, "measured": rounds},
        "seed_expiry_min_unix_ms": seed_min, "seed_expiry_max_unix_ms": seed_max,
        "final_expiry_min_unix_ms": previous_min, "valid": true
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut rounds = 30usize;
    let mut warmup = 5usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rounds" => rounds = args.next().context("--rounds needs a value")?.parse()?,
            "--warmup" => warmup = args.next().context("--warmup needs a value")?.parse()?,
            _ => anyhow::bail!("unknown option {arg}; use --rounds N --warmup N"),
        }
    }
    ensure!(
        rounds > 0 && rounds <= 10_000 && warmup <= 10_000,
        "rounds must be 1..=10000 and warmup 0..=10000"
    );
    let digest = Sha256::digest(std::fs::read(std::env::current_exe()?)?);
    let mut binary_sha256 = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        write!(&mut binary_sha256, "{byte:02x}").expect("writing to String cannot fail");
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("retention.sqlite");
    // Match the production connection options in src/db/mod.rs. In particular,
    // WAL avoids benchmarking rollback-journal autocommit costs instead of the
    // configured production backend. Synchronous stays at SQLx's default, just
    // as it does in production.
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .acquire_time_level(log::LevelFilter::Debug)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(5)),
        )
        .await?;
    stravia_core::migrations::migrate_sqlite(&pool, None).await?;
    let mut cases = Vec::with_capacity(6);
    for count in [1, 64, 405] {
        for workload in [Workload::Marker, Workload::Mapping] {
            cases.push(measure(&pool, workload, count, warmup, rounds).await?);
        }
    }
    pool.close().await;
    println!(
        "{}",
        json!({
            "benchmark": "retention", "binary_sha256": binary_sha256,
            "backend": "sqlite_temp_file", "pool_max_connections": 5,
            "sqlite_journal_mode": "wal", "sqlite_busy_timeout_ms": 5000,
            "sqlite_synchronous": "sqlx_default_matches_production",
            "timer": "std::time::Instant", "unit": "us", "valid": true,
            "workload": {"reference_counts": [1, 64, 405], "warmup": warmup, "rounds": rounds,
                "synthetic_only": true, "seed_and_validation_timed": false,
                "initial_retention_seconds": 7 * DAY, "extension_base_seconds": 8 * DAY,
                "extension_increment_seconds": 1},
            "cases": cases
        })
    );
    Ok(())
}
