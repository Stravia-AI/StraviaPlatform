use std::collections::HashMap;

use super::types::RunStart;

const RETRY_WINDOW_MS: i64 = 120_000;

#[derive(Debug, Clone)]
pub(super) struct GroupAssignment {
    pub interaction_id: String,
    pub parent_run_id: Option<String>,
    pub inferred_retry: bool,
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
    generation_nodes: HashMap<String, (String, String)>,
}

impl GroupingIndex {
    pub fn forget_interactions(&mut self, interactions: &[String]) {
        let removed: std::collections::HashSet<_> =
            interactions.iter().map(String::as_str).collect();
        self.runs
            .retain(|_, run| !removed.contains(run.interaction_id.as_str()));
        self.generation_nodes
            .retain(|_, (interaction, _)| !removed.contains(interaction.as_str()));
    }

    pub fn assign(&mut self, start: &RunStart, now: i64) -> GroupAssignment {
        let explicit_parent = start
            .generation_parent_id
            .as_ref()
            .and_then(|id| self.generation_nodes.get(id).cloned());
        let (interaction_id, parent_run_id, inferred_retry) =
            if let Some((interaction, run)) = explicit_parent {
                if start.has_new_user {
                    (uuid::Uuid::new_v4().to_string(), Some(run), false)
                } else {
                    (interaction, Some(run), false)
                }
            } else if start.generation_parent_id.is_some() {
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
        }
    }

    pub fn relink_run(&mut self, run_id: &str, interaction_id: &str) {
        if let Some(run) = self.runs.get_mut(run_id) {
            run.interaction_id = interaction_id.to_owned();
        }
    }

    pub fn generation_associated(&mut self, run_id: &str, node_id: &str) {
        if let Some(run) = self.runs.get(run_id) {
            self.generation_nodes.insert(
                node_id.to_owned(),
                (run.interaction_id.clone(), run_id.to_owned()),
            );
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
    for (status, is_leaf) in statuses {
        if status == "running" {
            return "running";
        }
        waiting |= is_leaf && status == "waiting_client";
        completed |= status == "completed";
    }
    if waiting {
        "waiting_client"
    } else if completed {
        "completed"
    } else {
        "interrupted"
    }
}

pub(super) fn add_usage(total: &mut Option<i64>, incoming: Option<i64>) {
    *total = total
        .zip(incoming)
        .and_then(|(current, value)| current.checked_add(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_attempt_usage_remains_unknown_after_known_attempts() {
        let mut total = Some(0);
        add_usage(&mut total, None);
        add_usage(&mut total, Some(12));
        assert_eq!(total, None);
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
            canonical_fingerprint: fingerprint.into(),
            route_id: "route".into(),
            model_display_name: None,
            ingress_protocol: "openai".into(),
        }
    }
    #[test]
    fn failed_root_retry_requires_every_exact_guard() {
        let mut index = GroupingIndex::default();
        let first = index.assign(&start("one", "principal", "exact"), 1_000);
        index.finish("one", "failed", 2_000);
        let retry = index.assign(&start("two", "principal", "exact"), 121_999);
        assert_eq!(retry.interaction_id, first.interaction_id);
        assert!(retry.inferred_retry);
        index.finish("two", "failed", 123_000);
        let late = index.assign(&start("three", "principal", "exact"), 243_000);
        assert_ne!(late.interaction_id, first.interaction_id);
        let other = index.assign(&start("four", "other", "exact"), 243_002);
        assert_ne!(other.interaction_id, first.interaction_id);
    }
    #[test]
    fn output_commit_and_concurrency_prevent_inferred_retry() {
        let mut committed = GroupingIndex::default();
        let first = committed.assign(&start("one", "p", "f"), 0);
        committed.output_committed("one");
        committed.finish("one", "failed", 1);
        let retry = committed.assign(&start("two", "p", "f"), 2);
        assert_ne!(retry.interaction_id, first.interaction_id);
        let mut concurrent = GroupingIndex::default();
        let active = concurrent.assign(&start("active", "p", "f"), 0);
        let duplicate = concurrent.assign(&start("duplicate", "p", "f"), 1);
        assert_ne!(duplicate.interaction_id, active.interaction_id);
    }
    #[test]
    fn explicit_parent_never_uses_failed_root_inference() {
        let mut index = GroupingIndex::default();
        let failed = index.assign(&start("failed", "p", "f"), 0);
        index.finish("failed", "failed", 1);
        let mut continuation = start("continuation", "p", "f");
        continuation.generation_parent_id = Some("unobserved-parent".into());
        continuation.has_new_user = false;
        let assigned = index.assign(&continuation, 2);
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
