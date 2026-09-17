use std::collections::{HashMap, HashSet};

const RETRY_WINDOW_MS: i64 = 120_000;
pub(super) const TAIL_MERGE_WINDOW_MS: i64 = 300_000;

/// Kernel input: caller-confirmed facts plus evidence Run Attribution computed.
/// The decision table never reads request or store types directly.
#[derive(Debug, Clone)]
pub(super) struct AssignInput<'a> {
    pub run_id: &'a str,
    pub principal: &'a str,
    pub canonical_fingerprint: &'a str,
    pub has_new_user: bool,
    pub has_matching_pending_tool_result: bool,
    pub generation_parent_id: Option<&'a str>,
    pub ingress_received_at: i64,
}

#[derive(Debug, Clone)]
pub(super) struct GroupAssignment {
    pub interaction_id: String,
    pub parent_run_id: Option<String>,
    pub inferred_retry: bool,
    pub grouping_reason: &'static str,
    pub parent_interaction_id: Option<String>,
    pub diagnostic_source_run_id: Option<String>,
    pub interrupt_parent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiagnosticKind {
    CurrentTool,
    RetainedTail,
}

#[derive(Debug, Clone)]
pub(super) struct DiagnosticSource {
    pub run_id: String,
    pub interaction_id: String,
    pub delivery_completed_at: Option<i64>,
    pub kind: DiagnosticKind,
    pub user_after_match: bool,
}

impl DiagnosticSource {
    pub(super) fn tail_merge_eligible(&self, ingress_received_at: i64) -> bool {
        self.delivery_completed_at.is_some_and(|delivered_at| {
            ingress_received_at
                .checked_sub(delivered_at)
                .is_some_and(|elapsed| (0..=TAIL_MERGE_WINDOW_MS).contains(&elapsed))
        })
    }
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
        input: AssignInput<'_>,
        now: i64,
        parent: Option<&ObservedParent>,
        diagnostic: Option<&DiagnosticSource>,
    ) -> GroupAssignment {
        let mut grouping_reason = "new_root";
        let mut parent_interaction_id = None;
        let mut diagnostic_source_run_id = None;
        let mut interrupt_parent = false;
        let (interaction_id, parent_run_id, inferred_retry) = if let Some(parent) = parent {
            let continuation = if !input.has_new_user {
                Some("exact_continuation")
            } else if input.has_matching_pending_tool_result {
                Some("pending_tool_result")
            } else if parent.delivery_completed_at.is_some_and(|delivered_at| {
                input
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
                interrupt_parent = true;
                (
                    stravia_runtime_contract::identifier::new_id(),
                    Some(parent.run_id.clone()),
                    false,
                )
            }
        } else if input.generation_parent_id.is_some() {
            grouping_reason = "unmatched_parent";
            (stravia_runtime_contract::identifier::new_id(), None, false)
        } else if let Some(source) = diagnostic {
            diagnostic_source_run_id = Some(source.run_id.clone());
            if source.kind == DiagnosticKind::CurrentTool
                || (!source.user_after_match
                    && source.tail_merge_eligible(input.ingress_received_at))
            {
                grouping_reason = if source.kind == DiagnosticKind::CurrentTool {
                    "current_tool_continuation"
                } else {
                    "retained_tail_continuation"
                };
                (
                    source.interaction_id.clone(),
                    Some(source.run_id.clone()),
                    false,
                )
            } else {
                grouping_reason = "retained_tail_linked";
                parent_interaction_id = Some(source.interaction_id.clone());
                interrupt_parent = source.user_after_match;
                (
                    stravia_runtime_contract::identifier::new_id(),
                    Some(source.run_id.clone()),
                    false,
                )
            }
        } else {
            let candidate = self
                .runs
                .values()
                .filter(|candidate| {
                    candidate.principal == input.principal
                        && candidate.fingerprint == input.canonical_fingerprint
                        && !candidate.active
                        && !candidate.client_output_committed
                        && candidate.failed_at.is_some_and(|failed_at| {
                            now >= failed_at && now.saturating_sub(failed_at) < RETRY_WINDOW_MS
                        })
                })
                .max_by_key(|candidate| candidate.failed_at);
            let identical_active = self.runs.values().any(|candidate| {
                candidate.principal == input.principal
                    && candidate.fingerprint == input.canonical_fingerprint
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
                    (stravia_runtime_contract::identifier::new_id(), None, false)
                }
            } else {
                (stravia_runtime_contract::identifier::new_id(), None, false)
            }
        };
        self.runs.insert(
            input.run_id.to_owned(),
            RetryCandidate {
                interaction_id: interaction_id.clone(),
                run_id: input.run_id.to_owned(),
                principal: input.principal.to_owned(),
                fingerprint: input.canonical_fingerprint.to_owned(),
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
            diagnostic_source_run_id,
            interrupt_parent,
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
            && *owner != event.run_id
            && event.sequence > *sequence
        {
            *returned = true;
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

    fn input<'a>(id: &'a str, principal: &'a str, fingerprint: &'a str) -> AssignInput<'a> {
        AssignInput {
            run_id: id,
            principal,
            canonical_fingerprint: fingerprint,
            has_new_user: true,
            has_matching_pending_tool_result: false,
            generation_parent_id: None,
            ingress_received_at: 0,
        }
    }
    #[test]
    fn failed_root_retry_requires_every_exact_guard() {
        let mut index = GroupingIndex::default();
        let first = index.assign(input("one", "principal", "exact"), 1_000, None, None);
        index.finish("one", "failed", 2_000);
        let retry = index.assign(input("two", "principal", "exact"), 121_999, None, None);
        assert_eq!(retry.interaction_id, first.interaction_id);
        assert!(retry.inferred_retry);
        index.finish("two", "failed", 123_000);
        let late = index.assign(input("three", "principal", "exact"), 243_000, None, None);
        assert_ne!(late.interaction_id, first.interaction_id);
        let other = index.assign(input("four", "other", "exact"), 243_002, None, None);
        assert_ne!(other.interaction_id, first.interaction_id);
    }
    #[test]
    fn output_commit_and_concurrency_prevent_inferred_retry() {
        let mut committed = GroupingIndex::default();
        let first = committed.assign(input("one", "p", "f"), 0, None, None);
        committed.output_committed("one");
        committed.finish("one", "failed", 1);
        let retry = committed.assign(input("two", "p", "f"), 2, None, None);
        assert_ne!(retry.interaction_id, first.interaction_id);
        let mut concurrent = GroupingIndex::default();
        let active = concurrent.assign(input("active", "p", "f"), 0, None, None);
        let duplicate = concurrent.assign(input("duplicate", "p", "f"), 1, None, None);
        assert_ne!(duplicate.interaction_id, active.interaction_id);
    }
    #[test]
    fn explicit_parent_never_uses_failed_root_inference() {
        let mut index = GroupingIndex::default();
        let failed = index.assign(input("failed", "p", "f"), 0, None, None);
        index.finish("failed", "failed", 1);
        let mut continuation = input("continuation", "p", "f");
        continuation.generation_parent_id = Some("unobserved-parent");
        continuation.has_new_user = false;
        let assigned = index.assign(continuation, 2, None, None);
        assert_ne!(assigned.interaction_id, failed.interaction_id);
        assert!(!assigned.inferred_retry);
        assert_eq!(assigned.parent_run_id, None);
    }

    fn source(
        kind: DiagnosticKind,
        user_after_match: bool,
        delivered_at: Option<i64>,
    ) -> DiagnosticSource {
        DiagnosticSource {
            run_id: "source-run".into(),
            interaction_id: "source-interaction".into(),
            delivery_completed_at: delivered_at,
            kind,
            user_after_match,
        }
    }

    #[test]
    fn current_tool_continuation_merges_without_window_or_user_split() {
        let mut index = GroupingIndex::default();
        let mut input = input("child", "p", "f");
        input.has_new_user = true;
        input.ingress_received_at = 400_000;
        let assigned = index.assign(
            input,
            400_000,
            None,
            Some(&source(DiagnosticKind::CurrentTool, true, Some(1))),
        );
        assert_eq!(assigned.interaction_id, "source-interaction");
        assert_eq!(assigned.parent_run_id.as_deref(), Some("source-run"));
        assert_eq!(assigned.grouping_reason, "current_tool_continuation");
        assert_eq!(assigned.parent_interaction_id, None);
        assert!(!assigned.interrupt_parent);
        assert_eq!(
            assigned.diagnostic_source_run_id.as_deref(),
            Some("source-run")
        );
    }

    #[test]
    fn retained_tail_merges_on_inclusive_five_minute_window() {
        let mut index = GroupingIndex::default();
        for elapsed in [0_i64, TAIL_MERGE_WINDOW_MS] {
            let run_id = format!("child-{elapsed}");
            let mut input = input(&run_id, "p", "f");
            input.has_new_user = false;
            input.ingress_received_at = 10_000 + elapsed;
            let received_at = input.ingress_received_at;
            let assigned = index.assign(
                input,
                received_at,
                None,
                Some(&source(DiagnosticKind::RetainedTail, false, Some(10_000))),
            );
            assert_eq!(assigned.interaction_id, "source-interaction", "{elapsed}");
            assert_eq!(assigned.grouping_reason, "retained_tail_continuation");
            assert_eq!(assigned.parent_interaction_id, None);
            assert!(!assigned.interrupt_parent);
        }
    }

    #[test]
    fn retained_tail_links_new_interaction_outside_window_or_without_time() {
        let mut index = GroupingIndex::default();
        let cases = [
            (TAIL_MERGE_WINDOW_MS + 1, Some(10_000_i64)),
            (1, None),
            (-1, Some(10_000)),
        ];
        for (elapsed, delivered_at) in cases {
            let run_id = format!("child-{elapsed}-{delivered_at:?}");
            let mut input = input(&run_id, "p", "f");
            input.ingress_received_at = delivered_at.unwrap_or(10_000) + elapsed;
            let received_at = input.ingress_received_at;
            let assigned = index.assign(
                input,
                received_at,
                None,
                Some(&source(DiagnosticKind::RetainedTail, false, delivered_at)),
            );
            assert_ne!(assigned.interaction_id, "source-interaction");
            assert_eq!(
                assigned.parent_interaction_id.as_deref(),
                Some("source-interaction")
            );
            assert_eq!(assigned.grouping_reason, "retained_tail_linked");
            assert!(!assigned.interrupt_parent);
        }
    }

    #[test]
    fn new_user_after_retained_tail_creates_child_and_interrupts() {
        let mut index = GroupingIndex::default();
        let mut input = input("child", "p", "f");
        input.has_new_user = true;
        input.ingress_received_at = 10_001;
        let assigned = index.assign(
            input,
            10_001,
            None,
            Some(&source(DiagnosticKind::RetainedTail, true, Some(10_000))),
        );
        assert_ne!(assigned.interaction_id, "source-interaction");
        assert_eq!(
            assigned.parent_interaction_id.as_deref(),
            Some("source-interaction")
        );
        assert_eq!(assigned.grouping_reason, "retained_tail_linked");
        assert!(assigned.interrupt_parent);
    }

    #[test]
    fn confirmed_generation_parent_ignores_diagnostic_source() {
        let mut index = GroupingIndex::default();
        let mut input = input("child", "p", "f");
        input.has_new_user = false;
        input.generation_parent_id = Some("node");
        let parent = ObservedParent {
            interaction_id: "exec-interaction".into(),
            run_id: "exec-run".into(),
            delivery_completed_at: Some(1),
        };
        let assigned = index.assign(
            input,
            2,
            Some(&parent),
            Some(&source(DiagnosticKind::CurrentTool, false, Some(1))),
        );
        assert_eq!(assigned.interaction_id, "exec-interaction");
        assert_eq!(assigned.grouping_reason, "exact_continuation");
        assert_eq!(assigned.diagnostic_source_run_id, None);
    }

    #[test]
    fn status_rollup_is_activity_first() {
        assert_eq!(
            rollup_status([("completed", true), ("running", true)]),
            "running"
        );
        assert_eq!(
            rollup_status([("completed", false), ("waiting_client", true)]),
            "waiting_client"
        );
        assert_eq!(
            rollup_status([("completed", true), ("failed", true)]),
            "completed"
        );
        assert_eq!(rollup_status([("failed", true)]), "interrupted");
    }
}
