//! 将索引事件区间映射为可读取的历史路径与安全摘要。

use std::ops::Range;

use stravia_runtime_contract::hook::PlatformToolError;
use stravia_web_access_contract::read_path::HISTORY_ROOT;

use crate::mcp::read::text::PAGE_BYTES;

use super::{
    Recall, RecallInteraction, Transcript,
    page::ChunkMark,
    render,
    search_text::{self, ParsedQuery},
    storage,
};

/// 摘要总字符预算，包含每个命中词两侧的 `«»` 标记。
const SNIPPET_CHARS: usize = 160;

/// 渲染命中时转录与索引不再对应：检索后该轮被清除或改写。检索结果本身不带游标，
/// 不能报游标失效。
fn changed_during_search() -> PlatformToolError {
    PlatformToolError::new("History changed while rendering search results; search again")
}

/// 按索引使用的规范词元查找查询词，并保留原文偏移。
fn matches(text: &str, query: &ParsedQuery) -> Vec<Range<usize>> {
    let tokens = search_text::tokenize(text);
    let mut ranges = Vec::new();
    for term in &query.terms {
        let n = term.tokens.len();
        if n == 0 || n > tokens.len() {
            continue;
        }
        for window in tokens.windows(n) {
            if window
                .iter()
                .zip(&term.tokens)
                .enumerate()
                .all(|(i, (actual, wanted))| {
                    if term.prefix && i + 1 == n {
                        actual.text.starts_with(&wanted.text)
                    } else {
                        actual.text == wanted.text
                    }
                })
            {
                ranges.push(window[0].range.start..window[n - 1].range.end);
            }
        }
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges
}

/// 摘要只取自渲染后（已替换凭据）的文本，不读取索引所用的未脱敏文本。
/// 嵌入式换行折叠为空格，保证检索结果每行只含一个 Markdown 列表项。
fn snippet(text: &str, query: &ParsedQuery) -> String {
    let text = text.replace(['\r', '\n'], " ");
    let mut ranges: Vec<Range<usize>> = Vec::new();
    // 重叠命中合并为一个标记，互不重叠的命中各自保留标记。
    for range in matches(&text, query) {
        if let Some(last) = ranges.last_mut()
            && range.start < last.end
        {
            last.end = last.end.max(range.end);
        } else {
            ranges.push(range);
        }
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let focus = ranges.first().map_or(0, |range| range.start);
    let focus_char = chars.partition_point(|(byte, _)| *byte < focus);
    let focus_len = ranges
        .first()
        .map_or(0, |range| text[range.clone()].chars().count());
    let leading = (SNIPPET_CHARS - 2 - focus_len.min(SNIPPET_CHARS - 2)) / 2;
    // 围绕命中词取窗口，在字符边界裁切，标记字符也计入预算。
    let start = focus_char.saturating_sub(leading);
    let end = (start + SNIPPET_CHARS - 2).min(chars.len());
    let start_byte = chars.get(start).map_or(text.len(), |(byte, _)| *byte);
    let end_byte = chars.get(end).map_or(text.len(), |(byte, _)| *byte);
    let mut output = String::new();
    let mut cursor = start_byte;
    let mut used = 0;
    for range in ranges {
        if range.start < cursor {
            continue;
        }
        if range.start >= end_byte {
            break;
        }
        let before = &text[cursor..range.start];
        let before_len = before.chars().count();
        if used + before_len + 2 >= SNIPPET_CHARS {
            output.extend(before.chars().take(SNIPPET_CHARS - used));
            return output;
        }
        output.push_str(before);
        output.push('«');
        let available = SNIPPET_CHARS - used - before_len - 2;
        let matched = &text[range.start..range.end.min(end_byte)];
        let matched_len = matched.chars().count();
        output.extend(matched.chars().take(available));
        output.push('»');
        used += before_len + matched_len.min(available) + 2;
        if range.end > end_byte || matched_len > available {
            return output;
        }
        cursor = range.end;
    }
    output.extend(text[cursor..end_byte].chars().take(SNIPPET_CHARS - used));
    output
}

/// 摘要聚焦的首个脱敏命中是否在其渲染行的第一页之后。
/// 无可见命中（例如命中词已被凭据标记替换）时不报告。
fn hit_after_first_page(text: &str, query: &ParsedQuery) -> bool {
    // 与摘要一样先折叠换行；每个替换均为单字节，偏移仍对应原文。
    let flattened = text.replace(['\r', '\n'], " ");
    let Some(hit) = matches(&flattened, query).into_iter().next() else {
        return false;
    };
    let line_start = text[..hit.start].rfind('\n').map_or(0, |index| index + 1);
    let line = text[line_start..].split('\n').next().unwrap_or_default();
    if line.len() <= PAGE_BYTES {
        return false;
    }
    let mut cut = PAGE_BYTES;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    hit.start - line_start >= cut
}

/// 计算字节位置在正文中的行偏移；只用于按 LF 切行的渲染文本。
fn line_offset(text: &str, byte: usize) -> u64 {
    text.as_bytes()[..byte]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u64
}

/// 命中词所在的行范围；事件正文按 LF 切行，因此范围在转录追加事件后仍稳定。
fn line_window(text: &str, start: usize, end: usize) -> (u64, u64) {
    (
        line_offset(text, start),
        line_offset(text, end.saturating_sub(1)),
    )
}

#[derive(Clone)]
pub(super) struct RenderedHit {
    pub(super) location: String,
    pub(super) snippet: String,
    pub(super) after_first_page: bool,
}

impl Recall<'_> {
    /// 一次遍历转录，按原始排序返回每个索引段的稳定行范围与摘要。
    pub(super) async fn render_hits(
        &self,
        interaction: &RecallInteraction,
        segments: &[super::search_store::SearchSegment],
        query: &ParsedQuery,
    ) -> Result<Vec<RenderedHit>, PlatformToolError> {
        let mut results = vec![None; segments.len()];
        let mut remaining = segments.len();
        if remaining == 0 {
            return Ok(Vec::new());
        }
        let transcript = Transcript::new(interaction, &self.secrets);
        let mut walker = self
            .walker(transcript, &interaction.id, &ChunkMark::header())
            .await?;
        while let Some(mut chunk) = walker.next().await? {
            let Some(event) = chunk.event.take() else {
                continue;
            };
            if !segments
                .iter()
                .any(|segment| segment.event_sequence == event.sequence)
            {
                continue;
            }
            let indexed = render::indexable_event_text(&event).ok_or_else(changed_during_search)?;
            let text = &indexed.text;
            let mut renderer = EventRenderer {
                recall: self,
                interaction,
                event: &event,
                chunk: &chunk,
                text,
                hits: matches(text, query),
                query,
                tool_lines: None,
            };
            for (index, segment) in segments.iter().enumerate() {
                if segment.event_sequence != event.sequence {
                    continue;
                }
                let start =
                    usize::try_from(segment.byte_start).map_err(|_| changed_during_search())?;
                let end = usize::try_from(segment.byte_end).map_err(|_| changed_during_search())?;
                results[index] = Some(renderer.render(start, end).await?);
                remaining -= 1;
            }
            if remaining == 0 {
                break;
            }
        }
        results
            .into_iter()
            .map(|result| result.ok_or_else(changed_during_search))
            .collect()
    }
}

struct EventRenderer<'a, 'b> {
    recall: &'a Recall<'b>,
    interaction: &'a RecallInteraction,
    event: &'a super::ObservationEvent,
    chunk: &'a super::Chunk,
    text: &'a str,
    hits: Vec<Range<usize>>,
    query: &'a ParsedQuery,
    tool_lines: Option<Vec<String>>,
}

impl EventRenderer<'_, '_> {
    async fn render(&mut self, start: usize, end: usize) -> Result<RenderedHit, PlatformToolError> {
        let Self {
            recall,
            interaction,
            event,
            chunk,
            text,
            hits,
            query,
            tool_lines,
        } = self;
        if start >= end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(changed_during_search());
        }
        let hit = hits
            .iter()
            .find(|range| range.start >= start && range.end <= end)
            .cloned()
            .unwrap_or(start..end);
        let (segment_start, segment_end) = line_window(text, hit.start, hit.end);
        let base = format!("{HISTORY_ROOT}{}/{}", interaction.root_id, interaction.id);
        let tool = matches!(
            event.kind.as_str(),
            "client_tool_handoff" | "platform_tool_started"
        );
        let separator = text.find('\n').unwrap_or(text.len());
        let argument_start = separator.saturating_add(1);
        // 工具名与参数命中同时存在时优先参数：参数一律映射到完整工具文件，
        // 工具名才映射到转录的 `### tool call` 行。
        let argument_hit = hits
            .iter()
            .find(|range| range.end > argument_start && range.start >= start && range.end <= end);
        if tool && let Some(argument) = argument_hit {
            let number = chunk.mark.tools.saturating_add(1);
            if tool_lines.is_none() {
                let tool = recall
                    .observation
                    .recall_tool(&interaction.id, number)
                    .await
                    .map_err(storage)?
                    .ok_or_else(changed_during_search)?;
                *tool_lines = Some(render::tool_file(
                    interaction,
                    number,
                    &tool,
                    &recall.secrets,
                ));
            }
            let lines = tool_lines.as_ref().ok_or_else(changed_during_search)?;
            let arguments = text
                .get(argument_start..)
                .ok_or_else(changed_during_search)?;
            let (first_line, last_line) = line_window(
                arguments,
                argument.start.max(argument_start) - argument_start,
                argument.end - argument_start,
            );
            let start_line = render::TOOL_FILE_HEADER_LINES + 1 + first_line;
            let end_line = render::TOOL_FILE_HEADER_LINES + 1 + last_line;
            let selected = lines
                .get((start_line - 1) as usize..end_line as usize)
                .ok_or_else(changed_during_search)?
                .join("\n");
            return Ok(RenderedHit {
                location: format!("{base}/tools/{number}:{start_line}-{end_line}"),
                snippet: snippet(&selected, query),
                after_first_page: hit_after_first_page(&selected, query),
            });
        }
        let heading = chunk
            .lines
            .iter()
            .position(|line| {
                if tool {
                    line.starts_with("### tool call ")
                } else if event.kind == "user_input_recorded" {
                    line == "## user"
                } else {
                    line == "## assistant"
                }
            })
            .ok_or_else(changed_during_search)?;
        let (first, last) = if tool {
            let line = chunk.mark.line + heading as u64;
            (line, line)
        } else {
            let body = chunk.mark.line + heading as u64 + 1;
            (body + segment_start, body + segment_end)
        };
        let first_index = (first - chunk.mark.line) as usize;
        let last_index = (last - chunk.mark.line + 1) as usize;
        let selected = chunk
            .lines
            .get(first_index..last_index)
            .ok_or_else(changed_during_search)?
            .join("\n");
        Ok(RenderedHit {
            location: format!("{base}:{first}-{last}"),
            snippet: snippet(&selected, query),
            after_first_page: hit_after_first_page(&selected, query),
        })
    }
}
