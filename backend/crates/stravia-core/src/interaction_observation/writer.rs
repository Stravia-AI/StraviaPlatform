use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use stravia_protocol_codec::accumulator::CanonicalPartIndex;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{
    attribution::{ObservationEvidence, RunAttribution},
    store::{Admission, ObservationStore},
    types::*,
};

/// Admission carries only the bounded received window and a full-request hash,
/// never an unbounded request snapshot behind a slow SQL writer.
pub(super) struct AdmitPayload {
    pub start: RunStart,
    pub facts: super::attribution::PreparedAdmission,
    pub received_at: i64,
    pub metadata: super::RequestMetadata,
    pub debug_enabled: bool,
    pub trace: Option<super::trace::TraceHandle>,
    pub discarded_trace: Option<super::trace::TraceHandle>,
}

pub(super) const QUEUE_CAPACITY: usize = 2048;
pub(super) const LIVE_COALESCE_BYTES: usize = 16 * 1024;

pub(super) enum WriterCommand {
    ClearTail,
    ClearDebug {
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    ClientDisconnected {
        runs: Vec<String>,
    },
    ClientToolResults {
        run_id: String,
        events: Vec<RunEvent>,
    },
    InputPreview {
        run_id: String,
        preview: String,
    },
    Purge {
        expired_before: Option<i64>,
        done: oneshot::Sender<anyhow::Result<()>>,
    },
    ClientCompletion {
        run_id: String,
        principal: String,
        window: Option<super::tail::Window>,
        delivered_at: i64,
        waiting_client: bool,
    },
    Admit(Box<AdmitPayload>),
    Event {
        run_id: String,
        event: RunEvent,
    },
    Text {
        run_id: String,
        event: Arc<Mutex<Option<RunEvent>>>,
        _slot: tokio::sync::OwnedSemaphorePermit,
    },
    Finish {
        run_id: String,
        outcome: RunOutcome,
        finished_at: i64,
    },
    Reject {
        ingress: IngressStart,
        outcome: RejectedOutcome,
        metadata: super::RequestMetadata,
        debug_enabled: bool,
        started_at: i64,
        duration_ms: i64,
    },
    Finalize {
        run_id: Option<String>,
        rejection_id: Option<String>,
        trace: Option<super::trace::TraceHandle>,
        pending_finish: Option<(RunOutcome, i64)>,
        gap: bool,
    },
    FlushInteraction {
        interaction_id: String,
        done: oneshot::Sender<()>,
    },
    Barrier(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

pub(super) struct WriterDeps {
    pub store: ObservationStore,
    pub debug_trace_index: Arc<super::manifest_index::DebugTraceIndex>,
    pub retention_days: Arc<AtomicU32>,
    pub updates: broadcast::Sender<ObservationUpdate>,
    pub trace_sequence: Arc<AtomicI64>,
    pub traces: super::trace::TraceManager,
    pub active_traces: Arc<Mutex<HashMap<String, super::trace::TraceHandle>>>,
    pub partial_trace_count: Arc<AtomicU64>,
    pub unpersisted_gaps: Arc<Mutex<super::UnpersistedGaps>>,
    pub live: Arc<super::live::LiveState>,
    pub generation_chains: crate::generation_chain::GenerationChain,
}

struct WriterContext<'a> {
    store: &'a ObservationStore,
    debug_trace_index: &'a super::manifest_index::DebugTraceIndex,
    retention: &'a AtomicU32,
    updates: &'a broadcast::Sender<ObservationUpdate>,
    trace_sequence: &'a AtomicI64,
    live: &'a super::live::LiveState,
}

pub(super) fn spawn(
    deps: WriterDeps,
) -> (mpsc::Sender<WriterCommand>, tokio::task::JoinHandle<()>) {
    let WriterDeps {
        store,
        debug_trace_index,
        retention_days,
        updates,
        trace_sequence,
        traces,
        active_traces,
        partial_trace_count,
        unpersisted_gaps,
        live,
        generation_chains,
    } = deps;
    let (tx, mut rx) = mpsc::channel(QUEUE_CAPACITY);
    let handle = tokio::spawn(async move {
        let mut attribution =
            RunAttribution::new(ObservationEvidence::new(store.clone(), generation_chains));
        // Volatile previews may be coalesced for live subscribers only. Durable
        // diagnostic content arrives separately at canonical item boundaries.
        let mut pending_text = TextBuffer {
            retained_bytes: 0,
            blocks: HashMap::new(),
            live: Arc::clone(&live),
            gaps: unpersisted_gaps.clone(),
        };
        let mut pending_gaps: HashMap<String, i64> = HashMap::new();
        let mut terminal_facts: HashMap<String, (Option<DeliveryOutcome>, bool)> = HashMap::new();
        let mut replayed_tool_results = HashSet::new();
        let mut persisted_manifests: HashMap<String, TraceManifest> = HashMap::new();
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut deferred = None;
        let mut maintenance = tokio::time::Instant::now();
        let mut next_live_publish = tokio::time::Instant::now() + Duration::from_millis(100);
        let context = WriterContext {
            store: &store,
            debug_trace_index: &debug_trace_index,
            retention: retention_days.as_ref(),
            updates: &updates,
            trace_sequence: trace_sequence.as_ref(),
            live: live.as_ref(),
        };
        loop {
            // Deferred batches bypass select!, so check the fixed deadline on
            // every writer turn as well as waking the idle writer with interval.
            if tokio::time::Instant::now() >= next_live_publish {
                pending_text.publish_live();
                next_live_publish = tokio::time::Instant::now() + Duration::from_millis(100);
            }
            // 仅重试缺失标记，不重放发现，避免把诊断故障转化为重复计数或执行失败。
            for (interaction_id, occurred_at) in std::mem::take(&mut pending_gaps) {
                if expires(occurred_at, retention_days.load(Ordering::Acquire)) > now()
                    && !matches!(store.mark_observation_gap(&interaction_id).await, Ok(true))
                {
                    pending_gaps.insert(interaction_id, occurred_at);
                }
            }
            let command = if deferred.is_some() {
                deferred.take()
            } else {
                tokio::select! {
                    _ = tokio::time::sleep_until(next_live_publish) => {
                        pending_text.publish_live();
                        next_live_publish = tokio::time::Instant::now() + Duration::from_millis(100);
                        continue;
                    },
                    _ = interval.tick() => {
                        unpersisted_gaps.lock()
                            .expire(now(), retention_days.load(Ordering::Acquire));
                        attribution.sweep(now());
                        if maintenance.elapsed() < Duration::from_secs(2) { continue; }
                        maintenance = tokio::time::Instant::now();
                        flush_active_manifests(
                            &context,
                            &attribution,
                            &active_traces,
                            &mut persisted_manifests,
                            None,
                        )
                        .await;
                        continue;
                    },
                    command = rx.recv() => command,
                }
            };
            let command = match command {
                Some(WriterCommand::Text { run_id, event, .. }) => {
                    let Some(event) = event.lock().take() else {
                        continue;
                    };
                    Some(WriterCommand::Event { run_id, event })
                }
                command => command,
            };
            attribution.sweep(now());
            if let Some(WriterCommand::Finalize {
                run_id: Some(run_id),
                ..
            }) = &command
            {
                flush_one(&mut pending_text, run_id);
            }
            match command {
                Some(WriterCommand::ClearTail) => attribution.clear_tail(),
                Some(WriterCommand::ClearDebug { response }) => {
                    for trace in active_traces.lock().values() {
                        trace.mark_partial("debug_data_cleared", true);
                    }
                    let result = async {
                        traces.delete_all().await?;
                        let ids = debug_trace_index.mark_all_debug_tombstones().await?;
                        debug_trace_index.delete_manifests(&ids).await?;
                        partial_trace_count.store(0, Ordering::Release);
                        persisted_manifests.clear();
                        Ok(())
                    }
                    .await;
                    if result.is_ok() {
                        let _ = updates.send(ObservationUpdate::ResetRequired {
                            snapshot_sequence: trace_sequence.load(Ordering::Acquire),
                        });
                    }
                    let _ = response.send(result);
                }
                Some(WriterCommand::ClientDisconnected { runs }) => {
                    for run_id in runs {
                        match store.disconnect_waiting_client(&run_id, now()).await {
                            Ok(Some(event)) => publish(&updates, &trace_sequence, event),
                            Ok(None) => {}
                            Err(error) => {
                                unpersisted_gaps.lock().record(&run_id, now());
                                if let Some(interaction) = attribution.interaction_for_run(&run_id)
                                {
                                    pending_gaps.insert(interaction.to_owned(), now());
                                }
                                tracing::warn!(%run_id, cause=%redacted_persist_cause(&error), "client disconnect persistence failed");
                            }
                        }
                    }
                }
                Some(WriterCommand::ClientToolResults { run_id, events }) => {
                    if replayed_tool_results.remove(&run_id) {
                        continue;
                    }
                    let Some(interaction) = attribution.interaction_for_run(&run_id) else {
                        continue;
                    };
                    let at = now();
                    let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                    let result = async {
                        let events = store
                            .filter_client_tool_results(interaction, &run_id, events, at)
                            .await?;
                        let events: Vec<_> =
                            events.into_iter().map(|event| (event, None, at)).collect();
                        for chunk in events.chunks(64) {
                            for event in store
                                .persist_run_events(interaction, &run_id, chunk, expiry)
                                .await?
                            {
                                publish(&updates, &trace_sequence, event);
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        unpersisted_gaps.lock().record(&run_id, at);
                        pending_gaps.insert(interaction.to_owned(), at);
                        // SQL errors may include result bind values; never log payloads.
                        tracing::warn!(%run_id, cause=%redacted_persist_cause(&error), "client tool result persistence failed");
                    }
                }
                Some(WriterCommand::InputPreview { run_id, preview }) => {
                    if !attribution.has_new_user(&run_id) {
                        continue;
                    }
                    let Some(interaction) = attribution.interaction_for_run(&run_id) else {
                        continue;
                    };
                    let at = now();
                    let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                    match store
                        .persist_input_preview(interaction, &run_id, &preview, at, expiry)
                        .await
                    {
                        Ok(Some(event)) => {
                            publish(&updates, &trace_sequence, event);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            // Never include SQL bind values or input text in diagnostics.
                            tracing::warn!(%run_id, cause=%redacted_persist_cause(&error), "input preview persistence failed");
                            pending_gaps.insert(interaction.to_owned(), at);
                        }
                    }
                }
                Some(WriterCommand::Purge {
                    expired_before,
                    done,
                }) => {
                    // Delete and invalidate grouping in the same writer turn, before another admission.
                    let result = match expired_before {
                        Some(at) => store.purge_expired_rows(at).await,
                        None => store.purge_clear_rows().await,
                    }
                    .map(|removed| {
                        live.invalidate(&removed);
                        if !removed.is_empty() || expired_before.is_none() {
                            let _ = updates.send(ObservationUpdate::ResetRequired {
                                snapshot_sequence: trace_sequence.load(Ordering::Acquire),
                            });
                        }
                        attribution.forget_interactions(&removed);
                        pending_gaps.retain(|interaction, _| !removed.contains(interaction));
                        pending_text.retain(|run| attribution.interaction_for_run(run).is_some());
                        persisted_manifests
                            .retain(|run, _| attribution.interaction_for_run(run).is_some());
                        if expired_before.is_none() {
                            attribution.clear_tail();
                        }
                    });
                    let _ = done.send(result);
                }
                Some(WriterCommand::ClientCompletion {
                    run_id,
                    principal,
                    window,
                    delivered_at,
                    waiting_client,
                }) => {
                    let at = now();
                    let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                    let Some(interaction) = attribution.interaction_for_run(&run_id) else {
                        continue;
                    };
                    // 客户端生命周期不依赖尾窗口或工具关联证据是否可捕获。
                    match store
                        .persist_client_completion(
                            &run_id,
                            interaction,
                            delivered_at,
                            waiting_client,
                            expiry,
                        )
                        .await
                    {
                        Ok(Some(event)) => publish(&updates, &trace_sequence, event),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(%run_id, cause=%redacted_persist_cause(&error), "client completion persistence failed");
                            pending_gaps.insert(interaction.to_owned(), at);
                        }
                    }
                    if let Some(window) = window {
                        let pending = window.pending_tool_ids().unwrap_or_default();
                        if let Some(hash) = window.last_hash_hex()
                            && let Err(error) = store
                                .persist_tail_source(
                                    &run_id,
                                    interaction,
                                    &principal,
                                    &hash,
                                    &pending,
                                    expiry,
                                )
                                .await
                        {
                            tracing::warn!(%run_id, %error, "tail source persistence failed");
                            pending_gaps.insert(interaction.to_owned(), at);
                        }
                        attribution.insert_tail_source(
                            run_id,
                            window,
                            expiry,
                            principal,
                            interaction.to_owned(),
                        );
                    }
                }
                Some(WriterCommand::Admit(payload)) => {
                    let AdmitPayload {
                        start,
                        mut facts,
                        received_at,
                        metadata,
                        debug_enabled,
                        trace,
                        discarded_trace,
                    } = *payload;
                    let now = now();
                    // Run Attribution owns the placement decision end to end; the
                    // writer only persists the outcome and publishes it.
                    let mut decision = attribution
                        .admit(&start, &mut facts, received_at, now)
                        .await;
                    if decision.fingerprint_gap
                        && let Some(trace) = &trace
                    {
                        trace.mark_partial("observation_gap", false);
                    }
                    if let Some(error) = &decision.parent_evidence_error {
                        unpersisted_gaps.lock().record(&start.id, now);
                        tracing::warn!(run_id=%start.id, %error, "observation parent lookup failed");
                    }
                    // 无关请求不应切碎活跃流；只刷新可能被本次准入中断的父链。
                    let affected: Vec<_> = pending_text
                        .blocks
                        .keys()
                        .filter(|run| {
                            decision.parent_run_id.as_deref() == Some(run.as_str())
                                || decision
                                    .parent_interaction_id
                                    .as_deref()
                                    .is_some_and(|parent| {
                                        attribution.interaction_for_run(run) == Some(parent)
                                    })
                        })
                        .cloned()
                        .collect();
                    for run in affected {
                        flush_one(&mut pending_text, &run);
                    }
                    let expires = expires(now, retention_days.load(Ordering::Relaxed));
                    if let Some(input) = decision.received_input.take() {
                        attribution.cache_received_input(start.id.clone(), input, expires);
                    }
                    if decision.replayed_client_tool_results {
                        replayed_tool_results.insert(start.id.clone());
                    }
                    match store
                        .admit(Admission {
                            start: &start,
                            metadata: Some(&metadata),
                            interaction_id: &decision.interaction_id,
                            generation_root_id: facts.generation_root_id.as_deref(),
                            generation_parent_id: facts.generation_parent_id.as_deref(),
                            has_new_user: decision.has_new_user,
                            ingress_received_at: decision.ingress_received_at,
                            parent_run_id: decision.parent_run_id.as_deref(),
                            parent_interaction_id: decision.parent_interaction_id.as_deref(),
                            debug_enabled,
                            inferred_retry: decision.inferred_retry,
                            grouping_reason: decision.grouping_reason,
                            diagnostic_source_run_id: decision.diagnostic_source_run_id.as_deref(),
                            interrupt_parent: decision.interrupt_parent,
                            now,
                            expires_at: expires,
                        })
                        .await
                    {
                        Ok(event) => {
                            if let Some(trace) = trace {
                                let manifest = trace.manifest();
                                match debug_trace_index
                                    .save_manifest(
                                        Some(&start.id),
                                        None,
                                        &manifest,
                                        now,
                                        expires,
                                        false,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        persisted_manifests.insert(start.id.clone(), manifest);
                                    }
                                    Err(error) => {
                                        trace.mark_observation_gap();
                                        tracing::warn!(run_id=%start.id,%error,"trace manifest persistence failed");
                                    }
                                }
                            }
                            publish(&updates, &trace_sequence, event);
                            if decision.grouping_reason == "unmatched_parent"
                                || decision.parent_evidence_error.is_some()
                            {
                                let gap = RunEvent::ObservationGap {
                                    reason: "generation_parent_observation_unavailable".into(),
                                };
                                match store
                                    .persist_run_event(
                                        &decision.interaction_id,
                                        &start.id,
                                        &gap,
                                        now,
                                        expires,
                                    )
                                    .await
                                {
                                    Ok(Some(event)) => publish(&updates, &trace_sequence, event),
                                    Ok(None) => {}
                                    Err(error) => {
                                        unpersisted_gaps.lock().record(&start.id, now);
                                        pending_gaps.insert(decision.interaction_id.clone(), now);
                                        tracing::warn!(run_id=%start.id, %error, "parent observation gap persistence failed");
                                    }
                                }
                            }
                            if let Some(diagnostic_event) = decision.diagnostic_event {
                                match store
                                    .persist_run_event(
                                        &decision.interaction_id,
                                        &start.id,
                                        &diagnostic_event,
                                        now,
                                        expires,
                                    )
                                    .await
                                {
                                    Ok(Some(event)) => {
                                        publish(&updates, &trace_sequence, event);
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        tracing::warn!(
                                            run_id=%start.id,
                                            %error,
                                            "tail observation persistence failed"
                                        );
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            unpersisted_gaps.lock().record(&start.id, now);
                            tracing::warn!(run_id=%start.id, %error, "observation admission persistence failed")
                        }
                    }
                    if let Some(trace) = discarded_trace {
                        let manifest = trace.finish().await;
                        if let Err(error) = traces.delete(&manifest.trace_id).await {
                            tracing::warn!(trace_id=%manifest.trace_id,%error,"unadmitted trace cleanup failed");
                        }
                    }
                }
                Some(WriterCommand::Event {
                    run_id,
                    event:
                        event @ (RunEvent::ClientVisibleContentDelta { .. }
                        | RunEvent::ModelThinkingDelta { .. }),
                    ..
                }) => {
                    let Some(interaction) =
                        attribution.interaction_for_run(&run_id).map(str::to_owned)
                    else {
                        continue;
                    };
                    let mut event = event;
                    super::redaction::redact_run_event(&mut event);
                    let id = delta_block_id(&event, &run_id);
                    let part_index = delta_part_index(&event);
                    let incoming = std::mem::take(text_mut(&mut event).expect("text event"));
                    let retained_bytes = pending_text.retained_bytes;
                    let block = pending_text.blocks.entry(id).or_insert_with(|| {
                        TextBlock::new(event, interaction.clone(), run_id.clone())
                    });
                    if block.overflowed {
                        // Canonical close is persisted independently; a capacity
                        // gap must not resurrect an incomplete volatile block.
                        continue;
                    }
                    let previous_bytes = block.allocated_bytes;
                    let growth_headroom = match &block.event {
                        RunEvent::ClientVisibleContentDelta { text, .. }
                        | RunEvent::ModelThinkingDelta { text, .. } => {
                            text.capacity().saturating_mul(2)
                        }
                        _ => 0,
                    };
                    if retained_bytes
                        .saturating_add(growth_headroom)
                        .saturating_add(incoming.len().saturating_mul(4))
                        > 8 * 1024 * 1024
                    {
                        block.overflowed = true;
                        block.parts.clear();
                        block.allocated_bytes = 0;
                        *text_mut(&mut block.event).expect("text block") = String::new();
                        block.revision += 1;
                    } else {
                        let ordered_append = block
                            .parts
                            .keys()
                            .next_back()
                            .is_none_or(|last| part_index >= *last);
                        block.append_part(part_index, &incoming);
                        if block.revision == 1 || !ordered_append {
                            if !pending_text.live.replace(block.live()) {
                                block.overflowed = true;
                            }
                        } else if !pending_text
                            .live
                            .append(&block.id, &incoming, block.revision)
                        {
                            block.overflowed = true;
                        }
                    }
                    pending_text.retained_bytes =
                        retained_bytes - previous_bytes + block.allocated_bytes;
                    if block.published == 0 || block.overflowed {
                        TextBuffer::publish_block(
                            &pending_text.live,
                            &mut pending_text.retained_bytes,
                            block,
                        );
                    }
                }
                Some(WriterCommand::Event { run_id, event }) => {
                    let block_id = canonical_block_id(&event);
                    let mut batch = vec![(event, block_id, now())];
                    while batch.len() < 64 {
                        match rx.try_recv() {
                            Ok(WriterCommand::Event {
                                run_id: next_run,
                                event,
                            }) if next_run == run_id
                                && !matches!(
                                    event,
                                    RunEvent::ClientVisibleContentDelta { .. }
                                        | RunEvent::ModelThinkingDelta { .. }
                                        | RunEvent::NativeCompactionAssociated { .. }
                                ) =>
                            {
                                let block_id = canonical_block_id(&event);
                                batch.push((event, block_id, now()))
                            }
                            Ok(command) => {
                                deferred = Some(command);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    for (event, block_id, occurred_at) in &batch {
                        if block_id.is_some()
                            || matches!(
                                event,
                                RunEvent::DeliveryFinished { .. }
                                    | RunEvent::ModelThinkingFinished { .. }
                            )
                        {
                            flush_one(&mut pending_text, &run_id);
                        }
                        if let Some(id) = block_id {
                            if let Some(block) = pending_text.blocks.remove(id) {
                                pending_text.retained_bytes -= block.allocated_bytes;
                            }
                            pending_text.live.remove(id);
                        }
                        match event {
                            RunEvent::ClientOutputCommitted => {
                                attribution.output_committed(&run_id);
                                terminal_facts.entry(run_id.clone()).or_default().1 = true;
                            }
                            RunEvent::DeliveryFinished { status, reason } => {
                                terminal_facts.entry(run_id.clone()).or_default().0 =
                                    Some(DeliveryOutcome {
                                        status: status.clone(),
                                        reason: reason.clone(),
                                        completed_at: (status == "delivered")
                                            .then_some(*occurred_at),
                                    });
                            }
                            _ => {}
                        }
                    }
                    if let Some(interaction) = attribution.interaction_for_run(&run_id) {
                        let at = now();
                        match store
                            .persist_run_events(
                                interaction,
                                &run_id,
                                &batch,
                                expires(at, retention_days.load(Ordering::Relaxed)),
                            )
                            .await
                        {
                            Ok(events) => {
                                for event in events {
                                    publish(&updates, &trace_sequence, event);
                                }
                            }
                            Err(error) => {
                                unpersisted_gaps.lock().record(&run_id, at);
                                pending_gaps.insert(interaction.to_owned(), at);
                                live.emit(ObservationUpdate::LiveGap {
                                    interaction_id: interaction.to_owned(),
                                    run_id: run_id.clone(),
                                    reason: "persistence_failed".into(),
                                });
                                tracing::warn!(%run_id, cause=%redacted_persist_cause(&error), "observation event persistence failed");
                            }
                        }
                    }
                }
                Some(WriterCommand::Finish {
                    run_id,
                    mut outcome,
                    finished_at,
                }) => {
                    replayed_tool_results.remove(&run_id);
                    if let Some((delivery, committed)) = terminal_facts.remove(&run_id) {
                        outcome.client_output_committed |= committed;
                        if outcome.delivery.is_none() {
                            outcome.delivery = delivery;
                        }
                    }
                    let trace = active_traces.lock().get(&run_id).cloned();
                    if let Some(trace) = trace {
                        let manifest = trace.finish().await;
                        let at = now();
                        match debug_trace_index
                            .save_manifest(
                                Some(&run_id),
                                None,
                                &manifest,
                                at,
                                expires(at, retention_days.load(Ordering::Relaxed)),
                                true,
                            )
                            .await
                        {
                            Ok(()) => {
                                persisted_manifests.insert(run_id.clone(), manifest);
                            }
                            Err(error) => {
                                trace.mark_observation_gap();
                                tracing::warn!(%run_id, %error, "terminal trace manifest persistence failed");
                            }
                        }
                    }
                    persist_finish(
                        &context,
                        &mut attribution,
                        &mut pending_text,
                        &run_id,
                        &outcome,
                        finished_at,
                    )
                    .await;
                }
                Some(WriterCommand::Reject {
                    ingress,
                    outcome,
                    metadata,
                    debug_enabled,
                    started_at,
                    duration_ms,
                }) => {
                    let at = now();
                    let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                    match store
                        .reject(super::store::Rejection {
                            ingress: &ingress,
                            outcome: &outcome,
                            metadata: &metadata,
                            debug_enabled,
                            occurred_at: at,
                            expires_at: expiry,
                            started_at,
                            duration_ms,
                        })
                        .await
                    {
                        Ok(value) => {
                            publish(&updates, &trace_sequence, value);
                        }
                        Err(error) => {
                            tracing::warn!(rejection_id=%ingress.id,%error,"rejection observation persistence failed")
                        }
                    }
                }
                Some(WriterCommand::Finalize {
                    run_id,
                    rejection_id,
                    trace,
                    pending_finish,
                    gap,
                }) => {
                    if let Some(run_id) = run_id.as_deref() {
                        replayed_tool_results.remove(run_id);
                        if gap {
                            if let Some(trace) = &trace {
                                trace.mark_partial("writer_overflow", false);
                            }
                            if let Some(interaction) = attribution.interaction_for_run(run_id) {
                                let at = now();
                                let event = RunEvent::ObservationGap {
                                    reason: "writer_overflow".into(),
                                };
                                match store
                                    .persist_run_event(
                                        interaction,
                                        run_id,
                                        &event,
                                        at,
                                        expires(at, retention_days.load(Ordering::Relaxed)),
                                    )
                                    .await
                                {
                                    Ok(Some(event)) => {
                                        publish(&updates, &trace_sequence, event);
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        unpersisted_gaps.lock().record(run_id, at);
                                        pending_gaps.insert(interaction.to_owned(), at);
                                        tracing::warn!(%run_id,%error,"observation gap persistence failed")
                                    }
                                }
                            }
                        }
                        if pending_finish.is_some()
                            && let Some(trace) = &trace
                        {
                            let manifest = trace.finish().await;
                            let at = now();
                            match debug_trace_index
                                .save_manifest(
                                    Some(run_id),
                                    None,
                                    &manifest,
                                    at,
                                    expires(at, retention_days.load(Ordering::Relaxed)),
                                    true,
                                )
                                .await
                            {
                                Ok(()) => {
                                    persisted_manifests.insert(run_id.to_owned(), manifest);
                                }
                                Err(error) => {
                                    trace.mark_observation_gap();
                                    tracing::warn!(%run_id, %error, "terminal trace manifest persistence failed");
                                }
                            }
                        }
                        if let Some((mut outcome, finished_at)) = pending_finish {
                            if let Some((delivery, committed)) = terminal_facts.remove(run_id) {
                                outcome.client_output_committed |= committed;
                                if outcome.delivery.is_none() {
                                    outcome.delivery = delivery;
                                }
                            }
                            persist_finish(
                                &context,
                                &mut attribution,
                                &mut pending_text,
                                run_id,
                                &outcome,
                                finished_at,
                            )
                            .await;
                        }
                        if let Some(interaction) = attribution.interaction_for_run(run_id) {
                            let at = now();
                            match store
                                .finalize_activity(
                                    interaction,
                                    run_id,
                                    at,
                                    expires(at, retention_days.load(Ordering::Relaxed)),
                                )
                                .await
                            {
                                Ok(Some(event)) => publish(&updates, &trace_sequence, event),
                                Ok(None) => {}
                                Err(error) => {
                                    unpersisted_gaps.lock().record(run_id, at);
                                    pending_gaps.insert(interaction.to_owned(), at);
                                    tracing::warn!(%run_id, %error, "observation activity finalization failed");
                                }
                            }
                        }
                    }
                    let mut persisted_partial = false;
                    if let Some(trace) = trace {
                        // Producer capture already entered Trace's own queue; finish drains it
                        // before publishing the terminal durable manifest boundary.
                        let manifest = trace.finish().await;
                        let at = now();
                        let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                        // The index retains terminal manifests in memory even when
                        // the durable write fails, so the finalized partial count
                        // covers indexed failures as well as persisted manifests.
                        persisted_partial = manifest.status == "partial"
                            && !manifest
                                .reasons
                                .iter()
                                .any(|reason| reason == "debug_data_cleared");
                        if !run_id.as_ref().is_some_and(|run_id| {
                            persisted_manifests
                                .get(run_id)
                                .is_some_and(|previous| manifests_match(previous, &manifest))
                        }) && let Err(error) = debug_trace_index
                            .save_manifest(
                                run_id.as_deref(),
                                rejection_id.as_deref(),
                                &manifest,
                                at,
                                expiry,
                                true,
                            )
                            .await
                        {
                            tracing::warn!(trace_id=%manifest.trace_id,%error,"trace manifest persistence failed");
                        }
                    }
                    if let Some(run_id) = run_id {
                        terminal_facts.remove(&run_id);
                        let mut active = active_traces.lock();
                        active.remove(&run_id);
                        if persisted_partial {
                            partial_trace_count.fetch_add(1, Ordering::AcqRel);
                        }
                        drop(active);
                        persisted_manifests.remove(&run_id);
                    } else if persisted_partial {
                        partial_trace_count.fetch_add(1, Ordering::AcqRel);
                    }
                }
                Some(WriterCommand::FlushInteraction {
                    interaction_id,
                    done,
                }) => {
                    let runs: Vec<_> = pending_text
                        .blocks
                        .keys()
                        .filter(|run| {
                            attribution.interaction_for_run(run) == Some(interaction_id.as_str())
                        })
                        .cloned()
                        .collect();
                    for run in runs {
                        flush_one(&mut pending_text, &run);
                    }
                    flush_active_manifests(
                        &context,
                        &attribution,
                        &active_traces,
                        &mut persisted_manifests,
                        Some(&interaction_id),
                    )
                    .await;
                    let _ = done.send(());
                }
                Some(WriterCommand::Barrier(done)) => {
                    // 读己之写屏障不绕过正文限频；新订阅从当前镜像读取。
                    flush_active_manifests(
                        &context,
                        &attribution,
                        &active_traces,
                        &mut persisted_manifests,
                        None,
                    )
                    .await;
                    let _ = done.send(());
                }
                Some(WriterCommand::Shutdown(done)) => {
                    pending_text.publish_live();
                    let _ = done.send(());
                    break;
                }
                None => {
                    pending_text.publish_live();
                    break;
                }
                Some(WriterCommand::Text { .. }) => unreachable!("text command resolved above"),
            }
        }
    });
    (tx, handle)
}

async fn flush_active_manifests(
    context: &WriterContext<'_>,
    attribution: &RunAttribution<ObservationEvidence>,
    active_traces: &Mutex<HashMap<String, super::trace::TraceHandle>>,
    persisted: &mut HashMap<String, TraceManifest>,
    only_interaction: Option<&str>,
) {
    let active: Vec<_> = active_traces
        .lock()
        .iter()
        .map(|(run_id, trace)| (run_id.clone(), trace.clone()))
        .collect();
    for (run_id, trace) in active {
        let Some(interaction_id) = attribution.interaction_for_run(&run_id) else {
            continue;
        };
        if only_interaction.is_some_and(|id| id != interaction_id) {
            continue;
        }
        if trace.flush().await.is_err() {
            trace.mark_observation_gap();
        }
        // Finish 已保存终态；维护不能将它重新发布为未完成捕获。
        if trace.is_finished() {
            continue;
        }
        let manifest = trace.manifest();
        if persisted
            .get(&run_id)
            .is_some_and(|previous| manifests_match(previous, &manifest))
        {
            continue;
        }
        let at = now();
        match context
            .debug_trace_index
            .save_manifest(
                Some(&run_id),
                None,
                &manifest,
                at,
                expires(at, context.retention.load(Ordering::Relaxed)),
                false,
            )
            .await
        {
            Ok(()) => {
                persisted.insert(run_id, manifest);
            }
            Err(error) => {
                trace.mark_observation_gap();
                tracing::warn!(%run_id,%error,"active trace manifest persistence failed");
            }
        }
    }
}

/// Persistence errors may echo SQL statements, bind values or payload text;
/// only the driver error class and database error code are safe diagnostics.
pub(super) fn redacted_persist_cause(error: &anyhow::Error) -> String {
    for cause in error.chain() {
        match cause.downcast_ref::<sqlx::Error>() {
            Some(sqlx::Error::Database(database)) => {
                return match database.code() {
                    Some(code) => format!("database:{:?}:{code}", database.kind()),
                    None => format!("database:{:?}", database.kind()),
                };
            }
            Some(_) => return "sqlx".to_owned(),
            None => {}
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return format!("io:{:?}", io.kind());
        }
    }
    "persistence".to_owned()
}

fn publish(
    updates: &broadcast::Sender<ObservationUpdate>,
    trace_sequence: &AtomicI64,
    event: ObservationEvent,
) {
    trace_sequence.fetch_max(event.sequence, Ordering::AcqRel);
    let _ = updates.send(ObservationUpdate::Event(event));
}

fn manifests_match(left: &TraceManifest, right: &TraceManifest) -> bool {
    left.status == right.status
        && left.bytes_written == right.bytes_written
        && left.event_count == right.event_count
        && left.reasons == right.reasons
}

async fn persist_finish(
    context: &WriterContext<'_>,
    attribution: &mut RunAttribution<ObservationEvidence>,
    pending_text: &mut TextBuffer,
    run_id: &str,
    outcome: &RunOutcome,
    at: i64,
) {
    flush_one(pending_text, run_id);
    pending_text.retain(|run| run != run_id);
    if let Some(interaction) = attribution.interaction_for_run(run_id) {
        let expiry = expires(at, context.retention.load(Ordering::Relaxed));
        use tracing::Instrument as _;
        let span = tracing::info_span!(target: "stravia::perf", "observation.writer.finish_run", status = tracing::field::Empty);
        let result = context
            .store
            .finish_run(interaction, run_id, outcome, at, expiry)
            .instrument(span.clone())
            .await;
        span.record("status", if result.is_ok() { "completed" } else { "error" });
        drop(span);
        match result {
            Ok(value) => {
                if let Some(node) = outcome.generation_node_id.as_deref()
                    && let Err(error) = context.store.set_tail_generation_node(run_id, node).await
                {
                    tracing::debug!(%run_id, %error, "failed to persist tail Generation node");
                }
                publish(context.updates, context.trace_sequence, value);
            }
            Err(error) => {
                pending_text.gaps.lock().record(run_id, at);
                if let Err(error) = context.store.mark_observation_gap(interaction).await {
                    tracing::debug!(%run_id, %error, "failed to mark Interaction observation gap");
                }
                context.live.emit(ObservationUpdate::LiveGap {
                    interaction_id: interaction.to_owned(),
                    run_id: run_id.to_owned(),
                    reason: "persistence_failed".into(),
                });
                tracing::warn!(%run_id, cause=%redacted_persist_cause(&error), "observation finish persistence failed");
            }
        }
    }
    if let Some(interaction) = attribution.interaction_for_run(run_id) {
        context.live.emit(ObservationUpdate::LiveFinished {
            interaction_id: interaction.to_owned(),
            run_id: run_id.to_owned(),
        });
    }
    attribution.finish(run_id, &outcome.status, at);
}

fn flush_one(pending: &mut TextBuffer, run_id: &str) {
    for block in pending
        .blocks
        .values_mut()
        .filter(|block| block.run == run_id)
    {
        TextBuffer::publish_block(&pending.live, &mut pending.retained_bytes, block);
    }
}

pub(super) fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub(super) fn expires(at: i64, days: u32) -> i64 {
    at.saturating_add(i64::from(days).saturating_mul(86_400_000))
}

struct TextBlock {
    id: String,
    interaction: String,
    run: String,
    event: RunEvent,
    at: i64,
    revision: u64,
    published: u64,
    parts: std::collections::BTreeMap<CanonicalPartIndex, String>,
    overflowed: bool,
    allocated_bytes: usize,
}
impl TextBlock {
    fn new(event: RunEvent, interaction: String, run: String) -> Self {
        Self {
            id: delta_block_id(&event, &run),
            interaction,
            run,
            event,
            at: now(),
            revision: 0,
            published: 0,
            parts: Default::default(),
            overflowed: false,
            allocated_bytes: 0,
        }
    }
    #[cfg(test)]
    fn append(&mut self, text: &str) -> usize {
        self.append_part(delta_part_index(&self.event), text);
        text.len()
    }
    fn append_part(&mut self, part_index: CanonicalPartIndex, text: &str) {
        if text.is_empty() || self.overflowed {
            return;
        }
        if text_mut(&mut self.event)
            .expect("text block")
            .len()
            .saturating_add(text.len())
            > 8 * 1024 * 1024
        {
            self.overflowed = true;
            self.parts.clear();
            self.allocated_bytes = 0;
            *text_mut(&mut self.event).expect("text block") = String::new();
            self.revision += 1;
            return;
        }
        let appends_at_end = self
            .parts
            .keys()
            .next_back()
            .is_none_or(|last| part_index >= *last);
        let part = self.parts.entry(part_index).or_default();
        let previous_part_capacity = part.capacity();
        part.push_str(text);
        self.allocated_bytes += part.capacity() - previous_part_capacity;
        let pending = text_mut(&mut self.event).expect("text block");
        let previous_text_capacity = pending.capacity();
        if appends_at_end {
            pending.push_str(text);
        } else {
            pending.clear();
            for part in self.parts.values() {
                pending.push_str(part);
            }
        }
        self.allocated_bytes += pending.capacity() - previous_text_capacity;
        self.revision += 1;
    }
    fn live(&self) -> LiveContentBlock {
        let (kind, turn, attempt, text) = match &self.event {
            RunEvent::ClientVisibleContentDelta { text, .. } => {
                ("client_visible_content_delta", None, None, text)
            }
            RunEvent::ModelThinkingDelta {
                model_turn_id,
                attempt_id,
                text,
                ..
            } => (
                "model_thinking_delta",
                Some(model_turn_id.clone()),
                Some(attempt_id.clone()),
                text,
            ),
            _ => unreachable!("text block"),
        };
        LiveContentBlock {
            block_id: self.id.clone(),
            interaction_id: self.interaction.clone(),
            run_id: self.run.clone(),
            kind: kind.into(),
            model_turn_id: turn,
            attempt_id: attempt,
            occurred_at: self.at,
            revision: self.revision,
            text: text.clone(),
        }
    }
}
struct TextBuffer {
    retained_bytes: usize,
    blocks: HashMap<String, TextBlock>,
    live: Arc<super::live::LiveState>,
    gaps: Arc<Mutex<super::UnpersistedGaps>>,
}
impl TextBuffer {
    fn publish_block(
        live: &super::live::LiveState,
        retained_bytes: &mut usize,
        block: &mut TextBlock,
    ) {
        if block.revision == block.published {
            return;
        }
        if !block.overflowed {
            live.publish(&block.id);
        } else {
            *retained_bytes -= block.allocated_bytes;
            block.allocated_bytes = 0;
            block.parts.clear();
            *text_mut(&mut block.event).expect("text block") = String::new();
            live.remove(&block.id);
            live.emit(ObservationUpdate::LiveGap {
                interaction_id: block.interaction.clone(),
                run_id: block.run.clone(),
                reason: "live_capacity".into(),
            });
        }
        block.published = block.revision;
    }
    fn publish_live(&mut self) {
        for block in self.blocks.values_mut() {
            Self::publish_block(&self.live, &mut self.retained_bytes, block);
        }
    }
    fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.blocks.retain(|_, block| {
            if keep(&block.run) {
                true
            } else {
                self.retained_bytes -= block.allocated_bytes;
                self.live.remove(&block.id);
                false
            }
        });
    }
}
fn canonical_block_id(event: &RunEvent) -> Option<String> {
    match event {
        RunEvent::ModelThinking { block_id, .. }
        | RunEvent::ClientVisibleContent { block_id, .. } => Some(block_id.clone()),
        _ => None,
    }
}

pub(super) fn text_mut(event: &mut RunEvent) -> Option<&mut String> {
    match event {
        RunEvent::ClientVisibleContentDelta { text, .. }
        | RunEvent::ModelThinkingDelta { text, .. } => Some(text),
        _ => None,
    }
}
pub(super) fn same_scope(left: &RunEvent, right: &RunEvent) -> bool {
    match (left, right) {
        (
            RunEvent::ClientVisibleContentDelta { .. },
            RunEvent::ClientVisibleContentDelta { .. },
        ) => delta_identity(left) == delta_identity(right),
        (
            RunEvent::ModelThinkingDelta {
                model_turn_id: l,
                attempt_id: a,
                ..
            },
            RunEvent::ModelThinkingDelta {
                model_turn_id: r,
                attempt_id: b,
                ..
            },
        ) => l == r && a == b && delta_identity(left) == delta_identity(right),
        _ => false,
    }
}

fn delta_identity(event: &RunEvent) -> (usize, CanonicalPartIndex) {
    match event {
        RunEvent::ClientVisibleContentDelta {
            item_ordinal,
            part_index,
            ..
        }
        | RunEvent::ModelThinkingDelta {
            item_ordinal,
            part_index,
            ..
        } => (*item_ordinal, *part_index),
        _ => unreachable!("canonical delta"),
    }
}
fn delta_part_index(event: &RunEvent) -> CanonicalPartIndex {
    delta_identity(event).1
}
fn delta_block_id(event: &RunEvent, run_id: &str) -> String {
    let scope = match event {
        RunEvent::ModelThinkingDelta { attempt_id, .. } => attempt_id.as_str(),
        _ => run_id,
    };
    super::canonical_item_block_id(scope, delta_identity(event).0)
}

#[cfg(test)]
mod block_tests {
    use super::*;
    #[tokio::test]
    async fn live_revisions_replace_and_committed_identity_is_removable() {
        use futures::StreamExt;
        let mirror = Arc::new(super::super::live::LiveState::default());
        let mut buffer = TextBuffer {
            retained_bytes: 0,
            blocks: HashMap::new(),
            live: mirror.clone(),
            gaps: Arc::new(Mutex::new(super::super::UnpersistedGaps::default())),
        };
        let mut receiver = mirror.subscribe("interaction".into());
        assert!(
            matches!(receiver.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty())
        );
        let mut block = TextBlock::new(
            RunEvent::ClientVisibleContentDelta {
                item_ordinal: 0,
                part_index: (false, 0),
                text: String::new(),
            },
            "interaction".into(),
            "run".into(),
        );
        block.append("first");
        mirror.replace(block.live());
        let id = block.id.clone();
        buffer.retained_bytes = block.allocated_bytes;
        buffer.blocks.insert(id.clone(), block);
        buffer.publish_live();
        let Some(ObservationUpdate::LiveContent(first)) = receiver.next().await else {
            panic!("live block");
        };
        let block = buffer.blocks.get_mut(&id).unwrap();
        let previous = block.allocated_bytes;
        block.append(" second");
        buffer.retained_bytes += block.allocated_bytes - previous;
        mirror.replace(block.live());
        flush_one(&mut buffer, "run");
        let Some(ObservationUpdate::LiveContent(second)) = receiver.next().await else {
            panic!("live block");
        };
        assert_eq!(first.block_id, second.block_id);
        assert!(first.revision < second.revision);
        let mut current = mirror.subscribe("interaction".into());
        assert!(
            matches!(current.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks[0].text == "first second")
        );
        mirror.remove(&second.block_id);
        let mut retired = mirror.subscribe("interaction".into());
        assert!(
            matches!(retired.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty())
        );
    }
    #[tokio::test]
    async fn overflow_boundary_retires_current_body_and_terminal_preserves_gap_order() {
        use futures::StreamExt;
        let live = Arc::new(super::super::live::LiveState::default());
        let mut buffer = TextBuffer {
            retained_bytes: 0,
            blocks: HashMap::new(),
            live: live.clone(),
            gaps: Arc::new(Mutex::new(super::super::UnpersistedGaps::default())),
        };
        let mut block = TextBlock::new(
            RunEvent::ClientVisibleContentDelta {
                item_ordinal: 0,
                part_index: (false, 0),
                text: String::new(),
            },
            "interaction".into(),
            "run".into(),
        );
        block.append("actual partial");
        live.replace(block.live());
        let id = block.id.clone();
        buffer.retained_bytes = block.allocated_bytes;
        buffer.blocks.insert(id.clone(), block);
        let mut existing = live.subscribe("interaction".into());
        assert!(
            matches!(existing.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks[0].text == "actual partial")
        );
        let block = buffer.blocks.get_mut(&id).unwrap();
        block.overflowed = true;
        // The first publication path uses the same cleanup as timer/terminal.
        flush_one(&mut buffer, "run");
        let mut reconnect = live.subscribe("interaction".into());
        assert!(
            matches!(reconnect.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty())
        );
        assert!(
            matches!(reconnect.next().await, Some(ObservationUpdate::LiveGap { reason, .. }) if reason == "live_capacity")
        );
        live.emit(ObservationUpdate::LiveFinished {
            interaction_id: "interaction".into(),
            run_id: "run".into(),
        });
        assert!(
            matches!(existing.next().await, Some(ObservationUpdate::LiveGap { reason, .. }) if reason == "live_capacity")
        );
        assert!(
            matches!(existing.next().await, Some(ObservationUpdate::LiveFinished { run_id, .. }) if run_id == "run")
        );
        let mut switched = live.subscribe("interaction".into());
        assert!(
            matches!(switched.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty())
        );
        buffer.publish_live();
        let mut recovered = live.subscribe("interaction".into());
        assert!(
            matches!(recovered.next().await, Some(ObservationUpdate::LiveSnapshot { blocks }) if blocks.is_empty())
        );
    }
    #[test]
    fn canonical_items_keep_identity_ordered_parts_and_cumulative_revisions() {
        let delta = |ordinal| RunEvent::ClientVisibleContentDelta {
            item_ordinal: ordinal,
            part_index: (false, 0),
            text: String::new(),
        };
        let mut first = TextBlock::new(delta(3), "interaction".into(), "run".into());
        let second = TextBlock::new(delta(4), "interaction".into(), "run".into());
        assert!(!same_scope(&first.event, &second.event));
        assert_ne!(first.id, second.id);
        first.append_part((false, 1), "tail");
        let initial = first.live();
        first.append_part((false, 0), "head");
        first.append_part((false, 1), "!");
        let final_live = first.live();
        assert_eq!(final_live.text, "headtail!");
        assert_eq!(initial.block_id, final_live.block_id);
        assert!(final_live.revision > initial.revision);
        let durable = RunEvent::ClientVisibleContent {
            text: final_live.text,
            parts: vec![],
            block_id: super::super::canonical_item_block_id("run", 3),
            item: serde_json::json!({"id":"late-provider-id"}),
            complete: true,
        };
        assert_eq!(
            canonical_block_id(&durable).as_deref(),
            Some(first.id.as_str())
        );
        first.append_part((false, 0), &"x".repeat(LIVE_COALESCE_BYTES * 2));
        assert_eq!(first.id, initial.block_id);
        assert!(first.live().text.ends_with("tail!"));
        assert!(first.revision > final_live.revision);
    }
}
