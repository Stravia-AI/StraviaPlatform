use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
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
        trace: Option<super::trace::TraceHandle>,
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
    Barrier(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

pub(super) fn spawn(
    store: ObservationStore,
    retention_days: Arc<AtomicU32>,
    updates: broadcast::Sender<ObservationUpdate>,
    traces: super::trace::TraceManager,
    active_traces: Arc<Mutex<HashMap<String, super::trace::TraceHandle>>>,
    partial_trace_count: Arc<AtomicU64>,
    unpersisted_gaps: Arc<Mutex<super::UnpersistedGaps>>,
) -> (mpsc::Sender<WriterCommand>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(2048);
    let handle = tokio::spawn(async move {
        let mut grouping = GroupingIndex::default();
        let mut tail = super::tail::TailIndex::default();
        // 只合并同一 Run 中相邻且同作用域的正文或思考增量，不跨事件边界重排。
        let mut pending_text: HashMap<String, RunEvent> = HashMap::new();
        let mut pending_gaps: HashMap<String, i64> = HashMap::new();
        let mut persisted_manifests: HashMap<String, TraceManifest> = HashMap::new();
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // 仅重试缺失标记，不重放发现，避免把诊断故障转化为重复计数或执行失败。
            for (interaction_id, occurred_at) in std::mem::take(&mut pending_gaps) {
                if expires(occurred_at, retention_days.load(Ordering::Acquire)) > now()
                    && !matches!(store.mark_observation_gap(&interaction_id).await, Ok(true))
                {
                    pending_gaps.insert(interaction_id, occurred_at);
                }
            }
            let command = tokio::select! {
                _ = interval.tick() => {
                    unpersisted_gaps.lock().expect("observation gaps")
                        .expire(now(), retention_days.load(Ordering::Acquire));
                    tail.sweep(now());
                    flush_text(&store, &grouping, &retention_days, &updates, &mut pending_text).await;
                    flush_active_manifests(
                        &store,
                        &retention_days,
                        &updates,
                        &grouping,
                        &active_traces,
                        &mut persisted_manifests,
                    )
                    .await;
                    continue;
                },
                command = rx.recv() => command,
            };
            tail.sweep(now());
            match command {
                Some(WriterCommand::ClearTail) => tail = super::tail::TailIndex::default(),
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
                            let _ = updates.send(ObservationUpdate::Event(event));
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
                        pending_text.retain(|run, _| grouping.interaction_for_run(run).is_some());
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
                            let _ = updates.send(ObservationUpdate::Event(event));
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
                            let _ = updates.send(ObservationUpdate::Event(event));
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
                    let append = match (pending_text.get_mut(&run_id), &event) {
                        (
                            Some(RunEvent::ClientVisibleContentDelta { text: pending }),
                            RunEvent::ClientVisibleContentDelta { text },
                        ) => Some((pending, text)),
                        (
                            Some(RunEvent::ModelThinkingDelta {
                                model_turn_id: pending_turn,
                                attempt_id: pending_attempt,
                                text: pending,
                            }),
                            RunEvent::ModelThinkingDelta {
                                model_turn_id,
                                attempt_id,
                                text,
                            },
                        ) if pending_turn == model_turn_id && pending_attempt == attempt_id => {
                            Some((pending, text))
                        }
                        _ => None,
                    };
                    if let Some((pending, text)) = append {
                        pending.push_str(text);
                    } else {
                        flush_one(
                            &store,
                            &grouping,
                            &retention_days,
                            &updates,
                            &mut pending_text,
                            &run_id,
                        )
                        .await;
                        pending_text.insert(run_id, event);
                    }
                }
                Some(WriterCommand::Event {
                    run_id,
                    event,
                    trace,
                }) => {
                    flush_one(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &mut pending_text,
                        &run_id,
                    )
                    .await;
                    if matches!(event, RunEvent::ClientOutputCommitted) {
                        grouping.output_committed(&run_id);
                    }
                    if let Some(interaction) = grouping.interaction_for_run(&run_id) {
                        let at = now();
                        let expiry = expires(at, retention_days.load(Ordering::Relaxed));
                        match store
                            .persist_run_event(interaction, &run_id, &event, at, expiry)
                            .await
                        {
                            Ok(Some(value)) => {
                                if let Some(trace) = trace {
                                    super::record_trace_at(
                                        &trace,
                                        Some(&run_id),
                                        None,
                                        event,
                                        value.sequence,
                                    );
                                }
                                let _ = updates.send(ObservationUpdate::Event(value));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                unpersisted_gaps
                                    .lock()
                                    .expect("observation gaps")
                                    .record(&run_id, at);
                                pending_gaps.insert(interaction.to_owned(), at);
                                if let Some(trace) = trace {
                                    trace.mark_observation_gap();
                                }
                                tracing::warn!(%run_id,%error,"observation event persistence failed")
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
                            let _ = updates.send(ObservationUpdate::Event(value));
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
                                        let _ = updates.send(ObservationUpdate::Event(event));
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
                                &mut pending_text,
                                run_id,
                                &outcome,
                            )
                            .await;
                        }
                    }
                    let mut persisted_partial = false;
                    if let Some(trace) = trace {
                        // 同一队列先提交事件、再关闭 Trace，避免关闭越过尚未写出的记录。
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
                                        let _ = updates.send(ObservationUpdate::Event(event));
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
                Some(WriterCommand::Barrier(done)) => {
                    flush_text(
                        &store,
                        &grouping,
                        &retention_days,
                        &updates,
                        &mut pending_text,
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
    grouping: &GroupingIndex,
    active_traces: &Mutex<HashMap<String, super::trace::TraceHandle>>,
    persisted: &mut HashMap<String, TraceManifest>,
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
                let _ = updates.send(ObservationUpdate::Event(event));
            }
            Err(error) => {
                trace.mark_observation_gap();
                tracing::warn!(%run_id,%error,"active trace manifest persistence failed");
            }
        }
    }
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
    pending_text: &mut HashMap<String, RunEvent>,
    run_id: &str,
    outcome: &RunOutcome,
) {
    flush_one(store, grouping, retention, updates, pending_text, run_id).await;
    let at = now();
    if let Some(interaction) = grouping.interaction_for_run(run_id) {
        let expiry = expires(at, retention.load(Ordering::Relaxed));
        match store
            .finish_run(interaction, run_id, outcome, at, expiry)
            .await
        {
            Ok(value) => {
                let _ = updates.send(ObservationUpdate::Event(value));
            }
            Err(error) => tracing::warn!(%run_id,%error,"observation finish persistence failed"),
        }
    }
    grouping.finish(run_id, &outcome.status, at);
}

async fn flush_text(
    store: &ObservationStore,
    grouping: &GroupingIndex,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    pending: &mut HashMap<String, RunEvent>,
) {
    let ids: Vec<_> = pending.keys().cloned().collect();
    for id in ids {
        flush_one(store, grouping, retention, updates, pending, &id).await;
    }
}
async fn flush_one(
    store: &ObservationStore,
    grouping: &GroupingIndex,
    retention: &AtomicU32,
    updates: &broadcast::Sender<ObservationUpdate>,
    pending: &mut HashMap<String, RunEvent>,
    run_id: &str,
) {
    let Some(mut event) = pending.remove(run_id) else {
        return;
    };
    let Some(interaction) = grouping.interaction_for_run(run_id) else {
        return;
    };
    let at = now();
    super::redaction::redact_run_event(&mut event);
    match store
        .persist_run_event(
            interaction,
            run_id,
            &event,
            at,
            expires(at, retention.load(Ordering::Relaxed)),
        )
        .await
    {
        Ok(Some(value)) => {
            let _ = updates.send(ObservationUpdate::Event(value));
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%run_id,%error,"observation text persistence failed"),
    }
}
pub(super) fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub(super) fn expires(at: i64, days: u32) -> i64 {
    at.saturating_add(i64::from(days).saturating_mul(86_400_000))
}
