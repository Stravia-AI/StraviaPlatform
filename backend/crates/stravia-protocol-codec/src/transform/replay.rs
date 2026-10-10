use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, ContentBlock, MessageContent, Role,
};

/// Prepare a caller-owned request copy; canonical history and strict encoding stay untouched.
/// The caller decides whether each item's protected payload may be replayed to this Target.
///
/// 宿主只负责来源约束：不可回放的签名、`encrypted_content` 与 redacted 块在此剥离。
/// 明文思考保持为思考块，由出口 codec 决定原生承载还是降级为正文；受保护载荷
/// 若出口协议无法承载，也由 codec 忽略。这里不按协议分支，插件协议同样适用。
/// Untouched text blocks retain their shared payload; downgraded reasoning moves into new text.
pub fn prepare_thinking_replay(
    request: &mut AiRequest,
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
        let assistant = item.role == Role::Assistant;
        let preserve = assistant && preserve_protected(item);
        if preserve {
            return true;
        }
        let thinking_only = blocks.iter().all(is_thinking)
            && item.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && item.tool_call_id.is_none();
        let native_fields = thinking_only
            && item
                .meta
                .as_ref()
                .is_some_and(|meta| meta.get(NATIVE_ITEM_FIELDS).is_some());
        if assistant && !native_fields && !blocks.iter().any(is_protected) {
            return true;
        }
        let MessageContent::Blocks(blocks) = &mut item.content else {
            unreachable!();
        };
        let mut replay = Vec::with_capacity(blocks.len());
        for block in blocks.drain(..) {
            match block {
                // 没有协议承载非 assistant 角色的推理，只能作为普通文本保留。
                ContentBlock::Thinking { thinking, .. } if !assistant => {
                    push_text(&mut replay, thinking);
                }
                ContentBlock::Reasoning {
                    summary, content, ..
                } if !assistant => {
                    for text in summary.into_iter().chain(content) {
                        push_text(&mut replay, text);
                    }
                }
                ContentBlock::Thinking { thinking, .. } => {
                    if !thinking.is_empty() {
                        replay.push(ContentBlock::Thinking {
                            thinking,
                            signature: None,
                        });
                    }
                }
                ContentBlock::Reasoning {
                    summary, content, ..
                } => {
                    if summary.iter().chain(&content).any(|text| !text.is_empty()) {
                        replay.push(ContentBlock::Reasoning {
                            summary,
                            content,
                            encrypted_content: None,
                        });
                    }
                }
                ContentBlock::RedactedThinking { .. } => {}
                other => replay.push(other),
            }
        }
        *blocks = replay;
        if !blocks.iter().any(is_thinking)
            && let Some(meta) = item.meta.as_mut()
        {
            for key in ["reasoning_content", "reasoning", "reasoning_text"] {
                meta.remove_extension(key)
                    .expect("reasoning keys are not reserved");
            }
        }
        // 原生条目字段（如 Responses reasoning id）指向来源上游保存的推理，与签名同样绑定来源。
        // 混合条目可能携带普通内容/工具的硬字段：保持严格。
        if thinking_only && let Some(meta) = item.meta.as_mut() {
            meta.remove_extension(NATIVE_ITEM_FIELDS)
                .expect("native item fields key is not reserved");
        }
        changed = true;
        !(assistant && blocks.is_empty() && thinking_only)
    });
    changed
}

const NATIVE_ITEM_FIELDS: &str = "__open_responses_item_fields";

fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::RedactedThinking { .. }
    )
}

fn is_protected(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking {
            signature: Some(_),
            ..
        } | ContentBlock::Reasoning {
            encrypted_content: Some(_),
            ..
        } | ContentBlock::RedactedThinking { .. }
    )
}

fn push_text(blocks: &mut Vec<ContentBlock>, text: String) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: text.into(),
            cache_control: None,
        });
    }
}
