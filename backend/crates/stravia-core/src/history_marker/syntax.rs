use std::collections::HashSet;

use crate::protocol::ir::{AiItem, AiRequest, MessageContent, Role};

use super::*;

pub const HISTORY_MARKER_PREFIX: &str = "<!-- stravia-history-marker:";
const HISTORY_MARKER_SUFFIX: &str = " -->";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkerResolution {
    pub restored_platform_segments: usize,
    pub restored_thinking_segments: usize,
}

pub fn render_history_marker(marker: &HistoryMarker) -> String {
    render_history_marker_reference(&marker.reference)
}

pub(crate) fn render_history_marker_reference(reference: &str) -> String {
    format!("{HISTORY_MARKER_PREFIX}{reference}{HISTORY_MARKER_SUFFIX}\n")
}

fn valid_reference(reference: &str) -> bool {
    reference.strip_prefix("hm_").is_some_and(|opaque| {
        opaque.len() == 20
            && opaque
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

fn strip_markers(text: &str) -> (String, Vec<String>) {
    let mut cleaned = String::with_capacity(text.len());
    let mut references = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find(HISTORY_MARKER_PREFIX) {
        cleaned.push_str(&remaining[..start]);
        let marker = &remaining[start + HISTORY_MARKER_PREFIX.len()..];
        let Some(comment_end) = marker.find(HISTORY_MARKER_SUFFIX) else {
            remaining = marker
                .find('\n')
                .map_or("", |line_end| &marker[line_end + 1..]);
            continue;
        };
        let reference = marker[..comment_end].trim();
        if valid_reference(reference) {
            references.push(reference.to_owned());
        }
        let mut consumed = comment_end + HISTORY_MARKER_SUFFIX.len();
        let after_comment = &marker[consumed..];
        let mut blockquote = after_comment;
        for _ in 0..3 {
            let Some(line) = blockquote.strip_prefix('\n') else {
                break;
            };
            let mut line_end = line.find('\n').unwrap_or(line.len());
            if let Some(next_marker) = line[..line_end].find(HISTORY_MARKER_PREFIX) {
                line_end = next_marker;
            }
            if !line[..line_end].starts_with('>') {
                break;
            }
            consumed += 1 + line_end;
            blockquote = &marker[consumed..];
        }
        remaining = &marker[consumed..];
        if cleaned.ends_with('\n') && remaining.starts_with('\n') {
            remaining = &remaining[1..];
        }
    }
    cleaned.push_str(remaining);
    (cleaned.trim_matches('\n').to_owned(), references)
}

fn item_is_empty(item: &AiItem) -> bool {
    let content_empty = match &item.content {
        MessageContent::Text(text) => text.is_empty(),
        MessageContent::Blocks(blocks) => blocks.is_empty(),
    };
    content_empty && item.tool_calls.as_ref().is_none_or(Vec::is_empty)
}

fn strip_item_markers(item: &mut AiItem) -> Vec<String> {
    let mut references = Vec::new();
    match &mut item.content {
        MessageContent::Text(text) => {
            let (cleaned, found) = strip_markers(text);
            *text = cleaned;
            references = found;
        }
        MessageContent::Blocks(blocks) => {
            for block in blocks.iter_mut() {
                if let ContentBlock::Text { text, .. } = block {
                    let (cleaned, found) = strip_markers(text);
                    *text = cleaned;
                    references.extend(found);
                }
            }
            blocks.retain(
                |block| !matches!(block, ContentBlock::Text { text, .. } if text.is_empty()),
            );
        }
    }
    references
}

pub fn history_marker_references(items: &[AiItem]) -> Vec<String> {
    let mut references = Vec::new();
    for item in items {
        match &item.content {
            MessageContent::Text(text) => references.extend(strip_markers(text).1),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text, .. } = block {
                        references.extend(strip_markers(text).1);
                    }
                }
            }
        }
    }
    references
}

fn mark_restored(mut item: AiItem, provenance: crate::protocol::ir::AiItemProvenance) -> AiItem {
    item.set_graph_metadata(
        None,
        None,
        provenance,
        crate::protocol::ir::AiItemAudience::Internal,
    );
    item.meta
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
        .expect("graph metadata is an object")
        .insert(
            "__stravia_history_marker_restored".into(),
            serde_json::Value::Bool(true),
        );
    item
}

fn segment_items(segment: HiddenHistorySegment) -> (Vec<AiItem>, MarkerResolution) {
    match segment {
        HiddenHistorySegment::Platform { call, result } => {
            let result_item = match result {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    cache_control,
                } => AiItem {
                    role: Role::Tool,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content,
                        is_error,
                        cache_control,
                    }]),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id),
                    meta: None,
                },
                _ => unreachable!("History Marker Store validates Platform terminal results"),
            };
            let call_item = mark_restored(
                AiItem::function_call(call),
                crate::protocol::ir::AiItemProvenance::Platform,
            );
            let result_item =
                mark_restored(result_item, crate::protocol::ir::AiItemProvenance::Platform);
            (
                vec![call_item, result_item],
                MarkerResolution {
                    restored_platform_segments: 1,
                    restored_thinking_segments: 0,
                },
            )
        }
        HiddenHistorySegment::Thinking { block } => {
            let item = AiItem {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![block]),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            };
            let item = mark_restored(item, crate::protocol::ir::AiItemProvenance::Provider);
            (
                vec![item],
                MarkerResolution {
                    restored_platform_segments: 0,
                    restored_thinking_segments: 1,
                },
            )
        }
    }
}

pub async fn resolve_request_markers(
    store: &dyn HistoryMarkerStore,
    principal: &Principal,
    request: &mut AiRequest,
) -> Result<MarkerResolution, HistoryMarkerError> {
    let mut seen = HashSet::new();
    let mut resolved_items = Vec::with_capacity(request.items.len());
    let mut restored_platform = Vec::new();
    let mut first_platform_marker_index = None;
    let mut legacy_duplicate_candidate = None;
    let mut summary = MarkerResolution::default();
    for mut item in std::mem::take(&mut request.items) {
        let references = strip_item_markers(&mut item);
        if references.is_empty()
            && legacy_duplicate_candidate
                .as_ref()
                .is_some_and(|candidate| {
                    crate::protocol::ir::canonical::history_items_equal(
                        std::slice::from_ref(candidate),
                        std::slice::from_ref(&item),
                    )
                })
        {
            legacy_duplicate_candidate = None;
            continue;
        }
        legacy_duplicate_candidate = None;
        let had_markers = !references.is_empty();
        for reference in references {
            if !seen.insert(reference.clone()) {
                continue;
            }
            let Some(marker) = store.resolve(principal, &reference).await? else {
                continue;
            };
            if !marker.published {
                continue;
            }
            let marker = if matches!(
                marker.execution_state,
                Some(PlatformExecutionState::Pending | PlatformExecutionState::Running)
            ) {
                let Some(marker) = store.wait_terminal(principal, &reference).await? else {
                    continue;
                };
                marker
            } else {
                marker
            };
            let Some(segment) = marker.segment else {
                continue;
            };
            let (mut restored, restored_summary) = segment_items(segment);
            summary.restored_platform_segments += restored_summary.restored_platform_segments;
            summary.restored_thinking_segments += restored_summary.restored_thinking_segments;
            if restored_summary.restored_platform_segments > 0 {
                first_platform_marker_index.get_or_insert(resolved_items.len());
                restored_platform.append(&mut restored);
            } else {
                resolved_items.append(&mut restored);
            }
        }
        if !item_is_empty(&item) {
            if had_markers {
                legacy_duplicate_candidate = Some(item.clone());
            }
            resolved_items.push(item);
        }
    }
    if !restored_platform.is_empty() {
        let insertion = resolved_items
            .iter()
            .position(|item| {
                item.function_call_ref().is_some()
                    || item.tool_call_id.is_some()
                    || matches!(item.role, Role::Tool)
            })
            .unwrap_or_else(|| first_platform_marker_index.unwrap_or(resolved_items.len()));
        resolved_items.splice(insertion..insertion, restored_platform);
    }
    request.items = resolved_items;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_reference_is_machine_readable_and_invisible() {
        let marker = HistoryMarker {
            reference: "hm_0123456789abcdefabcd".into(),
            kind: HistoryMarkerKind::Platform,
            activity: "Searching the web".into(),
        };
        let rendered = render_history_marker(&marker);
        assert_eq!(
            rendered,
            "<!-- stravia-history-marker:hm_0123456789abcdefabcd -->\n"
        );
        assert_eq!(
            strip_markers(&rendered),
            (String::new(), vec!["hm_0123456789abcdefabcd".into()])
        );
    }

    #[test]
    fn legacy_visible_marker_block_is_stripped_with_comment() {
        let text = "<!-- stravia-history-marker:hm_0123456789abcdefabcd -->\n\
                    > **Stravia activity:** Searching the web\n\
                    >\n\
                    > **History reference:** `hm_0123456789abcdefabcd`\n\
                    public text";
        assert_eq!(
            strip_markers(text),
            ("public text".into(), vec!["hm_0123456789abcdefabcd".into()])
        );
    }

    #[test]
    fn malformed_private_marker_is_removed() {
        let text = "before\n<!-- stravia-history-marker:not-private\npreserved after";
        let (cleaned, references) = strip_markers(text);
        assert_eq!(cleaned, "before\npreserved after");
        assert!(references.is_empty());
    }
}
