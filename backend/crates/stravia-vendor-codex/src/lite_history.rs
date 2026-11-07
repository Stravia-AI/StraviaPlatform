use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use stravia_vendor_sdk::{
    PluginError,
    state::{self, journal},
};

use super::{codex::NativeSession, invalid};

#[derive(Deserialize)]
pub(super) struct BoundaryFacts {
    pub version: u64,
    pub window_id: String,
    pub coverage: String,
    pub current_boundary_id: String,
    pub boundaries: Vec<BoundaryFact>,
}

#[derive(Deserialize)]
pub(super) struct BoundaryFact {
    pub id: String,
    pub item_index: usize,
}

#[derive(Default)]
pub(super) struct BoundaryOffsets {
    pub current: String,
    pub positions: BTreeMap<String, usize>,
}

pub(super) fn project_boundaries(
    provider: &stravia_vendor_sdk::ProviderSnapshot,
    session: &NativeSession,
    gaps: &[usize],
) -> Result<Option<BoundaryOffsets>, PluginError> {
    let raw = provider
        .operation_metadata
        .get("history_boundaries")
        .ok_or_else(|| invalid("Codex canonical history boundaries are unavailable"))?;
    let facts: BoundaryFacts = serde_json::from_value(raw.clone())
        .map_err(|_| invalid("Codex canonical history boundaries are invalid"))?;
    if facts.version != 1
        || facts.coverage != "complete"
        || facts.window_id != session.history_window()
    {
        return Err(invalid("Codex canonical history boundaries are incomplete"));
    }
    let mut positions = BTreeMap::new();
    for boundary in facts.boundaries {
        if boundary.id.is_empty() {
            return Err(invalid("Codex canonical history boundary ID is invalid"));
        }
        let offset = *gaps
            .get(boundary.item_index)
            .ok_or_else(|| invalid("Codex canonical history boundary index is invalid"))?;
        if positions
            .insert(boundary.id, offset)
            .is_some_and(|previous| previous != offset)
        {
            return Err(invalid("Codex canonical history boundary ID is ambiguous"));
        }
    }
    if !positions.contains_key(&facts.current_boundary_id) {
        return Err(invalid("Codex current history boundary is unavailable"));
    }
    Ok(Some(BoundaryOffsets {
        current: facts.current_boundary_id,
        positions,
    }))
}

#[derive(Default, Serialize, Deserialize)]
struct Checkpoint {
    window: String,
    model: String,
    instructions: String,
    tools: BTreeMap<String, String>,
    #[serde(default)]
    request_signature: String,
    boundary_id: String,
}

#[derive(Serialize, Deserialize)]
struct Update {
    boundary_id: String,
    signature: String,
    items: Vec<Value>,
}

struct HashWriter(Sha256);
impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn digest(value: &Value) -> Result<String, PluginError> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| invalid("Codex history could not be encoded"))?;
    Ok(stravia_runtime_contract::protocol::ir::canonical::hash_hex(
        &writer.0.finalize().into(),
    ))
}

fn request_signature(body: &Value) -> Result<String, PluginError> {
    let mut writer = HashWriter(Sha256::new());
    let object = body
        .as_object()
        .ok_or_else(|| invalid("Codex history request is invalid"))?;
    for (key, value) in object {
        serde_json::to_writer(&mut writer, key)
            .map_err(|_| invalid("Codex history signature could not be encoded"))?;
        if key == "input" {
            let items = value
                .as_array()
                .ok_or_else(|| invalid("Codex Responses input must be an item list"))?;
            for item in items {
                let source = if item.get("type").and_then(Value::as_str) == Some("additional_tools")
                {
                    item.get("tools").unwrap_or(item)
                } else if item
                    .pointer("/internal_chat_message_metadata_passthrough/content_item_kinds/0")
                    .and_then(Value::as_str)
                    == Some("model.base_instructions")
                {
                    item.get("content").unwrap_or(item)
                } else {
                    item
                };
                serde_json::to_writer(&mut writer, source)
                    .map_err(|_| invalid("Codex history signature could not be encoded"))?;
                writer.0.update([0]);
            }
        } else {
            serde_json::to_writer(&mut writer, value)
                .map_err(|_| invalid("Codex history signature could not be encoded"))?;
        }
    }
    Ok(stravia_runtime_contract::protocol::ir::canonical::hash_hex(
        &writer.0.finalize().into(),
    ))
}

fn name(value: &Value) -> Result<&str, PluginError> {
    value
        .get("name")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Codex tool declaration has no name"))
}

fn hashes(tools: &[Value]) -> Result<BTreeMap<String, String>, PluginError> {
    let mut hashes = BTreeMap::new();
    for tool in tools {
        let key = name(tool)?.to_owned();
        let mut header = tool.clone();
        if let Some(members) = header
            .as_object_mut()
            .and_then(|value| value.remove("tools"))
        {
            for member in members
                .as_array()
                .ok_or_else(|| invalid("Codex tool namespace is invalid"))?
            {
                let member_key = format!("{key}.{}", name(member)?);
                if hashes.insert(member_key, digest(member)?).is_some() {
                    return Err(invalid("Codex tool names collide"));
                }
            }
        }
        if hashes.insert(key, digest(&header)?).is_some() {
            return Err(invalid("Codex tool names collide"));
        }
    }
    Ok(hashes)
}

fn fragment(session: &NativeSession, text: String, kind: &str, provenance: Value) -> Value {
    let now = chrono::Utc::now();
    let seconds = now.timestamp() as f64 + f64::from(now.timestamp_subsec_nanos()) / 1e9;
    json!({
        "type": "message", "id": format!("msg_{}", uuid::Uuid::now_v7()), "role": "developer",
        "content": [{"type":"input_text", "text":text}],
        "internal_chat_message_metadata_passthrough": {
            "turn_id": session.history_turn(), "create_time": seconds,
            "content_item_kinds": [kind], "content_item_metadata": [{"provenance":provenance}]
        }
    })
}

fn lite_instruction_update(
    previous_model: &str,
    model: &str,
    instructions: &str,
    changed: bool,
    session: &NativeSession,
) -> Option<Value> {
    let model_changed = previous_model != model;
    if !changed && !model_changed {
        return None;
    }
    let mut text = String::from("<model_instructions>\n");
    if model_changed {
        text.push_str(&format!("The current model is {model}.\n"));
    }
    if changed {
        if instructions.is_empty() {
            text.push_str("The previous base instructions no longer apply. No replacement base instructions are active.\n");
        } else {
            text.push_str(&format!("The following instructions replace the previous base instructions:\n\n{instructions}\n"));
        }
    }
    text.push_str("</model_instructions>");
    Some(fragment(
        session,
        text,
        "model.instructions",
        json!({"type":"harness", "hook":false}),
    ))
}

fn lite_tool_updates(
    tools: &[Value],
    current: &BTreeMap<String, String>,
    previous: Option<&BTreeMap<String, String>>,
    original: Option<&Value>,
    session: &NativeSession,
) -> Result<Vec<Value>, PluginError> {
    let changed = |key: &str| previous.and_then(|p| p.get(key)) != current.get(key);
    let mut declarations = Vec::new();
    let mut namespace_notices = Vec::new();
    for definition in tools {
        let key = name(definition)?;
        if let Some(members) = definition.get("tools").and_then(Value::as_array) {
            let mut updated = Vec::new();
            for member in members {
                if changed(&format!("{key}.{}", name(member)?)) {
                    updated.push(member.clone());
                }
            }
            if !updated.is_empty() {
                let mut namespace = definition.clone();
                namespace["tools"] = Value::Array(updated);
                declarations.push(namespace);
            } else if changed(key) {
                let description = definition
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = if description.is_empty() {
                    session
                        .incremental_message(
                            "namespace_instructions_cleared",
                            "The {name} namespace no longer has additional instructions.",
                        )
                        .replace("{name}", key)
                } else {
                    format!(
                        "{}{description}",
                        session
                            .incremental_message(
                                "namespace_instructions_prefix",
                                "Updated instructions for the {name} namespace:\n"
                            )
                            .replace("{name}", key)
                    )
                };
                if !text.is_empty() {
                    namespace_notices.push(fragment(
                        session,
                        text,
                        "generic.developer_instructions",
                        json!({"type":"tool", "namespace":key}),
                    ));
                }
            }
        } else if changed(key) {
            declarations.push(definition.clone());
        }
    }
    let mut updates = Vec::new();
    if !declarations.is_empty() {
        if previous.is_some()
            && declarations
                .iter()
                .any(|d| d.get("type").and_then(Value::as_str) == Some("namespace"))
        {
            let hint = session.incremental_message("tool_update_hint", "This is an incremental tools update. Previously declared tools remain available for direct calls unless explicitly marked unavailable. If a tool is redefined here, its latest definition replaces the earlier one.");
            if !hint.is_empty() {
                updates.push(fragment(
                    session,
                    hint.into(),
                    "tools.incremental_update",
                    json!({"type":"harness", "hook":false}),
                ));
            }
        }
        let mut item = original
            .cloned()
            .ok_or_else(|| invalid("Codex tool update is missing its declaration"))?;
        item["tools"] = Value::Array(declarations);
        updates.push(item);
    }
    updates.extend(namespace_notices);
    if let Some(previous) = previous {
        let namespaces: std::collections::BTreeSet<&str> = previous
            .keys()
            .filter_map(|key| key.split_once('.').map(|(namespace, _)| namespace))
            .filter(|namespace| {
                previous.contains_key(*namespace) && !current.contains_key(*namespace)
            })
            .collect();
        let removed: Vec<&str> = previous
            .keys()
            .filter(|key| !current.contains_key(*key))
            .filter(|key| !namespaces.contains(key.as_str()))
            .filter(|key| {
                !key.split_once('.')
                    .is_some_and(|(namespace, _)| namespaces.contains(namespace))
            })
            .map(String::as_str)
            .collect();
        let mut sections = Vec::new();
        if !namespaces.is_empty() {
            let mut text = session.incremental_message("removed_namespaces_header", "The following namespaces are no longer available. Do not call tools in them unless those tools are declared in a later update:").to_owned();
            for key in namespaces {
                text.push_str(&format!("\n- {key}"));
            }
            sections.push(text);
        }
        if !removed.is_empty() {
            let mut text = session
                .incremental_message(
                    "removed_tools_header",
                    "The following tools are no longer available. Do not call them:",
                )
                .to_owned();
            for key in removed {
                text.push_str(&format!("\n- {key}"));
            }
            sections.push(text);
        }
        if !sections.is_empty() {
            updates.push(fragment(
                session,
                sections.join("\n\n"),
                "tools.removed_definition",
                json!({"type":"harness", "hook":false}),
            ));
        }
    }
    Ok(updates)
}

/// Apply the host-owned window journal to the current full request or WS suffix.
pub(super) fn stabilize(session: &NativeSession, body: &mut Value) -> Result<(), PluginError> {
    if !session.uses_lite_history() {
        return Ok(());
    }
    let continuation = super::codex::request_continuation_id(body).is_some();
    let request_signature = request_signature(body)?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid("Codex Responses input must be an item list"))?;
    let mut tools_item = None;
    let mut instructions_item = None;
    while let Some(item) = input.first() {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            tools_item = Some(input.remove(0));
        } else if item
            .pointer("/internal_chat_message_metadata_passthrough/content_item_kinds/0")
            .and_then(Value::as_str)
            == Some("model.base_instructions")
        {
            instructions_item = Some(input.remove(0));
        } else {
            break;
        }
    }
    let tools = tools_item
        .as_ref()
        .and_then(|item| item.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tool_hashes = hashes(&tools)?;
    let instructions = instructions_item
        .as_ref()
        .and_then(|item| item.pointer("/content/0/text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let instruction_hash = digest(&Value::String(instructions.clone()))?;
    let cell = state::open(state::Scope::Conversation, "codex-lite-history")?;
    loop {
        let snapshot = journal::load(&cell)?;
        let old: Option<Checkpoint> = snapshot
            .payload
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| invalid("Codex Lite history checkpoint is invalid"))?;
        let reset = old
            .as_ref()
            .is_none_or(|old| old.window != session.history_window());
        if continuation && reset {
            return Err(PluginError {
                kind: stravia_vendor_sdk::ErrorKind::ContinuationUnavailable,
                message: "Codex Lite continuation history is unavailable".into(),
                upstream_status: None,
            });
        }
        let previous = if reset { None } else { old.as_ref() };
        let mut items = lite_tool_updates(
            &tools,
            &tool_hashes,
            previous.map(|p| &p.tools),
            tools_item.as_ref(),
            session,
        )?;
        if previous.is_none() {
            if let Some(item) = &instructions_item {
                items.push(item.clone());
            }
        } else if let Some(previous) = previous
            && (previous.instructions != instruction_hash || previous.model != model)
            && let Some(item) = lite_instruction_update(
                &previous.model,
                &model,
                &instructions,
                previous.instructions != instruction_hash,
                session,
            )
        {
            items.push(item);
        }
        let boundaries = session
            .history_boundaries
            .as_ref()
            .ok_or_else(|| invalid("Codex canonical history boundaries are unavailable"))?;
        let retry = previous.is_some_and(|previous| {
            previous.request_signature == request_signature
                && previous.boundary_id == boundaries.current
        });
        if continuation
            && retry
            && items.is_empty()
            && let Some(head) = snapshot.head.as_ref()
        {
            let update: Update = serde_json::from_str(&head.read()?.text)
                .map_err(|_| invalid("Codex Lite history update is invalid"))?;
            if update.signature == request_signature {
                items = update.items;
            }
        }
        let checkpoint = Checkpoint {
            window: session.history_window().to_owned(),
            model: model.clone(),
            instructions: instruction_hash.clone(),
            tools: tool_hashes.clone(),
            request_signature: request_signature.clone(),
            boundary_id: boundaries.current.clone(),
        };
        let encoded = serde_json::to_vec(&checkpoint)
            .map_err(|_| invalid("Codex Lite history checkpoint could not be encoded"))?;
        let position = *boundaries
            .positions
            .get(&boundaries.current)
            .ok_or_else(|| invalid("Codex current history boundary is unavailable"))?;
        if position > input.len() {
            return Err(invalid("Codex current history boundary is invalid"));
        }
        let new_head = if !items.is_empty() && !retry {
            let text = serde_json::to_string(&Update {
                boundary_id: boundaries.current.clone(),
                signature: request_signature.clone(),
                items: items.clone(),
            })
            .map_err(|_| invalid("Codex Lite history update could not be encoded"))?;
            Some(journal::append(
                &cell,
                &text,
                if reset { None } else { snapshot.head.as_ref() },
            )?)
        } else {
            None
        };
        let head = new_head
            .as_ref()
            .or(if reset { None } else { snapshot.head.as_ref() });
        if !continuation {
            let mut next = head.map(|entry| entry.read()).transpose()?;
            let mut last_position = input.len();
            while let Some(data) = next {
                let update: Update = serde_json::from_str(&data.text)
                    .map_err(|_| invalid("Codex Lite history update is invalid"))?;
                let resolved = *boundaries
                    .positions
                    .get(&update.boundary_id)
                    .ok_or_else(|| {
                        invalid("Codex Lite canonical history boundary is unavailable")
                    })?;
                if resolved > last_position {
                    return Err(invalid("Codex Lite history boundary order is invalid"));
                }
                last_position = resolved;
                next = data.previous.map(|entry| entry.read()).transpose()?;
            }
        }
        if !retry {
            match journal::compare_exchange(&cell, snapshot.revision, Some(&encoded), head) {
                Ok(()) => {}
                Err(state::Error::Conflict) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if continuation {
            if position > input.len() {
                return Err(invalid("Codex current history boundary is invalid"));
            }
            input.splice(position..position, items);
        } else {
            let mut cursor = head;
            let mut history = Vec::new();
            while let Some(entry) = cursor {
                let data = entry.read()?;
                let update: Update = serde_json::from_str(&data.text)
                    .map_err(|_| invalid("Codex Lite history update is invalid"))?;
                history.push(update);
                // Own the next resource while traversing the linked list.
                cursor = None;
                let mut next = data.previous;
                while let Some(entry) = next {
                    let data = entry.read()?;
                    history.push(
                        serde_json::from_str(&data.text)
                            .map_err(|_| invalid("Codex Lite history update is invalid"))?,
                    );
                    next = data.previous;
                }
            }
            history.reverse();
            let mut inserted = 0;
            for update in history {
                let position = *boundaries
                    .positions
                    .get(&update.boundary_id)
                    .ok_or_else(|| {
                        invalid("Codex Lite canonical history boundary is unavailable")
                    })?;
                let at = position + inserted;
                inserted += update.items.len();
                input.splice(at..at, update.items);
            }
        }
        return Ok(());
    }
}
