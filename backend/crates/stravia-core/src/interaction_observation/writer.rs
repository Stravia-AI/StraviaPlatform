use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::{broadcast, mpsc, oneshot};

use super::{
    grouping::GroupingIndex,
    store::{Admission, ObservationStore},
    types::*,
};

pub(super) enum WriterCommand {
    ClearTail,
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
    Tail {
        run_id: String,
        principal: String,
        window: Option<super::tail::Window>,
        completed: bool,
    },
    Admit {
        start: RunStart,
        debug_enabled: bool,
        trace: Option<super::trace::TraceHandle>,
        discarded_trace: Option<super::trace::TraceHandle>,
    },
    Event {
        run_id: String,
        event: RunEvent,
    },
    Finish {
        run_id: String,
        outcome: RunOutcome,
    },
    Reject {
        ingress: IngressStart,
        outcome: RejectedOutcome,
        debug_enabled: bool,
    },
    Finalize {
        run_id: Option<String>,
        rejection_id: Option<String>,
        trace: Option<super::trace::TraceHandle>,
        pending_finish: Option<RunOutcome>,
        gap: bool,
    },
    FlushInteraction {
        interaction_id: String,
        done: oneshot::Sender<()>,
    },
    Barrier(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

pub(super) fn spawn(
    store: ObservationStore,
    retention_days: Arc<AtomicU32>,
    updates: broadcast::Sender<ObservationUpdate>,
    trace_sequence: Arc<AtomicI64>,
    traces: super::trace::TraceManager,
    active_traces: Arc<Mutex<HashMap<String, super::trace::TraceHandle>>>,
    partial_trace_count: Arc<AtomicU64>,
    unpersisted_gaps: Arc<Mutex<super::UnpersistedGaps>>,
    live: Arc<super::live::LiveState>,
) -> (mpsc::Sender<WriterCommand>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(2048);
    let handle = tokio::spawn(async move {
        let mut grouping = GroupingIndex::default();
        let mut tail = super::tail::TailIndex::default();
        // 只合并同一 Run 中相邻且同作用域的正文或思考增量，不跨事件边界重排。
        let mut pending_text = TextBuffer {
            blocks: HashMap::new(),
            live,
            gaps: unpersisted_gaps.clone(),
        };
        let mut pending_gaps: HashMap<String, i64> = HashMap::new();
        let mut persisted_manifests: HashMap<String, TraceManifest> = HashMap::new();
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut deferred = None;
        let mut maintenance = tokio::time::Instant::now();
        loop {
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
                    _ = interval.tick() => {
                        unpersisted_gaps.lock().expect("observation gaps")
                            .expire(now(), retention_days.load(Ordering::Acquire));
                        tail.sweep(now());
                        pending_text.publish_live(&updates);
                        let due: Vec<_> = pending_text.blocks.iter().filter(|(_, block)| block.due()).map(|(run, _)| run.clone()).collect();
                        for run in due { flush_one(&store, &grouping, &retention_days, &updates, &trace_sequence, &mut pending_text, &run).await; }
                        if maintenance.elapsed() < Duration::from_secs(2) { continue; }
                        maintenance = tokio::time::Instant::now();
                        flush_active_manifests(
                            &store,
                            &retention_days,
                            &updates,
                            &trace_sequence,
                            &grouping,
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
            tail.sweep(now());
            match &command {
                Some(WriterCommand::ClearTail | WriterCommand::Purge { .. }) => {
                    flush_text(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                    )
                    .await;
                }
                Some(
                    WriterCommand::InputPreview { run_id, .. }
                    | WriterCommand::Tail { run_id, .. }
                    | WriterCommand::Finalize {
                        run_id: Some(run_id),
                        ..
                    },
                ) => {
                    flush_one(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                        run_id,
                    )
                    .await;
                }
                _ => {}
            }
            match command {
                Some(WriterCommand::ClearTail) => tail = super::tail::TailIndex::default(),
                Some(WriterCommand::ClientToolResults { run_id, events }) => {
                    flush_one(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                        &run_id,
                    )
                    .await;
                    let Some(interaction) = grouping.interaction_for_run(&run_id) else {
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
                    if result.is_err() {
                        unpersisted_gaps
                            .lock()
                            .expect("observation gaps")
                            .record(&run_id, at);
                        pending_gaps.insert(interaction.to_owned(), at);
                        // SQL errors may include result bind values; never log payloads.
                        tracing::warn!(%run_id, "client tool result persistence failed");
                    }
                }
                Some(WriterCommand::InputPreview { run_id, preview }) => {
                    let Some(interaction) = grouping.interaction_for_run(&run_id) else {
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
                        Err(_) => {
                            // Never include SQL bind values or input text in diagnostics.
                            tracing::warn!(%run_id, "input preview persistence failed");
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
                        grouping.forget_interactions(&removed);
                        pending_gaps.retain(|interaction, _| !removed.contains(interaction));
                        pending_text.retain(|run| grouping.interaction_for_run(run).is_some());
                        persisted_manifests
                            .retain(|run, _| grouping.interaction_for_run(run).is_some());
                        if expired_before.is_none() {
                            tail = super::tail::TailIndex::default();
                        }
                    });
                    let _ = done.send(result);
                }
                Some(WriterCommand::Tail {
                    run_id,
                    principal,
                    window,
                    completed,
                }) => {
                    let at = now();
                    let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                    if completed {
                        if let Some(window) = window {
                            tail.insert(run_id, window, expiry);
                        }
                        continue;
                    }
                    let Some(interaction) = grouping.interaction_for_run(&run_id) else {
                        continue;
                    };
                    let event = match store.tail_candidates(&principal, &run_id, at).await {
                        Ok(candidates) => tail.associate(window.as_ref(), &candidates),
                        Err(_) => RunEvent::ObservationGap {
                            reason: "tail_index_unavailable".into(),
                        },
                    };
                    match store
                        .persist_run_event(interaction, &run_id, &event, at, expiry)
                        .await
                    {
                        Ok(Some(event)) => {
                            publish(&updates, &trace_sequence, event);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(%run_id, %error, "tail observation persistence failed")
                        }
                    }
                }
                Some(WriterCommand::Admit {
                    start,
                    debug_enabled,
                    trace,
                    discarded_trace,
                }) => {
                    let now = now();
                    let persisted_parent = match start.generation_parent_id.as_deref() {
                        Some(parent) => match store
                            .observed_generation_parent(parent, &start.principal)
                            .await
                        {
                            Ok(parent) => parent,
                            Err(error) => {
                                unpersisted_gaps
                                    .lock()
                                    .expect("observation gaps")
                                    .record(&start.id, now);
                                tracing::warn!(run_id=%start.id, %error, "observation parent lookup failed");
                                None
                            }
                        },
                        None => None,
                    };
                    let assignment = grouping.assign(&start, now, persisted_parent.as_ref());
                    // 无关请求不应切碎活跃流；只刷新可能被本次准入中断的父链。
                    let affected: Vec<_> = pending_text
                        .blocks
                        .keys()
                        .filter(|run| {
                            assignment.parent_run_id.as_deref() == Some(run.as_str())
                                || assignment.parent_interaction_id.as_deref().is_some_and(
                                    |parent| grouping.interaction_for_run(run) == Some(parent),
                                )
                        })
                        .cloned()
                        .collect();
                    for run in affected {
                        flush_one(
                            &store,
                            &grouping,
                            &retention_days,
                            &updates,
                            &trace_sequence,
                            &mut pending_text,
                            &run,
                        )
                        .await;
                    }
                    let expires = expires(now, retention_days.load(Ordering::Relaxed));
                    match store
                        .admit(Admission {
                            start: &start,
                            interaction_id: &assignment.interaction_id,
                            parent_run_id: assignment.parent_run_id.as_deref(),
                            parent_interaction_id: assignment.parent_interaction_id.as_deref(),
                            debug_enabled,
                            inferred_retry: assignment.inferred_retry,
                            grouping_reason: assignment.grouping_reason,
                            now,
                            expires_at: expires,
                        })
                        .await
                    {
                        Ok(event) => {
                            if let Some(trace) = trace {
                                let manifest = trace.manifest();
                                match store
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
                        }
                        Err(error) => {
                            unpersisted_gaps
                                .lock()
                                .expect("observation gaps")
                                .record(&start.id, now);
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
                        grouping.interaction_for_run(&run_id).map(str::to_owned)
                    else {
                        continue;
                    };
                    let mut event = event;
                    super::redaction::redact_run_event(&mut event);
                    if pending_text
                        .blocks
                        .get(&run_id)
                        .is_some_and(|block| !same_scope(&block.event, &event))
                    {
                        flush_one(
                            &store,
                            &grouping,
                            &retention_days,
                            &updates,
                            &trace_sequence,
                            &mut pending_text,
                            &run_id,
                        )
                        .await;
                    }
                    let text = text_mut(&mut event).expect("text event");
                    let incoming = std::mem::take(text);
                    let mut rest = incoming.as_str();
                    let mut sealed = Vec::new();
                    while !rest.is_empty() {
                        let block =
                            pending_text
                                .blocks
                                .entry(run_id.clone())
                                .or_insert_with(|| {
                                    TextBlock::new(
                                        event.clone(),
                                        interaction.clone(),
                                        run_id.clone(),
                                    )
                                });
                        let take = block.append(rest);
                        rest = &rest[take..];
                        if take == 0
                            || text_mut(&mut block.event).expect("text block").len()
                                == super::codec::CONTENT_BLOCK_BYTES
                        {
                            sealed.push(pending_text.blocks.remove(&run_id).expect("sealed block"));
                            if sealed.len() == 32 {
                                persist_blocks(
                                    &store,
                                    &retention_days,
                                    &updates,
                                    &trace_sequence,
                                    &pending_text,
                                    &interaction,
                                    &run_id,
                                    std::mem::take(&mut sealed),
                                )
                                .await;
                            }
                        }
                    }
                    if !sealed.is_empty() {
                        persist_blocks(
                            &store,
                            &retention_days,
                            &updates,
                            &trace_sequence,
                            &pending_text,
                            &interaction,
                            &run_id,
                            sealed,
                        )
                        .await;
                    }
                }
                Some(WriterCommand::Event { run_id, event }) => {
                    flush_one(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                        &run_id,
                    )
                    .await;
                    let mut batch = vec![(event, None, now())];
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
                                batch.push((event, None, now()))
                            }
                            Ok(command) => {
                                deferred = Some(command);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    for (event, _, _) in &batch {
                        if matches!(event, RunEvent::ClientOutputCommitted) {
                            grouping.output_committed(&run_id);
                        }
                    }
                    if let Some(interaction) = grouping.interaction_for_run(&run_id) {
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
                            Err(_) => {
                                unpersisted_gaps
                                    .lock()
                                    .expect("observation gaps")
                                    .record(&run_id, at);
                                pending_gaps.insert(interaction.to_owned(), at);
                                let _ = updates.send(ObservationUpdate::LiveGap {
                                    interaction_id: interaction.to_owned(),
                                    run_id: run_id.clone(),
                                    reason: "persistence_failed".into(),
                                });
                                tracing::warn!(%run_id, "observation event persistence failed");
                            }
                        }
                    }
                }
                Some(WriterCommand::Finish { run_id, outcome }) => {
                    persist_finish(
                        &store,
                        &mut grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                        &run_id,
                        &outcome,
                    )
                    .await;
                }
                Some(WriterCommand::Reject {
                    ingress,
                    outcome,
                    debug_enabled,
                }) => {
                    let at = now();
                    let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                    match store
                        .reject(&ingress, &outcome, debug_enabled, at, expiry)
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
                        if gap {
                            if let Some(trace) = &trace {
                                trace.mark_partial("writer_overflow", false);
                            }
                            if let Some(interaction) = grouping.interaction_for_run(run_id) {
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
                                        unpersisted_gaps
                                            .lock()
                                            .expect("observation gaps")
                                            .record(run_id, at);
                                        pending_gaps.insert(interaction.to_owned(), at);
                                        tracing::warn!(%run_id,%error,"observation gap persistence failed")
                                    }
                                }
                            }
                        }
                        if let Some(outcome) = pending_finish {
                            persist_finish(
                                &store,
                                &mut grouping,
                                &retention_days,
                                &updates,
                                &trace_sequence,
                                &mut pending_text,
                                run_id,
                                &outcome,
                            )
                            .await;
                        }
                    }
                    let mut persisted_partial = false;
                    if let Some(trace) = trace {
                        // Producer capture already entered Trace's own queue; finish drains it
                        // before publishing the terminal durable manifest boundary.
                        let manifest = trace.finish().await;
                        let at = now();
                        let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                        let persisted = if let Some(run_id) = run_id.as_deref() {
                            if let Some(interaction_id) = grouping.interaction_for_run(run_id) {
                                match store
                                    .persist_manifest_event(
                                        interaction_id,
                                        run_id,
                                        &manifest,
                                        at,
                                        expiry,
                                        true,
                                    )
                                    .await
                                {
                                    Ok(event) => {
                                        publish(&updates, &trace_sequence, event);
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            } else {
                                store
                                    .save_manifest(Some(run_id), None, &manifest, at, expiry, true)
                                    .await
                            }
                        } else {
                            store
                                .save_manifest(
                                    None,
                                    rejection_id.as_deref(),
                                    &manifest,
                                    at,
                                    expiry,
                                    true,
                                )
                                .await
                        };
                        match persisted {
                            Ok(()) => persisted_partial = manifest.status == "partial",
                            Err(error) => {
                                tracing::warn!(trace_id=%manifest.trace_id,%error,"trace manifest persistence failed");
                            }
                        }
                    }
                    if let Some(run_id) = run_id {
                        let mut active = active_traces.lock().expect("trace registry");
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
                            grouping.interaction_for_run(run) == Some(interaction_id.as_str())
                        })
                        .cloned()
                        .collect();
                    for run in runs {
                        flush_one(
                            &store,
                            &grouping,
                            &retention_days,
                            &updates,
                            &trace_sequence,
                            &mut pending_text,
                            &run,
                        )
                        .await;
                    }
                    flush_active_manifests(
                        &store,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &grouping,
                        &active_traces,
                        &mut persisted_manifests,
                        Some(&interaction_id),
                    )
                    .await;
                    let _ = done.send(());
                }
                Some(WriterCommand::Barrier(done)) => {
                    flush_text(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                    )
                    .await;
                    flush_active_manifests(
                        &store,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &grouping,
                        &active_traces,
                        &mut persisted_manifests,
                        None,
                    )
                    .await;
                    let _ = done.send(());
                }
                Some(WriterCommand::Shutdown(done)) => {
                    flush_text(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                    )
                    .await;
                    let _ = done.send(());
                    break;
                }
                None => {
                    flush_text(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &trace_sequence,
                        &mut pending_text,
                    )
                    .await;
                    break;
                }
            }
        }
    });
    (tx, handle)
}

async fn flush_active_manifests(
    store: &ObservationStore,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    trace_sequence: &AtomicI64,
    grouping: &GroupingIndex,
    active_traces: &Mutex<HashMap<String, super::trace::TraceHandle>>,
    persisted: &mut HashMap<String, TraceManifest>,
    only_interaction: Option<&str>,
) {
    let active: Vec<_> = active_traces
        .lock()
        .expect("trace registry")
        .iter()
        .map(|(run_id, trace)| (run_id.clone(), trace.clone()))
        .collect();
    for (run_id, trace) in active {
        let Some(interaction_id) = grouping.interaction_for_run(&run_id) else {
            continue;
        };
        if only_interaction.is_some_and(|id| id != interaction_id) {
            continue;
        }
        if trace.flush().await.is_err() {
            trace.mark_observation_gap();
        }
        let manifest = trace.manifest();
        if persisted
            .get(&run_id)
            .is_some_and(|previous| manifests_match(previous, &manifest))
        {
            continue;
        }
        let at = now();
        match store
            .persist_manifest_event(
                interaction_id,
                &run_id,
                &manifest,
                at,
                expires(at, retention.load(Ordering::Relaxed)),
                false,
            )
            .await
        {
            Ok(event) => {
                persisted.insert(run_id, manifest);
                publish(&updates, &trace_sequence, event);
            }
            Err(error) => {
                trace.mark_observation_gap();
                tracing::warn!(%run_id,%error,"active trace manifest persistence failed");
            }
        }
    }
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
    store: &ObservationStore,
    grouping: &mut GroupingIndex,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    trace_sequence: &AtomicI64,
    pending_text: &mut TextBuffer,
    run_id: &str,
    outcome: &RunOutcome,
) {
    flush_one(
        store,
        grouping,
        retention,
        updates,
        trace_sequence,
        pending_text,
        run_id,
    )
    .await;
    let at = now();
    if let Some(interaction) = grouping.interaction_for_run(run_id) {
        let expiry = expires(at, retention.load(Ordering::Relaxed));
        match store
            .finish_run(interaction, run_id, outcome, at, expiry)
            .await
        {
            Ok(value) => {
                publish(&updates, &trace_sequence, value);
            }
            Err(_) => {
                pending_text
                    .gaps
                    .lock()
                    .expect("observation gaps")
                    .record(run_id, at);
                let _ = store.mark_observation_gap(interaction).await;
                let _ = updates.send(ObservationUpdate::LiveGap {
                    interaction_id: interaction.to_owned(),
                    run_id: run_id.to_owned(),
                    reason: "persistence_failed".into(),
                });
                tracing::warn!(%run_id, "observation finish persistence failed");
            }
        }
    }
    grouping.finish(run_id, &outcome.status, at);
}

async fn flush_text(
    store: &ObservationStore,
    grouping: &GroupingIndex,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    trace_sequence: &AtomicI64,
    pending: &mut TextBuffer,
) {
    let ids: Vec<_> = pending.blocks.keys().cloned().collect();
    for id in ids {
        flush_one(
            store,
            grouping,
            retention,
            updates,
            trace_sequence,
            pending,
            &id,
        )
        .await;
    }
}
async fn flush_one(
    store: &ObservationStore,
    grouping: &GroupingIndex,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    trace_sequence: &AtomicI64,
    pending: &mut TextBuffer,
    run_id: &str,
) {
    let Some(block) = pending.blocks.remove(run_id) else {
        return;
    };
    let Some(interaction) = grouping.interaction_for_run(run_id) else {
        pending.live.remove(&block.id);
        return;
    };
    persist_blocks(
        store,
        retention,
        updates,
        trace_sequence,
        pending,
        interaction,
        run_id,
        vec![block],
    )
    .await;
}
async fn persist_blocks(
    store: &ObservationStore,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    trace_sequence: &AtomicI64,
    pending: &TextBuffer,
    interaction: &str,
    run_id: &str,
    blocks: Vec<TextBlock>,
) {
    let at = now();
    let events: Vec<_> = blocks
        .into_iter()
        .map(|block| {
            pending.publish_block(&block, updates);
            (block.event, Some(block.id), block.at)
        })
        .collect();
    let result = store
        .persist_run_events(
            interaction,
            run_id,
            &events,
            expires(at, retention.load(Ordering::Relaxed)),
        )
        .await;
    for (_, id, _) in &events {
        pending.live.remove(id.as_deref().expect("block id"));
    }
    match result {
        Ok(events) => {
            for event in events {
                publish(updates, trace_sequence, event);
            }
        }
        Err(_) => {
            pending
                .gaps
                .lock()
                .expect("observation gaps")
                .record(run_id, at);
            let _ = store.mark_observation_gap(interaction).await;
            let _ = updates.send(ObservationUpdate::LiveGap {
                interaction_id: interaction.to_owned(),
                run_id: run_id.to_owned(),
                reason: "persistence_failed".into(),
            });
            tracing::warn!(%run_id, "observation text persistence failed");
        }
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
    started: tokio::time::Instant,
    revision: u64,
    published: u64,
}
impl TextBlock {
    fn new(event: RunEvent, interaction: String, run: String) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: format!("{}-{}-{}", run, now(), NEXT.fetch_add(1, Ordering::Relaxed)),
            interaction,
            run,
            event,
            at: now(),
            started: tokio::time::Instant::now(),
            revision: 0,
            published: 0,
        }
    }
    fn due(&self) -> bool {
        self.started.elapsed() >= Duration::from_secs(2)
    }
    fn append(&mut self, text: &str) -> usize {
        let pending = text_mut(&mut self.event).expect("text block");
        let mut take = text
            .len()
            .min(super::codec::CONTENT_BLOCK_BYTES - pending.len());
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        if take > 0 {
            pending.push_str(&text[..take]);
            self.revision += 1;
        }
        take
    }
    fn live(&self) -> LiveContentBlock {
        let (kind, turn, attempt, text) = match &self.event {
            RunEvent::ClientVisibleContentDelta { text } => {
                ("client_visible_content_delta", None, None, text)
            }
            RunEvent::ModelThinkingDelta {
                model_turn_id,
                attempt_id,
                text,
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
    blocks: HashMap<String, TextBlock>,
    live: Arc<super::live::LiveState>,
    gaps: Arc<Mutex<super::UnpersistedGaps>>,
}
impl TextBuffer {
    fn publish_block(&self, block: &TextBlock, updates: &broadcast::Sender<ObservationUpdate>) {
        if block.revision == block.published {
            return;
        }
        let value = block.live();
        if self.live.replace(value.clone()) {
            if updates.receiver_count() > 0 {
                let _ = updates.send(ObservationUpdate::LiveContent(value));
            }
        } else {
            self.live.remove(&block.id);
            if updates.receiver_count() > 0 {
                let _ = updates.send(ObservationUpdate::LiveGap {
                    interaction_id: block.interaction.clone(),
                    run_id: block.run.clone(),
                    reason: "live_capacity".into(),
                });
            }
        }
    }
    fn publish_live(&mut self, updates: &broadcast::Sender<ObservationUpdate>) {
        for block in self.blocks.values_mut() {
            if block.revision == block.published {
                continue;
            }
            let value = block.live();
            if self.live.replace(value.clone()) {
                if updates.receiver_count() > 0 {
                    let _ = updates.send(ObservationUpdate::LiveContent(value));
                }
            } else {
                self.live.remove(&block.id);
                if updates.receiver_count() > 0 {
                    let _ = updates.send(ObservationUpdate::LiveGap {
                        interaction_id: block.interaction.clone(),
                        run_id: block.run.clone(),
                        reason: "live_capacity".into(),
                    });
                }
            }
            block.published = block.revision;
        }
    }
    fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.blocks.retain(|run, block| {
            if keep(run) {
                true
            } else {
                self.live.remove(&block.id);
                false
            }
        });
    }
}
fn text_mut(event: &mut RunEvent) -> Option<&mut String> {
    match event {
        RunEvent::ClientVisibleContentDelta { text }
        | RunEvent::ModelThinkingDelta { text, .. } => Some(text),
        _ => None,
    }
}
fn same_scope(left: &RunEvent, right: &RunEvent) -> bool {
    match (left, right) {
        (
            RunEvent::ClientVisibleContentDelta { .. },
            RunEvent::ClientVisibleContentDelta { .. },
        ) => true,
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
        ) => l == r && a == b,
        _ => false,
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;
    #[test]
    fn byte_and_time_seals_preserve_utf8_and_scope() {
        let original = "你好🙂abc".repeat(6000);
        let mut rest = original.as_str();
        let mut recovered = String::new();
        let event = RunEvent::ClientVisibleContentDelta {
            text: String::new(),
        };
        while !rest.is_empty() {
            let mut block = TextBlock::new(event.clone(), "interaction".into(), "run".into());
            let take = block.append(rest);
            assert!(take > 0 && take <= super::super::codec::CONTENT_BLOCK_BYTES);
            recovered.push_str(&block.live().text);
            rest = &rest[take..];
            assert!(!block.due());
            block.started -= Duration::from_secs(2);
            assert!(block.due());
        }
        assert_eq!(recovered, original);
        let thinking = |attempt: &str| RunEvent::ModelThinkingDelta {
            model_turn_id: "turn".into(),
            attempt_id: attempt.into(),
            text: String::new(),
        };
        assert!(!same_scope(&thinking("one"), &thinking("two")));
        assert!(!same_scope(&thinking("one"), &event));
        assert!(!same_scope(&event, &RunEvent::ClientOutputCommitted));
    }
    #[test]
    fn live_revisions_replace_and_committed_identity_is_removable() {
        let mirror = Arc::new(super::super::live::LiveState::default());
        let buffer = TextBuffer {
            blocks: HashMap::new(),
            live: mirror.clone(),
            gaps: Arc::new(Mutex::new(super::super::UnpersistedGaps::default())),
        };
        let (updates, mut receiver) = broadcast::channel(8);
        let mut block = TextBlock::new(
            RunEvent::ClientVisibleContentDelta {
                text: String::new(),
            },
            "interaction".into(),
            "run".into(),
        );
        block.append("first");
        buffer.publish_block(&block, &updates);
        block.append(" second");
        buffer.publish_block(&block, &updates);
        let ObservationUpdate::LiveContent(first) = receiver.try_recv().unwrap() else {
            panic!("live block");
        };
        let ObservationUpdate::LiveContent(second) = receiver.try_recv().unwrap() else {
            panic!("live block");
        };
        assert_eq!(first.block_id, second.block_id);
        assert!(first.revision < second.revision);
        assert_eq!(mirror.snapshot()[0].text, "first second");
        mirror.remove(&second.block_id);
        assert!(mirror.snapshot().is_empty());
    }
}
