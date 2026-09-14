use std::collections::{HashMap, HashSet};

use super::types::RunStart;

const RETRY_WINDOW_MS: i64 = 120_000;

#[derive(Debug, Clone)]
pub(super) struct GroupAssignment {
    pub interaction_id: String,
    pub parent_run_id: Option<String>,
    pub inferred_retry: bool,
    pub grouping_reason: &'static str,
    pub parent_interaction_id: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct ObservedParent {
    pub interaction_id: String,
    pub run_id: String,
    pub delivery_completed_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct RetryCandidate {
    interaction_id: String,
    run_id: String,
    principal: String,
    fingerprint: String,
    failed_at: Option<i64>,
    client_output_committed: bool,
    active: bool,
}

#[derive(Default)]
pub(super) struct GroupingIndex {
    runs: HashMap<String, RetryCandidate>,
}

impl GroupingIndex {
    pub fn forget_interactions(&mut self, interactions: &[String]) {
        let removed: std::collections::HashSet<_> =
            interactions.iter().map(String::as_str).collect();
        self.runs
            .retain(|_, run| !removed.contains(run.interaction_id.as_str()));
    }

    pub fn assign(
        &mut self,
        start: &RunStart,
        now: i64,
        parent: Option<&ObservedParent>,
    ) -> GroupAssignment {
        let mut grouping_reason = "new_root";
        let mut parent_interaction_id = None;
        let (interaction_id, parent_run_id, inferred_retry) = if let Some(parent) = parent {
            let continuation = if !start.has_new_user {
                Some("exact_continuation")
            } else if start.has_matching_pending_tool_result {
                Some("pending_tool_result")
            } else if parent.delivery_completed_at.is_some_and(|delivered_at| {
                start
                    .ingress_received_at
                    .checked_sub(delivered_at)
                    .is_some_and(|elapsed| (0..=2000).contains(&elapsed))
            }) {
                Some("rapid_exact_continuation")
            } else {
                None
            };
            if let Some(reason) = continuation {
                grouping_reason = reason;
                (
                    parent.interaction_id.clone(),
                    Some(parent.run_id.clone()),
                    false,
                )
            } else {
                grouping_reason = "new_user";
                parent_interaction_id = Some(parent.interaction_id.clone());
                (
                    uuid::Uuid::new_v4().to_string(),
                    Some(parent.run_id.clone()),
                    false,
                )
            }
        } else if start.generation_parent_id.is_some() {
            grouping_reason = "unmatched_parent";
            (uuid::Uuid::new_v4().to_string(), None, false)
        } else {
            let candidate = self
                .runs
                .values()
                .filter(|candidate| {
                    candidate.principal == start.principal
                        && candidate.fingerprint == start.canonical_fingerprint
                        && !candidate.active
                        && !candidate.client_output_committed
                        && candidate.failed_at.is_some_and(|failed_at| {
                            now >= failed_at && now.saturating_sub(failed_at) < RETRY_WINDOW_MS
                        })
                })
                .max_by_key(|candidate| candidate.failed_at);
            let identical_active = self.runs.values().any(|candidate| {
                candidate.principal == start.principal
                    && candidate.fingerprint == start.canonical_fingerprint
                    && candidate.active
            });
            if !identical_active {
                if let Some(candidate) = candidate {
                    grouping_reason = "inferred_retry";
                    (
                        candidate.interaction_id.clone(),
                        Some(candidate.run_id.clone()),
                        true,
                    )
                } else {
                    (uuid::Uuid::new_v4().to_string(), None, false)
                }
            } else {
                (uuid::Uuid::new_v4().to_string(), None, false)
            }
        };
        self.runs.insert(
            start.id.clone(),
            RetryCandidate {
                interaction_id: interaction_id.clone(),
                run_id: start.id.clone(),
                principal: start.principal.clone(),
                fingerprint: start.canonical_fingerprint.clone(),
                failed_at: None,
                client_output_committed: false,
                active: true,
            },
        );
        GroupAssignment {
            interaction_id,
            parent_run_id,
            inferred_retry,
            grouping_reason,
            parent_interaction_id,
        }
    }

    pub fn output_committed(&mut self, run_id: &str) {
        if let Some(run) = self.runs.get_mut(run_id) {
            run.client_output_committed = true;
        }
    }

    pub fn finish(&mut self, run_id: &str, status: &str, now: i64) {
        if let Some(run) = self.runs.get_mut(run_id) {
            run.active = false;
            if status == "failed" || status == "delivery_failed" {
                run.failed_at = Some(now);
            }
        }
    }

    pub fn interaction_for_run(&self, run_id: &str) -> Option<&str> {
        self.runs.get(run_id).map(|run| run.interaction_id.as_str())
    }
}

pub(super) fn rollup_status<'a>(
    statuses: impl IntoIterator<Item = (&'a str, bool)>,
) -> &'static str {
    let mut waiting = false;
    let mut completed = false;
    let mut disconnected = false;
    for (status, is_leaf) in statuses {
        if status == "running" {
            return "running";
        }
        waiting |= is_leaf && status == "waiting_client";
        completed |= status == "completed";
        disconnected |= is_leaf && status == "disconnected";
    }
    if waiting {
        "waiting_client"
    } else if completed {
        "completed"
    } else if disconnected {
        "disconnected"
    } else {
        "interrupted"
    }
}

pub(super) struct ClientToolEvidence<'a> {
    pub sequence: i64,
    pub run_id: &'a str,
    pub tool_id: Option<&'a str>,
    pub is_handoff: bool,
}

/// 输入为同一交互按 sequence 排序的证据；重复 handoff ID 不能证明分支归属。
pub(super) fn resolved_client_tool_runs<'a>(
    events: impl IntoIterator<Item = ClientToolEvidence<'a>>,
) -> HashSet<&'a str> {
    let mut runs = HashMap::new();
    let mut calls = HashMap::<&str, (&str, i64, bool)>::new();
    for event in events {
        if event.is_handoff {
            runs.entry(event.run_id).or_insert(true);
            let Some(id) = event.tool_id.filter(|id| !id.is_empty()) else {
                runs.insert(event.run_id, false);
                continue;
            };
            match calls.entry(id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((event.run_id, event.sequence, false));
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    runs.insert(entry.get().0, false);
                    runs.insert(event.run_id, false);
                }
            }
        } else if let Some((owner, sequence, returned)) =
            event.tool_id.and_then(|id| calls.get_mut(id))
        {
            if *owner != event.run_id && event.sequence > *sequence {
                *returned = true;
            }
        }
    }
    for (run, _, returned) in calls.into_values() {
        if !returned {
            runs.insert(run, false);
        }
    }
    runs.into_iter()
        .filter_map(|(run, resolved)| resolved.then_some(run))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_evidence_requires_complete_unambiguous_later_returns() {
        let evidence = |sequence, run_id, tool_id, is_handoff| ClientToolEvidence {
            sequence,
            run_id,
            tool_id,
            is_handoff,
        };
        assert_eq!(
            resolved_client_tool_runs([
                evidence(1, "waiting", Some("a"), true),
                evidence(2, "waiting", Some("b"), true),
                evidence(3, "sibling", Some("a"), false),
                evidence(4, "sibling", Some("b"), false),
                evidence(5, "sibling", Some("b"), false),
            ]),
            HashSet::from(["waiting"])
        );
        for events in [
            vec![evidence(1, "w", Some("a"), true)],
            vec![evidence(1, "r", Some("a"), false)],
            vec![
                evidence(1, "r", Some("a"), false),
                evidence(2, "w", Some("a"), true),
            ],
            vec![evidence(1, "w", None, true), evidence(2, "r", None, false)],
            vec![
                evidence(1, "w", Some(""), true),
                evidence(2, "r", Some(""), false),
            ],
            vec![
                evidence(1, "w", Some("a"), true),
                evidence(2, "w", Some("a"), false),
            ],
            vec![
                evidence(1, "w", Some("a"), true),
                evidence(2, "r", Some("a"), false),
                evidence(3, "other", Some("a"), true),
                evidence(4, "r", Some("a"), false),
            ],
            vec![
                evidence(1, "w", Some("a"), true),
                evidence(2, "w", Some("b"), true),
                evidence(3, "r", Some("a"), false),
            ],
        ] {
            assert!(resolved_client_tool_runs(events).is_empty());
        }
    }

    fn start(id: &str, principal: &str, fingerprint: &str) -> RunStart {
        RunStart {
            id: id.into(),
            principal: principal.into(),
            api_key_id: None,
            api_key_name: None,
            generation_root_id: None,
            generation_parent_id: None,
            has_new_user: true,
            has_matching_pending_tool_result: false,
            ingress_received_at: 0,
            canonical_fingerprint: fingerprint.into(),
            route_id: "route".into(),
            model_display_name: None,
            ingress_protocol: "openai".into(),
        }
    }
    #[test]
    fn failed_root_retry_requires_every_exact_guard() {
        let mut index = GroupingIndex::default();
        let first = index.assign(&start("one", "principal", "exact"), 1_000, None);
        index.finish("one", "failed", 2_000);
        let retry = index.assign(&start("two", "principal", "exact"), 121_999, None);
        assert_eq!(retry.interaction_id, first.interaction_id);
        assert!(retry.inferred_retry);
        index.finish("two", "failed", 123_000);
        let late = index.assign(&start("three", "principal", "exact"), 243_000, None);
        assert_ne!(late.interaction_id, first.interaction_id);
        let other = index.assign(&start("four", "other", "exact"), 243_002, None);
        assert_ne!(other.interaction_id, first.interaction_id);
    }
    #[test]
    fn output_commit_and_concurrency_prevent_inferred_retry() {
        let mut committed = GroupingIndex::default();
        let first = committed.assign(&start("one", "p", "f"), 0, None);
        committed.output_committed("one");
        committed.finish("one", "failed", 1);
        let retry = committed.assign(&start("two", "p", "f"), 2, None);
        assert_ne!(retry.interaction_id, first.interaction_id);
        let mut concurrent = GroupingIndex::default();
        let active = concurrent.assign(&start("active", "p", "f"), 0, None);
        let duplicate = concurrent.assign(&start("duplicate", "p", "f"), 1, None);
        assert_ne!(duplicate.interaction_id, active.interaction_id);
    }
    #[test]
    fn explicit_parent_never_uses_failed_root_inference() {
        let mut index = GroupingIndex::default();
        let failed = index.assign(&start("failed", "p", "f"), 0, None);
        index.finish("failed", "failed", 1);
        let mut continuation = start("continuation", "p", "f");
        continuation.generation_parent_id = Some("unobserved-parent".into());
        continuation.has_new_user = false;
        let assigned = index.assign(&continuation, 2, None);
        assert_ne!(assigned.interaction_id, failed.interaction_id);
        assert!(!assigned.inferred_retry);
        assert_eq!(assigned.parent_run_id, None);
    }
    #[test]
    fn status_rollup_is_activity_first() {
        assert_eq!(
            rollup_status([("completed".into(), true), ("running".into(), true)]),
            "running"
        );
        assert_eq!(
            rollup_status([("completed".into(), false), ("waiting_client".into(), true)]),
            "waiting_client"
        );
        assert_eq!(
            rollup_status([("completed".into(), true), ("failed".into(), true)]),
            "completed"
        );
        assert_eq!(rollup_status([("failed".into(), true)]), "interrupted");
    }
}
