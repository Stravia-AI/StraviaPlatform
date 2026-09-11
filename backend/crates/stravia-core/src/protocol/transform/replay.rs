use stravia_runtime_contract::protocol::ids::{Protocol, ProtocolEndpoint};
use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, ContentBlock, MessageContent, Role,
};

/// Prepare a caller-owned request copy; canonical history and strict encoding stay untouched.
/// The caller decides whether each item's protected payload belongs to this exact Target.
pub(crate) fn prepare_thinking_replay(
    request: &mut AiRequest,
    target: ProtocolEndpoint,
    preserve_protected: impl Fn(&AiItem) -> bool,
) -> bool {
    let mut changed = false;
    request.items.retain_mut(|item| {
        let MessageContent::Blocks(blocks) = &item.content else {
            return true;
        };
        if !blocks.iter().any(is_thinking) {
            return true;
        }
        let preserve = preserve_protected(item);
        if !blocks
            .iter()
            .any(|block| needs_replay(block, item.role, target.protocol, preserve))
        {
            return true;
        }
        let thinking_only = blocks.iter().all(is_thinking)
            && item.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && item.tool_call_id.is_none();
        let MessageContent::Blocks(blocks) = &mut item.content else {
            unreachable!();
        };
        let mut replay = Vec::with_capacity(blocks.len());
        for block in blocks.drain(..) {
            if !needs_replay(&block, item.role, target.protocol, preserve) {
                replay.push(block);
                continue;
            }
            match block {
                ContentBlock::Thinking { thinking, .. } => {
                    push_readable(&mut replay, thinking);
                }
                ContentBlock::Reasoning {
                    summary, content, ..
                } => {
                    for text in summary.into_iter().chain(content) {
                        push_readable(&mut replay, text);
                    }
                }
                ContentBlock::RedactedThinking { .. } => {}
                _ => unreachable!("only thinking blocks need replay"),
            }
        }
        *blocks = replay;
        if !blocks.iter().any(is_thinking)
            && let Some(meta) = item
                .meta
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
        {
            for key in ["reasoning_content", "reasoning", "reasoning_text"] {
                meta.remove(key);
            }
        }
        // These extras belong to the original native reasoning item, not its text fallback.
        // Mixed items may carry hard fields for ordinary content/tools: keep those strict.
        if thinking_only
            && let Some(meta) = item
                .meta
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
        {
            meta.remove("__open_responses_item_fields");
        }
        changed = true;
        !(item.role == Role::Assistant && blocks.is_empty() && thinking_only)
    });

    // The Responses encoder recognizes native Reasoning only as a standalone item. Split
    // mixed native items rather than dropping tools or flattening summary/content boundaries.
    if target.protocol == Protocol::OpenResponses {
        let mut index = 0;
        while index < request.items.len() {
            let item = &request.items[index];
            let MessageContent::Blocks(blocks) = &item.content else {
                index += 1;
                continue;
            };
            let mixed = blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Reasoning { .. }))
                && (blocks.len() > 1
                    || item
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty()));
            if !mixed {
                index += 1;
                continue;
            }
            let mut item = request.items.remove(index);
            let MessageContent::Blocks(blocks) = &mut item.content else {
                unreachable!()
            };
            let mut ordinary = Vec::new();
            let mut parts = Vec::new();
            for block in std::mem::take(blocks) {
                if matches!(block, ContentBlock::Reasoning { .. }) {
                    if !ordinary.is_empty() {
                        parts.push(AiItem {
                            content: MessageContent::Blocks(std::mem::take(&mut ordinary)),
                            role: item.role,
                            tool_calls: None,
                            tool_call_id: item.tool_call_id.clone(),
                            meta: item.meta.clone(),
                        });
                    }
                    parts.push(AiItem {
                        content: MessageContent::Blocks(vec![block]),
                        role: item.role,
                        tool_calls: None,
                        tool_call_id: item.tool_call_id.clone(),
                        meta: item.meta.clone(),
                    });
                } else {
                    ordinary.push(block);
                }
            }
            if !ordinary.is_empty()
                || item
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
            {
                item.content = MessageContent::Blocks(ordinary);
                parts.push(item);
            }
            let count = parts.len();
            request.items.splice(index..index, parts);
            index += count;
            changed = true;
        }
    }
    changed
}

fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::RedactedThinking { .. }
    )
}

fn needs_replay(block: &ContentBlock, role: Role, target: Protocol, preserve: bool) -> bool {
    match block {
        ContentBlock::Thinking { signature, .. } => {
            if role != Role::Assistant {
                return true;
            }
            match target {
                Protocol::AnthropicMessages
                | Protocol::BedrockConverse
                | Protocol::OpenResponses => {
                    !(preserve
                        && signature
                            .as_ref()
                            .is_some_and(|signature| !signature.trim().is_empty()))
                }
                Protocol::GoogleGemini => !preserve,
                Protocol::OpenAICompatible
                | Protocol::WatsonxTextChat
                | Protocol::CohereChat
                | Protocol::GatewayLanguageModel => !preserve || signature.is_some(),
            }
        }
        ContentBlock::Reasoning { .. } => {
            !(preserve && role == Role::Assistant && target == Protocol::OpenResponses)
        }
        ContentBlock::RedactedThinking { .. } => {
            !(preserve && role == Role::Assistant && target == Protocol::AnthropicMessages)
        }
        _ => false,
    }
}

fn push_readable(blocks: &mut Vec<ContentBlock>, text: String) {
    if text.is_empty() {
        return;
    }
    // 某些上游接受外国 thinking 字段却不将其放入上下文；普通文本才能保留可读内容。
    blocks.push(ContentBlock::Text {
        text,
        cache_control: None,
    });
}
