use std::collections::{HashMap, HashSet};

use crate::protocol::ir::{AiItem, AiRequest, CacheControl, MessageContent, Role};

use super::*;

pub const HISTORY_MARKER_PREFIX: &str = "<!-- stravia-history-marker:";
const HISTORY_MARKER_SUFFIX: &str = " -->";
pub const PROJECTION_DELIMITER_PREFIX: &str = "<!-- stravia-projection:";
const PROJECTION_DELIMITER_SUFFIX: &str = " -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionMode {
    Text,
    Preview,
}

impl ProjectionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Preview => "preview",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionBoundary {
    Start,
    End,
}

impl ProjectionBoundary {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::End => "end",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionDelimiter {
    reference: String,
    ordinal: usize,
    mode: ProjectionMode,
    boundary: ProjectionBoundary,
}

#[derive(Debug, Clone)]
enum PrivateToken {
    Visible(String),
    Marker(String),
    Delimiter(ProjectionDelimiter),
}

#[derive(Debug, Clone)]
enum ScalarKind {
    Text(Option<CacheControl>),
    Thinking,
    ReasoningSummary,
    ReasoningContent,
}

#[derive(Debug, Clone)]
enum CarrierAtom {
    Visible(ContentBlock),
    Marker(String),
    Projection {
        reference: String,
        mode: ProjectionMode,
        source: ContentBlock,
    },
}

enum ParsedItem {
    Unchanged(AiItem),
    Parsed {
        original: AiItem,
        atoms: Vec<CarrierAtom>,
        had_marker: bool,
    },
}

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

fn render_projection_delimiter(
    reference: &str,
    ordinal: usize,
    mode: ProjectionMode,
    boundary: ProjectionBoundary,
) -> String {
    format!(
        "{PROJECTION_DELIMITER_PREFIX}{reference}:{}:{ordinal}:{}{PROJECTION_DELIMITER_SUFFIX}",
        mode.as_str(),
        boundary.as_str()
    )
}

fn render_projection_span(
    reference: &str,
    ordinal: usize,
    mode: ProjectionMode,
    visible: &str,
) -> String {
    format!(
        "{}{visible}{}",
        render_projection_delimiter(reference, ordinal, mode, ProjectionBoundary::Start),
        render_projection_delimiter(reference, ordinal, mode, ProjectionBoundary::End)
    )
}

pub(crate) fn render_text_projection_span(
    reference: &str,
    ordinal: usize,
    visible: &str,
) -> String {
    render_projection_span(reference, ordinal, ProjectionMode::Text, visible)
}

pub(crate) fn render_text_projection_start(reference: &str, ordinal: usize) -> String {
    render_projection_delimiter(
        reference,
        ordinal,
        ProjectionMode::Text,
        ProjectionBoundary::Start,
    )
}

pub(crate) fn render_text_projection_end(reference: &str, ordinal: usize) -> String {
    render_projection_delimiter(
        reference,
        ordinal,
        ProjectionMode::Text,
        ProjectionBoundary::End,
    )
}

pub(crate) fn render_preview_projection_span(
    reference: &str,
    ordinal: usize,
    visible: &str,
) -> String {
    render_projection_span(reference, ordinal, ProjectionMode::Preview, visible)
}

fn valid_reference(reference: &str) -> bool {
    reference.strip_prefix("hm_").is_some_and(|opaque| {
        opaque.len() == 20
            && opaque
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

fn parse_projection_delimiter(payload: &str) -> Option<ProjectionDelimiter> {
    let mut parts = payload.trim().split(':');
    let reference = parts.next()?;
    let mode = match parts.next()? {
        "text" => ProjectionMode::Text,
        "preview" => ProjectionMode::Preview,
        _ => return None,
    };
    let ordinal = parts.next()?.parse().ok()?;
    let boundary = match parts.next()? {
        "start" => ProjectionBoundary::Start,
        "end" => ProjectionBoundary::End,
        _ => return None,
    };
    if parts.next().is_some() || !valid_reference(reference) {
        return None;
    }
    Some(ProjectionDelimiter {
        reference: reference.to_owned(),
        ordinal,
        mode,
        boundary,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateSyntaxKind {
    HistoryMarker,
    ProjectionDelimiter,
}

fn next_private_syntax(text: &str) -> Option<(usize, PrivateSyntaxKind)> {
    match (
        text.find(HISTORY_MARKER_PREFIX),
        text.find(PROJECTION_DELIMITER_PREFIX),
    ) {
        (Some(marker), Some(delimiter)) if marker <= delimiter => {
            Some((marker, PrivateSyntaxKind::HistoryMarker))
        }
        (Some(_), Some(delimiter)) => Some((delimiter, PrivateSyntaxKind::ProjectionDelimiter)),
        (Some(marker), None) => Some((marker, PrivateSyntaxKind::HistoryMarker)),
        (None, Some(delimiter)) => Some((delimiter, PrivateSyntaxKind::ProjectionDelimiter)),
        (None, None) => None,
    }
}

fn consume_malformed_private_syntax(text: &str) -> &str {
    text.find('\n').map_or("", |line_end| &text[line_end + 1..])
}

fn legacy_marker_decoration_len(text: &str, reference: &str) -> Option<usize> {
    let after_newline = text.strip_prefix('\n')?;
    let first_line_end = after_newline.find('\n')?;
    let activity = &after_newline[..first_line_end];
    if !activity.starts_with("> **Stravia activity:** ") {
        return None;
    }
    let after_activity = &after_newline[first_line_end + 1..];
    let after_spacer = after_activity.strip_prefix(">\n")?;
    let reference_line = format!("> **History reference:** `{reference}`");
    if !after_spacer.starts_with(&reference_line) {
        return None;
    }
    let after_reference = &after_spacer[reference_line.len()..];
    if !after_reference.is_empty() && !after_reference.starts_with('\n') {
        return None;
    }
    let mut consumed = 1 + first_line_end + 1 + 2 + reference_line.len();
    if after_reference.starts_with('\n') {
        consumed += 1;
    }
    Some(consumed)
}

fn tokenize_private_syntax(text: &str) -> Vec<PrivateToken> {
    let mut tokens = Vec::new();
    let mut remaining = text;
    while let Some((start, syntax_kind)) = next_private_syntax(remaining) {
        if start > 0 {
            tokens.push(PrivateToken::Visible(remaining[..start].to_owned()));
        }
        if syntax_kind == PrivateSyntaxKind::HistoryMarker {
            let payload = &remaining[start + HISTORY_MARKER_PREFIX.len()..];
            let Some(comment_end) = payload.find(HISTORY_MARKER_SUFFIX) else {
                remaining = consume_malformed_private_syntax(payload);
                continue;
            };
            let reference = payload[..comment_end].trim();
            if valid_reference(reference) {
                tokens.push(PrivateToken::Marker(reference.to_owned()));
            }
            let mut consumed = comment_end + HISTORY_MARKER_SUFFIX.len();
            let after_comment = &payload[consumed..];
            if let Some(legacy_len) = legacy_marker_decoration_len(after_comment, reference) {
                consumed += legacy_len;
            } else if after_comment.starts_with('\n') {
                consumed += 1;
            }
            remaining = &payload[consumed..];
            continue;
        }

        let payload = &remaining[start + PROJECTION_DELIMITER_PREFIX.len()..];
        let Some(comment_end) = payload.find(PROJECTION_DELIMITER_SUFFIX) else {
            remaining = consume_malformed_private_syntax(payload);
            continue;
        };
        if let Some(delimiter) = parse_projection_delimiter(&payload[..comment_end]) {
            tokens.push(PrivateToken::Delimiter(delimiter));
        }
        remaining = &payload[comment_end + PROJECTION_DELIMITER_SUFFIX.len()..];
    }
    if !remaining.is_empty() {
        tokens.push(PrivateToken::Visible(remaining.to_owned()));
    }
    tokens
}

fn scalar_block(kind: &ScalarKind, text: String) -> ContentBlock {
    match kind {
        ScalarKind::Text(cache_control) => ContentBlock::Text {
            text,
            cache_control: cache_control.clone(),
        },
        ScalarKind::Thinking => ContentBlock::Thinking {
            thinking: text,
            signature: None,
        },
        ScalarKind::ReasoningSummary => ContentBlock::Reasoning {
            summary: vec![text],
            content: Vec::new(),
            encrypted_content: None,
        },
        ScalarKind::ReasoningContent => ContentBlock::Reasoning {
            summary: Vec::new(),
            content: vec![text],
            encrypted_content: None,
        },
    }
}

fn parse_scalar(text: &str, kind: ScalarKind) -> Vec<CarrierAtom> {
    let tokens = tokenize_private_syntax(text);
    let mut atoms = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index] {
            PrivateToken::Visible(text) => {
                if !text.is_empty() {
                    atoms.push(CarrierAtom::Visible(scalar_block(&kind, text.clone())));
                }
                index += 1;
            }
            PrivateToken::Marker(reference) => {
                atoms.push(CarrierAtom::Marker(reference.clone()));
                index += 1;
            }
            PrivateToken::Delimiter(start)
                if start.boundary == ProjectionBoundary::Start
                    && index + 2 < tokens.len()
                    && matches!(&tokens[index + 1], PrivateToken::Visible(_))
                    && matches!(
                        &tokens[index + 2],
                        PrivateToken::Delimiter(end)
                            if end.boundary == ProjectionBoundary::End
                                && end.reference == start.reference
                                && end.ordinal == start.ordinal
                                && end.mode == start.mode
                    ) =>
            {
                let PrivateToken::Visible(text) = &tokens[index + 1] else {
                    unreachable!("projection pair requires a visible body");
                };
                atoms.push(CarrierAtom::Projection {
                    reference: start.reference.clone(),
                    mode: start.mode,
                    source: scalar_block(&kind, text.clone()),
                });
                index += 3;
            }
            PrivateToken::Delimiter(_) => {
                index += 1;
            }
        }
    }
    atoms
}

fn strip_markers(text: &str) -> (String, Vec<String>) {
    let mut cleaned = String::with_capacity(text.len());
    let mut references = Vec::new();
    for token in tokenize_private_syntax(text) {
        match token {
            PrivateToken::Visible(text) => cleaned.push_str(&text),
            PrivateToken::Marker(reference) => references.push(reference),
            PrivateToken::Delimiter(_) => {}
        }
    }
    (cleaned.trim_matches('\n').to_owned(), references)
}

pub fn history_marker_references(items: &[AiItem]) -> Vec<String> {
    let mut references = Vec::new();
    for item in items {
        match &item.content {
            MessageContent::Text(text) => references.extend(strip_markers(text).1),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text, .. }
                        | ContentBlock::Thinking {
                            thinking: text,
                            signature: None,
                        } => references.extend(strip_markers(text).1),
                        ContentBlock::Reasoning {
                            summary,
                            content,
                            encrypted_content: None,
                        } => {
                            for text in summary.iter().chain(content) {
                                references.extend(strip_markers(text).1);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    references
}

fn mark_restored(
    mut item: AiItem,
    provenance: crate::protocol::ir::AiItemProvenance,
    reference: &str,
) -> AiItem {
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
    item.meta
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
        .expect("graph metadata is an object")
        .insert(
            "__stravia_history_marker_reference".into(),
            serde_json::Value::String(reference.to_owned()),
        );
    item
}

fn segment_items(
    segment: HiddenHistorySegment,
    reference: &str,
) -> (Vec<AiItem>, MarkerResolution) {
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
                reference,
            );
            let result_item = mark_restored(
                result_item,
                crate::protocol::ir::AiItemProvenance::Platform,
                reference,
            );
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
            let item = mark_restored(
                item,
                crate::protocol::ir::AiItemProvenance::Provider,
                reference,
            );
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

fn contains_private_syntax(text: &str) -> bool {
    text.contains(HISTORY_MARKER_PREFIX) || text.contains(PROJECTION_DELIMITER_PREFIX)
}

fn parse_item(item: AiItem) -> ParsedItem {
    let mut atoms = Vec::new();
    let mut changed = false;
    match &item.content {
        MessageContent::Text(text) => {
            if contains_private_syntax(text) {
                changed = true;
                atoms.extend(parse_scalar(text, ScalarKind::Text(None)));
            }
        }
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text {
                        text,
                        cache_control,
                    } if contains_private_syntax(text) => {
                        changed = true;
                        atoms.extend(parse_scalar(text, ScalarKind::Text(cache_control.clone())));
                    }
                    ContentBlock::Thinking {
                        thinking,
                        signature: None,
                    } if contains_private_syntax(thinking) => {
                        changed = true;
                        atoms.extend(parse_scalar(thinking, ScalarKind::Thinking));
                    }
                    ContentBlock::Reasoning {
                        summary,
                        content,
                        encrypted_content: None,
                    } if summary
                        .iter()
                        .chain(content)
                        .any(|text| contains_private_syntax(text)) =>
                    {
                        changed = true;
                        for text in summary {
                            atoms.extend(parse_scalar(text, ScalarKind::ReasoningSummary));
                        }
                        for text in content {
                            atoms.extend(parse_scalar(text, ScalarKind::ReasoningContent));
                        }
                    }
                    _ => atoms.push(CarrierAtom::Visible(block.clone())),
                }
            }
        }
    }
    if !changed {
        return ParsedItem::Unchanged(item);
    }
    let had_marker = atoms
        .iter()
        .any(|atom| matches!(atom, CarrierAtom::Marker(_)));
    ParsedItem::Parsed {
        original: item,
        atoms,
        had_marker,
    }
}

fn projection_source_text(source: &ContentBlock) -> &str {
    match source {
        ContentBlock::Text { text, .. } => text,
        ContentBlock::Thinking { thinking, .. } => thinking,
        ContentBlock::Reasoning {
            summary, content, ..
        } => summary
            .first()
            .or_else(|| content.first())
            .map(String::as_str)
            .unwrap_or_default(),
        _ => unreachable!("Projection Delimiters only wrap textual carriers"),
    }
}

fn legacy_cleaned_item(original: &AiItem, atoms: &[CarrierAtom]) -> Option<AiItem> {
    let visible = atoms
        .iter()
        .filter_map(|atom| match atom {
            CarrierAtom::Visible(block) | CarrierAtom::Projection { source: block, .. } => {
                Some(block.clone())
            }
            CarrierAtom::Marker(_) => None,
        })
        .collect::<Vec<_>>();
    let has_tools = original
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
        || original.tool_call_id.is_some();
    if visible.is_empty() && !has_tools {
        return None;
    }
    let content = if matches!(original.content, MessageContent::Text(_))
        && visible
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. }))
    {
        MessageContent::Text(
            visible
                .iter()
                .filter_map(ContentBlock::as_text)
                .collect::<String>(),
        )
    } else {
        MessageContent::Blocks(visible)
    };
    Some(AiItem {
        role: original.role.clone(),
        content,
        tool_calls: original.tool_calls.clone(),
        tool_call_id: original.tool_call_id.clone(),
        meta: original.meta.clone(),
    })
}

fn client_fragment(
    original: &AiItem,
    block: ContentBlock,
    meta: &mut Option<serde_json::Value>,
) -> AiItem {
    AiItem {
        role: original.role.clone(),
        content: MessageContent::Blocks(vec![block]),
        tool_calls: None,
        tool_call_id: None,
        meta: meta.take(),
    }
}

async fn cached_marker(
    store: &dyn HistoryMarkerStore,
    principal: &Principal,
    cache: &mut HashMap<String, Option<ResolvedHistoryMarker>>,
    reference: &str,
) -> Result<Option<ResolvedHistoryMarker>, HistoryMarkerError> {
    if let Some(marker) = cache.get(reference) {
        return Ok(marker.clone());
    }
    let marker = store.resolve(principal, reference).await?;
    cache.insert(reference.to_owned(), marker.clone());
    Ok(marker)
}

async fn materialize_parsed_item(
    store: &dyn HistoryMarkerStore,
    principal: &Principal,
    original: AiItem,
    atoms: Vec<CarrierAtom>,
    request_marker_references: &HashSet<String>,
    seen: &mut HashSet<String>,
    cache: &mut HashMap<String, Option<ResolvedHistoryMarker>>,
    resolved_items: &mut Vec<AiItem>,
    summary: &mut MarkerResolution,
) -> Result<(), HistoryMarkerError> {
    let mut meta = original.meta.clone();
    for atom in atoms {
        match atom {
            CarrierAtom::Visible(block) => {
                resolved_items.push(client_fragment(&original, block, &mut meta));
            }
            CarrierAtom::Projection {
                reference,
                mode,
                source,
            } => {
                let marker = cached_marker(store, principal, cache, &reference)
                    .await?
                    .filter(|marker| marker.published);
                match (mode, marker) {
                    (ProjectionMode::Text, Some(_))
                        if request_marker_references.contains(&reference) =>
                    {
                        resolved_items.push(client_fragment(
                            &original,
                            ContentBlock::Text {
                                text: projection_source_text(&source).to_owned(),
                                cache_control: None,
                            },
                            &mut meta,
                        ));
                    }
                    (ProjectionMode::Preview, Some(marker))
                        if marker.marker.kind == HistoryMarkerKind::Thinking
                            && request_marker_references.contains(&reference) => {}
                    _ => {
                        resolved_items.push(client_fragment(&original, source, &mut meta));
                    }
                }
            }
            CarrierAtom::Marker(reference) => {
                if !seen.insert(reference.clone()) {
                    continue;
                }
                let Some(mut marker) = cached_marker(store, principal, cache, &reference).await?
                else {
                    continue;
                };
                if !marker.published {
                    continue;
                }
                if matches!(
                    marker.execution_state,
                    Some(PlatformExecutionState::Pending | PlatformExecutionState::Running)
                ) {
                    let Some(terminal) = store.wait_terminal(principal, &reference).await? else {
                        continue;
                    };
                    cache.insert(reference.clone(), Some(terminal.clone()));
                    marker = terminal;
                }
                let Some(segment) = marker.segment else {
                    continue;
                };
                let (mut restored, restored_summary) = segment_items(segment, &reference);
                summary.restored_platform_segments += restored_summary.restored_platform_segments;
                summary.restored_thinking_segments += restored_summary.restored_thinking_segments;
                resolved_items.append(&mut restored);
            }
        }
    }

    let has_tool_calls = original
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if has_tool_calls || original.tool_call_id.is_some() {
        resolved_items.push(AiItem {
            role: original.role,
            content: MessageContent::Text(String::new()),
            tool_calls: has_tool_calls.then_some(original.tool_calls.unwrap_or_default()),
            tool_call_id: original.tool_call_id,
            meta,
        });
    }
    Ok(())
}

pub async fn resolve_request_markers(
    store: &dyn HistoryMarkerStore,
    principal: &Principal,
    request: &mut AiRequest,
) -> Result<MarkerResolution, HistoryMarkerError> {
    let parsed_items = std::mem::take(&mut request.items)
        .into_iter()
        .map(parse_item)
        .collect::<Vec<_>>();
    let request_marker_references = parsed_items
        .iter()
        .flat_map(|item| match item {
            ParsedItem::Unchanged(_) => Vec::new(),
            ParsedItem::Parsed { atoms, .. } => atoms
                .iter()
                .filter_map(|atom| match atom {
                    CarrierAtom::Marker(reference) => Some(reference.clone()),
                    _ => None,
                })
                .collect(),
        })
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut cache = HashMap::new();
    let mut resolved_items = Vec::with_capacity(parsed_items.len());
    let mut legacy_duplicate_candidate: Option<AiItem> = None;
    let mut summary = MarkerResolution::default();
    for parsed in parsed_items {
        let original = match &parsed {
            ParsedItem::Unchanged(item) | ParsedItem::Parsed { original: item, .. } => item,
        };
        if legacy_duplicate_candidate
            .as_ref()
            .is_some_and(|candidate| {
                crate::protocol::ir::canonical::history_items_equal(
                    std::slice::from_ref(candidate),
                    std::slice::from_ref(original),
                )
            })
        {
            legacy_duplicate_candidate = None;
            continue;
        }
        legacy_duplicate_candidate = None;
        match parsed {
            ParsedItem::Unchanged(item) => resolved_items.push(item),
            ParsedItem::Parsed {
                original,
                atoms,
                had_marker,
            } => {
                if had_marker {
                    legacy_duplicate_candidate = legacy_cleaned_item(&original, &atoms);
                }
                materialize_parsed_item(
                    store,
                    principal,
                    original,
                    atoms,
                    &request_marker_references,
                    &mut seen,
                    &mut cache,
                    &mut resolved_items,
                    &mut summary,
                )
                .await?;
            }
        }
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
    fn marker_preserves_extra_newlines_and_public_blockquotes() {
        let marker = "<!-- stravia-history-marker:hm_0123456789abcdefabcd -->";
        assert_eq!(
            strip_markers(&format!("before{marker}\n\npublic text")).0,
            "before\npublic text"
        );
        assert_eq!(
            strip_markers(&format!("before\n{marker}\n> cited evidence")).0,
            "before\n> cited evidence"
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
