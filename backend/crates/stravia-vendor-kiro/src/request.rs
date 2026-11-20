use serde_json::{Map, Value, json};
use stravia_runtime_contract::protocol::ir::{
    AiRequest, CacheParameters, ContentBlock, DocumentSource, MediaSource, MessageContent,
    ResponseFormat, Role, ToolChoice, ToolResultContentKind,
};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, PluginError};

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

// Matches KAS 3.4.0 insertMsg0Separator (pV / IKe) in CustomAgentExecutionDefinition.
const SYSTEM_PROMPT_ACK: &str = "I will follow these instructions.";

// By default, injects leading instructions as history[0] (userInputMessage) +
// history[1] (assistantResponseMessage) without using the top-level systemPrompt field,
// matching Kiro CLI 3.4.0 Custom Agent behavior when system_field_injection is false.
pub(super) fn encode(
    request: &AiRequest,
    model: &str,
    conversation_id: &str,
    model_metadata: Option<&stravia_vendor_sdk::ModelMetadata>,
    inject_system_prompt_as_user: bool,
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
        || request.reasoning.summary.is_some()
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
        if let Some(meta) = &item.meta
            && (meta.reference().is_some()
                || meta.opaque_value().is_some()
                || meta.object_extensions().is_some_and(|fields| {
                    fields.keys().any(|key| {
                        key != stravia_runtime_contract::protocol::ir::TOOL_RESULT_CONTENT_KIND_META
                    })
                }))
        {
            return Err(invalid(
                "Kiro cannot represent message metadata or unresolved references",
            ));
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
                if turns.last().is_some_and(|turn| {
                    turn.user
                        && (!turn.text.is_empty()
                            || !turn.images.is_empty()
                            || !turn.documents.is_empty())
                }) {
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
                        if let ContentBlock::ToolResult { tool_use_id, .. } = &blocks[0]
                            && tool_use_id != id
                        {
                            return Err(invalid("Tool result call IDs do not match"));
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
    let mut document_names = std::collections::HashSet::new();
    for (index, mut turn) in turns.into_iter().enumerate() {
        for document in &mut turn.documents {
            let base = document["name"].as_str().expect("document name").to_owned();
            let mut name = base.clone();
            let mut suffix = 2;
            while !document_names.insert(name.clone()) {
                name = format!("{} ({suffix})", base.chars().take(190).collect::<String>());
                suffix += 1;
            }
            document["name"] = Value::String(name);
        }
        if turn.user {
            let mut message = json!({"modelId": model, "origin": "KIRO_CLI"});
            message["content"] = Value::String(turn.text);
            if !turn.images.is_empty() {
                message["images"] = Value::Array(turn.images);
            }
            if !turn.documents.is_empty() {
                message["documents"] = Value::Array(turn.documents);
            }
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
            if let Some(reasoning) = turn.reasoning {
                message.insert("reasoningContent".into(), reasoning);
            }
            let mut wrapper = Map::new();
            wrapper.insert("assistantResponseMessage".into(), Value::Object(message));
            messages.push(Value::Object(wrapper));
        }
    }
    let current = messages.pop().expect("nonempty turns");
    let has_instructions = !instructions.trim().is_empty();
    if inject_system_prompt_as_user && has_instructions {
        messages.splice(
            0..0,
            [
                json!({
                    "userInputMessage": {
                        "modelId": model,
                        "origin": "KIRO_CLI",
                        "content": instructions
                    }
                }),
                json!({
                    "assistantResponseMessage": {
                        "content": SYSTEM_PROMPT_ACK
                    }
                }),
            ],
        );
    }
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
    if let Some(fields) = additional_model_request_fields(request, model_metadata)? {
        body.insert("additionalModelRequestFields".into(), fields);
    }
    if !inject_system_prompt_as_user && has_instructions {
        body.insert("systemPrompt".into(), Value::String(instructions));
    }
    Ok(Value::Object(body))
}

fn additional_model_request_fields(
    request: &AiRequest,
    metadata: Option<&stravia_vendor_sdk::ModelMetadata>,
) -> Result<Option<Value>, PluginError> {
    use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
    let reasoning = &request.reasoning;
    let mode_thinking = match reasoning.mode.as_deref() {
        None => None,
        Some("disabled") => Some("disabled"),
        Some("adaptive" | "enabled") => Some("adaptive"),
        Some(_) => {
            return Err(invalid(
                "Kiro does not support the requested thinking mode or budget",
            ));
        }
    };
    let (mut effort, mut thinking_type) = match &reasoning.target_control {
        Some(TargetThinkingControl::Effort { value }) => (Some(value.as_str()), mode_thinking),
        Some(TargetThinkingControl::Enabled) => (None, Some("adaptive")),
        Some(TargetThinkingControl::Disabled) => (None, Some("disabled")),
        Some(_) => {
            return Err(invalid(
                "Kiro does not support the requested thinking control",
            ));
        }
        None if reasoning.budget_tokens.is_some() => {
            return Err(invalid(
                "Kiro does not support the requested thinking mode or budget",
            ));
        }
        None => match reasoning.level {
            Some(ThinkingLevel::Off) if mode_thinking.is_some() => (None, mode_thinking),
            Some(ThinkingLevel::Off) => (Some("none"), None),
            Some(level) => (Some(level.as_str()), mode_thinking),
            None => (None, mode_thinking),
        },
    };
    if effort.is_none() && thinking_type.is_none() {
        return Ok(None);
    }
    let extensions = &metadata
        .ok_or_else(|| invalid("Kiro reasoning requires model effort metadata"))?
        .extensions;
    let toggleable = extensions
        .get("kiro_thinking_toggleable")
        .and_then(Value::as_bool)
        == Some(true);
    if effort == Some("none") && toggleable {
        effort = None;
        thinking_type = Some("disabled");
    }
    let mut fields = Map::new();
    if let Some(requested_effort) = effort.take() {
        let group = extensions
            .get("kiro_effort_group")
            .and_then(Value::as_str)
            .filter(|group| matches!(*group, "output_config" | "reasoning"))
            .ok_or_else(|| invalid("Kiro reasoning requires a known model effort field"))?;
        let supported = extensions
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("Kiro reasoning requires declared model effort levels"))?;
        if !supported
            .iter()
            .any(|value| value.as_str() == Some(requested_effort))
        {
            return Err(invalid(
                "Kiro model does not declare the requested reasoning effort",
            ));
        }
        // Match KAS 3.4.0 R4t/cme: when thinking is disabled, cap xhigh/max to the
        // highest non-capped declared effort level (or omit if none exists).
        let applied =
            if thinking_type == Some("disabled") && matches!(requested_effort, "xhigh" | "max") {
                supported
                    .iter()
                    .filter_map(Value::as_str)
                    .rfind(|v| !matches!(*v, "none" | "xhigh" | "max"))
            } else {
                Some(requested_effort)
            };
        if let Some(applied) = applied {
            fields.insert(group.into(), json!({"effort": applied}));
        }
    }
    if let Some(thinking_type) = thinking_type {
        if !toggleable {
            return Err(invalid("Kiro model does not support thinking toggle"));
        }
        fields.insert("thinking".into(), json!({"type": thinking_type}));
    }
    if fields.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(fields)))
    }
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
    images: Vec<Value>,
    documents: Vec<Value>,
    uses: Vec<Value>,
    results: Vec<Value>,
    reasoning: Option<Value>,
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
                    ContentBlock::Image {
                        source: MediaSource::Base64 { media_type, data },
                        detail,
                        cache_control: None,
                    } if detail.as_deref().is_none_or(|detail| detail == "auto") => {
                        let format = match media_type.as_str() {
                            "image/png" => "png",
                            "image/jpeg" => "jpeg",
                            "image/gif" => "gif",
                            "image/webp" => "webp",
                            _ => return Err(invalid("Unsupported Kiro image format")),
                        };
                        validate_media(media_type, data)?;
                        turn.images
                            .push(json!({"format": format, "source": {"bytes": data}}));
                    }
                    ContentBlock::Document {
                        source: DocumentSource::Base64Pdf { data },
                        title,
                        context: None,
                        cache_control: None,
                    } => {
                        append_document(turn, "application/pdf", data, title.as_deref())?;
                    }
                    ContentBlock::Document {
                        source: DocumentSource::PlainText { data },
                        title,
                        context: None,
                        cache_control: None,
                    } => {
                        let reference = stravia_vendor_sdk::MediaReference {
                            media_type: "text/plain".into(),
                            bytes: data.as_bytes().to_vec(),
                        };
                        let encoded = serde_json::to_value(reference)
                            .map_err(|_| invalid("Invalid Kiro text document"))?;
                        let data = encoded["bytes"]
                            .as_str()
                            .ok_or_else(|| invalid("Invalid Kiro text document"))?;
                        append_document(turn, "text/plain", data, title.as_deref())?;
                    }
                    ContentBlock::File {
                        source: MediaSource::Base64 { media_type, data },
                        media_type: declared,
                    } if declared.as_deref().is_none_or(|value| value == media_type) => {
                        append_document(turn, media_type, data, None)?;
                    }
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
fn validate_media(media_type: &str, data: &str) -> Result<Vec<u8>, PluginError> {
    let reference: stravia_vendor_sdk::MediaReference = serde_json::from_value(json!({
        "media_type": media_type, "bytes": data
    }))
    .map_err(|_| invalid("Kiro media requires valid base64"))?;
    if reference.bytes.is_empty() {
        return Err(invalid("Kiro media must not be empty"));
    }
    Ok(reference.bytes)
}

fn append_document(
    turn: &mut Turn,
    media_type: &str,
    data: &str,
    title: Option<&str>,
) -> Result<(), PluginError> {
    let (format, magic): (&str, Option<&[u8]>) = match media_type {
        "application/pdf" => ("pdf", Some(b"%PDF")),
        "text/csv" => ("csv", None),
        "application/msword" => ("doc", Some(&[0xd0, 0xcf, 0x11, 0xe0])),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            ("docx", Some(b"PK\x03\x04"))
        }
        "application/vnd.ms-excel" => ("xls", Some(&[0xd0, 0xcf, 0x11, 0xe0])),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            ("xlsx", Some(b"PK\x03\x04"))
        }
        "text/html" => ("html", None),
        "text/plain" => ("txt", None),
        "text/markdown" => ("md", None),
        _ => return Err(invalid("Unsupported Kiro document format")),
    };
    let bytes = validate_media(media_type, data)?;
    if magic.is_some_and(|prefix| !bytes.starts_with(prefix)) {
        return Err(invalid("Kiro document content does not match its format"));
    }
    if turn.documents.len() >= 5 {
        return Err(invalid("Kiro supports at most five documents per message"));
    }
    let name = title.unwrap_or("document");
    let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '(' | ')' | '[' | ']') {
                c
            } else {
                '-'
            }
        })
        .take(200)
        .collect();
    let name = name.trim();
    let name = if name.is_empty() { "document" } else { name };
    turn.documents
        .push(json!({"name": name, "format": format, "source": {"bytes": data}}));
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
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        // Match KAS 3.4.0 n0l: replay signed reasoning; drop unsigned reasoning.
                        if !thinking.is_empty()
                            && let Some(sig) = signature.as_deref().filter(|s| !s.is_empty())
                            && turn.reasoning.is_none()
                        {
                            turn.reasoning = Some(json!({
                                "reasoningText": { "text": thinking, "signature": sig }
                            }));
                        }
                    }
                    ContentBlock::RedactedThinking { data } => {
                        if !data.is_empty() {
                            turn.reasoning = Some(json!({ "redactedContent": data }));
                        }
                    }
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

    fn encode(request: &AiRequest, model: &str, id: &str) -> Result<Value, PluginError> {
        super::encode(request, model, id, None, true)
    }

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
        assert!(body.get("systemPrompt").is_none());
        let state = &body["conversationState"];
        assert_eq!(
            state["history"][0]["userInputMessage"]["content"],
            "Instruction\n\nSystem"
        );
        assert_eq!(
            state["history"][0]["userInputMessage"]["modelId"],
            "qwen3-coder-next"
        );
        assert_eq!(
            state["history"][0]["userInputMessage"]["origin"],
            "KIRO_CLI"
        );
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["content"],
            "I will follow these instructions."
        );
        assert_eq!(state["history"][2]["userInputMessage"]["content"], "Hello");
        assert_eq!(
            state["history"][2]["userInputMessage"]["origin"],
            "KIRO_CLI"
        );
        assert_eq!(state["agentTaskType"], "vibe");
        assert_eq!(
            state["history"][3]["assistantResponseMessage"]["content"],
            "Hi"
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["modelId"],
            "qwen3-coder-next"
        );
        assert_eq!(state["conversationId"], "scoped-id");
        assert_eq!(state["rootConversationId"], "scoped-id");

        // When user injection is disabled, instructions stay in top-level systemPrompt.
        let legacy = super::encode(&req, "qwen3-coder-next", "scoped-id", None, false).unwrap();
        assert_eq!(legacy["systemPrompt"], "Instruction\n\nSystem");
        assert_eq!(
            legacy["conversationState"]["history"][0]["userInputMessage"]["content"],
            "Hello"
        );
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
    fn maps_user_media_in_current_and_history() {
        let req = request(json!([
            {"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="},"detail":"auto"},
                {"type":"document","source":{"type":"base64_pdf","data":"JVBERi0x"},"title":"report.pdf"}
            ]},
            {"role":"assistant","content":"OK"},
            {"role":"user","content":[
                {"type":"file","source":{"type":"base64","media_type":"text/plain","data":"aGk="}},
                {"type":"file","source":{"type":"base64","media_type":"text/plain","data":"Ynll"}},
                {"type":"document","source":{"type":"plain_text","data":"hi"},"title":"notes.txt"}
            ]}
        ]));
        let body = encode(&req, "model", "id").unwrap();
        let state = &body["conversationState"];
        assert_eq!(
            state["history"][0]["userInputMessage"]["images"],
            json!([{"format":"png","source":{"bytes":"aGk="}}])
        );
        assert_eq!(
            state["history"][0]["userInputMessage"]["documents"],
            json!([{"name":"report","format":"pdf","source":{"bytes":"JVBERi0x"}}])
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["documents"],
            json!([
                {"name":"document","format":"txt","source":{"bytes":"aGk="}},
                {"name":"document (2)","format":"txt","source":{"bytes":"Ynll"}},
                {"name":"notes","format":"txt","source":{"bytes":"aGk="}}
            ])
        );
    }

    #[test]
    fn rejects_unrepresentable_media() {
        for block in [
            json!({"type":"image","source":{"type":"url","url":"https://example.test/image.png"}}),
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"not base64"}}),
            json!({"type":"document","source":{"type":"base64_pdf","data":"aGk="}}),
            json!({"type":"document","source":{"type":"base64_pdf","data":"JVBERi0x"},"context":"context"}),
        ] {
            assert!(
                encode(
                    &request(json!([{"role":"user","content":[block]}])),
                    "model",
                    "id"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn maps_only_declared_effort_to_catalog_field() {
        use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
        let mut req = request(json!([{"role":"user","content":"hello"}]));
        req.reasoning.level = Some(ThinkingLevel::Low);
        let mut meta = stravia_vendor_sdk::ModelMetadata::default();
        meta.extensions
            .insert("reasoning_efforts".into(), json!(["low", "high"]));
        for group in ["output_config", "reasoning"] {
            meta.extensions
                .insert("kiro_effort_group".into(), json!(group));
            let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
            assert_eq!(body["additionalModelRequestFields"][group]["effort"], "low");
            req.reasoning.target_control = Some(TargetThinkingControl::Effort {
                value: "high".into(),
            });
            let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
            assert_eq!(
                body["additionalModelRequestFields"][group]["effort"],
                "high"
            );
            req.reasoning.target_control = None;
        }
        assert!(super::encode(&req, "model", "id", None, true).is_err());
        meta.extensions
            .insert("kiro_effort_group".into(), json!("thinking"));
        assert!(super::encode(&req, "model", "id", Some(&meta), true).is_err());
        meta.extensions
            .insert("kiro_effort_group".into(), json!("reasoning"));
        req.reasoning.level = Some(ThinkingLevel::Medium);
        assert!(super::encode(&req, "model", "id", Some(&meta), true).is_err());
        req.reasoning.level = Some(ThinkingLevel::Off);
        assert!(super::encode(&req, "model", "id", Some(&meta), true).is_err());
        meta.extensions
            .insert("reasoning_efforts".into(), json!(["none", "low", "high"]));
        let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"reasoning": {"effort": "none"}})
        );
        req.reasoning.target_control = Some(TargetThinkingControl::Enabled);
        assert!(super::encode(&req, "model", "id", Some(&meta), true).is_err());
    }

    #[test]
    fn maps_thinking_toggle_and_caps_effort_when_disabled() {
        use stravia_runtime_contract::thinking::{TargetThinkingControl, ThinkingLevel};
        let mut req = request(json!([{"role":"user","content":"hello"}]));
        let mut meta = stravia_vendor_sdk::ModelMetadata::default();
        meta.extensions
            .insert("kiro_effort_group".into(), json!("output_config"));
        meta.extensions.insert(
            "reasoning_efforts".into(),
            json!(["low", "medium", "high", "max"]),
        );
        meta.extensions
            .insert("kiro_thinking_toggleable".into(), json!(true));

        // Route 的 off 等级使用 canonical effort none；不能把 none 发进 output_config。
        req.reasoning.target_control = Some(TargetThinkingControl::Effort {
            value: "none".into(),
        });
        for mode in [None, Some("adaptive".into())] {
            req.reasoning.mode = mode;
            let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
            assert_eq!(
                body["additionalModelRequestFields"],
                json!({"thinking": {"type": "disabled"}})
            );
        }
        req.reasoning.mode = None;
        req.reasoning.target_control = None;
        req.reasoning.level = Some(ThinkingLevel::Off);
        let group = meta.extensions.remove("kiro_effort_group").unwrap();
        let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "disabled"}})
        );
        meta.extensions.insert("kiro_effort_group".into(), group);
        req.reasoning.level = None;

        req.reasoning.target_control = Some(TargetThinkingControl::Enabled);
        let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "adaptive"}})
        );

        req.reasoning.target_control = Some(TargetThinkingControl::Disabled);
        let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "disabled"}})
        );

        // When thinking is disabled with max effort, KAS 3.4.0 cme caps to highest non-capped level (high).
        req.reasoning.target_control = None;
        req.reasoning.mode = Some("disabled".into());
        req.reasoning.level = Some(ThinkingLevel::Max);
        let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({
                "output_config": {"effort": "high"},
                "thinking": {"type": "disabled"}
            })
        );
        meta.extensions
            .insert("reasoning_efforts".into(), json!(["none", "max"]));
        let body = super::encode(&req, "model", "id", Some(&meta), true).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "disabled"}})
        );
    }

    #[test]
    fn replays_signed_and_redacted_assistant_reasoning_and_drops_unsigned() {
        let req = request(json!([
            {"role":"user","content":"q1"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"step 1","signature":"sig-1"},
                {"type":"text","text":"a1"}
            ]},
            {"role":"user","content":"q2"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"unsigned step"},
                {"type":"text","text":"a2"}
            ]},
            {"role":"user","content":"q3"},
            {"role":"assistant","content":[
                {"type":"redacted_thinking","data":"c2VjcmV0"},
                {"type":"text","text":"a3"}
            ]},
            {"role":"user","content":"q4"}
        ]));
        let body = encode(&req, "model", "id").unwrap();
        let history = body["conversationState"]["history"].as_array().unwrap();
        assert_eq!(
            history[1]["assistantResponseMessage"]["reasoningContent"],
            json!({"reasoningText": {"text": "step 1", "signature": "sig-1"}})
        );
        assert!(
            history[3]["assistantResponseMessage"]
                .get("reasoningContent")
                .is_none()
        );
        assert_eq!(
            history[5]["assistantResponseMessage"]["reasoningContent"],
            json!({"redactedContent": "c2VjcmV0"})
        );
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
