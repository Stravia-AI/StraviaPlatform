use stravia_runtime_contract::protocol::ir::{
    AiItem, AiRequest, ContentBlock, MessageContent, Role,
};

/// 准备调用方持有的请求副本，不改写权威历史与严格编码行为。
/// 调用方逐个决定条目中的受保护载荷能否回放到当前 Target。
///
/// 宿主只负责来源约束：不可回放的签名、`encrypted_content` 与 redacted 块在此剥离。
/// 明文思考保持为思考块，由出口 codec 决定原生承载还是降级为正文；受保护载荷
/// 若出口协议无法承载，也由 codec 忽略。这里不按协议分支，插件协议同样适用。
/// 未改写的正文继续共享载荷；降级的可读推理移入新正文块。
pub fn prepare_thinking_replay(
    request: &mut AiRequest,
    preserve_protected: impl Fn(&AiItem, &ContentBlock) -> bool,
) -> bool {
    prepare_thinking_replay_with_removals(request, preserve_protected, |_| {})
}

/// Report removed item indices in the original request, in chronological order.
pub fn prepare_thinking_replay_with_removals(
    request: &mut AiRequest,
    preserve_protected: impl Fn(&AiItem, &ContentBlock) -> bool,
    mut removed: impl FnMut(usize),
) -> bool {
    let mut changed = false;
    let mut index = 0;
    request.items.retain_mut(|item| {
        let original_index = index;
        index += 1;
        let MessageContent::Blocks(blocks) = &item.content else {
            return true;
        };
        if !blocks.iter().any(is_thinking) {
            return true;
        }
        let assistant = item.role == Role::Assistant;
        // 判定时保留完整条目供宿主核对来源；只记录需要剥离的位置，不复制载荷。
        let mut stripped = Vec::new();
        let mut protected_retained = false;
        if assistant {
            for (index, block) in blocks
                .iter()
                .enumerate()
                .filter(|(_, block)| is_protected(block))
            {
                if preserve_protected(item, block) {
                    protected_retained = true;
                } else {
                    stripped.push(index);
                }
            }
        }
        if assistant && stripped.is_empty() {
            return true;
        }
        let thinking_only = blocks.iter().all(is_thinking)
            && item.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && item.tool_call_id.is_none();
        let MessageContent::Blocks(blocks) = &mut item.content else {
            unreachable!();
        };
        let mut replay = Vec::with_capacity(blocks.len());
        let mut stripped = stripped.into_iter().peekable();
        for (index, block) in blocks.drain(..).enumerate() {
            if assistant {
                if stripped.peek() != Some(&index) {
                    replay.push(block);
                    continue;
                }
                stripped.next();
            }
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
        // 原生条目字段可能仍服务于保留的本上游载荷，只有全部剥离后才移除。
        // 混合普通内容/工具的条目可能携带硬字段：保持严格。
        if thinking_only
            && !protected_retained
            && let Some(meta) = item.meta.as_mut()
        {
            meta.remove_extension(NATIVE_ITEM_FIELDS)
                .expect("native item fields key is not reserved");
        }
        changed = true;
        let keep = !(assistant && blocks.is_empty() && thinking_only);
        if !keep {
            removed(original_index);
        }
        keep
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
