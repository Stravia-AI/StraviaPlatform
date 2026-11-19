use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::{
    AiRequest, AiResponse, ContentBlock, MessageContent, Role,
};
use stravia_vendor_sdk::{
    PluginError, ProviderSnapshot,
    state::{self, Cell, Error, Scope},
};

const KEY: &str = "antigravity-lifecycle-v1";

#[derive(Clone, Serialize, Deserialize)]
struct PendingTool {
    id: String,
    prior_results: usize,
    history_results: usize,
    generation: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Generation {
    id: String,
    host_index: u64,
    ordinal: Option<u64>,
    input_step: Option<u64>,
    finished: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct State {
    version: u8,
    trajectory_id: uuid::Uuid,
    cascade_id: String,
    window_id: String,
    logical_turn_id: String,
    last_execution_id: Option<String>,
    generations: Option<u64>,
    step: Option<u64>,
    used_claude: bool,
    used_claude_conservative: bool,
    used_non_gemini_model: bool,
    current: Option<Generation>,
    pending_tools: Vec<PendingTool>,
}

pub(crate) struct Input {
    cascade: String,
    window: String,
    logical: String,
    turn: String,
    fresh: bool,
    host_index: u64,
    mixed_providers: bool,
    result_counts: std::collections::BTreeMap<String, usize>,
    claude: bool,
    api_known: bool,
    google: bool,
}

pub(crate) struct Labels {
    pub request_id: Option<String>,
    pub cascade_id: String,
    pub trajectory_id: uuid::Uuid,
    pub last_step_index: Option<u64>,
    pub last_execution_id: Option<String>,
    pub used_claude: bool,
    pub used_claude_conservative: bool,
    pub used_non_gemini_model: bool,
}

impl Input {
    pub fn new(
        provider: &ProviderSnapshot,
        request: &AiRequest,
        api: Option<&Value>,
        model: Option<&Value>,
    ) -> Result<Self, PluginError> {
        let meta = &provider.operation_metadata;
        let required = |name: &str| {
            meta.get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    crate::wire::invalid("Antigravity requires trusted lifecycle metadata")
                })
        };
        let cascade = required("client_conversation_id")?;
        let window = required("context_window_id")?;
        let logical = required("logical_turn_id")?;
        let turn = required("model_turn_id")?;
        let host_index = meta
            .get("model_turn_index")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                crate::wire::invalid("Antigravity requires trusted lifecycle metadata")
            })?;
        let mixed_providers = meta
            .get("model_turn_mixed_providers")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                crate::wire::invalid("Antigravity requires trusted lifecycle metadata")
            })?;
        let window_index = meta.get("context_window_index").and_then(Value::as_u64);
        let prompt_index = meta.get("user_prompt_index").and_then(Value::as_u64);
        let fresh = host_index == 0
            && prompt_index == Some(1)
            && window_index == Some(0)
            && request.items.iter().any(|item| item.role == Role::User)
            && request.items.iter().all(|item| {
                !matches!(item.role, Role::Assistant | Role::Tool)
                    && item.tool_call_id.is_none()
                    && item.tool_calls.as_ref().is_none_or(Vec::is_empty)
                    && match &item.content {
                        MessageContent::Blocks(blocks) => !blocks.iter().any(|b| {
                            matches!(
                                b,
                                ContentBlock::ToolUse { .. }
                                    | ContentBlock::ToolResult { .. }
                                    | ContentBlock::ServerToolUse { .. }
                                    | ContentBlock::ServerToolResult { .. }
                            )
                        }),
                        _ => true,
                    }
            });
        let mut result_counts = std::collections::BTreeMap::new();
        for item in &request.items {
            if let Some(id) = &item.tool_call_id {
                *result_counts.entry(id.to_string()).or_insert(0) += 1;
            }
            if let MessageContent::Blocks(blocks) = &item.content {
                for block in blocks {
                    if let ContentBlock::ToolResult { tool_use_id, .. }
                    | ContentBlock::ServerToolResult { tool_use_id, .. } = block
                    {
                        if item.tool_call_id.as_ref() != Some(tool_use_id) {
                            *result_counts.entry(tool_use_id.to_string()).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        Ok(Self {
            cascade,
            window,
            logical,
            turn,
            fresh,
            host_index,
            mixed_providers,
            result_counts,
            api_known: api.is_some_and(|v| v.as_i64().is_some() || v.as_str().is_some()),
            claude: api.is_some_and(|v| {
                v.as_i64() == Some(26) || v.as_str() == Some("API_PROVIDER_ANTHROPIC_VERTEX")
            }),
            google: model.is_some_and(|v| {
                v.as_i64() == Some(4) || v.as_str() == Some("MODEL_PROVIDER_GOOGLE")
            }),
        })
    }
}

impl State {
    fn new(input: &Input, trajectory_id: uuid::Uuid) -> Self {
        let fresh = input.fresh && input.host_index == 0 && !input.mixed_providers;
        Self {
            version: 1,
            trajectory_id,
            cascade_id: input.cascade.clone(),
            window_id: input.window.clone(),
            logical_turn_id: input.logical.clone(),
            last_execution_id: None,
            generations: fresh.then_some(0),
            step: fresh.then_some(0),
            used_claude: false,
            used_claude_conservative: !fresh,
            used_non_gemini_model: !fresh,
            current: None,
            pending_tools: Vec::new(),
        }
    }
    fn begin(&mut self, input: &Input) -> Result<Labels, PluginError> {
        if self.version != 1 || self.cascade_id != input.cascade {
            return Err(crate::wire::invalid("invalid Antigravity lifecycle state"));
        }
        let retry = self.current.as_ref().is_some_and(|g| g.id == input.turn);
        if retry
            && (self.logical_turn_id != input.logical
                || self.window_id != input.window
                || self
                    .current
                    .as_ref()
                    .is_some_and(|g| g.host_index != input.host_index))
        {
            return Err(crate::wire::invalid(
                "inconsistent Antigravity model turn identity",
            ));
        }
        if !retry
            && self
                .current
                .as_ref()
                .is_some_and(|g| input.host_index <= g.host_index)
        {
            return Err(stravia_vendor_common::common::plugin_error(
                stravia_vendor_sdk::ErrorKind::Cancelled,
                "Antigravity model turn was superseded",
            ));
        }
        if input.mixed_providers {
            self.generations = None;
            self.step = None;
            self.pending_tools.clear();
            if let Some(generation) = &mut self.current {
                generation.ordinal = None;
                generation.input_step = None;
            }
        }
        if !retry {
            let gap = self
                .current
                .as_ref()
                .is_some_and(|g| g.host_index.checked_add(1) != Some(input.host_index));
            if gap {
                self.generations = None;
                self.step = None;
                self.used_claude_conservative = true;
                self.used_non_gemini_model = true;
            }
            if self.current.as_ref().is_some_and(|g| !g.finished) {
                self.step = None;
            }
            if self.window_id != input.window {
                self.window_id = input.window.clone();
                self.step = None;
            }
            if self.logical_turn_id != input.logical {
                let previous = std::mem::replace(&mut self.logical_turn_id, input.logical.clone());
                self.last_execution_id = if gap { None } else { Some(previous) };
                self.step = self.step.and_then(|n| n.checked_add(1));
            }
            // Each outstanding emitted call can consume at most one new matching result.
            for pending in &mut self.pending_tools {
                if self.step.is_none() {
                    break;
                }
                let count = input.result_counts.get(&pending.id).copied().unwrap_or(0);
                if count < pending.history_results {
                    self.step = None;
                } else if count > pending.prior_results {
                    self.step = self.step.and_then(|n| n.checked_add(1));
                    pending.prior_results = usize::MAX;
                }
            }
            if self.step.is_none() {
                self.pending_tools.clear();
            } else {
                self.pending_tools.retain(|p| p.prior_results != usize::MAX);
            }
            let ordinal = self.generations;
            self.generations = self.generations.and_then(|n| n.checked_add(1));
            self.current = Some(Generation {
                id: input.turn.clone(),
                host_index: input.host_index,
                ordinal,
                input_step: self.step,
                finished: false,
            });
        }
        if retry {
            self.current.as_mut().expect("generation exists").finished = false;
        }
        self.used_claude |= input.claude;
        self.used_claude_conservative |= input.claude || !input.api_known || input.mixed_providers;
        self.used_non_gemini_model |= !input.google || input.mixed_providers;
        let generation = self.current.as_ref().expect("generation exists");
        Ok(Labels {
            request_id: generation
                .ordinal
                .map(|n| format!("{}-{n}", self.trajectory_id)),
            cascade_id: self.cascade_id.clone(),
            trajectory_id: self.trajectory_id,
            last_step_index: generation.input_step,
            last_execution_id: self.last_execution_id.clone(),
            used_claude: self.used_claude,
            used_claude_conservative: self.used_claude_conservative,
            used_non_gemini_model: self.used_non_gemini_model,
        })
    }
    fn finish(
        &mut self,
        turn: &str,
        result_counts: &std::collections::BTreeMap<String, usize>,
        response: Option<&AiResponse>,
    ) -> Result<(), PluginError> {
        let generation = self
            .current
            .as_mut()
            .filter(|g| g.id == turn)
            .ok_or_else(|| {
                crate::wire::invalid("Antigravity lifecycle generation changed during inference")
            })?;
        self.step = generation.input_step.and_then(|n| n.checked_add(1));
        generation.finished = true;
        self.pending_tools.retain(|p| p.generation != turn);
        if self.step.is_some() {
            if let Some(response) = response {
                // Keep occurrence offsets only for emitted IDs, not the full input history.
                let mut baselines = std::collections::BTreeMap::<String, usize>::new();
                let mut record = |id: &str| {
                    let history_results = result_counts.get(id).copied().unwrap_or(0);
                    let baseline = baselines.entry(id.to_owned()).or_insert(history_results);
                    self.pending_tools.push(PendingTool {
                        id: id.to_owned(),
                        prior_results: *baseline,
                        history_results,
                        generation: turn.to_owned(),
                    });
                    *baseline += 1;
                };
                for item in &response.items {
                    if item.role != Role::Assistant {
                        continue;
                    }
                    if let Some(calls) = &item.tool_calls {
                        for call in calls {
                            record(call.id.as_str());
                        }
                    }
                    if let MessageContent::Blocks(blocks) = &item.content {
                        for block in blocks {
                            if let ContentBlock::ToolUse { id, .. }
                            | ContentBlock::ServerToolUse { id, .. } = block
                            {
                                // Canonical items may expose the same call in both forms.
                                if !item
                                    .tool_calls
                                    .as_ref()
                                    .is_some_and(|calls| calls.iter().any(|call| call.id == *id))
                                {
                                    record(id.as_str());
                                }
                            }
                        }
                    }
                }
            }
        } else {
            self.pending_tools.clear();
        }
        Ok(())
    }
}

fn update<T>(
    cell: &Cell,
    mut transition: impl FnMut(Option<State>) -> Result<(State, T), PluginError>,
) -> Result<T, PluginError> {
    loop {
        let snapshot = cell.load().map_err(PluginError::from)?;
        let state = snapshot
            .payload
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| crate::wire::invalid("invalid Antigravity lifecycle state"))?;
        let (state, result) = transition(state)?;
        let bytes = serde_json::to_vec(&state)
            .map_err(|_| crate::wire::invalid("cannot encode Antigravity lifecycle state"))?;
        match cell.compare_exchange(snapshot.revision, Some(&bytes)) {
            Ok(()) => return Ok(result),
            Err(Error::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) struct Reservation {
    cell: Cell,
    turn: String,
    result_counts: std::collections::BTreeMap<String, usize>,
}
impl Reservation {
    pub fn begin(input: Input) -> Result<(Self, Labels), PluginError> {
        let cell = state::open(Scope::Conversation, KEY).map_err(PluginError::from)?;
        let mut trajectory = None;
        let labels = update(&cell, |state| {
            let mut state = state.unwrap_or_else(|| {
                State::new(&input, *trajectory.get_or_insert_with(uuid::Uuid::new_v4))
            });
            let labels = state.begin(&input)?;
            Ok((state, labels))
        })?;
        Ok((
            Self {
                cell,
                turn: input.turn,
                result_counts: input.result_counts,
            },
            labels,
        ))
    }
    pub fn finish(self, response: Option<&AiResponse>) -> Result<(), PluginError> {
        update(&self.cell, |state| {
            let mut state =
                state.ok_or_else(|| crate::wire::invalid("missing Antigravity lifecycle state"))?;
            state.finish(&self.turn, &self.result_counts, response)?;
            Ok((state, ()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ir::ToolCall;

    fn input(turn: &str, logical: &str) -> Input {
        Input {
            cascade: "cascade".into(),
            window: "window".into(),
            logical: logical.into(),
            turn: turn.into(),
            fresh: true,
            host_index: match turn {
                "a" => 0,
                "b" => 1,
                "c" => 2,
                "d" => 3,
                _ => panic!("unknown test turn"),
            },
            mixed_providers: false,
            result_counts: Default::default(),
            claude: false,
            api_known: true,
            google: true,
        }
    }
    fn response(ids: &[&str]) -> AiResponse {
        let mut response = AiResponse::new("response", "model");
        for id in ids {
            response.push_tool_call(ToolCall {
                id: (*id).into(),
                name: "tool".into(),
                arguments: "{}".into(),
            });
        }
        response
    }
    fn done(state: &mut State, input: &Input, ids: &[&str]) {
        state
            .finish(&input.turn, &input.result_counts, Some(&response(ids)))
            .unwrap();
    }
    fn new_state(input: &Input) -> State {
        State::new(input, uuid::Uuid::nil())
    }

    #[test]
    fn fresh_multiturn_and_retry_replace_outcome() {
        let a = input("a", "first");
        let mut state = new_state(&a);
        let first = state.begin(&a).unwrap();
        assert_eq!(first.request_id, Some(format!("{}-0", uuid::Uuid::nil())));
        assert_eq!(first.last_step_index, Some(0));
        assert!(
            !first.used_claude && !first.used_claude_conservative && !first.used_non_gemini_model
        );
        state.finish("a", &a.result_counts, None).unwrap();
        let retry = state.begin(&a).unwrap();
        assert_eq!(retry.request_id, first.request_id);
        assert_eq!(retry.last_step_index, Some(0));
        done(&mut state, &a, &[]);
        let b = input("b", "second");
        let second = state.begin(&b).unwrap();
        assert_eq!(second.last_step_index, Some(2));
        assert_eq!(second.last_execution_id.as_deref(), Some("first"));
        done(&mut state, &b, &[]);
        let c = input("c", "second");
        let continuation = state.begin(&c).unwrap();
        assert_eq!(continuation.last_step_index, Some(3));
        assert_eq!(continuation.last_execution_id.as_deref(), Some("first"));
    }

    #[test]
    fn superseded_turn_cannot_replace_newer_generation() {
        let a = input("a", "first");
        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        done(&mut state, &a, &[]);

        let mut b = input("b", "first");
        b.claude = true;
        b.google = false;
        state.begin(&b).unwrap();
        done(&mut state, &b, &[]);

        let error = match state.begin(&a) {
            Err(error) => error,
            Ok(_) => panic!("superseded turn was accepted"),
        };
        assert!(matches!(
            error.kind,
            stravia_vendor_sdk::ErrorKind::Cancelled
        ));
        let mut same_index = input("a", "first");
        same_index.host_index = b.host_index;
        assert!(matches!(
            state.begin(&same_index),
            Err(PluginError {
                kind: stravia_vendor_sdk::ErrorKind::Cancelled,
                ..
            })
        ));

        let c = input("c", "first");
        let labels = state.begin(&c).unwrap();
        assert_eq!(labels.request_id, Some(format!("{}-2", uuid::Uuid::nil())));
        assert_eq!(labels.last_step_index, Some(2));
        assert!(
            labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
    }

    #[test]
    fn failure_counts_once_without_assistant_history() {
        let a = input("a", "first");
        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        state.finish("a", &a.result_counts, None).unwrap();
        let b = input("b", "first");
        let labels = state.begin(&b).unwrap();
        assert_eq!(labels.request_id, Some(format!("{}-1", uuid::Uuid::nil())));
        assert_eq!(labels.last_step_index, Some(1));
    }

    #[test]
    fn emitted_results_replays_and_reused_id() {
        let a = input("a", "first");
        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        done(&mut state, &a, &["x", "y"]);
        let mut b = input("b", "first");
        b.result_counts.insert("x".into(), 1);
        b.result_counts.insert("y".into(), 1);
        assert_eq!(state.begin(&b).unwrap().last_step_index, Some(3));
        done(&mut state, &b, &["x"]);
        let mut c = input("c", "first");
        c.result_counts = b.result_counts.clone();
        assert_eq!(state.begin(&c).unwrap().last_step_index, Some(4));
        done(&mut state, &c, &[]);
        let mut d = input("d", "first");
        d.result_counts = b.result_counts.clone();
        d.result_counts.insert("x".into(), 2);
        assert_eq!(state.begin(&d).unwrap().last_step_index, Some(6));
    }

    #[test]
    fn duplicated_canonical_call_representation_counts_once() {
        let a = input("a", "first");
        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        let mut combined = AiResponse::new("response", "model");
        combined.items.push(response(&["x"]).to_assistant_item());
        state
            .finish(&a.turn, &a.result_counts, Some(&combined))
            .unwrap();
        assert_eq!(state.pending_tools.len(), 1);
        let mut b = input("b", "first");
        b.result_counts.insert("x".into(), 1);
        assert_eq!(state.begin(&b).unwrap().last_step_index, Some(2));
    }

    #[test]
    fn canonical_item_collects_each_result() {
        use stravia_protocol_codec::transform::ProtocolTransform;
        use stravia_runtime_contract::protocol::ids::GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
        let protocol = GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA;
        let mut request = ProtocolTransform::global()
            .bind(protocol, protocol)
            .unwrap()
            .decode_request(
                serde_json::json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}),
            )
            .unwrap();
        request.items[0].content = serde_json::from_value(serde_json::json!([
            {"type":"tool_result","tool_use_id":"x","content":"one"},
            {"type":"tool_result","tool_use_id":"y","content":"two"}
        ]))
        .unwrap();
        let mut provider: ProviderSnapshot = serde_json::from_value(serde_json::json!({
            "provider_id":"antigravity", "channel":"oauth", "base_url":crate::BASE_URL,
            "protocol":"google-gemini", "operation_metadata":{
                "client_conversation_id":"root", "context_window_id":"window", "logical_turn_id":"logical",
                "model_turn_id":"turn", "model_turn_index":0, "model_turn_mixed_providers":false, "context_window_index":0, "user_prompt_index":1
            }
        })).unwrap();
        let input = Input::new(&provider, &request, None, Some(&serde_json::json!(4))).unwrap();
        assert!(!input.fresh);
        assert_eq!(input.result_counts.get("x"), Some(&1));
        assert_eq!(input.result_counts.get("y"), Some(&1));
        request.items[0].content = MessageContent::Text("hello".to_owned().into());
        provider.operation_metadata.remove("user_prompt_index");
        provider.operation_metadata.remove("context_window_index");
        let input = Input::new(&provider, &request, None, Some(&serde_json::json!(4))).unwrap();
        assert!(!input.fresh);
        let mut state = new_state(&input);
        let labels = state.begin(&input).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
    }

    #[test]
    fn host_cursor_gaps_and_mixed_provider_retries_are_conservative() {
        let a = input("a", "first");
        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        done(&mut state, &a, &[]);
        let mut skipped = input("b", "second");
        skipped.host_index = 2;
        let labels = state.begin(&skipped).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert!(labels.last_execution_id.is_none());
        assert!(
            !labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
        done(&mut state, &skipped, &[]);
        let mut following = input("c", "third");
        following.host_index = 3;
        let labels = state.begin(&following).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert_eq!(labels.last_execution_id.as_deref(), Some("second"));

        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        done(&mut state, &a, &["pending"]);
        let mut mixed_retry = input("a", "first");
        mixed_retry.mixed_providers = true;
        let labels = state.begin(&mixed_retry).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert!(state.pending_tools.is_empty());
        assert!(
            !labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
        mixed_retry.host_index = 1;
        assert!(state.begin(&mixed_retry).is_err());

        let mut imported = input("a", "first");
        imported.host_index = 7;
        let mut state = new_state(&imported);
        let labels = state.begin(&imported).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert!(labels.used_claude_conservative && labels.used_non_gemini_model);
    }

    #[test]
    fn mixed_provider_history_invalidates_counts_without_a_cursor_gap() {
        let mut first_mixed = input("a", "first");
        first_mixed.mixed_providers = true;
        let mut state = new_state(&first_mixed);
        let labels = state.begin(&first_mixed).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert!(labels.used_claude_conservative && labels.used_non_gemini_model);

        let mut a = input("a", "first");
        a.claude = true;
        a.google = false;
        let mut state = new_state(&a);
        state.begin(&a).unwrap();
        state.finish(&a.turn, &a.result_counts, None).unwrap();
        let mut b = input("b", "second");
        b.mixed_providers = true;
        let labels = state.begin(&b).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert_eq!(labels.last_execution_id.as_deref(), Some("first"));
        assert!(
            labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
        done(&mut state, &b, &["discarded"]);
        assert!(state.pending_tools.is_empty());
        let c = input("c", "second");
        let labels = state.begin(&c).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert_eq!(labels.last_execution_id.as_deref(), Some("first"));
        assert!(
            labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
    }

    #[test]
    fn unknown_interrupted_window_and_model_evidence() {
        let mut imported = input("a", "first");
        imported.fresh = false;
        let mut state = new_state(&imported);
        let labels = state.begin(&imported).unwrap();
        assert!(labels.request_id.is_none() && labels.last_step_index.is_none());
        assert!(
            !labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
        let mut first = input("a", "first");
        first.claude = true;
        first.google = false;
        let mut state = new_state(&first);
        state.begin(&first).unwrap();
        done(&mut state, &first, &[]);
        let next = input("b", "first");
        let labels = state.begin(&next).unwrap();
        assert!(
            labels.used_claude && labels.used_claude_conservative && labels.used_non_gemini_model
        );
        let after_interruption = input("c", "second");
        let labels = state.begin(&after_interruption).unwrap();
        assert!(labels.last_step_index.is_none());
        assert_eq!(labels.last_execution_id.as_deref(), Some("first"));
        let mut first = input("a", "first");
        first.google = false;
        let mut state = new_state(&first);
        state.begin(&first).unwrap();
        done(&mut state, &first, &[]);
        let mut next = input("b", "first");
        next.window = "replacement".into();
        let labels = state.begin(&next).unwrap();
        assert!(
            !labels.used_claude && !labels.used_claude_conservative && labels.used_non_gemini_model
        );
        assert!(labels.last_step_index.is_none());
        assert_eq!(labels.request_id, Some(format!("{}-1", uuid::Uuid::nil())));
    }
}
