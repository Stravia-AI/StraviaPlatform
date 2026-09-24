//! Run Attribution owns the single decision "这次 Inference Run 归入哪个 Connect
//! Client Interaction". It consumes caller-confirmed `AdmissionFacts`, merges
//! in-memory tail evidence with persisted evidence through the
//! `AttributionEvidence` seam, and produces an `Attribution` the writer only
//! persists and publishes. ADR-0053 fixes the decision semantics; this module
//! owns where they are computed.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use stravia_runtime_contract::Principal;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_runtime_contract::protocol::ir::canonical;

use super::{
    grouping::{AssignInput, DiagnosticKind, DiagnosticSource, GroupingIndex, ObservedParent},
    store::ObservationStore,
    tail::{MAX_CANDIDATES, TailIndex, Window},
    types::{RunEvent, RunStart},
};

/// Inputs for the tail-diagnostic pass: the admitted run identity plus the
/// captured request window (or its overflow marker) and timing facts.
struct DiscoverInput<'a> {
    run_id: &'a str,
    principal: &'a str,
    generation_parent_id: Option<&'a str>,
    input: Option<&'a Window>,
    input_overflow: bool,
    ingress_received_at: i64,
    now: i64,
}

/// Caller-confirmed admission inputs: the received client items plus the facts
/// Generation Chain already confirmed (ADR-0040/0020 keep those caller-side).
/// Grouping evidence — tail window, canonical fingerprint, ingress receipt — is
/// computed inside this module and never trusted from callers.
#[derive(Debug, Clone)]
pub(crate) struct AdmissionFacts {
    /// The received canonical client request; never restored or effective
    /// history. Tail evidence reads `client_request.items`; the retry
    /// fingerprint covers the whole request per the documented contract.
    pub client_request: AiRequest,
    pub has_new_user: bool,
    pub has_matching_pending_tool_result: bool,
    pub generation_root_id: Option<String>,
    pub generation_parent_id: Option<String>,
}

/// One admission's resolved placement plus the diagnostic event to persist.
/// Carries no request or store types; the writer only persists it.
pub(super) struct Attribution {
    pub interaction_id: String,
    pub parent_run_id: Option<String>,
    pub parent_interaction_id: Option<String>,
    pub inferred_retry: bool,
    pub grouping_reason: &'static str,
    pub diagnostic_source_run_id: Option<String>,
    pub interrupt_parent: bool,
    pub diagnostic_event: Option<RunEvent>,
    /// Ingress receipt captured at admission; persisted as the run's start.
    pub ingress_received_at: i64,
    /// The persisted generation-parent lookup failed; the admission proceeds
    /// unparented and the writer records the observation gap.
    pub parent_evidence_error: Option<anyhow::Error>,
    /// The canonical request failed to serialize for fingerprinting; the
    /// writer marks the run's trace partial as `observation_gap`.
    pub fingerprint_gap: bool,
}

/// Materialized tail evidence for one requested source.
#[derive(Clone)]
pub(super) enum SourceWindow {
    Captured(Window),
    Unavailable,
    ResourceLimit,
}

/// Persisted attribution evidence. `ObservationEvidence` is backed by the
/// observation tables plus Generation Chain ancestor items; tests drive the
/// same production merge path through an in-memory adapter.
#[async_trait]
pub(super) trait AttributionEvidence {
    async fn pending_tool_sources(
        &self,
        principal: &str,
        tool_ids: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, String, String)>>;

    /// `(run_id, interaction_id, generation_node_id)` rows whose retained
    /// last-unit hash appears in the admitted window.
    async fn tail_sources_by_hashes(
        &self,
        principal: &str,
        excluding: &str,
        hashes: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, String, Option<String>)>>;

    /// Tail windows behind persisted sources, in exactly the requested order.
    /// Missing node ids are resolved from their source runs. Generation Chain
    /// visits shared ancestors once per materialized chain, while this seam
    /// retains only windows belonging to the requested candidates.
    async fn source_windows(
        &self,
        principal: &str,
        sources: &[(&str, Option<&str>)],
        preferred_head: Option<&str>,
    ) -> anyhow::Result<Vec<SourceWindow>>;

    async fn delivery_completed_at(&self, run_id: &str) -> anyhow::Result<Option<i64>>;

    async fn observed_generation_parent(
        &self,
        generation_node_id: &str,
        principal: &str,
    ) -> anyhow::Result<Option<ObservedParent>>;
}

/// Production evidence adapter. Ancestor items come from Generation Chain, which
/// owns the turn-chain schema; observation never touches those tables.
#[derive(Clone)]
pub(super) struct ObservationEvidence {
    store: ObservationStore,
    generations: crate::generation_chain::GenerationChain,
}

impl ObservationEvidence {
    pub(super) fn new(
        store: ObservationStore,
        generations: crate::generation_chain::GenerationChain,
    ) -> Self {
        Self { store, generations }
    }
}

#[async_trait]
impl AttributionEvidence for ObservationEvidence {
    async fn pending_tool_sources(
        &self,
        principal: &str,
        tool_ids: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        self.store
            .pending_tool_sources(principal, tool_ids, now)
            .await
    }

    async fn tail_sources_by_hashes(
        &self,
        principal: &str,
        excluding: &str,
        hashes: &[String],
        now: i64,
    ) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
        self.store
            .tail_sources_by_hashes(principal, excluding, hashes, now)
            .await
    }

    async fn source_windows(
        &self,
        principal: &str,
        sources: &[(&str, Option<&str>)],
        preferred_head: Option<&str>,
    ) -> anyhow::Result<Vec<SourceWindow>> {
        let mut resolved_nodes = Vec::with_capacity(sources.len());
        for (source_run_id, node) in sources {
            resolved_nodes.push(match node {
                Some(node) => Some(Cow::Borrowed(*node)),
                None => self
                    .store
                    .tail_generation_node(source_run_id)
                    .await?
                    .map(Cow::Owned),
            });
        }

        let mut windows: Vec<Option<SourceWindow>> = resolved_nodes
            .iter()
            .map(|node| node.is_none().then_some(SourceWindow::Unavailable))
            .collect();
        // Principal::new asserts an authenticated key identity; a principal that
        // cannot form one declines every candidate instead of panicking the writer.
        if principal.is_empty() || principal == "anonymous" {
            return Ok(windows
                .into_iter()
                .map(|window| window.unwrap_or(SourceWindow::Unavailable))
                .collect());
        }

        let mut candidate_positions: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut visit_order = Vec::new();
        for (index, node) in resolved_nodes.iter().enumerate() {
            let Some(node) = node.as_deref() else {
                continue;
            };
            candidate_positions
                .entry(node)
                .or_insert_with(|| {
                    visit_order.push(node);
                    Vec::new()
                })
                .push(index);
        }
        if let Some(preferred_head) = preferred_head
            && let Some(index) = visit_order.iter().position(|node| *node == preferred_head)
        {
            let preferred = visit_order.remove(index);
            visit_order.insert(0, preferred);
        }

        let principal = Principal::new(principal);
        for node in visit_order {
            let Some(positions) = candidate_positions.get(node) else {
                continue;
            };
            if positions.iter().all(|index| windows[*index].is_some()) {
                continue;
            }
            let available = self
                .generations
                .visit_ancestor_client_items(&principal, node, |visited_node, items| {
                    let Some(positions) = candidate_positions.get(visited_node) else {
                        return;
                    };
                    let Some((&last, duplicates)) = positions.split_last() else {
                        return;
                    };
                    if windows[last].is_some() {
                        return;
                    }
                    let captured = Window::capture(items)
                        .map(SourceWindow::Captured)
                        .unwrap_or(SourceWindow::ResourceLimit);
                    for index in duplicates {
                        windows[*index] = Some(captured.clone());
                    }
                    windows[last] = Some(captured);
                })
                .await?;
            if !available || positions.iter().any(|index| windows[*index].is_none()) {
                for index in positions {
                    windows[*index] = Some(SourceWindow::Unavailable);
                }
            }
        }

        Ok(windows
            .into_iter()
            .map(|window| window.unwrap_or(SourceWindow::Unavailable))
            .collect())
    }

    async fn delivery_completed_at(&self, run_id: &str) -> anyhow::Result<Option<i64>> {
        self.store.delivery_completed_at(run_id).await
    }

    async fn observed_generation_parent(
        &self,
        generation_node_id: &str,
        principal: &str,
    ) -> anyhow::Result<Option<ObservedParent>> {
        self.store
            .observed_generation_parent(generation_node_id, principal)
            .await
    }
}

/// The deep module: the only place tail evidence, persisted evidence, and the
/// ADR-0053 decision table meet. `TailIndex` and `GroupingIndex` are internal
/// kernels; callers interact only through `admit` and the lifecycle methods.
pub(super) struct RunAttribution<E> {
    evidence: E,
    tail: TailIndex,
    grouping: GroupingIndex,
}

impl<E: AttributionEvidence> RunAttribution<E> {
    pub(super) fn new(evidence: E) -> Self {
        Self {
            evidence,
            tail: TailIndex::default(),
            grouping: GroupingIndex::default(),
        }
    }

    /// Resolves one admission. `ingress_received_at` is the receipt captured at
    /// ingress; the module stamps it onto the attribution — callers ship no
    /// placeholder.
    pub(super) async fn admit(
        &mut self,
        start: &RunStart,
        facts: &AdmissionFacts,
        ingress_received_at: i64,
        now: i64,
    ) -> Attribution {
        let items = &facts.client_request.items;
        let (input, input_overflow) = match Window::capture(items) {
            Some(window) => (Some(window), false),
            None => (None, !items.is_empty()),
        };
        let (canonical_fingerprint, fingerprint_gap) =
            request_fingerprint(&facts.client_request, &start.id);
        let mut parent_evidence_error = None;
        let parent = match facts.generation_parent_id.as_deref() {
            Some(node) => match self
                .evidence
                .observed_generation_parent(node, &start.principal)
                .await
            {
                Ok(parent) => parent,
                Err(error) => {
                    parent_evidence_error = Some(error);
                    None
                }
            },
            None => None,
        };
        // The tail diagnostic also runs with a confirmed Generation parent: the
        // persisted event separates a model-switched continuation (input carries
        // the intermediate turn) from a real fork of the original chain.
        let (diagnostic, diagnostic_event) = self
            .discover(DiscoverInput {
                run_id: &start.id,
                principal: &start.principal,
                generation_parent_id: facts.generation_parent_id.as_deref(),
                input: input.as_ref(),
                input_overflow,
                ingress_received_at,
                now,
            })
            .await;
        let assignment = self.grouping.assign(
            AssignInput {
                run_id: &start.id,
                principal: &start.principal,
                canonical_fingerprint: &canonical_fingerprint,
                has_new_user: facts.has_new_user,
                has_matching_pending_tool_result: facts.has_matching_pending_tool_result,
                generation_parent_id: facts.generation_parent_id.as_deref(),
                ingress_received_at,
            },
            now,
            parent.as_ref(),
            diagnostic.as_ref(),
        );
        Attribution {
            interaction_id: assignment.interaction_id,
            parent_run_id: assignment.parent_run_id,
            parent_interaction_id: assignment.parent_interaction_id,
            inferred_retry: assignment.inferred_retry,
            grouping_reason: assignment.grouping_reason,
            diagnostic_source_run_id: assignment.diagnostic_source_run_id,
            interrupt_parent: assignment.interrupt_parent,
            diagnostic_event,
            ingress_received_at,
            parent_evidence_error,
            fingerprint_gap,
        }
    }

    /// A completed delivery contributes its input+output window as tail evidence.
    pub(super) fn insert_tail_source(
        &mut self,
        run_id: String,
        window: Window,
        expires_at: i64,
        principal: String,
        interaction_id: String,
    ) {
        self.tail
            .insert(run_id, window, expires_at, principal, interaction_id);
    }

    pub(super) fn sweep(&mut self, now: i64) {
        self.tail.sweep(now);
    }

    /// Tail evidence follows the retention setting; grouping assignments are
    /// dropped separately through `forget_interactions` after an actual purge.
    pub(super) fn clear_tail(&mut self) {
        self.tail = TailIndex::default();
    }

    pub(super) fn forget_interactions(&mut self, removed: &[String]) {
        self.grouping.forget_interactions(removed);
    }

    pub(super) fn interaction_for_run(&self, run_id: &str) -> Option<&str> {
        self.grouping.interaction_for_run(run_id)
    }

    pub(super) fn output_committed(&mut self, run_id: &str) {
        self.grouping.output_committed(run_id);
    }

    pub(super) fn finish(&mut self, run_id: &str, status: &str, now: i64) {
        self.grouping.finish(run_id, status, now);
    }

    /// Confirmed current-tool continuation: the request tail submits results for
    /// the source run's pending client tool calls. A unique source is required.
    async fn current_tool_source(
        &self,
        principal: &str,
        input: &Window,
        ingress_received_at: i64,
        now: i64,
    ) -> Option<DiagnosticSource> {
        let ids = input.current_tail_tool_ids()?;
        let rows = self
            .evidence
            .pending_tool_sources(principal, &ids, now)
            .await
            .ok()?;
        let mut source = None;
        for id in &ids {
            let mut matches: Vec<(String, String)> = self.tail.pending_runs(id, principal);
            for (tool_id, run, interaction) in &rows {
                if tool_id == id && !matches.iter().any(|(existing, _)| existing == run) {
                    matches.push((run.clone(), interaction.clone()));
                }
            }
            if matches.len() != 1 {
                return None;
            }
            match &source {
                None => source = Some(matches[0].clone()),
                Some(existing) if existing.0 != matches[0].0 => return None,
                Some(_) => {}
            }
        }
        let (run_id, interaction_id) = source?;
        let pending = if let Some(window) = self.tail.window(&run_id) {
            window.pending_tool_ids()?
        } else {
            let sources = [(run_id.as_str(), None)];
            let windows = self
                .evidence
                .source_windows(principal, &sources, None)
                .await
                .ok()?;
            match windows.into_iter().next()? {
                SourceWindow::Captured(window) => window.pending_tool_ids()?,
                SourceWindow::Unavailable | SourceWindow::ResourceLimit => return None,
            }
        };
        if !ids
            .iter()
            .all(|id| pending.iter().any(|pending_id| pending_id == id))
        {
            return None;
        }
        let delivery_completed_at = self.evidence.delivery_completed_at(&run_id).await.ok()??;
        if delivery_completed_at > ingress_received_at {
            return None;
        }
        Some(DiagnosticSource {
            run_id,
            interaction_id,
            delivery_completed_at: Some(delivery_completed_at),
            kind: DiagnosticKind::CurrentTool,
            user_after_match: false,
        })
    }

    async fn discover(
        &self,
        discovery: DiscoverInput<'_>,
    ) -> (Option<DiagnosticSource>, Option<RunEvent>) {
        let DiscoverInput {
            run_id,
            principal,
            generation_parent_id,
            input,
            input_overflow,
            ingress_received_at,
            now,
        } = discovery;
        if input_overflow {
            return (None, Some(tail_status_event("resource_limit")));
        }
        let Some(input) = input else {
            return (None, None);
        };
        // 历史编辑可能只保留较早的生成前缀；当前工具结果仍能确认实际续接来源。
        if let Some(source) = self
            .current_tool_source(principal, input, ingress_received_at, now)
            .await
        {
            return (Some(source), None);
        }
        let mut loaded = Vec::new();
        let mut unloaded = Vec::new();
        let mut seen = HashSet::new();
        for run in self.tail.fingerprint_runs(input, principal) {
            if !seen.insert(run.clone()) {
                continue;
            }
            let Some(window) = self.tail.window(&run) else {
                continue;
            };
            let Some(interaction) = self.tail.interaction(&run) else {
                continue;
            };
            loaded.push((run, interaction.to_owned(), window.clone()));
        }
        match self
            .evidence
            .tail_sources_by_hashes(principal, run_id, &input.unit_hash_hexes(), now)
            .await
        {
            Ok(rows) => {
                for (run, interaction, node) in rows {
                    if !seen.insert(run.clone()) {
                        continue;
                    }
                    if let Some(window) = self.tail.window(&run) {
                        loaded.push((run, interaction, window.clone()));
                    } else {
                        unloaded.push((run, interaction, node));
                    }
                }
            }
            Err(_) => {
                if generation_parent_id.is_some() {
                    return (None, None);
                }
                return (None, Some(tail_status_event("index_unavailable")));
            }
        }
        if loaded.len() + unloaded.len() > MAX_CANDIDATES {
            return (None, Some(tail_status_event("resource_limit")));
        }
        if !unloaded.is_empty() {
            let sources: Vec<_> = unloaded
                .iter()
                .map(|(run, _, node)| (run.as_str(), node.as_deref()))
                .collect();
            let windows = match self
                .evidence
                .source_windows(principal, &sources, generation_parent_id)
                .await
            {
                Ok(windows) if windows.len() == unloaded.len() => windows,
                _ => return (None, Some(tail_status_event("index_unavailable"))),
            };
            for ((run, interaction, _), window) in unloaded.into_iter().zip(windows) {
                match window {
                    SourceWindow::Captured(window) => loaded.push((run, interaction, window)),
                    SourceWindow::Unavailable => {
                        return (None, Some(tail_status_event("index_unavailable")));
                    }
                    SourceWindow::ResourceLimit => {
                        return (None, Some(tail_status_event("resource_limit")));
                    }
                }
            }
        }
        let refs: Vec<_> = loaded
            .iter()
            .map(|(run, interaction, window)| (run.clone(), interaction.clone(), window))
            .collect();
        let event = TailIndex::associate_loaded(Some(input), &refs);
        let diagnostic = if generation_parent_id.is_none() {
            match &event {
                RunEvent::RetainedTailAssociated {
                    status,
                    source_run_id: Some(source_run_id),
                    source_interaction_id: Some(source_interaction_id),
                    input_start: Some(start_idx),
                    matched_units,
                    ..
                } if status == "inferred" => {
                    let delivery_completed_at = self
                        .evidence
                        .delivery_completed_at(source_run_id)
                        .await
                        .ok()
                        .flatten();
                    Some(DiagnosticSource {
                        run_id: source_run_id.clone(),
                        interaction_id: source_interaction_id.clone(),
                        delivery_completed_at,
                        kind: DiagnosticKind::RetainedTail,
                        user_after_match: input.user_after_match(*start_idx, *matched_units),
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        (diagnostic, Some(event))
    }
}

/// Fingerprint over the canonical serialization of the received request — the
/// documented "same request" identity for failed-root retry merge. A request
/// that cannot serialize falls back to a unique value so it never merges.
/// Process-local only; never persisted or serialized.
fn request_fingerprint(request: &AiRequest, run_id: &str) -> (String, bool) {
    match serde_json::to_value(request).and_then(|mut canonical| {
        canonical.sort_all_objects();
        serde_json::to_vec(&canonical)
    }) {
        Ok(bytes) => (canonical::hash_hex(&canonical::hash_bytes(&bytes)), false),
        Err(_) => (format!("unavailable:{run_id}"), true),
    }
}

fn tail_status_event(status: &str) -> RunEvent {
    RunEvent::RetainedTailAssociated {
        source_run_id: None,
        source_interaction_id: None,
        status: status.into(),
        candidate_count: 0,
        matched_units: 0,
        matched_bytes: 0,
        input_start: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use stravia_runtime_contract::protocol::ir::{AiItem, MessageContent, Role};

    use super::*;

    /// `(principal, interaction_id, last_unit_hash, ancestor items)`; `None`
    /// items model a row whose history cannot be rematerialized.
    type TailSource = (String, String, String, Option<Vec<AiItem>>);

    /// In-memory evidence adapter: tests declare the persisted evidence and the
    /// production merge path reads it through the same seam the store uses.
    #[derive(Default)]
    struct MemoryEvidence {
        /// `(tool_id, run_id, interaction_id, principal)` pending-tool rows.
        pending_tools: Vec<(String, String, String, String)>,
        /// `run_id -> tail source`.
        tail_sources: HashMap<String, TailSource>,
        delivered: HashMap<String, i64>,
        /// `generation_node_id -> (principal, observed parent)`.
        parents: HashMap<String, (String, ObservedParent)>,
        fail_tail_sources: bool,
        fail_parent_lookup: bool,
    }

    #[async_trait]
    impl AttributionEvidence for MemoryEvidence {
        async fn pending_tool_sources(
            &self,
            principal: &str,
            tool_ids: &[String],
            _now: i64,
        ) -> anyhow::Result<Vec<(String, String, String)>> {
            Ok(self
                .pending_tools
                .iter()
                .filter(|(_, _, _, owner)| owner == principal)
                .filter(|(tool_id, _, _, _)| tool_ids.contains(tool_id))
                .map(|(tool_id, run, interaction, _)| {
                    (tool_id.clone(), run.clone(), interaction.clone())
                })
                .collect())
        }

        async fn tail_sources_by_hashes(
            &self,
            principal: &str,
            excluding: &str,
            hashes: &[String],
            _now: i64,
        ) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
            if self.fail_tail_sources {
                anyhow::bail!("tail source query failed");
            }
            if hashes.is_empty() {
                return Ok(Vec::new());
            }
            Ok(self
                .tail_sources
                .iter()
                .filter(|(run, (owner, _, hash, _))| {
                    owner == principal && run.as_str() != excluding && hashes.contains(hash)
                })
                .map(|(run, (_, interaction, _, _))| {
                    (run.clone(), interaction.clone(), Some(run.clone()))
                })
                .collect())
        }

        async fn source_windows(
            &self,
            principal: &str,
            sources: &[(&str, Option<&str>)],
            _preferred_head: Option<&str>,
        ) -> anyhow::Result<Vec<SourceWindow>> {
            Ok(sources
                .iter()
                .map(|(source_run_id, _)| {
                    let items = self
                        .tail_sources
                        .get(*source_run_id)
                        .filter(|(owner, _, _, _)| owner == principal)
                        .and_then(|(_, _, _, items)| items.as_deref());
                    match items {
                        Some(items) => Window::capture(items)
                            .map(SourceWindow::Captured)
                            .unwrap_or(SourceWindow::ResourceLimit),
                        None => SourceWindow::Unavailable,
                    }
                })
                .collect())
        }

        async fn delivery_completed_at(&self, run_id: &str) -> anyhow::Result<Option<i64>> {
            Ok(self.delivered.get(run_id).copied())
        }

        async fn observed_generation_parent(
            &self,
            generation_node_id: &str,
            principal: &str,
        ) -> anyhow::Result<Option<ObservedParent>> {
            if self.fail_parent_lookup {
                anyhow::bail!("parent lookup failed");
            }
            Ok(self
                .parents
                .get(generation_node_id)
                .filter(|(owner, _)| owner == principal)
                .map(|(_, parent)| parent.clone()))
        }
    }

    impl MemoryEvidence {
        /// Registers a persisted tail source whose retained hash derives from
        /// the window over `items`, exactly like the store's `persist_tail_source`.
        fn add_tail_source(
            &mut self,
            principal: &str,
            run_id: &str,
            interaction_id: &str,
            items: &[AiItem],
            ancestor_items: Option<Vec<AiItem>>,
        ) {
            let hash = Window::capture(items)
                .and_then(|window| window.last_hash_hex())
                .expect("persisted tail source requires a capturable window");
            self.tail_sources.insert(
                run_id.to_owned(),
                (
                    principal.to_owned(),
                    interaction_id.to_owned(),
                    hash,
                    ancestor_items,
                ),
            );
        }
    }

    fn user(text: &str) -> AiItem {
        AiItem {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }
    }

    fn long_user(tag: &str) -> AiItem {
        user(&format!("{tag} {}", "用户问题内容。".repeat(8)))
    }

    fn long_answer(tag: &str) -> AiItem {
        AiItem::output_text(format!("{tag} {}", "助手回答内容。".repeat(8)))
    }

    fn tool_call(id: &str) -> AiItem {
        AiItem::function_call(stravia_runtime_contract::protocol::ir::ToolCall {
            id: id.into(),
            name: "probe".into(),
            arguments: "{}".into(),
        })
    }

    fn tool_result(id: &str) -> AiItem {
        AiItem::function_call_output(id, serde_json::json!("result"))
    }

    fn start(id: &str) -> RunStart {
        RunStart {
            id: id.into(),
            principal: "owner".into(),
            api_key_id: None,
            api_key_name: None,
            route_id: "route".into(),
            model_display_name: None,
            ingress_protocol: "responses".into(),
        }
    }

    fn facts(items: Vec<AiItem>) -> AdmissionFacts {
        AdmissionFacts {
            client_request: AiRequest::new("model", items),
            has_new_user: false,
            has_matching_pending_tool_result: false,
            generation_root_id: None,
            generation_parent_id: None,
        }
    }

    fn observed_parent(interaction: &str, run: &str, delivered_at: Option<i64>) -> ObservedParent {
        ObservedParent {
            interaction_id: interaction.into(),
            run_id: run.into(),
            delivery_completed_at: delivered_at,
        }
    }

    /// Seeds the module's in-memory tail evidence the way a completed delivery
    /// does: `insert_tail_source` is the only writer entry point for it.
    fn completed_tail_source(
        attribution: &mut RunAttribution<MemoryEvidence>,
        run_id: &str,
        interaction_id: &str,
        principal: &str,
        items: &[AiItem],
    ) {
        attribution.insert_tail_source(
            run_id.to_owned(),
            Window::capture(items).expect("tail source window"),
            i64::MAX,
            principal.to_owned(),
            interaction_id.to_owned(),
        );
    }

    #[tokio::test]
    async fn no_evidence_admits_a_new_root_and_stamps_receipt() {
        let mut attribution = RunAttribution::new(MemoryEvidence::default());
        let assigned = attribution
            .admit(&start("run"), &facts(vec![user("hi")]), 1_000, 1_000)
            .await;
        assert_eq!(assigned.grouping_reason, "new_root");
        assert_eq!(assigned.parent_run_id, None);
        assert_eq!(assigned.parent_interaction_id, None);
        assert_eq!(assigned.ingress_received_at, 1_000);
        assert!(matches!(
            assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated { ref status, .. }) if status == "no_match"
        ));
    }

    #[tokio::test]
    async fn confirmed_parent_without_new_user_continues_exactly() {
        let mut evidence = MemoryEvidence::default();
        evidence.parents.insert(
            "node".into(),
            (
                "owner".into(),
                observed_parent("interaction", "parent-run", Some(100)),
            ),
        );
        let mut attribution = RunAttribution::new(evidence);
        let mut facts = facts(vec![user("follow up")]);
        facts.generation_parent_id = Some("node".into());
        let assigned = attribution.admit(&start("run"), &facts, 5_000, 5_000).await;
        assert_eq!(assigned.interaction_id, "interaction");
        assert_eq!(assigned.parent_run_id.as_deref(), Some("parent-run"));
        assert_eq!(assigned.grouping_reason, "exact_continuation");
        assert!(!assigned.interrupt_parent);
    }

    #[tokio::test]
    async fn new_user_with_matching_pending_tool_result_continues() {
        let mut evidence = MemoryEvidence::default();
        evidence.parents.insert(
            "node".into(),
            (
                "owner".into(),
                observed_parent("interaction", "parent-run", Some(100)),
            ),
        );
        let mut attribution = RunAttribution::new(evidence);
        let mut facts = facts(vec![user("follow up"), tool_result("call-1")]);
        facts.generation_parent_id = Some("node".into());
        facts.has_new_user = true;
        facts.has_matching_pending_tool_result = true;
        let assigned = attribution
            .admit(&start("run"), &facts, 100_000, 100_000)
            .await;
        assert_eq!(assigned.interaction_id, "interaction");
        assert_eq!(assigned.grouping_reason, "pending_tool_result");
        assert!(!assigned.interrupt_parent);
    }

    #[tokio::test]
    async fn rapid_new_user_within_two_seconds_still_continues() {
        for delay in [0_i64, 2_000] {
            let mut evidence = MemoryEvidence::default();
            evidence.parents.insert(
                "node".into(),
                (
                    "owner".into(),
                    observed_parent("interaction", "parent-run", Some(10_000)),
                ),
            );
            let mut attribution = RunAttribution::new(evidence);
            let mut facts = facts(vec![user("again")]);
            facts.generation_parent_id = Some("node".into());
            facts.has_new_user = true;
            let received_at = 10_000 + delay;
            let assigned = attribution
                .admit(&start("run"), &facts, received_at, received_at)
                .await;
            assert_eq!(
                assigned.grouping_reason, "rapid_exact_continuation",
                "{delay}"
            );
            assert_eq!(assigned.interaction_id, "interaction");
        }
    }

    #[tokio::test]
    async fn new_user_after_delivery_window_creates_interrupted_child() {
        for (delay, delivered) in [(2_001_i64, Some(10_000_i64)), (1, None), (-1, Some(10_000))] {
            let mut evidence = MemoryEvidence::default();
            evidence.parents.insert(
                "node".into(),
                (
                    "owner".into(),
                    observed_parent("interaction", "parent-run", delivered),
                ),
            );
            let mut attribution = RunAttribution::new(evidence);
            let mut facts = facts(vec![user("new topic")]);
            facts.generation_parent_id = Some("node".into());
            facts.has_new_user = true;
            let received_at = 10_000 + delay;
            let assigned = attribution
                .admit(&start("run"), &facts, received_at, received_at)
                .await;
            assert_eq!(
                assigned.grouping_reason, "new_user",
                "{delay} {delivered:?}"
            );
            assert_ne!(assigned.interaction_id, "interaction");
            assert_eq!(
                assigned.parent_interaction_id.as_deref(),
                Some("interaction")
            );
            assert!(assigned.interrupt_parent);
        }
    }

    #[tokio::test]
    async fn unmatched_generation_parent_without_tool_evidence_stays_unparented() {
        let mut attribution = RunAttribution::new(MemoryEvidence::default());
        let mut facts = facts(vec![user("hi")]);
        facts.generation_parent_id = Some("unobserved".into());
        let assigned = attribution.admit(&start("run"), &facts, 1, 1).await;
        assert_eq!(assigned.grouping_reason, "unmatched_parent");
        assert_eq!(assigned.parent_run_id, None);
        assert_eq!(assigned.parent_interaction_id, None);
    }

    #[tokio::test]
    async fn failed_parent_lookup_still_admits_unparented() {
        let evidence = MemoryEvidence {
            fail_parent_lookup: true,
            ..Default::default()
        };
        let mut attribution = RunAttribution::new(evidence);
        let mut facts = facts(vec![user("hi")]);
        facts.generation_parent_id = Some("node".into());
        let assigned = attribution.admit(&start("run"), &facts, 1, 1).await;
        assert_eq!(assigned.grouping_reason, "unmatched_parent");
        assert!(assigned.parent_evidence_error.is_some());
    }

    #[tokio::test]
    async fn current_tool_continuation_merges_onto_the_pending_source() {
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("source-run".into(), 1_000);
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "source-run",
            "source-interaction",
            "owner",
            &[long_user("task"), tool_call("call-1")],
        );
        let assigned = attribution
            .admit(
                &start("run"),
                &facts(vec![long_user("task"), tool_result("call-1")]),
                5_000,
                5_000,
            )
            .await;
        assert_eq!(assigned.interaction_id, "source-interaction");
        assert_eq!(assigned.grouping_reason, "current_tool_continuation");
        assert_eq!(
            assigned.diagnostic_source_run_id.as_deref(),
            Some("source-run")
        );
        // Current Tool Continuation wins before Retained Tail emits an event.
        assert!(assigned.diagnostic_event.is_none());
    }

    #[tokio::test]
    async fn current_tool_continuation_uses_persisted_window() {
        let source_items = vec![long_user("task"), tool_call("call-1")];
        let mut evidence = MemoryEvidence::default();
        evidence.pending_tools.push((
            "call-1".into(),
            "source-run".into(),
            "source-interaction".into(),
            "owner".into(),
        ));
        evidence.delivered.insert("source-run".into(), 1_000);
        evidence.add_tail_source(
            "owner",
            "source-run",
            "source-interaction",
            &source_items,
            Some(source_items.clone()),
        );
        let mut attribution = RunAttribution::new(evidence);
        let mut request = facts(vec![long_user("task"), tool_result("call-1")]);
        request.generation_parent_id = Some("unobserved-node".into());
        let assigned = attribution
            .admit(&start("run"), &request, 5_000, 5_000)
            .await;
        assert_eq!(assigned.interaction_id, "source-interaction");
        assert_eq!(assigned.parent_run_id.as_deref(), Some("source-run"));
        assert_eq!(assigned.grouping_reason, "current_tool_continuation");
        assert!(assigned.diagnostic_event.is_none());
    }

    #[tokio::test]
    async fn current_tool_continuation_advances_beyond_stale_generation_parent() {
        let mut evidence = MemoryEvidence::default();
        evidence.parents.insert(
            "early-node".into(),
            (
                "owner".into(),
                observed_parent("early-interaction", "early-run", Some(100)),
            ),
        );
        let mut attribution = RunAttribution::new(evidence);
        let mut source_run = "recent-run".to_owned();
        let mut source_items = vec![
            user("task"),
            tool_call("early-call"),
            tool_result("early-call"),
            user("keep working"),
        ];
        for step in 0..3 {
            let call_id = format!("current-call-{step}");
            source_items.push(tool_call(&call_id));
            attribution.evidence.pending_tools.push((
                call_id.clone(),
                source_run.clone(),
                "task-interaction".into(),
                "owner".into(),
            ));
            let delivered_at = 1_000 + step * 400_000;
            attribution
                .evidence
                .delivered
                .insert(source_run.clone(), delivered_at);
            attribution.evidence.add_tail_source(
                "owner",
                &source_run,
                "task-interaction",
                &source_items,
                Some(source_items.clone()),
            );
            source_items.push(tool_result(&call_id));
            source_items.push(user("additional task constraint"));
            let mut request = facts(source_items.clone());
            request.generation_parent_id = Some("early-node".into());
            request.has_new_user = true;
            let run_id = format!("continued-run-{step}");
            let received_at = delivered_at + 400_000;
            let assigned = attribution
                .admit(&start(&run_id), &request, received_at, received_at)
                .await;

            assert_eq!(assigned.interaction_id, "task-interaction");
            assert_eq!(assigned.parent_run_id.as_deref(), Some(source_run.as_str()));
            assert_eq!(assigned.parent_interaction_id, None);
            assert!(!assigned.interrupt_parent);
            assert_eq!(assigned.grouping_reason, "current_tool_continuation");
            assert_eq!(
                assigned.diagnostic_source_run_id.as_deref(),
                Some(source_run.as_str())
            );
            source_run = run_id;
        }
    }

    #[tokio::test]
    async fn historical_tool_result_does_not_override_generation_parent() {
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("source-run".into(), 1_000);
        evidence.parents.insert(
            "node".into(),
            (
                "owner".into(),
                observed_parent("interaction", "source-run", Some(1_000)),
            ),
        );
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "source-run",
            "interaction",
            "owner",
            &[user("original task"), tool_call("old-call")],
        );
        let mut request = facts(vec![
            user("original task"),
            tool_call("old-call"),
            tool_result("old-call"),
            AiItem::output_text("finished"),
            user("a new task"),
        ]);
        request.generation_parent_id = Some("node".into());
        request.has_new_user = true;
        let assigned = attribution
            .admit(&start("new-run"), &request, 10_000, 10_000)
            .await;
        assert_ne!(assigned.interaction_id, "interaction");
        assert_eq!(
            assigned.parent_interaction_id.as_deref(),
            Some("interaction")
        );
        assert_eq!(assigned.grouping_reason, "new_user");
        assert!(assigned.interrupt_parent);
    }

    #[tokio::test]
    async fn current_tool_wins_over_a_retained_tail_match() {
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("tool-run".into(), 1_000);
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "tool-run",
            "tool-interaction",
            "owner",
            &[long_user("tool task"), tool_call("call-1")],
        );
        completed_tail_source(
            &mut attribution,
            "tail-run",
            "tail-interaction",
            "owner",
            &[long_user("kept"), long_answer("kept")],
        );
        // The input matches the retained tail of `tail-run` AND submits a result
        // for `tool-run`'s pending call; Current Tool precedence must win.
        let assigned = attribution
            .admit(
                &start("run"),
                &facts(vec![
                    long_user("kept"),
                    long_answer("kept"),
                    tool_result("call-1"),
                ]),
                5_000,
                5_000,
            )
            .await;
        assert_eq!(assigned.interaction_id, "tool-interaction");
        assert_eq!(assigned.grouping_reason, "current_tool_continuation");
        assert!(assigned.diagnostic_event.is_none());
    }

    #[tokio::test]
    async fn current_tool_requires_unique_source_and_prior_delivery() {
        let mut evidence = MemoryEvidence::default();
        evidence.parents.insert(
            "node".into(),
            (
                "owner".into(),
                observed_parent("parent-interaction", "parent-run", Some(1_000)),
            ),
        );
        // Two persisted rows claim the same pending tool id: not unique.
        for run in ["run-a", "run-b"] {
            evidence.pending_tools.push((
                "call-1".into(),
                run.into(),
                format!("{run}-interaction"),
                "owner".into(),
            ));
            evidence.delivered.insert(run.into(), 1_000);
        }
        let mut attribution = RunAttribution::new(evidence);
        let input = vec![long_user("task"), tool_result("call-1")];
        let mut request = facts(input);
        request.generation_parent_id = Some("node".into());
        request.has_new_user = true;
        let assigned = attribution
            .admit(&start("run"), &request, 5_000, 5_000)
            .await;
        assert_eq!(assigned.grouping_reason, "new_user");
        assert_eq!(
            assigned.parent_interaction_id.as_deref(),
            Some("parent-interaction")
        );

        // A unique source whose delivery postdates ingress declines too.
        let mut evidence = MemoryEvidence::default();
        evidence.parents.insert(
            "node".into(),
            (
                "owner".into(),
                observed_parent("parent-interaction", "parent-run", Some(1_000)),
            ),
        );
        evidence.delivered.insert("source-run".into(), 9_000);
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "source-run",
            "source-interaction",
            "owner",
            &[long_user("task"), tool_call("call-1")],
        );
        let assigned = attribution
            .admit(&start("run"), &request, 5_000, 5_000)
            .await;
        assert_ne!(assigned.interaction_id, "source-interaction");
        assert_eq!(assigned.grouping_reason, "new_user");
        assert_eq!(
            assigned.parent_interaction_id.as_deref(),
            Some("parent-interaction")
        );
    }

    #[tokio::test]
    async fn retained_tail_merges_within_window_and_links_beyond() {
        let source_items = vec![long_user("task"), long_answer("done")];
        for (elapsed, merged, reason) in [
            (0_i64, true, "retained_tail_continuation"),
            (300_000, true, "retained_tail_continuation"),
            (300_001, false, "retained_tail_linked"),
        ] {
            let mut evidence = MemoryEvidence::default();
            evidence.delivered.insert("source-run".into(), 10_000);
            let mut attribution = RunAttribution::new(evidence);
            completed_tail_source(
                &mut attribution,
                "source-run",
                "source-interaction",
                "owner",
                &source_items,
            );
            let received_at = 10_000 + elapsed;
            let assigned = attribution
                .admit(
                    &start("run"),
                    &facts(source_items.clone()),
                    received_at,
                    received_at,
                )
                .await;
            assert_eq!(assigned.grouping_reason, reason, "{elapsed}");
            assert_eq!(assigned.interaction_id == "source-interaction", merged);
            assert_eq!(assigned.parent_run_id.as_deref(), Some("source-run"));
            assert_eq!(
                assigned.parent_interaction_id.is_some(),
                !merged,
                "{elapsed}"
            );
            assert!(matches!(
                assigned.diagnostic_event,
                Some(RunEvent::RetainedTailAssociated { ref status, .. }) if status == "inferred"
            ));
        }
    }

    #[tokio::test]
    async fn retained_tail_uses_persisted_evidence_beyond_memory() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("source-run".into(), 10_000);
        evidence.add_tail_source(
            "owner",
            "source-run",
            "source-interaction",
            &source_items,
            Some(source_items.clone()),
        );
        let mut attribution = RunAttribution::new(evidence);
        let assigned = attribution
            .admit(&start("run"), &facts(source_items), 20_000, 20_000)
            .await;
        assert_eq!(assigned.interaction_id, "source-interaction");
        assert_eq!(assigned.grouping_reason, "retained_tail_continuation");
    }

    #[tokio::test]
    async fn shared_ancestor_candidates_select_the_stronger_source() {
        let shared = vec![long_user("task"), long_answer("first")];
        let mut head = shared.clone();
        head.extend([long_user("follow up"), long_answer("final")]);
        let mut evidence = MemoryEvidence::default();
        evidence.add_tail_source(
            "owner",
            "ancestor-run",
            "ancestor-interaction",
            &shared,
            Some(shared.clone()),
        );
        evidence.add_tail_source(
            "owner",
            "head-run",
            "head-interaction",
            &head,
            Some(head.clone()),
        );
        evidence.parents.insert(
            "head-run".into(),
            (
                "owner".into(),
                observed_parent("confirmed-interaction", "confirmed-run", Some(100)),
            ),
        );
        let mut attribution = RunAttribution::new(evidence);
        let mut admitted = facts(head);
        admitted.generation_parent_id = Some("head-run".into());
        let assigned = attribution
            .admit(&start("run"), &admitted, 20_000, 20_000)
            .await;
        assert_eq!(assigned.interaction_id, "confirmed-interaction");
        assert!(matches!(
            &assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated {
                status,
                source_run_id: Some(run),
                candidate_count: 1,
                ..
            }) if status == "inferred" && run == "head-run"
        ));
    }

    #[tokio::test]
    async fn tied_persisted_sources_remain_ambiguous() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        for (run, interaction) in [("source-a", "interaction-a"), ("source-b", "interaction-b")] {
            evidence.add_tail_source(
                "owner",
                run,
                interaction,
                &source_items,
                Some(source_items.clone()),
            );
        }
        let mut attribution = RunAttribution::new(evidence);
        let assigned = attribution
            .admit(&start("run"), &facts(source_items), 20_000, 20_000)
            .await;
        assert_eq!(assigned.grouping_reason, "new_root");
        assert!(matches!(
            &assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated {
                status,
                source_run_id: None,
                candidate_count: 2,
                ..
            }) if status == "ambiguous"
        ));
    }

    #[tokio::test]
    async fn new_user_after_the_match_links_and_interrupts() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("source-run".into(), 10_000);
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "source-run",
            "source-interaction",
            "owner",
            &source_items,
        );
        let mut facts = facts(vec![
            long_user("task"),
            long_answer("done"),
            long_user("new direction"),
        ]);
        facts.has_new_user = true;
        let assigned = attribution
            .admit(&start("run"), &facts, 20_000, 20_000)
            .await;
        assert_eq!(assigned.grouping_reason, "retained_tail_linked");
        assert_ne!(assigned.interaction_id, "source-interaction");
        assert_eq!(
            assigned.parent_interaction_id.as_deref(),
            Some("source-interaction")
        );
        assert!(assigned.interrupt_parent);
        assert_eq!(
            assigned.diagnostic_source_run_id.as_deref(),
            Some("source-run")
        );
    }

    #[tokio::test]
    async fn confirmed_parent_records_tail_evidence_without_using_it() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("source-run".into(), 10_000);
        evidence.parents.insert(
            "node".into(),
            (
                "owner".into(),
                observed_parent("parent-interaction", "parent-run", Some(100)),
            ),
        );
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "source-run",
            "source-interaction",
            "owner",
            &source_items,
        );
        let mut facts = facts(source_items);
        facts.generation_parent_id = Some("node".into());
        let assigned = attribution
            .admit(&start("run"), &facts, 20_000, 20_000)
            .await;
        // The confirmed parent decides; the association is persisted as a
        // diagnostic event only, never as a grouping input.
        assert_eq!(assigned.interaction_id, "parent-interaction");
        assert_eq!(assigned.grouping_reason, "exact_continuation");
        assert_eq!(assigned.diagnostic_source_run_id, None);
        assert!(matches!(
            assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated {
                ref status,
                source_run_id: Some(ref run),
                ..
            }) if status == "inferred" && run == "source-run"
        ));
    }

    #[tokio::test]
    async fn failed_tail_query_marks_index_unavailable_unless_parented() {
        for parented in [false, true] {
            let mut evidence = MemoryEvidence {
                fail_tail_sources: true,
                ..Default::default()
            };
            if parented {
                evidence.parents.insert(
                    "node".into(),
                    (
                        "owner".into(),
                        observed_parent("interaction", "parent-run", None),
                    ),
                );
            }
            let mut attribution = RunAttribution::new(evidence);
            let mut facts = facts(vec![user("hi")]);
            if parented {
                facts.generation_parent_id = Some("node".into());
            }
            let assigned = attribution.admit(&start("run"), &facts, 1, 1).await;
            if parented {
                assert!(assigned.diagnostic_event.is_none());
                assert_eq!(assigned.interaction_id, "interaction");
            } else {
                assert!(matches!(
                    assigned.diagnostic_event,
                    Some(RunEvent::RetainedTailAssociated { ref status, .. })
                        if status == "index_unavailable"
                ));
            }
        }
    }

    #[tokio::test]
    async fn unrematerializable_source_marks_index_unavailable() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        // Row matches by hash but carries no materializable history.
        evidence.add_tail_source(
            "owner",
            "source-run",
            "source-interaction",
            &source_items,
            None,
        );
        let mut attribution = RunAttribution::new(evidence);
        let assigned = attribution
            .admit(&start("run"), &facts(source_items), 20_000, 20_000)
            .await;
        assert!(matches!(
            assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated { ref status, .. })
                if status == "index_unavailable"
        ));
        assert_eq!(assigned.grouping_reason, "new_root");
    }

    #[tokio::test]
    async fn oversized_persisted_evidence_is_a_resource_limit() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        evidence.add_tail_source(
            "owner",
            "source-run",
            "source-interaction",
            &source_items,
            Some(vec![user(&"a".repeat(600 * 1024))]),
        );
        let mut attribution = RunAttribution::new(evidence);
        let assigned = attribution
            .admit(&start("run"), &facts(source_items), 20_000, 20_000)
            .await;
        assert!(matches!(
            &assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated { status, .. })
                if status == "resource_limit"
        ));
        assert_eq!(assigned.grouping_reason, "new_root");
    }

    #[tokio::test]
    async fn candidate_overflow_is_a_resource_limit() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        for index in 0..(MAX_CANDIDATES + 1) {
            let run = format!("source-{index}");
            evidence.add_tail_source(
                "owner",
                &run,
                &format!("interaction-{index}"),
                &source_items,
                Some(source_items.clone()),
            );
        }
        let mut attribution = RunAttribution::new(evidence);
        let assigned = attribution
            .admit(&start("run"), &facts(source_items), 20_000, 20_000)
            .await;
        assert!(matches!(
            assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated { ref status, .. })
                if status == "resource_limit"
        ));
        assert_eq!(assigned.grouping_reason, "new_root");
    }

    #[tokio::test]
    async fn oversized_input_window_is_a_resource_limit() {
        let mut attribution = RunAttribution::new(MemoryEvidence::default());
        let oversized = user(&"a".repeat(600 * 1024));
        let assigned = attribution
            .admit(&start("run"), &facts(vec![oversized]), 1, 1)
            .await;
        assert!(matches!(
            assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated { ref status, .. })
                if status == "resource_limit"
        ));
    }

    #[tokio::test]
    async fn evidence_from_other_principals_never_matches() {
        let source_items = vec![long_user("task"), long_answer("done")];
        let mut evidence = MemoryEvidence::default();
        evidence.delivered.insert("source-run".into(), 10_000);
        evidence.add_tail_source(
            "other",
            "source-run",
            "source-interaction",
            &source_items,
            Some(source_items.clone()),
        );
        let mut attribution = RunAttribution::new(evidence);
        completed_tail_source(
            &mut attribution,
            "memory-run",
            "memory-interaction",
            "other",
            &source_items,
        );
        let assigned = attribution
            .admit(&start("run"), &facts(source_items), 20_000, 20_000)
            .await;
        assert_eq!(assigned.grouping_reason, "new_root");
        assert!(matches!(
            assigned.diagnostic_event,
            Some(RunEvent::RetainedTailAssociated { ref status, .. }) if status == "no_match"
        ));
    }

    #[tokio::test]
    async fn identical_failed_input_retries_within_window() {
        let items = vec![user("same request")];
        let mut attribution = RunAttribution::new(MemoryEvidence::default());
        let first = attribution
            .admit(&start("first"), &facts(items.clone()), 1_000, 1_000)
            .await;
        attribution.finish("first", "failed", 2_000);
        let retry = attribution
            .admit(&start("retry"), &facts(items), 3_000, 3_000)
            .await;
        assert_eq!(retry.interaction_id, first.interaction_id);
        assert_eq!(retry.grouping_reason, "inferred_retry");
        assert!(retry.inferred_retry);
    }

    #[tokio::test]
    async fn committed_output_blocks_retry_inference() {
        let items = vec![user("same request")];
        let mut attribution = RunAttribution::new(MemoryEvidence::default());
        let first = attribution
            .admit(&start("first"), &facts(items.clone()), 1_000, 1_000)
            .await;
        attribution.output_committed("first");
        attribution.finish("first", "failed", 2_000);
        let retry = attribution
            .admit(&start("retry"), &facts(items), 3_000, 3_000)
            .await;
        assert_ne!(retry.interaction_id, first.interaction_id);
        assert!(!retry.inferred_retry);
    }
}
