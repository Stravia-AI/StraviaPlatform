use serde_json::{Map, Value, json};
use stravia_runtime_contract::protocol::ir::{
    AiRequest, CacheParameters, ContentBlock, MessageContent, ResponseFormat, Role, ToolChoice,
    ToolResultContentKind,
};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, PluginError};

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

// Kiro's system-field injection path passes the initial instruction message
// as systemPrompt, separately from conversation history.
pub(super) fn encode(
    request: &AiRequest,
    model: &str,
    conversation_id: &str,
) -> Result<Value, PluginError> {
    if model.is_empty() || conversation_id.is_empty() {
        return Err(invalid("Kiro requires a model and conversation ID"));
    }
    let g = &request.generation;
    if g.temperature.is_some()
        || g.max_tokens.is_some()
        || g.top_p.is_some()
        || g.seed.is_some()
        || g.stop.is_some()
        || g.presence_penalty.is_some()
        || g.frequency_penalty.is_some()
        || request.reasoning.mode.is_some()
        || request.reasoning.budget_tokens.is_some()
        || request.reasoning.summary.is_some()
        || request.reasoning.level.is_some()
        || request.reasoning.target_control.is_some()
        || !matches!(&request.response_format, None | Some(ResponseFormat::Text))
        || request.parallel_tool_calls.is_some()
        || !matches!(&request.tool_choice, None | Some(ToolChoice::Auto))
    {
        return Err(invalid(
            "Kiro GenerateAssistantResponse does not support the requested controls",
        ));
    }
    // Session identifiers and resets are host-side bookkeeping, not generation
    // controls. The only upstream identity is the host-scoped conversation ID.
    let p = &request.parameters;
    if p.top_k.is_some()
        || p.min_p.is_some()
        || p.repetition_penalty.is_some()
        || p.candidate_count.is_some()
        || p.logit_bias.is_some()
        || p.logprobs.is_some()
        || p.modalities.is_some()
        || p.audio.is_some()
        || p.verbosity.is_some()
        || p.max_tool_calls.is_some()
        || p.store.is_some()
        || p.background.is_some()
        || p.generate.is_some()
        || p.truncation.is_some()
        || p.compaction.is_some()
        || p.metadata.is_some()
        || p.end_user_id.is_some()
        || p.service_tier.is_some()
        || p.continuation.is_some()
        || !p.native_tools.is_empty()
        || p.cache != CacheParameters::default()
    {
        return Err(invalid(
            "Kiro GenerateAssistantResponse does not support the requested parameters",
        ));
    }

    let mut instructions = String::new();
    if let Some(text) = &request.instructions {
        instructions.push_str(text);
    }
    let mut turns: Vec<Turn> = Vec::new();
    let mut seen_conversation = false;
    for item in &request.items {
        if let Some(meta) = &item.meta {
            if meta.reference().is_some()
                || meta.opaque_value().is_some()
                || meta.object_extensions().is_some_and(|fields| {
                    fields.keys().any(|key| {
                        key != stravia_runtime_contract::protocol::ir::TOOL_RESULT_CONTENT_KIND_META
                    })
                })
            {
                return Err(invalid(
                    "Kiro cannot represent message metadata or unresolved references",
                ));
            }
        }
        if matches!(item.role, Role::System | Role::Developer) {
            if seen_conversation || item.tool_calls.is_some() || item.tool_call_id.is_some() {
                return Err(invalid(
                    "Kiro cannot represent instructions within conversation history",
                ));
            }
            let text = text_only(&item.content)?;
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(&text);
            continue;
        }
        seen_conversation = true;
        match item.role {
            Role::User => {
                if item.tool_calls.is_some() || item.tool_call_id.is_some() {
                    return Err(invalid("Invalid user message tool fields"));
                }
                if turns
                    .last()
                    .is_some_and(|turn| turn.user && !turn.text.is_empty())
                {
                    return Err(invalid("Kiro cannot preserve consecutive user messages"));
                }
                let turn = user_turn(&mut turns);
                append_user_content(turn, &item.content)?;
            }
            Role::Tool => {
                if item.tool_calls.is_some() {
                    return Err(invalid("Invalid tool message"));
                }
                let id = item
                    .tool_call_id
                    .as_ref()
                    .ok_or_else(|| invalid("Tool result missing call ID"))?;
                let turn = user_turn(&mut turns);
                match &item.content {
                    MessageContent::Blocks(blocks)
                        if matches!(blocks.as_slice(), [ContentBlock::ToolResult { .. }]) =>
                    {
                        if let ContentBlock::ToolResult { tool_use_id, .. } = &blocks[0] {
                            if tool_use_id != id {
                                return Err(invalid("Tool result call IDs do not match"));
                            }
                        }
                        append_user_content(turn, &item.content)?;
                    }
                    _ => {
                        let text = text_only(&item.content)?;
                        turn.results.push(json!({"toolUseId": id.as_str(), "content": [{"text": text}], "status": "success"}));
                    }
                }
            }
            Role::Assistant => {
                if item.tool_call_id.is_some() {
                    return Err(invalid("Invalid assistant message"));
                }
                let turn = assistant_turn(&mut turns);
                append_assistant_content(turn, &item.content)?;
                if let Some(calls) = &item.tool_calls {
                    for call in calls {
                        let input: Value = serde_json::from_str(&call.arguments)
                            .map_err(|_| invalid("Tool call arguments must be valid JSON"))?;
                        let mut tool_use =
                            json!({"toolUseId": call.id.as_str(), "name": call.name});
                        tool_use["input"] = input;
                        turn.uses.push(tool_use);
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    if turns.is_empty() || turns.last().is_some_and(|turn| !turn.user) {
        return Err(invalid("Kiro requires a final user message or tool result"));
    }
    if !turns[0].user {
        return Err(invalid(
            "Kiro conversation history must begin with a user message",
        ));
    }
    for index in 0..turns.len() {
        if !turns[index].user {
            continue;
        }
        let expected = if index == 0 {
            &[][..]
        } else {
            turns[index - 1].uses.as_slice()
        };
        let results = &turns[index].results;
        let expected =
            tool_ids(expected).ok_or_else(|| invalid("Invalid or duplicate Kiro tool use ID"))?;
        let actual =
            tool_ids(results).ok_or_else(|| invalid("Invalid or duplicate Kiro tool result ID"))?;
        if expected != actual {
            return Err(invalid(
                "Kiro tool results must match the preceding assistant tool calls",
            ));
        }
    }
    let mut tools = Vec::new();
    for tool in request.tools.iter().flatten() {
        if tool.strict == Some(true) || tool.defer_loading == Some(true) {
            return Err(invalid("Kiro does not support strict or deferred tools"));
        }
        let mut spec = Map::new();
        spec.insert("name".into(), json!(tool.name));
        spec.insert("inputSchema".into(), json!({"json": tool.parameters}));
        let description = tool
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| {
                if tool.name.trim().is_empty() {
                    "tool"
                } else {
                    tool.name.as_str()
                }
            });
        spec.insert("description".into(), json!(description));
        let mut wrapper = Map::new();
        wrapper.insert("toolSpecification".into(), Value::Object(spec));
        tools.push(Value::Object(wrapper));
    }
    let last = turns.len() - 1;
    let mut tools = Some(tools);
    let mut messages = Vec::with_capacity(turns.len());
    for (index, turn) in turns.into_iter().enumerate() {
        if turn.user {
            let mut message = json!({"modelId": model, "origin": "KIRO_CLI"});
            message["content"] = Value::String(turn.text);
            let mut context = Map::new();
            if !turn.results.is_empty() {
                context.insert("toolResults".into(), Value::Array(turn.results));
            }
            if index == last {
                let tools = tools.take().expect("last turn appears once");
                if !tools.is_empty() {
                    context.insert("tools".into(), Value::Array(tools));
                }
            }
            if !context.is_empty() {
                message["userInputMessageContext"] = Value::Object(context);
            }
            let mut wrapper = Map::new();
            wrapper.insert("userInputMessage".into(), message);
            messages.push(Value::Object(wrapper));
        } else {
            let mut message = Map::new();
            message.insert("content".into(), Value::String(turn.text));
            if !turn.uses.is_empty() {
                message.insert("toolUses".into(), Value::Array(turn.uses));
            }
            let mut wrapper = Map::new();
            wrapper.insert("assistantResponseMessage".into(), Value::Object(message));
            messages.push(Value::Object(wrapper));
        }
    }
    let current = messages.pop().expect("nonempty turns");
    let mut state = json!({
        "chatTriggerType": "MANUAL",
        "conversationId": conversation_id,
        "rootConversationId": conversation_id,
        "agentTaskType": "vibe"
    });
    state["currentMessage"] = current;
    state["history"] = Value::Array(messages);
    let mut body = Map::new();
    body.insert("agentMode".into(), Value::String("vibe".into()));
    body.insert("conversationState".into(), state);
    if !instructions.is_empty() {
        body.insert("systemPrompt".into(), Value::String(instructions));
    }
    Ok(Value::Object(body))
}

fn tool_ids(entries: &[Value]) -> Option<std::collections::HashSet<&str>> {
    let mut ids = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        let id = entry.get("toolUseId")?.as_str()?;
        if id.is_empty() || !ids.insert(id) {
            return None;
        }
    }
    Some(ids)
}

#[derive(Default)]
struct Turn {
    user: bool,
    text: String,
    uses: Vec<Value>,
    results: Vec<Value>,
}

fn user_turn(turns: &mut Vec<Turn>) -> &mut Turn {
    if turns.last().is_none_or(|turn| !turn.user) {
        turns.push(Turn {
            user: true,
            ..Default::default()
        });
    }
    turns.last_mut().unwrap()
}
fn assistant_turn(turns: &mut Vec<Turn>) -> &mut Turn {
    if turns.last().is_none_or(|turn| turn.user) {
        turns.push(Turn::default());
    }
    turns.last_mut().unwrap()
}
fn text_only(content: &MessageContent) -> Result<String, PluginError> {
    match content {
        MessageContent::Text(text) => Ok(text.to_string()),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text {
                        text: part,
                        cache_control: None,
                    } => text.push_str(part),
                    _ => return Err(invalid("Kiro supports only plain text in this message")),
                }
            }
            Ok(text)
        }
    }
}
fn append_user_content(turn: &mut Turn, content: &MessageContent) -> Result<(), PluginError> {
    match content {
        MessageContent::Text(text) => turn.text.push_str(text),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text {
                        text,
                        cache_control: None,
                    } => turn.text.push_str(text),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        content_kind,
                        is_error,
                        cache_control: None,
                    } => {
                        let content = result_content(content, *content_kind)?;
                        let mut result = json!({"toolUseId": tool_use_id.as_str()});
                        result["content"] = Value::Array(content);
                        result["status"] = json!(if is_error.unwrap_or(false) {
                            "error"
                        } else {
                            "success"
                        });
                        turn.results.push(result);
                    }
                    _ => return Err(invalid("Unsupported Kiro user content")),
                }
            }
        }
    }
    Ok(())
}
fn result_content(
    value: &Value,
    kind: Option<ToolResultContentKind>,
) -> Result<Vec<Value>, PluginError> {
    match kind {
        Some(ToolResultContentKind::ContentBlocks) => {
            let blocks = value
                .as_array()
                .ok_or_else(|| invalid("Invalid tool result content blocks"))?;
            blocks
                .iter()
                .map(|block| {
                    let fields = block
                        .as_object()
                        .ok_or_else(|| invalid("Invalid tool result content block"))?;
                    if fields.get("type").and_then(Value::as_str) != Some("text")
                        || fields
                            .get("cache_control")
                            .is_some_and(|value| !value.is_null())
                        || fields
                            .keys()
                            .any(|key| !matches!(key.as_str(), "type" | "text" | "cache_control"))
                    {
                        return Err(invalid("Unsupported Kiro tool result content"));
                    }
                    let text = fields
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("Invalid tool result text"))?;
                    Ok(json!({"text": text}))
                })
                .collect()
        }
        _ => Ok(vec![match value {
            Value::String(text) => json!({"text": text}),
            _ => json!({"json": value}),
        }]),
    }
}
fn append_assistant_content(turn: &mut Turn, content: &MessageContent) -> Result<(), PluginError> {
    match content {
        MessageContent::Text(text) => {
            if !turn.uses.is_empty() && !text.is_empty() {
                return Err(invalid(
                    "Kiro cannot preserve interleaved assistant tool calls",
                ));
            }
            turn.text.push_str(text);
        }
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text {
                        text,
                        cache_control: None,
                    } if turn.uses.is_empty() || text.is_empty() => turn.text.push_str(text),
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control: None,
                    } => turn
                        .uses
                        .push(json!({"toolUseId": id.as_str(), "name": name, "input": input})),
                    _ => return Err(invalid("Unsupported or interleaved Kiro assistant content")),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(items: Value) -> AiRequest {
        serde_json::from_value(json!({"model":"source", "items": items})).unwrap()
    }

    #[test]
    fn constructs_conversation_and_preserves_model_and_scoped_id() {
        let mut req = request(json!([
            {"role":"system","content":"System"},
            {"role":"user","content":"Hello"},
            {"role":"assistant","content":"Hi"},
            {"role":"user","content":"Next"}
        ]));
        req.instructions = Some("Instruction".into());
        let body = encode(&req, "qwen3-coder-next", "scoped-id").unwrap();
        assert_eq!(body["systemPrompt"], "Instruction\n\nSystem");
        let state = &body["conversationState"];
        assert_eq!(state["history"][0]["userInputMessage"]["content"], "Hello");
        assert_eq!(
            state["history"][0]["userInputMessage"]["origin"],
            "KIRO_CLI"
        );
        assert_eq!(state["agentTaskType"], "vibe");
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["content"],
            "Hi"
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["modelId"],
            "qwen3-coder-next"
        );
        assert_eq!(state["conversationId"], "scoped-id");
        assert_eq!(state["rootConversationId"], "scoped-id");
    }

    #[test]
    fn maps_tool_calls_results_and_definitions() {
        let mut req = request(json!([
            {"role":"user","content":"Find it"},
            {"role":"assistant","content":"", "tool_calls":[{"id":"call-1","name":"lookup","arguments":"{\"q\":1}"}]},
            {"role":"tool","tool_call_id":"call-1","content":"found"}
        ]));
        req.tools = Some(
            serde_json::from_value(
                json!([{"name":"lookup","description":"Find","parameters":{"type":"object"}}]),
            )
            .unwrap(),
        );
        let state = encode(&req, "model", "id").unwrap()["conversationState"].clone();
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0],
            json!({"toolUseId":"call-1","name":"lookup","input":{"q":1}})
        );
        let context = &state["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        assert_eq!(context["toolResults"][0]["content"][0]["text"], "found");
        assert_eq!(
            context["tools"][0]["toolSpecification"]["inputSchema"]["json"]["type"],
            "object"
        );
    }

    #[test]
    fn preserves_tool_result_in_history_on_next_user_turn() {
        let req = request(json!([
            {"role":"user","content":"Read the synthetic input file."},
            {"role":"assistant","content":"Checking the file.","tool_calls":[{"id":"synthetic-tool-1","name":"read_file","arguments":"{\"path\":\"/workspace/input.txt\"}"}]},
            {"role":"tool","tool_call_id":"synthetic-tool-1","content":"synthetic file contents"},
            {"role":"assistant","content":"Synthetic file answer."},
            {"role":"user","content":"Next synthetic turn."}
        ]));
        let state = encode(&req, "model", "session-id").unwrap()["conversationState"].clone();
        assert_eq!(
            state["history"][1]["assistantResponseMessage"],
            json!({
                "content":"Checking the file.",
                "toolUses":[{"toolUseId":"synthetic-tool-1","name":"read_file","input":{"path":"/workspace/input.txt"}}]
            })
        );
        assert_eq!(
            state["history"][2]["userInputMessage"],
            json!({
                "content":"", "origin":"KIRO_CLI", "modelId":"model",
                "userInputMessageContext":{"toolResults":[{"toolUseId":"synthetic-tool-1","content":[{"text":"synthetic file contents"}],"status":"success"}]}
            })
        );
        assert_eq!(
            state["history"][3]["assistantResponseMessage"]["content"],
            "Synthetic file answer."
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["content"],
            "Next synthetic turn."
        );
    }

    #[test]
    fn maps_anthropic_tool_result_blocks_and_default_options() {
        let mut req = request(json!([
            {"role":"user","content":"Find it"},
            {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"lookup","input":{}}]},
            {"role":"tool","tool_call_id":"call-1","content":[{"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"failed"}],"content_kind":"content_blocks","is_error":true}],
             "meta":{"__stravia_tool_result_content_kind":"content_blocks"}}
        ]));
        req.tools = Some(serde_json::from_value(json!([{"name":"lookup","parameters":{"type":"object"},"strict":false,"defer_loading":false}])).unwrap());
        req.response_format = Some(ResponseFormat::Text);
        req.parameters.session.id = Some("host-local".into());
        let state = encode(&req, "model", "scoped-id").unwrap()["conversationState"].clone();
        let result = &state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"]
            [0];
        assert_eq!(result["content"], json!([{"text":"failed"}]));
        assert_eq!(result["status"], "error");
        assert_eq!(state["conversationId"], "scoped-id");
    }

    #[test]
    fn rejects_unsupported_media_controls_and_bad_tool_arguments() {
        let req = request(
            json!([{"role":"user","content":[{"type":"audio","source":{"type":"url","url":"https://example.test"}}]}]),
        );
        assert!(encode(&req, "model", "id").is_err());
        let mut req = request(json!([{"role":"user","content":"hello"}]));
        req.generation.temperature = Some(0.2);
        assert!(encode(&req, "model", "id").is_err());
        let req = request(json!([
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"", "tool_calls":[{"id":"x","name":"f","arguments":"bad"}]},
            {"role":"user","content":"next"}
        ]));
        assert!(encode(&req, "model", "id").is_err());
    }
}
